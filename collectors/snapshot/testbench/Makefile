# Builds the local test workloads into ./bin. Nothing is installed system-wide.
# MPI/OpenMP targets are skipped automatically if the toolchain is absent.

CC      ?= cc
CFLAGS  ?= -O2 -g
BIN     := bin

MPICC   := $(shell command -v mpicc 2>/dev/null)
HAVE_OMP := $(shell echo 'int main(){return 0;}' | $(CC) -fopenmp -x c - -o /dev/null 2>/dev/null && echo yes)

BINS := $(BIN)/compute_bound $(BIN)/memory_bound $(BIN)/io_bound $(BIN)/threaded $(BIN)/flops_simd
ifeq ($(HAVE_OMP),yes)
BINS += $(BIN)/omp_bound
endif
ifneq ($(MPICC),)
BINS += $(BIN)/mpi_ring
endif

all: $(BINS)

$(BIN):
	mkdir -p $(BIN)

$(BIN)/threaded: threaded.c | $(BIN)
	$(CC) $(CFLAGS) $< -o $@ -lpthread

$(BIN)/flops_simd: flops_simd.c | $(BIN)
	$(CC) -O3 -march=native -ffast-math $< -o $@

$(BIN)/omp_bound: omp_bound.c | $(BIN)
	$(CC) $(CFLAGS) -fopenmp $< -o $@

$(BIN)/mpi_ring: mpi_ring.c | $(BIN)
	$(MPICC) $(CFLAGS) $< -o $@

$(BIN)/%: %.c | $(BIN)
	$(CC) $(CFLAGS) $< -o $@

clean:
	rm -rf $(BIN) testbench_io.tmp

.PHONY: all clean
