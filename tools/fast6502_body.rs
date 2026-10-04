// Fast path for the documented NMOS 6502 opcodes over mos6502's CPU struct,
// matching mos6502's results exactly; see tools/gen_fast6502.py for what falls
// back. Kept compact (one addressing-mode switch, one operation switch) so the
// interpreter fits the Playdate's instruction cache.

use mos6502::cpu::CPU;
use mos6502::instruction::Nmos6502;
use mos6502::memory::Bus;
use mos6502::registers::Status;

use crate::memory::Memory;

// @CONSTS@

/// Per opcode: kind (bits 0-5), addressing mode (6-9), base cycles (10-13),
/// parameter (16-24: flag mask, plus bit 8 for "branch if set").
static TABLE: [u32; 256] = [
// @TABLE@
];

const C: u8 = 0x01;
const Z: u8 = 0x02;
const D: u8 = 0x08;
const V: u8 = 0x40;
const N: u8 = 0x80;

#[inline(always)]
fn nz(p: u8, v: u8) -> u8 {
    (p & !(Z | N)) | (v & N) | if v == 0 { Z } else { 0 }
}

#[inline(always)]
fn rd16(m: &Memory, addr: u16) -> u16 {
    u16::from_le_bytes([m.read(addr), m.read(addr.wrapping_add(1))])
}

/// CPU registers held outside mos6502's CPU struct, so a frame's worth of
/// instructions can run with them in locals: on the Playdate every store to a
/// new heap cache line costs ~0.5 us.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Regs {
    pub a: u8,
    pub x: u8,
    pub y: u8,
    pub s: u8,
    pub p: u8,
    pub pc: u16,
}

impl Regs {
    #[inline(always)]
    pub fn load(cpu: &CPU<Memory, Nmos6502>) -> Self {
        let r = &cpu.registers;
        Self {
            a: r.accumulator,
            x: r.index_x,
            y: r.index_y,
            s: r.stack_pointer.0,
            p: r.status.bits(),
            pc: r.program_counter,
        }
    }

    #[inline(always)]
    pub fn store(&self, cpu: &mut CPU<Memory, Nmos6502>) {
        let r = &mut cpu.registers;
        r.accumulator = self.a;
        r.index_x = self.x;
        r.index_y = self.y;
        r.stack_pointer.0 = self.s;
        r.status = Status::from_bits_truncate(self.p);
        r.program_counter = self.pc;
    }
}

/// `step` over mos6502's CPU struct, also advancing its cycle count.
#[inline(always)]
pub fn step_cpu(cpu: &mut CPU<Memory, Nmos6502>) -> Option<u32> {
    let mut r = Regs::load(cpu);
    let cycles = step(&mut r, &mut cpu.memory)?;
    r.store(cpu);
    cpu.cycles = cpu.cycles.wrapping_add(cycles as u64);
    Some(cycles)
}

