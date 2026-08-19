#!/usr/bin/env bash
# catprinterd kit installer — one command per machine, run as root.
#
#   sudo ./install.sh [install]            install/upgrade from the binary next to this script
#   sudo ./install.sh update               swap binary + restart (same as install; must be installed)
#   sudo ./install.sh uninstall [--purge]  remove queue, units, files (keep /etc/catprinter unless --purge)
#   ./install.sh status                    show units, health, queue (no root needed except journal)
#   ./install.sh status --json             same facts as JSON (exit 0 only when healthy)
#   flags: --binary PATH    use this catprinterd instead of ./catprinterd
#          --download [TAG] fetch the kit tarball from GitHub Releases (latest or vX.Y.Z; default
#                           latest), verify it against SHA256SUMS
#          --yes            no confirmation prompts
#          --json           with status: machine-readable JSON (do not parse the prose table)
#
# Works on any systemd + CUPS distro (Fedora/RHEL/openSUSE, Debian/Ubuntu, Arch), not just ostree
# images. Runtime deps: cups, cups-filters, bluez, util-linux/rfkill, curl (policycoreutils only where
# SELinux is on; avahi optional). We never install packages — a missing dep prints the exact install
# command for the running distro (dnf/apt/pacman/zypper).
# Immutable-first: nothing is layered into rpm-ostree and nothing outside /usr/local (= /var/usrlocal,
# writable and persistent) and /etc is touched.
# Image-baked machines (/usr/bin/catprinterd + /usr/lib/systemd/system/catprinter.service, from
# `make image-files`) are detected: then this script only manages /etc/catprinter/env and the unit
# state (enable/restart), removes leftover kit files (which would shadow the image's units), and
# ignores --download/--binary — the image always wins; ship a new image to update.
# Files: kit VERSION = "<semver> <git-sha|nogit> <build-utc>"; /usr/local/share/catprinter/INSTALLED =
# "<install-utc> <source>".
set -euo pipefail

HERE=$(cd "$(dirname "$(readlink -f "$0")")" && pwd)
REPO_RELEASES="https://github.com/Aelieth/catprinter-linux/releases"
BIN=/usr/local/bin/catprinterd
SHARE=/usr/local/share/catprinter
UNIT_DIR=/etc/systemd/system
ETC=/etc/catprinter
ENV_FILE=$ETC/env
KIT_DIR=$HERE                 # units/env.example/VERSION next to this script unless --download re-points it
IMG_DIR=/usr/lib/catprinter   # image-baked: env.example, VERSION, install.sh (make image-files)
UNITS=(catprinter.service catprinter-queue.service)

CMD=install
SRC_BIN=""
DOWNLOAD=""
PURGE=0
YES=0
STATUS_JSON=0
KIT_EXP_OP=""
KIT_EXP_CONF=""
KIT_EXP_STAMP=""
TMPD=""
KIT_EXP_STAMP_DEFAULT=/etc/catprinter/kit-set-experimental
BLUEZ_CONF_DEFAULT=/etc/bluetooth/main.conf
DISTRO_ID=""
DISTRO_LIKE=""
PKG_FAMILY=""

