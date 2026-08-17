#!/usr/bin/env bash
# scripts/fleet-test.sh — the fleet-path test: boots real systemd machines with podman and runs
# install.sh through both install paths, the way the fleet meets them.
#
#   phase 1  image-baked machine, REBASE case (machine-id present => no first boot => no presets):
#            units enabled by the shipped wants symlinks, daemon + CUPS queue up with zero steps,
#            /usr/lib/catprinter/install.sh status/install/update/uninstall, a print through real
#            CUPS into --fake-printer.
#   phase 2  kit path on a plain machine, then the machine "rebases" onto the image (image files
#            merged in like ostree does: local additions win) => mixed state => install.sh migrates
#            the kit off the image; uninstall/--purge idempotence.
#   phase 3  opt-in (FLEET_FIRST_BOOT=1): true first boot => the preset enables the units.
#            opt-in (FLEET_DOWNLOAD=vX.Y.Z): real `install.sh --download` against GitHub Releases.
#
#   make fleet-test                                   builds dist/catprinter-kit + dist/image-root first
#   PODMAN="distrobox-host-exec podman" scripts/fleet-test.sh     from a distrobox (rootless is fine)
#   KEEP=1                                            keep the containers (podman exec -it <name> bash)
#
# Inputs: dist/catprinter-kit (make kit), dist/image-root (make image-files).
# Logs:   tests/out/fleet/<container>.journal.log, .podman.log, .commands.log (every exec + output).
# Exit:   1 if any assertion failed (assertions never abort the run; the summary lists them).
set -euo pipefail

ROOT=$(cd "$(dirname "$(readlink -f "$0")")/.." && pwd)
cd "$ROOT"
if [[ -z ${PODMAN:-} ]]; then if [[ $EUID -eq 0 ]]; then PODMAN=podman; else PODMAN="sudo podman"; fi; fi
read -r -a P <<<"$PODMAN"          # may be several words: "sudo podman", "distrobox-host-exec podman"
KIT=${KIT:-$ROOT/dist/catprinter-kit}
IMAGE_ROOT=${IMAGE_ROOT:-$ROOT/dist/image-root}
OUT=$ROOT/tests/out/fleet
CTX=$OUT/ctx
IMG=${FLEET_IMAGE:-catprinter-fleet}
KEEP=${KEEP:-0}
FLEET_FIRST_BOOT=${FLEET_FIRST_BOOT:-0}
FLEET_DOWNLOAD=${FLEET_DOWNLOAD:-}
SEMVER=$(sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)
QUEUE=CatPrinter
PORT=8095
IMG_SH=/usr/lib/catprinter/install.sh
KIT_SH=/kit/install.sh
T_START=$(date +%s)

# ---- helpers ------------------------------------------------------------------------------------
C=""                    # current container name
CONTAINERS=()
PASSES=0; FAILS=0; FAILED=()
RC=0; OUTPUT=""         # set by capture()

say()  { printf '\n\033[1m== %s\033[0m  (%ss)\n' "$*" "$(( $(date +%s) - T_START ))"; }
pass() { PASSES=$((PASSES + 1)); printf '  \033[32mok\033[0m    %s\n' "$*"; }
fail() { FAILS=$((FAILS + 1)); FAILED+=("[$C] $*"); printf '  \033[31mFAIL\033[0m  %s\n' "$*"; }
note() { printf '  \033[36m..\033[0m    %s\n' "$*"; }
die()  { printf '\033[31mfleet-test: %s\033[0m\n' "$*" >&2; exit 1; }

