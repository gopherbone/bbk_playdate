// BBKEmu for Playdate: game picker, emulation loop, on-screen keypad and options.
//
// Files live in the game's Data folder, laid out like the macOS app's
// Application Support folder so ROMs, saves and states can be copied across:
//   Games/*.gam
//   ROMs/A4980/{8,E}.BIN, ROMs/A4988/{8,E}.BIN
//   Saves/<game>.bbksav
//   States/<game>/slot<N>.state
//   Config/<game>.txt, settings.txt

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <ctype.h>

#include "pd_api.h"
#include "bbkemu_pd.h"
#include "gb2312_table.h"

static PlaydateAPI* pd;
static LCDFont* font;
static LCDFont* small_font;

#define REFRESH_RATE 30
#define MAX_FRAMES_PER_UPDATE 3
#define REPEAT_DELAY 20     // frames before a held key repeats
#define REPEAT_INTERVAL 5   // frames between repeats
#define TAP_FRAMES 8        // how long a keypad tap holds its key
#define AUTOSAVE_FRAMES 600 // battery save check interval
#define STATE_SLOTS 3
#define MAX_GAMES 512

// BBK key codes (bbkemu-core's BbkKey).
enum {
    KEY_EXIT = 0x2E, KEY_ENTER = 0x2F,
    KEY_UP = 0x35, KEY_LEFT = 0x37, KEY_DOWN = 0x38, KEY_RIGHT = 0x39,
};

typedef enum { SCREEN_PICKER, SCREEN_GAME, SCREEN_KEYPAD, SCREEN_OPTIONS, SCREEN_MESSAGE } Screen;

static Screen screen = SCREEN_PICKER;
static int needs_redraw = 1;

// MARK: Files

static uint8_t* read_file(const char* path, size_t* len) {
    FileStat st;
    if (pd->file->stat(path, &st) != 0 || st.isdir) return NULL;
    SDFile* f = pd->file->open(path, kFileReadData | kFileRead);
    if (!f) return NULL;
    uint8_t* buf = pd->system->realloc(NULL, st.size ? st.size : 1);
    if (!buf) {
        pd->file->close(f);
        return NULL;
    }
    size_t got = 0;
    while (got < st.size) {
        int n = pd->file->read(f, buf + got, st.size - got);
        if (n <= 0) break;
        got += n;
    }
    pd->file->close(f);
    if (got != st.size) {
        pd->system->realloc(buf, 0);
        return NULL;
    }
    *len = got;
    return buf;
}

// Writes via a temporary file so a crash mid-write never leaves a torn save.
static int write_file(const char* path, const uint8_t* data, size_t len) {
    char tmp[300];
    snprintf(tmp, sizeof tmp, "%s.tmp", path);
    SDFile* f = pd->file->open(tmp, kFileWrite);
    if (!f) return 0;
    size_t put = 0;
    while (put < len) {
        int n = pd->file->write(f, data + put, len - put);
        if (n <= 0) break;
        put += n;
    }
    pd->file->close(f);
    if (put != len) {
        pd->file->unlink(tmp, 0);
        return 0;
    }
    pd->file->unlink(path, 0);
    return pd->file->rename(tmp, path) == 0;
}

static int file_exists(const char* path) {
    FileStat st;
    return pd->file->stat(path, &st) == 0 && !st.isdir;
}

static void free_buf(void* p) {
    if (p) pd->system->realloc(p, 0);
}

// MARK: Settings

typedef struct {
    int ghosting;   // 0 off, 1 low, 2 high
    int show_speed;
} Settings;

typedef struct {
    int model;      // 0 A4980, 1 A4988, -1 unset
    int landscape;
    int slot;
} GameConfig;

static Settings settings = {1, 0};

// Parses "key=value" lines, calling `apply` for each.
static void read_kv(const char* path, void (*apply)(const char* key, int value, void* ud), void* ud) {
    size_t len;
    char* text = (char*)read_file(path, &len);
    if (!text) return;
    char* end = text + len;
    char* line = text;
    while (line < end) {
        char* nl = memchr(line, '\n', end - line);
        if (!nl) nl = end;
        char* eq = memchr(line, '=', nl - line);
        if (eq) {
            char key[32];
            size_t klen = eq - line;
            if (klen < sizeof key) {
                memcpy(key, line, klen);
                key[klen] = 0;
                apply(key, atoi(eq + 1), ud);
            }
        }
        line = nl + 1;
    }
    free_buf(text);
}

static void apply_setting(const char* key, int value, void* ud) {
    (void)ud;
    if (!strcmp(key, "ghosting")) settings.ghosting = value < 0 ? 0 : value > 2 ? 2 : value;
    else if (!strcmp(key, "show_speed")) settings.show_speed = value != 0;
}

static void save_settings(void) {
    char buf[64];
    int n = snprintf(buf, sizeof buf, "ghosting=%d\nshow_speed=%d\n", settings.ghosting, settings.show_speed);
    write_file("settings.txt", (uint8_t*)buf, n);
}

static void apply_game_config(const char* key, int value, void* ud) {
    GameConfig* c = ud;
    if (!strcmp(key, "model")) c->model = value ? 1 : 0;
    else if (!strcmp(key, "landscape")) c->landscape = value != 0;
    else if (!strcmp(key, "slot")) c->slot = value < 1 ? 1 : value > STATE_SLOTS ? STATE_SLOTS : value;
}

// MARK: ROMs

static const char* model_name(int model) { return model ? "A4988" : "A4980"; }

static int roms_installed(int model) {
    char path[64];
    snprintf(path, sizeof path, "ROMs/%s/8.BIN", model_name(model));
    if (!file_exists(path)) return 0;
    snprintf(path, sizeof path, "ROMs/%s/E.BIN", model_name(model));
    return file_exists(path);
}

// MARK: Chinese text

// Game names are mostly Chinese, which the system fonts can't draw; the 16x16
// hanzi font in the dictionary's own 8.BIN can.
static uint8_t* cjk_font;
static int cjk_font_tried;

static int glyph_index(uint32_t cp) {
    int lo = 0, hi = (int)(sizeof gb2312_table / sizeof gb2312_table[0]) - 1;
    while (lo <= hi) {
        int mid = (lo + hi) / 2;
        if (gb2312_table[mid][0] == cp) return gb2312_table[mid][1];
        if (gb2312_table[mid][0] < cp) lo = mid + 1;
        else hi = mid - 1;
    }
    return -1;
}

