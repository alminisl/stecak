//! Stećak: a lightweight GPU terminal emulator.
//!
//! - winit: cross-platform windowing (incl. transparent windows)
//! - wgpu: GPU rendering via Metal / DX12 / Vulkan
//! - alacritty_terminal: battle-tested VT parser + grid/scrollback/selection/search
//! - portable-pty: cross-platform PTY (ConPTY on Windows)
//! - swash: font shaping (ligatures) and rasterization

// Windows: a GUI app, so launching it doesn't also open a console window. Logs and panics go
// to `stecak.log` next to the config instead (see `init_logging`).
#![cfg_attr(windows, windows_subsystem = "windows")]

mod agents;
mod ai;
mod bgimage;
mod browser;
mod config;
mod context;
mod draw;
mod input;
mod layout;
#[cfg(target_os = "macos")]
mod menu;
mod mouse;
mod palette;
mod pane;
mod renderer;
mod restore;
mod search;
mod settings;
mod shell;
mod text;
mod theme;
mod update;
mod welcome;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use alacritty_terminal::grid::Scroll;
use alacritty_terminal::index::{Column, Direction, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::TermMode;
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, Ime, KeyEvent, MouseButton, MouseScrollDelta, StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{CursorIcon, UserAttentionType, Window, WindowId};

use config::{Config, ShellConfig};
use draw::{DrawStats, Highlights, PaneCache};
use input::Action;
use layout::{Dir, Divider, Node, Rect};
use pane::{GridSize, Pane, PaneId, UserEvent};
use renderer::Renderer;
use search::Search;
use settings::Settings;
use theme::{rgba, Theme};

/// The update popup's state.
#[derive(Clone)]
enum UpdateUi {
    Available(update::Release),
    UpToDate,
    Installing(String),
    Ready(PathBuf),
    Failed(String, String),
}

struct Tab {
    root: Node,
    focus: PaneId,
    /// Room is kept for the context bar (a Claude Code session was seen in this tab). Per
    /// tab, so switching tabs never resizes panes, which would make idle agents redraw.
    context_bar: bool,
}

/// The Claude Code session the context bar is showing.
struct ContextState {
    session: String,
    cwd: PathBuf,
    transcript: Option<PathBuf>,
    mtime: Option<SystemTime>,
    /// Tokens in context after the last turn, and its model.
    used: Option<(u64, String)>,
}

/// The "Ask AI" bar (⌘I) at the bottom of a pane.
#[derive(Default)]
struct Ask {
    open: bool,
    pane: PaneId,
    query: String,
    /// Waiting for the model; replies from older requests (lower generation) are ignored.
    busy: bool,
    generation: u64,
    error: Option<String>,
}

/// Mouse button state while dragging a selection, reporting to an app, or resizing a split.
#[derive(Clone)]
enum Drag {
    Select(PaneId),
    Report(PaneId),
    Divider(Divider),
}

/// Frame-time statistics, logged every few seconds with STECAK_STATS=1.
#[derive(Default)]
struct Stats {
    enabled: bool,
    since: Option<Instant>,
    frames: u32,
    skipped: u32,
    build: Duration,
    present: Duration,
    rows: DrawStats,
}

struct App {
    config: Config,
    cli_shell: Option<ShellConfig>,
    config_path: PathBuf,
    theme: Theme,
    theme_gen: u64,
    proxy: EventLoopProxy<UserEvent>,
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,

    panes: HashMap<PaneId, Pane>,
    caches: HashMap<PaneId, PaneCache>,
    tabs: Vec<Tab>,
    active: usize,
    next_id: PaneId,
    /// The first tab's shell, started on a thread while the GPU initializes (Windows takes
    /// ~250 ms to attach a process to a pseudo-console).
    prespawned: Option<std::thread::JoinHandle<Result<Pane, String>>>,

    modifiers: ModifiersState,
    mouse: (f32, f32),
    drag: Option<Drag>,
    last_click: Option<(Instant, PaneId, Point, u8)>,
    hover_url: Option<(PaneId, i32, usize, usize, mouse::Link)>,
    scroll_accum: f64,
    cursor_icon: CursorIcon,
    /// IME composition in progress (e.g. pinyin, or a dead key like ´ before e).
    preedit: String,
    focused: bool,
    occluded: bool,

    search: Search,
    search_gen: u64,
    settings: Settings,
    sessions: browser::Browser,
    /// Where the browser's rows were last drawn: (panel rect, first row y, row height, first index).
    sessions_layout: Option<(Rect, f32, f32, usize)>,
    palette: palette::Palette,
    palette_layout: Option<(Rect, f32, f32, usize)>,
    ask: Ask,
    /// Text waiting for an agent that's still starting in a new split: (text, press Enter).
    pending_agent: HashMap<PaneId, (String, bool)>,
    /// Panes that rang the bell / notified while you were looking elsewhere.
    attention: HashSet<PaneId>,
    last_notified: HashMap<PaneId, Instant>,
    /// A newer release to offer: (version, release page URL), and where its buttons were drawn.
    update: Option<UpdateUi>,
    update_buttons: Option<(Rect, Rect)>,
    /// Keyboard shortcut legend (⌘/) is showing.
    help_open: bool,
    /// Tab under the mouse (shows its close ×).
    hover_tab: Option<usize>,
    /// Context bar: the session shown, when it was last checked (and for which tab/pane/setting),
    /// the mouse is over it, and `/context` breakdowns per folder (None while loading).
    context: Option<ContextState>,
    context_checked: Option<(Instant, usize, Option<PaneId>, bool)>,
    context_hover: bool,
    breakdowns: HashMap<PathBuf, (Instant, Option<context::Breakdown>)>,
    /// Claude Code's own busy/idle state per pane, re-read at most twice a second.
    claude_busy: HashMap<PaneId, (Instant, Option<bool>)>,
    /// The macOS menu bar; must stay alive for the app's lifetime.
    #[cfg(target_os = "macos")]
    _menu: Option<muda::Menu>,
    /// The previous frame was a redraw forced by a glyph-atlas reset.
    atlas_retry: bool,
    /// An agent is working in some tab: keep redrawing the tab animation.
    animating: bool,
    bg_gen: Arc<AtomicU64>,
    /// Set by PTY reader threads when they've sent a wakeup that hasn't been drawn yet.
    wakeup_pending: Arc<AtomicBool>,

    /// Something other than terminal output changed (input, layout, overlay…): must present.
    dirty: bool,
    stats: Stats,
}

/// Grid placement of one pane: its rect, grid origin, and size in cells.
struct PaneGeom {
    id: PaneId,
    rect: Rect,
    gx: f32,
    gy: f32,
    cols: usize,
    rows: usize,
}

impl App {
    fn new(config: Config, cli_shell: Option<ShellConfig>, config_path: PathBuf, proxy: EventLoopProxy<UserEvent>) -> Self {
        Self {
            theme: Theme::from_config(&config),
            theme_gen: 0,
            config,
            cli_shell,
            config_path,
            proxy,
            window: None,
            renderer: None,
            panes: HashMap::new(),
            caches: HashMap::new(),
            tabs: Vec::new(),
            active: 0,
            next_id: 1,
            prespawned: None,
            modifiers: ModifiersState::empty(),
            mouse: (0.0, 0.0),
            drag: None,
            last_click: None,
            hover_url: None,
            scroll_accum: 0.0,
            cursor_icon: CursorIcon::Text,
            preedit: String::new(),
            focused: true,
            occluded: false,
            search: Search::default(),
            search_gen: 0,
            settings: Settings::default(),
            sessions: browser::Browser::default(),
            sessions_layout: None,
            palette: palette::Palette::default(),
            palette_layout: None,
            ask: Ask::default(),
            pending_agent: HashMap::new(),
            attention: HashSet::new(),
            last_notified: HashMap::new(),
            update: None,
            update_buttons: None,
            help_open: false,
            atlas_retry: false,
            #[cfg(target_os = "macos")]
            _menu: None,
            hover_tab: None,
            context: None,
            context_checked: None,
            context_hover: false,
            breakdowns: HashMap::new(),
            claude_busy: HashMap::new(),
            animating: false,
            bg_gen: Arc::new(AtomicU64::new(0)),
            wakeup_pending: Arc::new(AtomicBool::new(false)),
            dirty: true,
            stats: Stats { enabled: std::env::var_os("STECAK_STATS").is_some(), ..Default::default() },
        }
    }

    fn r(&self) -> &Renderer {
        self.renderer.as_ref().unwrap()
    }

    /// Height of the macOS titlebar we paint ourselves (the window is transparent, so the
    /// native titlebar would show whatever is behind it). None in fullscreen or elsewhere.
    fn titlebar_h(&self) -> f32 {
        let fullscreen = self.window.as_ref().is_some_and(|w| w.fullscreen().is_some());
        if cfg!(target_os = "macos") && !fullscreen { (28.0 * self.r().scale).round() } else { 0.0 }
    }

    /// Everything above the panes: our titlebar band plus the tab strip.
    fn tab_bar_h(&self) -> f32 {
        let show = self.config.tabs.always_show || self.tabs.len() > 1;
        self.titlebar_h() + if show { (self.r().cell().1 * 1.7).round() } else { 0.0 }
    }

    /// The context bar under tab `tab`'s panes, while it has a Claude Code session.
    fn context_bar_h(&self, tab: usize) -> f32 {
        if self.tabs.get(tab).is_some_and(|t| t.context_bar) { (self.r().cell().1 * 1.5).round() } else { 0.0 }
    }

    /// Geometry of every pane in tab `tab`.
    fn geometry(&self, tab: usize) -> (Vec<PaneGeom>, Vec<Divider>) {
        let Some(t) = self.tabs.get(tab) else { return (vec![], vec![]) };
        let r = self.r();
        let (w, h) = r.size();
        let (cw, ch) = r.cell();
        let top = self.tab_bar_h();
        let pad = (self.config.window.padding * r.scale).round();
        let gap = (layout::DIVIDER * r.scale).round().max(1.0);
        let (mut rects, mut dividers) = (vec![], vec![]);
        let bottom = self.context_bar_h(tab);
        t.root.layout(Rect { x: 0.0, y: top, w, h: h - top - bottom }, gap, &mut rects, &mut dividers);
        let geoms = rects
            .into_iter()
            .map(|(id, rect)| {
                let cols = (((rect.w - 2.0 * pad) / cw).floor() as usize).max(2);
                let rows = (((rect.h - 2.0 * pad) / ch).floor() as usize).max(1);
                PaneGeom { id, rect, gx: rect.x + pad, gy: rect.y + pad, cols, rows }
            })
            .collect();
        (geoms, dividers)
    }

    fn cell_px(&self) -> (u16, u16) {
        let (cw, ch) = self.r().cell();
        (cw as u16, ch as u16)
    }

    fn shell_config(&self) -> Config {
        let mut cfg = self.config.clone();
        if let Some(shell) = &self.cli_shell {
            cfg.shell = shell.clone();
        }
        cfg
    }

    /// Spawn a pane running the shell, or `command` through the login shell. Starts in `cwd`,
    /// else the focused pane's directory.
    fn spawn_pane(&mut self, command: Option<&str>, cwd: Option<PathBuf>) -> Option<PaneId> {
        let id = self.next_id;
        self.next_id += 1;
        let cwd = cwd.or_else(|| self.focused_pane().and_then(|p| p.cwd()));
        let size = GridSize { cols: 80, rows: 24 }; // corrected by resize_all() right after
        let prespawned = self.prespawned.take().filter(|_| command.is_none() && cwd.is_none());
        let result = match prespawned {
            Some(handle) => handle.join().unwrap_or_else(|_| Err("shell spawn thread panicked".into())),
            None => Pane::spawn(id, &self.shell_config(), size, self.cell_px(), cwd.as_deref(), command, self.proxy.clone(), self.wakeup_pending.clone()),
        };
        match result {
            Ok(p) => {
                self.panes.insert(id, p);
                Some(id)
            }
            Err(e) => {
                log::error!("failed to spawn shell: {e}");
                None
            }
        }
    }

    fn new_tab(&mut self) {
        self.open_tab(None, None);
    }

    fn open_tab(&mut self, command: Option<&str>, cwd: Option<PathBuf>) {
        if let Some(id) = self.spawn_pane(command, cwd) {
            self.tabs.push(Tab { root: Node::Leaf(id), focus: id, context_bar: false });
            self.active = self.tabs.len() - 1;
            self.resize_all();
        }
    }

    fn split(&mut self, dir: Dir, command: Option<&str>) {
        let Some(focus) = self.tabs.get(self.active).map(|t| t.focus) else { return };
        if let Some(id) = self.spawn_pane(command, None) {
            let tab = &mut self.tabs[self.active];
            tab.root.split(focus, id, dir);
            tab.focus = id;
            self.resize_all();
        }
    }

    fn close_pane(&mut self, id: PaneId, event_loop: &ActiveEventLoop) {
        let Some(ti) = self.tabs.iter().position(|t| {
            let mut ids = vec![];
            t.root.leaves(&mut ids);
            ids.contains(&id)
        }) else {
            return;
        };
        self.panes.remove(&id);
        self.caches.remove(&id);
        if self.search.pane == id {
            self.search.close();
        }
        let tab = &mut self.tabs[ti];
        if tab.root.remove(id) {
            if tab.focus == id {
                tab.focus = tab.root.first_leaf();
            }
        } else {
            self.tabs.remove(ti);
            if self.tabs.is_empty() {
                event_loop.exit();
                return;
            }
            if self.active >= ti && self.active > 0 {
                self.active -= 1;
            }
        }
        self.resize_all();
    }

    fn resize_all(&mut self) {
        let (cw, ch) = self.cell_px();
        for ti in 0..self.tabs.len() {
            let (geoms, _) = self.geometry(ti);
            for g in geoms {
                if let Some(p) = self.panes.get_mut(&g.id) {
                    p.resize(GridSize { cols: g.cols, rows: g.rows }, cw, ch);
                }
            }
        }
        self.mark_dirty();
    }

    fn mark_dirty(&mut self) {
        self.dirty = true;
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    fn focused_id(&self) -> Option<PaneId> {
        self.tabs.get(self.active).map(|t| t.focus)
    }

    fn focused_pane(&self) -> Option<&Pane> {
        self.focused_id().and_then(|id| self.panes.get(&id))
    }

    fn cycle_pane(&mut self, delta: i32) {
        let Some(tab) = self.tabs.get_mut(self.active) else { return };
        let mut ids = vec![];
        tab.root.leaves(&mut ids);
        if let Some(i) = ids.iter().position(|&id| id == tab.focus) {
            tab.focus = ids[(i as i32 + delta).rem_euclid(ids.len() as i32) as usize];
        }
        self.mark_dirty();
    }

    fn apply_config(&mut self, new: Config) {
        // Turning session restore off forgets the saved one, so switching it back on later
        // doesn't bring back an old session.
        if self.config.restore_session && !new.restore_session && self.owns_session() {
            restore::save(&restore::Saved { active: 0, tabs: Vec::new() }, self.config_dir());
        }
        let font_changed = new.font != self.config.font || new.bosancica.font != self.config.bosancica.font;
        let image_changed = new.background_image.path != self.config.background_image.path || new.background_image.fit != self.config.background_image.fit;
        self.config = new;
        self.theme = Theme::from_config(&self.config);
        self.theme_gen += 1;
        if font_changed {
            let scale = self.window.as_ref().unwrap().scale_factor() as f32;
            self.renderer.as_mut().unwrap().reload_fonts(&self.config, scale);
        }
        if image_changed {
            self.load_background();
        }
        self.resize_all();
    }

    /// Apply a config change made in the app (settings page / font zoom) and persist it.
    fn apply_and_save(&mut self, new: Config) {
        if let Err(e) = config::save(&new, &self.config_path) {
            log::error!("could not save config: {e}");
        }
        self.apply_config(new);
    }

    fn load_background(&mut self) {
        match self.config.background_image.resolved_path() {
            Some(path) => {
                let (w, h) = self.r().size();
                bgimage::load_async(path, self.config.background_image.fit.clone(), (w as u32, h as u32), self.bg_gen.clone(), self.proxy.clone());
            }
            None => {
                self.renderer.as_mut().unwrap().set_background_image(None);
                self.mark_dirty();
            }
        }
    }

    fn open_config_file(&self) {
        if !self.config_path.exists() {
            if let Err(e) = config::save(&self.config, &self.config_path) {
                log::error!("could not create config: {e}");
                return;
            }
        }
        let path = self.config_path.as_os_str();
        let result = if cfg!(target_os = "macos") {
            std::process::Command::new("open").arg("-t").arg(path).spawn()
        } else if cfg!(target_os = "windows") {
            std::process::Command::new("notepad").arg(path).spawn()
        } else {
            std::process::Command::new("xdg-open").arg(path).spawn()
        };
        if let Err(e) = result {
            log::error!("could not open settings: {e}");
        }
    }

    fn copy(&self) {
        let Some(pane) = self.focused_pane() else { return };
        if let Some(text) = pane.term.lock().selection_to_string() {
            if let Ok(mut c) = arboard::Clipboard::new() {
                let _ = c.set_text(text);
            }
        }
    }

    fn paste_text(&self, text: &str) {
        if let Some(pane) = self.focused_pane() {
            paste_into(pane, text);
        }
    }

    // ---- AI ----------------------------------------------------------------------------

    fn is_agent_pane(&self, id: PaneId) -> bool {
        self.panes.get(&id).and_then(|p| p.foreground_process()).is_some_and(|n| agents::is_agent(&n))
    }

    /// The pane in the active tab running an agent (the focused one first), if any.
    fn agent_pane_in_tab(&self) -> Option<PaneId> {
        let t = self.tabs.get(self.active)?;
        let mut ids = vec![];
        t.root.leaves(&mut ids);
        ids.sort_by_key(|&id| id != t.focus);
        ids.into_iter().find(|&id| self.is_agent_pane(id))
    }

    /// The Claude Code pane in the active tab (the focused one first): (session id, folder).
    fn claude_in_tab(&self) -> Option<(String, PathBuf)> {
        let t = self.tabs.get(self.active)?;
        let mut ids = vec![];
        t.root.leaves(&mut ids);
        ids.sort_by_key(|&id| id != t.focus);
        ids.into_iter().find_map(|id| {
            let p = self.panes.get(&id)?;
            if !p.foreground_process()?.to_lowercase().contains("claude") {
                return None;
            }
            let (session, cwd) = agents::claude_session(p.foreground_pid()?)?;
            let cwd = if cwd.as_os_str().is_empty() { p.cwd()? } else { cwd };
            Some((session, cwd))
        })
    }

    /// Refresh the context bar: at most once a second (or right away when the tab, focus or
    /// setting changed), re-reading the transcript only when it changed on disk.
    fn poll_context(&mut self) {
        let enabled = self.config.agent.context_bar;
        let key = (self.active, self.focused_id(), enabled);
        if self.context_checked.is_some_and(|(t, a, f, e)| (a, f, e) == key && t.elapsed() < Duration::from_secs(1)) {
            return;
        }
        self.context_checked = Some((Instant::now(), key.0, key.1, key.2));
        match enabled.then(|| self.claude_in_tab()).flatten() {
            None => self.context = None,
            Some((session, cwd)) => {
                if self.context.as_ref().is_none_or(|c| c.session != session) {
                    self.context = Some(ContextState { session, cwd: cwd.clone(), transcript: None, mtime: None, used: None });
                    self.dirty = true;
                }
                let c = self.context.as_mut().unwrap();
                if c.transcript.is_none() {
                    c.transcript = context::transcript_path(&c.session);
                }
                let mtime = c.transcript.as_ref().and_then(|p| p.metadata().ok()?.modified().ok());
                if mtime != c.mtime {
                    c.mtime = mtime;
                    c.used = c.transcript.as_deref().and_then(context::last_usage);
                    self.dirty = true;
                }
                // The breakdown barely changes within a session: refresh it every 10 minutes.
                if self.breakdowns.get(&cwd).is_none_or(|b| b.0.elapsed() > Duration::from_secs(600)) {
                    let old = self.breakdowns.remove(&cwd).and_then(|b| b.1);
                    self.breakdowns.insert(cwd.clone(), (Instant::now(), old));
                    context::fetch_async(cwd, self.proxy.clone());
                }
            }
        }
        let want = self.context.is_some();
        if let Some(t) = self.tabs.get_mut(self.active).filter(|t| t.context_bar != want) {
            t.context_bar = want;
            self.resize_all();
        }
    }

    /// Paste `text` into the tab's agent (opening one in a split if there's none), and press
    /// Enter if `submit`.
    fn send_to_agent(&mut self, text: String, submit: bool) {
        if let Some(id) = self.agent_pane_in_tab() {
            self.tabs[self.active].focus = id;
            self.deliver(id, &text, submit);
        } else {
            let before = self.focused_id();
            let cmd = self.config.agent.command.clone();
            self.split(Dir::Horizontal, Some(&cmd));
            let Some(id) = self.focused_id().filter(|&id| Some(id) != before) else { return };
            self.pending_agent.insert(id, (text, submit));
            if let Some(p) = self.panes.get(&id) {
                wait_until_settled(id, p.last_output.clone(), self.proxy.clone());
            }
        }
        self.mark_dirty();
    }

    fn deliver(&mut self, id: PaneId, text: &str, submit: bool) {
        let Some(p) = self.panes.get(&id) else { return };
        paste_into(p, text);
        if submit {
            // A separate keystroke a moment later, so the agent sees a paste and then Enter.
            let proxy = self.proxy.clone();
            let _ = std::thread::Builder::new().stack_size(64 * 1024).spawn(move || {
                std::thread::sleep(Duration::from_millis(150));
                let _ = proxy.send_event(UserEvent::Submit(id));
            });
        }
    }

    /// The pane whose last command to explain: the focused one, unless that's the agent.
    fn shell_pane_in_tab(&self) -> Option<PaneId> {
        let t = self.tabs.get(self.active)?;
        if !self.is_agent_pane(t.focus) {
            return Some(t.focus);
        }
        let mut ids = vec![];
        t.root.leaves(&mut ids);
        ids.into_iter().find(|&id| !self.is_agent_pane(id))
    }

    fn explain_last_error(&mut self) {
        let Some(p) = self.shell_pane_in_tab().and_then(|id| self.panes.get(&id)) else { return };
        let record = p.last_command.lock().clone();
        let screen = if record.is_none() { mouse::screen_text(&p.term.lock()) } else { String::new() };
        let prompt = ai::explain_prompt(record.as_ref(), &screen, p.cwd().as_deref());
        self.send_to_agent(prompt, true);
    }

    /// The selection as context for the agent; with no selection, the last command and its output.
    fn send_selection(&mut self) {
        let Some(p) = self.focused_pane() else { return };
        let selection = p.term.lock().selection_to_string().filter(|s| !s.trim().is_empty());
        let text = match selection {
            Some(s) => s,
            None => match p.last_command.lock().clone() {
                Some(r) => format!("$ {}\n{}", r.command, ai::tail(&ai::plain_text(&r.output), 80, 6000)),
                None => return,
            },
        };
        let snippet = ai::context_snippet(&text, p.cwd().as_deref());
        p.term.lock().selection = None;
        self.send_to_agent(snippet, false);
    }

    fn open_ask(&mut self) {
        if let Some(id) = self.focused_id() {
            self.ask = Ask { open: true, pane: id, generation: self.ask.generation + 1, ..Default::default() };
            self.mark_dirty();
        }
    }

    /// Keys while the Ask AI bar is open. Returns true if consumed.
    fn ask_key(&mut self, event: &KeyEvent) -> bool {
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => {
                self.ask.open = false;
                self.ask.generation += 1;
            }
            _ if self.ask.busy => {}
            Key::Named(NamedKey::Enter) => {
                let task = self.ask.query.trim().to_string();
                if !task.is_empty() {
                    self.ask.busy = true;
                    self.ask.error = None;
                    let cwd = self.panes.get(&self.ask.pane).and_then(|p| p.cwd());
                    ai::ask_async(&self.config.agent.ask_command, &task, cwd, self.ask.pane, self.ask.generation, self.proxy.clone());
                }
            }
            Key::Named(NamedKey::Backspace) => {
                self.ask.query.pop();
                self.ask.error = None;
            }
            _ => match &event.text {
                Some(t) if !t.chars().any(char::is_control) => self.ask.query.push_str(t),
                _ => return false,
            },
        }
        self.mark_dirty();
        true
    }

    fn on_ai_reply(&mut self, pane: PaneId, generation: u64, reply: Result<String, String>) {
        if !self.ask.open || generation != self.ask.generation {
            return;
        }
        self.ask.busy = false;
        match reply {
            Ok(text) => {
                let Some(p) = self.panes.get(&pane) else { return };
                let bracketed = p.term.lock().mode().contains(TermMode::BRACKETED_PASTE);
                let command = ai::clean_command(&text, bracketed);
                if command.is_empty() {
                    self.ask.error = Some("The reply had no command in it".into());
                } else {
                    paste_into(p, &command);
                    self.ask.open = false;
                }
            }
            Err(e) => self.ask.error = Some(e),
        }
        self.mark_dirty();
    }

    // ---- command palette -----------------------------------------------------------------

    fn open_palette(&mut self) {
        let fonts = self.r().monospace_families().to_vec();
        self.palette.open(&fonts);
        self.mark_dirty();
    }

    fn palette_key(&mut self, event: &KeyEvent, event_loop: &ActiveEventLoop) -> bool {
        use palette::Key as K;
        let key = match &event.logical_key {
            Key::Named(NamedKey::ArrowUp) => K::Up,
            Key::Named(NamedKey::ArrowDown) => K::Down,
            Key::Named(NamedKey::Enter) => K::Enter,
            Key::Named(NamedKey::Escape) => K::Escape,
            Key::Named(NamedKey::Backspace) => K::Backspace,
            _ => match &event.text {
                Some(t) if !t.chars().any(char::is_control) => K::Text(t.as_str()),
                _ => return false,
            },
        };
        if let Some(id) = self.palette.handle(key) {
            self.menu_action(&id, event_loop);
        }
        self.mark_dirty();
        true
    }

    fn open_link(&mut self, link: mouse::Link) {
        match link {
            mouse::Link::Url(url) => mouse::open_url(&url),
            mouse::Link::File(path, line, col) => {
                if let Some(cmd) = mouse::open_file(&path, line, col, &self.config.editor) {
                    self.open_tab(Some(&cmd), path.parent().map(PathBuf::from));
                }
            }
        }
    }

    // ---- session restore -----------------------------------------------------------------

    /// Whether this run may read or write the saved session: not `stecak -e …`, and scripted
    /// runs only with their own config dir, so they never touch yours.
    fn owns_session(&self) -> bool {
        let demo = std::env::var_os("STECAK_DEMO").is_some() && std::env::var_os("STECAK_CONFIG").is_none();
        self.cli_shell.is_none() && !demo
    }

    fn restoring_enabled(&self) -> bool {
        self.config.restore_session && self.owns_session()
    }

    fn config_dir(&self) -> &std::path::Path {
        self.config_path.parent().unwrap_or(std::path::Path::new("."))
    }

    fn save_session(&self) {
        if !self.restoring_enabled() {
            return;
        }
        let tabs = self
            .tabs
            .iter()
            .map(|t| {
                let mut ids = vec![];
                t.root.leaves(&mut ids);
                restore::SavedTab { root: self.saved_node(&t.root), focus: ids.iter().position(|&id| id == t.focus).unwrap_or(0) }
            })
            .collect();
        restore::save(&restore::Saved { active: self.active, tabs }, self.config_dir());
    }

    fn saved_node(&self, node: &Node) -> restore::SavedNode {
        match node {
            Node::Leaf(id) => {
                let Some(p) = self.panes.get(id) else { return restore::SavedNode::Pane { cwd: None, command: None } };
                let command = p.foreground_process().and_then(|name| {
                    let session = p.foreground_pid().and_then(agents::claude_session_id);
                    restore::resume_command(&name, session.as_deref())
                });
                restore::SavedNode::Pane { cwd: p.cwd(), command }
            }
            Node::Split { dir, ratio, a, b } => {
                restore::SavedNode::Split { horizontal: *dir == Dir::Horizontal, ratio: *ratio, a: Box::new(self.saved_node(a)), b: Box::new(self.saved_node(b)) }
            }
        }
    }

    /// Reopen the saved tabs. Returns false if there was nothing to restore.
    fn restore_session(&mut self) -> bool {
        let Some(saved) = restore::load(self.config_dir()) else { return false };
        for t in saved.tabs {
            if let Some(root) = self.restored_node(t.root) {
                let mut ids = vec![];
                root.leaves(&mut ids);
                let focus = ids.get(t.focus).copied().unwrap_or(ids[0]);
                self.tabs.push(Tab { root, focus, context_bar: false });
            }
        }
        if self.tabs.is_empty() {
            return false;
        }
        self.active = saved.active.min(self.tabs.len() - 1);
        self.resize_all();
        true
    }

    fn restored_node(&mut self, node: restore::SavedNode) -> Option<Node> {
        match node {
            restore::SavedNode::Pane { cwd, command } => self.spawn_pane(command.as_deref(), cwd.or_else(dirs::home_dir)).map(Node::Leaf),
            restore::SavedNode::Split { horizontal, ratio, a, b } => match (self.restored_node(*a), self.restored_node(*b)) {
                (Some(a), Some(b)) => Some(Node::Split { dir: restore::SavedNode::dir(horizontal), ratio: ratio.clamp(0.1, 0.9), a: Box::new(a), b: Box::new(b) }),
                (Some(n), None) | (None, Some(n)) => Some(n),
                (None, None) => None,
            },
        }
    }

    fn handle_action(&mut self, action: Action, event_loop: &ActiveEventLoop) {
        match action {
            Action::NewTab => self.new_tab(),
            Action::ClosePane => {
                if let Some(id) = self.focused_id() {
                    self.close_pane(id, event_loop);
                }
            }
            Action::NextTab | Action::PrevTab => {
                let n = self.tabs.len().max(1);
                let d = if matches!(action, Action::NextTab) { 1 } else { n - 1 };
                self.active = (self.active + d) % n;
                self.mark_dirty();
            }
            Action::SelectTab(i) => {
                if i < self.tabs.len() {
                    self.active = i;
                    self.mark_dirty();
                }
            }
            Action::Split(dir) => self.split(dir, None),
            Action::AgentSplit => {
                let cmd = self.config.agent.command.clone();
                self.split(Dir::Horizontal, Some(&cmd));
            }
            Action::Sessions => {
                self.sessions.open();
                self.mark_dirty();
            }
            Action::Palette => self.open_palette(),
            Action::AskAi => self.open_ask(),
            Action::ExplainError => self.explain_last_error(),
            Action::SendToAgent => self.send_selection(),
            Action::NextPane => self.cycle_pane(1),
            Action::PrevPane => self.cycle_pane(-1),
            Action::Shortcuts => {
                self.help_open = !self.help_open;
                self.settings.open = false;
                self.mark_dirty();
            }
            Action::OpenSettings => {
                self.settings.toggle();
                self.mark_dirty();
            }
            Action::Copy => self.copy(),
            Action::Paste => match arboard::Clipboard::new().and_then(|mut c| c.get_text()) {
                Ok(text) if !text.is_empty() => self.paste_text(&text),
                // No text (e.g. a screenshot): agents read images from the clipboard themselves
                // when they get Ctrl+V, the key Claude Code and Codex use for pasting images.
                _ => {
                    if let Some(id) = self.focused_id().filter(|&id| self.is_agent_pane(id)) {
                        self.panes[&id].write(b"\x16");
                    }
                }
            },
            Action::Find => {
                if let Some(id) = self.focused_id() {
                    self.search.open(id);
                    self.search_gen += 1;
                    self.mark_dirty();
                }
            }
            Action::FindNext | Action::FindPrev => self.search_step(if matches!(action, Action::FindNext) { Direction::Left } else { Direction::Right }),
            Action::ToggleBosancica => {
                let mut cfg = self.config.clone();
                cfg.bosancica.enabled = !cfg.bosancica.enabled;
                if cfg.bosancica.enabled && !self.r().has_bosancica() {
                    log::warn!("Bosančica mode: font \"{}\" is not installed; see README", cfg.bosancica.font);
                }
                self.apply_and_save(cfg);
            }
            Action::ClearScrollback => {
                if let Some(p) = self.focused_pane() {
                    p.term.lock().grid_mut().clear_history();
                }
                self.mark_dirty();
            }
            Action::FontBigger | Action::FontSmaller | Action::FontReset => {
                let mut cfg = self.config.clone();
                cfg.font.size = match action {
                    Action::FontBigger => (cfg.font.size + 1.0).min(72.0),
                    Action::FontSmaller => (cfg.font.size - 1.0).max(6.0),
                    _ => config::load_from(&self.config_path).map(|c| c.font.size).unwrap_or(config::FontConfig::default().size),
                };
                self.apply_config(cfg);
            }
            Action::Write(bytes) => {
                if let Some(p) = self.focused_pane() {
                    let mut term = p.term.lock();
                    term.scroll_display(Scroll::Bottom);
                    term.selection = None;
                    drop(term);
                    p.write(&bytes);
                }
            }
        }
    }

    /// A menu bar item: map its id onto the same actions the shortcuts use.
    fn menu_action(&mut self, id: &str, event_loop: &ActiveEventLoop) {
        let action = match id {
            "check-updates" => return update::check_async(self.proxy.clone(), true),
            "open-config" => return self.open_config_file(),
            "website" => return mouse::open_url("https://alminisl.github.io/stecak/"),
            "issue" => return mouse::open_url("https://github.com/alminisl/stecak/issues/new"),
            "settings" => Action::OpenSettings,
            "new-tab" => Action::NewTab,
            "split-right" => Action::Split(Dir::Horizontal),
            "split-down" => Action::Split(Dir::Vertical),
            "agent" => Action::AgentSplit,
            "sessions" => Action::Sessions,
            "close" => Action::ClosePane,
            "copy" => Action::Copy,
            "paste" => Action::Paste,
            "find" => Action::Find,
            "find-next" => Action::FindNext,
            "find-prev" => Action::FindPrev,
            "clear" => Action::ClearScrollback,
            "bigger" => Action::FontBigger,
            "smaller" => Action::FontSmaller,
            "actual-size" => Action::FontReset,
            "bosancica" => Action::ToggleBosancica,
            "next-tab" => Action::NextTab,
            "prev-tab" => Action::PrevTab,
            "next-pane" => Action::NextPane,
            "prev-pane" => Action::PrevPane,
            "shortcuts" => Action::Shortcuts,
            "palette" => Action::Palette,
            "ask-ai" => Action::AskAi,
            "explain-error" => Action::ExplainError,
            "send-to-agent" => Action::SendToAgent,
            _ => {
                let mut cfg = self.config.clone();
                if let Some(preset) = id.strip_prefix("theme:").and_then(|i| i.parse::<usize>().ok()).and_then(|i| theme::PRESETS.get(i)) {
                    preset.apply(&mut cfg.colors);
                } else if let Some(family) = id.strip_prefix("font:") {
                    cfg.font.family.retain(|f| f != family);
                    cfg.font.family.insert(0, family.to_string());
                } else {
                    return;
                }
                return self.apply_and_save(cfg);
            }
        };
        self.handle_action(action, event_loop);
    }

    /// The update popup's button (primary = ⏎, otherwise esc/Later).
    fn update_action(&mut self, primary: bool, event_loop: &ActiveEventLoop) {
        let Some(state) = self.update.clone() else { return };
        self.update = match (state, primary) {
            (UpdateUi::Available(rel), true) if update::can_install(&rel) => {
                let v = rel.version.clone();
                update::install_async(rel, self.proxy.clone());
                Some(UpdateUi::Installing(v))
            }
            (UpdateUi::Available(rel), true) => {
                mouse::open_url(&rel.page);
                None
            }
            (UpdateUi::Ready(app), true) => {
                update::relaunch(&app);
                event_loop.exit();
                None
            }
            (UpdateUi::Failed(_, page), true) => {
                mouse::open_url(&page);
                None
            }
            // Installing keeps going in the background; its result reopens the popup.
            (UpdateUi::Installing(_), _) | (UpdateUi::UpToDate, _) | (_, false) => None,
        };
        self.mark_dirty();
    }

    /// A pane rang the bell or sent a notification (agents do this when they finish or need
    /// permission). Mark its tab, and if you're not looking at it, tell the OS.
    fn on_notify(&mut self, id: PaneId, msg: Option<String>) {
        let in_active_tab = self.tabs.get(self.active).is_some_and(|t| {
            let mut ids = vec![];
            t.root.leaves(&mut ids);
            ids.contains(&id)
        });
        if self.focused && in_active_tab {
            return;
        }
        self.attention.insert(id);
        if let Some(w) = &self.window {
            w.request_user_attention(Some(UserAttentionType::Informational));
        }
        let Some(pane) = self.panes.get(&id) else { return };
        // Plain bells from a shell (e.g. tab completion) don't deserve a desktop notification.
        let from_agent = msg.is_some() || pane.foreground_process().is_some_and(|p| agents::is_agent(&p));
        let recent = self.last_notified.get(&id).is_some_and(|t| t.elapsed() < Duration::from_secs(3));
        if self.config.agent.notifications && !self.focused && from_agent && !recent {
            self.last_notified.insert(id, Instant::now());
            let body = msg.unwrap_or_else(|| "Needs your attention".into());
            desktop_notification(&pane.display_title(), &body);
        }
        self.mark_dirty();
    }

    /// Keys while the session browser is open. Returns true if consumed.
    fn sessions_key(&mut self, event: &KeyEvent) -> bool {
        use browser::Key as K;
        let key = match &event.logical_key {
            Key::Named(NamedKey::ArrowUp) => K::Up,
            Key::Named(NamedKey::ArrowDown) => K::Down,
            Key::Named(NamedKey::Enter) => K::Enter,
            Key::Named(NamedKey::Escape) => K::Escape,
            Key::Named(NamedKey::Backspace) => K::Backspace,
            _ => match &event.text {
                Some(t) if !t.chars().any(char::is_control) => K::Text(t.as_str()),
                _ => return false,
            },
        };
        if let Some(s) = self.sessions.handle(key) {
            self.resume(s);
        }
        self.mark_dirty();
        true
    }

    fn resume(&mut self, s: agents::Session) {
        self.open_tab(Some(&s.resume_command()), Some(s.cwd));
    }

    fn search_step(&mut self, dir: Direction) {
        if !self.search.open {
            return;
        }
        if let Some(p) = self.panes.get(&self.search.pane) {
            self.search.step(&mut p.term.lock(), dir);
        }
        self.search_gen += 1;
        self.mark_dirty();
    }

    /// Keys while the search bar is open. Returns true if consumed.
    fn search_key(&mut self, event: &KeyEvent) -> bool {
        let Some(pane) = self.panes.get(&self.search.pane) else { return false };
        let mut query = self.search.query.clone();
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => {
                self.search.close();
                self.search_gen += 1;
                self.mark_dirty();
                return true;
            }
            Key::Named(NamedKey::Enter) => {
                self.search_step(if self.modifiers.shift_key() { Direction::Right } else { Direction::Left });
                return true;
            }
            Key::Named(NamedKey::Backspace) => {
                query.pop();
            }
            _ => match &event.text {
                Some(t) if !t.chars().any(char::is_control) => query.push_str(t),
                _ => return false,
            },
        }
        self.search.set_query(query, &mut pane.term.lock());
        self.search_gen += 1;
        self.mark_dirty();
        true
    }

    fn settings_key(&mut self, event: &KeyEvent) -> bool {
        use settings::Key as K;
        let key = match &event.logical_key {
            Key::Named(NamedKey::ArrowUp) => K::Up,
            Key::Named(NamedKey::ArrowDown) => K::Down,
            Key::Named(NamedKey::ArrowLeft) => K::Left,
            Key::Named(NamedKey::ArrowRight) => K::Right,
            Key::Named(NamedKey::Enter) | Key::Named(NamedKey::Space) if self.settings.editing.is_none() => K::Enter,
            Key::Named(NamedKey::Enter) => K::Enter,
            Key::Named(NamedKey::Escape) => K::Escape,
            Key::Named(NamedKey::Backspace) => K::Backspace,
            _ => match &event.text {
                Some(t) if !t.chars().any(char::is_control) => K::Text(t.as_str()),
                _ => return false,
            },
        };
        match self.settings.handle(key, &self.config) {
            settings::Outcome::Changed(cfg) => self.apply_and_save(cfg),
            settings::Outcome::OpenFile => self.open_config_file(),
            settings::Outcome::Shortcuts => {
                self.settings.open = false;
                self.help_open = true;
            }
            settings::Outcome::None => {}
        }
        self.mark_dirty();
        true
    }

    fn demo_step(&mut self, step: &str, event_loop: &ActiveEventLoop) {
        let (cmd, arg) = step.split_once(':').unwrap_or((step, ""));
        match cmd {
            "split-h" => self.split(Dir::Horizontal, None),
            "split-v" => self.split(Dir::Vertical, None),
            "tab" => self.new_tab(),
            "pane" => self.cycle_pane(1),
            "goto" => self.handle_action(Action::SelectTab(arg.parse().unwrap_or(0)), event_loop),
            "type" => self.handle_action(Action::Write(arg.replace("\\n", "\r").into_bytes()), event_loop),
            "find" => {
                self.handle_action(Action::Find, event_loop);
                if let Some(p) = self.panes.get(&self.search.pane) {
                    self.search.set_query(arg.to_string(), &mut p.term.lock());
                }
                self.search_gen += 1;
            }
            "settings" => self.handle_action(Action::OpenSettings, event_loop),
            "sessions" => {
                self.sessions.open();
                self.sessions.query = arg.to_string();
            }
            "help" => self.help_open = true,
            "palette" => {
                self.open_palette();
                self.palette.query = arg.to_string();
            }
            "ask" => {
                self.open_ask();
                self.ask.query = arg.to_string();
            }
            "ask-go" => {
                self.ask.busy = true;
                let cwd = self.panes.get(&self.ask.pane).and_then(|p| p.cwd());
                ai::ask_async(&self.config.agent.ask_command, &self.ask.query, cwd, self.ask.pane, self.ask.generation, self.proxy.clone());
            }
            "explain" => self.explain_last_error(),
            "quit" => {
                self.save_session();
                event_loop.exit();
            }
            // Log what's on screen, for scripted tests that can't take screenshots.
            "dump" => {
                for (i, t) in self.tabs.iter().enumerate() {
                    let mut ids = vec![];
                    t.root.leaves(&mut ids);
                    for id in ids {
                        if let Some(p) = self.panes.get(&id) {
                            log::info!("dump tab {i} pane {id}:\n{}", mouse::screen_text(&p.term.lock()));
                        }
                    }
                }
                if self.palette.open {
                    log::info!("dump palette: {:?}", self.palette.visible().iter().take(5).map(|e| &e.label).collect::<Vec<_>>());
                }
                if self.settings.open {
                    log::info!("dump settings: {:?}", self.settings.rows(&self.config).filter(|r| r.2).map(|(l, v, _)| format!("{l}={v}")).collect::<Vec<_>>());
                }
                if self.ask.open {
                    log::info!("dump ask: busy={} error={:?}", self.ask.busy, self.ask.error);
                }
                let statuses = self.statuses();
                log::info!("dump tabs: active={} statuses={statuses:?}", self.active);
                if let Some(c) = &self.context {
                    use alacritty_terminal::grid::Dimensions;
                    let b = self.breakdowns.get(&c.cwd).and_then(|b| b.1.as_ref());
                    let rows = self.focused_id().and_then(|id| self.panes.get(&id)).map(|p| p.term.lock().screen_lines());
                    log::info!("dump context: session={} used={:?} rows={rows:?} breakdown={:?}", c.session, c.used, b.map(|b| b.rows(c.used.as_ref().map_or(b.overhead(), |u| u.0))));
                }
            }
            "send" => self.send_selection(),
            "check-update" => update::check_async(self.proxy.clone(), true),
            "update-go" => self.update_action(true, event_loop),
            "update" => self.update = Some(UpdateUi::Available(update::Release { version: arg.to_string(), page: String::new(), dmg: None })),
            "attention" => {
                if let Some(id) = self.tabs.first().map(|t| t.focus) {
                    self.attention.insert(id);
                }
            }
            "settings-key" => {
                use settings::Key as K;
                let key = match arg {
                    "up" => K::Up,
                    "down" => K::Down,
                    "left" => K::Left,
                    "right" => K::Right,
                    "enter" => K::Enter,
                    _ => K::Escape,
                };
                if let settings::Outcome::Changed(cfg) = self.settings.handle(key, &self.config) {
                    self.apply_config(cfg); // not saved: demo runs must not touch the user's config
                }
            }
            // select:row,col,row,col in the focused pane (viewport cells).
            "select" => {
                let n: Vec<usize> = arg.split(',').filter_map(|v| v.parse().ok()).collect();
                if let (Some(p), [r1, c1, r2, c2]) = (self.focused_pane(), n.as_slice()) {
                    let mut term = p.term.lock();
                    let mut sel = Selection::new(SelectionType::Simple, Point::new(Line(*r1 as i32), Column(*c1)), Side::Left);
                    sel.update(Point::new(Line(*r2 as i32), Column(*c2)), Side::Right);
                    term.selection = Some(sel);
                }
            }
            "image" => {
                let mut cfg = self.config.clone();
                cfg.background_image.path = arg.to_string();
                self.apply_config(cfg);
            }
            _ => log::warn!("unknown demo step {step}"),
        }
        self.mark_dirty();
    }

    // ---- mouse -------------------------------------------------------------------------

    /// Pane under the mouse, with the cell (viewport col/row) and which half of the cell.
    fn hit(&self) -> Option<(PaneGeom, usize, usize, Side)> {
        let (x, y) = self.mouse;
        let (cw, ch) = self.r().cell();
        let (geoms, _) = self.geometry(self.active);
        let g = geoms.into_iter().find(|g| g.rect.contains(x, y))?;
        let fx = ((x - g.gx) / cw).max(0.0);
        let col = (fx as usize).min(g.cols - 1);
        let row = (((y - g.gy) / ch).max(0.0) as usize).min(g.rows - 1);
        let side = if fx.fract() < 0.5 { Side::Left } else { Side::Right };
        Some((g, col, row, side))
    }

    fn open_link_modifier(&self) -> bool {
        if cfg!(target_os = "macos") { self.modifiers.super_key() } else { self.modifiers.control_key() }
    }

    fn update_hover(&mut self) {
        let mut hover = None;
        if self.open_link_modifier() {
            if let Some((g, col, row, _)) = self.hit() {
                if let Some(p) = self.panes.get(&g.id) {
                    let cwd = p.cwd();
                    let term = p.term.lock();
                    let line = Line(row as i32 - term.grid().display_offset() as i32);
                    if let Some((a, b, url)) = mouse::hyperlink_at(&term, line, col).or_else(|| mouse::url_at(&term, line, col)) {
                        hover = Some((g.id, line.0, a, b, mouse::Link::Url(url)));
                    } else if let Some((a, b, link)) = mouse::path_at(&term, line, col, cwd.as_deref()) {
                        hover = Some((g.id, line.0, a, b, link));
                    }
                }
            }
        }
        let changed = hover.as_ref().map(|h| (h.0, h.1, h.2)) != self.hover_url.as_ref().map(|h| (h.0, h.1, h.2));
        self.hover_url = hover;
        if changed {
            self.mark_dirty();
        }
        let icon = match (&self.hover_url, self.divider_under_mouse()) {
            (Some(_), _) => CursorIcon::Pointer,
            (None, Some(d)) if d.dir == Dir::Horizontal => CursorIcon::ColResize,
            (None, Some(_)) => CursorIcon::RowResize,
            (None, None) if self.mouse.1 < self.tab_bar_h() => CursorIcon::Default,
            (None, None) => CursorIcon::Text,
        };
        if icon != self.cursor_icon {
            self.cursor_icon = icon;
            if let Some(w) = &self.window {
                w.set_cursor(icon);
            }
        }
    }

    fn mouse_report(&self, id: PaneId, button: mouse::Button, pressed: bool, motion: bool, col: usize, row: usize) -> bool {
        let Some(p) = self.panes.get(&id) else { return false };
        let mode = *p.term.lock().mode();
        match mouse::report(mode, button, pressed, motion, col, row, self.modifiers) {
            Some(bytes) => {
                p.write(&bytes);
                true
            }
            None => false,
        }
    }

    fn wants_mouse(&self, id: PaneId) -> bool {
        // Shift always selects text, even in apps that capture the mouse (xterm convention).
        !self.modifiers.shift_key() && self.panes.get(&id).is_some_and(|p| p.term.lock().mode().intersects(TermMode::MOUSE_MODE))
    }

    fn on_press(&mut self, button: MouseButton, event_loop: &ActiveEventLoop) {
        let (x, y) = self.mouse;
        if self.help_open {
            self.help_open = false;
            self.mark_dirty();
            return;
        }
        if let Some((primary, secondary)) = self.update_buttons {
            if primary.contains(x, y) || secondary.contains(x, y) {
                return self.update_action(primary.contains(x, y), event_loop);
            }
        }
        if self.sessions.open {
            match self.sessions_layout {
                Some((panel, y0, lh, first)) if panel.contains(x, y) => {
                    if y >= y0 {
                        if let Some(s) = self.sessions.pick(first + ((y - y0) / lh) as usize) {
                            self.resume(s);
                        }
                    }
                }
                _ => self.sessions.open = false,
            }
            self.mark_dirty();
            return;
        }
        if self.palette.open {
            match self.palette_layout {
                Some((panel, y0, lh, first)) if panel.contains(x, y) => {
                    if y >= y0 {
                        if let Some(id) = self.palette.pick(first + ((y - y0) / lh) as usize) {
                            self.menu_action(&id, event_loop);
                        }
                    }
                }
                _ => self.palette.open = false,
            }
            self.mark_dirty();
            return;
        }
        if self.settings.open {
            self.settings.open = false;
            self.mark_dirty();
            return;
        }
        if y < self.tab_bar_h() {
            if button == MouseButton::Left && y >= self.titlebar_h() {
                self.on_tab_bar_click(x, event_loop);
            }
            return;
        }
        if button == MouseButton::Left {
            if let Some(d) = self.divider_under_mouse() {
                self.drag = Some(Drag::Divider(d));
                return;
            }
        }
        let Some((g, col, row, side)) = self.hit() else { return };
        if self.tabs[self.active].focus != g.id {
            self.tabs[self.active].focus = g.id;
            self.mark_dirty();
        }
        if button == MouseButton::Left {
            if let Some((_, _, _, _, link)) = self.hover_url.clone().filter(|h| h.0 == g.id) {
                self.open_link(link);
                return;
            }
        }
        let mbutton = match button {
            MouseButton::Left => mouse::Button::Left,
            MouseButton::Middle => mouse::Button::Middle,
            MouseButton::Right => mouse::Button::Right,
            _ => return,
        };
        if self.wants_mouse(g.id) {
            if self.mouse_report(g.id, mbutton, true, false, col, row) {
                self.drag = Some(Drag::Report(g.id));
            }
            return;
        }
        if button != MouseButton::Left {
            return;
        }
        // Start a selection: single = characters, double = word, triple = line.
        let Some(p) = self.panes.get(&g.id) else { return };
        let mut term = p.term.lock();
        let point = Point::new(Line(row as i32 - term.grid().display_offset() as i32), Column(col));
        let now = Instant::now();
        let clicks = match self.last_click {
            Some((t, id, pt, n)) if id == g.id && pt == point && now - t < Duration::from_millis(400) => n % 3 + 1,
            _ => 1,
        };
        self.last_click = Some((now, g.id, point, clicks));
        if self.modifiers.shift_key() && term.selection.is_some() && clicks == 1 {
            if let Some(sel) = term.selection.as_mut() {
                sel.update(point, side);
            }
        } else {
            let ty = match clicks {
                2 => SelectionType::Semantic,
                3 => SelectionType::Lines,
                _ => SelectionType::Simple,
            };
            term.selection = Some(Selection::new(ty, point, side));
        }
        drop(term);
        self.drag = Some(Drag::Select(g.id));
        self.mark_dirty();
    }

    fn on_release(&mut self, button: MouseButton) {
        match self.drag.take() {
            Some(Drag::Report(id)) => {
                if let Some((_, col, row, _)) = self.hit() {
                    let b = match button {
                        MouseButton::Middle => mouse::Button::Middle,
                        MouseButton::Right => mouse::Button::Right,
                        _ => mouse::Button::Left,
                    };
                    self.mouse_report(id, b, false, false, col, row);
                }
            }
            Some(Drag::Select(id)) => {
                // A plain click (no drag) leaves an empty selection; clear it.
                if let Some(p) = self.panes.get(&id) {
                    let mut term = p.term.lock();
                    if term.selection.as_ref().is_some_and(|s| s.is_empty()) {
                        term.selection = None;
                    }
                }
                self.mark_dirty();
            }
            Some(Drag::Divider(_)) | None => {}
        }
    }

    /// Divider within a few pixels of the mouse (dividers are 1px; give them a usable grab area).
    fn divider_under_mouse(&self) -> Option<Divider> {
        let (x, y) = self.mouse;
        let slop = 4.0 * self.r().scale;
        let (_, dividers) = self.geometry(self.active);
        dividers.into_iter().find(|d| {
            let r = d.rect;
            Rect { x: r.x - slop, y: r.y - slop, w: r.w + 2.0 * slop, h: r.h + 2.0 * slop }.contains(x, y)
        })
    }

    fn on_motion(&mut self) {
        self.update_hover();
        // Redraw only when the hovered tab changes (it shows a close ×).
        let (x, y) = self.mouse;
        let hover_tab = (y >= self.titlebar_h() && y < self.tab_bar_h()).then(|| {
            let strip = self.tab_bar_h() - self.titlebar_h();
            let tab_w = ((self.r().size().0 - strip) / self.tabs.len().max(1) as f32).min(260.0 * self.r().scale);
            (x / tab_w) as usize
        });
        if hover_tab != self.hover_tab {
            self.hover_tab = hover_tab;
            self.mark_dirty();
        }
        let over_context = self.context.is_some() && y >= self.r().size().1 - self.context_bar_h(self.active);
        if over_context != self.context_hover {
            self.context_hover = over_context;
            self.mark_dirty();
        }
        match self.drag.clone() {
            Some(Drag::Divider(d)) => {
                let (x, y) = self.mouse;
                let ratio = match d.dir {
                    Dir::Horizontal => (x - d.split.x) / d.split.w,
                    Dir::Vertical => (y - d.split.y) / d.split.h,
                };
                self.tabs[self.active].root.set_ratio(&d.path, ratio);
                self.resize_all();
            }
            Some(Drag::Select(id)) => {
                let (cw, ch) = self.r().cell();
                let (geoms, _) = self.geometry(self.active);
                let Some(g) = geoms.into_iter().find(|g| g.id == id) else { return };
                let (x, y) = self.mouse;
                let fx = ((x - g.gx) / cw).clamp(0.0, g.cols as f32 - 0.01);
                let row = ((y - g.gy) / ch).floor().clamp(0.0, g.rows as f32 - 1.0) as usize;
                let side = if fx.fract() < 0.5 { Side::Left } else { Side::Right };
                if let Some(p) = self.panes.get(&id) {
                    let mut term = p.term.lock();
                    let point = Point::new(Line(row as i32 - term.grid().display_offset() as i32), Column(fx as usize));
                    if let Some(sel) = term.selection.as_mut() {
                        sel.update(point, side);
                    }
                }
                self.mark_dirty();
            }
            Some(Drag::Report(id)) => {
                if let Some((_, col, row, _)) = self.hit().filter(|h| h.0.id == id) {
                    self.mouse_report(id, mouse::Button::Left, true, true, col, row);
                }
            }
            None => {}
        }
    }

    fn on_tab_bar_click(&mut self, x: f32, event_loop: &ActiveEventLoop) {
        let (win_w, _) = self.r().size();
        let plus_w = self.tab_bar_h() - self.titlebar_h();
        if x >= win_w - plus_w {
            // ⚙ at the far right: settings (where the config file and shortcuts are).
            self.settings.toggle();
            self.mark_dirty();
            return;
        }
        let tab_w = ((win_w - plus_w) / self.tabs.len().max(1) as f32).min(260.0 * self.r().scale);
        let idx = (x / tab_w) as usize;
        if idx < self.tabs.len() {
            // The × sits in the last two cells of the tab.
            let on_close = x >= (idx as f32 + 1.0) * tab_w - 2.5 * self.r().cell().0;
            if self.modifiers.alt_key() || on_close {
                let mut ids = vec![];
                self.tabs[idx].root.leaves(&mut ids);
                for id in ids {
                    self.close_pane(id, event_loop);
                }
            } else {
                self.active = idx;
            }
            self.mark_dirty();
        } else if x < tab_w * self.tabs.len() as f32 + plus_w {
            self.new_tab();
        }
    }

    fn on_scroll(&mut self, delta: MouseScrollDelta) {
        let (_, ch) = self.r().cell();
        let lines = match delta {
            MouseScrollDelta::LineDelta(_, y) => y as f64 * 3.0,
            MouseScrollDelta::PixelDelta(p) => p.y / ch as f64,
        };
        self.scroll_accum += lines;
        let whole = self.scroll_accum.trunc() as i32;
        if whole == 0 {
            return;
        }
        self.scroll_accum -= whole as f64;
        let Some((g, col, row, _)) = self.hit() else { return };
        if self.wants_mouse(g.id) {
            let b = if whole > 0 { mouse::Button::WheelUp } else { mouse::Button::WheelDown };
            for _ in 0..whole.abs().min(10) {
                self.mouse_report(g.id, b, true, false, col, row);
            }
            return;
        }
        let Some(p) = self.panes.get(&g.id) else { return };
        let mut term = p.term.lock();
        let mode = *term.mode();
        if mode.contains(TermMode::ALT_SCREEN) && mode.contains(TermMode::ALTERNATE_SCROLL) {
            // Full-screen apps without mouse mode (less, man…): the wheel becomes arrow keys.
            let key: &[u8] = match (whole > 0, mode.contains(TermMode::APP_CURSOR)) {
                (true, true) => b"\x1bOA",
                (true, false) => b"\x1b[A",
                (false, true) => b"\x1bOB",
                (false, false) => b"\x1b[B",
            };
            drop(term);
            for _ in 0..whole.abs() {
                p.write(key);
            }
        } else {
            term.scroll_display(Scroll::Delta(whole));
            drop(term);
            self.mark_dirty();
        }
    }

    // ---- drawing -----------------------------------------------------------------------

    fn draw(&mut self) {
        let t0 = Instant::now();
        self.poll_context();
        let atlas_gen = self.r().atlas_gen();
        // Output arriving from now on needs another frame.
        self.wakeup_pending.store(false, Ordering::Release);
        let (geoms, dividers) = self.geometry(self.active);
        let tab_bar_h = self.tab_bar_h();
        let title_h = self.titlebar_h();
        let focus = self.focused_id();
        let multi = geoms.len() > 1;
        let image = self.config.background_image.clone();

        // Tab status for agents: working (recent output from claude/codex) or waiting on you.
        let now = pane::now_ms();
        if self.focused {
            if let Some(t) = self.tabs.get(self.active) {
                let mut ids = vec![];
                t.root.leaves(&mut ids);
                ids.iter().for_each(|id| {
                    self.attention.remove(id);
                });
            }
        }
        let statuses = self.statuses();
        self.animating = statuses.iter().any(|s| s.0) || self.ask.busy;

        // Frame skipping: only panes on screen matter. Output in background tabs (or nothing
        // at all) costs no CPU rendering and no GPU work.
        let fresh = geoms.iter().filter_map(|g| self.panes.get(&g.id)).fold(false, |acc, p| p.fresh.swap(false, Ordering::AcqRel) | acc);
        if !fresh && !self.dirty {
            self.stats.skipped += 1;
            if self.stats.enabled && self.stats.since.get_or_insert_with(Instant::now).elapsed() >= Duration::from_secs(2) {
                self.flush_stats();
            }
            return;
        }

        let App { renderer, panes, caches, theme, search, tabs, config, .. } = self;
        let r = renderer.as_mut().unwrap();
        let (cw, ch) = r.cell();
        let (win_w, win_h) = r.size();
        let opacity = config.window.opacity.clamp(0.0, 1.0);
        r.begin();

        // Background: optional image, then the theme color as a tint over it.
        let clear = if let Some((iw, ih)) = r.image_size {
            let (iw, ih) = (iw as f32, ih as f32);
            match image.fit.as_str() {
                "contain" | "center" => r.image(((win_w - iw) / 2.0).round(), ((win_h - ih) / 2.0).round(), iw, ih, [0.0, 0.0, 1.0, 1.0], image.opacity),
                _ => r.image(0.0, 0.0, win_w, win_h, [0.0, 0.0, 1.0, 1.0], image.opacity),
            }
            r.rect(0.0, 0.0, win_w, win_h, rgba(theme.bg, image.tint));
            [0.0; 4]
        } else {
            rgba(theme.bg, opacity)
        };

        // Panes.
        let mut stats = DrawStats::default();
        let hover = self.hover_url.as_ref().map(|h| (h.0, (h.1, h.2, h.3)));
        for g in &geoms {
            let Some(p) = panes.get(&g.id) else { continue };
            let mut term = p.term.lock();
            let searching = search.open && search.pane == g.id;
            let matches = if searching { search.visible_matches(&term) } else { Vec::new() };
            let hl = Highlights {
                matches: &matches,
                current: if searching { search.current.as_ref() } else { None },
                hover: hover.filter(|h| h.0 == g.id).map(|h| h.1),
                bosancica: config.bosancica.enabled,
            };
            let cache = caches.entry(g.id).or_default();
            let s = draw::draw_pane(r, &mut term, cache, theme, g.gx, g.gy, self.theme_gen, self.search_gen, &hl);
            stats.rows_built += s.rows_built;
            stats.rows_cached += s.rows_cached;
            let is_focus = Some(g.id) == focus;
            draw::draw_cursor(r, &term, theme, g.gx, g.gy, self.focused && is_focus);
            drop(term);
            if multi && !is_focus {
                // Dim unfocused splits slightly.
                r.rect(g.rect.x, g.rect.y, g.rect.w, g.rect.h, rgba(theme.bg, 0.35));
            }
            if searching {
                draw_search_bar(r, theme, search, g);
            }
            if self.ask.open && self.ask.pane == g.id {
                draw_ask_bar(r, theme, &self.ask, g, now, &config.agent.ask_command);
            }
        }
        // IME: show the text being composed at the cursor, and tell the OS where the cursor is
        // so the candidate window appears next to it.
        if let Some(g) = geoms.iter().find(|g| Some(g.id) == focus) {
            if let Some(p) = panes.get(&g.id) {
                let term = p.term.lock();
                let c = term.grid().cursor.point;
                let (x, y) = (g.gx + c.column.0 as f32 * cw, g.gy + c.line.0 as f32 * ch);
                drop(term);
                if !self.preedit.is_empty() {
                    let w = self.preedit.chars().count() as f32 * cw;
                    r.rect(x, y, w, ch, rgba(theme.bg, 1.0));
                    r.text(&self.preedit, x, y, x + w + cw, rgba(theme.fg, 1.0));
                    r.rect(x, y + ch - 2.0 * r.scale, w, r.scale.max(1.0), rgba(theme.fg, 1.0));
                }
                if let Some(w) = &self.window {
                    w.set_ime_cursor_area(winit::dpi::PhysicalPosition::new(x, y), PhysicalSize::new(cw, ch));
                }
            }
        }
        for d in &dividers {
            let d = d.rect;
            r.rect(d.x, d.y, d.w, d.h, rgba(theme.tab_active, 1.0));
        }
        if let Some(c) = &self.context {
            let breakdown = self.breakdowns.get(&c.cwd).and_then(|b| b.1.as_ref());
            draw_context_bar(r, theme, c.used.as_ref(), breakdown, self.context_hover);
        }

        // Tab bar.
        if tab_bar_h > 0.0 {
            // Titlebar band + tab strip: opaque enough that the native titlebar stays readable.
            r.rect(0.0, 0.0, win_w, tab_bar_h, rgba(theme.tab_bar, opacity.max(0.92)));
            let strip_h = tab_bar_h - title_h;
            let plus_w = strip_h;
            let tab_w = ((win_w - plus_w) / tabs.len().max(1) as f32).min(260.0 * r.scale);
            let text_y = (title_h + (strip_h - ch) / 2.0).round();
            for (i, tab) in tabs.iter().enumerate() {
                let x = i as f32 * tab_w;
                let active = i == self.active;
                if active {
                    r.rect(x, title_h, tab_w, strip_h, rgba(theme.tab_active, 1.0));
                    r.rect(x, tab_bar_h - 2.0 * r.scale, tab_w, 2.0 * r.scale, rgba(theme.palette[4], 1.0));
                }
                r.rect(x + tab_w - r.scale, title_h + strip_h * 0.25, r.scale, strip_h * 0.5, rgba(theme.fg, 0.15));
                let mut n = vec![];
                tab.root.leaves(&mut n);
                let title = panes.get(&tab.focus).map(|p| p.display_title()).unwrap_or_default();
                let label = if n.len() > 1 { format!("{}  {}  [{}]", i + 1, title, n.len()) } else { format!("{}  {}", i + 1, title) };
                let (working, waiting) = statuses.get(i).copied().unwrap_or_default();
                let amber = rgba(theme.palette[3], 1.0);
                let mut lx = x + cw;
                if waiting {
                    // Steady dot: an agent finished or wants permission.
                    r.text("●", lx, text_y, x + tab_w, amber);
                    lx += 2.0 * cw;
                } else if working {
                    // A rosette that turns while the agent works…
                    const ROSETTE: [&str; 4] = ["✻", "✼", "✽", "❋"];
                    r.text(ROSETTE[(now / 160 % 4) as usize], lx, text_y, x + tab_w, amber);
                    lx += 2.0 * cw;
                    // …and a chisel stroke sweeping along the tab's edge, like carving stone.
                    let t = (now % 1600) as f32 / 1600.0;
                    let seg = tab_w * 0.3;
                    let sx = x - seg + (tab_w + seg) * t;
                    let (a, b) = (sx.max(x), (sx + seg).min(x + tab_w));
                    if b > a {
                        r.rect(a, tab_bar_h - 2.0 * r.scale, b - a, 2.0 * r.scale, amber);
                    }
                }
                let hovered = self.mouse.1 >= title_h && self.mouse.1 < tab_bar_h && self.mouse.0 >= x && self.mouse.0 < x + tab_w;
                let show_close = active || hovered;
                let label_end = if show_close { x + tab_w - 3.0 * cw } else { x + tab_w - cw };
                r.text(&label, lx, text_y, label_end, rgba(theme.fg, if active { 1.0 } else { 0.55 }));
                if show_close {
                    r.text("×", x + tab_w - 2.0 * cw, text_y, x + tab_w, rgba(theme.fg, if hovered { 0.8 } else { 0.45 }));
                }
            }
            let plus_x = tab_w * tabs.len() as f32;
            r.text("+", plus_x + (plus_w - cw) / 2.0, text_y, plus_x + plus_w, rgba(theme.fg, 0.7));
            let gear_x = win_w - plus_w;
            r.text("⚙", gear_x + (plus_w - cw) / 2.0, text_y, win_w, rgba(theme.fg, 0.6));
        }

        self.update_buttons = self.update.as_ref().and_then(|u| {
            let current = env!("CARGO_PKG_VERSION");
            match u {
                UpdateUi::Available(rel) => {
                    let action = if update::can_install(rel) { "Update now" } else { "Download" };
                    draw_update(r, theme, &format!("Stećak {} is available", rel.version), &format!("You have {current}"), Some(action), "Later")
                }
                UpdateUi::UpToDate => draw_update(r, theme, "You're up to date", &format!("Stećak {current} is the latest version"), None, "OK"),
                UpdateUi::Installing(v) => draw_update(r, theme, &format!("Updating to {v}…"), "Downloading and verifying the release", None, "Hide"),
                UpdateUi::Ready(_) => draw_update(r, theme, "Update installed", "Restart Stećak to use the new version", Some("Restart"), "Later"),
                UpdateUi::Failed(e, _) => draw_update(r, theme, "Update failed", e, Some("Download page"), "Close"),
            }
        });
        self.sessions_layout = None;
        if self.sessions.open {
            self.sessions_layout = Some(draw_sessions(r, theme, &self.sessions));
        }
        self.palette_layout = None;
        if self.palette.open {
            self.palette_layout = Some(draw_palette(r, theme, &self.palette));
        }
        if self.help_open {
            draw_shortcuts(r, theme);
        }
        if self.settings.open {
            self.settings.bosancica_font_ok = r.has_bosancica();
            if self.settings.fonts.is_empty() {
                self.settings.fonts = r.monospace_families().to_vec();
            }
            draw_settings(r, theme, &self.settings, config);
        }

        let t1 = Instant::now();
        // The glyph atlas filled up and was reset mid-frame: glyphs placed before the reset (and
        // cached rows) point at stale atlas slots. Redraw everything once more right away, or
        // wrong/missing letters can stay on screen until the next output.
        let atlas_reset = r.atlas_gen() != atlas_gen;
        if !r.present(clear) {
            // The OS had no drawable for us (launch animation, resize, occlusion): this frame
            // never reached the screen, so the next one must not be skipped. Don't request a
            // redraw here: if presenting keeps failing that would spin at 100% CPU; the next
            // event (focus, resize, output, unocclusion) draws it.
            self.dirty = true;
            return;
        }
        self.dirty = false;
        // Retry once only: if a single screen needs more glyphs than the atlas holds, every
        // frame resets it and an unconditional retry would redraw forever.
        if atlas_reset && !self.atlas_retry {
            log::debug!("glyph atlas reset; redrawing");
            self.mark_dirty();
        }
        self.atlas_retry = atlas_reset;

        if let (Some(w), Some(p)) = (&self.window, focus.and_then(|f| self.panes.get(&f))) {
            w.set_title(&format!("{} — Stećak", p.display_title()));
        }
        self.record_stats(t1 - t0, t1.elapsed(), stats);
    }

    /// Per tab: an agent is working, and one is waiting on you. Claude Code says whether it's
    /// busy; other agents count as working while they print (idle Claude Code also redraws
    /// now and then, e.g. its status line, so output alone would flicker the animation).
    fn statuses(&mut self) -> Vec<(bool, bool)> {
        let now = pane::now_ms();
        let mut ids = vec![];
        for t in &self.tabs {
            t.root.leaves(&mut ids);
        }
        self.claude_busy.retain(|id, _| self.panes.contains_key(id));
        let mut working = HashSet::new();
        for id in ids {
            let Some(p) = self.panes.get(&id) else { continue };
            let Some(name) = p.foreground_process().filter(|n| agents::is_agent(n)) else { continue };
            let printing = now.saturating_sub(p.last_output.load(Ordering::Acquire)) < 1500;
            let busy = if name.contains("claude") {
                let busy = match self.claude_busy.get(&id) {
                    Some(&(t, busy)) if t.elapsed() < Duration::from_millis(500) => busy,
                    _ => {
                        let busy = p.foreground_pid().and_then(agents::claude_busy);
                        self.claude_busy.insert(id, (Instant::now(), busy));
                        busy
                    }
                };
                busy.unwrap_or(printing)
            } else {
                printing
            };
            if busy {
                working.insert(id);
            }
        }
        self.tabs
            .iter()
            .map(|t| {
                let mut ids = vec![];
                t.root.leaves(&mut ids);
                (ids.iter().any(|id| working.contains(id)), ids.iter().any(|id| self.attention.contains(id)))
            })
            .collect()
    }

    fn record_stats(&mut self, build: Duration, present: Duration, rows: DrawStats) {
        let s = &mut self.stats;
        if !s.enabled {
            return;
        }
        s.frames += 1;
        s.build += build;
        s.present += present;
        s.rows.rows_built += rows.rows_built;
        s.rows.rows_cached += rows.rows_cached;
        let since = *s.since.get_or_insert_with(Instant::now);
        if since.elapsed() >= Duration::from_secs(2) {
            self.flush_stats();
        }
    }

    fn flush_stats(&mut self) {
        let s = &mut self.stats;
        if !s.enabled || s.frames + s.skipped == 0 {
            return;
        }
        let n = s.frames.max(1);
        let secs = s.since.map_or(0.0, |t| t.elapsed().as_secs_f32());
        log::info!(
            "{:.1}s: frames {} (skipped {}), build avg {:?}, present avg {:?}, rows rebuilt {} / cached {}",
            secs,
            s.frames,
            s.skipped,
            s.build / n,
            s.present / n,
            s.rows.rows_built,
            s.rows.rows_cached
        );
        *s = Stats { enabled: true, since: Some(Instant::now()), ..Default::default() };
    }
}

