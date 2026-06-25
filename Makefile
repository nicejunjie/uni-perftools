# Performance Suite — top-level build orchestrator
#
#   collectors/profile   C   → libprofile (sci-lib/MPI/sampling/IO/heap)
#   collectors/snapshot  Rust→ snapshot (HWPC roofline + characterization)
#   core/                shared spine (contract, symbolize, analysis, cli)
#
# Each collector also builds standalone in its own directory.

PREFIX ?= /usr/local
PROFILE := collectors/profile
SNAPSHOT := collectors/snapshot

.PHONY: all profile snapshot test validate-hwpc clean install

all: profile snapshot

profile:
	$(MAKE) -C $(PROFILE) all

snapshot:
	cd $(SNAPSHOT) && cargo build

# Hardware-independent HWPC gate: for every vendored CPU model, the canonical
# top-down metrics must resolve fully (events present + formula supported) or be
# an explicit gap — never partial/guessed. Runs without the target hardware.
validate-hwpc:
	@echo "== HWPC structural validation (all vendored models) =="
	cd $(SNAPSHOT) && cargo test -p uaps-collect structural_validation -- --nocapture

test: all validate-hwpc
	@echo "== profile tests =="; bash $(PROFILE)/tests/run.sh preload
	@echo "== snapshot tests =="; cd $(SNAPSHOT) && cargo test
	@echo "== suite tests =="; bash tests/run.sh

clean:
	$(MAKE) -C $(PROFILE) clean
	cd $(SNAPSHOT) && cargo clean

# Universal Performance Tool Suite: two commands — uaps (snapshot) + upat (deep tier).
# Stage the tree under $(PREFIX)/lib/uni-perftools; bin/uaps + bin/upat wrappers.
DEST := $(DESTDIR)$(PREFIX)/lib/uni-perftools
install: profile
	cd $(SNAPSHOT) && cargo build --release
	mkdir -p $(DEST)/collectors/profile/tools \
	         $(DEST)/collectors/snapshot/target/release \
	         $(DESTDIR)$(PREFIX)/bin
	install -m644 $(PROFILE)/libupat-preload.so $(PROFILE)/libupat-frida.so $(DEST)/collectors/profile/
	install -m755 $(PROFILE)/tools/upat-report.py $(DEST)/collectors/profile/tools/
	install -m755 $(SNAPSHOT)/target/release/uaps $(DEST)/collectors/snapshot/target/release/
	install -m644 $$(ls $(SNAPSHOT)/target/release/build/uaps-cli-*/out/uaps_mpi.so | head -1) \
	              $(DEST)/collectors/snapshot/target/release/uaps_mpi.so
	# Co-locate the vendored pmu-events DB with the binary (uaps finds it at
	# <exe>/../../pmu-events). WITHOUT this, vendor HWPC silently gaps off the build
	# host — fatal for multi-node, where this install lives on a shared FS so every
	# compute node runs the same binary + DB.
	cp -r $(SNAPSHOT)/pmu-events $(DEST)/collectors/snapshot/
	cp -r core $(DEST)/
	printf '#!/bin/sh\nexec "%s/lib/uni-perftools/core/cli/upat" "$$@"\n' "$(PREFIX)" > $(DESTDIR)$(PREFIX)/bin/upat
	printf '#!/bin/sh\nexec "%s/lib/uni-perftools/collectors/snapshot/target/release/uaps" "$$@"\n' "$(PREFIX)" > $(DESTDIR)$(PREFIX)/bin/uaps
	chmod +x $(DESTDIR)$(PREFIX)/bin/upat $(DESTDIR)$(PREFIX)/bin/uaps
	@echo "installed → run on a SHARED FS: 'mpirun -n N uaps ./app' then 'uaps report uaps_result'"