// 一 should be a lone horizontal bar; anything else means an unexpected 8.BIN layout.
static int font_looks_valid(const uint8_t* f) {
    const uint8_t* g = f + glyph_index(0x4E00) * 32;
    int bar_rows = 0, stray = 0;
    for (int r = 0; r < 16; r++) {
        int bits = __builtin_popcount(g[2 * r] << 8 | g[2 * r + 1]);
        if (bits >= 8) bar_rows++;
        else stray += bits;
    }
    return bar_rows >= 1 && bar_rows <= 2 && stray <= 4;
}

static void load_cjk_font(void) {
    if (cjk_font || cjk_font_tried) return;
    cjk_font_tried = 1;
    const size_t len = GB2312_GLYPHS * 32;
    for (int model = 0; model < 2 && !cjk_font; model++) {
        char path[64];
        snprintf(path, sizeof path, "ROMs/%s/8.BIN", model_name(model));
        SDFile* f = pd->file->open(path, kFileReadData);
        if (!f) continue;
        uint8_t* buf = pd->system->realloc(NULL, len);
        size_t got = 0;
        while (buf && got < len) {
            int n = pd->file->read(f, buf + got, len - got);
            if (n <= 0) break;
            got += n;
        }
        pd->file->close(f);
        if (buf && got == len && font_looks_valid(buf)) cjk_font = buf;
        else free_buf(buf);
    }
}

static uint32_t utf8_next(const char** s) {
    const unsigned char* p = (const unsigned char*)*s;
    uint32_t cp = *p++;
    int extra = cp >= 0xF0 ? 3 : cp >= 0xE0 ? 2 : cp >= 0xC0 ? 1 : 0;
    if (extra) cp &= 0x3F >> extra;
    while (extra-- && (*p & 0xC0) == 0x80) cp = cp << 6 | (*p++ & 0x3F);
    *s = (const char*)p;
    return cp;
}

static void draw_glyph(const uint8_t* g, int x, int y, int white) {
    uint8_t* frame = pd->graphics->getFrame();
    for (int r = 0; r < 16; r++) {
        int py = y + r;
        if (py < 0 || py >= LCD_ROWS) continue;
        unsigned bits = g[2 * r] << 8 | g[2 * r + 1];
        for (int c = 0; c < 16; c++) {
            int px = x + c;
            if (!(bits >> (15 - c) & 1) || px < 0 || px >= LCD_COLUMNS) continue;
            uint8_t* byte = &frame[py * LCD_ROWSIZE + px / 8];
            uint8_t mask = 0x80 >> (px & 7);
            *byte = white ? (*byte | mask) : (*byte & ~mask);
        }
    }
    pd->graphics->markUpdatedRows(y < 0 ? 0 : y, y + 15 >= LCD_ROWS ? LCD_ROWS - 1 : y + 15);
}

// Draws UTF-8 text in the current font, taking hanzi from 8.BIN when available.
// `white` must match the draw mode (kDrawModeFillWhite) for inverted rows.
static void draw_name(LCDFont* f, const char* text, int x, int y, int max_x, int white) {
    int glyph_y = y + (pd->graphics->getFontHeight(f) - 16) / 2;
    const char* p = text;
    while (*p && x < max_x) {
        const char* run = p;
        const char* q = p;
        int glyph = -1;
        // Collect a run the system font can draw, stopping at the next hanzi.
        while (*q) {
            const char* next = q;
            uint32_t cp = utf8_next(&next);
            glyph = cjk_font ? glyph_index(cp) : -1;
            if (glyph >= 0) break;
            q = next;
        }
        if (q > run) {
            pd->graphics->drawText(run, q - run, kUTF8Encoding, x, y);
            x += pd->graphics->getTextWidth(f, run, q - run, kUTF8Encoding, 0);
        }
        p = q;
        if (glyph >= 0 && *p) {
            if (x + 16 > max_x) break;
            draw_glyph(cjk_font + glyph * 32, x, glyph_y, white);
            x += 17;
            utf8_next(&p);
        }
    }
}

// MARK: Message screen

static char message_title[64];
static char message_body[512];
static Screen message_return = SCREEN_PICKER;

static void show_message(const char* title, const char* body, Screen back) {
    snprintf(message_title, sizeof message_title, "%s", title);
    snprintf(message_body, sizeof message_body, "%s", body);
    message_return = back;
    screen = SCREEN_MESSAGE;
    needs_redraw = 1;
}

// Draws `text` word-wrapped inside `width`, returning the y after the last line.
static int draw_wrapped(LCDFont* f, const char* text, int x, int y, int width) {
    int line_h = pd->graphics->getFontHeight(f) + 3;
    const char* p = text;
    while (*p) {
        const char* line_end = p;
        const char* best = NULL;
        while (*line_end && *line_end != '\n') {
            const char* next = line_end;
            while (*next && *next != ' ' && *next != '\n') next++;
            if (pd->graphics->getTextWidth(f, p, next - p, kUTF8Encoding, 0) > width && best) break;
            best = next;
            line_end = next;
            if (*line_end == ' ') line_end++;
        }
        if (!best) best = line_end;
        pd->graphics->drawText(p, best - p, kUTF8Encoding, x, y);
        y += line_h;
        p = best;
        while (*p == ' ') p++;
        if (*p == '\n') p++;
    }
    return y;
}

static void message_update(void) {
    PDButtons cur, pushed, released;
    pd->system->getButtonState(&cur, &pushed, &released);
    if (pushed & (kButtonA | kButtonB)) {
        screen = message_return;
        needs_redraw = 1;
        return;
    }
    if (!needs_redraw) return;
    needs_redraw = 0;
    pd->graphics->clear(kColorWhite);
    pd->graphics->setFont(font);
    pd->graphics->drawText(message_title, strlen(message_title), kUTF8Encoding, 16, 14);
    pd->graphics->drawLine(16, 36, 384, 36, 1, kColorBlack);
    pd->graphics->setFont(small_font);
    draw_wrapped(small_font, message_body, 16, 46, 368);
    pd->graphics->drawText("Ⓐ OK", strlen("Ⓐ OK"), kUTF8Encoding, 16, 218);
}

// MARK: Game session

static BBKEmulator* emu;
static char game_name[256];
static GameConfig config;
static uint8_t* last_battery;
static size_t last_battery_len;

