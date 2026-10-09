#!/bin/sh
# Stećak installer: builds from source and installs for the current user.
#
#   curl -fsSL https://raw.githubusercontent.com/alminisl/stecak/main/install.sh | sh
#   or, from a checkout:  ./install.sh
#
# macOS: installs Stećak.app into /Applications (or ~/Applications) and a `stecak` command.
# Linux: installs the binary into ~/.local/bin plus a desktop entry and icon.
# Requires a Rust toolchain (https://rustup.rs).
set -eu

REPO="https://github.com/alminisl/stecak.git"
say() { printf '\033[1;33m==>\033[0m %s\n' "$*"; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

command -v cargo >/dev/null 2>&1 || die "Rust is required: install it from https://rustup.rs and re-run."

# Use the current checkout if we're in one, otherwise clone into a temp dir.
if [ -f Cargo.toml ] && grep -q '^name = "stecak"' Cargo.toml; then
    SRC=$(pwd)
else
    command -v git >/dev/null 2>&1 || die "git is required."
    SRC=$(mktemp -d)/stecak
    say "Cloning $REPO"
    git clone --depth 1 "$REPO" "$SRC"
fi
cd "$SRC"

BIN_DIR="$HOME/.local/bin"

case "$(uname -s)" in
Darwin)
    say "Building Stećak.app (release)…"
    sh scripts/bundle-macos.sh
    if [ -w /Applications ]; then APPS=/Applications; else APPS="$HOME/Applications"; mkdir -p "$APPS"; fi
    rm -rf "$APPS/Stećak.app"
    cp -R "dist/Stećak.app" "$APPS/"
    mkdir -p "$BIN_DIR"
    ln -sf "$APPS/Stećak.app/Contents/MacOS/stecak" "$BIN_DIR/stecak"
    say "Installed $APPS/Stećak.app and $BIN_DIR/stecak"
    ;;
Linux)
    say "Building stecak (release)…"
    cargo build --release
    mkdir -p "$BIN_DIR" "$HOME/.local/share/applications" "$HOME/.local/share/icons/hicolor/1024x1024/apps"
    install -m 755 target/release/stecak "$BIN_DIR/stecak"
    install -m 644 assets/icon-1024.png "$HOME/.local/share/icons/hicolor/1024x1024/apps/stecak.png"
    sed "s|^Exec=.*|Exec=$BIN_DIR/stecak|" assets/stecak.desktop > "$HOME/.local/share/applications/stecak.desktop"
    say "Installed $BIN_DIR/stecak and a desktop entry"
    ;;
*)
    die "Unsupported OS for this script. On Windows run: cargo install --git $REPO"
    ;;
esac

case ":$PATH:" in
*":$BIN_DIR:"*) ;;
*) say "Add $BIN_DIR to your PATH to run \`stecak\` from a shell." ;;
esac
say "Done. Settings: Cmd+, (Ctrl+Shift+, on Linux)."
