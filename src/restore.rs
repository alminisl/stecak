//! Restore tabs on relaunch: the tab/split layout and each pane's folder are saved when the
//! window closes and reopened at the next launch. Panes running Claude Code or Codex come
//! back in the exact conversation they had open (Claude Code), or the latest one in that
//! folder (Codex), and drop to a shell when you quit the agent.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::layout::Dir;

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct Saved {
    pub active: usize,
    pub tabs: Vec<SavedTab>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct SavedTab {
    pub root: SavedNode,
    /// Index of the focused pane among the tab's leaves, left to right.
    pub focus: usize,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum SavedNode {
    Pane {
        cwd: Option<PathBuf>,
        /// Run instead of the shell, e.g. "claude --continue".
        command: Option<String>,
    },
    Split {
        /// true = side by side.
        horizontal: bool,
        ratio: f32,
        a: Box<SavedNode>,
        b: Box<SavedNode>,
    },
}

impl SavedNode {
    pub fn dir(horizontal: bool) -> Dir {
        if horizontal { Dir::Horizontal } else { Dir::Vertical }
    }
}

/// The command that brings a pane's agent back: its exact Claude Code session when known,
/// else the latest conversation in the folder; then a shell once you quit the agent.
pub fn resume_command(process: &str, claude_session: Option<&str>) -> Option<String> {
    let p = process.to_lowercase();
    let agent = if p.contains("claude") {
        match claude_session {
            Some(id) => format!("claude --resume {id}"),
            None => "claude --continue".into(),
        }
    } else if p.contains("codex") {
        "codex resume --last".into()
    } else {
        return None;
    };
    Some(format!("{agent}; exec \"$SHELL\" -l"))
}

/// Kept next to the config file (`~/.config/stecak/session.json`).
fn path(config_dir: &Path) -> PathBuf {
    config_dir.join("session.json")
}

pub fn load(config_dir: &Path) -> Option<Saved> {
    let text = std::fs::read_to_string(path(config_dir)).ok()?;
    serde_json::from_str(&text).map_err(|e| log::warn!("ignoring saved session: {e}")).ok()
}

/// Save the session; no tabs (you closed them all) removes it, so the next launch starts fresh.
pub fn save(saved: &Saved, config_dir: &Path) {
    let path = path(config_dir);
    if saved.tabs.is_empty() {
        let _ = std::fs::remove_file(&path);
        return;
    }
    let result = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|_| serde_json::to_string(saved).map_err(std::io::Error::other))
        .and_then(|text| std::fs::write(&path, text));
    if let Err(e) = result {
        log::warn!("could not save session: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_resumes_agents() {
        let s = Saved {
            active: 0,
            tabs: vec![SavedTab {
                root: SavedNode::Split {
                    horizontal: true,
                    ratio: 0.4,
                    a: Box::new(SavedNode::Pane { cwd: Some("/tmp".into()), command: None }),
                    b: Box::new(SavedNode::Pane { cwd: None, command: resume_command("claude", Some("abc-1")) }),
                },
                focus: 1,
            }],
        };
        let text = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<Saved>(&text).unwrap(), s);
        assert_eq!(resume_command("zsh", None), None);
        assert_eq!(resume_command("claude", Some("abc-1")).as_deref(), Some("claude --resume abc-1; exec \"$SHELL\" -l"));
        assert_eq!(resume_command("codex", None).as_deref(), Some("codex resume --last; exec \"$SHELL\" -l"));
    }
}