ok()   { printf '  \033[32m✔\033[0m %s\n' "$*"; }
warn() { printf '  \033[33m!\033[0m %s\n' "$*" >&2; }
bad()  { printf '  \033[31m✘\033[0m %s\n' "$*" >&2; }
die()  { bad "$@"; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

cleanup() { if [[ -n ${TMPD:-} ]]; then rm -rf "$TMPD"; fi; }
trap cleanup EXIT

# ---- distro detection + package hints ----------------------------------------------------------
# We never install packages (immutable-first: on ostree a runtime `dnf install` does not apply). We
# only NAME the right package + command for the running distro, so a missing-dep error is actionable
# everywhere instead of Fedora-only. Parse /etc/os-release (never source it).
os_release() {
  [[ -r /etc/os-release ]] || return 0
  sed -n "s/^$1=//p" /etc/os-release | tail -1 | tr -d '"'
}
detect_distro() {
  DISTRO_ID=$(os_release ID)
  DISTRO_LIKE=$(os_release ID_LIKE)
  case " $DISTRO_ID $DISTRO_LIKE " in
    *" fedora "*|*" rhel "*|*" centos "*|*" rocky "*|*" almalinux "*) PKG_FAMILY=dnf ;;
    *" debian "*|*" ubuntu "*|*" linuxmint "*|*" pop "*)              PKG_FAMILY=apt ;;
    *" arch "*|*" archlinux "*|*" manjaro "*|*" endeavouros "*)       PKG_FAMILY=pacman ;;
    *" suse "*|*" opensuse "*|*" sles "*|*" sled "*)                  PKG_FAMILY=zypper ;;
    *) PKG_FAMILY="" ;;
  esac
}
# command -> abstract dependency name.
dep_of() {
  case "$1" in
    lpadmin|lpstat|cupsenable|cupsaccept|cupsd|lpinfo) echo cups ;;
    driverless) echo cups-filters ;;
    rfkill)     echo rfkill ;;
    restorecon) echo policycoreutils ;;
    systemctl)  echo systemd ;;
    install)    echo coreutils ;;
    curl)       echo curl ;;
    *)          echo "$1" ;;
  esac
}
# abstract dependency -> "package(s) (sudo <pm> install <package(s)>)" for the running distro.
# Name nuances: rfkill is its own package on apt but util-linux elsewhere; avahi CLI is avahi-tools
# (dnf) / avahi-utils (apt,zypper) / avahi (pacman); Debian needs cups-ipp-utils for `driverless`;
# Arch needs bluez-utils; Arch ships no default SELinux.
pkg_hint() {
  local dep=$1 pkg="" cmd=""
  case "$PKG_FAMILY:$dep" in
    dnf:cups) pkg=cups ;;                dnf:cups-filters) pkg=cups-filters ;;
    dnf:bluez) pkg=bluez ;;              dnf:avahi) pkg="avahi avahi-tools" ;;
    dnf:rfkill) pkg=util-linux ;;        dnf:policycoreutils) pkg=policycoreutils ;;
    dnf:curl) pkg=curl ;;                dnf:systemd) pkg=systemd ;;   dnf:coreutils) pkg=coreutils ;;
    apt:cups) pkg="cups cups-client" ;;  apt:cups-filters) pkg="cups-filters cups-ipp-utils" ;;
    apt:bluez) pkg=bluez ;;              apt:avahi) pkg="avahi-daemon avahi-utils" ;;
    apt:rfkill) pkg=rfkill ;;            apt:policycoreutils) pkg=policycoreutils ;;
    apt:curl) pkg=curl ;;                apt:systemd) pkg=systemd ;;   apt:coreutils) pkg=coreutils ;;
    pacman:cups) pkg=cups ;;             pacman:cups-filters) pkg=cups-filters ;;
    pacman:bluez) pkg="bluez bluez-utils" ;; pacman:avahi) pkg=avahi ;;
    pacman:rfkill) pkg=util-linux ;;     pacman:policycoreutils) pkg="(Arch ships no default SELinux)" ;;
    pacman:curl) pkg=curl ;;             pacman:systemd) pkg=systemd ;; pacman:coreutils) pkg=coreutils ;;
    zypper:cups) pkg=cups ;;             zypper:cups-filters) pkg=cups-filters ;;
    zypper:bluez) pkg=bluez ;;           zypper:avahi) pkg="avahi avahi-utils" ;;
    zypper:rfkill) pkg=util-linux ;;     zypper:policycoreutils) pkg=policycoreutils ;;
    zypper:curl) pkg=curl ;;             zypper:systemd) pkg=systemd ;; zypper:coreutils) pkg=coreutils ;;
    *) printf "'%s' (unknown distro — install your CUPS / cups-filters / BlueZ packages)" "$dep"; return 0 ;;
  esac
  case "$PKG_FAMILY" in
    dnf)    cmd="sudo dnf install $pkg" ;;
    apt)    cmd="sudo apt install $pkg" ;;
    pacman) cmd="sudo pacman -S $pkg" ;;
    zypper) cmd="sudo zypper install $pkg" ;;
  esac
  printf '%s (%s)' "$pkg" "$cmd"
}
# CUPS serverbin (filter/backend dir). cups-config lives in cups-devel/libcups2-dev (usually absent at
# runtime); the old /usr/lib/cups fallback is wrong on lib64 distros (Fedora/RHEL/openSUSE).
cups_serverbin() {
  local b
  if have cups-config; then
    b=$(cups-config --serverbin 2>/dev/null || true)
    if [[ -n $b && -d $b ]]; then printf '%s\n' "$b"; return 0; fi
  fi
  for b in /usr/lib64/cups /usr/lib/cups /usr/libexec/cups; do
    if [[ -d $b/filter || -d $b/backend ]]; then printf '%s\n' "$b"; return 0; fi
  done
  return 1
}
# Something can render to what the queue needs: cups-filters `driverless`, CUPS built-in `everywhere`,
# or a known raster filter. Absence is a WARNING only — ensure_queue falls back driverless -> `-m
# everywhere`, and catprinter-queue.service is the authoritative end gate.
raster_path_present() {
  have driverless && return 0
  if have lpinfo && lpinfo -m 2>/dev/null | grep -q 'everywhere'; then return 0; fi
  local b f
  b=$(cups_serverbin) || return 1
  for f in gstoraster pdftoraster rastertopwg pdftopdf; do
    [[ -x $b/filter/$f ]] && return 0
  done
  return 1
}

