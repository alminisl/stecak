//! A pane = one shell process (PTY) + one terminal state machine (alacritty_terminal).
//! Tabs hold one or more panes arranged by a split layout.
//!
//! Threading model (same idea as Alacritty): a dedicated reader thread per pane reads PTY
//! output and feeds it straight into the VT parser under a lock, then pokes the UI thread
//! to redraw. The UI thread never blocks on I/O, and redraws are coalesced by winit.

use alacritty_terminal::event::{Event as TermEvent, EventListener, WindowSize};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::{Config as TermConfig, Term};
use alacritty_terminal::vte::ansi::Processor;
use parking_lot::Mutex;
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use winit::event_loop::EventLoopProxy;

use crate::config::Config;

pub type PaneId = u64;

#[derive(Debug)]
pub enum UserEvent {
    /// New PTY output was parsed; redraw.
    Wakeup,
    /// A pane's shell exited.
    Exited(PaneId),
    /// Config file changed on disk.
    ConfigChanged,
    /// A background image finished decoding (RGBA8, width, height).
    BackgroundImage(Option<(Vec<u8>, u32, u32)>),
    /// A pane rang the bell or sent a desktop notification (agents waiting for input).
    Notify(PaneId, Option<String>),
    /// A newer release exists.
    UpdateAvailable(crate::update::Release),
    /// A menu bar item was chosen (its id).
    Menu(String),
    /// A manual update check found nothing newer (None), or failed (Some(reason)).
    UpdateNone(Option<String>),
    /// An in-place install finished: the app bundle to relaunch, or why it failed.
    UpdateInstalled(Result<std::path::PathBuf, String>),
    /// One step of a scripted UI session (STECAK_DEMO), used for automated visual tests.
    Demo(String),
    /// "Ask AI" finished: (pane, request generation, command or error).
    AiReply(PaneId, u64, Result<String, String>),
    /// An agent started in a new split has settled: deliver the text waiting for it.
    AgentReady(PaneId),
    /// Press Enter in a pane (after pasting a prompt into an agent).
    Submit(PaneId),
    /// `/context` finished for a folder (None if it failed).
    ContextBreakdown(std::path::PathBuf, Option<crate::context::Breakdown>),
}

pub struct GridSize {
    pub cols: usize,
    pub rows: usize,
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

type Writer = Arc<Mutex<Box<dyn Write + Send>>>;

/// Receives events emitted by the terminal state machine (title changes, replies to queries…).
#[derive(Clone)]
pub struct Listener {
    writer: Writer,
    title: Arc<Mutex<String>>,
    size: Arc<Mutex<WindowSize>>,
    bell: Arc<AtomicBool>,
}

impl EventListener for Listener {
    fn send_event(&self, event: TermEvent) {
        match event {
            // Replies to device status / cursor position queries etc. Shells and TUIs need these.
            TermEvent::PtyWrite(s) => {
                let _ = self.writer.lock().write_all(s.as_bytes());
            }
            TermEvent::TextAreaSizeRequest(f) => {
                let s = f(*self.size.lock());
                let _ = self.writer.lock().write_all(s.as_bytes());
            }
            TermEvent::Title(t) => *self.title.lock() = t,
            TermEvent::Bell => self.bell.store(true, Ordering::Release),
            TermEvent::ResetTitle => self.title.lock().clear(),
            // OSC 52: programs (e.g. vim/tmux over ssh) copying to the system clipboard.
            TermEvent::ClipboardStore(_, text) => {
                if let Ok(mut c) = arboard::Clipboard::new() {
                    let _ = c.set_text(text);
                }
            }
            TermEvent::ClipboardLoad(_, format) => {
                if let Ok(text) = arboard::Clipboard::new().and_then(|mut c| c.get_text()) {
                    let _ = self.writer.lock().write_all(format(&text).as_bytes());
                }
            }
            _ => {}
        }
    }
}

pub struct Pane {
    pub term: Arc<Mutex<Term<Listener>>>,
    /// Set by the reader thread when output arrived since this pane was last drawn.
    pub fresh: Arc<AtomicBool>,
    /// Milliseconds since the Unix epoch when output last arrived (agent "working" detection).
    pub last_output: Arc<AtomicU64>,
    pub title: Arc<Mutex<String>>,
    /// The last command that finished, when the shell reports commands (shell integration).
    pub last_command: Arc<Mutex<Option<crate::ai::CommandRecord>>>,
    writer: Writer,
    master: Box<dyn MasterPty + Send>,
    window_size: Arc<Mutex<WindowSize>>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    /// Last size sent to the PTY, to avoid redundant resizes (which make shells redraw).
    last_size: (usize, usize),
}

impl Pane {
    pub fn spawn(
        id: PaneId,
        config: &Config,
        size: GridSize,
        cell: (u16, u16),
        cwd: Option<&Path>,
        command: Option<&str>,
        proxy: EventLoopProxy<UserEvent>,
        wakeup_pending: Arc<AtomicBool>,
    ) -> Result<Self, String> {
        let (cell_w, cell_h) = cell;
        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows: size.rows as u16,
                cols: size.cols as u16,
                pixel_width: size.cols as u16 * cell_w,
                pixel_height: size.rows as u16 * cell_h,
            })
            .map_err(|e| e.to_string())?;