static uint8_t held[8];
static int held_count;
static int repeat_countdown;
static PDButtons deferred_release;
static int tap_code = -1;
static int tap_frames;
static int autosave_countdown;
static unsigned int last_ms;
static int frame_acc; // thousandths of a frame
static float frame_cost_ms; // smoothed time per emulated frame
// Smoothed per-update times (ms): our render, and time spent outside update()
// (mostly the system pushing changed rows to the LCD).
static float render_ms, outside_ms, getframe_ms, mark_ms;
static unsigned int update_end_ms;
// Deterministic benchmark: average cost of emulated frames 300-599 after boot.
static int game_frames;
static float bench_ms;
static float bench_ms_result;
static int toast_frames;
static char toast_text[64];

static void toast(const char* text) {
    snprintf(toast_text, sizeof toast_text, "%s", text);
    toast_frames = REFRESH_RATE * 2;
}

static void save_game_config(void) {
    char path[300];
    char buf[64];
    snprintf(path, sizeof path, "Config/%s.txt", game_name);
    int n = snprintf(buf, sizeof buf, "model=%d\nlandscape=%d\nslot=%d\n", config.model, config.landscape, config.slot);
    write_file(path, (uint8_t*)buf, n);
}

static void save_battery(void) {
    if (!emu) return;
    size_t len = 0;
    const uint8_t* data = bbk_battery_export(emu, &len);
    if (!data) return;
    if (last_battery && len == last_battery_len && !memcmp(data, last_battery, len)) {
        bbk_release_export(emu);
        return;
    }
    char path[300];
    snprintf(path, sizeof path, "Saves/%s.bbksav", game_name);
    if (write_file(path, data, len)) {
        free_buf(last_battery);
        last_battery = pd->system->realloc(NULL, len);
        if (last_battery) {
            memcpy(last_battery, data, len);
            last_battery_len = len;
        }
    }
    bbk_release_export(emu);
}

static void state_path(char* out, size_t cap, int slot) {
    snprintf(out, cap, "States/%s/slot%d.state", game_name, slot);
}

static void save_state(void) {
    char dir[300], path[300];
    snprintf(dir, sizeof dir, "States/%s", game_name);
    pd->file->mkdir(dir);
    state_path(path, sizeof path, config.slot);
    size_t len = 0;
    const uint8_t* data = bbk_state_save(emu, &len);
    int ok = data && write_file(path, data, len);
    bbk_release_export(emu);
    char msg[64];
    snprintf(msg, sizeof msg, ok ? "Saved state %d" : "Couldn't save state %d", config.slot);
    toast(msg);
}

static void load_state(void) {
    char path[300];
    state_path(path, sizeof path, config.slot);
    size_t len;
    uint8_t* data = read_file(path, &len);
    char msg[64];
    if (!data) {
        snprintf(msg, sizeof msg, "State %d is empty", config.slot);
    } else if (bbk_state_load(emu, data, len)) {
        snprintf(msg, sizeof msg, "Loaded state %d", config.slot);
        held_count = 0;
    } else {
        snprintf(msg, sizeof msg, "State %d is for another model", config.slot);
    }
    free_buf(data);
    toast(msg);
}

static void end_game(void) {
    if (!emu) return;
    save_battery();
    bbk_destroy(emu);
    emu = NULL;
    free_buf(last_battery);
    last_battery = NULL;
    pd->system->removeAllMenuItems();
    pd->display->setRefreshRate(REFRESH_RATE);
}

static void menu_keypad(void* ud);
static void menu_options(void* ud);
static void menu_quit(void* ud);

static void game_add_menu_items(void) {
    pd->system->removeAllMenuItems();
    pd->system->addMenuItem("keypad", menu_keypad, NULL);
    pd->system->addMenuItem("options", menu_options, NULL);
    pd->system->addMenuItem("game list", menu_quit, NULL);
}

static void start_game(const char* name) {
    snprintf(game_name, sizeof game_name, "%s", name);
    config = (GameConfig){-1, 0, 1};
    char path[300];
    snprintf(path, sizeof path, "Config/%s.txt", game_name);
    read_kv(path, apply_game_config, &config);
    if (config.model < 0) config.model = roms_installed(0) || !roms_installed(1) ? 0 : 1;

    if (!roms_installed(config.model)) {
        char body[256];
        snprintf(body, sizeof body,
                 "This game is set to the %s, but its system ROMs are missing.\n\n"
                 "Copy 8.BIN and E.BIN into ROMs/%s in BBKEmu's Data folder.",
                 model_name(config.model), model_name(config.model));
        show_message("ROMs needed", body, SCREEN_PICKER);
        return;
    }

    // A copy in the Data folder's Games wins over one bundled in the pdx.
    snprintf(path, sizeof path, "Games/%s.gam", game_name);
    size_t game_len;
    uint8_t* game = read_file(path, &game_len);
    if (!game) {
        snprintf(path, sizeof path, "Bundled/%s.gam", game_name);
        game = read_file(path, &game_len);
    }
    if (!game) {
        show_message("Couldn't open game", path, SCREEN_PICKER);
        return;
    }

    emu = bbk_create(config.model);
    const char* rom_files[2] = {"8.BIN", "E.BIN"};
    for (int i = 0; i < 2; i++) {
        snprintf(path, sizeof path, "ROMs/%s/%s", model_name(config.model), rom_files[i]);
        size_t len;
        uint8_t* rom = read_file(path, &len);
        if (i == 0) bbk_load_rom8(emu, rom, rom ? len : 0);
        else bbk_load_rome(emu, rom, rom ? len : 0);
        free_buf(rom);
    }

    char err[256] = "";
    int ok = bbk_load_game(emu, game, game_len, err, sizeof err);
    free_buf(game);
    if (!ok) {
        bbk_destroy(emu);
        emu = NULL;
        show_message("Couldn't load game", err, SCREEN_PICKER);
        return;
    }

    snprintf(path, sizeof path, "Saves/%s.bbksav", game_name);
    size_t len;
    uint8_t* battery = read_file(path, &len);
    if (battery && bbk_battery_import(emu, battery, len)) {
        last_battery = battery;
        last_battery_len = len;
    } else {
        free_buf(battery);
    }

    held_count = 0;
    deferred_release = 0;
    tap_code = -1;
    autosave_countdown = AUTOSAVE_FRAMES;
    frame_acc = 0;
    frame_cost_ms = 0;
    game_frames = 0;
    bench_ms = 0;
    bench_ms_result = 0;
#ifdef BBK_REFERENCE
    bbk_set_reference(emu, 1);
#endif
    toast_frames = 0;
    last_ms = pd->system->getCurrentTimeMilliseconds();
    game_add_menu_items();
    screen = SCREEN_GAME;
    needs_redraw = 1;
}