# ---- args --------------------------------------------------------------------------------------
while [[ $# -gt 0 ]]; do
  case "$1" in
    install|update|uninstall|status) CMD=$1 ;;
    _kit-experimental)
      CMD=$1
      KIT_EXP_OP=${2:-}
      KIT_EXP_CONF=${3:-}
      KIT_EXP_STAMP=${4:-}
      break
      ;;
    --binary)   SRC_BIN=${2:?--binary needs a path}; shift ;;
    --download) DOWNLOAD=latest; if [[ ${2:-} == v* || ${2:-} == latest ]]; then DOWNLOAD=$2; shift; fi ;;
    --purge)    PURGE=1 ;;
    --yes|-y)   YES=1 ;;
    --json)     STATUS_JSON=1 ;;
    -h|--help)  sed -n '2,/^set -euo pipefail$/p' "$0" | sed '$d'; exit 0 ;;
    *) die "unknown argument: $1 (see --help)" ;;
  esac
  shift
done

# ---- settings from /etc/catprinter/env (KEY=VALUE lines only; no sourcing of arbitrary shell) ------
envval() { if [[ -r $ENV_FILE ]]; then sed -n "s/^[[:space:]]*$1=//p" "$ENV_FILE" | tail -1 | tr -d '"'; fi; }
PORT=$(envval CATPRINTER_PORT); PORT=${PORT:-8095}
QUEUE=$(envval CATPRINTER_QUEUE); QUEUE=${QUEUE:-CatPrinter}
URI="ipp://127.0.0.1:$PORT/ipp/print"
HEALTH="http://127.0.0.1:$PORT/health"
ARGS_RE='(^|[[:space:]])--(port|queue|bind)([[:space:]=]|$)'
if [[ $(envval CATPRINTERD_ARGS) =~ $ARGS_RE ]]; then
  warn "CATPRINTERD_ARGS in $ENV_FILE sets --port/--queue/--bind; catprinter-queue.service and this script only read CATPRINTER_PORT/CATPRINTER_QUEUE — use those"
fi

# ---- install modes -----------------------------------------------------------------------------
# image-baked: binary + units shipped in the OS image (make image-files); kit: this installer's files
# under /usr/local + /etc/systemd/system; mixed: a kit-installed machine that rebased onto an image —
# the /etc units shadow the image's until `install` migrates (the image always wins).
image_baked() { [[ -x /usr/bin/catprinterd && -f /usr/lib/systemd/system/catprinter.service ]]; }
kit_present()  { [[ -f $UNIT_DIR/catprinter.service || -f $UNIT_DIR/catprinter-queue.service || -e $BIN || -d $SHARE ]]; }
install_mode() {  # image-baked | kit | mixed | none
  if image_baked; then
    if kit_present; then echo mixed; else echo image-baked; fi
  elif kit_present; then echo kit
  else echo none
  fi
}
installed_bin() { if image_baked; then echo /usr/bin/catprinterd; else echo "$BIN"; fi; }
semver_of() { "$1" --version 2>/dev/null | awk '{print $2; exit}' || true; }   # "catprinterd 0.2.0" -> 0.2.0

wait_health() {
  for _ in $(seq 1 30); do
    if curl -fsS --max-time 2 "$HEALTH" >/dev/null 2>&1; then return 0; fi
    sleep 1
  done
  return 1
}