x()  { "${P[@]}" exec "$C" "$@"; }                       # run in the current container
xs() { "${P[@]}" exec "$C" bash -c "$1"; }               # shell snippet in the current container
# capture CMD... -> RC (exit status) and OUTPUT (stdout+stderr); never trips errexit; logged.
capture() {
  RC=0; OUTPUT=$("$@" 2>&1) || RC=$?
  { printf '\n$ %s\n' "$*"; printf '%s\n' "$OUTPUT"; printf '[rc=%s]\n' "$RC"; } >> "$OUT/$C.commands.log"
}
# assert MSG CMD...      -> pass when CMD succeeds (output logged)
assert()    { local msg=$1; shift; capture "$@"; if [[ $RC -eq 0 ]]; then pass "$msg"; else fail "$msg (rc=$RC): $(printf '%s' "$OUTPUT" | tail -n 3 | tr '\n' '|')"; fi; }
# assert_rc MSG WANT CMD... -> pass when CMD exits with WANT
assert_rc() { local msg=$1 want=$2; shift 2; capture "$@"; if [[ $RC -eq $want ]]; then pass "$msg (rc=$RC)"; else fail "$msg: rc=$RC, want $want: $(printf '%s' "$OUTPUT" | tail -n 3 | tr '\n' '|')"; fi; }
# assert_eq MSG GOT WANT
assert_eq() { if [[ $2 == "$3" ]]; then pass "$1 = '$3'"; else fail "$1: got '$2', want '$3'"; fi; }
# assert_has MSG NEEDLE  -> last captured OUTPUT contains NEEDLE
assert_has() { if [[ $OUTPUT == *"$2"* ]]; then pass "$1 (mentions '$2')"; else fail "$1: output lacks '$2': $(printf '%s' "$OUTPUT" | tail -n 3 | tr '\n' '|')"; fi; }
assert_not_has() { if [[ $OUTPUT != *"$2"* ]]; then pass "$1 (no '$2')"; else fail "$1: output contains '$2'"; fi; }
# wait_for SECONDS MSG CMD... -> polls once a second; LAST=0 on success, 1 on timeout (never aborts)
LAST=0
wait_for() {
  local n=$1 msg=$2 i; shift 2
  for ((i = 0; i <= n; i++)); do
    if "$@" >/dev/null 2>&1; then pass "$msg (${i}s)"; LAST=0; return 0; fi
    sleep 1
  done
  fail "$msg (timeout ${n}s)"; LAST=1; return 0
}
out1() { xs "$1" 2>/dev/null | head -n1 || true; }        # first line of a snippet's stdout, never fails

collect() {
  local c=$1
  {
    echo "### systemctl status catprinter catprinter-queue"
    "${P[@]}" exec "$c" systemctl status catprinter catprinter-queue --no-pager -l 2>&1 || true
    echo; echo "### systemctl --failed"; "${P[@]}" exec "$c" systemctl --failed --no-pager 2>&1 || true
    echo; echo "### journalctl -b"
    "${P[@]}" exec "$c" journalctl -b --no-pager -o short-precise 2>&1 || true
  } > "$OUT/$c.journal.log" 2>&1
  "${P[@]}" logs "$c" > "$OUT/$c.podman.log" 2>&1 || true
}
finish() {
  local rc=$? c
  trap - EXIT
  for c in "${CONTAINERS[@]}"; do
    collect "$c" || true
    if [[ $KEEP == 1 ]]; then note "kept container $c (podman exec -it $c bash; podman rm -f $c)"
    else "${P[@]}" rm -f "$c" >/dev/null 2>&1 || true; fi
  done
  echo
  printf '\033[1m== summary: %d passed, %d failed  (%ss, logs in %s)\033[0m\n' "$PASSES" "$FAILS" "$(( $(date +%s) - T_START ))" "$OUT"
  local f; for f in "${FAILED[@]}"; do printf '  \033[31mFAIL\033[0m  %s\n' "$f"; done
  if [[ $rc -ne 0 ]]; then printf '\033[31mfleet-test aborted (rc=%s)\033[0m\n' "$rc"; exit "$rc"; fi
  if [[ $FAILS -ne 0 ]]; then exit 1; fi
  exit 0
}
trap finish EXIT

