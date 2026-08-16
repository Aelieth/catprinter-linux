#!/usr/bin/env bash
# Regenerate tests/fixtures/*.pwg with the host's CUPS filters (cups-filters: pdftopdf,
# gstoraster/pdftoraster, rastertopwg). Uses scripts/fixture.ppd, a throw-away PPD whose only
# purpose is to make cupsfilter emit image/pwg-raster at 203 dpi sGray-8 — the daemon itself
# ships no PPD (CUPS generates one via `lpadmin -m everywhere`).
#
# Requires: cupsfilter (cups), the cups-filters chain, zcat, magick (ImageMagick) for the photo.
# Fixtures are committed; run this only when the CUPS toolchain or the sample inputs change.
set -euo pipefail

cd "$(dirname "$(readlink -f "$0")")/.."
PPD=scripts/fixture.ppd
FIX=tests/fixtures
SAMPLES=tests/samples
mkdir -p "$FIX" "$SAMPLES"

for tool in cupsfilter zcat; do
  command -v "$tool" >/dev/null || { echo "missing $tool" >&2; exit 1; }
done

# --- sample inputs -----------------------------------------------------------------------
# A plain-text file → PDF through texttopdf (needs a PPD for its page size: the roll).
[[ -f "$SAMPLES/text.pdf" ]] || \
  cupsfilter -p "$PPD" -m application/pdf "$SAMPLES/text-file.txt" > "$SAMPLES/text.pdf" 2>/dev/null
# CUPS's own one-page A4 test document.
[[ -f "$SAMPLES/onepage-a4.pdf" ]] || \
  zcat /usr/share/cups/ipptool/onepage-a4.pdf.gz > "$SAMPLES/onepage-a4.pdf"
# A baseline JPEG (imagetopdf cannot open the original progressive hackoclock.jpg), 600 px wide.
if [[ ! -f "$SAMPLES/photo.jpg" ]]; then
  command -v magick >/dev/null || { echo "missing magick (ImageMagick) for photo.jpg" >&2; exit 1; }
  magick media/hackoclock.jpg -resize 600x -interlace none -strip -quality 85 "$SAMPLES/photo.jpg"
fi

# --- PWG raster fixtures ---------------------------------------------------------------------
gen() { # name, then cupsfilter args…
  local name=$1; shift
  cupsfilter -p "$PPD" -m image/pwg-raster "$@" > "$FIX/$name.pwg" 2>/dev/null
  printf '%-22s %7d bytes  pages=%s\n' "$name.pwg" "$(stat -c %s "$FIX/$name.pwg")" \
    "$(grep -ao PwgRaster "$FIX/$name.pwg" | wc -l)"
}

gen text-roll48      -o PageSize=Roll48 "$SAMPLES/text.pdf"          # 383 px wide roll page
gen photo-roll48     -o PageSize=Roll48 "$SAMPLES/photo.jpg"         # photo on the roll
gen onepage-a4-doc   -o PageSize=DocA4  "$SAMPLES/text.pdf"          # 1678 px wide A4 sheet
gen twoup-2copies    -o PageSize=DocA4 -o number-up=2 -o landscape -n 2 "$SAMPLES/text.pdf"  # 2 pages

echo "fixtures written to $FIX"
