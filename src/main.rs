//! Lumen: a proof-of-concept GPU terminal emulator.
//!
//! - winit: cross-platform windowing (incl. transparent windows)
//! - wgpu: GPU rendering via Metal / DX12 / Vulkan
//! - alacritty_terminal: battle-tested VT parser + grid/scrollback
//! - portable-pty: cross-platform PTY (ConPTY on Windows)

mod config;
mod input;
mod renderer;
mod tab;
mod text;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::Point;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::TermMode;
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor};
use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::ModifiersState;
use winit::window::{Window, WindowId};

use config::{hex_or, Config};
use input::Action;
use renderer::Renderer;
use tab::{GridSize, Tab, UserEvent};

/// Resolved colors for the current theme.
struct Theme {
    palette: [[f32; 3]; 256],
    fg: [f32; 3],
    bg: [f32; 3],
    cursor: [f32; 3],
    tab_bar: [f32; 3],
    tab_active: [f32; 3],
}

impl Theme {
    fn from_config(cfg: &Config) -> Theme {
        let mut palette = [[0.0; 3]; 256];
        let defaults = config::ColorConfig::default().palette;
        for i in 0..16 {
            let fallback = hex_or(&defaults[i], [0.5; 3]);
            palette[i] = cfg.colors.palette.get(i).map(|s| hex_or(s, fallback)).unwrap_or(fallback);
        }
        // xterm 6x6x6 color cube + 24-step grayscale ramp.
        let steps = [0.0, 95.0, 135.0, 175.0, 215.0, 255.0];
        for i in 0..216 {
            palette[16 + i] = [steps[i / 36] / 255.0, steps[(i / 6) % 6] / 255.0, steps[i % 6] / 255.0];
        }
        for i in 0..24 {
            let v = (8.0 + i as f32 * 10.0) / 255.0;
            palette[232 + i] = [v; 3];
        }
        Theme {
            palette,
            fg: hex_or(&cfg.colors.foreground, [0.8; 3]),
            bg: hex_or(&cfg.colors.background, [0.1; 3]),
            cursor: hex_or(&cfg.colors.cursor, [0.9; 3]),
            tab_bar: hex_or(&cfg.colors.tab_bar, [0.08; 3]),
            tab_active: hex_or(&cfg.colors.tab_active, [0.2; 3]),
        }
    }

    fn resolve(&self, color: Color, overrides: &Colors) -> [f32; 3] {
        let rgb = |c: alacritty_terminal::vte::ansi::Rgb| [c.r as f32 / 255.0, c.g as f32 / 255.0, c.b as f32 / 255.0];
        match color {
            Color::Spec(c) => rgb(c),
            Color::Indexed(i) => overrides[i as usize].map(rgb).unwrap_or(self.palette[i as usize]),
            Color::Named(n) => {
                if let Some(c) = overrides[n] {
                    return rgb(c);
                }
                let idx = n as usize;
                match n {
                    NamedColor::Foreground | NamedColor::BrightForeground => self.fg,
                    NamedColor::Background => self.bg,
                    NamedColor::Cursor => self.cursor,
                    NamedColor::DimForeground => dim(self.fg),
                    _ if idx < 16 => self.palette[idx],
                    // Dim{Black..White} follow DimForeground in the enum ordering.
                    _ => dim(self.palette[(idx - NamedColor::DimBlack as usize) % 8]),
                }
            }
        }
    }
}

/// A run of adjacent cells in one row sharing style and color, shaped as one unit.
struct Run {
    row: i32,
    col0: usize,
    style: u8,
    fg: [f32; 3],
    cells: Vec<(char, u16)>,
}

fn dim(c: [f32; 3]) -> [f32; 3] {
    [c[0] * 0.66, c[1] * 0.66, c[2] * 0.66]
}

fn rgba(c: [f32; 3], a: f32) -> [f32; 4] {
    [c[0], c[1], c[2], a]
}

struct App {
    config: Config,
    config_path: PathBuf,
    theme: Theme,
    proxy: EventLoopProxy<UserEvent>,
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    tabs: Vec<Tab>,
    active: usize,
    next_id: u64,
    modifiers: ModifiersState,
    mouse: PhysicalPosition<f64>,
    scroll_accum: f64,
    focused: bool,
    occluded: bool,
}

