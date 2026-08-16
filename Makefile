# catprinterd — build, check, kit, image files, fleet test. `make help` lists targets.
VERSION := $(shell sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)
GIT_SHA    := $(shell git rev-parse --short HEAD 2>/dev/null || echo nogit)
BUILD_DATE := $(shell date -u +%FT%TZ)
# Canonical VERSION line, byte-identical in the kit and in the image: "<semver> <git-sha|nogit> <build-utc>".
# Field 1 is always the semver (fleet validators: `cut -d' ' -f1`).
VERSION_LINE = $(VERSION) $(GIT_SHA) $(BUILD_DATE)
ARCH    := x86_64
KIT     := dist/catprinter-kit
TARBALL := dist/catprinter-kit-$(VERSION)-$(ARCH).tar.gz
KIT_FILES := packaging/install.sh packaging/catprinter.service packaging/catprinter-queue.service \
             packaging/env.example packaging/80-catprinter.preset
DEST ?= dist/image-root
FIXTURE := tests/fixtures/text-roll48.pwg
IPPTOOL_TESTS := /usr/share/cups/ipptool
# `make fleet-test` boots systemd containers; podman must be able to run --privileged --systemd=always.
# From a distrobox: make fleet-test PODMAN="distrobox-host-exec podman" (rootless works too).
FLEET_PODMAN ?= $(shell if [ "$$(id -u)" = 0 ]; then echo podman; else echo "sudo podman"; fi)

.PHONY: help version build check test lint kit image-files fleet-test fixtures install ipptool musl clean

help:
	@printf '%s\n' \
	  'build        cargo build --release --locked' \
	  'check        fmt --check, clippy -D warnings, tests, shell lint (install.sh, fleet-test.sh), unit verify' \
	  'test         cargo test --locked' \
	  'kit          dist/catprinter-kit/ + $(TARBALL) + dist/SHA256SUMS' \
	  'image-files  DEST=dir  files for an image build: /usr/bin, /usr/lib/systemd/system (+wants symlinks), preset, /usr/lib/catprinter' \
	  'fleet-test   boot the kit and the image files in systemd containers (PODMAN="$(FLEET_PODMAN)"; KEEP=1, FLEET_FIRST_BOOT=1)' \
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
	bash -n scripts/fleet-test.sh
	@if command -v shellcheck >/dev/null 2>&1; then shellcheck -S warning packaging/install.sh scripts/fleet-test.sh; else echo "shellcheck not installed — skipped"; fi
	@if command -v systemd-analyze >/dev/null 2>&1; then \
	  out=$$(systemd-analyze verify packaging/catprinter.service packaging/catprinter-queue.service 2>&1 | grep -Ev 'catprinterd.*(not executable|No such file)' || true); \
	  if [ -n "$$out" ]; then printf '%s\n' "$$out"; echo "systemd-analyze verify: unexpected findings (only the missing-binary lines are tolerated)"; exit 1; fi; \
	else echo "systemd-analyze not installed — unit verify skipped"; fi
	packaging/install.sh --help >/dev/null

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
	printf '%s\n' "$(VERSION_LINE)" > $(KIT)/VERSION
	chmod 0755 $(KIT)/install.sh $(KIT)/catprinterd
	tar -C dist -czf $(TARBALL) catprinter-kit
	cp $(TARBALL) dist/catprinter-kit-$(ARCH).tar.gz
	cd dist && sha256sum catprinter-kit-$(VERSION)-$(ARCH).tar.gz catprinter-kit-$(ARCH).tar.gz > SHA256SUMS
	@echo "kit: $(KIT)/  tarball: $(TARBALL)  VERSION: $(VERSION_LINE)"

# Files for a custom (uBlue) image build: binary in /usr/bin, units in /usr/lib/systemd/system with
# ExecStart rewritten, the multi-user.target.wants symlinks (= `systemctl enable` at build time; the
# preset alone only fires on a true first boot, not on a rebase), preset, and /usr/lib/catprinter/
# {env.example,install.sh,VERSION} (install.sh keeps managing /etc/catprinter/env on the machine).
# DEST is only ever added to — never wiped.
image-files: build
	install -D -m 0755 target/release/catprinterd $(DEST)/usr/bin/catprinterd
	install -d $(DEST)/usr/lib/systemd/system $(DEST)/etc/systemd/system/multi-user.target.wants
	for u in catprinter.service catprinter-queue.service; do \
	  sed 's#^ExecStart=/usr/local/bin/catprinterd#ExecStart=/usr/bin/catprinterd#' packaging/$$u > $(DEST)/usr/lib/systemd/system/$$u && chmod 0644 $(DEST)/usr/lib/systemd/system/$$u; \
	  ln -sfn /usr/lib/systemd/system/$$u $(DEST)/etc/systemd/system/multi-user.target.wants/$$u; \
	done
	install -D -m 0644 packaging/80-catprinter.preset $(DEST)/usr/lib/systemd/system-preset/80-catprinter.preset
	install -D -m 0644 packaging/env.example $(DEST)/usr/lib/catprinter/env.example
	install -D -m 0755 packaging/install.sh  $(DEST)/usr/lib/catprinter/install.sh
	printf '%s\n' "$(VERSION_LINE)" > $(DEST)/usr/lib/catprinter/VERSION
	@echo "image files under $(DEST):"; find $(DEST) \( -type f -o -type l \) | sort
	@echo "Containerfile: COPY $(DEST)/ /   (the etc/ symlinks == 'systemctl enable catprinter catprinter-queue' at build time)"

# Real systemd-in-podman boot of both install paths (image-baked rebase case, kit + kit->image migration).
fleet-test: kit image-files
	PODMAN="$(if $(PODMAN),$(PODMAN),$(FLEET_PODMAN))" scripts/fleet-test.sh

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
