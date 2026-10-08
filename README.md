# Lumen — GPU terminal emulator (proof of concept)

A small, fast, cross-platform terminal: GPU rendering, tabs, ligatures, transparency,
and a hot-reloaded YAML/JSON config. ~2,100 lines of Rust.

```sh
cargo run --release                 # login shell
cargo run --release -- -e htop      # run a command instead
```

## Rust or C++? → Rust

| | Rust | C++ |
|---|---|---|
| Existing terminal building blocks | `alacritty_terminal` (VT parser + grid + scrollback, used by Zed/Lapce), `portable-pty` (incl. Windows ConPTY), `wgpu`, `winit`, `swash` | libvterm (C, smaller feature set); mostly hand-rolled per platform |
| Cross-platform GPU | `wgpu` → Metal / DX12 / Vulkan / GL from one shader | Write Metal + D3D + GL/Vulkan backends yourself, or pull in bgfx/Skia |
| Windows + macOS + Linux windowing incl. transparency | `winit` (one flag) | Qt (heavy) or GLFW/SDL + per-OS code |
| Memory safety in a parser fed by untrusted bytes | By default | Discipline + fuzzing |
| Build/packaging | `cargo build` everywhere | CMake + vcpkg/conan per OS |
| Prior art | Alacritty, WezTerm, Rio, Warp, Zed's terminal | Windows Terminal, Konsole, (Kitty is C) |

Ghostty is Zig with a C ABI core; its speed comes from architecture (GPU atlas,
dedicated I/O thread, SIMD parsing) rather than language. Rust gives you the same
performance ceiling plus a ready-made ecosystem, so for a solo/small-team terminal it's the
clear pick. C++ only wins if you want Qt widgets or must embed into an existing C++ app.

## Architecture

```
 PTY reader thread (per tab)                     UI thread (winit event loop, idle = 0% CPU)
 ┌──────────────────────────┐   Wakeup event    ┌─────────────────────────────────────────┐
 │ read 64 KB → VT parser →  │ ────────────────▶ │ request_redraw (coalesced per frame)    │
 │ Term grid (under a mutex) │                   │ lock grid → build runs → shape (cached) │
 └──────────────────────────┘                   │ → instanced quads → 1 draw call/frame   │
                                                └─────────────────────────────────────────┘
```

- **Parsing off the UI thread**: output is parsed as it arrives; rendering just snapshots the
  grid, so a flood of output never blocks input or redraws (vsync caps frame count).
- **Rendering**: a 1 MB R8 glyph atlas plus one instanced draw per frame. Box-drawing and
  block characters are drawn as rects so lines join seamlessly.
- **Text**: `swash` shapes runs of same-style cells with OpenType `calt`/`liga`, so
  ligatures work while glyphs stay on the cell grid. Shaped runs are cached by text.
- **Fonts**: memory-mapped, not copied. Fallback fonts (icons, CJK, symbols) are opened only
  the first time a character needs them, and the system font database is dropped after startup.

## Measurements (Apple M5, macOS, 100×30 window, release build)

| | Lumen | iTerm2 |
|---|---|---|
| `cat` 34 MB of colored, ligature-heavy text | **0.46 s** (avg of 3) | 4.96 s (avg of 3) |
| Memory, idle, 1 tab | **31–36 MB** | ~40 MB per window over its base (727 MB total with your existing sessions) |
| Memory after 1.2 M lines output | 36 MB (2k-line scrollback) / 55 MB (5k) / 65 MB (10k) | +41 MB for one extra window |
| Binary size | 6.0 MB | ~100 MB app bundle |

Where Lumen's memory goes:
- 19 MB is the two Retina-size swapchain buffers, which scale with window size.
- About 5 MB per 1,000 scrollback lines at 100 columns.
- A few MB of heap.

macOS's Metal driver briefly uses about 70 MB more in the first second after launch, then releases it.

## Configuration

`~/.config/lumen/config.yaml` (or `.yml` / `.json`; `%APPDATA%\lumen\` on Windows, or set
`LUMEN_CONFIG`). It's reloaded live on save. **Cmd+,** (Ctrl+Shift+, elsewhere) creates the
file and opens it. See [`config.example.yaml`](config.example.yaml).

## Shortcuts (Cmd on macOS, Ctrl+Shift on Windows/Linux)

| Key | Action |
|---|---|
| T / W | new / close tab |
| 1–9, [ / ], Ctrl+Tab | switch tab |
| V | paste (bracketed-paste aware) |
| = / - / 0 | font size bigger / smaller / reset |
| , | open settings |

Click a tab to switch, Alt+click to close, `+` for a new tab.

## Transparency per OS

| OS | Transparency | Blur |
|---|---|---|
| macOS | ✅ verified (Metal, post-multiplied alpha) | ✅ window-server blur (same approach as Ghostty) |
| Windows 10/11 | DX12 composition swapchain (DirectComposition) | Acrylic / blur via `window-vibrancy` |
| Linux Wayland | ✅ via compositor alpha | compositor-dependent (KDE, Hyprland rules) |
| Linux X11 | needs a compositing WM (picom, KWin, Mutter) | compositor rules |

**Only macOS has been run so far.** Windows and Linux compile paths exist but are untested.

## Not done yet (next steps)

- Mouse selection + copy, URL clicking, search
- Mouse reporting to apps (vim/tmux mouse mode)
- Color emoji (needs an RGBA atlas next to the R8 one)
- A GUI settings page. A config file plus hot reload is how Ghostty and Alacritty do it. A GUI
  could be a small `egui` panel drawn with the same `wgpu` device.
- Splits, and per-tab current working directory for new tabs
- Damage tracking (redraw only changed rows) and a more compact scrollback format
  (Ghostty-style pages) to push memory lower