        let (shell, shell_args) = crate::shell::interactive(config);
        let integration_shell = if command.is_some() || !config.shell_integration { "" } else { shell.as_str() };
        let mut cmd = if let Some(command) = command {
            // Through the shell, with the PATH your interactive shell sets up (npm,
            // ~/.local/bin…), which a login shell alone may not have.
            let (program, args) = crate::shell::pane_command(command, config);
            let mut c = CommandBuilder::new(program);
            c.args(&args);
            if let Some(path) = crate::ai::user_path() {
                c.env("PATH", path);
            }
            c
        } else if cfg!(windows) || !config.shell.program.is_empty() {
            let mut c = CommandBuilder::new(&shell);
            c.args(&shell_args);
            c
        } else {
            CommandBuilder::new_default_prog()
        };
        // Where a restored agent pane goes when you quit the agent (`shell::then_shell`).
        cmd.env("STECAK_SHELL", &shell);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("TERM_PROGRAM", "stecak");
        cmd.env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));
        for (k, v) in crate::ai::integration_env(integration_shell) {
            cmd.env(k, v);
        }
        match cwd.map(Path::to_path_buf).or_else(dirs::home_dir) {
            Some(dir) if dir.is_dir() => cmd.cwd(dir),
            _ => {}
        }

        let child = pair.slave.spawn_command(cmd).map_err(|e| e.to_string())?;
        drop(pair.slave);

        let writer: Writer = Arc::new(Mutex::new(pair.master.take_writer().map_err(|e| e.to_string())?));
        let mut reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;

        let title = Arc::new(Mutex::new(String::new()));
        let window_size = Arc::new(Mutex::new(WindowSize {
            num_lines: size.rows as u16,
            num_cols: size.cols as u16,
            cell_width: cell_w,
            cell_height: cell_h,
        }));
        let bell = Arc::new(AtomicBool::new(false));
        let listener = Listener { writer: writer.clone(), title: title.clone(), size: window_size.clone(), bell: bell.clone() };
        let term_config = TermConfig { scrolling_history: config.scrollback, ..Default::default() };
        let last_size = (size.cols, size.rows);
        let term = Arc::new(Mutex::new(Term::new(term_config, &size, listener)));

        let term_reader = term.clone();
        let fresh = Arc::new(AtomicBool::new(true));
        let fresh_reader = fresh.clone();
        let last_output = Arc::new(AtomicU64::new(0));
        let last_output_reader = last_output.clone();
        let last_command = Arc::new(Mutex::new(None));
        let last_command_reader = last_command.clone();
        std::thread::Builder::new()
            .name(format!("pty-reader-{id}"))
            .stack_size(256 * 1024)
            .spawn(move || {
                let mut parser: Processor = Processor::new();
                let mut tracker = crate::ai::CommandTracker::default();
                let mut buf = vec![0u8; 1 << 16];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            parser.advance(&mut *term_reader.lock(), &buf[..n]);
                            fresh_reader.store(true, Ordering::Release);
                            last_output_reader.store(now_ms(), Ordering::Release);
                            if let Some(record) = tracker.feed(&buf[..n]) {
                                *last_command_reader.lock() = Some(record);
                            }
                            for msg in crate::agents::notifications(&buf[..n]) {
                                let _ = proxy.send_event(UserEvent::Notify(id, Some(msg)));
                            }
                            if bell.swap(false, Ordering::AcqRel) {
                                let _ = proxy.send_event(UserEvent::Notify(id, None));
                            }
                            // Coalesce: at most one wakeup in flight until the next frame is
                            // drawn, so a flood of output can't starve redraws and input.
                            if !wakeup_pending.swap(true, Ordering::AcqRel) {
                                let _ = proxy.send_event(UserEvent::Wakeup);
                            }
                        }
                    }
                }
                let _ = proxy.send_event(UserEvent::Exited(id));
            })
            .map_err(|e| e.to_string())?;

        Ok(Self { term, fresh, last_output, title, last_command, writer, master: pair.master, window_size, child, last_size })
    }

    pub fn write(&self, bytes: &[u8]) {
        let mut w = self.writer.lock();
        let _ = w.write_all(bytes);
        let _ = w.flush();
    }

    pub fn resize(&mut self, size: GridSize, cell_w: u16, cell_h: u16) {
        if self.last_size == (size.cols, size.rows) {
            return;
        }
        self.last_size = (size.cols, size.rows);
        *self.window_size.lock() = WindowSize {
            num_lines: size.rows as u16,
            num_cols: size.cols as u16,
            cell_width: cell_w,
            cell_height: cell_h,
        };
        let _ = self.master.resize(PtySize {
            rows: size.rows as u16,
            cols: size.cols as u16,
            pixel_width: size.cols as u16 * cell_w,
            pixel_height: size.rows as u16 * cell_h,
        });
        self.term.lock().resize(size);
    }

    pub fn display_title(&self) -> String {
        let t = self.title.lock();
        if t.is_empty() { "shell".to_string() } else { t.clone() }
    }

    /// Show text in the pane as if the program had printed it (the welcome screen).
    pub fn inject(&self, bytes: &[u8]) {
        let mut parser: Processor = Processor::new();
        parser.advance(&mut *self.term.lock(), bytes);
        self.fresh.store(true, Ordering::Release);
    }

    /// Name of the pane's foreground process (e.g. "claude" while an agent runs).
    pub fn foreground_process(&self) -> Option<String> {
        #[cfg(unix)]
        return self.master.process_group_leader().and_then(crate::agents::process_name);
        #[cfg(windows)]
        return crate::agents::foreground(self.child.process_id()?).map(|p| p.1);
        #[cfg(not(any(unix, windows)))]
        None
    }

    /// Pid of the pane's foreground process (the agent, while one runs).
    pub fn foreground_pid(&self) -> Option<i32> {
        #[cfg(unix)]
        return self.master.process_group_leader();
        #[cfg(windows)]
        return crate::agents::foreground(self.child.process_id()?).map(|p| p.0 as i32);
        #[cfg(not(any(unix, windows)))]
        None
    }

    /// The shell's current working directory, so new tabs/splits can open there.
    pub fn cwd(&self) -> Option<PathBuf> {
        process_cwd(self.child.process_id()?)
    }
}

#[cfg(target_os = "macos")]
fn process_cwd(pid: u32) -> Option<PathBuf> {
    use std::ffi::CStr;
    let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
    // SAFETY: proc_pidinfo writes at most `size` bytes into `info`.
    let n = unsafe { libc::proc_pidinfo(pid as libc::c_int, libc::PROC_PIDVNODEPATHINFO, 0, (&mut info as *mut libc::proc_vnodepathinfo).cast(), size) };
    if n != size {
        return None;
    }
    // SAFETY: the kernel NUL-terminates vip_path.
    let path = unsafe { CStr::from_ptr(info.pvi_cdir.vip_path.as_ptr().cast()) };
    Some(PathBuf::from(path.to_str().ok()?))
}

#[cfg(target_os = "linux")]
fn process_cwd(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn process_cwd(_pid: u32) -> Option<PathBuf> {
    None
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}
