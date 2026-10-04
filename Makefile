# Builds BBKEmu.pdx: a no_std build of upstream bbkemu-core (patched copy in gen/),
# the Rust glue in rust/ as a static library, and the C frontend in src/, linked by
# the Playdate SDK's own makefile rules.
#
#   make             device + Simulator build
#   make simulator   Simulator only
#   make run         build and open in the Simulator
#   make install     build, copy to a USB-connected Playdate and launch it

# On macOS the Simulator build links with Xcode's toolchain; the Command Line Tools
# linker can lag behind the installed SDK.
ifeq ($(shell uname -s),Darwin)
  XCODE ?= /Applications/Xcode.app/Contents/Developer
  ifneq ($(wildcard $(XCODE)),)
    export DEVELOPER_DIR := $(XCODE)
    export SDKROOT := $(XCODE)/Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk
  endif
endif

HEAP_SIZE      = 8388208
STACK_SIZE     = 61800

PRODUCT = BBKEmu.pdx

SDK = ${PLAYDATE_SDK_PATH}
ifeq ($(SDK),)
	SDK = $(shell egrep '^\s*SDKRoot' ~/.Playdate/config | head -n 1 | cut -c9-)
endif
ifeq ($(SDK),)
$(error SDK path not found; set ENV value PLAYDATE_SDK_PATH)
endif

VPATH += src
SRC = src/main.c src/calibrate.c
UINCDIR = src

UPSTREAM_CORE := upstream/crates/bbkemu-core
CORE_PATCH := patches/bbkemu-core-nostd.patch
CORE_STAMP := gen/bbkemu-core/.patched

DEVICE_TARGET := thumbv7em-none-eabihf
RUST_DEVICE_LIB := rust/target/$(DEVICE_TARGET)/release/libbbkemu_pd.a
RUST_SIM_LIB := rust/target/release/libbbkemu_pd.a
# Same flags the `crank` tool uses: Playdate loads position-independent code and
# has a single-precision FPU.
RUST_DEVICE_FLAGS := -Ctarget-cpu=cortex-m7 -Ctarget-feature=-fp64 -Crelocation-model=pic

ULIBS = $(RUST_DEVICE_LIB)

include $(SDK)/C_API/buildsupport/common.mk

# Device builds need newlib, which Homebrew's bare arm-none-eabi-gcc lacks. Prefer Arm's
# toolchain (brew install --cask gcc-arm-embedded), or point ARM_TOOLCHAIN at its bin/.
ARM_TOOLCHAIN ?= $(lastword $(sort $(wildcard /Applications/ArmGNUToolchain/*/arm-none-eabi/bin/)))
ifneq ($(ARM_TOOLCHAIN),)
  GCC := $(patsubst %//,%/,$(ARM_TOOLCHAIN)/)
  OJBCPY := $(GCC)
endif

$(OBJS): | check-arm-toolchain

.PHONY: check-arm-toolchain
check-arm-toolchain:
	@case "$$($(CC) -print-file-name=libc.a)" in /*) ;; *) \
		echo "error: $(GCC)$(TRGT)gcc has no C library (newlib). Install Arm's toolchain with"; \
		echo "  brew install --cask gcc-arm-embedded"; \
		echo "or set ARM_TOOLCHAIN=/path/to/arm-gnu-toolchain/bin."; exit 1;; esac

# common.mk's Simulator rule only compiles $(SRC); link the host build of the glue too.
SIMCOMPILER += $(RUST_SIM_LIB)

$(OBJDIR)/pdex.elf: $(RUST_DEVICE_LIB)
$(OBJDIR)/pdex.$(DYLIB_EXT): $(RUST_SIM_LIB)

.PHONY: FORCE run install check

$(CORE_STAMP): $(CORE_PATCH) $(wildcard $(UPSTREAM_CORE)/src/*) $(UPSTREAM_CORE)/Cargo.toml
	rm -rf gen/bbkemu-core
	mkdir -p gen/bbkemu-core
	cp -R $(UPSTREAM_CORE)/src $(UPSTREAM_CORE)/Cargo.toml gen/bbkemu-core/
	patch -s -p1 -d gen/bbkemu-core < $(CORE_PATCH)
	touch $@

$(RUST_DEVICE_LIB): $(CORE_STAMP) FORCE
	RUSTFLAGS="$(RUST_DEVICE_FLAGS)" cargo build --release --manifest-path rust/Cargo.toml --target $(DEVICE_TARGET)

$(RUST_SIM_LIB): $(CORE_STAMP) FORCE
	cargo build --release --manifest-path rust/Cargo.toml

# Differential tests of the fast paths (host only). Lockstep games: make check GAMES="a.gam b.gam" ROMS=dir
check: $(CORE_STAMP)
	cargo run --release --manifest-path tools/difftest/Cargo.toml -- 3000 $(GAMES)

run: simulator
	open -a "$(SDK)/bin/Playdate Simulator.app" $(PRODUCT)

# Mounts the Playdate's data disk over USB, copies the game, ejects and launches it.
PLAYDATE_PORT ?= $(firstword $(wildcard /dev/cu.usbmodemPD*))
PLAYDATE_VOLUME ?= /Volumes/PLAYDATE

install: device
	@test -n "$(PLAYDATE_PORT)" || { echo "No Playdate on USB: connect and unlock it."; exit 1; }
	$(SDK)/bin/pdutil $(PLAYDATE_PORT) datadisk
	@for i in $$(seq 30); do test -d $(PLAYDATE_VOLUME)/Games && break; sleep 1; done
	rm -rf $(PLAYDATE_VOLUME)/Games/$(PRODUCT)
	cp -R $(PRODUCT) $(PLAYDATE_VOLUME)/Games/
	diskutil eject $(PLAYDATE_VOLUME)
	@for i in $$(seq 30); do test -e $(PLAYDATE_PORT) && break; sleep 1; done; sleep 2
	$(SDK)/bin/pdutil $(PLAYDATE_PORT) run /Games/$(PRODUCT)
