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

/// Instructions run through the fast path, per opcode (profiling builds only).
#[cfg(feature = "trace")]
pub static mut OPCODE_COUNTS: [u64; 256] = [0; 256];

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

/// Dedicated paths for the opcodes that dominate real games (the 17 below
/// are ~80% of instructions executed), ahead of the generic table-driven
/// path. Same results as that path; None means "not one of these".
#[inline(always)]
fn hot(r: &mut Regs, m: &mut Memory, opcode: u8, b1: u8, b2: u8) -> Option<u32> {
    let abs = u16::from_le_bytes([b1, b2]);
    let pc = r.pc;
    let (len, cycles) = match opcode {
        0xAD => {
            // LDA abs
            r.a = m.get_byte(abs);
            r.p = nz(r.p, r.a);
            (3, 4)
        }
        0x8D => {
            // STA abs
            m.write(abs, r.a);
            (3, 4)
        }
        0x69 if r.p & D == 0 => {
            // ADC #imm (binary mode)
            let sum = r.a as u16 + b1 as u16 + (r.p & C) as u16;
            let res = sum as u8;
            let overflow = (!(r.a ^ b1) & (r.a ^ res) & 0x80) != 0;
            r.p = nz(r.p & !(C | V), res) | (sum > 0xFF) as u8 | if overflow { V } else { 0 };
            r.a = res;
            (2, 2)
        }
        0xA0 => {
            // LDY #imm
            r.y = b1;
            r.p = nz(r.p, b1);
            (2, 2)
        }
        0xA9 => {
            // LDA #imm
            r.a = b1;
            r.p = nz(r.p, b1);
            (2, 2)
        }
        0xB1 | 0x91 => {
            // LDA / STA (zp),Y
            let lo = m.get_byte(b1 as u16);
            let hi = m.get_byte(b1.wrapping_add(1) as u16);
            let base = u16::from_le_bytes([lo, hi]);
            let ea = base.wrapping_add(r.y as u16);
            if opcode == 0xB1 {
                r.a = m.get_byte(ea);
                r.p = nz(r.p, r.a);
                (2, 5 + ((base ^ ea) & 0xFF00 != 0) as u32)
            } else {
                m.write(ea, r.a);
                (2, 6)
            }
        }
        0xF0 | 0xD0 => {
            // BEQ / BNE
            let next = pc.wrapping_add(2);
            if (r.p & Z != 0) == (opcode == 0xF0) {
                let target = next.wrapping_add(b1 as i8 as u16);
                r.pc = target;
                return Some(3 + ((next ^ target) & 0xFF00 != 0) as u32);
            }
            (2, 2)
        }
        0x18 => {
            // CLC
            r.p &= !C;
            (1, 2)
        }
        0xC9 | 0xE0 => {
            // CMP / CPX #imm
            let reg = if opcode == 0xC9 { r.a } else { r.x };
            r.p = nz(r.p & !C, reg.wrapping_sub(b1)) | (reg >= b1) as u8;
            (2, 2)
        }
        0x85 => {
            // STA zp
            m.write(b1 as u16, r.a);
            (2, 3)
        }
        0xA5 => {
            // LDA zp
            r.a = m.get_byte(b1 as u16);
            r.p = nz(r.p, r.a);
            (2, 3)
        }
        0x4C => {
            // JMP abs
            r.pc = abs;
            return Some(3);
        }
        0xAA => {
            // TAX
            r.x = r.a;
            r.p = nz(r.p, r.x);
            (1, 2)
        }
        0xCE => {
            // DEC abs
            let v = m.get_byte(abs).wrapping_sub(1);
            r.p = nz(r.p, v);
            m.write(abs, v);
            (3, 6)
        }
        _ => return None,
    };
    r.pc = pc.wrapping_add(len);
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
    // Opcode and both operand bytes, through one page lookup when the code
    // sits in a fast-mapped page. Plain reads: with PC in 0x0100..=0xFFFC
    // these can't touch the side-effecting DATA registers.
    let code = m.code_ptr(pc);
    let (opcode, b1, b2) = if code.is_null() {
        (m.read(pc), m.read(pc.wrapping_add(1)), m.read(pc.wrapping_add(2)))
    } else {
        // SAFETY: code_ptr guarantees three readable bytes.
        unsafe { (*code, *code.add(1), *code.add(2)) }
    };
    #[cfg(feature = "trace")]
    // SAFETY: profiling builds are single-threaded host tools.
    unsafe {
        OPCODE_COUNTS[opcode as usize] += 1;
    }
    if let Some(cycles) = hot(r, m, opcode, b1, b2) {
        return Some(cycles);
    }
    let e = TABLE[opcode as usize];
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

    // Pointer and data accesses go through get_byte, in the same order as mos6502.
    let abs = u16::from_le_bytes([b1, b2]);
    let mut ea: u16 = 0;
    let mut crossed = false;
    let mut next = pc.wrapping_add(match mode {
        M_IMP | M_ACC => 1,
        M_ABS | M_ABSX | M_ABSY | M_IND => 3,
        _ => 2,
    });
    match mode {
        M_ZP => ea = b1 as u16,
        M_ZPX => ea = b1.wrapping_add(x) as u16,
        M_ZPY => ea = b1.wrapping_add(y) as u16,
        M_ABS => ea = abs,
        M_ABSX | M_ABSY => {
            let base = abs;
            ea = base.wrapping_add(if mode == M_ABSX { x } else { y } as u16);
            crossed = (base ^ ea) & 0xFF00 != 0;
        }
        M_IZX | M_IZY => {
            let t = b1.wrapping_add(if mode == M_IZX { x } else { 0 });
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
            let ptr = abs;
            let lo = m.get_byte(ptr);
            let hi = m.get_byte((ptr & 0xFF00) | (ptr.wrapping_add(1) & 0x00FF));
            ea = u16::from_le_bytes([lo, hi]);
        }
        M_REL => ea = b1 as i8 as u16,
        _ => {}
    }

    match kind {
        ORA..=BIT => {
            let v = if mode == M_IMM { b1 } else { m.get_byte(ea) };
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

/// Why `run` returned.
pub enum Exit {
    /// The frame's cycle budget is used up.
    Budget,
    /// The next instruction needs `Emulator::step` (BRK, the HLE far-return
    /// address, or anything the fast path doesn't cover). It hasn't run.
    Slow,
    /// An instruction ran, taking this many cycles (not yet in `cycles_run`
    /// or the timers), and an interrupt is pending: `handle_interrupts` must
    /// run before the cycles are counted.
    Interrupt(u32),
}

/// Frame-loop counters `run` keeps for `Emulator::run_frame`.
pub struct Counters {
    /// Cycles of the frame so far, including halted time.
    pub cycles_run: u32,
    /// Cycles toward the next timer tick.
    pub remainder: u32,
    /// Halted cycles within `cycles_run` (the rest ran on the CPU).
    pub halted: u32,
    /// Instructions run here (profiling builds only).
    #[cfg(feature = "trace")]
    pub steps: u32,
}

/// The common case of `Emulator::run_frame` as one tight loop: fast-path
/// instructions with no interrupt pending, halted time and timer ticks, all
/// in locals (on the Playdate each store to a new heap cache line costs
/// ~0.5 us). Returns for anything else; see `Exit`.
#[inline(never)]
pub fn run(r: &mut Regs, m: &mut Memory, n: &mut Counters, budget: u32, timer_step: u32, hle_return: u16) -> Exit {
    let mut regs = *r;
    // The RAM buffer never moves (it is only ever copied into).
    let ram = m.ram.as_ptr();
    // SAFETY: every index below is under 0x300 and ram is 32 KiB.
    let ram_at = |i: usize| unsafe { *ram.add(i) };
    let mut cycles_run = n.cycles_run;
    let mut halted = n.halted;
    // Cycle count at which the next timer tick falls due.
    let mut tick_at = cycles_run + (timer_step - n.remainder);
    let exit = loop {
        if cycles_run >= budget {
            break Exit::Budget;
        }
        if ram_at(0x200) & 0x08 != 0 {
            // Halted until an interrupt.
            cycles_run += 400;
            halted += 400;
        } else {
            if regs.pc == hle_return {
                break Exit::Slow;
            }
            let Some(cycles) = step(&mut regs, m) else { break Exit::Slow };
            #[cfg(feature = "trace")]
            {
                n.steps += 1;
            }
            // Emulator::handle_interrupts acts only when one of these is set.
            if regs.p & 0x04 == 0
                && (ram_at(0x04) & ram_at(0x23A) & 0x83 != 0 || ram_at(0x05) & ram_at(0x23B) & 0xEF != 0)
            {
                *r = regs;
                n.cycles_run = cycles_run;
                n.halted = halted;
                n.remainder = timer_step - (tick_at - cycles_run);
                return Exit::Interrupt(cycles);
            }
            cycles_run += cycles;
        }
        if cycles_run >= tick_at {
            let elapsed = cycles_run - (tick_at - timer_step); // == old remainder + cycles
            m.update_timers(elapsed / timer_step);
            tick_at = cycles_run + (timer_step - elapsed % timer_step);
        }
    };
    *r = regs;
    n.cycles_run = cycles_run;
    n.halted = halted;
    n.remainder = timer_step - (tick_at - cycles_run);
    exit
}