static void restart_game(void) {
    char name[256];
    snprintf(name, sizeof name, "%s", game_name);
    end_game();
    start_game(name);
}

// Physical button -> BBK key, rotated to match the screen in landscape
// (the image turns clockwise, so the Playdate is held turned counter-clockwise).
static uint8_t key_for_button(PDButtons b) {
    switch (b) {
    case kButtonA: return KEY_ENTER;
    case kButtonB: return KEY_EXIT;
    case kButtonUp: return config.landscape ? KEY_LEFT : KEY_UP;
    case kButtonDown: return config.landscape ? KEY_RIGHT : KEY_DOWN;
    case kButtonLeft: return config.landscape ? KEY_DOWN : KEY_LEFT;
    case kButtonRight: return config.landscape ? KEY_UP : KEY_RIGHT;
    }
    return 0;
}

static void press_key(uint8_t code) {
    for (int i = 0; i < held_count; i++)
        if (held[i] == code) return;
    if (held_count < (int)sizeof held) held[held_count++] = code;
    bbk_key_down(emu, code);
    repeat_countdown = REPEAT_DELAY;
}

static void release_key(uint8_t code) {
    for (int i = 0; i < held_count; i++) {
        if (held[i] != code) continue;
        memmove(&held[i], &held[i + 1], held_count - i - 1);
        held_count--;
        bbk_key_up(emu);
        repeat_countdown = REPEAT_DELAY;
        return;
    }
}

static void release_all_keys(void) {
    while (held_count > 0) release_key(held[held_count - 1]);
    deferred_release = 0;
}

static void game_draw_chrome(void) {
    pd->graphics->clear(kColorWhite);
    if (config.landscape) {
        pd->graphics->drawRect(128 - 3, 1 - 1, 144 + 6, 238 + 2, kColorBlack);
    } else {
        pd->graphics->drawRect(40 - 3, 24 - 3, 318 + 6, 192 + 6, kColorBlack);
    }
    if (emu) bbk_reset_ghosting(emu);
}

static char status_shown[96];

static void game_draw_status(int force) {
    char text[96] = "";
    if (toast_frames > 0) {
        snprintf(text, sizeof text, "%s", toast_text);
    } else if (settings.show_speed) {
        // Share of each real-time frame (16.7 ms) the emulator needs.
        int load = (int)(frame_cost_ms * 60.0f / 10.0f + 0.5f);
        int n = snprintf(text, sizeof text, "%d.%d ms/f %d%% %dfps rnd %d.%d out %d.%d", (int)frame_cost_ms,
                         (int)(frame_cost_ms * 10) % 10, load, (int)(pd->display->getFPS() + 0.5f),
                         (int)render_ms, (int)(render_ms * 10) % 10, (int)outside_ms, (int)(outside_ms * 10) % 10);
        if (bench_ms_result > 0)
            snprintf(text + n, sizeof text - n, "  bench %d.%d", (int)bench_ms_result, (int)(bench_ms_result * 10) % 10);
    }
    if (!force && !strcmp(text, status_shown)) return;
    memcpy(status_shown, text, sizeof text);
    // Status text lives in the top band (portrait) or the left margin (landscape).
    int x = 4, y = 4, w = config.landscape ? 118 : 392, h = config.landscape ? 64 : 15;
    pd->graphics->fillRect(x, y, w, h, kColorWhite);
    pd->graphics->setFont(small_font);
    if (config.landscape) {
        draw_wrapped(small_font, text, x, y, w);
    } else {
        pd->graphics->drawText(text, strlen(text), kUTF8Encoding, x, y);
    }
}

// Runs inside bbk_run_frames before each emulated frame: key repeat, keypad taps, autosave.
static void before_frame(void* ud, uint32_t frame) {
    (void)ud;
    (void)frame;
    if (tap_code >= 0) {
        if (tap_frames == TAP_FRAMES) bbk_key_down(emu, tap_code);
        if (--tap_frames <= 0) {
            bbk_key_up(emu);
            tap_code = -1;
        }
    } else if (held_count > 0 && --repeat_countdown <= 0) {
        bbk_key_down(emu, held[held_count - 1]);
        repeat_countdown = REPEAT_INTERVAL;
    }
    if (--autosave_countdown <= 0) {
        autosave_countdown = AUTOSAVE_FRAMES;
        save_battery();
    }
}

static void game_update(void) {
    unsigned int update_start_ms = pd->system->getCurrentTimeMilliseconds();
    if (update_end_ms) outside_ms = outside_ms * 0.9f + (float)(update_start_ms - update_end_ms) * 0.1f;
    int redraw = needs_redraw;
    if (needs_redraw) {
        needs_redraw = 0;
        game_draw_chrome();
    }

    PDButtons cur, pushed, released;
    pd->system->getButtonState(&cur, &pushed, &released);
    // A press and release within one update: hold the key for a frame first.
    PDButtons release_now = (released & ~pushed) | deferred_release;
    deferred_release = released & pushed;
    for (int i = 0; i < 6; i++) {
        PDButtons b = 1 << i;
        if (pushed & b) press_key(key_for_button(b));
    }
    for (int i = 0; i < 6; i++) {
        PDButtons b = 1 << i;
        if (release_now & b) release_key(key_for_button(b));
    }

    unsigned int now = pd->system->getCurrentTimeMilliseconds();
    frame_acc += (int)(now - last_ms) * 60;
    last_ms = now;
    int frames = frame_acc / 1000;
    frame_acc -= frames * 1000;
    if (frames > MAX_FRAMES_PER_UPDATE) {
        frames = MAX_FRAMES_PER_UPDATE;
        frame_acc = 0;
    }

    float t0 = pd->system->getElapsedTime();
    int ran = frames > 0 ? bbk_run_frames(emu, frames, before_frame, NULL) : 0;
    if (!bbk_is_running(emu)) {
        end_game();
        show_message("Game ended", "The game exited back to the dictionary menu.", SCREEN_PICKER);
        return;
    }
    if (ran > 0) {
        float spent = (pd->system->getElapsedTime() - t0) * 1000.0f;
        if (game_frames >= 300 && game_frames + ran <= 600) bench_ms += spent;
        game_frames += ran;
        if (game_frames >= 600 && bench_ms_result == 0) bench_ms_result = bench_ms / 300;
        float cost = spent / ran;
        frame_cost_ms = frame_cost_ms == 0 ? cost : frame_cost_ms * 0.9f + cost * 0.1f;
        pd->system->resetElapsedTime();
    }

    static const uint8_t ghost_keep[3] = {0, 140, 200};
    float r0 = pd->system->getElapsedTime();
    int first, last;
    uint8_t* frame = pd->graphics->getFrame();
    float r1 = pd->system->getElapsedTime();
    getframe_ms = getframe_ms * 0.9f + (r1 - r0) * 1000.0f * 0.1f;
    if (config.landscape) {
        bbk_render(emu, frame, LCD_ROWSIZE, 128, 1, 1, ghost_keep[settings.ghosting], &first, &last);
    } else {
        bbk_render(emu, frame, LCD_ROWSIZE, 40, 24, 0, ghost_keep[settings.ghosting], &first, &last);
    }
    float r2 = pd->system->getElapsedTime();
    if (first <= last) pd->graphics->markUpdatedRows(first, last);
    mark_ms = mark_ms * 0.9f + (pd->system->getElapsedTime() - r2) * 1000.0f * 0.1f;
    render_ms = render_ms * 0.9f + (pd->system->getElapsedTime() - r0) * 1000.0f * 0.1f;

    if (toast_frames > 0) toast_frames--;
    game_draw_status(redraw);
    update_end_ms = pd->system->getCurrentTimeMilliseconds();
}