fn draw_search_bar(r: &mut Renderer, theme: &Theme, search: &Search, g: &PaneGeom) {
    let (cw, ch) = r.cell();
    let h = (ch * 1.5).round();
    let y = g.rect.y + g.rect.h - h;
    r.rect(g.rect.x, y, g.rect.w, h, rgba(theme.tab_bar, 0.95));
    r.rect(g.rect.x, y, g.rect.w, r.scale.max(1.0), rgba(theme.palette[4], 1.0));
    let status = if search.query.is_empty() {
        "type to search · ⏎ older · ⇧⏎ newer · esc"
    } else if search.no_match {
        "no matches"
    } else {
        "⏎ older · ⇧⏎ newer · esc"
    };
    let ty = y + ((h - ch) / 2.0).round();
    let text = format!("Find: {}▏", search.query);
    let end = r.text_end(&text, g.rect.x + cw, g.rect.x + g.rect.w - cw);
    r.text(&text, g.rect.x + cw, ty, g.rect.x + g.rect.w - cw, rgba(theme.fg, 1.0));
    let color = if search.no_match { theme.palette[1] } else { theme.fg };
    r.text(status, end + 2.0 * cw, ty, g.rect.x + g.rect.w - cw, rgba(color, 0.6));
}

/// Keyboard shortcut legend (⌘/).
fn draw_shortcuts(r: &mut Renderer, theme: &Theme) {
    let (cw, ch) = r.cell();
    let (win_w, win_h) = r.size();
    let items = settings::shortcuts();
    let lh = (ch * 1.35).round();
    let w = (cw * 70.0).min(win_w - 4.0 * cw);
    let h = lh * (items.len() as f32 + 4.2);
    let (x, y) = (((win_w - w) / 2.0).round(), ((win_h - h) / 2.0).round().max(0.0));
    r.rect(0.0, 0.0, win_w, win_h, [0.0, 0.0, 0.0, 0.45]);
    r.rect(x, y, w, h, rgba(theme.tab_bar, 0.98));
    r.rect(x, y, w, r.scale.max(1.0), rgba(theme.palette[3], 1.0));
    let pad = 2.0 * cw;
    let dy = ((lh - ch) / 2.0).round();
    r.text("Keyboard shortcuts", x + pad, y + dy + lh * 0.3, x + w - pad, rgba(theme.fg, 1.0));
    let keys_w = 26.0 * cw;
    for (i, (keys, what)) in items.iter().enumerate() {
        let ry = y + lh * (i as f32 + 1.6);
        r.text(keys, x + pad, ry + dy, x + pad + keys_w, rgba(theme.palette[3], 1.0));
        r.text(what, x + pad + keys_w, ry + dy, x + w - pad, rgba(theme.fg, 0.85));
    }
    let config = config::find_config_path().map_or("~/.config/stecak/config.yaml (created on first save)".into(), |p| browser::tilde(&p));
    r.text(&format!("Config: {config} · reloads on save"), x + pad, y + h - lh * 1.3 + dy, x + w - pad, rgba(theme.fg, 0.5));
}

