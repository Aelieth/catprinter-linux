#!/bin/bash
# Bind a systemd --user unit to a copy of this tree.
#
#   --root <dir>         the tree the unit should RUN from (default: this one).
#                        A fleet installer reads the template from the kit but
#                        aims the unit at /var/lib — those are different paths.
#   --dest <dir>         write into <dir> instead of ~/.config/systemd/user
#   --wanted-by <target> default.target (hand install) or graphical-session.target
#   --stdout             print the unit and exit; the caller decides the name
set -euo pipefail

HERE="$(cd "$(dirname "$(readlink -f "$0")")" && pwd)"
ROOT="$HERE"
DEST="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
WANTEDBY="default.target"
STDOUT=0

while [ $# -gt 0 ]; do
  case "$1" in
    --root)      ROOT="$2"; shift 2 ;;
    --dest)      DEST="$2"; shift 2 ;;
    --wanted-by) WANTEDBY="$2"; shift 2 ;;
    --stdout)    STDOUT=1; shift ;;
    -h|--help)   sed -n '2,11p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

TEMPLATE="$HERE/catprinter-mxw01d.service.in"
[ -f "$TEMPLATE" ] || { echo "missing $TEMPLATE" >&2; exit 1; }

render() { sed -e "s|@ROOT@|$ROOT|g" -e "s|@WANTEDBY@|$WANTEDBY|g" "$TEMPLATE"; }

if [ "$STDOUT" -eq 1 ]; then render; exit 0; fi

mkdir -p "$DEST"
render > "$DEST/catprinter-mxw01d.service"
echo "Wrote $DEST/catprinter-mxw01d.service"
grep -E '^(ExecStart|WantedBy)=' "$DEST/catprinter-mxw01d.service"
echo "Then: systemctl --user daemon-reload && systemctl --user enable --now catprinter-mxw01d"
