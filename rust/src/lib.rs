//! C ABI over a `no_std` `bbkemu-core` for the Playdate frontend.
//!
//! See `../include/bbkemu_pd.h` for the contract. The crate allocates through
//! the C heap (which the SDK routes to `playdate->system->realloc`) and panics
//! by reporting through `bbk_pd_panic`, which never returns.

#![no_std]

extern crate alloc;

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;
use core::alloc::{GlobalAlloc, Layout};
use core::ffi::{c_char, c_void};
use core::fmt::{self, Write};
use core::{ptr, slice};

use bbkemu_core::input::BbkKey;
use bbkemu_core::lcd::{LCD_HEIGHT, LCD_WIDTH};
use bbkemu_core::model::{MODEL_4980, MODEL_4988};
use bbkemu_core::save::SaveState;
use bbkemu_core::Emulator;
use mos6502::registers::Status;

const PIXELS: usize = LCD_WIDTH * LCD_HEIGHT;
const BATTERY_MAGIC: &[u8; 8] = b"BBKBAT1\0";

extern "C" {
    fn malloc(size: usize) -> *mut c_void;
    fn realloc(ptr: *mut c_void, size: usize) -> *mut c_void;
    fn free(ptr: *mut c_void);
    fn bbk_pd_panic(message: *const c_char);
}

// MARK: Runtime

/// Alignment the C heap guarantees: newlib on device promises 8, but stay
/// conservative there; macOS (Simulator) gives 16.
const MIN_ALIGN: usize = if cfg!(target_pointer_width = "32") { 4 } else { 16 };

struct CHeap;

unsafe impl GlobalAlloc for CHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if layout.align() <= MIN_ALIGN {
            return malloc(layout.size()) as *mut u8;
        }
        // Over-allocate and stash the original pointer just below the aligned block.
        let raw = malloc(layout.size() + layout.align()) as *mut u8;
        if raw.is_null() {
            return raw;
        }
        let aligned = (raw as usize + layout.align()) & !(layout.align() - 1);
        let aligned = aligned as *mut u8;
        (aligned as *mut *mut u8).sub(1).write_unaligned(raw);
        aligned
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if layout.align() <= MIN_ALIGN {
            free(ptr as *mut c_void);
        } else {
            free((ptr as *mut *mut u8).sub(1).read_unaligned() as *mut c_void);
        }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if layout.align() <= MIN_ALIGN {
            return realloc(ptr as *mut c_void, new_size) as *mut u8;
        }
        let new = self.alloc(Layout::from_size_align_unchecked(new_size, layout.align()));
        if !new.is_null() {
            ptr::copy_nonoverlapping(ptr, new, layout.size().min(new_size));
            self.dealloc(ptr, layout);
        }
        new
    }
}

#[global_allocator]
static HEAP: CHeap = CHeap;

/// Fixed buffer for formatting panic messages without allocating.
struct MessageBuf {
    buf: [u8; 256],
    len: usize,
}