/// The update popup at the top of the window. Returns (primary, secondary) button rects.
fn draw_update(r: &mut Renderer, theme: &Theme, title: &str, subtitle: &str, primary: Option<&str>, secondary: &str) -> Option<(Rect, Rect)> {
    let (cw, ch) = r.cell();
    let (win_w, _) = r.size();
    let lh = (ch * 1.5).round();
    let w = (cw * 64.0).min(win_w - 4.0 * cw);
    let (x, y) = (((win_w - w) / 2.0).round(), (ch * 3.0).round());
    let h = lh * 3.4;
    r.rect(x, y, w, h, rgba(theme.tab_bar, 0.98));
    r.rect(x, y, w, r.scale.max(1.0), rgba(theme.palette[3], 1.0));
    let pad = 2.0 * cw;
    let dy = ((lh - ch) / 2.0).round();
    r.text(title, x + pad, y + lh * 0.3 + dy, x + w - pad, rgba(theme.fg, 1.0));
    r.text(subtitle, x + pad, y + lh * 1.1 + dy, x + w - pad, rgba(theme.fg, 0.55));
    let by = y + lh * 2.1;
    let button = |r: &mut Renderer, label: &str, right: f32, primary: bool| {
        let bw = (label.chars().count() as f32 + 3.0) * cw;
        let bx = right - bw;
        r.rect(bx, by, bw, lh, if primary { rgba(theme.palette[3], 1.0) } else { rgba(theme.tab_active, 1.0) });
        r.text(label, bx + 1.5 * cw, by + dy, bx + bw, if primary { rgba(theme.bg, 1.0) } else { rgba(theme.fg, 0.9) });
        Rect { x: bx, y: by, w: bw, h: lh }
    };
    let second = button(r, &format!("{secondary}  esc"), x + w - pad, false);
    let first = match primary {
        Some(label) => button(r, &format!("{label}  ⏎"), second.x - cw, true),
        None => Rect { x: 0.0, y: 0.0, w: 0.0, h: 0.0 },
    };
    Some((first, second))
}

