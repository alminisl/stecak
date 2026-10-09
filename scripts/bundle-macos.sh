#!/bin/sh
# Build Stećak.app from a release binary.
#   scripts/bundle-macos.sh [target-triple]   e.g. aarch64-apple-darwin, x86_64-apple-darwin, or "universal"
# Output: dist/Stećak.app (ad-hoc signed so it runs locally; not notarized).
set -eu
cd "$(dirname "$0")/.."

TARGET="${1:-}"
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)

if [ "$TARGET" = "universal" ]; then
    cargo build --release --target aarch64-apple-darwin
    cargo build --release --target x86_64-apple-darwin
    BIN=target/stecak-universal
    lipo -create -output "$BIN" target/aarch64-apple-darwin/release/stecak target/x86_64-apple-darwin/release/stecak
elif [ -n "$TARGET" ]; then
    cargo build --release --target "$TARGET"
    BIN="target/$TARGET/release/stecak"
else
    cargo build --release
    BIN=target/release/stecak
fi

APP="dist/Stećak.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/stecak"
cp assets/stecak.icns "$APP/Contents/Resources/stecak.icns"
sed "s/__VERSION__/$VERSION/g" assets/Info.plist > "$APP/Contents/Info.plist"
codesign --force --deep --sign - "$APP" >/dev/null 2>&1 || echo "warning: ad-hoc codesign failed"
echo "built $APP ($VERSION)"