impl Write for MessageBuf {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let room = self.buf.len() - 1 - self.len;
        let n = s.len().min(room);
        self.buf[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        Ok(())
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    let mut msg = MessageBuf { buf: [0; 256], len: 0 };
    let _ = write!(msg, "BBKEmu core panic: {}", info.message());
    if let Some(loc) = info.location() {
        let _ = write!(msg, " ({}:{})", loc.file(), loc.line());
    }
    msg.buf[msg.len] = 0;
    unsafe { bbk_pd_panic(msg.buf.as_ptr() as *const c_char) };
    loop {}
}

/// The Simulator links against the host's prebuilt `core`, which references
/// an unwinding personality even though this crate aborts on panic.
#[cfg(not(target_os = "none"))]
#[no_mangle]
pub extern "C" fn rust_eh_personality() {}

// MARK: Emulator handle

pub struct BBKEmulator {
    emu: Emulator,
    /// Flash contents right after the game was loaded; battery saves are
    /// stored as a diff against this so patched .gam files stay intact.
    pristine_flash: Vec<u8>,
    /// Decoded LCD, kept on the heap: the Playdate's game stack is small.
    pixels: Box<[bool; PIXELS]>,
    /// Per-pixel darkness, 0 (clear) to 255 (fully on), for ghosting.
    intensity: Vec<u8>,
    /// Pixels as of the last portrait render, and whether each source row's
    /// ghosting had fully settled then: unchanged settled rows are skipped.
    drawn: Box<[bool; PIXELS]>,
    settled: [bool; LCD_HEIGHT],
    /// Redraw every row next time (the frame buffer was cleared).
    redraw_all: bool,
    /// The LCD's RAM (0x400..0x1000, plus 0x1000 which is copied over 0x400)
    /// as of the last portrait render: if it hasn't changed and every row has
    /// settled, there is nothing to draw.
    lcd_ram: Vec<u8>,
    /// Backing store for the last `bbk_battery_export` / `bbk_state_save`.
    export: Vec<u8>,
    /// The last battery diff and the flash write count it was taken at, so an
    /// unchanged flash isn't diffed again (2 MB of slow memory on device).
    battery: Option<(u32, Vec<u8>)>,
}

unsafe fn emu_mut<'a>(emu: *mut BBKEmulator) -> Option<&'a mut BBKEmulator> {
    emu.as_mut()
}

unsafe fn bytes<'a>(data: *const u8, len: usize) -> &'a [u8] {
    if data.is_null() || len == 0 {
        &[]
    } else {
        slice::from_raw_parts(data, len)
    }
}

/// Hands `data` back to C through an internal buffer valid until the next export.
unsafe fn export(e: &mut BBKEmulator, data: Vec<u8>, len: *mut usize) -> *const u8 {
    e.export = data;
    if !len.is_null() {
        *len = e.export.len();
    }
    e.export.as_ptr()
}

#[no_mangle]
pub extern "C" fn bbk_create(model: u32) -> *mut BBKEmulator {
    let model = if model == 1 { &MODEL_4988 } else { &MODEL_4980 };
    Box::into_raw(Box::new(BBKEmulator {
        emu: Emulator::new(model),
        pristine_flash: Vec::new(),
        pixels: vec![false; PIXELS].into_boxed_slice().try_into().unwrap(),
        intensity: vec![0; PIXELS],
        drawn: vec![false; PIXELS].into_boxed_slice().try_into().unwrap(),
        settled: [false; LCD_HEIGHT],
        redraw_all: true,
        lcd_ram: vec![0; LCD_RAM.len() + 1],
        export: Vec::new(),
        battery: None,
    }))
}

/// # Safety
/// `emu` must come from `bbk_create` and not be used afterwards.
#[no_mangle]
pub unsafe extern "C" fn bbk_destroy(emu: *mut BBKEmulator) {
    if !emu.is_null() {
        drop(Box::from_raw(emu));
    }
}

/// # Safety
/// `data` must point to `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn bbk_load_rom8(emu: *mut BBKEmulator, data: *const u8, len: usize) {
    if let Some(e) = emu_mut(emu) {
        e.emu.load_rom_8(bytes(data, len));
    }
}

/// # Safety
/// `data` must point to `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn bbk_load_rome(emu: *mut BBKEmulator, data: *const u8, len: usize) {
    if let Some(e) = emu_mut(emu) {
        e.emu.load_rom_e(bytes(data, len));
    }
}

/// # Safety
/// `data` must point to `len` readable bytes; `err` to `err_len` writable bytes or be NULL.
#[no_mangle]
pub unsafe extern "C" fn bbk_load_game(
    emu: *mut BBKEmulator,
    data: *const u8,
    len: usize,
    err: *mut c_char,
    err_len: usize,
) -> bool {
    let Some(e) = emu_mut(emu) else { return false };
    match e.emu.load_gam(bytes(data, len)) {
        Ok(()) => {
            e.pristine_flash = e.emu.cpu.memory().flash.clone();
            true
        }
        Err(error) => {
            if !err.is_null() && err_len > 0 {
                let message = error.0.as_bytes();
                let n = message.len().min(err_len - 1);
                ptr::copy_nonoverlapping(message.as_ptr(), err as *mut u8, n);
                *err.add(n) = 0;
            }
            false
        }
    }
}

/// Runs every instruction through mos6502 instead of the fast path (for comparison).
///
/// # Safety
/// `emu` must be a live handle or NULL.
#[no_mangle]
pub unsafe extern "C" fn bbk_set_reference(emu: *mut BBKEmulator, reference: bool) {
    if let Some(e) = emu_mut(emu) {
        e.emu.cpu.reference = reference;
    }
}

/// Called before each frame of `bbk_run_frames` with the frame's index.
pub type FrameHook = unsafe extern "C" fn(userdata: *mut c_void, frame: u32);

