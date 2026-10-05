// C API of the Rust glue in ../rust (libbbkemu_pd.a).
#pragma once

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

typedef struct BBKEmulator BBKEmulator;

// model: 0 = A4980, 1 = A4988.
BBKEmulator* bbk_create(uint32_t model);
void bbk_destroy(BBKEmulator* emu);

void bbk_load_rom8(BBKEmulator* emu, const uint8_t* data, size_t len);
void bbk_load_rome(BBKEmulator* emu, const uint8_t* data, size_t len);
bool bbk_load_game(BBKEmulator* emu, const uint8_t* data, size_t len, char* err, size_t err_len);

// Runs up to `count` frames (fewer if the game exits) and returns how many ran.
// `hook`, if given, is called before each frame and may use the input functions.
typedef void BBKFrameHook(void* userdata, uint32_t frame);
uint32_t bbk_run_frames(BBKEmulator* emu, uint32_t count, BBKFrameHook* hook, void* userdata);
bool bbk_is_running(const BBKEmulator* emu);
void bbk_key_down(BBKEmulator* emu, uint8_t code);
void bbk_key_up(BBKEmulator* emu);
// Runs every instruction through mos6502 instead of the fast path (for comparison).
void bbk_set_reference(BBKEmulator* emu, bool reference);
void bbk_set_cpu_rate(BBKEmulator* emu, float rate);

// Draws into a 1-bit frame buffer (MSB first, 1 = white). Portrait is 2x (318x192),
// landscape is rotated clockwise at 1.5x (144x238). x0 must be a multiple of 8.
// ghosting: 0-242, the share of the previous frame (out of 256) that persists.
// Reports the changed rows; first_row > last_row when nothing changed.
void bbk_render(BBKEmulator* emu, uint8_t* frame, size_t rowbytes, uint32_t x0, uint32_t y0,
                bool landscape, uint8_t ghosting, int* first_row, int* last_row);
void bbk_reset_ghosting(BBKEmulator* emu);

// Renders the sound produced since the last call (mono, 44.1 kHz) into `out`;
// returns the number of samples. `volume` is each channel's amplitude.
size_t bbk_audio_render(BBKEmulator* emu, int16_t* out, size_t cap, int32_t volume);

// Exports return a buffer owned by the emulator, valid until the next export
// or bbk_release_export; NULL if there is nothing to export.
const uint8_t* bbk_battery_export(BBKEmulator* emu, size_t* len);
// False (and nothing changed) for files from older versions; see BATTERY_MAGIC.
bool bbk_battery_import(BBKEmulator* emu, const uint8_t* data, size_t len);
const uint8_t* bbk_state_save(BBKEmulator* emu, size_t* len);
bool bbk_state_load(BBKEmulator* emu, const uint8_t* data, size_t len);
void bbk_release_export(BBKEmulator* emu);

// Implemented by the frontend: reports a fatal core error. Never returns.
void bbk_pd_panic(const char* message);