/// The session browser panel. Returns (panel rect, first row y, row height, first index)
/// so clicks can be mapped back to rows.
fn draw_sessions(r: &mut Renderer, theme: &Theme, b: &browser::Browser) -> (Rect, f32, f32, usize) {
    let (cw, ch) = r.cell();
    let (win_w, win_h) = r.size();
    let lh = (ch * 1.45).round();
    let w = (cw * 110.0).min(win_w - 4.0 * cw);
    let h = (win_h * 0.8).min(lh * 22.0);
    let (x, y) = (((win_w - w) / 2.0).round(), ((win_h - h) / 2.0).round());
    r.rect(0.0, 0.0, win_w, win_h, [0.0, 0.0, 0.0, 0.45]);
    r.rect(x, y, w, h, rgba(theme.tab_bar, 0.98));
    r.rect(x, y, w, r.scale.max(1.0), rgba(theme.palette[3], 1.0));
    let pad = 2.0 * cw;
    let dy = ((lh - ch) / 2.0).round();
    let items = b.visible();
    r.text("Sessions", x + pad, y + dy + lh * 0.2, x + w - pad, rgba(theme.fg, 1.0));
    let count = format!("{} · ⏎ resume · esc", items.len());
    let cx = x + w - pad - count.chars().count() as f32 * cw;
    r.text(&count, cx, y + dy + lh * 0.2, x + w - pad, rgba(theme.fg, 0.5));
    let search = format!("Search: {}▏", b.query);
    r.text(&search, x + pad, y + dy + lh * 1.3, x + w - pad, rgba(theme.fg, 0.9));

    let y0 = y + lh * 2.6;
    let rows = (((y + h - lh * 0.4) - y0) / lh).floor().max(1.0) as usize;
    let first = b.selected.saturating_sub(rows - 1);
    let right_cols = 44.0;
    if items.is_empty() {
        r.text("No Claude Code or Codex sessions found", x + pad, y0 + dy, x + w - pad, rgba(theme.fg, 0.5));
    }
    for (i, s) in items.iter().enumerate().skip(first).take(rows) {
        let ry = y0 + (i - first) as f32 * lh;
        let selected = i == b.selected;
        if selected {
            r.rect(x + cw, ry, w - 2.0 * cw, lh, rgba(theme.tab_active, 1.0));
        }
        let a = if selected { 1.0 } else { 0.75 };
        if s.is_live() {
            r.text("●", x + pad, ry + dy, x + w, rgba(theme.palette[3], 1.0));
        }
        let tool = format!("{:<7}", s.tool.name());
        r.text(&tool, x + pad + 2.0 * cw, ry + dy, x + w, rgba(theme.palette[4], a));
        let right = format!("{} · {}{}", browser::tilde(&s.cwd), if s.branch.is_empty() { String::new() } else { format!("{} · ", s.branch) }, browser::ago(s.modified));
        let right: String = right.chars().rev().take(right_cols as usize).collect::<Vec<_>>().into_iter().rev().collect();
        let rx = x + w - pad - right.chars().count() as f32 * cw;
        r.text(&s.title, x + pad + 10.0 * cw, ry + dy, rx - 2.0 * cw, rgba(theme.fg, a));
        r.text(&right, rx, ry + dy, x + w - pad, rgba(theme.fg, 0.5));
    }
    (Rect { x, y, w, h }, y0, lh, first)
}

