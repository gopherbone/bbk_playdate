//! Differential tests for the Playdate speedups in the no_std core patch:
//! - Memory::read / write fast paths vs read_reference / write_reference
//! - the fast 6502 path vs mos6502, one random instruction at a time
//! - whole games in lockstep: fast frame loop vs the original loop on mos6502
//!
//! Usage: difftest [iterations per opcode] [game.gam ...]  (ROMS=dir with 8.BIN and E.BIN)
//! Lockstep options (environment): STATE=file starts from a save state;
//! TITLE_IDLE=n idles n frames then taps Enter every 2 s; REF_ONLY=1 runs
//! the reference on both sides; TRACE_FRAMES=1 prints the CPU every frame.
use bbkemu_core::cpu::CpuWrapper;
use bbkemu_core::input::BbkKey;
use bbkemu_core::memory::Memory;
use bbkemu_core::model::MODEL_4980;
use bbkemu_core::Emulator;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 { self.0 ^= self.0 << 13; self.0 ^= self.0 >> 7; self.0 ^= self.0 << 17; self.0 }
    fn byte(&mut self) -> u8 { self.next() as u8 }
}

fn state(c: &CpuWrapper) -> (u16, u8, u8, u8, u8, u8, u64) {
    let r = &c.inner.registers;
    (r.program_counter, r.accumulator, r.index_x, r.index_y, r.stack_pointer.0, r.status.bits(), c.inner.cycles)
}

fn random_ops(iters: usize) -> usize {
    let mut a = CpuWrapper::new(Memory::new());
    let mut b = CpuWrapper::new(Memory::new());
    b.reference = true;
    a.memory_mut().init();
    b.memory_mut().init();
    let mut rng = Rng(0x9E3779B97F4A7C15);
    let mut fails = 0;
    let mut ram = vec![0u8; 0x8000];
    for op in 0..=255u8 {
        if op == 0x00 { continue; } // BRK: both use mos6502, and it's intercepted by the emulator anyway
        let mut op_fails = 0;
        for it in 0..iters {
            if it % 32 == 0 { for v in ram.iter_mut() { *v = rng.byte(); } }
            let edge = |rng: &mut Rng, a: u8| match rng.next() % 6 { 0 => 0x00, 1 => 0x7F, 2 => 0x80, 3 => 0xFF, 4 => a, _ => rng.byte() };
            let a0 = rng.byte();
            for i in 0..0x200 { ram[i] = if rng.next() % 3 == 0 { edge(&mut rng, a0) } else { rng.byte() }; }
            // Keep the DATA channel / bank registers sane-ish but random.
            let pc = 0x0400 + (rng.next() % 0x0BF0) as u16;
            ram[pc as usize] = op;
            ram[pc as usize + 1] = rng.byte();
            ram[pc as usize + 2] = rng.byte();
            let mut banks = [0u32; 16];
            for bk in banks.iter_mut() {
                *bk = match rng.next() % 4 { 0 => (rng.next() % 8) as u32, 1 => if rng.next() % 2 == 0 { 0x3F8 + (rng.next() % 8) as u32 } else { 0x200 + (rng.next() % 0x200) as u32 }, 2 => 0x800 + (rng.next() % 0x200) as u32, _ => 0xE00 + (rng.next() % 0x200) as u32 };
            }
            let regs = [a0, edge(&mut rng, a0), edge(&mut rng, a0), rng.byte(), rng.byte()];
            for c in [&mut a, &mut b] {
                let m = c.memory_mut();
                m.ram.copy_from_slice(&ram);
                m.bank_switch.banks = banks;
                let r = &mut c.inner.registers;
                r.program_counter = pc;
                r.accumulator = regs[0];
                r.index_x = regs[1];
                r.index_y = regs[2];
                r.stack_pointer.0 = regs[3];
                r.status = mos6502::registers::Status::from_bits_truncate(regs[4]);
                c.inner.cycles = 1000;
            }
            let ca = a.step();
            let cb = b.step();
            let bad = ca != cb || state(&a) != state(&b) || a.memory().ram != b.memory().ram
                || a.memory().bank_switch.banks != b.memory().bank_switch.banks || a.memory().flash != b.memory().flash;
            if bad {
                if op_fails < 2 {
                    println!("op {op:02X} pc {pc:04X} regs {regs:02X?}: fast {:?} ret {ca} | ref {:?} ret {cb}", state(&a), state(&b));
                    if let Some(i) = (0..0x8000).find(|&i| a.memory().ram[i] != b.memory().ram[i]) {
                        println!("   ram[{i:04X}] fast {:02X} ref {:02X}", a.memory().ram[i], b.memory().ram[i]);
                    }
                }
                op_fails += 1;
                let flash = b.memory().flash.clone();
                a.memory_mut().flash.copy_from_slice(&flash);
            }
        }
        if op_fails > 0 { println!("op {op:02X}: {op_fails}/{iters} mismatches"); fails += 1; }
    }
    fails
}