/// Runs up to `count` frames, stopping early if the game exits; returns the
/// number run. `hook` (if any) runs before each frame and may call the input
/// functions.
///
/// # Safety
/// `emu` must be a live handle or NULL.
#[no_mangle]
pub unsafe extern "C" fn bbk_run_frames(
    emu: *mut BBKEmulator,
    count: u32,
    hook: Option<FrameHook>,
    userdata: *mut c_void,
) -> u32 {
    let Some(e) = emu_mut(emu) else { return 0 };
    let mut ran = 0;
    while ran < count && e.emu.is_running() {
        if let Some(hook) = hook {
            hook(userdata, ran);
        }
        e.emu.run_frame();
        ran += 1;
    }
    ran
}


/// # Safety
/// `emu` must be a live handle or NULL.
#[no_mangle]
pub unsafe extern "C" fn bbk_is_running(emu: *const BBKEmulator) -> bool {
    emu.as_ref().is_some_and(|e| e.emu.is_running())
}

/// # Safety
/// `emu` must be a live handle or NULL.
#[no_mangle]
pub unsafe extern "C" fn bbk_key_down(emu: *mut BBKEmulator, code: u8) {
    if let (Some(e), Some(key)) = (emu_mut(emu), BbkKey::from_code(code)) {
        e.emu.key_down(key);
    }
}

/// # Safety
/// `emu` must be a live handle or NULL.
#[no_mangle]
pub unsafe extern "C" fn bbk_key_up(emu: *mut BBKEmulator) {
    if let Some(e) = emu_mut(emu) {
        e.emu.key_up();
    }
}

/// # Safety
/// `emu` must be a live handle or NULL.
#[no_mangle]
pub unsafe extern "C" fn bbk_set_cpu_rate(emu: *mut BBKEmulator, rate: f32) {
    if let Some(e) = emu_mut(emu) {
        e.emu.set_cpu_rate(rate);
    }
}

// MARK: Benchmarks

/// Device microbenchmark: runs a small 6502 loop. `mode` 0 calls the fast path
/// directly `count` times; mode 1 runs `count` frames of the emulator's frame
/// loop over the same program. Returns instructions executed (mode 0) or 6502
/// cycles run (mode 1).
#[doc(hidden)]
#[no_mangle]
pub extern "C" fn bbk_bench_interpreter(mode: u32, count: u32) -> u32 {
    use bbkemu_core::fast6502::{step, Regs};
    // loop: LDA $10; CLC; ADC #1; STA $10; INX; BNE loop; JMP loop
    const PROGRAM: [u8; 13] = [0xA5, 0x10, 0x18, 0x69, 0x01, 0x85, 0x10, 0xE8, 0xD0, 0xF6, 0x4C, 0x00, 0x04];
    let mut emu = Box::new(Emulator::new(&MODEL_4980));
    let m = emu.cpu.memory_mut();
    m.init();
    m.ram[0x400..0x400 + PROGRAM.len()].copy_from_slice(&PROGRAM);
    m.ram[0x200] = 0; // not halted
    let mut r = Regs { a: 0, x: 0, y: 0, s: 0xFF, p: 0x24, pc: 0x0400 }; // interrupts disabled
    if mode == 0 {
        let m = emu.cpu.memory_mut();
        for _ in 0..count {
            step(&mut r, m);
        }
        count
    } else {
        r.store(&mut emu.cpu.inner);
        emu.set_running_for_bench();
        let before = emu.cpu.cycles();
        for _ in 0..count {
            emu.run_frame();
        }
        (emu.cpu.cycles() - before) as u32
    }
}

// MARK: Rendering

/// The part of RAM the LCD is decoded from (render_lcd_into also reads 0x1000).
const LCD_RAM: core::ops::Range<usize> = 0x400..0x1000;

/// 4x4 ordered-dither thresholds, used to show ghosting on the 1-bit screen.
const BAYER4: [u8; 16] = [0, 8, 2, 10, 12, 4, 14, 6, 3, 11, 1, 9, 15, 7, 13, 5];

