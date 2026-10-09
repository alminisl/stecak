//! Stećak: a lightweight GPU terminal emulator.
//!
//! - winit: cross-platform windowing (incl. transparent windows)
//! - wgpu: GPU rendering via Metal / DX12 / Vulkan
//! - alacritty_terminal: battle-tested VT parser + grid/scrollback/selection/search
//! - portable-pty: cross-platform PTY (ConPTY on Windows)
//! - swash: font shaping (ligatures) and rasterization

mod bgimage;
mod config;
mod draw;
mod input;
mod layout;
mod mouse;
mod pane;
mod renderer;
mod search;
mod settings;
mod text;
mod theme;

use std::collections::HashMap;
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
use winit::event::{ElementState, Ime, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{CursorIcon, Window, WindowId};

use config::{Config, ShellConfig};
use draw::{DrawStats, Highlights, PaneCache};
use input::Action;
use layout::{Dir, Divider, Node, Rect};
use pane::{GridSize, Pane, PaneId, UserEvent};
use renderer::Renderer;
use search::Search;
use settings::Settings;
use theme::{rgba, Theme};

struct Tab {
    root: Node,
    focus: PaneId,
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

    modifiers: ModifiersState,
    mouse: (f32, f32),
    drag: Option<Drag>,
    last_click: Option<(Instant, PaneId, Point, u8)>,
    hover_url: Option<(PaneId, i32, usize, usize, String)>,
    scroll_accum: f64,
    cursor_icon: CursorIcon,
    /// IME composition in progress (e.g. pinyin, or a dead key like ´ before e).
    preedit: String,
    focused: bool,
    occluded: bool,

    search: Search,
    search_gen: u64,
    settings: Settings,
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
            bg_gen: Arc::new(AtomicU64::new(0)),
            wakeup_pending: Arc::new(AtomicBool::new(false)),
            dirty: true,
            stats: Stats { enabled: std::env::var_os("STECAK_STATS").is_some(), ..Default::default() },
        }
    }

    fn r(&self) -> &Renderer {
        self.renderer.as_ref().unwrap()
    }

    fn tab_bar_h(&self) -> f32 {
        let show = self.config.tabs.always_show || self.tabs.len() > 1;
        if show { (self.r().cell().1 * 1.7).round() } else { 0.0 }
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
        t.root.layout(Rect { x: 0.0, y: top, w, h: h - top }, gap, &mut rects, &mut dividers);
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

    fn spawn_pane(&mut self) -> Option<PaneId> {
        let id = self.next_id;
        self.next_id += 1;
        // New tabs/splits start in the focused pane's directory.
        let cwd = self.focused_pane().and_then(|p| p.cwd());
        let size = GridSize { cols: 80, rows: 24 }; // corrected by resize_all() right after
        match Pane::spawn(id, &self.shell_config(), size, self.cell_px(), cwd.as_deref(), self.proxy.clone(), self.wakeup_pending.clone()) {
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
        if let Some(id) = self.spawn_pane() {
            self.tabs.push(Tab { root: Node::Leaf(id), focus: id });
            self.active = self.tabs.len() - 1;
            self.resize_all();
        }
    }

    fn split(&mut self, dir: Dir) {
        let Some(focus) = self.tabs.get(self.active).map(|t| t.focus) else { return };
        if let Some(id) = self.spawn_pane() {
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
        let Some(pane) = self.focused_pane() else { return };
        let text = text.replace("\r\n", "\r").replace('\n', "\r");
        let bracketed = pane.term.lock().mode().contains(TermMode::BRACKETED_PASTE);
        if bracketed {
            pane.write(format!("\x1b[200~{}\x1b[201~", text.replace('\x1b', "")).as_bytes());
        } else {
            pane.write(text.as_bytes());
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
            Action::Split(dir) => self.split(dir),
            Action::NextPane => self.cycle_pane(1),
            Action::PrevPane => self.cycle_pane(-1),
            Action::OpenSettings => {
                self.settings.toggle();
                self.mark_dirty();
            }
            Action::Copy => self.copy(),
            Action::Paste => {
                if let Ok(text) = arboard::Clipboard::new().and_then(|mut c| c.get_text()) {
                    self.paste_text(&text);
                }
            }
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
            settings::Outcome::None => {}
        }
        self.mark_dirty();
        true
    }

    fn demo_step(&mut self, step: &str, event_loop: &ActiveEventLoop) {
        let (cmd, arg) = step.split_once(':').unwrap_or((step, ""));
        match cmd {
            "split-h" => self.split(Dir::Horizontal),
            "split-v" => self.split(Dir::Vertical),
            "tab" => self.new_tab(),
            "pane" => self.cycle_pane(1),
            "type" => self.handle_action(Action::Write(arg.replace("\\n", "\r").into_bytes()), event_loop),
            "find" => {
                self.handle_action(Action::Find, event_loop);
                if let Some(p) = self.panes.get(&self.search.pane) {
                    self.search.set_query(arg.to_string(), &mut p.term.lock());
                }
                self.search_gen += 1;
            }
            "settings" => self.handle_action(Action::OpenSettings, event_loop),
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
                    let term = p.term.lock();
                    let line = Line(row as i32 - term.grid().display_offset() as i32);
                    if let Some((a, b, url)) = mouse::url_at(&term, line, col) {
                        hover = Some((g.id, line.0, a, b, url));
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
        if self.settings.open {
            self.settings.open = false;
            self.mark_dirty();
            return;
        }
        if y < self.tab_bar_h() {
            if button == MouseButton::Left {
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
            if let Some((_, _, _, _, url)) = self.hover_url.clone().filter(|h| h.0 == g.id) {
                mouse::open_url(&url);
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
        let plus_w = self.tab_bar_h();
        let tab_w = ((win_w - plus_w) / self.tabs.len().max(1) as f32).min(260.0 * self.r().scale);
        let idx = (x / tab_w) as usize;
        if idx < self.tabs.len() {
            if self.modifiers.alt_key() {
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
        // Output arriving from now on needs another frame.
        self.wakeup_pending.store(false, Ordering::Release);
        let (geoms, dividers) = self.geometry(self.active);
        let tab_bar_h = self.tab_bar_h();
        let focus = self.focused_id();
        let multi = geoms.len() > 1;
        let image = self.config.background_image.clone();

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

        // Tab bar.
        if tab_bar_h > 0.0 {
            r.rect(0.0, 0.0, win_w, tab_bar_h, rgba(theme.tab_bar, opacity.max(0.6)));
            let plus_w = tab_bar_h;
            let tab_w = ((win_w - plus_w) / tabs.len().max(1) as f32).min(260.0 * r.scale);
            let text_y = ((tab_bar_h - ch) / 2.0).round();
            for (i, tab) in tabs.iter().enumerate() {
                let x = i as f32 * tab_w;
                let active = i == self.active;
                if active {
                    r.rect(x, 0.0, tab_w, tab_bar_h, rgba(theme.tab_active, 1.0));
                    r.rect(x, tab_bar_h - 2.0 * r.scale, tab_w, 2.0 * r.scale, rgba(theme.palette[4], 1.0));
                }
                r.rect(x + tab_w - r.scale, tab_bar_h * 0.25, r.scale, tab_bar_h * 0.5, rgba(theme.fg, 0.15));
                let mut n = vec![];
                tab.root.leaves(&mut n);
                let title = panes.get(&tab.focus).map(|p| p.display_title()).unwrap_or_default();
                let label = if n.len() > 1 { format!("{}  {}  [{}]", i + 1, title, n.len()) } else { format!("{}  {}", i + 1, title) };
                r.text(&label, x + cw, text_y, x + tab_w - cw, rgba(theme.fg, if active { 1.0 } else { 0.55 }));
            }
            let plus_x = tab_w * tabs.len() as f32;
            r.text("+", plus_x + (plus_w - cw) / 2.0, text_y, plus_x + plus_w, rgba(theme.fg, 0.7));
        }

        if self.settings.open {
            self.settings.bosancica_font_ok = r.has_bosancica();
            draw_settings(r, theme, &self.settings, config);
        }

        let t1 = Instant::now();
        if !r.present(clear) {
            // The OS had no drawable for us (launch animation, resize, occlusion): this frame
            // never reached the screen, so the next one must not be skipped.
            self.mark_dirty();
            return;
        }
        self.dirty = false;

        if let (Some(w), Some(p)) = (&self.window, focus.and_then(|f| self.panes.get(&f))) {
            w.set_title(&format!("{} — Stećak", p.display_title()));
        }
        self.record_stats(t1 - t0, t1.elapsed(), stats);
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
    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        self.flush_stats();
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("Stećak")
            .with_transparent(true)
            .with_inner_size(winit::dpi::LogicalSize::new(900.0, 560.0));
        let window = Arc::new(event_loop.create_window(attrs).expect("create window"));
        window.set_cursor(CursorIcon::Text);
        window.set_ime_allowed(true);

        if self.config.window.blur {
            apply_blur(&window, 20);
        }

        let renderer = Renderer::new(window.clone(), Box::new(event_loop.owned_display_handle()), &self.config);
        if !renderer.transparent {
            log::warn!("this GPU/compositor does not support a transparent swapchain; opacity is ignored");
        }
        self.window = Some(window.clone());
        self.renderer = Some(renderer);

        // Size the window to the configured grid.
        let (cw, ch) = self.r().cell();
        let pad = self.config.window.padding * window.scale_factor() as f32;
        let tab_bar = if self.config.tabs.always_show { (ch * 1.7).round() } else { 0.0 };
        let w = self.config.window.columns as f32 * cw + 2.0 * pad;
        let h = self.config.window.rows as f32 * ch + 2.0 * pad + tab_bar;
        if let Some(size) = window.request_inner_size(PhysicalSize::new(w as u32, h as u32)) {
            self.renderer.as_mut().unwrap().resize(size.width, size.height);
        }

        self.new_tab();
        self.load_background();

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
            UserEvent::Demo(step) => self.demo_step(&step, event_loop),
            UserEvent::BackgroundImage(img) => {
                self.renderer.as_mut().unwrap().set_background_image(img);
                self.mark_dirty();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
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
                if self.search.open {
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
                self.occluded = o;
                self.mark_dirty();
            }
            WindowEvent::RedrawRequested if !self.occluded => self.draw(),
            _ => {}
        }
    }
}

/// Native background blur behind the transparent window, where the OS offers it.
fn apply_blur(window: &Window, radius: i32) {
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
        if apply_acrylic(window, None).is_err() {
            let _ = win_blur(window, None);
        }
    }
    // Linux: blur is a compositor feature (KDE/Hyprland/Picom rules); transparency alone works.
    let _ = (window, radius);
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

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("stecak=info")).init();

    let config_path = config::find_config_path().unwrap_or_else(config::default_config_path);
    let config = config::load();
    // `stecak -e <program> [args…]` runs a command instead of the login shell.
    let args: Vec<String> = std::env::args().collect();
    let cli_shell = args.iter().position(|a| a == "-e").and_then(|i| {
        let program = args.get(i + 1)?;
        Some(ShellConfig { program: program.clone(), args: args[i + 2..].to_vec() })
    });
    log::info!("config path: {}", config_path.display());

    let event_loop = EventLoop::<UserEvent>::with_user_event().build().expect("event loop");
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();
    watch_config(config_path.clone(), proxy.clone());

    let mut app = App::new(config, cli_shell, config_path, proxy);
    event_loop.run_app(&mut app).expect("event loop error");
}