/// Paste into a pane the way ⌘V does (bracketed when the app asked for it, so a shell
/// never runs pasted text by itself).
fn paste_into(pane: &Pane, text: &str) {
    let text = text.replace("\r\n", "\r").replace('\n', "\r");
    let bracketed = pane.term.lock().mode().contains(TermMode::BRACKETED_PASTE);
    if bracketed {
        pane.write(format!("\x1b[200~{}\x1b[201~", text.replace('\x1b', "")).as_bytes());
    } else {
        pane.write(text.as_bytes());
    }
}

/// Tell the UI once an agent that just started has drawn its UI and gone quiet (ready for
/// input), or after 20 s regardless.
fn wait_until_settled(id: PaneId, last_output: Arc<AtomicU64>, proxy: EventLoopProxy<UserEvent>) {
    let _ = std::thread::Builder::new().name("agent-wait".into()).stack_size(64 * 1024).spawn(move || {
        let start = Instant::now();
        loop {
            std::thread::sleep(Duration::from_millis(200));
            let last = last_output.load(Ordering::Acquire);
            let quiet = last > 0 && pane::now_ms().saturating_sub(last) > 1200 && start.elapsed() > Duration::from_secs(2);
            if quiet || start.elapsed() > Duration::from_secs(20) {
                let _ = proxy.send_event(UserEvent::AgentReady(id));
                return;
            }
        }
    });
}