/// Renders the LCD into a 1-bit frame buffer (MSB first, 1 = white).
///
/// Portrait draws 2x (318x192); landscape rotates clockwise and draws 1.5x
/// (144x238). `x0` must be a multiple of 8. `ghosting` is how much of the
/// previous frame persists, 0-242 out of 256. Returns the changed row range
/// through `first_row`/`last_row` (first > last when nothing changed).
///
/// # Safety
/// `frame` must cover every row and byte the image touches at `rowbytes` stride.
#[no_mangle]
pub unsafe extern "C" fn bbk_render(
    emu: *mut BBKEmulator,
    frame: *mut u8,
    rowbytes: usize,
    x0: u32,
    y0: u32,
    landscape: bool,
    ghosting: u8,
    first_row: *mut i32,
    last_row: *mut i32,
) {
    let Some(e) = emu_mut(emu) else { return };
    if frame.is_null() {
        return;
    }
    if !landscape && !e.redraw_all && e.settled.iter().all(|&s| s) {
        let ram = &e.emu.cpu.memory().ram;
        if ram[LCD_RAM] == e.lcd_ram[..LCD_RAM.len()] && ram[0x1000] == e.lcd_ram[LCD_RAM.len()] {
            if !first_row.is_null() {
                *first_row = i32::MAX;
            }
            if !last_row.is_null() {
                *last_row = i32::MIN;
            }
            return;
        }
    }
    {
        let ram = &e.emu.cpu.memory().ram;
        let n = LCD_RAM.len();
        e.lcd_ram[..n].copy_from_slice(&ram[LCD_RAM]);
        e.lcd_ram[n] = ram[0x1000];
    }
    e.emu.render_lcd_into(&mut e.pixels);
    let (first, last) = if landscape {
        e.redraw_all = true; // portrait's row tracking doesn't cover this path
        render_landscape(e, frame, rowbytes, x0, y0, ghosting)
    } else {
        render_portrait(e, frame, rowbytes, x0, y0, ghosting)
    };
    if !first_row.is_null() {
        *first_row = first;
    }
    if !last_row.is_null() {
        *last_row = last;
    }
}

/// Moves `level` toward `target` (0 or 255), keeping `keep`/256 of the gap;
/// snaps the last few steps so rows can settle exactly.
#[inline(always)]
fn fade(level: u8, target: i32, keep: i32) -> u8 {
    let next = target + (((level as i32 - target) * keep) >> 8);
    if (next - target).abs() < 4 {
        target as u8
    } else {
        next as u8
    }
}

/// Writes `bits` (MSB first, `width` pixels) into an output row, returning
/// whether anything changed.
unsafe fn store_row(row: &mut [u8], bits: &[u8], width: usize) -> bool {
    let full = width / 8;
    let mut changed = row[..full] != bits[..full];
    row[..full].copy_from_slice(&bits[..full]);
    let tail = width & 7;
    if tail != 0 {
        let mask = 0xFFu8 << (8 - tail);
        let merged = (row[full] & !mask) | (bits[full] & mask);
        changed |= row[full] != merged;
        row[full] = merged;
    }
    changed
}

/// 2x portrait, row by row: source rows that haven't changed since the last
/// render and whose ghosting has settled are skipped; settled rows skip the
/// dither.
unsafe fn render_portrait(e: &mut BBKEmulator, frame: *mut u8, rowbytes: usize, x0: u32, y0: u32, ghosting: u8) -> (i32, i32) {
    const WIDTH: usize = LCD_WIDTH * 2;
    let keep = ghosting.min(242) as i32;
    let x_byte = (x0 / 8) as usize;
    let mut first = i32::MAX;
    let mut last = i32::MIN;
    let mut bits = [0u8; WIDTH.div_ceil(8)];
    for sy in 0..LCD_HEIGHT {
        let span = sy * LCD_WIDTH..(sy + 1) * LCD_WIDTH;
        let src = &e.pixels[span.clone()];
        if !e.redraw_all && e.settled[sy] && *src == e.drawn[span.clone()] {
            continue;
        }
        let mut settled = true;
        for (level, &on) in e.intensity[span.clone()].iter_mut().zip(src) {
            let target = if on { 255 } else { 0 };
            *level = fade(*level, target, keep);
            settled &= *level as i32 == target;
        }
        e.settled[sy] = settled;
        e.drawn[span.clone()].copy_from_slice(src);
        let levels = &e.intensity[span];
        for half in 0..2 {
            let r = sy * 2 + half;
            if half == 0 || !settled {
                // Settled rows are pure black/white: both output rows match.
                let dither = &BAYER4[(r & 3) * 4..(r & 3) * 4 + 4];
                let mut acc = 0u8;
                for c in 0..WIDTH {
                    let level = levels[c / 2];
                    let white = if settled { level == 0 } else { level <= dither[c & 3] * 16 + 8 };
                    acc = (acc << 1) | white as u8;
                    if c & 7 == 7 {
                        bits[c / 8] = acc;
                    }
                }
                bits[WIDTH / 8] = acc << (8 - (WIDTH & 7));
            }
            let y = y0 as usize + r;
            let row = slice::from_raw_parts_mut(frame.add(y * rowbytes + x_byte), WIDTH.div_ceil(8));
            if store_row(row, &bits, WIDTH) {
                first = first.min(y as i32);
                last = last.max(y as i32);
            }
        }
    }
    e.redraw_all = false;
    (first, last)
}

