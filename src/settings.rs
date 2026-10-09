//! In-app settings page (Cmd+,). Drawn with the terminal's own renderer, so it costs
//! no extra UI toolkit, and every change is applied live and written back to the config.

use crate::config::Config;
use crate::theme::PRESETS;

#[derive(Clone, Copy, PartialEq)]
pub enum Item {
    Theme,
    Font,
    FontSize,
    LineHeight,
    Ligatures,
    Bosancica,
    Opacity,
    Blur,
    Padding,
    ImagePath,
    ImageOpacity,
    ImageTint,
    ImageFit,
    Scrollback,
    OptionAsAlt,
    AlwaysShowTabs,
    Welcome,
    RestoreSession,
    AgentNotifications,
    ContextBar,
    CheckUpdates,
    Shortcuts,
    OpenFile,
}

pub const ITEMS: &[Item] = &[
    Item::Theme,
    Item::Font,
    Item::FontSize,
    Item::LineHeight,
    Item::Ligatures,
    Item::Bosancica,
    Item::Opacity,
    Item::Blur,
    Item::Padding,
    Item::ImagePath,
    Item::ImageOpacity,
    Item::ImageTint,
    Item::ImageFit,
    Item::Scrollback,
    Item::OptionAsAlt,
    Item::AlwaysShowTabs,
    Item::Welcome,
    Item::RestoreSession,
    Item::AgentNotifications,
    Item::ContextBar,
    Item::CheckUpdates,
    Item::Shortcuts,
    Item::OpenFile,
];

const FITS: [&str; 4] = ["cover", "contain", "stretch", "center"];

#[derive(Default)]
pub struct Settings {
    pub open: bool,
    pub selected: usize,
    /// Some while the background-image path is being typed.
    pub editing: Option<String>,
    /// Whether the configured Bosančica font is installed (shown next to the toggle).
    pub bosancica_font_ok: bool,
    /// Installed monospace families the Font row cycles through.
    pub fonts: Vec<String>,
}

pub enum Outcome {
    None,
    Changed(Config),
    OpenFile,
    Shortcuts,
}

fn on_off(b: bool) -> String {
    if b { "on".into() } else { "off".into() }
}

