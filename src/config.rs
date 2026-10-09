//! Configuration: loaded from YAML or JSON, with sane defaults for every field.
//!
//! Search order (first hit wins):
//!   $STECAK_CONFIG
//!   $XDG_CONFIG_HOME/stecak/config.{yaml,yml,json}  (falls back to ~/.config)
//!   <platform config dir>/stecak/config.{yaml,yml,json}  (e.g. %APPDATA% on Windows)

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    pub font: FontConfig,
    pub window: WindowConfig,
    pub colors: ColorConfig,
    pub shell: ShellConfig,
    pub tabs: TabsConfig,
    pub agent: AgentConfig,
    pub background_image: BackgroundImageConfig,
    pub bosancica: BosancicaConfig,
    pub scrollback: usize,
    /// Treat macOS Option key as Alt (sends ESC-prefixed sequences).
    pub option_as_alt: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct FontConfig {
    /// Preferred families, tried in order; the first one installed is used.
    pub family: Vec<String>,
    /// Extra fonts tried for characters the main font lacks (icons, symbols, CJK…).
    /// Any installed "Nerd Font" is also picked up automatically.
    pub fallback: Vec<String>,
    pub size: f32,
    pub line_height: f32,
    /// Programming ligatures (`->`, `=>`, `!=`…) via OpenType `calt`/`liga`.
    pub ligatures: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct WindowConfig {
    /// 0.0 (fully transparent) .. 1.0 (opaque) for the terminal background.
    pub opacity: f32,
    /// Native background blur (macOS vibrancy / Windows acrylic). Read at startup.
    pub blur: bool,
    pub padding: f32,
    pub columns: u16,
    pub rows: u16,
    /// "mailbox" (low latency), "fifo" (vsync), "immediate".
    pub present_mode: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ColorConfig {
    pub foreground: String,
    pub background: String,
    pub cursor: String,
    pub tab_bar: String,
    pub tab_active: String,
    pub selection: String,
    pub search_match: String,
    pub search_current: String,
    /// 16 ANSI colors: black, red, green, yellow, blue, magenta, cyan, white, then the bright variants.
    pub palette: Vec<String>,
}

/// iTerm2-style background image. Layers, bottom to top: desktop → image (`opacity`)
/// → theme background (`tint`) → text. Lower both for a see-through window.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct BackgroundImageConfig {
    /// PNG/JPEG/WebP path; `~` is expanded. Empty = no image.
    pub path: String,
    /// How opaque the image is (0..1). Below 1 the desktop shows through it.
    pub opacity: f32,
    /// How strongly the theme background color is laid over the image (0..1).
    pub tint: f32,
    /// "cover" (fill, crop), "contain" (fit, letterbox), "stretch", or "center".
    pub fit: String,
}

impl Default for BackgroundImageConfig {
    fn default() -> Self {
        Self { path: String::new(), opacity: 1.0, tint: 0.6, fit: "cover".into() }
    }
}

impl BackgroundImageConfig {
    pub fn resolved_path(&self) -> Option<PathBuf> {
        let p = self.path.trim();
        if p.is_empty() {
            return None;
        }
        Some(match p.strip_prefix("~/") {
            Some(rest) => dirs::home_dir()?.join(rest),
            None => PathBuf::from(p),
        })
    }
}

/// "Bosančica mode": draw terminal text with a Bosančica font. Fonts like BoSanko2 map
/// ordinary Latin letters to Bosančica letterforms, so the text itself (and copy, search,
/// what the shell receives) stays Latin; only the rendering changes. The font is not
/// bundled; install one and name its family here.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct BosancicaConfig {
    pub enabled: bool,
    /// Installed family name, or a path to a .ttf/.otf file.
    pub font: String,
    /// Size multiplier on top of automatic cap-height matching.
    pub size: f32,
    /// Synthetic stroke thickening (0 = the font's own weight), for thin display fonts.
    pub weight: f32,
}

