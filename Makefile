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

.PHONY: all profile snapshot test clean install

all: profile snapshot

profile:
	$(MAKE) -C $(PROFILE) all

snapshot:
	cd $(SNAPSHOT) && cargo build

test: all
	@echo "== profile tests =="; bash $(PROFILE)/tests/run.sh preload
	@echo "== snapshot tests =="; cd $(SNAPSHOT) && cargo test

clean:
	$(MAKE) -C $(PROFILE) clean
	cd $(SNAPSHOT) && cargo clean

install: all
	@echo "install target is fleshed out in the packaging phase (PREFIX=$(PREFIX))"