/// Pixel layout of the window, derived from the window size, cell size and config.
struct Layout {
    tab_bar_h: f32,
    grid_x: f32,
    grid_y: f32,
    cols: usize,
    rows: usize,
}

impl App {
    fn new(config: Config, config_path: PathBuf, proxy: EventLoopProxy<UserEvent>) -> Self {
        Self {
            theme: Theme::from_config(&config),
            config,
            config_path,
            proxy,
            window: None,
            renderer: None,
            tabs: Vec::new(),
            active: 0,
            next_id: 1,
            modifiers: ModifiersState::empty(),
            mouse: PhysicalPosition::new(0.0, 0.0),
            scroll_accum: 0.0,
            focused: true,
            occluded: false,
        }
    }

    fn layout(&self) -> Layout {
        let r = self.renderer.as_ref().unwrap();
        let (w, h) = r.size();
        let (cw, ch) = r.cell();
        let pad = (self.config.window.padding * r.scale).round();
        let show_tabs = self.config.tabs.always_show || self.tabs.len() > 1;
        let tab_bar_h = if show_tabs { (ch * 1.7).round() } else { 0.0 };
        let cols = (((w - 2.0 * pad) / cw).floor() as usize).max(2);
        let rows = (((h - tab_bar_h - 2.0 * pad) / ch).floor() as usize).max(1);
        Layout { tab_bar_h, grid_x: pad, grid_y: tab_bar_h + pad, cols, rows }
    }

    fn cell_px(&self) -> (u16, u16) {
        let (cw, ch) = self.renderer.as_ref().unwrap().cell();
        (cw as u16, ch as u16)
    }

    fn new_tab(&mut self) {
        let l = self.layout();
        let (cw, ch) = self.cell_px();
        let id = self.next_id;
        self.next_id += 1;
        match Tab::spawn(id, &self.config, GridSize { cols: l.cols, rows: l.rows }, cw, ch, self.proxy.clone()) {
            Ok(tab) => {
                self.tabs.push(tab);
                self.active = self.tabs.len() - 1;
                // Showing the tab bar for the first time changes the grid height.
                self.resize_all();
            }
            Err(e) => log::error!("failed to spawn shell: {e}"),
        }
        self.request_redraw();
    }

    fn close_tab(&mut self, index: usize, event_loop: &ActiveEventLoop) {
        if index >= self.tabs.len() {
            return;
        }
        self.tabs.remove(index);
        if self.tabs.is_empty() {
            event_loop.exit();
            return;
        }
        self.active = self.active.min(self.tabs.len() - 1);
        self.resize_all();
        self.request_redraw();
    }

    fn resize_all(&mut self) {
        let l = self.layout();
        let (cw, ch) = self.cell_px();
        for tab in &self.tabs {
            tab.resize(GridSize { cols: l.cols, rows: l.rows }, cw, ch);
        }
    }

    fn request_redraw(&self) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    fn active_tab(&self) -> Option<&Tab> {
        self.tabs.get(self.active)
    }

    fn apply_config(&mut self, new: Config) {
        let font_changed = new.font != self.config.font;
        self.config = new;
        self.theme = Theme::from_config(&self.config);
        if font_changed {
            let scale = self.window.as_ref().unwrap().scale_factor() as f32;
            self.renderer.as_mut().unwrap().reload_fonts(&self.config, scale);
        }
        self.resize_all();
        self.request_redraw();
    }

