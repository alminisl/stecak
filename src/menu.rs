//! Native macOS menu bar (Stećak / File / Edit / View / AI / Window / Help), like iTerm2's.
//! Items carry the same shortcuts as the keyboard handler; macOS routes a shortcut to the
//! menu first, so each item just forwards its id to the event loop as `UserEvent::Menu`.

use muda::accelerator::Accelerator;
use muda::{AboutMetadata, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use winit::event_loop::EventLoopProxy;

use crate::pane::UserEvent;

fn item(id: &str, text: &str, keys: Option<&str>) -> MenuItem {
    MenuItem::with_id(id, text, true, keys.and_then(|k| k.parse::<Accelerator>().ok()))
}

/// Build and install the menu bar. The returned `Menu` must be kept alive.
pub fn install(proxy: EventLoopProxy<UserEvent>) -> Menu {
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        let _ = proxy.send_event(UserEvent::Menu(e.id.0));
    }));
    let sep = PredefinedMenuItem::separator;
    let about = AboutMetadata {
        name: Some("Stećak".into()),
        version: Some(env!("CARGO_PKG_VERSION").into()),
        comments: Some("A fast, lightweight GPU terminal".into()),
        website: Some("https://alminisl.github.io/stecak/".into()),
        license: Some("MIT".into()),
        ..Default::default()
    };
    let app = Submenu::with_items(
        "Stećak",
        true,
        &[
            &PredefinedMenuItem::about(Some("About Stećak"), Some(about)),
            &item("check-updates", "Check for Updates…", None),
            &sep(),
            &item("settings", "Settings…", Some("CmdOrCtrl+,")),
            &item("open-config", "Open Config File", None),
            &sep(),
            &PredefinedMenuItem::services(None),
            &sep(),
            &PredefinedMenuItem::hide(Some("Hide Stećak")),
            &PredefinedMenuItem::hide_others(None),
            &PredefinedMenuItem::show_all(None),
            &sep(),
            &PredefinedMenuItem::quit(Some("Quit Stećak")),
        ],
    )
    .expect("app menu");
    let file = Submenu::with_items(
        "File",
        true,
        &[
            &item("new-tab", "New Tab", Some("CmdOrCtrl+T")),
            &item("split-right", "Split Right", Some("CmdOrCtrl+D")),
            &item("split-down", "Split Down", Some("CmdOrCtrl+Shift+D")),
            &item("close", "Close", Some("CmdOrCtrl+W")),
        ],
    )
    .expect("file menu");
    let edit = Submenu::with_items(
        "Edit",
        true,
        &[
            &item("copy", "Copy", Some("CmdOrCtrl+C")),
            &item("paste", "Paste", Some("CmdOrCtrl+V")),
            &sep(),
            &item("find", "Find…", Some("CmdOrCtrl+F")),
            &item("find-next", "Find Next", Some("CmdOrCtrl+G")),
            &item("find-prev", "Find Previous", Some("CmdOrCtrl+Shift+G")),
            &sep(),
            &item("clear", "Clear Scrollback", Some("CmdOrCtrl+K")),
        ],
    )
    .expect("edit menu");
    let view = Submenu::with_items(
        "View",
        true,
        &[
            &item("bigger", "Bigger", Some("CmdOrCtrl+=")),
            &item("smaller", "Smaller", Some("CmdOrCtrl+-")),
            &item("actual-size", "Actual Size", Some("CmdOrCtrl+0")),
            &sep(),
            &item("bosancica", "Bosančica Mode", Some("CmdOrCtrl+Shift+B")),
            &sep(),
            &PredefinedMenuItem::fullscreen(None),
            &sep(),
            &item("palette", "Command Palette…", Some("CmdOrCtrl+Shift+P")),
        ],
    )
    .expect("view menu");
    let ai = Submenu::with_items(
        "AI",
        true,
        &[
            &item("ask-ai", "Ask AI for a Command…", Some("CmdOrCtrl+I")),
            &item("explain-error", "Explain Last Error", Some("CmdOrCtrl+Shift+E")),
            &item("send-to-agent", "Send Selection to Agent", Some("CmdOrCtrl+Shift+L")),
            &sep(),
            &item("agent", "Open Agent in Split", Some("CmdOrCtrl+Shift+A")),
            &item("sessions", "Sessions…", Some("CmdOrCtrl+Shift+S")),
        ],
    )
    .expect("ai menu");
    let window = Submenu::with_items(
        "Window",
        true,
        &[
            &PredefinedMenuItem::minimize(None),
            &PredefinedMenuItem::maximize(Some("Zoom")),
            &sep(),
            &item("next-tab", "Show Next Tab", Some("CmdOrCtrl+Shift+]")),
            &item("prev-tab", "Show Previous Tab", Some("CmdOrCtrl+Shift+[")),
            &item("next-pane", "Select Next Pane", Some("CmdOrCtrl+]")),
            &item("prev-pane", "Select Previous Pane", Some("CmdOrCtrl+[")),
            &sep(),
            &PredefinedMenuItem::bring_all_to_front(None),
        ],
    )
    .expect("window menu");
    let help = Submenu::with_items(
        "Help",
        true,
        &[
            &item("shortcuts", "Keyboard Shortcuts", Some("CmdOrCtrl+/")),
            &sep(),
            &item("website", "Stećak Website", None),
            &item("issue", "Report an Issue", None),
        ],
    )
    .expect("help menu");
    let menu = Menu::with_items(&[&app, &file, &edit, &view, &ai, &window, &help]).expect("menu bar");
    menu.init_for_nsapp();
    window.set_as_windows_menu_for_nsapp();
    help.set_as_help_menu_for_nsapp();
    menu
}
