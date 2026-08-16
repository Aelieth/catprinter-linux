#!/bin/bash
# Refresh this payload from the repo root. Run from deploy/ after driver edits.
set -euo pipefail
dest="$(cd "$(dirname "$(readlink -f "$0")")" && pwd)"
# The repo, explicitly. Once this folder is shipped inside the kit, "the
# directory above me" is the kit — not a catprinter checkout — and a silent
# sync from the wrong tree is worse than a refusal.
if [ -n "${CATPRINTER_SRC:-}" ]; then
  root="$CATPRINTER_SRC"
elif [ -n "${1:-}" ]; then
  root="$(cd "$1" && pwd)"
else
  root="$(cd "$dest/.." && pwd)"
fi
[ -f "$root/mxw01d" ] && [ -d "$root/catprinter" ] || {
  echo "not a catprinter repo: $root" >&2
  echo "  pass it: $0 /path/to/catprinter   (or set CATPRINTER_SRC)" >&2
  exit 1
}

mkdir -p "$dest/catprinter"
cp -a "$root/catprinter/__init__.py" \
      "$root/catprinter/ble.py" \
      "$root/catprinter/img.py" \
      "$root/catprinter/ipp.py" \
      "$root/catprinter/protocol.py" \
      "$root/catprinter/render.py" \
      "$dest/catprinter/"
cp -a "$root/mxw01d" "$root/print.py" "$root/requirements.txt" "$dest/"
cp -a "$root/data/catprinter.ppd" "$dest/catprinter.ppd"
chmod +x "$dest/mxw01d" "$dest/print.py"
echo "Updated $dest from $root"