/// Color for a `/context` category.
fn category_color(theme: &Theme, name: &str) -> [f32; 3] {
    let n = name.to_lowercase();
    let i = if n.starts_with("system prompt") {
        4
    } else if n.starts_with("system tools") {
        12
    } else if n.starts_with("mcp") {
        6
    } else if n.starts_with("memory") {
        2
    } else if n.starts_with("skills") {
        5
    } else if n.contains("agent") {
        13
    } else if n.starts_with("messages") {
        3
    } else if n.starts_with("autocompact") {
        1
    } else {
        8
    };
    theme.palette[i]
}

/// The context bar under the panes: how full the Claude Code session's context window is,
/// split by category, with the full breakdown in a panel while the mouse is over it.
fn draw_context_bar(r: &mut Renderer, theme: &Theme, used: Option<&(u64, String)>, b: Option<&context::Breakdown>, hover: bool) {
    let (cw, ch) = r.cell();
    let (win_w, win_h) = r.size();
    let h = (ch * 1.5).round();
    let y = win_h - h;
    let ty = y + ((h - ch) / 2.0).round();
    // Before the first reply the context is just the overhead /context reports.
    let tokens = used.map(|u| u.0).or(b.map(|b| b.overhead())).unwrap_or(0);
    let window = b.map(|b| b.window).unwrap_or(if tokens > 200_000 { 1_000_000 } else { 200_000 });
    let buffer = b.map_or(0, |b| b.buffer());
    let usable = window.saturating_sub(buffer).max(1);
    let full = tokens as f32 / usable as f32;
    r.rect(0.0, y, win_w, h, rgba(theme.tab_bar, 0.97));
    r.rect(0.0, y, win_w, r.scale.max(1.0), rgba(theme.fg, 0.08));

    let amber = theme.palette[3];
    r.text("✻ Context", cw, ty, win_w, rgba(amber, 1.0));
    let mut right = format!("{} / {}  {:.0}%", context::short(tokens), context::short(window), tokens as f32 / window as f32 * 100.0);
    if buffer > 0 {
        right += &format!(" · {:.0}% to compact", ((1.0 - full) * 100.0).max(0.0));
    }
    let rx = win_w - cw - right.chars().count() as f32 * cw;
    let color = if full > 0.85 { theme.palette[1] } else if full > 0.6 { amber } else { theme.fg };
    r.text(&right, rx, ty, win_w, rgba(color, if full > 0.6 { 1.0 } else { 0.7 }));

    // The bar: one segment per category, the autocompact buffer reserved at the right end.
    let (x0, x1) = (11.0 * cw, rx - 2.0 * cw);
    if x1 > x0 + 4.0 * cw {
        let bh = (ch * 0.45).round();
        let by = y + ((h - bh) / 2.0).round();
        let px = |t: u64| (x1 - x0) * t as f32 / window as f32;
        r.rect(x0, by, x1 - x0, bh, rgba(theme.fg, 0.08));
        let mut x = x0;
        let segments = match b {
            Some(b) => b.rows(tokens).into_iter().filter(|(n, _)| context::Breakdown::is_loaded(n) && !n.starts_with("Free") && !n.starts_with("Autocompact")).collect(),
            None => vec![(context::MESSAGES.to_string(), tokens)],
        };
        for (name, t) in segments {
            let w = px(t).min(x1 - x);
            r.rect(x, by, w, bh, rgba(category_color(theme, &name), 1.0));
            x += w;
        }
        if buffer > 0 {
            let w = px(buffer);
            r.rect(x1 - w, by, w, bh, rgba(theme.palette[1], 0.3));
        }
    }

    if !hover {
        return;
    }
    let lh = (ch * 1.35).round();
    let rows = b.map(|b| b.rows(tokens)).unwrap_or_default();
    let w = (cw * 46.0).min(win_w - 2.0 * cw);
    let ph = lh * (rows.len().max(1) as f32 + 3.0);
    let (px0, py0) = (win_w - w - cw, y - ph - 4.0 * r.scale);
    r.rect(px0, py0, w, ph, rgba(theme.tab_bar, 0.98));
    r.rect(px0, py0, w, r.scale.max(1.0), rgba(amber, 1.0));
    let pad = 2.0 * cw;
    let dy = ((lh - ch) / 2.0).round();
    let model = used.map(|u| u.1.as_str()).filter(|m| !m.is_empty()).or(b.map(|b| b.model.as_str())).unwrap_or("Claude Code");
    r.text(model, px0 + pad, py0 + lh * 0.3 + dy, px0 + w - pad, rgba(theme.fg, 1.0));
    let ry0 = py0 + lh * 1.5;
    if rows.is_empty() {
        r.text("Reading /context…", px0 + pad, ry0 + dy, px0 + w - pad, rgba(theme.fg, 0.55));
    }
    let (tok_x, pct_x) = (px0 + w - pad - 15.0 * cw, px0 + w - pad - 6.0 * cw);
    for (i, (name, t)) in rows.iter().enumerate() {
        let ry = ry0 + i as f32 * lh + dy;
        let loaded = context::Breakdown::is_loaded(name);
        let a = if loaded { 0.9 } else { 0.45 };
        if loaded && !name.starts_with("Free") {
            let alpha = if name.starts_with("Autocompact") { 0.3 } else { 1.0 };
            r.rect(px0 + pad, ry + ch * 0.25, cw * 0.8, ch * 0.5, rgba(category_color(theme, name), alpha));
        }
        r.text(name, px0 + pad + 2.0 * cw, ry, tok_x - cw, rgba(theme.fg, a));
        let tok = context::short(*t);
        let pct = format!("{:.1}%", *t as f32 / window as f32 * 100.0);
        r.text(&tok, pct_x - cw - tok.chars().count() as f32 * cw, ry, pct_x, rgba(theme.fg, a));
        r.text(&pct, px0 + w - pad - pct.chars().count() as f32 * cw, ry, px0 + w, rgba(theme.fg, a * 0.7));
    }
    let note = "estimated · deferred tools load on demand";
    r.text(note, px0 + pad, py0 + ph - lh + dy - lh * 0.2, px0 + w - pad, rgba(theme.fg, 0.4));
}