// MARK: Keypad

typedef struct {
    uint8_t code;
    const char* label;
} PadKey;

#define PAD_COLS 10
#define PAD_ROWS 6

static const PadKey pad_keys[PAD_ROWS][PAD_COLS] = {
    {{0x00, "ON"}, {0x01, "MENU"}, {0x02, "SJ"}, {0x03, "SW"}, {0x04, "CE"},
     {0x05, "DLG"}, {0x06, "DL"}, {0x07, "SPK"}, {0x29, "HELP"}, {0x2A, "SRCH"}},
    {{0x08, "1"}, {0x09, "2"}, {0x0A, "3"}, {0x0B, "4"}, {0x0C, "5"},
     {0x0D, "6"}, {0x0E, "7"}, {0x0F, "8"}, {0x30, "9"}, {0x31, "0"}},
    {{0x10, "Q"}, {0x11, "W"}, {0x12, "E"}, {0x13, "R"}, {0x14, "T"},
     {0x15, "Y"}, {0x16, "U"}, {0x17, "I"}, {0x32, "O"}, {0x33, "P"}},
    {{0x18, "A"}, {0x19, "S"}, {0x1A, "D"}, {0x1B, "F"}, {0x1C, "G"},
     {0x1D, "H"}, {0x1E, "J"}, {0x1F, "K"}, {0x34, "L"}, {0x2D, "DEL"}},
    {{0x21, "Z"}, {0x22, "X"}, {0x23, "C"}, {0x24, "V"}, {0x25, "B"},
     {0x26, "N"}, {0x27, "M"}, {0x20, "INPT"}, {0x28, "ZY"}, {0x36, "SPC"}},
    {{0x2B, "INS"}, {0x2C, "MOD"}, {0x2E, "EXIT"}, {0x2F, "ENTR"}, {0x3A, "PGUP"},
     {0x3B, "PGDN"}, {0x35, "UP"}, {0x38, "DOWN"}, {0x37, "LEFT"}, {0x39, "RGHT"}},
};

static int pad_row = 5, pad_col = 3;
static float pad_crank;

static void keypad_draw(void) {
    pd->graphics->clear(kColorWhite);
    pd->graphics->setFont(small_font);
    const char* title = "Ⓐ press key    Ⓑ back    crank or d-pad to move";
    pd->graphics->drawText(title, strlen(title), kUTF8Encoding, 8, 6);
    int cell_w = 39, cell_h = 34, x0 = 5, y0 = 30;
    int text_h = pd->graphics->getFontHeight(small_font);
    for (int r = 0; r < PAD_ROWS; r++) {
        for (int c = 0; c < PAD_COLS; c++) {
            int x = x0 + c * (cell_w + 0), y = y0 + r * cell_h;
            const char* label = pad_keys[r][c].label;
            int selected = r == pad_row && c == pad_col;
            if (selected) {
                pd->graphics->fillRoundRect(x + 1, y + 1, cell_w - 2, cell_h - 2, 4, kColorBlack);
                pd->graphics->setDrawMode(kDrawModeFillWhite);
            } else {
                pd->graphics->drawRoundRect(x + 1, y + 1, cell_w - 2, cell_h - 2, 4, 1, kColorBlack);
            }
            int tw = pd->graphics->getTextWidth(small_font, label, strlen(label), kUTF8Encoding, 0);
            pd->graphics->drawText(label, strlen(label), kUTF8Encoding, x + (cell_w - tw) / 2, y + (cell_h - text_h) / 2);
            pd->graphics->setDrawMode(kDrawModeCopy);
        }
    }
}

static void keypad_update(void) {
    PDButtons cur, pushed, released;
    pd->system->getButtonState(&cur, &pushed, &released);
    int moved = 0;
    if (pushed & kButtonUp) pad_row = (pad_row + PAD_ROWS - 1) % PAD_ROWS, moved = 1;
    if (pushed & kButtonDown) pad_row = (pad_row + 1) % PAD_ROWS, moved = 1;
    if (pushed & kButtonLeft) pad_col = (pad_col + PAD_COLS - 1) % PAD_COLS, moved = 1;
    if (pushed & kButtonRight) pad_col = (pad_col + 1) % PAD_COLS, moved = 1;
    pad_crank += pd->system->getCrankChange();
    while (pad_crank >= 30 || pad_crank <= -30) {
        int step = pad_crank > 0 ? 1 : -1;
        pad_crank -= step * 30;
        int i = (pad_row * PAD_COLS + pad_col + step + PAD_ROWS * PAD_COLS) % (PAD_ROWS * PAD_COLS);
        pad_row = i / PAD_COLS;
        pad_col = i % PAD_COLS;
        moved = 1;
    }
    if (pushed & kButtonA) {
        tap_code = pad_keys[pad_row][pad_col].code;
        tap_frames = TAP_FRAMES;
    }
    if (pushed & (kButtonA | kButtonB)) {
        last_ms = pd->system->getCurrentTimeMilliseconds();
        screen = SCREEN_GAME;
        needs_redraw = 1;
        return;
    }
    if (moved || needs_redraw) {
        needs_redraw = 0;
        keypad_draw();
    }
}

// MARK: Options

