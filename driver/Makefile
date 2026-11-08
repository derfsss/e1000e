# Makefile - link the e1000e driver, a SANA-II network driver loaded from
# DEVS:Networks/. No CRT, no clib4.
# Run inside Docker: walkero/amigagccondocker:os4-gcc15-3.6.1
#
#   make            e1000e.device       (cargo build --release)
#   make DIAG=1     e1000e_diag.device  (cargo build --release --features diag)
#
# tools/build-driver.sh builds both.

DIAG     ?= 0
CC        = ppc-amigaos-gcc
SDK_INC   = /opt/ppc-amigaos/ppc-amigaos/SDK/include/include_h

ifeq ($(DIAG),1)
TARGET    = e1000e_diag.device
BUILD     = build-diag
VARIANT   = -DDIAG_BUILD
else
TARGET    = e1000e.device
BUILD     = build
VARIANT   =
endif

GLUE_SRC      = src/e1000e_glue.c
SYS_GLUE_SRC  = ../amigaos4/glue/amiga_glue.c
MEM_GLUE_SRC  = ../amigaos4/glue/freestanding_mem.c
GLUE_OBJ      = $(BUILD)/e1000e_glue.o
SYS_GLUE_OBJ  = $(BUILD)/sys_glue.o
MEM_GLUE_OBJ  = $(BUILD)/freestanding_mem.o

RUST_LIB  = target/powerpc-unknown-amigaos/release/libe1000e.a

# The commit (tools/build-package.sh passes it), for the build banner.
GIT_HASH ?= unknown

CFLAGS    = -O2 -Wall -D__AMIGAOS4__ -U__USE_INLINE__ -I$(SDK_INC) -DGIT_HASH=\"$(GIT_HASH)\" $(VARIANT) \
            -fno-tree-loop-distribute-patterns -ffunction-sections -fdata-sections

# The Resident is found by scanning for RTC_MATCHWORD: keep it as a GC root.
LDFLAGS   = -nostartfiles -nodefaultlibs -Wl,-z,noexecstack -Wl,--gc-sections \
            -Wl,--undefined=NetTag
LIBS      = -lgcc

.PHONY: all clean

all: $(BUILD) $(TARGET)

$(BUILD):
	mkdir -p $(BUILD)

$(GLUE_OBJ): $(GLUE_SRC)
	$(CC) $(CFLAGS) -c $< -o $@

$(MEM_GLUE_OBJ): $(MEM_GLUE_SRC)
	$(CC) $(CFLAGS) -ffreestanding -fno-builtin -c $< -o $@

$(SYS_GLUE_OBJ): $(SYS_GLUE_SRC)
	$(CC) $(CFLAGS) -c $< -o $@

$(TARGET): $(GLUE_OBJ) $(SYS_GLUE_OBJ) $(MEM_GLUE_OBJ) $(RUST_LIB)
	$(CC) $(LDFLAGS) -o $@ $(GLUE_OBJ) $(SYS_GLUE_OBJ) $(MEM_GLUE_OBJ) $(RUST_LIB) $(LIBS)
	ppc-amigaos-strip $@

clean:
	rm -rf $(BUILD) $(TARGET)