/// The Ask AI bar at the bottom of the pane.
fn draw_ask_bar(r: &mut Renderer, theme: &Theme, ask: &Ask, g: &PaneGeom, now: u64, command: &str) {
    let (cw, ch) = r.cell();
    let h = (ch * 1.5).round();
    let y = g.rect.y + g.rect.h - h;
    let amber = theme.palette[3];
    r.rect(g.rect.x, y, g.rect.w, h, rgba(theme.tab_bar, 0.97));
    r.rect(g.rect.x, y, g.rect.w, r.scale.max(1.0), rgba(amber, 1.0));
    let ty = y + ((h - ch) / 2.0).round();
    let (x0, x1) = (g.rect.x + cw, g.rect.x + g.rect.w - cw);
    const ROSETTE: [&str; 4] = ["✻", "✼", "✽", "❋"];
    let icon = if ask.busy { ROSETTE[(now / 160 % 4) as usize] } else { "✻" };
    r.text(icon, x0, ty, x1, rgba(amber, 1.0));
    let text = if ask.query.is_empty() && !ask.busy { "Ask AI: describe a command…".to_string() } else { format!("Ask AI: {}{}", ask.query, if ask.busy { "" } else { "▏" }) };
    let end = r.text_end(&text, x0 + 2.0 * cw, x1);
    r.text(&text, x0 + 2.0 * cw, ty, x1, rgba(theme.fg, if ask.query.is_empty() && !ask.busy { 0.5 } else { 1.0 }));
    let program = command.split_whitespace().next().unwrap_or("agent");
    let (status, color, alpha) = match &ask.error {
        Some(e) => (e.clone(), theme.palette[1], 0.9),
        None if ask.busy => (format!("asking {program}… · esc cancel"), theme.fg, 0.55),
        None => ("⏎ type it at the prompt (not run) · esc".into(), theme.fg, 0.55),
    };
    r.text(&status, end + 2.0 * cw, ty, x1, rgba(color, alpha));
}

/// The command palette. Returns (panel rect, first row y, row height, first index).
fn draw_palette(r: &mut Renderer, theme: &Theme, p: &palette::Palette) -> (Rect, f32, f32, usize) {
    let (cw, ch) = r.cell();
    let (win_w, win_h) = r.size();
    let lh = (ch * 1.45).round();
    let w = (cw * 72.0).min(win_w - 4.0 * cw);
    let h = (win_h * 0.7).min(lh * 16.0);
    let (x, y) = (((win_w - w) / 2.0).round(), (win_h * 0.12).round());
    r.rect(0.0, 0.0, win_w, win_h, [0.0, 0.0, 0.0, 0.35]);
    r.rect(x, y, w, h, rgba(theme.tab_bar, 0.98));
    r.rect(x, y, w, r.scale.max(1.0), rgba(theme.palette[4], 1.0));
    let pad = 2.0 * cw;
    let dy = ((lh - ch) / 2.0).round();
    let query = if p.query.is_empty() { "Type a command, theme or font…".to_string() } else { format!("{}▏", p.query) };
    r.text("›", x + pad, y + dy + lh * 0.3, x + w, rgba(theme.palette[4], 1.0));
    r.text(&query, x + pad + 2.0 * cw, y + dy + lh * 0.3, x + w - pad, rgba(theme.fg, if p.query.is_empty() { 0.5 } else { 1.0 }));
    let items = p.visible();
    let y0 = y + lh * 1.6;
    let rows = (((y + h - lh * 0.3) - y0) / lh).floor().max(1.0) as usize;
    let first = p.selected.saturating_sub(rows - 1);
    if items.is_empty() {
        r.text("No matches", x + pad, y0 + dy, x + w - pad, rgba(theme.fg, 0.5));
    }
    for (i, e) in items.iter().enumerate().skip(first).take(rows) {
        let ry = y0 + (i - first) as f32 * lh;
        let selected = i == p.selected;
        if selected {
            r.rect(x + cw, ry, w - 2.0 * cw, lh, rgba(theme.tab_active, 1.0));
        }
        let kx = x + w - pad - e.keys.chars().count() as f32 * cw;
        r.text(&e.label, x + pad, ry + dy, kx - cw, rgba(theme.fg, if selected { 1.0 } else { 0.8 }));
        r.text(e.keys, kx, ry + dy, x + w - pad, rgba(theme.palette[3], if selected { 1.0 } else { 0.7 }));
    }
    (Rect { x, y, w, h }, y0, lh, first)
}

/// Native notification. shortcut: shells out to osascript/notify-send instead of linking a
/// notification framework; the macOS notification is attributed to Script Editor.
fn desktop_notification(title: &str, body: &str) {
    let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    let result = if cfg!(target_os = "macos") {
        std::process::Command::new("osascript")
            .arg("-e")
            .arg(format!("display notification \"{}\" with title \"Stećak — {}\"", esc(body), esc(title)))
            .spawn()
    } else if cfg!(target_os = "linux") {
        std::process::Command::new("notify-send").arg(format!("Stećak — {title}")).arg(body).spawn()
    } else {
        return;
    };
    if let Err(e) = result {
        log::warn!("notification failed: {e}");
    }
}

fn draw_settings(r: &mut Renderer, theme: &Theme, settings: &Settings, cfg: &Config) {
    let (cw, ch) = r.cell();
    let (win_w, win_h) = r.size();
    let rows: Vec<_> = settings.rows(cfg).collect();
    let line_h = (ch * 1.35).round();
    let w = (cw * 72.0).min(win_w - 4.0 * cw);
    let h = line_h * (rows.len() as f32 + 4.0);
    let (x, y) = (((win_w - w) / 2.0).round(), ((win_h - h) / 2.0).round().max(0.0));
    // Dim everything behind the panel.
    r.rect(0.0, 0.0, win_w, win_h, [0.0, 0.0, 0.0, 0.45]);
    r.rect(x, y, w, h, rgba(theme.tab_bar, 0.98));
    r.rect(x, y, w, r.scale.max(1.0), rgba(theme.palette[4], 1.0));
    let pad = 2.0 * cw;
    let text_dy = ((line_h - ch) / 2.0).round();
    r.text("Settings", x + pad, y + text_dy + line_h * 0.3, x + w - pad, rgba(theme.fg, 1.0));
    let value_x = x + pad + 30.0 * cw;
    for (i, (label, value, selected)) in rows.into_iter().enumerate() {
        let ry = y + line_h * (i as f32 + 1.5);
        if selected {
            r.rect(x + cw, ry, w - 2.0 * cw, line_h, rgba(theme.tab_active, 1.0));
        }
        let a = if selected { 1.0 } else { 0.75 };
        r.text(label, x + pad, ry + text_dy, value_x - cw, rgba(theme.fg, a));
        let shown = if selected && !value.is_empty() && settings.editing.is_none() { format!("‹ {value} ›") } else { value };
        r.text(&shown, value_x, ry + text_dy, x + w - pad, rgba(if selected { theme.palette[4] } else { theme.fg }, a));
    }
    let hint = "↑↓ select  ←→ change  ⏎ edit  esc close  · drop an image here";
    r.text(hint, x + pad, y + h - line_h + text_dy, x + w - pad, rgba(theme.fg, 0.5));
}

impl ApplicationHandler<UserEvent> for App {
    fn new_events(&mut self, _event_loop: &ActiveEventLoop, cause: StartCause) {
        if matches!(cause, StartCause::ResumeTimeReached { .. }) {
            self.mark_dirty();
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // ~15 fps tab animation only while an agent works; otherwise sleep until an event.
        event_loop.set_control_flow(if self.animating && !self.occluded {
            ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(66))
        } else {
            ControlFlow::Wait
        });
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        self.save_session();
        self.flush_stats();
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("Stećak")
            .with_transparent(true)
            // Hidden until sized and centered (see below), so it never flashes at the wrong spot.
            .with_visible(false)
            .with_inner_size(winit::dpi::LogicalSize::new(900.0, 560.0));
        // macOS: draw under a transparent titlebar so we paint it ourselves (see titlebar_h).
        #[cfg(target_os = "macos")]
        let attrs = {
            use winit::platform::macos::WindowAttributesExtMacOS;
            attrs.with_titlebar_transparent(true).with_fullsize_content_view(true)
        };
        // Windows: title bar / taskbar / Alt+Tab icon, from the resource build.rs embeds.
        // No redirection bitmap: the GPU draws through DirectComposition, and the legacy GDI
        // surface would otherwise sit under it as an opaque white layer, so opacity blended
        // the theme with white instead of the desktop (Windows Terminal sets the same flag).
        #[cfg(target_os = "windows")]
        let attrs = {
            use winit::platform::windows::{IconExtWindows, WindowAttributesExtWindows};
            attrs.with_window_icon(winit::window::Icon::from_resource(1, None).ok()).with_no_redirection_bitmap(true)
        };
        // Scripted test runs: float above other windows so macOS keeps drawing us, but never
        // take keyboard focus away from what the user is typing into.
        let attrs = if std::env::var_os("STECAK_DEMO").is_some() {
            attrs.with_window_level(winit::window::WindowLevel::AlwaysOnTop).with_active(false)
        } else {
            attrs
        };
        let window = Arc::new(event_loop.create_window(attrs).expect("create window"));
        window.set_cursor(CursorIcon::Text);
        window.set_ime_allowed(true);
        #[cfg(target_os = "macos")]
        {
            self._menu = Some(menu::install(self.proxy.clone()));
        }

        if self.config.window.blur {
            apply_blur(&window, 20, theme::luma(self.theme.bg) < 0.5);
        }

        // Start the first tab's shell now, in parallel with GPU setup. Not when a saved
        // session will be restored instead. It becomes pane `next_id` (the first spawn).
        if !(self.restoring_enabled() && restore::load(self.config_dir()).is_some()) {
            let (id, cfg, proxy, wakeup) = (self.next_id, self.shell_config(), self.proxy.clone(), self.wakeup_pending.clone());
            // Placeholder size; resize_all() fixes it once the tab exists.
            let spawn = move || Pane::spawn(id, &cfg, GridSize { cols: 80, rows: 24 }, (8, 16), None, None, proxy, wakeup);
            self.prespawned = std::thread::Builder::new().name("prespawn".into()).spawn(spawn).ok();
        }
        let renderer = Renderer::new(window.clone(), event_loop.owned_display_handle(), &self.config);
        if !renderer.transparent {
            log::warn!("this GPU/compositor does not support a transparent swapchain; opacity is ignored");
        }
        self.window = Some(window.clone());
        self.renderer = Some(renderer);

        // Size the window to the configured grid.
        let (cw, ch) = self.r().cell();
        let pad = self.config.window.padding * window.scale_factor() as f32;
        let tab_bar = self.tab_bar_h();
        let w = self.config.window.columns as f32 * cw + 2.0 * pad;
        let h = self.config.window.rows as f32 * ch + 2.0 * pad + tab_bar;
        let monitor = window.current_monitor();
        // Never larger than the screen, or the titlebar ends up off-screen.
        let (w, h) = match &monitor {
            Some(m) => (w.min(m.size().width as f32 * 0.9), h.min(m.size().height as f32 * 0.85)),
            None => (w, h),
        };
        if let Some(size) = window.request_inner_size(PhysicalSize::new(w as u32, h as u32)) {
            self.renderer.as_mut().unwrap().resize(size.width, size.height);
        }
        // macOS grows windows upward from their bottom-left corner, which pushed the titlebar
        // under the menu bar on short screens. Center on the window's own monitor instead.
        if let Some(m) = monitor {
            let (mp, ms, os) = (m.position(), m.size(), window.outer_size());
            let x = mp.x + (ms.width as i32 - os.width as i32).max(0) / 2;
            let y = mp.y + (ms.height as i32 - os.height as i32).max(0) / 2;
            window.set_outer_position(winit::dpi::PhysicalPosition::new(x, y));
        }
        window.set_visible(true);

        let restored = self.restoring_enabled() && self.restore_session();
        if !restored {
            self.new_tab();
        }
        if self.config.welcome && !restored {
            if let (Some(g), Some(id)) = (self.geometry(self.active).0.first(), self.focused_id()) {
                let seed = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs());
                if let Some(p) = self.panes.get(&id) {
                    p.inject(welcome::banner(g.cols, seed).as_bytes());
                }
            }
        }
        self.load_background();
        if self.config.check_for_updates && std::env::var_os("STECAK_DEMO").is_none() {
            update::check_async(self.proxy.clone(), false);
        }

