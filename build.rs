//! Windows: embed the app icon so the .exe shows it in Explorer, on the desktop and in the
//! taskbar (the window also loads it at runtime as resource 1).

fn main() {
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon_with_id("assets/stecak.ico", "1");
        res.set("ProductName", "Stećak");
        res.set("FileDescription", "Stećak terminal");
        if let Err(e) = res.compile() {
            println!("cargo:warning=could not embed the Windows icon: {e}");
        }
    }
    println!("cargo:rerun-if-changed=assets/stecak.ico");
}