impl Default for BosancicaConfig {
    fn default() -> Self {
        Self { enabled: false, font: "BoSanko2".into(), size: 1.15, weight: 0.4 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct ShellConfig {
    /// Empty = the user's default shell ($SHELL, or PowerShell on Windows).
    pub program: String,
    pub args: Vec<String>,
}

/// AI coding agent launched by Cmd+Shift+A in a split (e.g. "claude" or "codex").
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AgentConfig {
    pub command: String,
    /// macOS notification when an agent rings the bell or notifies while you're elsewhere.
    pub notifications: bool,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self { command: "claude".into(), notifications: true }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct TabsConfig {
    /// Show the tab bar even when only one tab is open.
    pub always_show: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            font: FontConfig::default(),
            window: WindowConfig::default(),
            colors: ColorConfig::default(),
            shell: ShellConfig::default(),
            tabs: TabsConfig::default(),
            agent: AgentConfig::default(),
            background_image: BackgroundImageConfig::default(),
            bosancica: BosancicaConfig::default(),
            scrollback: 2_000,
            option_as_alt: true,
        }
    }
}

impl Default for FontConfig {
    fn default() -> Self {
        Self {
            family: [
                "JetBrains Mono",
                "JetBrainsMono Nerd Font Mono",
                "JetBrainsMono Nerd Font",
                "Fira Code",
                "Cascadia Code",
                "SF Mono",
                "Menlo",
                "Cascadia Mono",
                "Consolas",
                "DejaVu Sans Mono",
                "Liberation Mono",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            fallback: [
                "Symbols Nerd Font Mono",
                "Apple Symbols",
                "Zapf Dingbats",
                "Segoe UI Symbol",
                "Noto Sans Symbols 2",
                "DejaVu Sans",
                "PingFang SC",
                "Hiragino Sans",
                "Microsoft YaHei",
                "Noto Sans CJK SC",
                // Color emoji last so text-presentation symbols prefer the fonts above.
                "Apple Color Emoji",
                "Segoe UI Emoji",
                "Noto Color Emoji",
            ]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            size: 14.0,
            line_height: 1.2,
            ligatures: true,
        }
    }
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            opacity: 0.85,
            blur: true,
            padding: 8.0,
            columns: 100,
            rows: 30,
            present_mode: "mailbox".into(),
        }
    }
}

impl Default for TabsConfig {
    fn default() -> Self {
        Self { always_show: true }
    }
}

impl Default for ColorConfig {
    fn default() -> Self {
        // Catppuccin Mocha-ish.
        Self {
            foreground: "#cdd6f4".into(),
            background: "#1e1e2e".into(),
            cursor: "#f5e0dc".into(),
            tab_bar: "#181825".into(),
            tab_active: "#313244".into(),
            selection: "#585b70".into(),
            search_match: "#f9e2af".into(),
            search_current: "#fab387".into(),
            palette: [
                "#45475a", "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#f5c2e7", "#94e2d5",
                "#bac2de", "#585b70", "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#f5c2e7",
                "#94e2d5", "#a6adc8",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        }
    }
}

/// Parse "#rrggbb" into linear-ish 0..1 floats (sRGB values; the surface is non-sRGB).
pub fn parse_hex(s: &str) -> Option<[f32; 3]> {
    let s = s.trim().trim_start_matches('#');
    if s.len() != 6 {
        return None;
    }
    let v = u32::from_str_radix(s, 16).ok()?;
    Some([
        ((v >> 16) & 0xff) as f32 / 255.0,
        ((v >> 8) & 0xff) as f32 / 255.0,
        (v & 0xff) as f32 / 255.0,
    ])
}

pub fn hex_or(s: &str, fallback: [f32; 3]) -> [f32; 3] {
    parse_hex(s).unwrap_or(fallback)
}

fn candidate_dirs() -> Vec<PathBuf> {
    let mut dirs_out = Vec::new();
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        dirs_out.push(PathBuf::from(xdg).join("stecak"));
    }
    if let Some(home) = dirs::home_dir() {
        dirs_out.push(home.join(".config").join("stecak"));
    }
    if let Some(cfg) = dirs::config_dir() {
        dirs_out.push(cfg.join("stecak"));
    }
    dirs_out
}

/// Path of an existing config file, if any.
pub fn find_config_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("STECAK_CONFIG") {
        return Some(PathBuf::from(p));
    }
    for dir in candidate_dirs() {
        for name in ["config.yaml", "config.yml", "config.json"] {
            let p = dir.join(name);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// Where we create a config file when the user asks to open settings and none exists.
pub fn default_config_path() -> PathBuf {
    candidate_dirs()
        .into_iter()
        .next()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("config.yaml")
}

pub fn load_from(path: &Path) -> Result<Config, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let is_json = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("json"));
    if is_json {
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
    } else {
        serde_yaml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
    }
}

pub fn load() -> Config {
    match find_config_path() {
        Some(p) => load_from(&p).unwrap_or_else(|e| {
            log::error!("config error, using defaults: {e}");
            Config::default()
        }),
        None => Config::default(),
    }
}

/// Persist the config (used by the settings page). JSON files stay JSON.
pub fn save(cfg: &Config, path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let is_json = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("json"));
    let text = if is_json {
        serde_json::to_string_pretty(cfg).map_err(std::io::Error::other)?
    } else {
        format!("# Stećak configuration (hot-reloaded on save; also editable from the settings page, Cmd+,)\n{}", serde_yaml::to_string(cfg).map_err(std::io::Error::other)?)
    };
    std::fs::write(path, text)
}
