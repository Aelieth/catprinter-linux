#!/usr/bin/env bash
# catprinterd kit installer — one command per machine, run as root.
#
#   sudo ./install.sh [install]            install/upgrade from the binary next to this script
#   sudo ./install.sh update               swap binary + restart (same as install; must be installed)
#   sudo ./install.sh uninstall [--purge]  remove queue, units, files (keep /etc/catprinter unless --purge)
#   ./install.sh status                    show units, health, queue (no root needed except journal)
#   flags: --binary PATH   use this catprinterd instead of ./catprinterd
#          --download [TAG] fetch the kit tarball from GitHub Releases (latest or vX.Y.Z), verify SHA256SUMS
#          --yes           no confirmation prompts
#
# Immutable-first: nothing is layered into rpm-ostree and nothing outside /usr/local (= /var/usrlocal,
# writable and persistent) and /etc is touched. Runtime deps are base-image packages only:
# cups, cups-filters, bluez, util-linux (rfkill), policycoreutils (restorecon), curl, avahi (optional).
# Image-baked installs (/usr/bin/catprinterd + /usr/lib/systemd/system/catprinter.service) are
# detected: then only /etc/catprinter/env is managed and the units restarted.
set -euo pipefail

HERE=$(cd "$(dirname "$(readlink -f "$0")")" && pwd)
REPO_RELEASES="https://github.com/Aelieth/catprinter-linux/releases"
BIN=/usr/local/bin/catprinterd
SHARE=/usr/local/share/catprinter
UNIT_DIR=/etc/systemd/system
ETC=/etc/catprinter
ENV_FILE=$ETC/env
CUPS_FILTERS=$(cups-config --serverbin 2>/dev/null || echo /usr/lib/cups)/filter
UNITS=(catprinter.service catprinter-queue.service)

CMD=install
SRC_BIN=""
DOWNLOAD=""
PURGE=0
YES=0

ok()   { printf '  \033[32m✔\033[0m %s\n' "$*"; }
warn() { printf '  \033[33m!\033[0m %s\n' "$*" >&2; }
bad()  { printf '  \033[31m✘\033[0m %s\n' "$*" >&2; }
die()  { bad "$@"; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

# ---- args --------------------------------------------------------------------------------------
while [[ $# -gt 0 ]]; do
  case "$1" in
    install|update|uninstall|status) CMD=$1 ;;
    --binary)   SRC_BIN=${2:?--binary needs a path}; shift ;;
    --download) DOWNLOAD=latest; if [[ ${2:-} == v* ]]; then DOWNLOAD=$2; shift; fi ;;
    --purge)    PURGE=1 ;;
    --yes|-y)   YES=1 ;;
    -h|--help)  sed -n '2,20p' "$0"; exit 0 ;;
    *) die "unknown argument: $1 (see --help)" ;;
  esac
  shift
done

# ---- settings from /etc/catprinter/env (KEY=VALUE lines only; no sourcing of arbitrary shell) ------
envval() { [[ -r $ENV_FILE ]] && sed -n "s/^[[:space:]]*$1=//p" "$ENV_FILE" | tail -1 | tr -d '"' || true; }
PORT=$(envval CATPRINTER_PORT); PORT=${PORT:-8095}
QUEUE=$(envval CATPRINTER_QUEUE); QUEUE=${QUEUE:-CatPrinter}
URI="ipp://127.0.0.1:$PORT/ipp/print"
HEALTH="http://127.0.0.1:$PORT/health"

image_baked() { [[ -x /usr/bin/catprinterd && -f /usr/lib/systemd/system/catprinter.service ]]; }
installed_bin() { if image_baked; then echo /usr/bin/catprinterd; else echo "$BIN"; fi; }

wait_health() {
  local i
  for i in $(seq 1 30); do
    if curl -fsS --max-time 2 "$HEALTH" >/dev/null 2>&1; then return 0; fi
    sleep 1
  done
  return 1
}