# ---- status (no root) ---------------------------------------------------------------------------
do_status() {
  local rc=0 mode b u v
  mode=$(install_mode)
  b=$(installed_bin)
  echo "catprinterd status ($mode install)"
  for u in "${UNITS[@]}"; do
    printf '  %-26s %s / %s  %s\n' "$u" "$(systemctl is-active "$u" 2>/dev/null || true)" \
      "$(systemctl is-enabled "$u" 2>/dev/null || true)" "$(systemctl show -p FragmentPath --value "$u" 2>/dev/null || true)"
    systemctl is-active --quiet "$u" 2>/dev/null || rc=1
  done
  if [[ -x /usr/bin/catprinterd ]]; then printf '  %-26s %s\n' "binary (image)" "/usr/bin/catprinterd: $(/usr/bin/catprinterd --version 2>&1 || true)"; fi
  if [[ -x $BIN ]]; then printf '  %-26s %s\n' "binary (kit)" "$BIN: $("$BIN" --version 2>&1 || true)"; fi
  if [[ ! -x /usr/bin/catprinterd && ! -x $BIN ]]; then printf '  %-26s %s\n' binary missing; fi
  for v in "$IMG_DIR/VERSION" "$SHARE/VERSION"; do
    if [[ -r $v ]]; then printf '  %-26s %s\n' "VERSION ($v)" "$(head -n1 "$v")"; fi
  done
  if [[ -r $SHARE/INSTALLED ]]; then printf '  %-26s %s\n' "installed at" "$(head -n1 "$SHARE/INSTALLED")"; fi
  printf '  %-26s %s\n' health "$(curl -fsS --max-time 3 "$HEALTH" 2>/dev/null | tr -d '\n' | cut -c1-200 || echo DOWN)"
  printf '  %-26s %s\n' check "$([[ -x $b ]] && "$b" check --port "$PORT" >/dev/null 2>&1 && echo ok || echo 'NOT READY (Bluetooth off?)')"
  printf '  %-26s %s' bluetooth "$(systemctl is-active bluetooth 2>/dev/null || true)"
  if have rfkill; then printf ' / rfkill: %s blocked' "$(rfkill list bluetooth 2>/dev/null | grep -c 'blocked: yes' || true)"; fi
  echo
  echo "  --- CUPS ---"
  if lpstat -v "$QUEUE" >/dev/null 2>&1; then
    lpstat -v "$QUEUE" | sed 's/^/  /'
    lpstat -p "$QUEUE" -l 2>/dev/null | head -4 | sed 's/^/  /'
    lpstat -o "$QUEUE" 2>/dev/null | head -5 | sed 's/^/  /'
    lpoptions -p "$QUEUE" -l 2>/dev/null | grep -E '^(PageSize|cupsPrintQuality)/' | cut -c1-140 | sed 's/^/  /' || true
  else
    bad "queue $QUEUE missing"; rc=1
  fi
  printf '  %-26s %s\n' default "$(lpstat -d 2>/dev/null || echo none)"
  if [[ $(lpstat -d 2>/dev/null | awk '{print $NF}') == "$QUEUE" ]]; then
    warn "$QUEUE is the system default printer — run: lpadmin -d <other>"
  fi
  printf '  %-26s %s\n' "lpstat -e ($QUEUE count)" "$(lpstat -e 2>/dev/null | grep -cx "$QUEUE" || true)"
  if have avahi-browse; then
    printf '  %-26s %s\n' "dns-sd on lo" "$(timeout 4 avahi-browse -rpt _ipp._tcp 2>/dev/null | grep -c ';lo;' || true)"
  fi
  if systemctl is-active --quiet cups-browsed 2>/dev/null; then
    warn "cups-browsed is active — it may create a duplicate queue from DNS-SD (set CATPRINTER_DNSSD=off or disable it)"
  fi
  echo "  --- journal ---"
  journalctl -u catprinter -u catprinter-queue -n 15 --no-pager 2>/dev/null | sed 's/^/  /' || true
  if [[ $mode == mixed ]]; then
    bad "kit files ($UNIT_DIR/catprinter*.service, $BIN) shadow the image's units — run: sudo $0 install (migrates to the image)"
    rc=1
  fi
  return $rc
}