enum {
    OPT_SAVE, OPT_LOAD, OPT_SLOT, OPT_DISPLAY, OPT_GHOSTING, OPT_MODEL, OPT_SPEED, OPT_RESET, OPT_QUIT, OPT_COUNT
};

static int opt_selected;

static void option_label(int i, char* out, size_t cap) {
    static const char* ghost_names[3] = {"Off", "Low", "High"};
    switch (i) {
    case OPT_SAVE: snprintf(out, cap, "Save state"); break;
    case OPT_LOAD: snprintf(out, cap, "Load state"); break;
    case OPT_SLOT: snprintf(out, cap, "State slot\t%d", config.slot); break;
    case OPT_DISPLAY: snprintf(out, cap, "Display\t%s", config.landscape ? "Landscape" : "Portrait"); break;
    case OPT_GHOSTING: snprintf(out, cap, "LCD ghosting\t%s", ghost_names[settings.ghosting]); break;
    case OPT_MODEL: snprintf(out, cap, "Model (A switches, restarts)\t%s", model_name(config.model)); break;
    case OPT_SPEED: snprintf(out, cap, "Show performance\t%s", settings.show_speed ? "On" : "Off"); break;
    case OPT_RESET: snprintf(out, cap, "Reset game"); break;
    case OPT_QUIT: snprintf(out, cap, "Back to game list"); break;
    }
}

static void options_draw(void) {
    pd->graphics->clear(kColorWhite);
    pd->graphics->setFont(font);
    draw_name(font, game_name, 12, 8, 388, 0);
    pd->graphics->drawLine(12, 30, 388, 30, 1, kColorBlack);
    int row_h = 22, y0 = 36;
    for (int i = 0; i < OPT_COUNT; i++) {
        char label[96];
        option_label(i, label, sizeof label);
        char* value = strchr(label, '\t');
        if (value) *value++ = 0;
        int y = y0 + i * row_h;
        if (i == opt_selected) {
            pd->graphics->fillRect(8, y - 1, 384, row_h, kColorBlack);
            pd->graphics->setDrawMode(kDrawModeFillWhite);
        }
        pd->graphics->drawText(label, strlen(label), kUTF8Encoding, 16, y + 2);
        if (value) {
            char shown[48];
            snprintf(shown, sizeof shown, "< %s >", value);
            int w = pd->graphics->getTextWidth(font, shown, strlen(shown), kUTF8Encoding, 0);
            pd->graphics->drawText(shown, strlen(shown), kUTF8Encoding, 384 - w, y + 2);
        }
        pd->graphics->setDrawMode(kDrawModeCopy);
    }
}

static void options_close(void) {
    last_ms = pd->system->getCurrentTimeMilliseconds();
    screen = SCREEN_GAME;
    needs_redraw = 1;
}

static void options_update(void) {
    PDButtons cur, pushed, released;
    pd->system->getButtonState(&cur, &pushed, &released);
    int changed = needs_redraw;
    needs_redraw = 0;
    if (pushed & kButtonUp) opt_selected = (opt_selected + OPT_COUNT - 1) % OPT_COUNT, changed = 1;
    if (pushed & kButtonDown) opt_selected = (opt_selected + 1) % OPT_COUNT, changed = 1;
    int dir = (pushed & kButtonRight) ? 1 : (pushed & kButtonLeft) ? -1 : (pushed & kButtonA) ? 1 : 0;
    if (pushed & kButtonB) {
        options_close();
        return;
    }
    if (dir) {
        changed = 1;
        switch (opt_selected) {
        case OPT_SLOT:
            config.slot = (config.slot - 1 + dir + STATE_SLOTS) % STATE_SLOTS + 1;
            save_game_config();
            break;
        case OPT_DISPLAY:
            config.landscape = !config.landscape;
            save_game_config();
            break;
        case OPT_GHOSTING:
            settings.ghosting = (settings.ghosting + dir + 3) % 3;
            save_settings();
            break;
        case OPT_SPEED:
            settings.show_speed = !settings.show_speed;
            save_settings();
            break;
        }
        if (pushed & kButtonA) {
            switch (opt_selected) {
            case OPT_SAVE:
                save_state();
                options_close();
                return;
            case OPT_LOAD:
                load_state();
                options_close();
                return;
            case OPT_MODEL:
                config.model = !config.model;
                save_game_config();
                restart_game();
                return;
            case OPT_RESET:
                restart_game();
                return;
            case OPT_QUIT:
                end_game();
                screen = SCREEN_PICKER;
                needs_redraw = 1;
                return;
            }
        }
    }
    if (changed) options_draw();
}

// MARK: System menu

static void menu_keypad(void* ud) {
    (void)ud;
    if (screen != SCREEN_GAME && screen != SCREEN_OPTIONS) return;
    release_all_keys();
    pad_crank = 0;
    screen = SCREEN_KEYPAD;
    needs_redraw = 1;
}

static void menu_options(void* ud) {
    (void)ud;
    if (screen != SCREEN_GAME && screen != SCREEN_KEYPAD) return;
    release_all_keys();
    opt_selected = 0;
    screen = SCREEN_OPTIONS;
    needs_redraw = 1;
}

static void menu_quit(void* ud) {
    (void)ud;
    end_game();
    screen = SCREEN_PICKER;
    needs_redraw = 1;
}

// MARK: Game picker

static char* games[MAX_GAMES];
static int game_count;
static int picker_selected;
static float picker_crank;

static int has_gam_ext(const char* name, size_t len) {
    return len > 4 && name[len - 4] == '.' && tolower((unsigned char)name[len - 3]) == 'g' &&
           tolower((unsigned char)name[len - 2]) == 'a' && tolower((unsigned char)name[len - 1]) == 'm';
}

static void collect_game(const char* path, void* ud) {
    (void)ud;
    size_t len = strlen(path);
    if (game_count >= MAX_GAMES || path[0] == '.' || !has_gam_ext(path, len)) return;
    for (int i = 0; i < game_count; i++)
        if (strlen(games[i]) == len - 4 && !memcmp(games[i], path, len - 4)) return;
    char* name = pd->system->realloc(NULL, len - 3);
    memcpy(name, path, len - 4);
    name[len - 4] = 0;
    games[game_count++] = name;
}

static int compare_names(const void* a, const void* b) {
    const unsigned char* x = *(const unsigned char* const*)a;
    const unsigned char* y = *(const unsigned char* const*)b;
    while (*x && tolower(*x) == tolower(*y)) x++, y++;
    return tolower(*x) - tolower(*y);
}