# ---- status (no root) ---------------------------------------------------------------------------
do_status() {
  local rc=0 b
  b=$(installed_bin)
  echo "catprinterd status ($(image_baked && echo image-baked || echo kit) install)"
  for u in "${UNITS[@]}"; do
    printf '  %-26s %s / %s\n' "$u" "$(systemctl is-active "$u" 2>/dev/null || true)" "$(systemctl is-enabled "$u" 2>/dev/null || true)"
    systemctl is-active --quiet "$u" 2>/dev/null || rc=1
  done
  printf '  %-26s %s\n' binary "$([[ -x $b ]] && "$b" --version 2>&1 || echo missing)"
  [[ -r $SHARE/VERSION ]] && printf '  %-26s %s\n' installed "$(cat "$SHARE/VERSION")"
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
  case "$(lpstat -d 2>/dev/null)" in *"$QUEUE"*) warn "$QUEUE is the system default printer — run: lpadmin -d <other>" ;; esac
  printf '  %-26s %s\n' "lpstat -e ($QUEUE count)" "$(lpstat -e 2>/dev/null | grep -cx "$QUEUE" || true)"
  if have avahi-browse; then
    printf '  %-26s %s\n' "dns-sd on lo" "$(timeout 4 avahi-browse -rpt _ipp._tcp 2>/dev/null | grep -c ';lo;' || true)"
  fi
  systemctl is-active --quiet cups-browsed 2>/dev/null && warn "cups-browsed is active — it may create a duplicate queue from DNS-SD (set CATPRINTER_DNSSD=off or disable it)"
  echo "  --- journal ---"
  journalctl -u catprinter -u catprinter-queue -n 15 --no-pager 2>/dev/null | sed 's/^/  /' || true
  return $rc
}

if [[ $CMD == status ]]; then do_status; exit $?; fi

# ---- everything below needs root ------------------------------------------------------------------
[[ $EUID -eq 0 ]] || die "run as root: sudo $0 $CMD"

preflight() {
  local c
  for c in systemctl lpadmin lpstat cupsenable cupsaccept restorecon rfkill curl install; do
    have "$c" || die "missing $c (base packages cups, policycoreutils, util-linux, curl)"
  done
  ok "tools present"
  [[ -x $CUPS_FILTERS/pdftopdf && -x $CUPS_FILTERS/rastertopwg ]] || die "cups-filters incomplete: need $CUPS_FILTERS/{pdftopdf,rastertopwg}"
  [[ -x $CUPS_FILTERS/gstoraster || -x $CUPS_FILTERS/pdftoraster ]] || die "cups-filters incomplete: need gstoraster or pdftoraster"
  ok "cups-filters chain present"
  systemctl is-active --quiet cups || { systemctl start cups || die "cannot start cups"; }
  local i; for i in $(seq 1 15); do lpstat -r 2>/dev/null | grep -q 'is running' && break; sleep 1; done
  ok "cupsd running"
  systemctl is-enabled --quiet bluetooth 2>/dev/null || systemctl enable bluetooth >/dev/null 2>&1 || true
  systemctl is-active --quiet bluetooth || systemctl start bluetooth || warn "bluetooth.service failed to start"
  if rfkill list bluetooth 2>/dev/null | grep -q 'Soft blocked: yes'; then warn "Bluetooth soft-blocked — unblocking"; rfkill unblock bluetooth || true; fi
  rfkill list bluetooth 2>/dev/null | grep -q 'Hard blocked: yes' && warn "Bluetooth is HARD blocked (hardware switch)"
  ok "bluetooth.service $(systemctl is-active bluetooth)"
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
  local tag=$1 tmp url
  tmp=$(mktemp -d)
  if [[ $tag == latest ]]; then url="$REPO_RELEASES/latest/download"; else url="$REPO_RELEASES/download/$tag"; fi
  echo "  downloading $url/catprinter-kit-x86_64.tar.gz"
  curl -fsSL -o "$tmp/kit.tgz" "$url/catprinter-kit-x86_64.tar.gz" || die "download failed"
  curl -fsSL -o "$tmp/SHA256SUMS" "$url/SHA256SUMS" || die "SHA256SUMS download failed"
  (cd "$tmp" && sed -n 's/ .*catprinter-kit.*tar.gz$/  kit.tgz/p' SHA256SUMS | sha256sum -c - >/dev/null) || die "checksum mismatch"
  tar -C "$tmp" -xzf "$tmp/kit.tgz"
  KIT_DIR=$(find "$tmp" -maxdepth 2 -name catprinterd -type f -printf '%h\n' | head -1)
  [[ -n $KIT_DIR ]] || die "kit tarball has no catprinterd"
  ok "kit fetched into $KIT_DIR"
}