impl Item {
    pub fn label(self) -> &'static str {
        match self {
            Item::Theme => "Theme",
            Item::Font => "Font",
            Item::FontSize => "Font size",
            Item::LineHeight => "Line height",
            Item::Ligatures => "Ligatures",
            Item::Bosancica => "Bosančica mode (⌘⇧B)",
            Item::Opacity => "Window opacity",
            Item::Blur => "Background blur (restart)",
            Item::Padding => "Padding",
            Item::ImagePath => "Background image",
            Item::ImageOpacity => "  Image opacity",
            Item::ImageTint => "  Color tint over image",
            Item::ImageFit => "  Image fit",
            Item::Scrollback => "Scrollback lines",
            Item::OptionAsAlt => "Option key as Alt",
            Item::AlwaysShowTabs => "Always show tab bar",
            Item::Welcome => "Welcome screen at launch",
            Item::RestoreSession => "Restore sessions at launch",
            Item::AgentNotifications => "Agent notifications",
            Item::ContextBar => "Claude context bar",
            Item::CheckUpdates => "Check for updates at launch",
            Item::Shortcuts => "Keyboard shortcuts…",
            Item::OpenFile => "Open config file…",
        }
    }

    pub fn value(self, c: &Config) -> String {
        match self {
            Item::Theme => PRESETS.iter().find(|p| p.matches(&c.colors)).map_or("Custom".into(), |p| p.name.into()),
            Item::Font => c.font.family.first().cloned().unwrap_or_default(),
            Item::FontSize => format!("{:.0}", c.font.size),
            Item::LineHeight => format!("{:.2}", c.font.line_height),
            Item::Ligatures => on_off(c.font.ligatures),
            Item::Bosancica => on_off(c.bosancica.enabled),
            Item::Opacity => format!("{:.0}%", c.window.opacity * 100.0),
            Item::Blur => on_off(c.window.blur),
            Item::Padding => format!("{:.0}", c.window.padding),
            Item::ImagePath => {
                if c.background_image.path.is_empty() { "none — ⏎ to type a path, or drop an image".into() } else { c.background_image.path.clone() }
            }
            Item::ImageOpacity => format!("{:.0}%", c.background_image.opacity * 100.0),
            Item::ImageTint => format!("{:.0}%", c.background_image.tint * 100.0),
            Item::ImageFit => c.background_image.fit.clone(),
            Item::Scrollback => format!("{}", c.scrollback),
            Item::OptionAsAlt => on_off(c.option_as_alt),
            Item::AlwaysShowTabs => on_off(c.tabs.always_show),
            Item::Welcome => on_off(c.welcome),
            Item::RestoreSession => on_off(c.restore_session),
            Item::AgentNotifications => on_off(c.agent.notifications),
            Item::ContextBar => on_off(c.agent.context_bar),
            Item::CheckUpdates => on_off(c.check_for_updates),
            Item::Shortcuts => if cfg!(target_os = "macos") { "⌘/".into() } else { "Ctrl+Shift+/".into() },
            Item::OpenFile => crate::config::find_config_path().map_or("creates ~/.config/stecak/config.yaml".into(), |p| crate::browser::tilde(&p)),
        }
    }

    /// Change the value by one step in `dir` (-1 or +1).
    fn adjust(self, c: &mut Config, dir: i32) -> bool {
        let d = dir as f32;
        match self {
            Item::Theme => {
                let cur = PRESETS.iter().position(|p| p.matches(&c.colors)).map_or(-1, |i| i as i32);
                let next = (cur + dir).rem_euclid(PRESETS.len() as i32) as usize;
                PRESETS[next].apply(&mut c.colors);
            }
            Item::FontSize => c.font.size = (c.font.size + d).clamp(6.0, 72.0),
            Item::LineHeight => c.font.line_height = ((c.font.line_height + 0.05 * d) * 100.0).round() / 100.0,
            Item::Ligatures => c.font.ligatures = !c.font.ligatures,
            Item::Bosancica => c.bosancica.enabled = !c.bosancica.enabled,
            Item::Opacity => c.window.opacity = ((c.window.opacity + 0.05 * d).clamp(0.0, 1.0) * 100.0).round() / 100.0,
            Item::Blur => c.window.blur = !c.window.blur,
            Item::Padding => c.window.padding = (c.window.padding + 2.0 * d).clamp(0.0, 64.0),
            Item::ImagePath => return false,
            Item::ImageOpacity => c.background_image.opacity = ((c.background_image.opacity + 0.05 * d).clamp(0.0, 1.0) * 100.0).round() / 100.0,
            Item::ImageTint => c.background_image.tint = ((c.background_image.tint + 0.05 * d).clamp(0.0, 1.0) * 100.0).round() / 100.0,
            Item::ImageFit => {
                let cur = FITS.iter().position(|f| *f == c.background_image.fit).unwrap_or(0) as i32;
                c.background_image.fit = FITS[(cur + dir).rem_euclid(FITS.len() as i32) as usize].into();
            }
            Item::Scrollback => c.scrollback = (c.scrollback as i64 + 1000 * dir as i64).clamp(0, 100_000) as usize,
            Item::OptionAsAlt => c.option_as_alt = !c.option_as_alt,
            Item::AlwaysShowTabs => c.tabs.always_show = !c.tabs.always_show,
            Item::Welcome => c.welcome = !c.welcome,
            Item::RestoreSession => c.restore_session = !c.restore_session,
            Item::AgentNotifications => c.agent.notifications = !c.agent.notifications,
            Item::ContextBar => c.agent.context_bar = !c.agent.context_bar,
            Item::CheckUpdates => c.check_for_updates = !c.check_for_updates,
            // Needs the installed font list: see Settings::cycle_font.
            Item::Font | Item::OpenFile | Item::Shortcuts => return false,
        }
        true
    }
}

