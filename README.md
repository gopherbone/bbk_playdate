# BBKEmu for Playdate

Play BBK (步步高) A4980/A4988 electronic-dictionary games on a [Playdate](https://play.date).
It runs [BBKEmu](https://github.com/AloysHF/BBKEmu)'s emulator core (pulled in unmodified as
the `upstream/` submodule, then patched at build time to run without the Rust standard library)
behind a small C frontend.

It comes ready to play: the A4980 system ROMs and **Demonbane Chronicle v0.2**, the English
translation of 伏魔记, are bundled in the release.

| | |
|---|---|
| ![Game list with Chinese titles](docs/game-list.png) | ![Demonbane Chronicle title screen](docs/demonbane-title.png) |
| ![Demonbane Chronicle prologue](docs/demonbane-prologue.png) | ![新仙剑奇侠传 title screen](docs/xianjian-title.png) |
| ![新仙剑奇侠传 in game](docs/xianjian-game.png) | ![搬运工 (Sokoban)](docs/sokoban.png) |

- The 159×96 LCD is drawn at 2× in portrait, or rotated at 1.5× for landscape games, with
  optional LCD ghosting shown by dithering.
- Game list with Chinese titles, drawn with the dictionary's own font from `8.BIN`.
- Battery saves are written automatically; 3 save-state slots per game.
- An on-screen keypad (crank or D-pad) for every BBK key.

## Install

Download `BBKEmu.pdx.zip` from the [latest release](../../releases/latest) and either sideload it at
[play.date/account/sideload](https://play.date/account/sideload/), or unzip it into the `Games`
folder of the Playdate's data disk.

Demonbane Chronicle is in the game list straight away. To add more games, run BBKEmu once so it
creates its folders, then reboot the Playdate to its data disk (Settings → System → Reboot to
Data Disk) and open the folder in `Data/` ending in `com.gopherbone.bbkemu` (sideloaded games
get a `user.` prefix):

| Put this | Here |
|---|---|
| More games | `Games/*.gam` |
| A4988 system ROMs, for games that need that model | `ROMs/A4988/8.BIN` and `E.BIN` |

Files in the Data folder take priority over the bundled ones, so you can also drop in your own
A4980 ROMs or a newer build of the translation. Saves go to `Saves/<game>.bbksav` and states to
`States/<game>/slotN.state`. This is the same layout and file format as the macOS BBKEmu
frontend's `~/Library/Application Support/BBKEmu`, so ROMs, saves and states can be copied
between them.

## Controls

| Playdate | BBK |
|---|---|
| D-pad | Arrow keys |
| Ⓐ | Enter |
| Ⓑ | Exit |
| Menu → **keypad** | Any key: move with the crank or D-pad, Ⓐ presses it |
| Menu → **options** | Save/load state, state slot, portrait/landscape, ghosting, model, performance overlay, reset |
| Menu → **game list** | Back to the list (saves first) |

For landscape games, hold the Playdate with the crank on top; the D-pad turns with it.

Each game is set to the A4980 by default (or whichever model has ROMs installed); switch it in
options if a game needs the A4988.

## Performance

Not full speed yet. The dictionaries ran a 6502 at 4 MHz, about 21,000 instructions per 60 Hz
frame. On a Rev B Playdate, emulating a frame of Demonbane's title screen takes about 39 ms,
against a budget of 16.7 ms, so games run at roughly 40% speed. The unmodified core took about
194 ms.

The Playdate's main memory is slow external PSRAM: a cache miss costs about 0.9 µs and every
store to a new cache line about 0.56 µs. The game task's stack is in fast tightly-coupled memory,
but there is only about 8 KB of it. The speedups so far:

- **A compact fast path for the documented 6502 opcodes** (`tools/gen_fast6502.py`) instead of
  mos6502's generic decoder. Everything it doesn't cover still runs through mos6502.
- **A frame loop that keeps CPU registers, cycle counts and timer state in locals**, writing
  them back only for interrupts, BRK and fallbacks.
- **Inlined fast paths for RAM, flash and ROM reads and writes.**
- **`opt-level = "s"`.** Smaller code measured faster than `-O3`.

`make check` compares all of these against the original code: memory accesses exhaustively,
every opcode from thousands of random states, and whole games in lockstep against the original
loop running on mos6502. Turn on **Show performance** in options to see the frame cost on your
device. Its `bench` figure averages emulated frames 300 to 599, so builds can be compared on
identical work.

## Building

Requires the [Playdate SDK](https://play.date/dev/), Rust stable with the
`thumbv7em-none-eabihf` target, and Arm's GNU toolchain (Homebrew's `arm-none-eabi-gcc` lacks
the C library):

```sh
git clone --recursive git@github.com:gopherbone/bbk_playdate.git
cd bbk_playdate
rustup target add thumbv7em-none-eabihf
brew install --cask gcc-arm-embedded   # Linux: apt install gcc-arm-none-eabi libnewlib-arm-none-eabi

make            # BBKEmu.pdx for the device and the Simulator
make run        # open it in the Playdate Simulator
make install    # copy it to a USB-connected, unlocked Playdate and launch it
```

Set `ARM_TOOLCHAIN=/path/to/bin` if the Arm toolchain isn't under `/Applications/ArmGNUToolchain`.

### Layout

- `patches/bbkemu-core-nostd.patch`: makes upstream `bbkemu-core` build without `std`: `alloc`
  collections, a small error type in place of `anyhow`, and a hand-written save-state codec that
  writes the same bytes as upstream's `bincode` one. The Makefile applies it to a copy in `gen/`.
- `rust/`: C ABI over the core as a static library: allocator on the Playdate heap, 1-bit
  renderer with dithered ghosting, battery saves and save states.
- `src/main.c`: the frontend: game list, emulation loop, keypad, options.
- `src/gb2312_table.h`: Unicode → GB2312 glyph lookup, generated by `tools/gen_gb2312.py`.
- `Source/`: copied into the `.pdx` by `pdc`: the bundled ROMs (`ROMs/A4980/`) and games
  (`Bundled/`; a separate folder because the Data folder's `Games/` hides the pdx's own).
- `docs/`: screenshots, rendered from the emulator's frame buffer.

## License

The code is GPL-3.0-or-later, the same as BBKEmu; see [LICENSE](LICENSE). The bundled BBK system
ROMs and 伏魔记 belong to BBK and are included as abandonware; Demonbane Chronicle is a fan
translation.