resolve_source() {
  KIT_DIR=$HERE
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

do_install() {
  echo "== catprinterd $CMD"
  preflight
  remove_old_user_units
  if image_baked; then
    ok "image-baked install detected (/usr/bin/catprinterd) — managing /etc/catprinter/env and units only"
  else
    resolve_source
    port_check
    install -D -m 0755 "$SRC_BIN" "$BIN"
    for u in "${UNITS[@]}"; do
      install -D -m 0644 "$KIT_DIR/$u" "$SHARE/$u"
      install -m 0644 "$KIT_DIR/$u" "$UNIT_DIR/$u"
    done
    [[ -f $KIT_DIR/80-catprinter.preset ]] && install -m 0644 "$KIT_DIR/80-catprinter.preset" "$SHARE/80-catprinter.preset"
    printf '%s %s\n' "$("$BIN" --version)" "$(date -u +%FT%TZ)" > "$SHARE/VERSION"
    ok "installed $BIN, units in $UNIT_DIR"
  fi
  mkdir -p "$ETC"
  if [[ ! -f $ENV_FILE ]]; then
    if [[ -f $KIT_DIR/env.example ]]; then install -m 0644 "$KIT_DIR/env.example" "$ENV_FILE"; else : > "$ENV_FILE"; fi
    ok "created $ENV_FILE"
  fi
  restorecon -R "$ETC" >/dev/null 2>&1 || true
  image_baked || restorecon -R "$BIN" "$SHARE" "$UNIT_DIR/catprinter.service" "$UNIT_DIR/catprinter-queue.service" >/dev/null 2>&1 || true
  systemctl daemon-reload
  systemctl enable "${UNITS[@]}" >/dev/null 2>&1 || true
  systemctl restart catprinter
  if ! wait_health; then
    journalctl -u catprinter -n 30 --no-pager >&2 || true
    die "catprinterd did not answer on $HEALTH"
  fi
  ok "daemon answering at $HEALTH"
  "$(installed_bin)" check --port "$PORT" >/dev/null 2>&1 && ok "Bluetooth ready" || warn "Bluetooth adapter not powered — kids: turn Bluetooth on"
  systemctl restart catprinter-queue || true
  if ! systemctl is-active --quiet catprinter-queue; then
    journalctl -u catprinter-queue -n 20 --no-pager >&2 || true
    die "lpadmin -m everywhere failed — is the daemon answering on :$PORT?"
  fi
  ok "CUPS queue $QUEUE points at $URI"
  echo
  do_status || true
}

do_uninstall() {
  echo "== catprinterd uninstall"
  if [[ $YES -ne 1 ]]; then read -r -p "Remove queue $QUEUE, units and files? [y/N] " a; [[ $a == y* || $a == Y* ]] || exit 1; fi
  local b; b=$(installed_bin)
  if [[ -x $b ]]; then "$b" ensure-queue --remove --queue "$QUEUE" --port "$PORT" >/dev/null 2>&1 || lpadmin -x "$QUEUE" 2>/dev/null || true
  else lpadmin -x "$QUEUE" 2>/dev/null || true; fi
  ok "queue $QUEUE removed"
  systemctl disable --now "${UNITS[@]}" >/dev/null 2>&1 || true
  if ! image_baked; then
    for u in "${UNITS[@]}"; do rm -f "$UNIT_DIR/$u"; done
    rm -f "$BIN"; rm -rf "$SHARE"
    ok "removed $BIN, $SHARE, units"
  else
    warn "image-baked install: binary/units live in the image and were left alone (units disabled)"
  fi
  systemctl daemon-reload
  if [[ $PURGE -eq 1 ]]; then rm -rf "$ETC" /var/lib/catprinter; ok "purged $ETC and /var/lib/catprinter"; else ok "kept $ETC"; fi
}

case "$CMD" in
  install) do_install ;;
  update)  [[ -f $UNIT_DIR/catprinter.service ]] || image_baked || die "not installed; use: $0 install"; do_install ;;
  uninstall) do_uninstall ;;
esac
