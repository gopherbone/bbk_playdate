//! Runs a game headless and simulates the Playdate (STM32H7) data cache over the
//! emulated memory accesses: 16 KB, 4-way, 32-byte lines, LRU. On device a miss
//! costs ~0.9 us and a store to PSRAM ~0.56 us (merged when consecutive stores
//! hit the same line), so this estimates where emulation time goes.
//!
//! Usage: ROMS=dir memprof game.gam [first_frame last_frame]
use bbkemu_core::memory;
use bbkemu_core::model::MODEL_4980;
use bbkemu_core::Emulator;
use std::cell::RefCell;

const WAYS: usize = 4;
const SETS: usize = 16 * 1024 / 32 / WAYS;

#[derive(Default)]
struct Stats {
    reads: u64,
    read_misses: u64,
    writes: u64,
    store_lines: u64, // stores not merged with the previous store's line
    last_store_line: u32,
    lines: Vec<[(u32, u64); WAYS]>,
    tick: u64,
    by_region: [u64; 4], // read misses: RAM, flash, ROM 8, ROM E
    stores_by_page: std::collections::BTreeMap<u32, u64>,
}

thread_local! {
    static S: RefCell<Stats> = RefCell::new(Stats { lines: vec![[(u32::MAX, 0); WAYS]; SETS], ..Default::default() });
}

fn access(addr: u32, write: bool) {
    S.with(|s| {
        let mut s = s.borrow_mut();
        let line = addr >> 5;
        if write {
            s.writes += 1;
            if line != s.last_store_line {
                s.store_lines += 1;
                s.last_store_line = line;
                *s.stores_by_page.entry(addr >> 8).or_default() += 1;
            }
            return; // write-through, assume no allocate
        }
        s.reads += 1;
        s.tick += 1;
        let tick = s.tick;
        let set = &mut s.lines[line as usize % SETS];
        if let Some(w) = set.iter_mut().find(|w| w.0 == line) {
            w.1 = tick;
            return;
        }
        let victim = set.iter_mut().min_by_key(|w| w.1).unwrap();
        *victim = (line, tick);
        s.read_misses += 1;
        let region = match addr { 0..=0x7FFF => 0, 0x200000..=0x3FFFFF => 1, 0x800000..=0x9FFFFF => 2, _ => 3 };
        s.by_region[region] += 1;
    });
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (first, last): (u64, u64) = (args.get(2).map_or(300, |a| a.parse().unwrap()), args.get(3).map_or(600, |a| a.parse().unwrap()));
    let rom = std::env::var("ROMS").unwrap();
    let mut e = Emulator::new(&MODEL_4980);
    e.load_rom_8(&std::fs::read(format!("{rom}/8.BIN")).unwrap());
    e.load_rom_e(&std::fs::read(format!("{rom}/E.BIN")).unwrap());
    e.load_gam(&std::fs::read(&args[1]).unwrap()).unwrap();
    for _ in 0..first { e.run_frame(); }
    unsafe { memory::ACCESS_HOOK = Some(access) };
    let steps0 = e.cpu.steps;
    for _ in first..last { e.run_frame(); }
    let n = (last - first) as f64;
    S.with(|s| {
        let s = s.borrow();
        let insts = (e.cpu.steps - steps0) as f64 / n;
        println!("per frame: {insts:.0} instructions, {:.0} reads, {:.0} read misses, {:.0} writes, {:.0} unmerged stores",
            s.reads as f64 / n, s.read_misses as f64 / n, s.writes as f64 / n, s.store_lines as f64 / n);
        println!("read misses by region: RAM {:.0}, flash {:.0}, ROM8 {:.0}, ROME {:.0}",
            s.by_region[0] as f64 / n, s.by_region[1] as f64 / n, s.by_region[2] as f64 / n, s.by_region[3] as f64 / n);
        let est = s.read_misses as f64 / n * 0.9 + s.store_lines as f64 / n * 0.56;
        println!("estimated stall time: {:.1} ms/frame (misses x 0.9 us + stores x 0.56 us)", est / 1000.0);
        let mut pages: Vec<_> = s.stores_by_page.iter().collect();
        pages.sort_by(|a, b| b.1.cmp(a.1));
        let total: u64 = s.store_lines;
        print!("top store pages (256 B):");
        for (p, k) in pages.iter().take(8) { print!(" {:05X}:{:.0}%", **p << 8, **k as f64 * 100.0 / total as f64); }
        println!();
    });
}
