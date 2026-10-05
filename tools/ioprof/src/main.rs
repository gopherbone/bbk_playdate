//! Runs a game headless, tapping keys, and tallies writes to the I/O pages
//! ($00-$FF and $200-$2FF): counts, distinct values, and the most written
//! registers per frame window. Usage: ROMS=dir ioprof game.gam [frames]
use bbkemu_core::input::BbkKey;
use bbkemu_core::memory;
use bbkemu_core::model::MODEL_4980;
use bbkemu_core::Emulator;
use std::cell::RefCell;
use std::collections::BTreeMap;

#[derive(Default)]
struct Reg { writes: u64, values: BTreeMap<u8, u64>, frames: std::collections::BTreeSet<u64>, max_per_frame: u64, this_frame: u64 }

thread_local! {
    static EXEC: RefCell<BTreeMap<u32, u64>> = RefCell::new(BTreeMap::new());
    static READS: RefCell<BTreeMap<u16, u64>> = RefCell::new(BTreeMap::new());
    static REGS: RefCell<BTreeMap<u16, Reg>> = RefCell::new(BTreeMap::new());
    static FRAME: RefCell<u64> = RefCell::new(0);
}

fn on_exec(_pc: u16, phys: u32) {
    EXEC.with(|x| *x.borrow_mut().entry(phys).or_default() += 1);
}

fn on_read(addr: u16) {
    if addr < 0x20 || (0x200..0x300).contains(&addr) {
        READS.with(|r| *r.borrow_mut().entry(addr).or_default() += 1);
    }
}

fn on_write(addr: u16, val: u8) {
    let page2_only = std::env::var("PAGE2").is_ok();
    if !((!page2_only && addr < 0x100) || (0x200..0x300).contains(&addr)) { return; }
    let f = FRAME.with(|f| *f.borrow());
    REGS.with(|r| {
        let mut r = r.borrow_mut();
        let e = r.entry(addr).or_default();
        e.writes += 1;
        *e.values.entry(val).or_default() += 1;
        if e.frames.insert(f) { e.this_frame = 0; }
        e.this_frame += 1;
        e.max_per_frame = e.max_per_frame.max(e.this_frame);
    });
}

/// Writes the LCD as a PBM (2x) for looking at.
fn dump(e: &mut Emulator, name: &str) {
    let px = e.render_lcd_buffer();
    let mut out = format!("P1\n{} {}\n", 159 * 2, 96 * 2).into_bytes();
    for y in 0..96 * 2 {
        for x in 0..159 * 2 { out.push(if px[(y / 2) * 159 + x / 2] { b'1' } else { b'0' }); out.push(b' '); }
        out.push(b'\n');
    }
    std::fs::write(format!("{name}.pbm"), out).unwrap();
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let frames: u64 = args.get(2).map_or(3000, |a| a.parse().unwrap());
    let rom = std::env::var("ROMS").unwrap();
    let mut e = Emulator::new(&MODEL_4980);
    e.load_rom_8(&std::fs::read(format!("{rom}/8.BIN")).unwrap());
    e.load_rom_e(&std::fs::read(format!("{rom}/E.BIN")).unwrap());
    e.load_gam(&std::fs::read(&args[1]).unwrap()).unwrap();
    unsafe { memory::WRITE_HOOK = Some(on_write) };
    if let Ok(script) = std::env::var("SCRIPT") {
        // Script: space-separated steps. KEY[*n] taps a key (hold 3, wait 40),
        // wN runs N frames, sNAME saves the screen to NAME.pbm, mark starts the tally.
        unsafe { memory::WRITE_HOOK = None };
        let mut f = 0u64;
        let mut run = |e: &mut Emulator, n: u64, f: &mut u64| for _ in 0..n { FRAME.with(|x| *x.borrow_mut() = *f); e.run_frame(); *f += 1; };
        for step in script.split_whitespace() {
            if let Some(n) = step.strip_prefix('w') { run(&mut e, n.parse().unwrap(), &mut f); continue; }
            if let Some(name) = step.strip_prefix('s') { dump(&mut e, name); continue; }
            if let Some(pk) = step.strip_prefix('p') {
                // pADDR=VAL pokes RAM (hex), e.g. p1A97=01
                let (ad, v) = pk.split_once('=').unwrap();
                let ad = usize::from_str_radix(ad, 16).unwrap();
                e.cpu.memory_mut().ram[ad] = u8::from_str_radix(v, 16).unwrap();
                continue;
            }
            if step == "mark" { unsafe { memory::WRITE_HOOK = Some(on_write); memory::READ_HOOK = Some(on_read); memory::EXEC_HOOK = Some(on_exec) }; continue; }
            let (k, times) = step.split_once('*').map_or((step, 1), |(k, n)| (k, n.parse().unwrap()));
            let key = match k { "ENTER" => BbkKey::Enter, "EXIT" => BbkKey::Exit, "UP" => BbkKey::Up, "DOWN" => BbkKey::Down,
                "LEFT" => BbkKey::Left, "RIGHT" => BbkKey::Right, other => panic!("key {other}") };
            for _ in 0..times { e.key_down(key); run(&mut e, 3, &mut f); e.key_up(); run(&mut e, 40, &mut f); }
        }
    } else {
        let keys = [BbkKey::Enter, BbkKey::Down, BbkKey::Enter, BbkKey::Up, BbkKey::Left, BbkKey::Right, BbkKey::Enter];
        for f in 0..frames {
            FRAME.with(|x| *x.borrow_mut() = f);
            if f % 90 == 45 { e.key_down(keys[(f / 90) as usize % keys.len()]); }
            if f % 90 == 50 { e.key_up(); }
            e.run_frame();
        }
    }
    println!("banks: {:03X?}", e.cpu.memory().bank_switch.banks);
    if let Ok(path) = std::env::var("EXEC_OUT") {
        EXEC.with(|x| {
            let s: String = x.borrow().iter().map(|(a, c)| format!("{a:06X} {c}\n")).collect();
            std::fs::write(path, s).unwrap();
        });
    }
    READS.with(|r| {
        let line: Vec<String> = r.borrow().iter().map(|(a, c)| format!("${a:03X}:{c}")).collect();
        println!("reads: {}", line.join(" "));
    });
    REGS.with(|r| {
        let r = r.borrow();
        let mut v: Vec<_> = r.iter().collect();
        v.sort_by(|a, b| b.1.writes.cmp(&a.1.writes));
        println!("{:>6} {:>9} {:>7} {:>9}  values (top)", "addr", "writes", "frames", "max/frm");
        for (a, reg) in v.iter().take(if std::env::var("ALL").is_ok() { 10000 } else if std::env::var("PAGE2").is_ok() { 12 } else { 40 }) {
            let mut vals: Vec<_> = reg.values.iter().collect();
            vals.sort_by(|x, y| y.1.cmp(x.1));
            let top: Vec<String> = vals.iter().take(6).map(|(k, c)| format!("{k:02X}x{c}")).collect();
            println!("{:>6} {:>9} {:>7} {:>9}  {} ({} distinct)", format!("${a:03X}"), reg.writes, reg.frames.len(), reg.max_per_frame, top.join(" "), reg.values.len());
        }
    });
}