/// Rotated 1.5x landscape: redraws everything each time.
unsafe fn render_landscape(e: &mut BBKEmulator, frame: *mut u8, rowbytes: usize, x0: u32, y0: u32, ghosting: u8) -> (i32, i32) {
    let landscape = true;
    let keep = ghosting.min(242) as i32;
    for (level, &on) in e.intensity.iter_mut().zip(e.pixels.iter()) {
        let target = if on { 255 } else { 0 };
        *level = (target + (((*level as i32 - target) * keep) >> 8)) as u8;
    }

    // Source index = row_term[r] + col_term[c] for each output pixel.
    let (width, height) = if landscape { (144, 238) } else { (318, 192) };
    let mut col_term = [0u16; 318];
    let mut row_term = [0u16; 238];
    for c in 0..width {
        col_term[c] = if landscape {
            ((LCD_HEIGHT - 1 - c * 2 / 3) * LCD_WIDTH) as u16
        } else {
            (c / 2) as u16
        };
    }
    for r in 0..height {
        row_term[r] = if landscape {
            (r * 2 / 3) as u16
        } else {
            (r / 2 * LCD_WIDTH) as u16
        };
    }

    let mut first = i32::MAX;
    let mut last = i32::MIN;
    let x_byte = (x0 / 8) as usize;
    for r in 0..height {
        let y = y0 as usize + r;
        let row = slice::from_raw_parts_mut(frame.add(y * rowbytes + x_byte), width.div_ceil(8));
        let base = row_term[r] as usize;
        let dither = &BAYER4[(r & 3) * 4..(r & 3) * 4 + 4];
        let mut changed = false;
        let mut bits = 0u8;
        for c in 0..width {
            let level = e.intensity[base + col_term[c] as usize];
            let white = level <= dither[c & 3] * 16 + 8;
            bits = (bits << 1) | white as u8;
            if c & 7 == 7 {
                changed |= row[c / 8] != bits;
                row[c / 8] = bits;
            }
        }
        let tail = width & 7;
        if tail != 0 {
            let shift = 8 - tail;
            let mask = 0xFFu8 << shift;
            let byte = &mut row[width / 8];
            let merged = (*byte & !mask) | (bits << shift);
            changed |= *byte != merged;
            *byte = merged;
        }
        if changed {
            first = first.min(y as i32);
            last = last.max(y as i32);
        }
    }
    (first, last)
}

/// Forgets ghosting history so the next frame draws crisp.
///
/// # Safety
/// `emu` must be a live handle or NULL.
#[no_mangle]
pub unsafe extern "C" fn bbk_reset_ghosting(emu: *mut BBKEmulator) {
    if let Some(e) = emu_mut(emu) {
        e.intensity.fill(0);
        e.redraw_all = true;
    }
}

// MARK: Saves

/// Diff of the current flash against the post-load snapshot, as
/// `MAGIC, then repeated [u32 offset][u32 len][bytes]` (little endian).
/// Same format as the macOS app's battery saves.
fn battery_diff(e: &BBKEmulator) -> Vec<u8> {
    let flash = &e.emu.cpu.memory().flash;
    let base = &e.pristine_flash;
    let mut out = BATTERY_MAGIC.to_vec();
    if base.len() != flash.len() {
        return out;
    }
    let mut i = 0;
    while i < flash.len() {
        if flash[i] == base[i] {
            i += 1;
            continue;
        }
        let start = i;
        // Merge runs separated by short equal gaps to keep the record count low.
        let mut end = i + 1;
        let mut gap = 0;
        let mut j = end;
        while j < flash.len() && gap < 16 {
            if flash[j] != base[j] {
                end = j + 1;
                gap = 0;
            } else {
                gap += 1;
            }
            j += 1;
        }
        out.extend_from_slice(&(start as u32).to_le_bytes());
        out.extend_from_slice(&((end - start) as u32).to_le_bytes());
        out.extend_from_slice(&flash[start..end]);
        i = end;
    }
    out
}

