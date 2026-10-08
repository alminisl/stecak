//! Color resolution for terminal cells, plus built-in theme presets.

use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};

use crate::config::{hex_or, ColorConfig, Config};

pub struct Theme {
    pub palette: [[f32; 3]; 256],
    pub fg: [f32; 3],
    pub bg: [f32; 3],
    pub cursor: [f32; 3],
    pub tab_bar: [f32; 3],
    pub tab_active: [f32; 3],
    pub selection: [f32; 3],
    pub search_match: [f32; 3],
    pub search_current: [f32; 3],
}

impl Theme {
    pub fn from_config(cfg: &Config) -> Theme {
        let c = &cfg.colors;
        let defaults = ColorConfig::default();
        let mut palette = [[0.0; 3]; 256];
        for (i, slot) in palette.iter_mut().take(16).enumerate() {
            let fallback = hex_or(&defaults.palette[i], [0.5; 3]);
            *slot = c.palette.get(i).map(|s| hex_or(s, fallback)).unwrap_or(fallback);
        }
        // xterm 6x6x6 color cube + 24-step grayscale ramp.
        let steps = [0.0, 95.0, 135.0, 175.0, 215.0, 255.0];
        for i in 0..216 {
            palette[16 + i] = [steps[i / 36] / 255.0, steps[(i / 6) % 6] / 255.0, steps[i % 6] / 255.0];
        }
        for i in 0..24 {
            palette[232 + i] = [(8.0 + i as f32 * 10.0) / 255.0; 3];
        }
        let h = |s: &str, d: &str| hex_or(s, hex_or(d, [0.5; 3]));
        Theme {
            palette,
            fg: h(&c.foreground, &defaults.foreground),
            bg: h(&c.background, &defaults.background),
            cursor: h(&c.cursor, &defaults.cursor),
            tab_bar: h(&c.tab_bar, &defaults.tab_bar),
            tab_active: h(&c.tab_active, &defaults.tab_active),
            selection: h(&c.selection, &defaults.selection),
            search_match: h(&c.search_match, &defaults.search_match),
            search_current: h(&c.search_current, &defaults.search_current),
        }
    }

    pub fn resolve(&self, color: Color, overrides: &Colors) -> [f32; 3] {
        let rgb = |c: Rgb| [c.r as f32 / 255.0, c.g as f32 / 255.0, c.b as f32 / 255.0];
        match color {
            Color::Spec(c) => rgb(c),
            Color::Indexed(i) => overrides[i as usize].map(rgb).unwrap_or(self.palette[i as usize]),
            Color::Named(n) => {
                if let Some(c) = overrides[n] {
                    return rgb(c);
                }
                let idx = n as usize;
                match n {
                    NamedColor::Foreground | NamedColor::BrightForeground => self.fg,
                    NamedColor::Background => self.bg,
                    NamedColor::Cursor => self.cursor,
                    NamedColor::DimForeground => dim(self.fg),
                    _ if idx < 16 => self.palette[idx],
                    // Dim{Black..White} follow DimForeground in the enum ordering.
                    _ => dim(self.palette[(idx - NamedColor::DimBlack as usize) % 8]),
                }
            }
        }
    }
}

pub fn dim(c: [f32; 3]) -> [f32; 3] {
    [c[0] * 0.66, c[1] * 0.66, c[2] * 0.66]
}

pub fn rgba(c: [f32; 3], a: f32) -> [f32; 4] {
    [c[0], c[1], c[2], a]
}

/// Perceived brightness, used to pick readable text on highlighted cells.
pub fn luma(c: [f32; 3]) -> f32 {
    0.299 * c[0] + 0.587 * c[1] + 0.114 * c[2]
}

pub struct Preset {
    pub name: &'static str,
    /// fg, bg, cursor, tab_bar, tab_active, selection, then 16 ANSI colors.
    colors: [&'static str; 22],
}

pub const PRESETS: &[Preset] = &[
    Preset {
        name: "Catppuccin Mocha",
        colors: [
            "#cdd6f4", "#1e1e2e", "#f5e0dc", "#181825", "#313244", "#585b70", //
            "#45475a", "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#f5c2e7", "#94e2d5", "#bac2de", //
            "#585b70", "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#f5c2e7", "#94e2d5", "#a6adc8",
        ],
    },
    Preset {
        name: "Tokyo Night",
        colors: [
            "#c0caf5", "#1a1b26", "#c0caf5", "#16161e", "#292e42", "#33467c", //
            "#15161e", "#f7768e", "#9ece6a", "#e0af68", "#7aa2f7", "#bb9af7", "#7dcfff", "#a9b1d6", //
            "#414868", "#f7768e", "#9ece6a", "#e0af68", "#7aa2f7", "#bb9af7", "#7dcfff", "#c0caf5",
        ],
    },
    Preset {
        name: "Dracula",
        colors: [
            "#f8f8f2", "#282a36", "#f8f8f2", "#21222c", "#44475a", "#44475a", //
            "#21222c", "#ff5555", "#50fa7b", "#f1fa8c", "#bd93f9", "#ff79c6", "#8be9fd", "#f8f8f2", //
            "#6272a4", "#ff6e6e", "#69ff94", "#ffffa5", "#d6acff", "#ff92df", "#a4ffff", "#ffffff",
        ],
    },
    Preset {
        name: "Gruvbox Dark",
        colors: [
            "#ebdbb2", "#282828", "#ebdbb2", "#1d2021", "#3c3836", "#504945", //
            "#282828", "#cc241d", "#98971a", "#d79921", "#458588", "#b16286", "#689d6a", "#a89984", //
            "#928374", "#fb4934", "#b8bb26", "#fabd2f", "#83a598", "#d3869b", "#8ec07c", "#ebdbb2",
        ],
    },
    Preset {
        name: "Solarized Light",
        colors: [
            "#586e75", "#fdf6e3", "#586e75", "#eee8d5", "#fdf6e3", "#eee8d5", //
            "#073642", "#dc322f", "#859900", "#b58900", "#268bd2", "#d33682", "#2aa198", "#eee8d5", //
            "#002b36", "#cb4b16", "#586e75", "#657b83", "#839496", "#6c71c4", "#93a1a1", "#fdf6e3",
        ],
    },
];

impl Preset {
    pub fn apply(&self, c: &mut ColorConfig) {
        let s = |i: usize| self.colors[i].to_string();
        c.foreground = s(0);
        c.background = s(1);
        c.cursor = s(2);
        c.tab_bar = s(3);
        c.tab_active = s(4);
        c.selection = s(5);
        c.palette = (6..22).map(s).collect();
    }

    pub fn matches(&self, c: &ColorConfig) -> bool {
        c.background.eq_ignore_ascii_case(self.colors[1]) && c.foreground.eq_ignore_ascii_case(self.colors[0])
    }
}
