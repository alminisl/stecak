//! A tab = one shell process (PTY) + one terminal state machine (alacritty_terminal).
//!
//! Threading model (same idea as Alacritty): a dedicated reader thread per tab reads PTY
//! output and feeds it straight into the VT parser under a lock, then pokes the UI thread
//! to redraw. The UI thread never blocks on I/O, and redraws are coalesced by winit.

use alacritty_terminal::event::{Event as TermEvent, EventListener, WindowSize};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::{Config as TermConfig, Term};
use alacritty_terminal::vte::ansi::Processor;
use parking_lot::Mutex;
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use std::io::{Read, Write};
use std::sync::Arc;
use winit::event_loop::EventLoopProxy;

use crate::config::Config;

#[derive(Debug, Clone)]
pub enum UserEvent {
    /// New PTY output was parsed for this tab; redraw.
    Wakeup,
    /// The tab's shell exited.
    Exited(u64),
    /// Config file changed on disk.
    ConfigChanged,
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
            _ => {}
        }
    }
}

pub struct Tab {
    pub id: u64,
    pub term: Arc<Mutex<Term<Listener>>>,
    pub title: Arc<Mutex<String>>,
    writer: Writer,
    master: Box<dyn MasterPty + Send>,
    window_size: Arc<Mutex<WindowSize>>,
    _child: Box<dyn portable_pty::Child + Send + Sync>,
}

impl Tab {
    pub fn spawn(
        id: u64,
        config: &Config,
        size: GridSize,
        cell_w: u16,
        cell_h: u16,
        proxy: EventLoopProxy<UserEvent>,
    ) -> Result<Self, String> {
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
        if let Some(home) = dirs::home_dir() {
            cmd.cwd(home);
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
        let term = Arc::new(Mutex::new(Term::new(term_config, &size, listener)));

        let term_reader = term.clone();
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
                            let _ = proxy.send_event(UserEvent::Wakeup);
                        }
                    }
                }
                let _ = proxy.send_event(UserEvent::Exited(id));
            })
            .map_err(|e| e.to_string())?;

        Ok(Self { id, term, title, writer, master: pair.master, window_size, _child: child })
    }

    pub fn write(&self, bytes: &[u8]) {
        let mut w = self.writer.lock();
        let _ = w.write_all(bytes);
        let _ = w.flush();
    }

    pub fn resize(&self, size: GridSize, cell_w: u16, cell_h: u16) {
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
        if t.is_empty() { format!("Tab {}", self.id) } else { t.clone() }
    }
}