pub enum Key<'a> {
    Up,
    Down,
    Left,
    Right,
    Enter,
    Escape,
    Backspace,
    Text(&'a str),
}

impl Settings {
    /// The font in use: the first configured family that's installed.
    fn current_font(&self, cfg: &Config) -> Option<usize> {
        cfg.font.family.iter().find_map(|f| self.fonts.iter().position(|n| n == f))
    }

    /// Move to the previous/next installed font; it goes first in `font.family`, and the
    /// rest of the list stays as fallback.
    fn cycle_font(&self, c: &mut Config, dir: i32) -> bool {
        if self.fonts.is_empty() {
            return false;
        }
        let cur = self.current_font(c).map_or(-1, |i| i as i32);
        let next = self.fonts[(cur + dir).rem_euclid(self.fonts.len() as i32) as usize].clone();
        c.font.family.retain(|f| *f != next);
        c.font.family.insert(0, next);
        true
    }

    pub fn toggle(&mut self) {
        self.open = !self.open;
        self.editing = None;
    }

    pub fn handle(&mut self, key: Key, cfg: &Config) -> Outcome {
        // Typing a background-image path.
        if let Some(buf) = self.editing.as_mut() {
            match key {
                Key::Text(t) => buf.push_str(t),
                Key::Backspace => {
                    buf.pop();
                }
                Key::Escape => self.editing = None,
                Key::Enter => {
                    let mut c = cfg.clone();
                    c.background_image.path = self.editing.take().unwrap_or_default().trim().to_string();
                    return Outcome::Changed(c);
                }
                _ => {}
            }
            return Outcome::None;
        }
        let item = ITEMS[self.selected];
        let mut c = cfg.clone();
        let changed = match key {
            Key::Up => {
                self.selected = (self.selected + ITEMS.len() - 1) % ITEMS.len();
                false
            }
            Key::Down => {
                self.selected = (self.selected + 1) % ITEMS.len();
                false
            }
            Key::Left | Key::Right | Key::Enter if item == Item::Font => self.cycle_font(&mut c, if matches!(key, Key::Left) { -1 } else { 1 }),
            Key::Left => item.adjust(&mut c, -1),
            Key::Right => item.adjust(&mut c, 1),
            Key::Enter => match item {
                Item::OpenFile => return Outcome::OpenFile,
                Item::Shortcuts => return Outcome::Shortcuts,
                Item::ImagePath => {
                    self.editing = Some(cfg.background_image.path.clone());
                    false
                }
                _ => item.adjust(&mut c, 1),
            },
            Key::Backspace if item == Item::ImagePath => {
                c.background_image.path.clear();
                true
            }
            Key::Escape => {
                self.open = false;
                false
            }
            _ => false,
        };
        if changed { Outcome::Changed(c) } else { Outcome::None }
    }

