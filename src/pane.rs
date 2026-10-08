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
use std::sync::atomic::{AtomicBool, Ordering};
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
    /// One step of a scripted UI session (LUMEN_DEMO), used for automated visual tests.
    Demo(String),
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
    pub title: Arc<Mutex<String>>,
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

        let mut cmd = if config.shell.program.is_empty() {
            CommandBuilder::new_default_prog()
        } else {
            let mut c = CommandBuilder::new(&config.shell.program);
            c.args(&config.shell.args);
            c
        };
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("TERM_PROGRAM", "lumen");
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
        let listener = Listener { writer: writer.clone(), title: title.clone(), size: window_size.clone() };
        let term_config = TermConfig { scrolling_history: config.scrollback, ..Default::default() };
        let last_size = (size.cols, size.rows);
        let term = Arc::new(Mutex::new(Term::new(term_config, &size, listener)));

        let term_reader = term.clone();
        let fresh = Arc::new(AtomicBool::new(true));
        let fresh_reader = fresh.clone();
        std::thread::Builder::new()
            .name(format!("pty-reader-{id}"))
            .stack_size(256 * 1024)
            .spawn(move || {
                let mut parser: Processor = Processor::new();
                let mut buf = vec![0u8; 1 << 16];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            parser.advance(&mut *term_reader.lock(), &buf[..n]);
                            fresh_reader.store(true, Ordering::Release);
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

        Ok(Self { term, fresh, title, writer, master: pair.master, window_size, child, last_size })
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