json_str() {
  local s=$1
  s=${s//\\/\\\\}
  s=${s//\"/\\\"}
  s=${s//$'\n'/ }
  printf '%s' "$s"
}

do_status_json() {
  local rc=0 mode u active enabled health queue_state default_q
  mode=$(install_mode)
  echo '{'
  printf '  "mode": "%s",\n' "$(json_str "$mode")"
  printf '  "port": %s,\n' "$PORT"
  printf '  "queue": "%s",\n' "$(json_str "$QUEUE")"
  echo '  "units": {'
  local first=1
  for u in "${UNITS[@]}"; do
    active=$(systemctl is-active "$u" 2>/dev/null || true)
    enabled=$(systemctl is-enabled "$u" 2>/dev/null || true)
    systemctl is-active --quiet "$u" 2>/dev/null || rc=1
    if [[ $first -eq 1 ]]; then first=0; else printf ',\n'; fi
    printf '    "%s": {"active":"%s","enabled":"%s"}' "$(json_str "$u")" "$(json_str "$active")" "$(json_str "$enabled")"
  done
  echo
  echo '  },'
  if curl -fsS --max-time 3 "$HEALTH" >/dev/null 2>&1; then health=up; else health=down; rc=1; fi
  printf '  "health": "%s",\n' "$health"
  if lpstat -v "$QUEUE" >/dev/null 2>&1; then queue_state=present; else queue_state=missing; rc=1; fi
  printf '  "queue_present": %s,\n' "$([[ $queue_state == present ]] && echo true || echo false)"
  default_q=$(lpstat -d 2>/dev/null | awk '{print $NF}' || true)
  if [[ $default_q == "$QUEUE" ]]; then printf '  "queue_is_default": true,\n'; else printf '  "queue_is_default": false,\n'; fi
  if [[ $mode == mixed ]]; then rc=1; fi
  if [[ $rc -eq 0 ]]; then
    printf '  "ok": true\n'
  else
    printf '  "ok": false\n'
  fi
  echo '}'
  return $rc
}

if [[ $CMD == status ]]; then
  if [[ $STATUS_JSON -eq 1 ]]; then do_status_json; else do_status; fi
  exit $?
fi

# ---- everything below needs root (except the Experimental text helper used by tests) ----------
if [[ $CMD != _kit-experimental ]]; then
  [[ $EUID -eq 0 ]] || die "run as root: sudo $0 $CMD"
fi

preflight() {
  detect_distro
  local c
  # Hard requirements: without these the installer itself cannot run.
  for c in systemctl lpadmin lpstat install curl; do
    have "$c" || die "missing '$c' — install $(pkg_hint "$(dep_of "$c")")"
  done
  ok "core tools present"
  # Best-effort helpers: the daemon/queue tolerate their absence, so warn (don't die).
  for c in cupsenable cupsaccept rfkill; do
    have "$c" || warn "'$c' not found (best-effort only) — install $(pkg_hint "$(dep_of "$c")")"
  done
  if [[ -d /sys/fs/selinux ]]; then
    have restorecon || warn "SELinux is on but 'restorecon' is missing — install $(pkg_hint policycoreutils)"
  fi
  # cupsd: try the service, then socket-activation (Debian/Ubuntu ship cups.socket), then poll.
  systemctl is-active --quiet cups 2>/dev/null || systemctl start cups 2>/dev/null \
    || systemctl start cups.socket 2>/dev/null \
    || die "cannot start CUPS — install $(pkg_hint cups), then: systemctl start cups"
  for _ in $(seq 1 15); do
    if lpstat -r 2>/dev/null | grep -q 'is running'; then break; fi
    sleep 1
  done
  lpstat -r 2>/dev/null | grep -q 'is running' || die "cupsd did not come up (systemctl status cups)"
  ok "cupsd running"
  # Not fatal: ensure_queue falls back driverless -> `-m everywhere`, and the queue step is the real gate.
  if raster_path_present; then ok "driverless/everywhere print path available"
  else warn "no driverless/everywhere raster path detected — printing may not render; install $(pkg_hint cups-filters)"; fi
  # ---- Bluetooth (best-effort; the daemon starts even without an adapter) ----
  systemctl cat bluetooth.service >/dev/null 2>&1 || warn "bluetooth.service not found — install $(pkg_hint bluez)"
  systemctl is-enabled --quiet bluetooth 2>/dev/null || systemctl enable bluetooth >/dev/null 2>&1 || true
  systemctl is-active --quiet bluetooth || systemctl start bluetooth || warn "bluetooth.service failed to start"
  if rfkill list bluetooth 2>/dev/null | grep -q 'Soft blocked: yes'; then warn "Bluetooth soft-blocked — unblocking"; rfkill unblock bluetooth || true; fi
  if rfkill list bluetooth 2>/dev/null | grep -q 'Hard blocked: yes'; then warn "Bluetooth is HARD blocked (hardware switch)"; fi
  if systemctl is-active --quiet bluetooth; then ok "bluetooth.service active"
  else warn "bluetooth.service $(systemctl is-active bluetooth 2>/dev/null || true) — no usable Bluetooth adapter? (printing needs one; the daemon still starts)"; fi
}

# Old per-user Python daemon (mxw01d) owns :8095 while that user is logged in — remove it.
remove_old_user_units() {
  local user home unit
  while IFS=: read -r user _ uid _ _ home _; do
    [[ $uid -ge 1000 && -d $home ]] || continue
    unit="$home/.config/systemd/user/catprinter-mxw01d.service"
    [[ -f $unit ]] || continue
    warn "removing old per-user daemon for $user"
    systemctl --user -M "$user@" disable --now catprinter-mxw01d >/dev/null 2>&1 || true
    rm -f "$unit"
    systemctl --user -M "$user@" daemon-reload >/dev/null 2>&1 || true
  done < <(getent passwd)
}

fetch_kit() {
  local tag=$1 url arch sum
  arch=$(uname -m)
  case "$arch" in
    x86_64|aarch64) ;;
    *) die "no prebuilt kit for $arch — build from source: cargo build --release, then $0 install --binary target/release/catprinterd" ;;
  esac
  TMPD=$(mktemp -d /tmp/catprinter-kit.XXXXXX)
  if [[ $tag == latest ]]; then url="$REPO_RELEASES/latest/download"; else url="$REPO_RELEASES/download/$tag"; fi
  echo "  downloading $url/catprinter-kit-$arch.tar.gz"
  curl -fsSL --proto '=https' -o "$TMPD/kit.tgz" "$url/catprinter-kit-$arch.tar.gz" || die "download failed"
  curl -fsSL --proto '=https' -o "$TMPD/SHA256SUMS" "$url/SHA256SUMS" || die "SHA256SUMS download failed"
  sum=$(awk -v f="catprinter-kit-$arch.tar.gz" '$2==f || $2=="*"f {print $1; exit}' "$TMPD/SHA256SUMS")
  [[ ${#sum} -eq 64 ]] || die "SHA256SUMS has no entry for catprinter-kit-$arch.tar.gz"
  (cd "$TMPD" && printf '%s  kit.tgz\n' "$sum" | sha256sum -c --quiet -) || die "checksum mismatch"
  tar -C "$TMPD" --no-same-owner -xzf "$TMPD/kit.tgz"
  KIT_DIR=$(find "$TMPD" -maxdepth 2 -name catprinterd -type f -printf '%h\n' | head -1)
  [[ -n $KIT_DIR ]] || die "kit tarball has no catprinterd"
  ok "kit fetched into $KIT_DIR"
}

resolve_source() {
  if [[ -n $DOWNLOAD ]]; then fetch_kit "$DOWNLOAD"; fi
  if [[ -z $SRC_BIN ]]; then SRC_BIN=$KIT_DIR/catprinterd; fi
  [[ -x $SRC_BIN ]] || die "no catprinterd binary at $SRC_BIN (use --binary PATH or --download)"
  "$SRC_BIN" --version >/dev/null 2>&1 || die "$SRC_BIN does not run on this machine"
  ok "binary: $SRC_BIN ($("$SRC_BIN" --version))"
  for u in "${UNITS[@]}"; do [[ -f $KIT_DIR/$u ]] || die "missing $KIT_DIR/$u"; done
}

port_check() {
  if have ss && ss -Hltn "sport = :$PORT" 2>/dev/null | grep -q . && ! systemctl is-active --quiet catprinter; then
    die "port $PORT is in use by something else: $(ss -Hltnp "sport = :$PORT" 2>/dev/null | head -2)"
  fi
}

# VERSION: copy the kit's line ("<semver> <sha|nogit> <build-utc>") when its semver matches the binary,
# else record "<semver> nogit <now>". INSTALLED: "<install-utc> <source>".
write_version_files() {
  local want line=""
  want=$(semver_of "$BIN")
  if [[ -r $KIT_DIR/VERSION ]]; then line=$(head -n1 "$KIT_DIR/VERSION"); fi
  if [[ ${line%% *} != "$want" ]]; then
    if [[ -n $line ]]; then warn "kit VERSION says '${line%% *}' but the installed binary is $want — recording '$want nogit'"; fi
    line="$want nogit $(date -u +%FT%TZ)"
  fi
  printf '%s\n' "$line" > "$SHARE/VERSION"
  printf '%s %s\n' "$(date -u +%FT%TZ)" "${DOWNLOAD:+download:$DOWNLOAD }$SRC_BIN" > "$SHARE/INSTALLED"
}

# Disable USB BT autosuspend on Wireless Controller interfaces (e0/01/01) and their
# parent. Combo IAD cards are bDeviceClass=ef — a device-class e0 rule misses them.
# Image ships the rule in /usr/lib/udev/rules.d; kit copies it to /etc.
ensure_btusb_udev() {
  local name=61-catprinter-btusb.rules src="" dest_etc dest_lib
  dest_etc=/etc/udev/rules.d/$name
  dest_lib=/usr/lib/udev/rules.d/$name
  for c in "$KIT_DIR/$name" "$IMG_DIR/$name"; do
    if [[ -f $c ]]; then src=$c; break; fi
  done
  if [[ -f $dest_lib ]]; then
    if have udevadm; then
      udevadm control --reload-rules >/dev/null 2>&1 || true
      udevadm trigger --action=change --subsystem-match=usb >/dev/null 2>&1 || true
    fi
    ok "udev $dest_lib (USB Bluetooth autosuspend off)"
    return 0
  fi
  [[ -n $src ]] || return 0
  install -D -m 0644 "$src" "$dest_etc"
  if have udevadm; then
    udevadm control --reload-rules >/dev/null 2>&1 || true
    udevadm trigger --action=change --subsystem-match=usb >/dev/null 2>&1 || true
  fi
  ok "udev $dest_etc (USB Bluetooth autosuspend off)"
}

# Write Experimental = true. Stamp records whether *this kit* introduced it
# (`added` or `changed`). Pre-existing true leaves no stamp so uninstall
# will not turn Experimental off for someone else.
kit_apply_experimental() {
  local conf=${1:?} stamp=${2:?} tmp
  mkdir -p "$(dirname "$conf")" "$(dirname "$stamp")"
  if [[ -f $conf ]] && grep -qE '^[[:space:]]*Experimental[[:space:]]*=[[:space:]]*true([[:space:]]|$)' "$conf"; then
    return 0
  fi
  if [[ -f $conf ]] && grep -qE '^[[:space:]]*Experimental[[:space:]]*=' "$conf"; then
    printf 'changed\n' > "$stamp"
    tmp=$(mktemp)
    sed -E 's/^[[:space:]]*Experimental[[:space:]]*=.*/Experimental = true/' "$conf" > "$tmp"
    install -m 0644 "$tmp" "$conf"
    rm -f "$tmp"
  elif [[ -f $conf ]] && grep -qE '^\[General\]' "$conf"; then
    printf 'added\n' > "$stamp"
    tmp=$(mktemp)
    awk 'BEGIN{d=0} /^\[General\]/{print; if(!d){print "Experimental = true"; d=1} next} {print} END{if(!d) print "\n[General]\nExperimental = true"}' "$conf" > "$tmp"
    install -m 0644 "$tmp" "$conf"
    rm -f "$tmp"
  else
    printf 'added\n' > "$stamp"
    mkdir -p "$(dirname "$conf")"
    printf '\n[General]\nExperimental = true\n' >> "$conf"
  fi
}

# Undo only a kit-introduced Experimental. No stamp → leave main.conf alone.
kit_revert_experimental() {
  local conf=${1:?} stamp=${2:?} how tmp
  [[ -f $stamp ]] || return 0
  how=$(tr -d '[:space:]' < "$stamp")
  if [[ -f $conf ]]; then
    tmp=$(mktemp)
    case "$how" in
      changed)
        sed -E 's/^[[:space:]]*Experimental[[:space:]]*=.*/Experimental = false/' "$conf" > "$tmp"
        ;;
      *)
        grep -vE '^[[:space:]]*Experimental[[:space:]]*=[[:space:]]*true([[:space:]]|$)' "$conf" > "$tmp" || true
        ;;
    esac
    install -m 0644 "$tmp" "$conf"
    rm -f "$tmp"
  fi
  rm -f "$stamp"
}

# BlueZ 5.87 hides Adapter1.ConnectDevice unless Experimental is on. That method
# is how we force LE instead of Classic on MXW01 ads that look dual-mode.
ensure_bluez_experimental() {
  local conf=${BLUEZ_CONF:-$BLUEZ_CONF_DEFAULT}
  local stamp=${KIT_EXP_STAMP_PATH:-$KIT_EXP_STAMP_DEFAULT}
  mkdir -p /etc/bluetooth /etc/catprinter
  if [[ -f $conf ]] && grep -qE '^[[:space:]]*Experimental[[:space:]]*=[[:space:]]*true([[:space:]]|$)' "$conf"; then
    ok "BlueZ Experimental already on ($conf)"
    return 0
  fi
  kit_apply_experimental "$conf" "$stamp"
  if [[ $conf == /etc/bluetooth/main.conf ]]; then
    systemctl restart bluetooth >/dev/null 2>&1 || warn "restart bluetooth.service after Experimental = true"
  fi
  ok "BlueZ Experimental = true in $conf (LE ConnectDevice)"
}

revert_adopted_record() {
  local f mac b
  b=$(installed_bin)
  mac=""
  for f in /var/lib/catprinter/adopted /var/lib/private/catprinter/adopted; do
    if [[ -r $f ]]; then
      mac=$(head -n1 "$f" | tr -d '[:space:]')
      break
    fi
  done
  if [[ -x $b ]]; then
    if [[ -n $mac ]]; then
      "$b" adopt --forget --device "$mac" >/dev/null 2>&1 || true
    else
      "$b" adopt --forget >/dev/null 2>&1 || true
    fi
  fi
  rm -f /var/lib/catprinter/adopted /var/lib/private/catprinter/adopted
}

if [[ $CMD == _kit-experimental ]]; then
  case "$KIT_EXP_OP" in
    apply) kit_apply_experimental "$KIT_EXP_CONF" "$KIT_EXP_STAMP" ;;
    revert) kit_revert_experimental "$KIT_EXP_CONF" "$KIT_EXP_STAMP" ;;
    *) die "usage: $0 _kit-experimental apply|revert CONF STAMP" ;;
  esac
  exit 0
