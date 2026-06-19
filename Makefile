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
	@echo "== suite tests =="; bash tests/run.sh

clean:
	$(MAKE) -C $(PROFILE) clean
	cd $(SNAPSHOT) && cargo clean

# Stage the suite under $(PREFIX)/lib/perfsuite (the driver discovers its
# collectors via this tree) + a bin/perfsuite wrapper.
DEST := $(DESTDIR)$(PREFIX)/lib/perfsuite
install: profile
	cd $(SNAPSHOT) && cargo build --release
	mkdir -p $(DEST)/collectors/profile/tools \
	         $(DEST)/collectors/snapshot/target/release \
	         $(DESTDIR)$(PREFIX)/bin
	install -m644 $(PROFILE)/libscilibprof-preload.so $(PROFILE)/libscilibprof-frida.so $(DEST)/collectors/profile/
	install -m755 $(PROFILE)/tools/scilib-report.py $(DEST)/collectors/profile/tools/
	install -m755 $(SNAPSHOT)/target/release/uaps $(DEST)/collectors/snapshot/target/release/
	cp -r core $(DEST)/
	printf '#!/bin/sh\nexec "%s/lib/perfsuite/core/cli/perfsuite" "$$@"\n' "$(PREFIX)" > $(DESTDIR)$(PREFIX)/bin/perfsuite
	chmod +x $(DESTDIR)$(PREFIX)/bin/perfsuite
	@echo "installed → run: perfsuite run -- ./your_app"
