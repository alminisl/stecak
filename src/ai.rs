//! AI helpers that reuse the agent CLI you already have (`claude -p`), so Stećak needs no API
//! key, HTTP client or TLS stack:
//! - "Ask AI" (⌘I): describe a task, get a shell command typed at your prompt (never run).
//! - "Explain last error" (⌘⇧E): the last command, its exit code and output go to the agent.
//! - Shell integration: zsh reports where each command starts and ends (OSC 133), which is
//!   how we know what the last command printed.

use std::path::{Path, PathBuf};

use winit::event_loop::EventLoopProxy;

use crate::pane::{PaneId, UserEvent};

/// The last finished command in a pane, as reported by shell integration.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CommandRecord {
    pub command: String,
    pub exit: i32,
    /// Raw output bytes (escape sequences included), tail only.
    pub output: Vec<u8>,
}

/// Raw output kept per command: enough for an error and its context, never the whole flood.
const OUTPUT_CAP: usize = 32 * 1024;

/// Follows OSC 133 marks in a pane's output stream: `C` = command output starts (with
/// `cmdline=`), `D;<exit>` = it finished. Runs on the PTY reader thread.
/// shortcut: a mark split across two reads is missed, like OSC 9 notifications.
#[derive(Default)]
pub struct CommandTracker {
    capturing: bool,
    current: CommandRecord,
}

impl CommandTracker {
    /// Feed a chunk of PTY output. Returns the command that finished in it, if any.
    pub fn feed(&mut self, buf: &[u8]) -> Option<CommandRecord> {
        let mut done = None;
        let mut i = 0;
        while let Some(pos) = find(&buf[i..], b"\x1b]133;") {
            let mark = i + pos;
            if self.capturing {
                self.push(&buf[i..mark]);
            }
            let body_start = mark + 6;
            let end = buf[body_start..].iter().position(|&b| b == 0x07 || b == 0x1b).map_or(buf.len(), |e| body_start + e);
            let body = String::from_utf8_lossy(&buf[body_start..end]);
            let mut parts = body.split(';');
            match parts.next() {
                Some("C") => {
                    self.capturing = true;
                    self.current = CommandRecord::default();
                    // The command line is last and may itself contain ';'.
                    if let Some(at) = body.find(";cmdline=") {
                        self.current.command = body[at + 9..].trim().to_string();
                    }
                }
                Some("D") if self.capturing => {
                    self.capturing = false;
                    self.current.exit = parts.next().and_then(|c| c.trim().parse().ok()).unwrap_or(0);
                    done = Some(std::mem::take(&mut self.current));
                }
                // A new prompt without D (e.g. Ctrl+C at the prompt): nothing finished.
                Some("A") => self.capturing = false,
                _ => {}
            }
            // Skip the terminator (BEL, or ESC \).
            i = if buf.get(end) == Some(&0x1b) { (end + 2).min(buf.len()) } else { (end + 1).min(buf.len()) };
        }
        if self.capturing {
            self.push(&buf[i..]);
        }
        done
    }

