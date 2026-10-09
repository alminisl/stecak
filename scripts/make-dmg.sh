#!/bin/sh
# Package dist/Stećak.app into a drag-to-install disk image.
#   scripts/make-dmg.sh [output.dmg]     (run scripts/bundle-macos.sh first)
set -eu
cd "$(dirname "$0")/.."

APP="dist/Stećak.app"
OUT="${1:-dist/Stecak.dmg}"
[ -d "$APP" ] || { echo "missing $APP: run scripts/bundle-macos.sh first" >&2; exit 1; }

STAGE=$(mktemp -d)
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
rm -f "$OUT"
hdiutil create -volname "Stećak" -srcfolder "$STAGE" -fs HFS+ -format UDZO -ov "$OUT" >/dev/null
rm -rf "$STAGE"
echo "built $OUT"