fi

# A kit-installed machine that rebased onto an image-baked image: the /etc units shadow the image's.
migrate_kit_off_image() {
  warn "kit files found on an image-baked machine — $UNIT_DIR/catprinter*.service shadow the image's units; removing the kit"
  systemctl disable "${UNITS[@]}" >/dev/null 2>&1 || true   # drops the wants symlinks that point at the kit units
  rm -f "$UNIT_DIR/catprinter.service" "$UNIT_DIR/catprinter-queue.service" "$BIN"
  rm -rf "$SHARE"
  systemctl daemon-reload
  ok "removed $BIN, $SHARE and the kit units; /usr/lib/systemd/system/catprinter*.service take over"
}

do_install() {
  local u iv kv src c
  echo "== catprinterd $CMD"
  preflight
  remove_old_user_units
  if image_baked; then
    ok "image-baked machine (/usr/bin/catprinterd + /usr/lib/systemd/system units) — managing $ENV_FILE and unit state"
    if [[ -n $DOWNLOAD || -n $SRC_BIN ]]; then
      warn "image-baked machine: binary and units come from the image — --download/--binary ignored (ship a new image to update)"
    fi
    if [[ -x $HERE/catprinterd ]]; then
      kv=$(semver_of "$HERE/catprinterd"); iv=$(semver_of /usr/bin/catprinterd)
      if [[ -n $kv && $kv != "$iv" ]]; then warn "the kit next to this script is $kv; the image has $iv — the image wins (ship a new image to update)"; fi
    fi
    if kit_present; then migrate_kit_off_image; fi
  else
    resolve_source
    port_check
    install -D -m 0755 "$SRC_BIN" "$BIN"
    for u in "${UNITS[@]}"; do
      install -D -m 0644 "$KIT_DIR/$u" "$SHARE/$u"
      install -m 0644 "$KIT_DIR/$u" "$UNIT_DIR/$u"
    done
    if [[ -f $KIT_DIR/80-catprinter.preset ]]; then install -m 0644 "$KIT_DIR/80-catprinter.preset" "$SHARE/80-catprinter.preset"; fi
    write_version_files
    ok "installed $BIN, units in $UNIT_DIR ($(head -n1 "$SHARE/VERSION"))"
  fi
  mkdir -p "$ETC"
  if [[ ! -f $ENV_FILE ]]; then
    src=""
    for c in "$KIT_DIR/env.example" "$IMG_DIR/env.example"; do if [[ -f $c ]]; then src=$c; break; fi; done
    if [[ -n $src ]]; then install -m 0644 "$src" "$ENV_FILE"; else : > "$ENV_FILE"; fi
    ok "created $ENV_FILE${src:+ from $src}"
  fi
  if [[ -d /sys/fs/selinux ]] && have restorecon; then
    restorecon -R "$ETC" >/dev/null 2>&1 || true
    if ! image_baked; then restorecon -R "$BIN" "$SHARE" "$UNIT_DIR/catprinter.service" "$UNIT_DIR/catprinter-queue.service" >/dev/null 2>&1 || true; fi
  fi
  ensure_btusb_udev
  ensure_bluez_experimental
  systemctl daemon-reload
  systemctl enable "${UNITS[@]}" >/dev/null 2>&1 || true
  systemctl restart catprinter || { journalctl -u catprinter -n 30 --no-pager >&2 || true; die "catprinter.service failed to start"; }
  if ! wait_health; then
    journalctl -u catprinter -n 30 --no-pager >&2 || true
    die "catprinterd did not answer on $HEALTH"
  fi
  ok "daemon answering at $HEALTH"
  if "$(installed_bin)" check --port "$PORT" >/dev/null 2>&1; then ok "Bluetooth ready"; else warn "Bluetooth adapter not powered — kids: turn Bluetooth on"; fi
  systemctl restart catprinter-queue || true
  if ! systemctl is-active --quiet catprinter-queue; then
    journalctl -u catprinter-queue -n 20 --no-pager >&2 || true
    die "queue creation failed — is the daemon answering on :$PORT, and is cups-filters installed ($(pkg_hint cups-filters))?"
  fi
  ok "CUPS queue $QUEUE points at $URI"
  echo
  do_status || true
}