/// Runs one instruction if the fast path covers it, returning its cycles;
/// returns None, with no state touched, when mos6502 must run it instead.
#[inline(always)]
pub fn step(r: &mut Regs, m: &mut Memory) -> Option<u32> {
    let pc = r.pc;
    if !(0x0100..=0xFFFC).contains(&pc) {
        return None;
    }
    let e = TABLE[m.read(pc) as usize];
    let kind = e & 0x3F;
    let mut p = r.p;
    if kind == SLOW || ((kind == ADC || kind == SBC) && p & D != 0) {
        return None;
    }
    let mode = (e >> 6) & 0xF;
    let mut cycles = (e >> 10) & 0xF;
    let param = e >> 16;
    let mut a = r.a;
    let mut x = r.x;
    let mut y = r.y;
    let mut s = r.s;

    // Operand bytes are plain reads: with PC in 0x0100..=0xFFFC they can't
    // touch the side-effecting DATA registers. Pointer and data accesses go
    // through get_byte, in the same order as mos6502.
    let op1 = pc.wrapping_add(1);
    let mut ea: u16 = 0;
    let mut crossed = false;
    let mut next = pc.wrapping_add(match mode {
        M_IMP | M_ACC => 1,
        M_ABS | M_ABSX | M_ABSY | M_IND => 3,
        _ => 2,
    });
    match mode {
        M_ZP => ea = m.read(op1) as u16,
        M_ZPX => ea = m.read(op1).wrapping_add(x) as u16,
        M_ZPY => ea = m.read(op1).wrapping_add(y) as u16,
        M_ABS => ea = rd16(m, op1),
        M_ABSX | M_ABSY => {
            let base = rd16(m, op1);
            ea = base.wrapping_add(if mode == M_ABSX { x } else { y } as u16);
            crossed = (base ^ ea) & 0xFF00 != 0;
        }
        M_IZX | M_IZY => {
            let t = m.read(op1).wrapping_add(if mode == M_IZX { x } else { 0 });
            let lo = m.get_byte(t as u16);
            let hi = m.get_byte(t.wrapping_add(1) as u16);
            ea = u16::from_le_bytes([lo, hi]);
            if mode == M_IZY {
                let base = ea;
                ea = base.wrapping_add(y as u16);
                crossed = (base ^ ea) & 0xFF00 != 0;
            }
        }
        M_IND => {
            let ptr = rd16(m, op1);
            let lo = m.get_byte(ptr);
            let hi = m.get_byte((ptr & 0xFF00) | (ptr.wrapping_add(1) & 0x00FF));
            ea = u16::from_le_bytes([lo, hi]);
        }
        M_REL => ea = m.read(op1) as i8 as u16,
        _ => {}
    }

    match kind {
        ORA..=BIT => {
            let v = if mode == M_IMM { m.read(op1) } else { m.get_byte(ea) };
            match kind {
                ORA => {
                    a |= v;
                    p = nz(p, a);
                }
                AND => {
                    a &= v;
                    p = nz(p, a);
                }
                EOR => {
                    a ^= v;
                    p = nz(p, a);
                }
                ADC | SBC => {
                    // Binary mode only; SBC is ADC of the complement.
                    let v = if kind == SBC { !v } else { v };
                    let sum = a as u16 + v as u16 + (p & C) as u16;
                    let r = sum as u8;
                    let overflow = (!(a ^ v) & (a ^ r) & 0x80) != 0;
                    p = nz(p & !(C | V), r) | (sum > 0xFF) as u8 | if overflow { V } else { 0 };
                    a = r;
                }
                CMP | CPX | CPY => {
                    let r = if kind == CMP { a } else if kind == CPX { x } else { y };
                    p = nz(p & !C, r.wrapping_sub(v)) | (r >= v) as u8;
                }
                LDA => {
                    a = v;
                    p = nz(p, a);
                }
                LDX => {
                    x = v;
                    p = nz(p, x);
                }
                LDY => {
                    y = v;
                    p = nz(p, y);
                }
                _ => {
                    // BIT
                    p = (p & !(Z | V | N)) | (v & (N | V)) | if a & v == 0 { Z } else { 0 };
                }
            }
            cycles += crossed as u32;
        }
        STA => m.write(ea, a),
        STX => m.write(ea, x),
        STY => m.write(ea, y),
        ASL..=DEC => {
            let v = if mode == M_ACC { a } else { m.get_byte(ea) };
            let (r, carry) = match kind {
                ASL => (v << 1, v >> 7),
                LSR => (v >> 1, v & 1),
                ROL => ((v << 1) | (p & C), v >> 7),
                ROR => ((v >> 1) | ((p & C) << 7), v & 1),
                INC => (v.wrapping_add(1), p & C),
                _ => (v.wrapping_sub(1), p & C),
            };
            p = nz(p & !C, r) | carry;
            if mode == M_ACC {
                a = r;
            } else {
                m.write(ea, r);
            }
            cycles += crossed as u32;
        }
        BR => {
            let set = p as u32 & param & 0xFF != 0;
            if set == (param & 0x100 != 0) {
                let target = next.wrapping_add(ea);
                cycles += 1 + ((next ^ target) & 0xFF00 != 0) as u32;
                next = target;
            }
        }
        JMP => next = ea,
        JSR => {
            let ret = next.wrapping_sub(1);
            m.write(0x0100 | s as u16, (ret >> 8) as u8);
            s = s.wrapping_sub(1);
            m.write(0x0100 | s as u16, ret as u8);
            s = s.wrapping_sub(1);
            next = ea;
        }
        RTS | RTI | PLA | PLP => {
            s = s.wrapping_add(1);
            let v = m.get_byte(0x0100 | s as u16);
            match kind {
                PLA => {
                    a = v;
                    p = nz(p, a);
                }
                PLP => p = v,
                _ => {
                    let lo = if kind == RTI {
                        p = v;
                        s = s.wrapping_add(1);
                        m.get_byte(0x0100 | s as u16)
                    } else {
                        v
                    };
                    s = s.wrapping_add(1);
                    let hi = m.get_byte(0x0100 | s as u16);
                    next = u16::from_le_bytes([lo, hi]).wrapping_add((kind == RTS) as u16);
                }
            }
        }
        PHA | PHP => {
            m.write(0x0100 | s as u16, if kind == PHA { a } else { p | 0x30 });
            s = s.wrapping_sub(1);
        }
        CLF => p &= !(param as u8),
        SEF => p |= param as u8,
        TAX => {
            x = a;
            p = nz(p, x);
        }
        TAY => {
            y = a;
            p = nz(p, y);
        }
        TXA => {
            a = x;
            p = nz(p, a);
        }
        TYA => {
            a = y;
            p = nz(p, a);
        }
        TSX => {
            x = s;
            p = nz(p, x);
        }
        TXS => s = x,
        INX => {
            x = x.wrapping_add(1);
            p = nz(p, x);
        }
        INY => {
            y = y.wrapping_add(1);
            p = nz(p, y);
        }
        DEX => {
            x = x.wrapping_sub(1);
            p = nz(p, x);
        }
        DEY => {
            y = y.wrapping_sub(1);
            p = nz(p, y);
        }
        _ => debug_assert_eq!(kind, NOP),
    }

    *r = Regs { a, x, y, s, p, pc: next };
    Some(cycles)
}
