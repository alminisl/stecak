//! Running commands the way the user's shell would, on every platform.
//!
//! Unix: through the login shell (`$SHELL -lc`), so PATH matches your terminal.
//! Windows: background helpers (Ask AI, /context, editors) start the program directly, found
//! on PATH like the shell would (`claude.exe`, `code.cmd`…), so no shell quoting is involved;
//! commands shown in a pane (agents, terminal editors, restored sessions) run in PowerShell.

#[cfg(windows)]
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::Config;

/// A background command line, plus an optional last argument passed verbatim (a prompt).
pub fn background(line: &str, extra: Option<&str>) -> Option<Command> {
    #[cfg(windows)]
    {
        let words = split_words(line);
        let (program, args) = words.split_first()?;
        let mut cmd = Command::new(which(program).unwrap_or_else(|| PathBuf::from(program)));
        cmd.args(args).args(extra);
        crate::ai::no_console(&mut cmd);
        Some(cmd)
    }
    #[cfg(not(windows))]
    {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let script = if extra.is_some() { format!("{line} \"$STECAK_ARG\"") } else { line.to_string() };
        let mut cmd = Command::new(shell);
        cmd.args(["-lc", &script]);
        if let Some(arg) = extra {
            cmd.env("STECAK_ARG", arg);
        }
        if let Some(path) = crate::ai::user_path() {
            cmd.env("PATH", path);
        }
        Some(cmd)
    }
}

/// The interactive shell a pane starts with: `shell.program`, else `$SHELL` (Unix) or
/// PowerShell (Windows; PowerShell 7 when installed).
pub fn interactive(config: &Config) -> (String, Vec<String>) {
    if !config.shell.program.is_empty() {
        return (config.shell.program.clone(), config.shell.args.clone());
    }
    #[cfg(windows)]
    return (powershell(), vec!["-NoLogo".into()]);
    #[cfg(not(windows))]
    (std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into()), vec!["-l".into()])
}

/// Program and arguments that run `line` inside a pane.
pub fn pane_command(line: &str, config: &Config) -> (String, Vec<String>) {
    #[cfg(windows)]
    {
        let _ = config;
        (powershell(), vec!["-NoLogo".into(), "-Command".into(), line.into()])
    }
    #[cfg(not(windows))]
    {
        let shell = if config.shell.program.is_empty() { std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into()) } else { config.shell.program.clone() };
        (shell, vec!["-lc".into(), line.into()])
    }
}

/// `line`, then the interactive shell once it exits (restored agent panes drop to a shell).
/// Windows: panes set `STECAK_SHELL` to the interactive shell (see pane.rs).
pub fn then_shell(line: &str) -> String {
    if cfg!(windows) { format!("{line}; & $env:STECAK_SHELL") } else { format!("{line}; exec \"$SHELL\" -l") }
}

/// Quote one argument for `pane_command` (sh on Unix, PowerShell on Windows).
pub fn quote(s: &str) -> String {
    if cfg!(windows) { format!("'{}'", s.replace('\'', "''")) } else { format!("'{}'", s.replace('\'', "'\\''")) }
}

/// What to call the shell in prompts ("Reply with one … command").
pub fn name() -> String {
    if cfg!(windows) {
        return "PowerShell".into();
    }
    std::env::var("SHELL").ok().and_then(|s| s.rsplit('/').next().map(str::to_string)).unwrap_or_else(|| "sh".into())
}

/// A program's file name without directory or `.exe`, e.g. "nvim" for `C:\…\nvim.exe`.
pub fn program_stem(program: &str) -> String {
    let name = program.rsplit(['/', '\\']).next().unwrap_or(program);
    let lower = name.to_ascii_lowercase();
    lower.strip_suffix(".exe").unwrap_or(&lower).to_string()
}

#[cfg(windows)]
fn powershell() -> String {
    static PS: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    PS.get_or_init(|| which("pwsh").map_or_else(|| "powershell.exe".into(), |p| p.display().to_string())).clone()
}

/// Find a program on PATH, trying PATHEXT extensions (.exe, .cmd…) like the shell does.
#[cfg(windows)]
pub fn which(program: &str) -> Option<PathBuf> {
    let p = Path::new(program);
    if p.components().count() > 1 {
        return p.is_file().then(|| p.to_path_buf());
    }
    let exts = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
    let exts: Vec<&str> = exts.split(';').filter(|e| !e.is_empty()).collect();
    let has_ext = p.extension().is_some_and(|e| exts.iter().any(|x| x.trim_start_matches('.').eq_ignore_ascii_case(&e.to_string_lossy())));
    for dir in std::env::split_paths(&std::env::var_os("PATH")?) {
        if has_ext && dir.join(program).is_file() {
            return Some(dir.join(program));
        }
        for ext in &exts {
            let candidate = dir.join(format!("{program}{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Split a command line into words: whitespace separates, '…' and "…" group (quotes removed).
#[cfg(any(windows, test))]
pub fn split_words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    for c in line.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                in_word = true;
            }
            (None, c) if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            (None, c) => {
                cur.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        words.push(cur);
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_words() {
        assert_eq!(split_words("claude -p --model haiku"), ["claude", "-p", "--model", "haiku"]);
        assert_eq!(split_words(r#"code -g "C:\My Files\a.rs:3" ''"#), ["code", "-g", r"C:\My Files\a.rs:3", ""]);
        assert_eq!(split_words(r#"--settings '{"a":true}'"#), ["--settings", r#"{"a":true}"#]);
        assert!(split_words("   ").is_empty());
    }

    #[test]
    fn program_stems() {
        assert_eq!(program_stem(r"C:\Tools\NVim.EXE"), "nvim");
        assert_eq!(program_stem("/usr/bin/hx"), "hx");
    }
}