        // STECAK_DEMO="split-h;find:foo;…" replays UI actions for automated visual tests,
        // without synthesizing OS-level keystrokes.
        if let Ok(script) = std::env::var("STECAK_DEMO") {
            let proxy = self.proxy.clone();
            let _ = std::thread::Builder::new().name("demo".into()).spawn(move || {
                for step in script.split(';').map(str::trim).filter(|s| !s.is_empty()) {
                    std::thread::sleep(Duration::from_millis(400));
                    let _ = proxy.send_event(UserEvent::Demo(step.to_string()));
                }
            });
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Wakeup => {
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            UserEvent::Exited(id) => self.close_pane(id, event_loop),
            UserEvent::ConfigChanged => match config::load_from(&self.config_path) {
                Ok(cfg) if cfg != self.config => {
                    log::info!("config reloaded");
                    self.apply_config(cfg);
                }
                Ok(_) => {}
                Err(e) => log::error!("config error (keeping previous config): {e}"),
            },
            UserEvent::Notify(id, msg) => self.on_notify(id, msg),
            UserEvent::UpdateAvailable(release) => {
                self.update = Some(UpdateUi::Available(release));
                self.mark_dirty();
            }
            UserEvent::Menu(id) => self.menu_action(&id, event_loop),
            UserEvent::UpdateNone(err) => {
                log::info!("update check: {}", err.as_deref().unwrap_or("up to date"));
                self.update = Some(match err {
                    None => UpdateUi::UpToDate,
                    Some(e) => UpdateUi::Failed(e, "https://github.com/alminisl/stecak/releases/latest".into()),
                });
                self.mark_dirty();
            }
            UserEvent::UpdateInstalled(result) => {
                self.update = Some(match result {
                    Ok(app) => UpdateUi::Ready(app),
                    Err(e) => UpdateUi::Failed(e, "https://github.com/alminisl/stecak/releases/latest".into()),
                });
                self.mark_dirty();
            }
            UserEvent::Demo(step) => self.demo_step(&step, event_loop),
            UserEvent::AiReply(pane, generation, reply) => self.on_ai_reply(pane, generation, reply),
            UserEvent::AgentReady(id) => {
                if let Some((text, submit)) = self.pending_agent.remove(&id) {
                    self.deliver(id, &text, submit);
                }
            }
            UserEvent::Submit(id) => {
                if let Some(p) = self.panes.get(&id) {
                    p.write(b"\r");
                }
            }

            UserEvent::ContextBreakdown(cwd, breakdown) => {
                // On failure keep the previous breakdown (if any); retried after 10 minutes.
                let entry = self.breakdowns.entry(cwd).or_insert((Instant::now(), None));
                entry.0 = Instant::now();
                if breakdown.is_some() {
                    entry.1 = breakdown;
                }
                self.mark_dirty();
            }
            UserEvent::BackgroundImage(img) => {
                self.renderer.as_mut().unwrap().set_background_image(img);
                self.mark_dirty();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                self.save_session();
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                if let Some(r) = self.renderer.as_mut() {
                    r.resize(size.width, size.height);
                    self.resize_all();
                    if self.config.background_image.resolved_path().is_some() {
                        self.load_background();
                    }
                }
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                let cfg = self.config.clone();
                if let Some(r) = self.renderer.as_mut() {
                    r.reload_fonts(&cfg, scale_factor as f32);
                }
                self.resize_all();
            }
            WindowEvent::Focused(f) => {
                self.focused = f;
                // Apps that asked for focus events (vim, tmux) get CSI I / CSI O.
                if let Some(p) = self.focused_pane() {
                    if p.term.lock().mode().contains(TermMode::FOCUS_IN_OUT) {
                        p.write(if f { b"\x1b[I" } else { b"\x1b[O" });
                    }
                }
                self.mark_dirty();
            }
            WindowEvent::ModifiersChanged(m) => {
                self.modifiers = m.state();
                self.update_hover();
            }
            // While an IME is composing, keys belong to it; the result arrives as Ime::Commit.
            WindowEvent::KeyboardInput { .. } if !self.preedit.is_empty() => {}
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                let app_mod = input::is_app_modifier(self.modifiers);
                if self.help_open && !app_mod {
                    self.help_open = false;
                    self.mark_dirty();
                    return;
                }
                // Update popup: ⏎ = its main action, esc = dismiss; every other key still types.
                if self.update.is_some() && !app_mod {
                    match event.logical_key {
                        Key::Named(NamedKey::Enter) => return self.update_action(true, event_loop),
                        Key::Named(NamedKey::Escape) => return self.update_action(false, event_loop),
                        _ => {}
                    }
                }
                if self.sessions.open && !app_mod && self.sessions_key(&event) {
                    return;
                }
                if self.palette.open && !app_mod && self.palette_key(&event, event_loop) {
                    return;
                }
                if self.ask.open && !app_mod && self.ask_key(&event) {
                    return;
                }
                if self.settings.open && !app_mod && self.settings_key(&event) {
                    return;
                }
                if self.search.open && !app_mod && self.search_key(&event) {
                    return;
                }
                let app_cursor = self.focused_pane().is_some_and(|p| p.term.lock().mode().contains(TermMode::APP_CURSOR));
                if let Some(action) = input::translate(&event, self.modifiers, app_cursor, self.config.option_as_alt) {
                    self.handle_action(action, event_loop);
                }
            }
            WindowEvent::Ime(Ime::Preedit(text, _)) => {
                self.preedit = text;
                self.mark_dirty();
            }
            WindowEvent::Ime(Ime::Commit(text)) => {
                self.preedit.clear();
                if self.ask.open && !self.ask.busy {
                    self.ask.query.push_str(&text);
                    self.mark_dirty();
                } else if self.search.open {
                    let q = format!("{}{text}", self.search.query);
                    if let Some(p) = self.panes.get(&self.search.pane) {
                        self.search.set_query(q, &mut p.term.lock());
                    }
                    self.search_gen += 1;
                    self.mark_dirty();
                } else {
                    self.handle_action(Action::Write(text.into_bytes()), event_loop);
                }
            }
            WindowEvent::Ime(_) => {}
            WindowEvent::DroppedFile(path) => {
                let is_image = path.extension().and_then(|e| e.to_str()).is_some_and(|e| ["png", "jpg", "jpeg", "webp"].contains(&e.to_ascii_lowercase().as_str()));
                if self.settings.open && is_image {
                    let mut cfg = self.config.clone();
                    cfg.background_image.path = path.display().to_string();
                    self.apply_and_save(cfg);
                } else {
                    // Like other terminals: dropping a file types its (quoted) path.
                    self.paste_text(&format!("'{}' ", path.display().to_string().replace('\'', "'\\''")));
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.mouse = (position.x as f32, position.y as f32);
                self.on_motion();
            }
            WindowEvent::MouseInput { state: ElementState::Pressed, button, .. } => self.on_press(button, event_loop),
            WindowEvent::MouseInput { state: ElementState::Released, button, .. } => self.on_release(button),
            WindowEvent::MouseWheel { delta, .. } => self.on_scroll(delta),
            // Hidden/minimized windows don't render at all; the terminal state still updates.
            WindowEvent::Occluded(o) => {
                log::debug!("occluded: {o}");
                // Scripted test runs sit behind other windows on purpose but still need frames.
                self.occluded = o && std::env::var_os("STECAK_DEMO").is_none();
                self.mark_dirty();
            }
            WindowEvent::RedrawRequested if !self.occluded => self.draw(),
            _ => {}
        }
    }
}

/// Native background blur behind the transparent window, where the OS offers it. `dark`: the
/// theme background is dark (Windows picks the matching acrylic).
fn apply_blur(window: &Window, radius: i32, dark: bool) {
    #[cfg(target_os = "macos")]
    {
        // Same private window-server call Ghostty uses. An NSVisualEffectView would sit
        // *above* the GPU layer and hide the content.
        use objc2::msg_send;
        use objc2::runtime::AnyObject;
        use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
        #[link(name = "CoreGraphics", kind = "framework")]
        unsafe extern "C" {
            fn CGSMainConnectionID() -> i32;
            fn CGSSetWindowBackgroundBlurRadius(cid: i32, wid: isize, radius: i32) -> i32;
        }
        if let Ok(handle) = window.window_handle() {
            if let RawWindowHandle::AppKit(h) = handle.as_raw() {
                unsafe {
                    let view = h.ns_view.as_ptr() as *const AnyObject;
                    let ns_window: *const AnyObject = msg_send![&*view, window];
                    let number: isize = msg_send![&*ns_window, windowNumber];
                    CGSSetWindowBackgroundBlurRadius(CGSMainConnectionID(), number, radius);
                }
            }
        }
    }
    #[cfg(target_os = "windows")]
    {
        use window_vibrancy::{apply_acrylic, apply_blur as win_blur};
        use windows_sys::Win32::Graphics::Dwm::{DWMWA_USE_IMMERSIVE_DARK_MODE, DwmExtendFrameIntoClientArea, DwmSetWindowAttribute};
        use windows_sys::Win32::UI::Controls::MARGINS;
        use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
        // The Windows 11 acrylic system backdrop. It only covers the window frame unless the
        // frame is extended over the whole client area, behind our translucent background.
        if let Ok(RawWindowHandle::Win32(h)) = window.window_handle().map(|h| h.as_raw()) {
            let hwnd = h.hwnd.get() as windows_sys::Win32::Foundation::HWND;
            let dark = dark as i32;
            let all = MARGINS { cxLeftWidth: -1, cxRightWidth: -1, cyTopHeight: -1, cyBottomHeight: -1 };
            // SAFETY: `hwnd` is our live window; both calls only read the values passed in.
            unsafe {
                // Dark mode: dark title bar, and the dark acrylic (the light one reads as
                // near-opaque white frost behind a dark theme).
                DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE as u32, (&dark as *const i32).cast(), 4);
                DwmExtendFrameIntoClientArea(hwnd, &all);
            }
        }
        // Older Windows without system backdrops: DWM blur-behind instead.
        if apply_acrylic(window, None).is_err() {
            let _ = win_blur(window, None);
        }
    }
    // Linux: blur is a compositor feature (KDE/Hyprland/Picom rules); transparency alone works.
    let _ = (window, radius, dark);
}

/// Poll the config file's mtime; cheap and works everywhere (including editors that
/// replace the file on save, which trips up some inotify-style watchers).
fn watch_config(path: PathBuf, proxy: EventLoopProxy<UserEvent>) {
    let builder = std::thread::Builder::new().name("config-watch".into()).stack_size(64 * 1024);
    let _ = builder.spawn(move || {
        let mtime = |p: &PathBuf| std::fs::metadata(p).and_then(|m| m.modified()).ok();
        let mut last: Option<SystemTime> = mtime(&path);
        loop {
            std::thread::sleep(Duration::from_millis(500));
            let now = mtime(&path);
            if now != last {
                last = now;
                if now.is_some() && proxy.send_event(UserEvent::ConfigChanged).is_err() {
                    break;
                }
            }
        }
    });
}

/// Log to stderr, and on Windows (no console) to `stecak.log` next to the config, so a crash
/// leaves a reason behind. Panics are logged too: release builds abort on panic, silently.
fn init_logging(config_path: &std::path::Path) {
    let mut builder = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("stecak=info"));
    #[cfg(target_os = "windows")]
    if let Some(dir) = config_path.parent() {
        let _ = std::fs::create_dir_all(dir);
        if let Ok(file) = std::fs::File::create(dir.join("stecak.log")) {
            builder.target(env_logger::Target::Pipe(Box::new(file)));
        }
    }
    #[cfg(not(target_os = "windows"))]
    let _ = config_path;
    builder.init();
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log::error!("panic: {info}
{}", std::backtrace::Backtrace::force_capture());
        default_hook(info);
    }));
}

fn main() {
    let config_path = config::find_config_path().unwrap_or_else(config::default_config_path);
    init_logging(&config_path);
    let config = config::load();
    // `stecak -e <program> [args…]` runs a command instead of the login shell.
    let args: Vec<String> = std::env::args().collect();
    let cli_shell = args.iter().position(|a| a == "-e").and_then(|i| {
        let program = args.get(i + 1)?;
        Some(ShellConfig { program: program.clone(), args: args[i + 2..].to_vec() })
    });
    log::info!("config path: {}", config_path.display());
    // Resolve the shell's PATH now, so agent tabs and /context find `claude` without waiting.
    ai::warm_user_path();

    let mut builder = EventLoop::<UserEvent>::with_user_event();
    // Scripted test runs must not steal keyboard focus from whatever the user is doing.
    #[cfg(target_os = "macos")]
    if std::env::var_os("STECAK_DEMO").is_some() {
        use winit::platform::macos::EventLoopBuilderExtMacOS;
        builder.with_activate_ignoring_other_apps(false);
    }
    let event_loop = builder.build().expect("event loop");
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();
    watch_config(config_path.clone(), proxy.clone());

    let mut app = App::new(config, cli_shell, config_path, proxy);
    event_loop.run_app(&mut app).expect("event loop error");
}
