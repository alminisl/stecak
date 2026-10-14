//! Crash reports. A panic writes `crashes/crash-<unix time>.txt` next to the config, and a
//! copy to `crash-stecak.txt` on the Desktop so it's easy to find and send; the next
//! launch offers to report it, which opens a prefilled GitHub issue (nothing is sent without
//! the user submitting it). On macOS, crashes that never reach the panic hook (aborts, failed
//! allocations, signals) are picked up from the system's DiagnosticReports.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const ISSUES: &str = "https://github.com/alminisl/stecak/issues/new";
/// Keeps the issue URL under what browsers and GitHub accept.
const MAX_BODY: usize = 6000;

#[derive(Clone, Debug)]
pub struct Report {
    /// One line for the popup and the issue title.
    pub summary: String,
    pub body: String,
    /// The file to mark handled: our own report, or a macOS .ips (attached by hand).
    pub file: PathBuf,
}

fn dir(config_dir: &Path) -> PathBuf {
    config_dir.join("crashes")
}

/// `~/Desktop/crash-stecak.txt` (the home folder if there's no Desktop). Newest crash wins.
fn desktop_file(ext: &str) -> Option<PathBuf> {
    Some(dirs::desktop_dir().or_else(dirs::home_dir)?.join(format!("crash-stecak.{ext}")))
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn header() -> String {
    format!("Stećak {} on {} {}", env!("CARGO_PKG_VERSION"), std::env::consts::OS, std::env::consts::ARCH)
}

/// Write a report on panic, then run the previous hook (which logs it).
pub fn install_hook(config_dir: &Path) {
    let dir = dir(config_dir);
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current().name().unwrap_or("unnamed").to_string();
        let message = info.payload_as_str().unwrap_or("(no message)").replace('\n', " ");
        let at = info.location().map_or(String::new(), |l| format!(" at {}:{}", l.file(), l.line()));
        // Release builds strip symbols, so the message and location are what identify a crash.
        let report = format!(
            "{}\nthread: {thread}\npanic: {message}{at}\n\nbacktrace:\n{}\n",
            header(),
            std::backtrace::Backtrace::force_capture()
        );
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(dir.join(format!("crash-{}.txt", now())), &report);
        if let Some(path) = desktop_file("txt") {
            let _ = std::fs::write(path, &report);
        }
        previous(info);
    }));
}

/// The newest crash not yet reported or dismissed.
pub fn pending(config_dir: &Path) -> Option<Report> {
    let dir = dir(config_dir);
    let newest = std::fs::read_dir(&dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "txt"))
        .max();
    if let Some(file) = newest {
        let body = std::fs::read_to_string(&file).ok()?;
        let summary = body.lines().find_map(|l| l.strip_prefix("panic: ")).unwrap_or("panic").to_string();
        return Some(Report { summary, body, file });
    }
    #[cfg(target_os = "macos")]
    return macos_report(&dir);
    #[cfg(not(target_os = "macos"))]
    None
}

/// The newest stecak crash in ~/Library/Logs/DiagnosticReports since we last looked.
#[cfg(target_os = "macos")]
fn macos_report(dir: &Path) -> Option<Report> {
    let seen_file = dir.join("last-seen-ips");
    let seen = std::fs::read_to_string(&seen_file).ok().and_then(|s| s.trim().parse::<u64>().ok());
    let Some(seen) = seen else {
        // First run with crash reports: don't surface crashes from before this feature.
        let _ = std::fs::create_dir_all(dir);
        let _ = std::fs::write(&seen_file, now().to_string());
        return None;
    };
    let reports = dirs::home_dir()?.join("Library/Logs/DiagnosticReports");
    let mtime = |p: &Path| p.metadata().and_then(|m| m.modified()).ok()?.duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs());
    let (_, file) = std::fs::read_dir(reports)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.to_ascii_lowercase().starts_with("stecak") && n.ends_with(".ips")))
        .filter_map(|p| Some((mtime(&p)?, p)))
        .filter(|(t, _)| *t > seen)
        .max()?;
    let text = std::fs::read_to_string(&file).unwrap_or_default();
    // The .ips body is JSON; its exception and termination reason are enough for a title.
    let field = |key: &str| text.split(&format!("\"{key}\"")).nth(1).and_then(|s| s.split('}').next()).map(|s| s.trim_start_matches([':', ' ']).to_string());
    let summary = field("exception").map_or_else(|| "the app quit unexpectedly".to_string(), |e| format!("crash {e}"));
    let body = format!(
        "{}\n{summary}\ntermination: {}\n\nPlease attach the macOS crash report: {}\n(it was revealed in Finder; drag it into this issue)\n",
        header(),
        field("termination").unwrap_or_default(),
        file.display()
    );
    // Aborts never reached our hook, so put the system's report on the Desktop instead.
    if let Some(path) = desktop_file("ips") {
        let _ = std::fs::copy(&file, path);
    }
    Some(Report { summary, body, file })
}

/// Stop offering this report: rename ours, or remember how far we've looked at .ips files.
pub fn mark_handled(config_dir: &Path, report: &Report) {
    if report.file.extension().is_some_and(|e| e == "ips") {
        let _ = std::fs::write(dir(config_dir).join("last-seen-ips"), now().to_string());
    } else {
        let _ = std::fs::rename(&report.file, report.file.with_extension("reported"));
    }
}

/// Open a prefilled GitHub issue (and on macOS, reveal the .ips so it can be attached).
pub fn report(report: &Report) {
    let mut body = report.body.clone();
    if body.len() > MAX_BODY {
        let cut = (0..=MAX_BODY).rev().find(|&i| body.is_char_boundary(i)).unwrap_or(0);
        body.truncate(cut);
        body.push_str("\n…(truncated; full report in the crashes folder next to the config)");
    }
    let mut title: String = report.summary.chars().take(100).collect();
    title.insert_str(0, "Crash: ");
    let url = format!("{ISSUES}?title={}&body={}", encode(&title), encode(&format!("```\n{body}\n```\n\nWhat were you doing when it crashed?\n")));
    crate::mouse::open_url(&url);
    #[cfg(target_os = "macos")]
    if report.file.extension().is_some_and(|e| e == "ips") {
        let _ = std::process::Command::new("open").arg("-R").arg(&report.file).spawn();
    }
}

fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_then_handled() {
        let tmp = std::env::temp_dir().join(format!("stecak-crash-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(dir(&tmp)).unwrap();
        std::fs::write(dir(&tmp).join("last-seen-ips"), u64::MAX.to_string()).unwrap();
        assert!(pending(&tmp).is_none());
        std::fs::write(dir(&tmp).join("crash-1.txt"), "Stećak\npanic: boom at src/x.rs:1\n").unwrap();
        let r = pending(&tmp).expect("a crash is pending");
        assert_eq!(r.summary, "boom at src/x.rs:1");
        mark_handled(&tmp, &r);
        assert!(pending(&tmp).is_none());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn encodes_for_urls() {
        assert_eq!(encode("a b&c=ć"), "a%20b%26c%3D%C4%87");
    }
}