fn lockstep(game: &str, frames: u64) -> bool {
    lockstep_keys(game, frames, |f| if f % 50 == 10 { Some(true) } else if f % 50 == 14 { Some(false) } else { None })
}

/// `keys(frame)`: Some(true) to press the next key in the cycle, Some(false) to release.
fn lockstep_keys(game: &str, frames: u64, keys_at: impl Fn(u64) -> Option<bool>) -> bool {
    let rom = std::env::var("ROMS").unwrap();
    let r8 = std::fs::read(format!("{rom}/8.BIN")).unwrap();
    let re = std::fs::read(format!("{rom}/E.BIN")).unwrap();
    let g = std::fs::read(game).unwrap();
    let mut emus: Vec<Emulator> = (0..2).map(|_| { let mut e = Emulator::new(&MODEL_4980); e.load_rom_8(&r8); e.load_rom_e(&re); e }).collect();
    emus[1].cpu.reference = true;
    if std::env::var("REF_ONLY").is_ok() { emus[0].cpu.reference = true; }
    for e in emus.iter_mut() { e.load_gam(&g).unwrap(); }
    if let Ok(path) = std::env::var("STATE") {
        // Same restore as the Playdate glue's bbk_state_load.
        let state = bbkemu_core::save::SaveState::from_bytes(&std::fs::read(path).unwrap()).unwrap();
        for e in emus.iter_mut() {
            e.load_save_state(&state).unwrap();
            let regs = &mut e.cpu.inner.registers;
            regs.accumulator = state.cpu.a;
            regs.index_x = state.cpu.x;
            regs.index_y = state.cpu.y;
            regs.status = mos6502::registers::Status::from_bits_truncate(state.cpu.status);
            let banks = &mut e.cpu.memory_mut().bank_switch;
            for (dst, &src) in banks.banks.iter_mut().zip(state.bank_switch.banks.iter()) { *dst = src; }
            banks.set_selected(state.bank_switch.selected);
        }
        println!("loaded state: pc {:04X} sp {:02X} p {:02X}", emus[0].cpu.pc(), emus[0].cpu.sp(), emus[0].cpu.status());
    }
    let keys: Vec<BbkKey> = if std::env::var("TITLE_IDLE").is_ok() { vec![BbkKey::Enter] } else {
        vec![BbkKey::Enter, BbkKey::Down, BbkKey::Enter, BbkKey::Right, BbkKey::Enter, BbkKey::Up, BbkKey::Left, BbkKey::Enter] };
    let t = std::time::Instant::now();
    let mut times = [0f64; 2];
    for f in 0..frames {
        for (i, e) in emus.iter_mut().enumerate() {
            match keys_at(f) {
                Some(true) => e.key_down(keys[(f / 50) as usize % keys.len()]),
                Some(false) => e.key_up(),
                None => {}
            }
            let t0 = std::time::Instant::now();
            e.run_frame();
            times[i] += t0.elapsed().as_secs_f64();
            if std::env::var("TRACE_FRAMES").is_ok() && i == 1 { eprintln!("frame {f}: pc {:04X} sp {:02X} p {:02X} cycles {}", e.cpu.pc(), e.cpu.sp(), e.cpu.status(), e.cpu.cycles()); }
        }
        let (x, y) = (&emus[0], &emus[1]);
        if state(&x.cpu) != state(&y.cpu) || x.cpu.memory().ram != y.cpu.memory().ram || x.cpu.memory().flash != y.cpu.memory().flash || x.is_running() != y.is_running() {
            println!("{game}: diverged at frame {f}: fast {:?} ref {:?}", state(&x.cpu), state(&y.cpu));
            return false;
        }
        if !x.is_running() { println!("{game}: game ended at frame {f}"); break; }
    }
    println!("{game}: {frames} frames identical ({:.1}s); fast {:.3} ms/frame, ref {:.3} ms/frame",
        t.elapsed().as_secs_f64(), times[0] * 1000.0 / frames as f64, times[1] * 1000.0 / frames as f64);
    true
}