    fn open_settings(&self) {
        if !self.config_path.exists() {
            if let Err(e) = config::write_default(&self.config_path) {
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

    fn paste(&self) {
        let Some(tab) = self.active_tab() else { return };
        let Ok(text) = arboard::Clipboard::new().and_then(|mut c| c.get_text()) else { return };
        let text = text.replace("\r\n", "\r").replace('\n', "\r");
        let bracketed = tab.term.lock().mode().contains(TermMode::BRACKETED_PASTE);
        if bracketed {
            tab.write(format!("\x1b[200~{}\x1b[201~", text.replace('\x1b', "")).as_bytes());
        } else {
            tab.write(text.as_bytes());
        }
    }

    fn change_font_size(&mut self, delta: f32) {
        let mut cfg = self.config.clone();
        cfg.font.size = (cfg.font.size + delta).clamp(6.0, 72.0);
        self.apply_config(cfg);
    }

    fn handle_action(&mut self, action: Action, event_loop: &ActiveEventLoop) {
        match action {
            Action::NewTab => self.new_tab(),
            Action::CloseTab => self.close_tab(self.active, event_loop),
            Action::NextTab => {
                self.active = (self.active + 1) % self.tabs.len().max(1);
                self.request_redraw();
            }
            Action::PrevTab => {
                self.active = (self.active + self.tabs.len().max(1) - 1) % self.tabs.len().max(1);
                self.request_redraw();
            }
            Action::SelectTab(i) => {
                if i < self.tabs.len() {
                    self.active = i;
                    self.request_redraw();
                }
            }
            Action::OpenSettings => self.open_settings(),
            Action::Paste => self.paste(),
            Action::FontBigger => self.change_font_size(1.0),
            Action::FontSmaller => self.change_font_size(-1.0),
            Action::FontReset => {
                let size = config::load_from(&self.config_path).map(|c| c.font.size).unwrap_or(config::FontConfig::default().size);
                self.change_font_size(size - self.config.font.size);
            }
            Action::Write(bytes) => {
                if let Some(tab) = self.active_tab() {
                    tab.term.lock().scroll_display(Scroll::Bottom);
                    tab.write(&bytes);
                }
            }
        }
    }

    fn draw(&mut self) {
        let l = self.layout();
        let opacity = self.config.window.opacity.clamp(0.0, 1.0);
        let theme = &self.theme;
        let r = self.renderer.as_mut().unwrap();
        let (cw, ch) = r.cell();
        let (win_w, _) = r.size();
        r.begin();

        // Tab bar.
        if l.tab_bar_h > 0.0 {
            r.rect(0.0, 0.0, win_w, l.tab_bar_h, rgba(theme.tab_bar, opacity.max(0.6)));
            let plus_w = l.tab_bar_h;
            let n = self.tabs.len().max(1) as f32;
            let tab_w = ((win_w - plus_w) / n).min(260.0 * r.scale);
            let text_y = ((l.tab_bar_h - ch) / 2.0).round();
            for (i, tab) in self.tabs.iter().enumerate() {
                let x = i as f32 * tab_w;
                let active = i == self.active;
                if active {
                    r.rect(x, 0.0, tab_w, l.tab_bar_h, rgba(theme.tab_active, 1.0));
                    r.rect(x, l.tab_bar_h - 2.0 * r.scale, tab_w, 2.0 * r.scale, rgba(theme.palette[4], 1.0));
                }
                r.rect(x + tab_w - r.scale, l.tab_bar_h * 0.25, r.scale, l.tab_bar_h * 0.5, rgba(theme.fg, 0.15));
                let title = format!("{}  {}", i + 1, tab.display_title());
                let alpha = if active { 1.0 } else { 0.55 };
                r.text(&title, x + cw, text_y, x + tab_w - cw, rgba(theme.fg, alpha));
            }
            let plus_x = tab_w * self.tabs.len() as f32;
            r.text("+", plus_x + (plus_w - cw) / 2.0, text_y, plus_x + plus_w, rgba(theme.fg, 0.7));
        }

        let Some(tab) = self.tabs.get(self.active) else {
            r.present(rgba(theme.bg, opacity));
            return;
        };

        let term = tab.term.lock();
        let content = term.renderable_content();
        let offset = content.display_offset as i32;
        let colors = content.colors;
        let cursor = content.cursor;

        // Backgrounds/decorations are emitted immediately; text is grouped into runs of
        // same-style, same-color cells and drawn afterwards (on top) so ligatures can form.
        let mut runs: Vec<Run> = Vec::new();
        let mut cur: Option<Run> = None;
        for indexed in content.display_iter {
            let cell = indexed.cell;
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            let row = indexed.point.line.0 + offset;
            let col = indexed.point.column.0;
            let x = l.grid_x + col as f32 * cw;
            let y = l.grid_y + row as f32 * ch;

            let bold = cell.flags.contains(Flags::BOLD);
            let mut fg_color = cell.fg;
            // Bold-as-bright for the 8 base ANSI colors.
            if bold {
                if let Color::Named(n) = fg_color {
                    if (n as usize) < 8 {
                        fg_color = Color::Indexed(n as u8 + 8);
                    }
                }
            }
            let mut fg = theme.resolve(fg_color, colors);
            let mut bg = theme.resolve(cell.bg, colors);
            let mut default_bg = cell.bg == Color::Named(NamedColor::Background);
            if cell.flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
                default_bg = false;
            }
            if cell.flags.contains(Flags::DIM) {
                fg = dim(fg);
            }
            let width = if cell.flags.contains(Flags::WIDE_CHAR) { 2.0 } else { 1.0 };

            if !default_bg {
                r.rect(x, y, cw * width, ch, rgba(bg, 1.0));
            }
            if cell.flags.intersects(Flags::ALL_UNDERLINES) {
                r.rect(x, y + ch - 2.0 * r.scale, cw * width, r.scale.max(1.0), rgba(fg, 1.0));
            }
            if cell.flags.contains(Flags::STRIKEOUT) {
                r.rect(x, y + ch / 2.0, cw * width, r.scale.max(1.0), rgba(fg, 1.0));
            }

            let c = if cell.flags.contains(Flags::HIDDEN) { ' ' } else { cell.c };
            let style = bold as u8 * text::BOLD + cell.flags.contains(Flags::ITALIC) as u8 * text::ITALIC;
            match &mut cur {
                Some(run) if run.row == row && run.style == style && run.fg == fg => {
                    run.cells.push((c, (col - run.col0) as u16));
                }
                _ => {
                    runs.extend(cur.take());
                    cur = Some(Run { row, col0: col, style, fg, cells: vec![(c, 0)] });
                }
            }
        }
        runs.extend(cur.take());
        for run in &runs {
            let x = l.grid_x + run.col0 as f32 * cw;
            let y = l.grid_y + run.row as f32 * ch;
            r.run(&run.cells, run.style, x, y, rgba(run.fg, 1.0));
        }

        // Cursor (drawn last, with the character re-drawn on top for block cursors).
        let cur_row = cursor.point.line.0 + offset;
        if cursor.shape != CursorShape::Hidden && cur_row >= 0 && (cur_row as usize) < term.screen_lines() {
            let x = l.grid_x + cursor.point.column.0 as f32 * cw;
            let y = l.grid_y + cur_row as f32 * ch;
            let cc = rgba(theme.resolve(Color::Named(NamedColor::Cursor), colors), 1.0);
            let t = (r.scale * 2.0).round();
            match (cursor.shape, self.focused) {
                (CursorShape::Beam, true) => r.rect(x, y, t, ch, cc),
                (CursorShape::Underline, true) => r.rect(x, y + ch - t, cw, t, cc),
                (CursorShape::Block, true) => {
                    r.rect(x, y, cw, ch, cc);
                    let c = term.grid()[Point::new(cursor.point.line, cursor.point.column)].c;
                    r.run(&[(c, 0)], text::REGULAR, x, y, rgba(theme.bg, 1.0));
                }
                _ => {
                    // Hollow block (unfocused window or explicit hollow shape).
                    r.rect(x, y, cw, t, cc);
                    r.rect(x, y + ch - t, cw, t, cc);
                    r.rect(x, y, t, ch, cc);
                    r.rect(x + cw - t, y, t, ch, cc);
                }
            }
        }
        drop(term);

        r.present(rgba(theme.bg, opacity));

        if let Some(w) = &self.window {
            w.set_title(&format!("{} — Lumen", tab.display_title()));
        }
    }

    fn on_click(&mut self, event_loop: &ActiveEventLoop) {
        let l = self.layout();
        let (x, y) = (self.mouse.x as f32, self.mouse.y as f32);
        if y >= l.tab_bar_h {
            return;
        }
        let r = self.renderer.as_ref().unwrap();
        let (win_w, _) = r.size();
        let plus_w = l.tab_bar_h;
        let tab_w = ((win_w - plus_w) / self.tabs.len().max(1) as f32).min(260.0 * r.scale);
        let idx = (x / tab_w) as usize;
        if idx < self.tabs.len() {
            if self.modifiers.alt_key() {
                self.close_tab(idx, event_loop);
            } else {
                self.active = idx;
                self.request_redraw();
            }
        } else if x < tab_w * self.tabs.len() as f32 + plus_w {
            self.new_tab();
        }
    }

    fn on_scroll(&mut self, delta: MouseScrollDelta) {
        let (_, ch) = self.renderer.as_ref().unwrap().cell();
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
        let Some(tab) = self.active_tab() else { return };
        let mut term = tab.term.lock();
        if term.mode().contains(TermMode::ALT_SCREEN) {
            // Full-screen apps (less, vim…): translate the wheel into arrow keys.
            let key: &[u8] = if whole > 0 { b"\x1b[A" } else { b"\x1b[B" };
            drop(term);
            for _ in 0..whole.abs() {
                tab.write(key);
            }
        } else {
            term.scroll_display(Scroll::Delta(whole));
            drop(term);
            self.request_redraw();
        }
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("Lumen")
            .with_transparent(true)
            .with_inner_size(winit::dpi::LogicalSize::new(900.0, 560.0));
        let window = Arc::new(event_loop.create_window(attrs).expect("create window"));

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
        let (cw, ch) = self.renderer.as_ref().unwrap().cell();
        let pad = self.config.window.padding * window.scale_factor() as f32;
        let tab_bar = if self.config.tabs.always_show { (ch * 1.7).round() } else { 0.0 };
        let w = self.config.window.columns as f32 * cw + 2.0 * pad;
        let h = self.config.window.rows as f32 * ch + 2.0 * pad + tab_bar;
        if let Some(size) = window.request_inner_size(PhysicalSize::new(w as u32, h as u32)) {
            self.renderer.as_mut().unwrap().resize(size.width, size.height);
        }

        self.new_tab();
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Wakeup => self.request_redraw(),
            UserEvent::Exited(id) => {
                if let Some(i) = self.tabs.iter().position(|t| t.id == id) {
                    self.close_tab(i, event_loop);
                }
            }
            UserEvent::ConfigChanged => match config::load_from(&self.config_path) {
                Ok(cfg) => {
                    log::info!("config reloaded");
                    self.apply_config(cfg);
                }
                Err(e) => log::error!("config error (keeping previous config): {e}"),
            },
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(r) = self.renderer.as_mut() {
                    r.resize(size.width, size.height);
                    self.resize_all();
                    self.request_redraw();
                }
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                let cfg = self.config.clone();
                if let Some(r) = self.renderer.as_mut() {
                    r.reload_fonts(&cfg, scale_factor as f32);
                }
            }
            WindowEvent::Focused(f) => {
                self.focused = f;
                self.request_redraw();
            }
            WindowEvent::ModifiersChanged(m) => self.modifiers = m.state(),
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                let app_cursor = self.active_tab().is_some_and(|t| t.term.lock().mode().contains(TermMode::APP_CURSOR));
                if let Some(action) = input::translate(&event, self.modifiers, app_cursor, self.config.option_as_alt) {
                    self.handle_action(action, event_loop);
                }
            }
            WindowEvent::CursorMoved { position, .. } => self.mouse = position,
            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Left, .. } => self.on_click(event_loop),
            WindowEvent::MouseWheel { delta, .. } => self.on_scroll(delta),
            // Hidden/minimized windows don't render at all; the terminal state still updates.
            WindowEvent::Occluded(o) => {
                self.occluded = o;
                self.request_redraw();
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
        // Same private window-server call Ghostty/Alacritty-style terminals use. An
        // NSVisualEffectView would sit *above* the GPU layer and hide the content.
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
        let _ = radius;
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
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("lumen=info")).init();

    let config_path = config::find_config_path().unwrap_or_else(config::default_config_path);
    let mut config = config::load();
    // `lumen -e <program> [args…]` runs a command instead of the login shell.
    let args: Vec<String> = std::env::args().collect();
    if let Some(i) = args.iter().position(|a| a == "-e") {
        if let Some(program) = args.get(i + 1) {
            config.shell.program = program.clone();
            config.shell.args = args[i + 2..].to_vec();
        }
    }
    log::info!("config path: {}", config_path.display());

    let event_loop = EventLoop::<UserEvent>::with_user_event().build().expect("event loop");
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();
    watch_config(config_path.clone(), proxy.clone());

    let mut app = App::new(config, config_path, proxy);
    event_loop.run_app(&mut app).expect("event loop error");
}
