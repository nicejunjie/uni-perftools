# scilib-prof - scientific library profiler
# Builds two preloadable profilers:
#   libscilibprof-preload.so : LD_PRELOAD symbol interposition (dynamic libs)
#   libscilibprof-frida.so   : Frida DBI (also static libs)

CC      ?= cc
PYTHON  ?= python3

# --- OpenMP flag detection (gcc/clang: -fopenmp ; nvhpc: -mp) ---------------
OMPFLAG := $(shell $(CC) -fopenmp -E -x c /dev/null >/dev/null 2>&1 && echo -fopenmp || echo -mp)

GEN      := build/gen
OBJ      := build/obj
INCLUDE  := -Isrc/core -Isrc/wrap -Isrc/analyzers -Isrc/sample -I$(GEN)
CFLAGS   ?= -O2 -g -w
CFLAGS   += -fPIC $(OMPFLAG) -ftls-model=initial-exec $(INCLUDE)
# ILP64=1 -> profile 64-bit-integer BLAS/LAPACK (MKL/NVPL ILP64)
ifeq ($(ILP64),1)
CFLAGS   += -DLIBPROF_ILP64
endif
LDFLAGS  := -shared
LDLIBS   := -ldl -lrt -lpthread -lm

PROTOS   := $(wildcard gen/prototypes/*.txt)
HEADERS  := $(wildcard src/core/*.h src/analyzers/*.h src/sample/*.h) \
            src/wrap/libprof_wrap.h $(GEN)/libprof_slots.h
# MPI is profiled via the "opaque" dialect (mpi.h-free), so it builds like any
# other group with the plain compiler - no mpicc, no MPI headers required.
WRAP_GRP := blas lapack pblas scalapack cblas lapacke fftw mpi

# --- sources ---------------------------------------------------------------
CORE_SRC := $(wildcard src/core/*.c)
ANA_SRC  := $(wildcard src/analyzers/*.c)
SAMP_SRC := $(wildcard src/sample/*.c)
CORE_OBJ := $(patsubst src/core/%.c,$(OBJ)/shared/%.o,$(CORE_SRC)) \
            $(patsubst src/analyzers/%.c,$(OBJ)/shared/%.o,$(ANA_SRC)) \
            $(patsubst src/sample/%.c,$(OBJ)/shared/%.o,$(SAMP_SRC)) \
            $(OBJ)/shared/libprof_desc.o
WRAP_PRE := $(patsubst %,$(OBJ)/preload/%_wrap.o,$(WRAP_GRP))
WRAP_FRI := $(patsubst %,$(OBJ)/frida/%_wrap.o,$(WRAP_GRP))

TARGET_PRE := libscilibprof-preload.so
TARGET_FRI := libscilibprof-frida.so

all: $(TARGET_PRE) $(TARGET_FRI)
preload: $(TARGET_PRE)
frida:   $(TARGET_FRI)

# --- code generation -------------------------------------------------------
$(GEN)/.stamp: gen/gen.py $(PROTOS)
	@mkdir -p $(GEN)
	$(PYTHON) gen/gen.py --out $(GEN)
	@touch $@
GENERATED := $(GEN)/.stamp

# the generator produces these as a side effect of the stamp rule
GENFILES := $(patsubst %,$(GEN)/%_wrap.c,$(WRAP_GRP)) $(GEN)/libprof_desc.c $(GEN)/libprof_slots.h
$(GENFILES): $(GENERATED) ;

# --- shared (backend-agnostic) objects -------------------------------------
$(OBJ)/shared/%.o: src/core/%.c $(GENERATED) $(HEADERS) | dirs
	$(CC) $(CFLAGS) -c $< -o $@
$(OBJ)/shared/%.o: src/analyzers/%.c $(GENERATED) $(HEADERS) | dirs
	$(CC) $(CFLAGS) -c $< -o $@
$(OBJ)/shared/%.o: src/sample/%.c $(GENERATED) $(HEADERS) | dirs
	$(CC) $(CFLAGS) -c $< -o $@
$(OBJ)/shared/libprof_desc.o: $(GEN)/libprof_desc.c $(GENERATED) $(HEADERS) | dirs
	$(CC) $(CFLAGS) -c $< -o $@

# --- preload backend -------------------------------------------------------
$(OBJ)/preload/%_wrap.o: $(GEN)/%_wrap.c $(GENERATED) $(HEADERS) | dirs
	$(CC) $(CFLAGS) -c $< -o $@
$(OBJ)/preload/preload.o: src/backends/preload.c $(HEADERS) | dirs
	$(CC) $(CFLAGS) -c $< -o $@
$(TARGET_PRE): $(CORE_OBJ) $(WRAP_PRE) $(OBJ)/preload/preload.o
	$(CC) $(LDFLAGS) -o $@ $^ $(LDLIBS)
	@echo "built $@"

# --- frida backend ---------------------------------------------------------
$(OBJ)/frida/%_wrap.o: $(GEN)/%_wrap.c $(GENERATED) $(HEADERS) frida/frida-gum.h | dirs
	$(CC) $(CFLAGS) -DLIBPROF_BACKEND_FRIDA -Ifrida -c $< -o $@
$(OBJ)/frida/frida.o: src/backends/frida.c $(HEADERS) frida/frida-gum.h | dirs
	$(CC) $(CFLAGS) -Ifrida -c $< -o $@
$(TARGET_FRI): $(CORE_OBJ) $(WRAP_FRI) $(OBJ)/frida/frida.o frida/libfrida-gum.a
	$(CC) $(LDFLAGS) -o $@ $^ frida/libfrida-gum.a $(LDLIBS)
	@echo "built $@"

dirs:
	@mkdir -p $(OBJ)/shared $(OBJ)/preload $(OBJ)/frida

# --- frida devkit download -------------------------------------------------
FRIDA_VERSION := 16.2.1
CPUARCH := $(shell uname -m)
ifeq ($(CPUARCH),x86_64)
  FRIDA_ARCH := x86_64
else
  FRIDA_ARCH := arm64
endif
FRIDA_URL := https://github.com/frida/frida/releases/download/$(FRIDA_VERSION)/frida-gum-devkit-$(FRIDA_VERSION)-linux-$(FRIDA_ARCH).tar.xz

frida/frida-gum.h:
	@mkdir -p frida
	curl -s -L $(FRIDA_URL) -o frida/devkit.tar.xz
	tar -xf frida/devkit.tar.xz -C frida
frida/libfrida-gum.a: frida/frida-gum.h

PREFIX ?= /usr/local
install: all
	mkdir -p $(DESTDIR)$(PREFIX)/lib $(DESTDIR)$(PREFIX)/bin
	install -m 755 $(TARGET_PRE) $(TARGET_FRI) $(DESTDIR)$(PREFIX)/lib/
	install -m 755 bin/scilib-prof $(DESTDIR)$(PREFIX)/bin/scilib-prof
	install -m 755 tools/scilib-report.py $(DESTDIR)$(PREFIX)/bin/scilib-report
	@echo "installed to $(DESTDIR)$(PREFIX) — run: scilib-prof ./your_app"

.PHONY: all preload frida dirs clean veryclean install
clean:
	rm -rf $(OBJ) $(GEN)
veryclean: clean
	rm -f $(TARGET_PRE) $(TARGET_FRI)
	rm -rf frida