do_uninstall() {
  local a b u
  echo "== catprinterd uninstall"
  if [[ $YES -ne 1 ]]; then
    read -r -p "Remove queue $QUEUE, units and files? [y/N] " a || die "no answer (use --yes)"
    [[ $a == y* || $a == Y* ]] || exit 1
  fi
  revert_adopted_record
  kit_revert_experimental "${BLUEZ_CONF:-$BLUEZ_CONF_DEFAULT}" "${KIT_EXP_STAMP_PATH:-$KIT_EXP_STAMP_DEFAULT}"
  b=$(installed_bin)
  if [[ -x $b ]]; then "$b" ensure-queue --remove --queue "$QUEUE" --port "$PORT" >/dev/null 2>&1 || lpadmin -x "$QUEUE" 2>/dev/null || true
  else lpadmin -x "$QUEUE" 2>/dev/null || true; fi
  ok "queue $QUEUE removed"
  systemctl disable --now "${UNITS[@]}" >/dev/null 2>&1 || true
  rm -f /etc/udev/rules.d/61-catprinter-btusb.rules
  if have udevadm; then udevadm control --reload-rules >/dev/null 2>&1 || true; fi
  if kit_present; then
    for u in "${UNITS[@]}"; do rm -f "$UNIT_DIR/$u"; done
    rm -f "$BIN"; rm -rf "$SHARE"
    ok "removed $BIN, $SHARE and the kit units"
  fi
  if image_baked; then
    warn "image-baked machine: /usr/bin/catprinterd and the image units were left alone (units disabled; re-enable with: sudo $0 install)"
  fi
  systemctl daemon-reload
  if [[ $PURGE -eq 1 ]]; then
    rm -rf "$ETC" /var/lib/catprinter /var/lib/private/catprinter
    ok "purged $ETC, /var/lib/catprinter and /var/lib/private/catprinter"
  else
    ok "kept $ETC"
  fi
}

case "$CMD" in
  install) do_install ;;
  update)  [[ -f $UNIT_DIR/catprinter.service ]] || image_baked || die "not installed; use: $0 install"; do_install ;;
  uninstall) do_uninstall ;;
esac
