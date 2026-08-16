# catprinterd — build, check, kit, image files. `make help` lists targets.
VERSION := $(shell sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)
ARCH    := x86_64
KIT     := dist/catprinter-kit
TARBALL := dist/catprinter-kit-$(VERSION)-$(ARCH).tar.gz
KIT_FILES := packaging/install.sh packaging/catprinter.service packaging/catprinter-queue.service \
             packaging/env.example packaging/80-catprinter.preset
DEST ?= dist/image-root
FIXTURE := tests/fixtures/text-roll48.pwg
IPPTOOL_TESTS := /usr/share/cups/ipptool

.PHONY: help version build check test lint kit image-files fixtures install ipptool musl clean

help:
	@printf '%s\n' \
	  'build        cargo build --release --locked' \
	  'check        fmt --check, clippy -D warnings, tests, install.sh syntax (+shellcheck if present)' \
	  'test         cargo test --locked' \
	  'kit          dist/catprinter-kit/ + $(TARBALL) + dist/SHA256SUMS' \
	  'image-files  DEST=dir  files for an image build: /usr/bin, /usr/lib/systemd/system, preset' \
	  'ipptool      run the IPP Everywhere suite against a fake-printer daemon (needs ipptool)' \
	  'fixtures     regenerate tests/fixtures/*.pwg with the host CUPS filters' \
	  'install      sudo dist/catprinter-kit/install.sh install' \
	  'musl         static build for x86_64-unknown-linux-musl' \
	  'version      print the crate version'

version:
	@echo $(VERSION)

build:
	cargo build --release --locked

test:
	cargo test --locked

lint:
	cargo fmt --check
	cargo clippy --all-targets --locked -- -D warnings
	bash -n packaging/install.sh
	@if command -v shellcheck >/dev/null 2>&1; then shellcheck -S warning packaging/install.sh; else echo "shellcheck not installed — skipped"; fi
	@if command -v systemd-analyze >/dev/null 2>&1; then systemd-analyze verify packaging/catprinter.service packaging/catprinter-queue.service 2>&1 | grep -v 'catprinterd' || true; fi

check: lint test

# The IPP Everywhere compliance gate: every [FAIL] must be listed in tests/ipptool/allowlist.txt.
ipptool: build
	@command -v ipptool >/dev/null || { echo "ipptool missing (cups-ipptool / cups-ipp-utils)"; exit 1; }
	@rm -rf tests/out/ippfake && mkdir -p tests/out/ippfake
	@./target/release/catprinterd serve --port 8096 --fake-printer tests/out/ippfake --dnssd off --log-level warn & echo $$! > tests/out/ippfake.pid; \
	 for i in $$(seq 1 30); do curl -fsS http://127.0.0.1:8096/health >/dev/null 2>&1 && break; sleep 0.5; done; \
	 ipptool -V 2.0 -tI -f $(FIXTURE) -d filetype=image/pwg-raster ipp://127.0.0.1:8096/ipp/print $(IPPTOOL_TESTS)/ipp-everywhere.test | tee tests/out/ippeve.log; \
	 rc=0; grep '\[FAIL\]' tests/out/ippeve.log | sed 's/ *\[FAIL\].*//; s/^ *//' | while read -r name; do \
	   grep -qxF "$$name" tests/ipptool/allowlist.txt || { echo "FAIL not allowlisted: $$name"; exit 1; }; done || rc=1; \
	 kill $$(cat tests/out/ippfake.pid) 2>/dev/null; rm -f tests/out/ippfake.pid; exit $$rc

kit: build
	rm -rf $(KIT) && mkdir -p $(KIT)
	cp target/release/catprinterd $(KIT)/
	cp $(KIT_FILES) $(KIT)/
	cp packaging/KIT-README.md $(KIT)/README.md
	printf '%s %s %s\n' "$(VERSION)" "$$(git rev-parse --short HEAD 2>/dev/null || echo nogit)" "$$(date -u +%FT%TZ)" > $(KIT)/VERSION
	chmod 0755 $(KIT)/install.sh $(KIT)/catprinterd
	tar -C dist -czf $(TARBALL) catprinter-kit
	cp $(TARBALL) dist/catprinter-kit-$(ARCH).tar.gz
	cd dist && sha256sum catprinter-kit-$(VERSION)-$(ARCH).tar.gz catprinter-kit-$(ARCH).tar.gz > SHA256SUMS
	@echo "kit: $(KIT)/  tarball: $(TARBALL)"

# Files for a custom (uBlue) image build: binary in /usr/bin, units in /usr/lib/systemd/system with
# ExecStart rewritten, preset enabling both units, env.example as documentation.
image-files: build
	install -D -m 0755 target/release/catprinterd $(DEST)/usr/bin/catprinterd
	mkdir -p $(DEST)/usr/lib/systemd/system
	for u in catprinter.service catprinter-queue.service; do \
	  sed 's#/usr/local/bin/catprinterd#/usr/bin/catprinterd#g' packaging/$$u > $(DEST)/usr/lib/systemd/system/$$u; \
	done
	install -D -m 0644 packaging/80-catprinter.preset $(DEST)/usr/lib/systemd/system-preset/80-catprinter.preset
	install -D -m 0644 packaging/env.example $(DEST)/usr/lib/catprinter/env.example
	@echo "image files under $(DEST):"; find $(DEST) -type f | sort

fixtures:
	scripts/make-fixtures.sh

install: kit
	sudo $(KIT)/install.sh install

musl:
	rustup target add x86_64-unknown-linux-musl
	cargo build --release --locked --target x86_64-unknown-linux-musl
	@file target/x86_64-unknown-linux-musl/release/catprinterd
	@cargo tree -i libdbus-sys 2>/dev/null | head -1 || true

clean:
	rm -rf dist tests/out
	cargo clean