    fn push(&mut self, bytes: &[u8]) {
        let out = &mut self.current.output;
        out.extend_from_slice(bytes);
        if out.len() > OUTPUT_CAP {
            out.drain(..out.len() - OUTPUT_CAP);
        }
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Terminal output as plain text: escape sequences removed, `\r` progress-bar rewrites
/// collapsed to their final state, backspaces applied.
pub fn plain_text(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    let mut lines: Vec<String> = vec![String::new()];
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.next() {
                // CSI: parameters, then a final byte in @..~
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC / DCS / APC…: up to BEL or ST.
                Some(']' | 'P' | '_' | '^') => {
                    while let Some(c) = chars.next() {
                        if c == '\x07' || (c == '\x1b' && chars.peek() == Some(&'\\')) {
                            chars.next_if_eq(&'\\');
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\n' => lines.push(String::new()),
            '\r' if chars.peek() == Some(&'\n') => {}
            '\r' => lines.last_mut().unwrap().clear(),
            '\x08' => {
                lines.last_mut().unwrap().pop();
            }
            '\t' => lines.last_mut().unwrap().push('\t'),
            c if c.is_control() => {}
            c => lines.last_mut().unwrap().push(c),
        }
    }
    let lines: Vec<&str> = lines.iter().map(|l| l.trim_end()).collect();
    lines.join("\n").trim_matches('\n').to_string()
}

/// The last `max_lines` lines of `text`, and at most `max_chars` characters of them.
pub fn tail(text: &str, max_lines: usize, max_chars: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut t = lines[lines.len().saturating_sub(max_lines)..].join("\n");
    let n = t.chars().count();
    if n > max_chars {
        t = format!("…{}", t.chars().skip(n - max_chars).collect::<String>());
    }
    t
}

/// What "Explain last error" sends to the agent.
pub fn explain_prompt(record: Option<&CommandRecord>, screen: &str, cwd: Option<&Path>) -> String {
    let wd = cwd.map(|p| format!(" in {}", crate::browser::tilde(p))).unwrap_or_default();
    match record {
        Some(r) => {
            let out = tail(&plain_text(&r.output), 80, 6000);
            let status = if r.exit == 0 { "it exited with status 0".to_string() } else { format!("it failed with exit code {}", r.exit) };
            format!(
                "I ran this command in my terminal{wd} and {status}:\n```\n$ {}\n{out}\n```\nExplain briefly what went wrong and how to fix it.",
                r.command
            )
        }
        None => format!("Here's the end of my terminal{wd}:\n```\n{}\n```\nExplain briefly what went wrong in the last command and how to fix it.", tail(screen, 60, 6000)),
    }
}

/// What "Send selection to agent" pastes (not submitted, so you can add your question).
pub fn context_snippet(selection: &str, cwd: Option<&Path>) -> String {
    let wd = cwd.map(|p| format!(" (in {})", crate::browser::tilde(p))).unwrap_or_default();
    format!("From my terminal{wd}:\n```\n{}\n```\n", selection.trim_end())
}

fn command_prompt(task: &str, cwd: Option<&Path>) -> String {
    let shell = crate::shell::name();
    let os = if cfg!(target_os = "macos") { "macOS" } else { std::env::consts::OS };
    let wd = cwd.map(|p| format!("The working directory is {}. ", p.display())).unwrap_or_default();
    format!(
        "Reply with exactly one {shell} command for {os} that does what is asked below, and nothing else: \
         no explanation, no Markdown, no code fences. Chain steps with && or pipes if needed. \
         Do not run anything yourself. {wd}Task: {task}"
    )
}

/// Turn the model's reply into a single command line: drop code fences, prompts and blank
/// lines. Several lines are kept only if they can be pasted safely (bracketed paste).
pub fn clean_command(reply: &str, multiline_ok: bool) -> String {
    let lines: Vec<&str> = reply
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with("```"))
        .map(|l| l.strip_prefix("$ ").unwrap_or(l))
        .collect();
    let lines: Vec<&str> = lines.iter().map(|l| l.trim_matches('`')).collect();
    if multiline_ok { lines.join("\n") } else { lines.iter().map(|l| l.trim()).collect::<Vec<_>>().join(" && ") }
}

/// Run `command "<prompt>"` (e.g. `claude -p --model haiku`) on a worker thread, the way your
/// shell would (see `shell::background`). The reply arrives as `UserEvent::AiReply`.
pub fn ask_async(command: &str, task: &str, cwd: Option<PathBuf>, pane: PaneId, generation: u64, proxy: EventLoopProxy<UserEvent>) {
    let prompt = command_prompt(task, cwd.as_deref());
    let command = command.to_string();
    let _ = std::thread::Builder::new().name("ai-ask".into()).spawn(move || {
        let program = command.split_whitespace().next().unwrap_or("").to_string();
        let result = match crate::shell::background(&command, Some(&prompt)) {
            None => Err("Ask AI: `agent.ask_command` is empty".to_string()),
            Some(mut cmd) => {
                cmd.stdin(std::process::Stdio::null());
                if let Some(dir) = cwd.filter(|d| d.is_dir()) {
                    cmd.current_dir(dir);
                }
                match cmd.output() {
                    Ok(out) if out.status.success() => Ok(String::from_utf8_lossy(&out.stdout).into_owned()),
                    Ok(out) => {
                        let err = String::from_utf8_lossy(&out.stderr);
                        let line = err.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("no output");
                        Err(format!("`{program}` failed: {}", line.trim()))
                    }
                    Err(e) => Err(format!("`{program}`: {e}")),
                }
            }
        };
        let _ = proxy.send_event(UserEvent::AiReply(pane, generation, result));
    });
}

/// Windows: run a background command without popping up a console window (Stećak is a GUI
/// app there, so each console child would otherwise get its own window).
pub fn no_console(cmd: &mut std::process::Command) -> &mut std::process::Command {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

// ---- the user's PATH ---------------------------------------------------------------------

static USER_PATH: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

/// PATH as your interactive shell sets it. An app started from the Dock gets a bare PATH, and
/// a login shell (`zsh -lc`) skips ~/.zshrc, which is often where ~/.local/bin (claude) is
/// added. Commands we run ourselves (agents, Ask AI, /context) use this. Blocks until it's
/// resolved; `warm_user_path` starts that at launch.
pub fn user_path() -> Option<&'static str> {
    USER_PATH.get_or_init(resolve_user_path).as_deref()
}

pub fn warm_user_path() {
    let _ = std::thread::Builder::new().name("user-path".into()).spawn(|| {
        user_path();
    });
}

fn resolve_user_path() -> Option<String> {
    use std::io::Read;
    // Windows apps inherit the user's PATH from the registry; a Git Bash `$SHELL` would
    // only hand back a POSIX-style one.
    if cfg!(windows) {
        return None;
    }
    let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty())?;
    let mut child = no_console(&mut std::process::Command::new(shell))
        .args(["-ilc", "printf '\\n__STECAK_PATH__%s\\n' \"$PATH\""])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    // A slow or stuck ~/.zshrc must not hold anything up for long.
    let start = std::time::Instant::now();
    while child.try_wait().ok()?.is_none() {
        if start.elapsed() > std::time::Duration::from_secs(5) {
            let _ = child.kill();
            log::warn!("reading PATH from the shell timed out");
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    let path = out.lines().find_map(|l| l.strip_prefix("__STECAK_PATH__")).filter(|p| !p.is_empty()).map(str::to_string);
    log::debug!("shell PATH: {path:?}");
    path
}

// ---- shell integration -----------------------------------------------------------------

/// zsh integration, loaded through ZDOTDIR: restores your ZDOTDIR, sources your .zshenv,
/// then marks prompts (A), command output (C, with the command line) and exit status (D).
const ZSHENV: &str = r#"# Stećak shell integration for zsh (generated; changes are overwritten).
if [[ -n "${STECAK_ZSH_ZDOTDIR+x}" ]]; then
  ZDOTDIR="$STECAK_ZSH_ZDOTDIR"; unset STECAK_ZSH_ZDOTDIR
else
  unset ZDOTDIR
fi
[[ -f "${ZDOTDIR:-$HOME}/.zshenv" ]] && source "${ZDOTDIR:-$HOME}/.zshenv"
if [[ -o interactive && -z "$_STECAK_HOOKED" ]]; then
  _STECAK_HOOKED=1
  _stecak_precmd() {
    local s=$?
    [[ -n "$_stecak_ran" ]] && printf '\e]133;D;%s\a' "$s"
    _stecak_ran=
    printf '\e]133;A\a'
  }
  _stecak_preexec() {
    _stecak_ran=1
    printf '\e]133;C;cmdline=%s\a' "${1//[[:cntrl:]]/ }"
  }
  # First in line, so $? is still the command's status.
  precmd_functions=(_stecak_precmd $precmd_functions)
  preexec_functions+=(_stecak_preexec)
fi
"#;

/// Environment that loads shell integration into a new shell, if we have it for that shell.
/// PowerShell integration: wrap the prompt (yours, oh-my-posh's…) so each prompt reports the
/// current folder with OSC 9;9, as Windows Terminal does. PowerShell's `cd` doesn't change the
/// process's own folder, so without this new tabs and restored sessions can't follow it.
/// Single quotes only, so it survives Windows command-line quoting as one argument.
const PWSH_PROMPT: &str = "$global:__stecak_prompt = $function:prompt; \
function global:prompt { $l = $executionContext.SessionState.Path.CurrentLocation; \
$o = ''; if ($l.Provider.Name -eq 'FileSystem') { $o = [string][char]27 + ']9;9;' + [char]34 + $l.ProviderPath + [char]34 + [char]7 }; \
$o + (& $global:__stecak_prompt) }";

/// Extra arguments that turn on integration for `shell_program` (PowerShell), unless the
/// configured arguments already run a command of their own.
pub fn integration_args(shell_program: &str, args: &[String]) -> Vec<String> {
    let stem = crate::shell::program_stem(shell_program);
    let own_command = args.iter().any(|a| ["-c", "-command", "-file", "-f", "-encodedcommand", "-e"].contains(&a.to_ascii_lowercase().as_str()));
    if !matches!(stem.as_str(), "pwsh" | "powershell") || own_command {
        return Vec::new();
    }
    vec!["-NoExit".into(), "-Command".into(), PWSH_PROMPT.into()]
}

pub fn integration_env(shell_program: &str) -> Vec<(String, String)> {
    let name = Path::new(shell_program).file_name().and_then(|n| n.to_str()).unwrap_or_default();
    if name != "zsh" {
        return Vec::new();
    }
    let Some(dir) = dirs::cache_dir().map(|d| d.join("stecak").join("zsh")) else { return Vec::new() };
    let file = dir.join(".zshenv");
    if std::fs::read_to_string(&file).ok().as_deref() != Some(ZSHENV) {
        if let Err(e) = std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&file, ZSHENV)) {
            log::warn!("shell integration unavailable: {e}");
            return Vec::new();
        }
    }
    let mut env = vec![("ZDOTDIR".to_string(), dir.display().to_string())];
    if let Ok(orig) = std::env::var("ZDOTDIR") {
        env.push(("STECAK_ZSH_ZDOTDIR".into(), orig));
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_commands_across_reads() {
        let mut t = CommandTracker::default();
        assert!(t.feed(b"\x1b]133;A\x07$ \x1b]133;C;cmdline=cargo build; echo a\x07\x1b[31merror\x1b[0m: oops\r\n").is_none());
        let r = t.feed(b"more\r\n\x1b]133;D;101\x07\x1b]133;A\x07$ ").unwrap();
        assert_eq!((r.command.as_str(), r.exit), ("cargo build; echo a", 101));
        assert_eq!(plain_text(&r.output), "error: oops\nmore");
        // Ctrl+C at the prompt: A without D records nothing.
        assert!(t.feed(b"\x1b]133;A\x07").is_none());
    }

    #[test]
    fn plain_text_handles_progress_and_osc() {
        assert_eq!(plain_text(b"10%\r50%\r100%\r\ndone\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\"), "100%\ndonelink");
        assert_eq!(plain_text(b"ab\x08c"), "ac");
    }

    #[test]
    fn cleans_model_replies() {
        assert_eq!(clean_command("```bash\nls -la\n```\n", true), "ls -la");
        assert_eq!(clean_command("$ cd src\n$ ls", false), "cd src && ls");
        assert_eq!(clean_command("`du -sh *`", false), "du -sh *");
    }

    #[test]
    fn explain_prompt_has_command_and_tail() {
        let r = CommandRecord { command: "make".into(), exit: 2, output: b"line\nboom\n".to_vec() };
        let p = explain_prompt(Some(&r), "", None);
        assert!(p.contains("$ make\nline\nboom") && p.contains("exit code 2"));
        assert_eq!(tail("a\nb\nc", 2, 100), "b\nc");
        assert_eq!(tail("abcdef", 10, 3), "…def");
    }
}