# start VARIANT NAME [podman run args...] [-- command args...] -> boots the container and waits for
# systemd (LAST=0 when up). CMDLINE overrides the kernel command line systemd sees.
start() {
  local variant=$1 name=$2; shift 2
  local run=() cmd=()
  while [[ $# -gt 0 ]]; do
    if [[ $1 == -- ]]; then shift; cmd=("$@"); break; fi
    run+=("$1"); shift
  done
  "${P[@]}" rm -f "$name" >/dev/null 2>&1 || true
  "${P[@]}" run -d --name "$name" --systemd=always --privileged --stop-timeout 10 \
    -e "SYSTEMD_PROC_CMDLINE=${CMDLINE:-systemd.condition_first_boot=0 systemd.firstboot=off}" \
    "${run[@]}" "$IMG:$variant" "${cmd[@]}" >/dev/null || die "podman run $name failed"
  CONTAINERS+=("$name"); C=$name
  : > "$OUT/$C.commands.log"
  wait_for 60 "$name: systemd reports running|degraded" xs 'systemctl is-system-running 2>/dev/null | grep -Eq "^(running|degraded)$"'
  if [[ $LAST -ne 0 ]]; then note "state: $(out1 'systemctl is-system-running')"; fi
}

build_image() {  # build_image TAG IMAGE_BAKED
  local log=$OUT/build-$1.log
  if ! "${P[@]}" build -t "$IMG:$1" --build-arg "IMAGE_BAKED=$2" -f "$CTX/Containerfile" "$CTX" >"$log" 2>&1; then
    tail -n 40 "$log" >&2; die "podman build $IMG:$1 failed (see $log)"
  fi
}

# ---- shared checks ------------------------------------------------------------------------------
check_daemon_and_queue() {  # $1 = label, $2 = expected FragmentPath dir (/usr/lib/systemd/system or /etc/systemd/system)
  local label=$1 frag=$2
  wait_for 30 "$label: catprinter.service active" xs 'systemctl is-active catprinter.service | grep -qx active'
  wait_for 90 "$label: catprinter-queue.service active" xs 'systemctl is-active catprinter-queue.service | grep -qx active'
  assert_eq "$label: catprinter FragmentPath" "$(out1 'systemctl show -p FragmentPath --value catprinter.service')" "$frag/catprinter.service"
  assert_eq "$label: catprinter-queue FragmentPath" "$(out1 'systemctl show -p FragmentPath --value catprinter-queue.service')" "$frag/catprinter-queue.service"
  wait_for 30 "$label: /health answers" xs "curl -fsS 127.0.0.1:$PORT/health >/dev/null"
  capture xs "curl -fsS 127.0.0.1:$PORT/health"
  if [[ $OUTPUT == *"\"version\": \"$SEMVER\""* || $OUTPUT == *"\"version\":\"$SEMVER\""* ]]; then pass "$label: /health has version $SEMVER"; else fail "$label: /health lacks version $SEMVER: $(printf '%s' "$OUTPUT" | tr '\n' ' ' | cut -c1-200)"; fi
  wait_for 30 "$label: lpstat -v $QUEUE" xs "lpstat -v $QUEUE >/dev/null"
  assert_eq "$label: queue URI" "$(out1 "lpstat -v $QUEUE")" "device for $QUEUE: ipp://127.0.0.1:$PORT/ipp/print"
  assert "$label: queue idle/enabled" xs "lpstat -p $QUEUE | grep -Eq 'idle|enabled|is idle'"
  assert "$label: PPD has *PageSize 48x297mm" xs "grep -q '^\*PageSize 48x297mm' /etc/cups/ppd/$QUEUE.ppd"
  assert "$label: $QUEUE is not the default printer" xs "! lpstat -d | grep -q '$QUEUE'"
}

# ---- inputs -------------------------------------------------------------------------------------
for f in "$KIT/catprinterd" "$KIT/install.sh" "$KIT/VERSION" "$KIT/catprinter.service" \
         "$KIT/61-catprinter-btusb.rules" "$IMAGE_ROOT/usr/bin/catprinterd" \
         "$IMAGE_ROOT/usr/lib/systemd/system/catprinter.service" "$IMAGE_ROOT/usr/lib/catprinter/install.sh" \
         "$IMAGE_ROOT/usr/lib/udev/rules.d/61-catprinter-btusb.rules"; do
  [[ -e $f ]] || die "missing $f — run: make kit image-files"
done
[[ -x $KIT/catprinterd && -x $KIT/install.sh && -x $IMAGE_ROOT/usr/bin/catprinterd && -x $IMAGE_ROOT/usr/lib/catprinter/install.sh ]] || die "kit/image binaries or install.sh not executable (chmod 0755)"
[[ -f tests/fixtures/text-roll48.pwg ]] || die "missing tests/fixtures/text-roll48.pwg"
"${P[@]}" --version >/dev/null 2>&1 || die "podman not usable via: $PODMAN"
mkdir -p "$OUT"
rm -f "$OUT"/*.log

say "fleet-test: semver $SEMVER, kit VERSION '$(head -n1 "$KIT/VERSION")', podman='$PODMAN'"
[[ $(cut -d' ' -f1 "$KIT/VERSION") == "$SEMVER" ]] || die "kit VERSION field 1 is not $SEMVER: $(head -n1 "$KIT/VERSION")"
# The wants symlinks are a property under test (phase 1 fails without them), not a harness input.
# -L: they dangle on the build host (absolute targets inside the image).
for u in catprinter.service catprinter-queue.service; do
  [[ -L $IMAGE_ROOT/etc/systemd/system/multi-user.target.wants/$u ]] || note "image-root has no etc/systemd/system/multi-user.target.wants/$u — stale image-root? phase 1 will show whether the units still get enabled"
done

# ---- build the two images ------------------------------------------------------------------------
say "building $IMG:image and $IMG:plain (context $CTX)"
rm -rf "$CTX"; mkdir -p "$CTX/fixtures"
cp -a "$KIT" "$CTX/kit"
cp -a "$IMAGE_ROOT" "$CTX/image-root"
cp tests/fixtures/text-roll48.pwg "$CTX/fixtures/"
cp tests/fleet/Containerfile "$CTX/Containerfile"
build_image image 1
build_image plain 0
note "images built ($(( $(date +%s) - T_START ))s; logs $OUT/build-*.log)"

# =================================================================================================
say "phase 1 — image-baked machine, rebase case (no first boot, no presets)"
start image catprinter-fleet-image
if [[ $LAST -ne 0 ]]; then
  fail "phase 1: machine did not boot — skipping the phase"
else
  # 1. this is a rebase, not a first boot: the baked machine-id survived, no transient id was committed,
  #    and PID 1 did not populate /etc from presets (that line is logged before journald exists, so
  #    its absence in the journal is only supporting evidence)
  assert_eq "machine-id is the baked one (not a first boot)" "$(out1 'cat /etc/machine-id')" 8f5f8b7c2c8e4a7d9e2b1c3d4e5f6a7b
  assert_eq "systemd-machine-id-commit.service did not run" "$(out1 'systemctl show -p ActiveState --value systemd-machine-id-commit.service')" inactive
  assert "no 'Populated /etc with preset' in the journal" xs '! journalctl -b -o cat | grep -q "Populated /etc with preset"'
  # 2. enabled purely by the shipped wants symlinks
  assert_eq "catprinter is-enabled" "$(out1 'systemctl is-enabled catprinter.service')" enabled
  assert_eq "catprinter-queue is-enabled" "$(out1 'systemctl is-enabled catprinter-queue.service')" enabled
  assert_eq "wants symlink target" "$(out1 'readlink /etc/systemd/system/multi-user.target.wants/catprinter.service')" /usr/lib/systemd/system/catprinter.service
  # 3–5. daemon, health, queue — zero per-machine steps
  check_daemon_and_queue "boot" /usr/lib/systemd/system
  # 6. nothing was written to /etc by anyone
  assert "no /etc/catprinter/env after boot (zero steps)" x test ! -e /etc/catprinter/env
  # 7. status without root, and --help does not leak the script body
  assert "$IMG_SH status (root) exit 0" x "$IMG_SH" status
  assert "$IMG_SH status (nobody) exit 0" x runuser -u nobody -- "$IMG_SH" status
  assert "$IMG_SH --help exit 0" x "$IMG_SH" --help
  assert_not_has "--help output" "set -euo"
  # 8. bug 1: install on an image-baked machine (KIT_DIR unbound before the fix)
  assert "$IMG_SH install --yes exit 0 (bug 1)" x "$IMG_SH" install --yes
  assert_not_has "install --yes" "unbound variable"
  assert "/etc/catprinter/env created" x test -f /etc/catprinter/env
  check_daemon_and_queue "after install" /usr/lib/systemd/system
  assert "install --binary /kit/catprinterd --yes exit 0" x "$IMG_SH" install --binary /kit/catprinterd --yes
  assert_has "install --binary on an image machine" "ignored"
  # 9. update
  assert "$IMG_SH update --yes exit 0" x "$IMG_SH" update --yes
  # 10. VERSION shape: field 1 == semver == binary --version
  assert_eq "cut -f1 /usr/lib/catprinter/VERSION" "$(out1 "cut -d' ' -f1 /usr/lib/catprinter/VERSION")" "$SEMVER"
  assert_eq "/usr/lib/catprinter/VERSION has 3 fields" "$(out1 'wc -w < /usr/lib/catprinter/VERSION')" 3
  assert_eq "/usr/bin/catprinterd --version" "$(out1 '/usr/bin/catprinterd --version')" "catprinterd $SEMVER"
  assert "no /usr/local/share/catprinter/VERSION on an image machine" x test ! -e /usr/local/share/catprinter/VERSION
  assert "no /usr/local/bin/catprinterd on an image machine" x test ! -e /usr/local/bin/catprinterd
  # 11. a real print through CUPS into --fake-printer
  assert "append CATPRINTERD_ARGS=--fake-printer to env + restart" xs "printf 'CATPRINTERD_ARGS=--fake-printer /var/lib/catprinter/fake\n' >> /etc/catprinter/env && systemctl restart catprinter"
  wait_for 30 "/health after restart" xs "curl -fsS 127.0.0.1:$PORT/health >/dev/null"
  assert "lp -d $QUEUE /fixtures/text-roll48.pwg" x lp -d "$QUEUE" /fixtures/text-roll48.pwg
  wait_for 30 "fake printer wrote job-*.png" xs 'ls /var/lib/private/catprinter/fake/job-*.png /var/lib/catprinter/fake/job-*.png 2>/dev/null | grep -q png'
  wait_for 30 "job completed in CUPS" xs "lpstat -W completed -o $QUEUE | grep -q ."
  assert_eq "completed jobs" "$(out1 "lpstat -W completed -o $QUEUE | wc -l")" 1
  assert_eq "no jobs left in the queue" "$(out1 "lpstat -o $QUEUE | wc -l")" 0
  # 12. uninstall keeps the image files, then install again
  assert "$IMG_SH uninstall --yes exit 0" x "$IMG_SH" uninstall --yes
  assert "queue gone after uninstall" xs "! lpstat -v $QUEUE >/dev/null 2>&1"
  assert_eq "catprinter is-enabled after uninstall" "$(out1 'systemctl is-enabled catprinter.service')" disabled
  assert_eq "catprinter is-active after uninstall" "$(out1 'systemctl is-active catprinter.service')" inactive
  assert_eq "catprinter-queue is-active after uninstall" "$(out1 'systemctl is-active catprinter-queue.service')" inactive
  assert "/etc/catprinter/env kept" x test -f /etc/catprinter/env
  assert "/usr/bin/catprinterd kept" x test -x /usr/bin/catprinterd
  assert "/usr/lib/systemd/system/catprinter.service kept" x test -f /usr/lib/systemd/system/catprinter.service
  assert "$IMG_SH install --yes again exit 0" x "$IMG_SH" install --yes
  check_daemon_and_queue "after re-install" /usr/lib/systemd/system
  assert_eq "wants symlink target after re-install" "$(out1 'readlink /etc/systemd/system/multi-user.target.wants/catprinter.service')" /usr/lib/systemd/system/catprinter.service
  assert "$IMG_SH status exit 0 at the end" x "$IMG_SH" status
  assert_has "status header" "image-baked install"
fi

# =================================================================================================
say "phase 2 — kit path on a plain machine, then kit -> image migration"
start plain catprinter-fleet-plain
if [[ $LAST -ne 0 ]]; then
  fail "phase 2: machine did not boot — skipping the phase"
else
  # 1. status on a bare machine: exit 1, runs to the end (no crash)
  assert_rc "$KIT_SH status on a bare machine" 1 x "$KIT_SH" status
  assert_has "status on a bare machine" "--- journal ---"
  assert_not_has "status on a bare machine" "unbound variable"
  assert_has "status header" "none install"
  assert_rc "$KIT_SH install as nobody" 1 x runuser -u nobody -- "$KIT_SH" install
  assert_has "install as nobody" "run as root"
  # 2. kit install
  assert "$KIT_SH install --yes exit 0" x "$KIT_SH" install --yes
  assert "/usr/local/bin/catprinterd installed" x test -x /usr/local/bin/catprinterd
  for f in /etc/systemd/system/catprinter.service /etc/systemd/system/catprinter-queue.service \
           /usr/local/share/catprinter/VERSION /usr/local/share/catprinter/INSTALLED /usr/local/share/catprinter/catprinter.service \
           /usr/local/share/catprinter/catprinter-queue.service /usr/local/share/catprinter/80-catprinter.preset \
           /etc/udev/rules.d/61-catprinter-btusb.rules; do
    assert "$f exists" x test -f "$f"
  done
  assert_eq "catprinter is-enabled (kit)" "$(out1 'systemctl is-enabled catprinter.service')" enabled
  assert_eq "wants symlink target (kit)" "$(out1 'readlink /etc/systemd/system/multi-user.target.wants/catprinter.service')" /etc/systemd/system/catprinter.service
  check_daemon_and_queue "kit" /etc/systemd/system
  assert "kit VERSION copied verbatim" x cmp /kit/VERSION /usr/local/share/catprinter/VERSION
  assert_eq "cut -f1 /usr/local/share/catprinter/VERSION" "$(out1 "cut -d' ' -f1 /usr/local/share/catprinter/VERSION")" "$SEMVER"
  assert "INSTALLED names the source" xs 'grep -q " /kit/catprinterd$" /usr/local/share/catprinter/INSTALLED'
  # 3. update + status
  assert "$KIT_SH update --yes exit 0" x "$KIT_SH" update --yes
  assert "$KIT_SH status exit 0 (kit)" x "$KIT_SH" status
  assert_has "status header (kit)" "kit install"
  if [[ -n $FLEET_DOWNLOAD ]]; then
    # 3b. (opt-in, network) a real --download over the kit install, then back to the local kit
    say "phase 2b — real download: $KIT_SH install --download $FLEET_DOWNLOAD --yes"
    assert "$KIT_SH install --download $FLEET_DOWNLOAD --yes exit 0" x "$KIT_SH" install --download "$FLEET_DOWNLOAD" --yes
    assert_has "download install" "kit fetched into"
    assert "INSTALLED records download:$FLEET_DOWNLOAD" xs "grep -q ' download:$FLEET_DOWNLOAD ' /usr/local/share/catprinter/INSTALLED"
    assert "no leftover /tmp/catprinter-kit.*" xs '! ls -d /tmp/catprinter-kit.* 2>/dev/null | grep -q .'
    assert "$KIT_SH status exit 0 after download" x "$KIT_SH" status
    assert "$KIT_SH install --yes (back to the local kit)" x "$KIT_SH" install --yes
    assert "kit VERSION copied verbatim again" x cmp /kit/VERSION /usr/local/share/catprinter/VERSION
    say "phase 2 — continued"
  fi
  # 4. the machine rebases onto the image: image files appear, local (kit) files and symlinks win
  assert "simulate rebase: cp -an /image-root/. /" xs 'cp -an /image-root/. / && systemctl daemon-reload && systemctl restart catprinter catprinter-queue'
  assert_eq "wants symlink still points at the kit unit" "$(out1 'readlink /etc/systemd/system/multi-user.target.wants/catprinter.service')" /etc/systemd/system/catprinter.service
  # 5. status: mixed, exit 1, says shadow
  assert_rc "$IMG_SH status in the mixed state" 1 x "$IMG_SH" status
  assert_has "mixed status" "shadow"
  assert_has "mixed status header" "mixed install"
  # 6. install migrates the kit off the image
  assert "$IMG_SH install --yes (migration) exit 0" x "$IMG_SH" install --yes
  assert_has "migration" "take over"
  assert "kit binary removed" x test ! -e /usr/local/bin/catprinterd
  assert "kit unit removed" x test ! -e /etc/systemd/system/catprinter.service
  assert "kit queue unit removed" x test ! -e /etc/systemd/system/catprinter-queue.service
  assert "kit share dir removed" x test ! -d /usr/local/share/catprinter
  assert_eq "wants symlink -> image unit" "$(out1 'readlink /etc/systemd/system/multi-user.target.wants/catprinter.service')" /usr/lib/systemd/system/catprinter.service
  assert_eq "queue wants symlink -> image unit" "$(out1 'readlink /etc/systemd/system/multi-user.target.wants/catprinter-queue.service')" /usr/lib/systemd/system/catprinter-queue.service
  check_daemon_and_queue "after migration" /usr/lib/systemd/system
  assert "$IMG_SH status exit 0 after migration" x "$IMG_SH" status
  assert_has "status header after migration" "image-baked install"
  # 7. uninstall is idempotent; --purge removes the DynamicUser state dir
  assert "$IMG_SH uninstall --yes exit 0" x "$IMG_SH" uninstall --yes
  assert "$IMG_SH uninstall --yes again exit 0" x "$IMG_SH" uninstall --yes
  assert "image binary kept after uninstall" x test -x /usr/bin/catprinterd
  assert_eq "catprinter is-enabled after uninstall" "$(out1 'systemctl is-enabled catprinter.service')" disabled
  assert "$IMG_SH uninstall --purge --yes exit 0" x "$IMG_SH" uninstall --purge --yes
  assert "no /var/lib/private/catprinter after --purge" x test ! -e /var/lib/private/catprinter
  assert "no /var/lib/catprinter after --purge" x test ! -e /var/lib/catprinter
  assert "no /etc/catprinter after --purge" x test ! -e /etc/catprinter
  # --download latest is accepted by the parser (status does not fetch anything)
  capture x "$KIT_SH" --download latest status
  assert_not_has "--download latest parses" "unknown argument"
fi

# =================================================================================================
if [[ $FLEET_FIRST_BOOT == 1 ]]; then
  say "phase 3 — image-baked machine, TRUE first boot (preset path)"
  # Remove the machine-id and the shipped wants symlinks before systemd starts: 'enabled' afterwards
  # can only come from 80-catprinter.preset being applied on first boot.
  # systemd 259 lets the kernel command line decide (systemd.condition_first_boot=1 forces PID 1's
  # first-boot path even with a machine-id present; =0 suppresses it even without one). We remove the
  # machine-id as well, so the id becomes transient and systemd-machine-id-commit.service runs — the
  # log-independent proof of a first boot ("Populated /etc with preset unit settings." itself is
  # logged before journald exists and never reaches the container journal).
  CMDLINE="systemd.condition_first_boot=1 systemd.firstboot=off" \
    start image catprinter-fleet-firstboot --entrypoint /bin/sh \
    -- -c 'rm -f /etc/machine-id /etc/systemd/system/multi-user.target.wants/catprinter.service /etc/systemd/system/multi-user.target.wants/catprinter-queue.service; exec /sbin/init'
  if [[ $LAST -ne 0 ]]; then
    fail "phase 3: machine did not boot — skipping the phase"
  else
    assert "machine-id was regenerated" xs 'test "$(cat /etc/machine-id)" != 8f5f8b7c2c8e4a7d9e2b1c3d4e5f6a7b'
    assert_eq "systemd-machine-id-commit.service ran (transient id => first boot)" "$(out1 'systemctl show -p ActiveState --value systemd-machine-id-commit.service')" active
    if xs 'journalctl -b -o cat | grep -q "Populated /etc with preset"' >/dev/null 2>&1; then note "journal has 'Populated /etc with preset unit settings.'"; else note "'Populated /etc with preset' not in the journal (logged before journald; expected)"; fi
    assert_eq "catprinter is-enabled (preset)" "$(out1 'systemctl is-enabled catprinter.service')" enabled
    assert_eq "catprinter-queue is-enabled (preset)" "$(out1 'systemctl is-enabled catprinter-queue.service')" enabled
    assert_eq "wants symlink target (preset)" "$(out1 'readlink /etc/systemd/system/multi-user.target.wants/catprinter.service')" /usr/lib/systemd/system/catprinter.service
    check_daemon_and_queue "first boot" /usr/lib/systemd/system
  fi
fi