    /// Text shown for each row: (label, value, selected).
    pub fn rows<'a>(&'a self, cfg: &'a Config) -> impl Iterator<Item = (&'static str, String, bool)> + 'a {
        ITEMS.iter().enumerate().map(move |(i, item)| {
            let value = match (&self.editing, item) {
                (Some(buf), Item::ImagePath) => format!("{buf}▏"),
                (_, Item::Font) => self.current_font(cfg).map_or_else(|| item.value(cfg), |i| self.fonts[i].clone()),
                (_, Item::Bosancica) if !self.bosancica_font_ok => format!("{} — font \"{}\" not found", item.value(cfg), cfg.bosancica.font),
                _ => item.value(cfg),
            };
            (item.label(), value, i == self.selected)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adjust_and_theme_cycle() {
        let cfg = Config::default();
        let mut s = Settings { open: true, ..Default::default() };
        // Theme: default colors are the first preset; Right moves to the second.
        match s.handle(Key::Right, &cfg) {
            Outcome::Changed(c) => assert!(PRESETS[1].matches(&c.colors)),
            _ => panic!("theme should change"),
        }
        // Font: cycles installed families, keeping the old ones as fallback.
        s.fonts = vec!["Fira Code".into(), "JetBrains Mono".into(), "Menlo".into()];
        s.handle(Key::Down, &cfg);
        match s.handle(Key::Right, &cfg) {
            Outcome::Changed(c) => assert_eq!(c.font.family[..2], ["Menlo".to_string(), "JetBrains Mono".to_string()]),
            _ => panic!("font should change"),
        }
        s.handle(Key::Down, &cfg);
        match s.handle(Key::Right, &cfg) {
            Outcome::Changed(c) => assert_eq!(c.font.size, cfg.font.size + 1.0),
            _ => panic!("font size should change"),
        }
    }
}

/// Keyboard shortcut legend: (keys, action).
pub fn shortcuts() -> &'static [(&'static str, &'static str)] {
    if cfg!(target_os = "macos") {
        &[
            ("⌘T / ⌘W", "New tab / close pane"),
            ("⌘D / ⌘⇧D", "Split right / split down"),
            ("⌘] / ⌘[", "Next / previous pane"),
            ("⌘⇧] / ⌘⇧[  ⌘1…9", "Next / previous tab, go to tab"),
            ("⌘F  ⌘G / ⌘⇧G", "Find, next / previous match"),
            ("⌘C / ⌘V", "Copy / paste"),
            ("⌘K", "Clear scrollback"),
            ("⌘= / ⌘- / ⌘0", "Font bigger / smaller / reset"),
            ("⌘⇧A", "Open agent (claude) in a split"),
            ("⌘⇧S", "Session browser: resume Claude / Codex"),
            ("⌘I", "Ask AI for a command (typed, never run)"),
            ("⌘⇧E", "Explain the last error with the agent"),
            ("⌘⇧L", "Send selection (or last output) to agent"),
            ("⌘⇧P", "Command palette: actions, themes, fonts"),
            ("⇧⏎", "Newline in agent prompts"),
            ("⌘-click", "Open link, or file:line in your editor"),
            ("double / triple click", "Select word / line"),
            ("⌘⇧B", "Bosančica mode"),
            ("⌘,", "Settings (and the config file)"),
            ("⌘/", "This list"),
        ]
    } else {
        &[
            ("Ctrl+Shift+T / W", "New tab / close pane"),
            ("Ctrl+Shift+D / E", "Split right / split down"),
            ("Ctrl+Shift+] / [", "Next / previous pane"),
            ("Ctrl+Tab  Ctrl+Shift+1…9", "Next tab, go to tab"),
            ("Ctrl+Shift+F  G", "Find, next match"),
            ("Ctrl+Shift+C / V", "Copy / paste"),
            ("Ctrl+Shift+K", "Clear scrollback"),
            ("Ctrl+Shift+= / - / 0", "Font bigger / smaller / reset"),
            ("Ctrl+Shift+A", "Open agent (claude) in a split"),
            ("Ctrl+Shift+S", "Session browser: resume Claude / Codex"),
            ("Ctrl+Shift+I", "Ask AI for a command (typed, never run)"),
            ("Ctrl+Shift+X", "Explain the last error with the agent"),
            ("Ctrl+Shift+L", "Send selection (or last output) to agent"),
            ("Ctrl+Shift+P", "Command palette: actions, themes, fonts"),
            ("Shift+Enter", "Newline in agent prompts"),
            ("Ctrl+click", "Open link, or file:line in your editor"),
            ("double / triple click", "Select word / line"),
            ("Ctrl+Shift+B", "Bosančica mode"),
            ("Ctrl+Shift+,", "Settings (and the config file)"),
            ("Ctrl+Shift+/", "This list"),
        ]
    }
}