fn reads(configs: usize) -> usize {
    let mut m = Memory::new();
    m.init();
    let mut rng = Rng(12345);
    for v in m.ram.iter_mut() { *v = rng.byte(); }
    for v in m.flash.iter_mut() { *v = rng.byte(); }
    let mut fails = 0;
    for c in 0..configs {
        m.rom_8 = if c % 3 == 0 { None } else { Some((0..0x200000).map(|i| (i * 7 + c) as u8).collect()) };
        m.rom_e = if c % 5 == 0 { None } else { Some((0..0x200000).map(|i| (i * 13 + c) as u8).collect()) };
        m.invalidate_page_cache();
        for bk in m.bank_switch.banks.iter_mut() {
            *bk = match rng.next() % 6 {
                0 => (rng.next() % 8) as u32, 1 => if rng.next() % 2 == 0 { 0x3F8 + (rng.next() % 8) as u32 } else { 0x200 + (rng.next() % 0x200) as u32 }, 2 => 0x800 + (rng.next() % 0x200) as u32,
                3 => 0xE00 + (rng.next() % 0x200) as u32, 4 => (rng.next() % 0x1000) as u32, _ => rng.next() as u32,
            };
        }
        // Second pass with only the flash mode changed, to catch stale page caches.
        for cmd in [(rng.next() % 4) as u8, (rng.next() % 4) as u8] {
            m.set_flash_state(cmd, 0);
            for addr in 0..=0xFFFFu16 {
                if m.read(addr) != m.read_reference(addr) {
                    if fails < 5 { println!("read {addr:04X} banks {:X?}: fast {:02X} ref {:02X}", m.bank_switch.banks, m.read(addr), m.read_reference(addr)); }
                    fails += 1;
                }
            }
        }
    }
    fails
}

fn writes(configs: usize) -> usize {
    let mut ms = [Memory::new(), Memory::new()];
    let mut rng = Rng(777);
    let mut fails = 0;
    for c in 0..configs {
        let mut banks = [0u32; 16];
        for bk in banks.iter_mut() {
            *bk = match rng.next() % 5 { 0 => (rng.next() % 8) as u32, 1 => 0x200 + (rng.next() % 0x200) as u32, 2 => 0xE00 + (rng.next() % 0x200) as u32, 3 => (rng.next() % 0x1000) as u32, _ => rng.next() as u32 };
        }
        let cmd = (rng.next() % 4) as u8;
        let rom = c % 2 == 0;
        for m in ms.iter_mut() {
            m.init();
            m.bank_switch.banks = banks;
            m.set_flash_state(cmd, 0);
            m.rom_e = if rom { Some(vec![0; 0x200000]) } else { None };
            m.invalidate_page_cache();
        }
        for _ in 0..4000 {
            let addr = match rng.next() % 4 { 0 => (rng.next() % 0x400) as u16, 1 => 0x200 + (rng.next() % 0x40) as u16, _ => rng.next() as u16 };
            let v = rng.byte();
            ms[0].write(addr, v);
            ms[1].write_reference(addr, v);
        }
        if ms[0].ram != ms[1].ram || ms[0].flash != ms[1].flash || ms[0].bank_switch.banks != ms[1].bank_switch.banks
            || ms[0].flash_cycles() != ms[1].flash_cycles() || ms[0].flash_cmd() != ms[1].flash_cmd() {
            fails += 1;
            if fails < 3 { println!("write mismatch in config {c}"); }
        }
    }
    fails
}

fn main() {
    println!("memory writes: {} mismatched configs", writes(400));
    println!("memory reads: {} mismatches", reads(300));
    let iters: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(2000);
    let fails = random_ops(iters);
    println!("random single-step: {} opcodes with mismatches", fails);
    if std::env::var("TITLE_IDLE").is_ok() {
        // Sit on the title for a while, then tap Enter every 2 s, like on device.
        for g in std::env::args().skip(2) {
            let start: u64 = std::env::var("TITLE_IDLE").unwrap().parse().unwrap_or(3600);
            lockstep_keys(&g, start + 3000, |f| {
                if f < start { return None; }
                match (f - start) % 120 { 0 => Some(true), 6 => Some(false), _ => None }
            });
        }
        return;
    }
    if std::env::var("COUNT").is_ok() { for g in std::env::args().skip(2) { count(&g); } return; }
    for g in std::env::args().skip(2) { lockstep(&g, 6000); }
}

#[allow(dead_code)]
pub fn count(game: &str) {
    let rom = std::env::var("ROMS").unwrap();
    let mut e = Emulator::new(&MODEL_4980);
    e.load_rom_8(&std::fs::read(format!("{rom}/8.BIN")).unwrap());
    e.load_rom_e(&std::fs::read(format!("{rom}/E.BIN")).unwrap());
    e.load_gam(&std::fs::read(game).unwrap()).unwrap();
    let mut last = (0u64, 0u64);
    for f in 0..600u64 {
        e.run_frame();
        if f % 100 == 99 {
            let (s, c) = (e.cpu.steps, e.cpu.cycles());
            println!("frames {}-{}: {} insts/frame, {} cpu cycles/frame (rest halted)", f - 99, f, (s - last.0) / 100, (c - last.1) / 100);
            last = (s, c);
        }
    }
}
