# Makefile for the membench reference benchmarks.
#
# Two programs, both plain C11 with no dependencies beyond glibc:
#   mbstream   - read / write / copy bandwidth
#   mblatency  - dependent pointer-chase latency
#
# `-march=native` is on by default because the whole point of the default
# kernels is to let gcc emit the widest vector code this CPU supports.  Drop it
# with `make NATIVE=`, or point it somewhere specific with `make NATIVE=-march=x86-64-v2`;
# the portable-kernel path (--isa scalar) does not depend on it.
#
# `make scalar` rebuilds with -DMB_FORCE_SCALAR=1, which makes even
# `--isa default` run the portable loops.  gcc may still vectorise those loops;
# see README.md.

CC      ?= gcc
OPT     ?= -O3
NATIVE  ?= -march=native
WARN    := -Wall -Wextra
BIN     := bin
OUT     := out

# EXTRA_CFLAGS is the hook for -DMB_FORCE_SCALAR=1 and for experiments such as
# -fno-tree-vectorize (which is the only way to *force* scalar code; --isa
# scalar only picks the portable source loop).
EXTRA_CFLAGS ?=

CFLAGS := $(OPT) -std=gnu11 -pthread $(NATIVE) $(WARN) -g $(EXTRA_CFLAGS)
LDLIBS := -pthread

PROGS := $(BIN)/mbstream $(BIN)/mblatency

.PHONY: all clean scalar run help

all: $(PROGS)

$(BIN):
	mkdir -p $(BIN)

$(BIN)/%: %.c | $(BIN)
	$(CC) $(CFLAGS) -o $@ $< $(LDLIBS)

# Rebuild the programs with the portable kernels compiled in.  -B rather than
# clean, so out/ keeps the results of the run you are comparing against.
scalar:
	@$(MAKE) --no-print-directory -B all EXTRA_CFLAGS="-DMB_FORCE_SCALAR=1"
	@echo "built with MB_FORCE_SCALAR=1: --isa default now runs the portable loops"

run: all
	./run.sh

clean:
	rm -rf $(BIN) $(OUT)

help:
	@echo "targets: all (default), scalar (portable kernels compiled in), run (./run.sh),"
	@echo "         clean (remove bin/ and out/)"
	@echo "knobs:   NATIVE=      drop -march=native (make NATIVE=)"
	@echo "         NATIVE=-march=x86-64-v2   or any other -march"
	@echo "         OPT=-O2      change the optimisation level"
	@echo "         EXTRA_CFLAGS=...  e.g. -fno-tree-vectorize -DMB_FORCE_SCALAR=1"