static void scan_games(void) {
    for (int i = 0; i < game_count; i++) free_buf(games[i]);
    game_count = 0;
    // Games/ in the Data folder hides the pdx's own Games/, so bundled games live
    // in Bundled/, which only exists in the pdx.
    pd->file->listfiles("Games", collect_game, NULL, 0);
    pd->file->listfiles("Bundled", collect_game, NULL, 0);
    qsort(games, game_count, sizeof games[0], compare_names);
    if (picker_selected >= game_count) picker_selected = game_count ? game_count - 1 : 0;
}

static void picker_draw(void) {
    pd->graphics->clear(kColorWhite);
    pd->graphics->setFont(font);
    pd->graphics->drawText("BBKEmu", strlen("BBKEmu"), kUTF8Encoding, 12, 8);
    pd->graphics->setFont(small_font);
    const char* models;
    int r0 = roms_installed(0), r1 = roms_installed(1);
    models = r0 && r1 ? "ROMs: A4980, A4988" : r0 ? "ROMs: A4980" : r1 ? "ROMs: A4988" : "No ROMs installed";
    int w = pd->graphics->getTextWidth(small_font, models, strlen(models), kUTF8Encoding, 0);
    pd->graphics->drawText(models, strlen(models), kUTF8Encoding, 388 - w, 12);
    pd->graphics->drawLine(12, 30, 388, 30, 1, kColorBlack);

    if (game_count == 0) {
        pd->graphics->setFont(small_font);
        draw_wrapped(small_font,
                     "No games yet.\n\n"
                     "1. On the Playdate: Settings > System > Reboot to Data Disk, then connect USB.\n"
                     "2. Open the Data folder ending in com.gopherbone.bbkemu.\n"
                     "3. Put .gam files in Games, and 8.BIN + E.BIN in ROMs/A4980 or ROMs/A4988. "
                     "From the Mac app you can copy the ROMs, Saves and States folders as they are.\n"
                     "4. Eject, then press Ⓐ here to rescan.",
                     16, 40, 368);
        return;
    }

    pd->graphics->setFont(font);
    int row_h = 24, visible = 8, y0 = 36;
    int top = picker_selected - visible / 2;
    if (top > game_count - visible) top = game_count - visible;
    if (top < 0) top = 0;
    for (int i = top; i < game_count && i < top + visible; i++) {
        int y = y0 + (i - top) * row_h;
        if (i == picker_selected) {
            pd->graphics->fillRect(8, y - 1, 384, row_h, kColorBlack);
            pd->graphics->setDrawMode(kDrawModeFillWhite);
        }
        draw_name(font, games[i], 16, y + 3, 384, i == picker_selected);
        pd->graphics->setDrawMode(kDrawModeCopy);
    }
    char count[32];
    snprintf(count, sizeof count, "%d / %d", picker_selected + 1, game_count);
    pd->graphics->setFont(small_font);
    w = pd->graphics->getTextWidth(small_font, count, strlen(count), kUTF8Encoding, 0);
    pd->graphics->drawText(count, strlen(count), kUTF8Encoding, 388 - w, 226);
}

static void picker_update(void) {
    if (needs_redraw) {
        if (!cjk_font) cjk_font_tried = 0;
        load_cjk_font();
        scan_games();
    }
    PDButtons cur, pushed, released;
    pd->system->getButtonState(&cur, &pushed, &released);
    int changed = needs_redraw;
    needs_redraw = 0;
    if (game_count > 0) {
        int step = 0;
        if (pushed & kButtonUp) step = -1;
        if (pushed & kButtonDown) step = 1;
        if (pushed & kButtonLeft) step = -8;
        if (pushed & kButtonRight) step = 8;
        picker_crank += pd->system->getCrankChange();
        while (picker_crank >= 20 || picker_crank <= -20) {
            int s = picker_crank > 0 ? 1 : -1;
            picker_crank -= s * 20;
            step += s;
        }
        if (step) {
            int next = picker_selected + step;
            if (next < 0) next = step == -1 ? game_count - 1 : 0;
            if (next >= game_count) next = step == 1 ? 0 : game_count - 1;
            if (next != picker_selected) picker_selected = next, changed = 1;
        }
        if (pushed & kButtonA) {
            start_game(games[picker_selected]);
            return;
        }
    } else if (pushed & kButtonA) {
        needs_redraw = 1;
        return;
    }
    if (changed) picker_draw();
}

// MARK: Entry points

static int update(void* ud) {
    (void)ud;
    switch (screen) {
    case SCREEN_PICKER: picker_update(); break;
    case SCREEN_GAME: game_update(); break;
    case SCREEN_KEYPAD: keypad_update(); break;
    case SCREEN_OPTIONS: options_update(); break;
    case SCREEN_MESSAGE: message_update(); break;
    }
    return 1;
}

void bbk_pd_panic(const char* message) {
    pd->system->error("%s", message);
}

#if TARGET_PLAYDATE
// newlib's snprintf drags in its stdio layer, which wants these POSIX hooks.
// Nothing here uses stdio streams, so they only need to link.
struct stat;
int _close(int fd) { (void)fd; return -1; }
int _fstat(int fd, struct stat* st) { (void)fd; (void)st; return -1; }
int _getpid(void) { return 1; }
int _isatty(int fd) { (void)fd; return 0; }
int _kill(int pid, int sig) { (void)pid; (void)sig; return -1; }
int _lseek(int fd, int off, int whence) { (void)fd; (void)off; (void)whence; return -1; }
int _read(int fd, void* buf, size_t n) { (void)fd; (void)buf; (void)n; return -1; }
int _write(int fd, const void* buf, size_t n) { (void)fd; (void)buf; (void)n; return -1; }
void _exit(int code) {
    (void)code;
    for (;;) {}
}
#endif

