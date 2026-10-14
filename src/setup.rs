//! Agent tools Stećak has support for: offered once on first launch, and from the palette.
//! Installing types each tool's own installer into a new tab, so the user sees it and can
//! answer its questions (claude-mem's asks to sign in or pick a provider).

use std::path::Path;
use std::process::{Command, Stdio};

/// Asked once; the file next to the config remembers it.
const OFFERED: &str = "setup-offered";

/// (name, installer on Unix, on Windows). shortcut: the Windows lines assume PowerShell, the default shell there.
const TOOLS: &[(&str, &str, &str)] = &[
    (
        "claude-mem",
        "if command -v npx >/dev/null; then npx claude-mem install; else echo 'claude-mem needs Node.js 20+: https://nodejs.org'; fi",
        "if (Get-Command npx -ErrorAction SilentlyContinue) { npx claude-mem install } else { echo 'claude-mem needs Node.js 20+: https://nodejs.org' }",
    ),
    ("Herdr", "curl -fsSL https://herdr.dev/install.sh | sh", "irm https://herdr.dev/install.ps1 | iex"),
];

fn installed(name: &str) -> bool {
    match name {
        "claude-mem" => crate::mem::installed(),
        _ => {
            let mut cmd = Command::new("herdr");
            if let Some(path) = crate::ai::user_path().filter(|_| cfg!(unix)) {
                cmd.env("PATH", path);
            }
            crate::ai::no_console(cmd.arg("--version")).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok()
        }
    }
}

/// The tools not installed yet.
pub fn missing() -> Vec<&'static str> {
    TOOLS.iter().map(|t| t.0).filter(|n| !installed(n)).collect()
}

/// On first launch: the missing tools to offer (none if they were offered before).
pub fn offer(config_dir: &Path) -> Option<Vec<&'static str>> {
    if config_dir.join(OFFERED).exists() {
        return None;
    }
    Some(missing()).filter(|m| !m.is_empty())
}

pub fn mark_offered(config_dir: &Path) {
    let _ = std::fs::create_dir_all(config_dir);
    let _ = std::fs::write(config_dir.join(OFFERED), "");
}

/// One shell line installing `tools`, one after another.
pub fn install_line(tools: &[&str]) -> String {
    let lines = TOOLS.iter().filter(|t| tools.contains(&t.0)).map(|t| if cfg!(windows) { t.2 } else { t.1 });
    lines.collect::<Vec<_>>().join("; ") + "\r"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_line_runs_each_tool() {
        let line = install_line(&["Herdr", "claude-mem"]);
        assert!(line.contains("claude-mem install") && line.contains("herdr.dev/install") && line.ends_with('\r'));
        assert_eq!(install_line(&[]), "\r");
    }
}