/// Returns the battery save; the pointer stays valid until the next export.
///
/// # Safety
/// `len` must be writable.
#[no_mangle]
pub unsafe extern "C" fn bbk_battery_export(emu: *mut BBKEmulator, len: *mut usize) -> *const u8 {
    let Some(e) = emu_mut(emu) else { return ptr::null() };
    if e.pristine_flash.is_empty() {
        return ptr::null();
    }
    let writes = e.emu.cpu.memory().flash_writes;
    let diff = match &e.battery {
        Some((at, diff)) if *at == writes => diff.clone(),
        _ => {
            let diff = battery_diff(e);
            e.battery = Some((writes, diff.clone()));
            diff
        }
    };
    export(e, diff, len)
}

/// # Safety
/// `data` must point to `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn bbk_battery_import(emu: *mut BBKEmulator, data: *const u8, len: usize) -> bool {
    let Some(e) = emu_mut(emu) else { return false };
    let data = bytes(data, len);
    if data.len() < 8 || &data[..8] != BATTERY_MAGIC {
        return false;
    }
    // Validate the whole file before touching flash.
    let flash_len = e.emu.cpu.memory().flash.len();
    let mut records = Vec::new();
    let mut p = 8;
    while p < data.len() {
        if p + 8 > data.len() {
            return false;
        }
        let off = u32::from_le_bytes(data[p..p + 4].try_into().unwrap()) as usize;
        let n = u32::from_le_bytes(data[p + 4..p + 8].try_into().unwrap()) as usize;
        p += 8;
        if p + n > data.len() || off + n > flash_len {
            return false;
        }
        records.push((off, &data[p..p + n]));
        p += n;
    }
    let flash = &mut e.emu.cpu.memory_mut().flash;
    for (off, chunk) in records {
        flash[off..off + chunk.len()].copy_from_slice(chunk);
    }
    e.battery = None;
    true
}

/// Returns a save state in upstream's `SaveState` format (the same bytes the
/// macOS app and libretro core write); valid until the next export.
///
/// # Safety
/// `len` must be writable.
#[no_mangle]
pub unsafe extern "C" fn bbk_state_save(emu: *mut BBKEmulator, len: *mut usize) -> *const u8 {
    let Some(e) = emu_mut(emu) else { return ptr::null() };
    let state = e.emu.save_state().to_bytes();
    export(e, state, len)
}

/// # Safety
/// `data` must point to `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn bbk_state_load(emu: *mut BBKEmulator, data: *const u8, len: usize) -> bool {
    let Some(e) = emu_mut(emu) else { return false };
    let Ok(state) = SaveState::from_bytes(bytes(data, len)) else { return false };
    if state.bank_sys_d != e.emu.model().bank_sys_d || state.ram.len() != e.emu.cpu.memory().ram.len() {
        return false;
    }
    if e.emu.load_save_state(&state).is_err() {
        return false;
    }
    // Upstream only restores PC/SP; restore the rest of the CPU and the
    // bank mapping so states taken mid-routine resume correctly.
    let regs = &mut e.emu.cpu.inner.registers;
    regs.accumulator = state.cpu.a;
    regs.index_x = state.cpu.x;
    regs.index_y = state.cpu.y;
    regs.status = Status::from_bits_truncate(state.cpu.status);
    let banks = &mut e.emu.cpu.memory_mut().bank_switch;
    for (dst, &src) in banks.banks.iter_mut().zip(state.bank_switch.banks.iter()) {
        *dst = src;
    }
    banks.set_selected(state.bank_switch.selected);
    e.intensity.fill(0);
    e.redraw_all = true;
    e.battery = None;
    true
}

/// Frees the buffer behind the last export.
///
/// # Safety
/// `emu` must be a live handle or NULL.
#[no_mangle]
pub unsafe extern "C" fn bbk_release_export(emu: *mut BBKEmulator) {
    if let Some(e) = emu_mut(emu) {
        e.export = Vec::new();
    }
}