#ifdef _WINDLL
__declspec(dllexport)
#endif
int eventHandler(PlaydateAPI* playdate, PDSystemEvent event, uint32_t arg) {
    (void)arg;
    switch (event) {
    case kEventInit: {
        pd = playdate;
        const char* err;
        font = pd->graphics->loadFont("/System/Fonts/Asheville-Sans-14-Bold.pft", &err);
        small_font = pd->graphics->loadFont("/System/Fonts/Roobert-10-Bold.pft", &err);
        if (!small_font) small_font = font;
        const char* dirs[] = {"Games", "ROMs", "ROMs/A4980", "ROMs/A4988", "Saves", "States", "Config"};
        for (size_t i = 0; i < sizeof dirs / sizeof dirs[0]; i++) pd->file->mkdir(dirs[i]);
        read_kv("settings.txt", apply_setting, NULL);
        pd->display->setRefreshRate(REFRESH_RATE);
        pd->system->setUpdateCallback(update, NULL);
#ifdef BBK_CALIBRATE
        {
            extern uint32_t bbk_unrolled(uint32_t v);
            pd->system->resetElapsedTime();
            uint32_t uv = 1;
            for (int i = 0; i < 2000; i++) uv = bbk_unrolled(uv);
            float unrolled = pd->system->getElapsedTime();
            pd->system->logToConsole("CAL6 unrolled code: %d ps/op (%u)", (int)(unrolled * 1e12f / (2000.0f * 1024)), (unsigned)uv);
        }
        {
            volatile uint32_t sink;
            uint32_t v = 1;
            pd->system->resetElapsedTime();
            for (int i = 0; i < 10000000; i++) v = v * 1664525u + 1013904223u;
            sink = v;
            float alu = pd->system->getElapsedTime();
            uint8_t* buf = pd->system->realloc(NULL, 1 << 20);
            memset(buf, 1, 1 << 20);
            uint32_t sum = 0;
            pd->system->resetElapsedTime();
            for (int r = 0; r < 10; r++)
                for (int i = 0; i < (1 << 20); i += 32) sum += buf[i];
            float stream = pd->system->getElapsedTime();
            uint8_t small[4096];
            memset(small, 1, sizeof small);
            pd->system->resetElapsedTime();
            for (int r = 0; r < 2560; r++)
                for (int i = 0; i < 4096; i += 32) sum += ((volatile uint8_t*)small)[i];
            float stack = pd->system->getElapsedTime();
            pd->system->resetElapsedTime();
            for (int r = 0; r < 2560; r++)
                for (int i = 0; i < 4096; i += 32) sum += ((volatile uint8_t*)buf)[i];
            float heapsmall = pd->system->getElapsedTime();
            pd->system->resetElapsedTime();
            for (int r = 0; r < 2560; r++)
                for (int i = 0; i < 4096; i += 32) ((volatile uint8_t*)buf)[i] = (uint8_t)r;
            float heapstore = pd->system->getElapsedTime();
            pd->system->resetElapsedTime();
            for (int r = 0; r < 2560; r++)
                for (int i = 0; i < 4096; i += 32) ((volatile uint8_t*)small)[i] = (uint8_t)r;
            float stackstore = pd->system->getElapsedTime();
            pd->system->resetElapsedTime();
            for (int r = 0; r < 81920; r++) ((volatile uint8_t*)buf)[r & 63] = (uint8_t)r;
            float heapsame = pd->system->getElapsedTime();
            static uint8_t sbuf[8192] __attribute__((aligned(32)));
            pd->system->resetElapsedTime();
            for (int r = 0; r < 2560; r++)
                for (int i = 0; i < 4096; i += 32) ((volatile uint8_t*)sbuf)[i] = (uint8_t)r;
            float staticstore = pd->system->getElapsedTime();
            pd->system->resetElapsedTime();
            for (int r = 0; r < 2560; r++)
                for (int i = 0; i < 4096; i += 32) sum += ((volatile uint8_t*)sbuf)[i];
            float staticread = pd->system->getElapsedTime();
            {
                // Read-only scan below the stack pointer for FreeRTOS's 0xA5 stack fill.
                volatile uint32_t* sp = (volatile uint32_t*)__builtin_frame_address(0);
                volatile uint32_t* q = (volatile uint32_t*)(((uintptr_t)sp - 4096) & ~3u);
                uint32_t* lowest_fill = NULL;
                int run = 0;
                for (volatile uint32_t* w = q; (uintptr_t)w > 0x20000000u; w--) {
                    if (*w == 0xA5A5A5A5u) { lowest_fill = (uint32_t*)w; run++; }
                    else if (run > 16) break;
                    else run = 0;
                }
                pd->system->logToConsole("CAL4 frame %p, 0xA5 fill reaches down to %p (%d words in last run); free below frame ~%d bytes",
                    (void*)sp, (void*)lowest_fill, run, lowest_fill ? (int)((uintptr_t)sp - (uintptr_t)lowest_fill) : -1);
            }
            {
                extern uint32_t bbk_bench_interpreter(uint32_t mode, uint32_t count);
                pd->system->resetElapsedTime();
                uint32_t n0 = bbk_bench_interpreter(0, 2000000);
                float t0 = pd->system->getElapsedTime();
                pd->system->resetElapsedTime();
                uint32_t n1 = bbk_bench_interpreter(1, 60);
                float t1 = pd->system->getElapsedTime();
                pd->system->logToConsole("CAL5 bare step %d ns/inst (%u insts), run_frame %d ns/6502-cycle (%u cycles)",
                    (int)(t0 * 1e9f / n0), (unsigned)n0, (int)(t1 * 1e9f / n1), (unsigned)n1);
            }
            pd->system->logToConsole("CAL3 static-store %d ns, static-read %d ns, sbuf at %p, heap at %p, stack at %p",
                (int)(staticstore * 1e9f / (2560 * 128)), (int)(staticread * 1e9f / (2560 * 128)), (void*)sbuf, (void*)buf, (void*)small);
            pd->system->logToConsole("CAL2 heap-store %d ns, stack-store %d ns, heap-store-same-line %d ns",
                (int)(heapstore * 1e9f / (2560 * 128)), (int)(stackstore * 1e9f / (2560 * 128)), (int)(heapsame * 1e9f / 81920));
            sink = sum;
            (void)sink;
            pd->system->realloc(buf, 0);
            pd->system->logToConsole("CAL alu %d ns/iter, sdram-miss %d ns/line, stack-hit %d ns/access, heap-hit %d ns/access",
                (int)(alu * 1e9f / 1e7f), (int)(stream * 1e9f / (10 * 32768)), (int)(stack * 1e9f / (2560 * 128)), (int)(heapsmall * 1e9f / (2560 * 128)));
        }
#endif
        break;
    }
    case kEventPause:
    case kEventLock:
    case kEventTerminate:
    case kEventLowPower:
        save_battery();
        if (emu) release_all_keys();
        break;
    case kEventResume:
    case kEventUnlock:
        last_ms = pd->system->getCurrentTimeMilliseconds();
        if (screen == SCREEN_GAME) needs_redraw = 1;
        break;
    default:
        break;
    }
    return 0;
}
