//! Command palette (⌘⇧P): every action, theme and installed font in one searchable list.
//! Entries carry the same ids as the menu bar, so picking one runs the same code path.

use crate::theme::PRESETS;

#[derive(Clone)]
pub struct Entry {
    pub label: String,
    pub keys: &'static str,
    pub id: String,
}

#[derive(Default)]
pub struct Palette {
    pub open: bool,
    pub query: String,
    pub selected: usize,
    all: Vec<Entry>,
}

pub enum Key<'a> {
    Up,
    Down,
    Enter,
    Escape,
    Backspace,
    Text(&'a str),
}

/// (label, macOS keys, other keys, id)
const ACTIONS: &[(&str, &str, &str, &str)] = &[
    ("Ask AI for a command", "⌘I", "Ctrl+Shift+I", "ask-ai"),
    ("Explain last error with the agent", "⌘⇧E", "Ctrl+Shift+X", "explain-error"),
    ("Send selection to agent", "⌘⇧L", "Ctrl+Shift+L", "send-to-agent"),
    ("Open agent in split", "⌘⇧A", "Ctrl+Shift+A", "agent"),
    ("Sessions: resume Claude / Codex", "⌘⇧S", "Ctrl+Shift+S", "sessions"),
    ("New tab", "⌘T", "Ctrl+Shift+T", "new-tab"),
    ("Split right", "⌘D", "Ctrl+Shift+D", "split-right"),
    ("Split down", "⌘⇧D", "Ctrl+Shift+E", "split-down"),
    ("Close pane", "⌘W", "Ctrl+Shift+W", "close"),
    ("Next tab", "⌘⇧]", "Ctrl+Tab", "next-tab"),
    ("Previous tab", "⌘⇧[", "Ctrl+Shift+Tab", "prev-tab"),
    ("Next pane", "⌘]", "Ctrl+Shift+]", "next-pane"),
    ("Previous pane", "⌘[", "Ctrl+Shift+[", "prev-pane"),
    ("Find", "⌘F", "Ctrl+Shift+F", "find"),
    ("Copy", "⌘C", "Ctrl+Shift+C", "copy"),
    ("Paste", "⌘V", "Ctrl+Shift+V", "paste"),
    ("Clear scrollback", "⌘K", "Ctrl+Shift+K", "clear"),
    ("Font bigger", "⌘=", "Ctrl+Shift+=", "bigger"),
    ("Font smaller", "⌘-", "Ctrl+Shift+-", "smaller"),
    ("Font actual size", "⌘0", "Ctrl+Shift+0", "actual-size"),
    ("Bosančica mode", "⌘⇧B", "Ctrl+Shift+B", "bosancica"),
    ("Settings", "⌘,", "Ctrl+Shift+,", "settings"),
    ("Open config file", "", "", "open-config"),
    ("Keyboard shortcuts", "⌘/", "Ctrl+Shift+/", "shortcuts"),
    ("Check for updates", "", "", "check-updates"),
];

impl Palette {
    /// `fonts`: installed monospace families.
    pub fn open(&mut self, fonts: &[String]) {
        let mac = cfg!(target_os = "macos");
        let mut all: Vec<Entry> = ACTIONS
            .iter()
            .map(|(label, mk, ok, id)| Entry { label: label.to_string(), keys: if mac { mk } else { ok }, id: id.to_string() })
            .collect();
        all.extend(PRESETS.iter().enumerate().map(|(i, p)| Entry { label: format!("Theme: {}", p.name), keys: "", id: format!("theme:{i}") }));
        all.extend(fonts.iter().map(|f| Entry { label: format!("Font: {f}"), keys: "", id: format!("font:{f}") }));
        *self = Palette { open: true, all, ..Default::default() };
    }

    /// Entries matching the query, best first.
    pub fn visible(&self) -> Vec<&Entry> {
        let q = self.query.to_lowercase();
        let mut hits: Vec<(u32, usize, &Entry)> = self.all.iter().enumerate().filter_map(|(i, e)| score(&e.label.to_lowercase(), &q).map(|s| (s, i, e))).collect();
        hits.sort_by_key(|&(s, i, _)| (s, i));
        hits.into_iter().map(|(_, _, e)| e).collect()
    }

    /// Returns the id to run, if Enter picked one.
    pub fn handle(&mut self, key: Key) -> Option<String> {
        let n = self.visible().len();
        match key {
            Key::Up => self.selected = self.selected.saturating_sub(1),
            Key::Down => self.selected = (self.selected + 1).min(n.saturating_sub(1)),
            Key::Escape => self.open = false,
            Key::Backspace => {
                self.query.pop();
                self.selected = 0;
            }
            Key::Text(t) => {
                self.query.push_str(t);
                self.selected = 0;
            }
            Key::Enter => return self.pick(self.selected),
        }
        None
    }

    pub fn pick(&mut self, index: usize) -> Option<String> {
        let id = self.visible().get(index).map(|e| e.id.clone());
        if id.is_some() {
            self.open = false;
        }
        id
    }
}

/// Lower is better; None = no match. Every query word must appear in order as a
/// subsequence; whole-word prefixes beat substrings, which beat scattered letters.
fn score(label: &str, query: &str) -> Option<u32> {
    let mut total = 0;
    for word in query.split_whitespace() {
        total += if label.starts_with(word) || label.contains(&format!(" {word}")) {
            0
        } else if label.contains(word) {
            1
        } else if is_subsequence(word, label) {
            3
        } else {
            return None;
        };
    }
    Some(total)
}

fn is_subsequence(needle: &str, hay: &str) -> bool {
    let mut it = hay.chars();
    needle.chars().all(|c| it.any(|h| h == c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_ranking() {
        let mut p = Palette::default();
        p.open(&["JetBrains Mono".into()]);
        p.query = "ask".into();
        assert_eq!(p.visible()[0].id, "ask-ai");
        p.query = "font jet".into();
        assert_eq!(p.visible()[0].id, "font:JetBrains Mono");
        p.query = "splr".into();
        assert_eq!(p.visible()[0].id, "split-right");
        p.query = "zzzz".into();
        assert!(p.visible().is_empty());
    }
}
