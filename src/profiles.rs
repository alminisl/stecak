//! Shell profiles: the shells installed on this machine, found the way Windows Terminal's
//! profile generators find them (PowerShell, cmd, WSL distros from the registry), plus Git
//! Bash, plus `profiles:` from the config. The palette opens a tab with any of them.

use std::sync::OnceLock;

use crate::config::{Config, Profile, ShellConfig};

/// Configured profiles first, then detected ones whose name the config doesn't reuse.
pub fn all(config: &Config) -> Vec<Profile> {
    merge(&config.profiles, detected())
}

pub fn find(config: &Config, name: &str) -> Option<ShellConfig> {
    // Configured first, so naming one of those never triggers detection.
    config.profiles.iter().find(|p| p.name == name).or_else(|| detected().iter().find(|p| p.name == name)).map(Profile::shell)
}

fn merge(user: &[Profile], detected: &[Profile]) -> Vec<Profile> {
    let mut all = user.to_vec();
    all.extend(detected.iter().filter(|d| !user.iter().any(|u| u.name == d.name)).cloned());
    all
}

/// shortcut: detected once per run, so a shell installed while Stećak runs shows up after a
/// restart (Windows Terminal also only rescans on settings reload).
fn detected() -> &'static [Profile] {
    static DETECTED: OnceLock<Vec<Profile>> = OnceLock::new();
    DETECTED.get_or_init(detect)
}

fn profile(name: &str, program: impl Into<String>, args: &[&str]) -> Profile {
    Profile { name: name.into(), program: program.into(), args: args.iter().map(|a| a.to_string()).collect() }
}

#[cfg(windows)]
fn detect() -> Vec<Profile> {
    use std::path::PathBuf;
    let env = |k: &str| std::env::var_os(k).map(PathBuf::from);
    let system = env("SystemRoot").unwrap_or_else(|| PathBuf::from(r"C:\Windows")).join("System32");
    let existing = |p: PathBuf| p.is_file().then(|| p.display().to_string());
    let mut out = Vec::new();

    let pwsh = crate::shell::which("pwsh").map(|p| p.display().to_string()).or_else(|| existing(env("ProgramFiles")?.join(r"PowerShell\7\pwsh.exe")));
    if let Some(p) = pwsh {
        out.push(profile("PowerShell", p, &["-NoLogo"]));
    }
    if let Some(p) = existing(system.join(r"WindowsPowerShell\v1.0\powershell.exe")) {
        out.push(profile("Windows PowerShell", p, &["-NoLogo"]));
    }
    if let Some(p) = existing(system.join("cmd.exe")) {
        out.push(profile("Command Prompt", p, &[]));
    }
    if let Some(wsl) = existing(system.join("wsl.exe")) {
        for name in wsl_distros() {
            out.push(profile(&name, wsl.clone(), &["-d", &name, "--cd", "~"]));
        }
    }
    let git = [env("ProgramFiles").map(|d| d.join(r"Git\bin\bash.exe")), env("LOCALAPPDATA").map(|d| d.join(r"Programs\Git\bin\bash.exe"))];
    if let Some(p) = git.into_iter().flatten().find_map(existing) {
        out.push(profile("Git Bash", p, &["--login", "-i"]));
    }
    out
}

/// Distro names under HKCU\…\Lxss (one subkey per registered distro), like Windows
/// Terminal's WslDistroGenerator; this avoids spawning `wsl.exe -l`, which can take seconds
/// while the WSL VM starts.
#[cfg(windows)]
fn wsl_distros() -> Vec<String> {
    use windows_sys::Win32::System::Registry::{HKEY, HKEY_CURRENT_USER, KEY_READ, RRF_RT_REG_SZ, RegCloseKey, RegEnumKeyExW, RegGetValueW, RegOpenKeyExW};
    let wide = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
    let mut key: HKEY = std::ptr::null_mut();
    let path = wide(r"Software\Microsoft\Windows\CurrentVersion\Lxss");
    // SAFETY: valid NUL-terminated path and out pointer; the key is closed below.
    if unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, path.as_ptr(), 0, KEY_READ, &mut key) } != 0 {
        return Vec::new();
    }
    let value = wide("DistributionName");
    let mut names = Vec::new();
    for i in 0.. {
        let mut sub = [0u16; 256];
        let mut sub_len = sub.len() as u32;
        // SAFETY: `sub` holds `sub_len` u16s; the optional out pointers are null.
        let err = unsafe { RegEnumKeyExW(key, i, sub.as_mut_ptr(), &mut sub_len, std::ptr::null(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) };
        if err != 0 {
            break; // ERROR_NO_MORE_ITEMS
        }
        let mut buf = [0u16; 256];
        let mut bytes = std::mem::size_of_val(&buf) as u32;
        // SAFETY: `buf` holds `bytes` bytes; RRF_RT_REG_SZ guarantees a NUL-terminated string.
        let err = unsafe { RegGetValueW(key, sub.as_ptr(), value.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), buf.as_mut_ptr().cast(), &mut bytes) };
        if err == 0 {
            let len = (bytes as usize / 2).saturating_sub(1);
            names.push(String::from_utf16_lossy(&buf[..len]));
        }
    }
    // SAFETY: opened above.
    unsafe { RegCloseKey(key) };
    names.retain(|n| !is_hidden_distro(n));
    names
}

/// Docker's and Rancher's internal distros aren't shells (Windows Terminal hides them too).
#[cfg(any(windows, test))]
fn is_hidden_distro(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.starts_with("docker-desktop") || n.starts_with("rancher-desktop")
}

#[cfg(not(windows))]
fn detect() -> Vec<Profile> {
    let text = std::fs::read_to_string("/etc/shells").unwrap_or_default();
    unix_shells(&text).into_iter().filter(|p| std::path::Path::new(&p.program).is_file()).collect()
}

/// One profile per shell name in /etc/shells (it lists /bin/zsh and /usr/bin/zsh alike).
#[cfg(any(not(windows), test))]
fn unix_shells(text: &str) -> Vec<Profile> {
    let mut out: Vec<Profile> = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| l.starts_with('/')) {
        let name = line.rsplit('/').next().unwrap_or(line);
        if !out.iter().any(|p| p.name == name) {
            out.push(profile(name, line, &["-l"]));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_profiles_replace_detected_ones_by_name() {
        let user = [profile("Ubuntu", "wsl.exe", &["-d", "Ubuntu-24.04"])];
        let detected = [profile("PowerShell", "pwsh", &[]), profile("Ubuntu", "wsl.exe", &["-d", "Ubuntu"])];
        let all = merge(&user, &detected);
        assert_eq!(all.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["Ubuntu", "PowerShell"]);
        assert_eq!(all[0].args, ["-d", "Ubuntu-24.04"]);
    }

    #[test]
    fn parses_etc_shells() {
        let all = unix_shells("# comment\n/bin/sh\n/bin/zsh\n/usr/bin/zsh\n\n/opt/homebrew/bin/fish\n");
        assert_eq!(all.iter().map(|p| p.program.as_str()).collect::<Vec<_>>(), ["/bin/sh", "/bin/zsh", "/opt/homebrew/bin/fish"]);
        assert_eq!(all[1].name, "zsh");
    }

    #[test]
    fn hides_container_distros() {
        assert!(is_hidden_distro("docker-desktop-data"));
        assert!(is_hidden_distro("rancher-desktop"));
        assert!(!is_hidden_distro("Ubuntu"));
    }
}
