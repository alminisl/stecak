//! CI status for the focused pane's repo: the latest GitHub Actions runs or GitLab pipeline on
//! its current branch, shown as an icon in the bottom bar (with a finished/total job count while
//! it runs) and every run with its jobs listed on hover.
//!
//! shortcut: shells out to git and curl (like the update check) instead of linking git and an
//! HTTP/TLS stack; no ETags, so every poll (one request per workflow run for its jobs, plus one
//! for the list) counts against the API rate limit.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;

use winit::event_loop::EventLoopProxy;

use crate::pane::UserEvent;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum State {
    Running,
    Passed,
    Failed,
    /// Cancelled, skipped, waiting for a manual action…
    Other,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Status {
    pub state: State,
    /// The run or pipeline page.
    pub url: String,
    /// Workflow name, or "Pipeline".
    pub name: String,
    /// Every run of the newest commit (one per workflow), for the hover list.
    pub runs: Vec<Run>,
}

/// One workflow run (GitHub) or the pipeline (GitLab).
#[derive(Clone, Debug, PartialEq)]
pub struct Run {
    pub name: String,
    pub state: State,
    pub url: String,
    /// The run's jobs (GitHub: each `jobs:` entry; GitLab: each pipeline job), in order.
    pub jobs: Vec<Job>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Job {
    pub name: String,
    pub state: State,
    pub url: String,
}

/// A line in the hover list: a run (depth 0) or one of its jobs (depth 1).
pub struct Row<'a> {
    pub depth: usize,
    pub name: &'a str,
    pub state: State,
    pub url: &'a str,
}

impl Status {
    /// Jobs finished so far and in total, across every run of the commit. None without jobs.
    pub fn jobs_done(&self) -> Option<(usize, usize)> {
        let total = self.runs.iter().map(|r| r.jobs.len()).sum::<usize>();
        let done = self.runs.iter().flat_map(|r| &r.jobs).filter(|j| j.state != State::Running).count();
        (total > 0).then_some((done, total))
    }

    pub fn rows(&self) -> Vec<Row<'_>> {
        let mut rows = vec![];
        for r in &self.runs {
            rows.push(Row { depth: 0, name: &r.name, state: r.state, url: &r.url });
            rows.extend(r.jobs.iter().map(|j| Row { depth: 1, name: &j.name, state: j.state, url: &j.url }));
        }
        rows
    }
}

#[derive(Debug, PartialEq)]
enum Host {
    GitHub,
    GitLab(String),
}

/// Fetch the status for `cwd` on a background thread; the answer (None when the folder isn't
/// a GitHub/GitLab repo or the request failed) arrives as `UserEvent::Ci`.
pub fn fetch_async(cwd: PathBuf, gitlab_hosts: Vec<String>, proxy: EventLoopProxy<UserEvent>) {
    let _ = std::thread::Builder::new().name("ci".into()).stack_size(256 * 1024).spawn(move || {
        let status = fetch(&cwd, &gitlab_hosts);
        log::debug!("ci: {} → {:?}", cwd.display(), status.as_ref().map(|s| s.state));
        let _ = proxy.send_event(UserEvent::Ci(cwd, status));
    });
}

fn fetch(cwd: &Path, gitlab_hosts: &[String]) -> Option<Status> {
    let git = |args: &[&str]| -> Option<String> {
        let out = crate::ai::no_console(Command::new("git").arg("-C").arg(cwd).args(args)).stderr(Stdio::null()).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let (host, path) = parse_remote(&git(&["remote", "get-url", "origin"])?)?;
    let branch = git(&["branch", "--show-current"]).filter(|b| !b.is_empty())?;
    match provider(&host, gitlab_hosts)? {
        Host::GitHub => {
            let token = token("github.com", &host)?;
            let runs = |query: &str| get(&format!("https://api.github.com/repos/{path}/actions/runs?per_page=10&{query}"), token.as_deref());
            // By commit first, so a tag's release run shows next to the branch's CI run. A
            // commit that isn't pushed yet has no runs: then the branch's newest.
            let by_sha = git(&["rev-parse", "HEAD"]).and_then(|sha| runs(&format!("head_sha={sha}")));
            let v = match by_sha.filter(|v| v["workflow_runs"].as_array().is_some_and(|a| !a.is_empty())) {
                Some(v) => v,
                None => runs(&format!("branch={}", encode(&branch)))?,
            };
            github_status(&v, |jobs_url| get(&format!("{jobs_url}?per_page=100"), token.as_deref()))
        }
        Host::GitLab(h) => {
            // By commit first: merge request pipelines run on refs/merge-requests/<iid>/head,
            // so filtering by branch alone never finds them.
            let project = format!("projects/{}/pipelines?per_page=1", encode(&path));
            let by_sha = git(&["rev-parse", "HEAD"]).and_then(|sha| gitlab_get(&h, &format!("{project}&sha={sha}")));
            let v = match by_sha.filter(|v| v.as_array().is_some_and(|a| !a.is_empty())) {
                Some(v) => v,
                None => gitlab_get(&h, &format!("{project}&ref={}", encode(&branch)))?,
            };
            gitlab_status(&v, |id| gitlab_get(&h, &format!("projects/{}/pipelines/{id}/jobs?per_page=100", encode(&path))))
        }
    }
}

/// A GitLab API call. Through `glab api` when it's installed: it reads its token from the OS
/// keyring, where recent glab versions keep it (`glab config get token` returns nothing then).
/// Otherwise curl, with GITLAB_TOKEN if set.
fn gitlab_get(host: &str, endpoint: &str) -> Option<serde_json::Value> {
    let mut cmd = Command::new("glab");
    if let Some(path) = crate::ai::user_path().filter(|_| cfg!(unix)) {
        cmd.env("PATH", path);
    }
    let out = crate::ai::no_console(cmd.args(["api", "--hostname", host, endpoint])).stdin(Stdio::null()).stderr(Stdio::null()).output();
    match out {
        Ok(out) if out.status.success() => serde_json::from_slice(&out.stdout).ok(),
        Ok(_) => {
            log::warn!("ci: glab api failed: {host} {endpoint}");
            None
        }
        // No glab: plain HTTPS.
        Err(_) => get(&format!("https://{host}/api/v4/{endpoint}"), token("gitlab", host)?.as_deref()),
    }
}

/// `git@host:a/b.git`, `ssh://git@host:22/a/b.git` or `https://host/a/b` → (host, "a/b").
fn parse_remote(url: &str) -> Option<(String, String)> {
    let (host, path) = match url.split_once("://") {
        Some((_, rest)) => rest.split_once('/')?,
        None => url.split_once(':')?,
    };
    let host = host.rsplit('@').next()?.split(':').next()?.to_lowercase();
    let path = path.trim_end_matches('/').trim_end_matches(".git").trim_matches('/');
    (!host.is_empty() && path.contains('/')).then(|| (host, path.to_string()))
}

fn provider(host: &str, gitlab_hosts: &[String]) -> Option<Host> {
    if host == "github.com" {
        Some(Host::GitHub)
    } else if host == "gitlab.com" || gitlab_hosts.iter().any(|h| h.eq_ignore_ascii_case(host)) {
        Some(Host::GitLab(host.to_string()))
    } else {
        None
    }
}

/// Percent-encode everything but unreserved characters (for paths and query values).
pub fn encode(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

/// A token from the environment, or for GitHub from `gh`, looked up once per host. (GitLab
/// without GITLAB_TOKEN goes through `glab api` instead, see `gitlab_get`.) Outer None = never
/// (unreachable); inner None = no token, try unauthenticated.
fn token(kind: &str, host: &str) -> Option<Option<String>> {
    static CACHE: Mutex<Vec<(String, Option<String>)>> = Mutex::new(Vec::new());
    let mut cache = CACHE.lock().ok()?;
    if let Some((_, t)) = cache.iter().find(|(h, _)| h == host) {
        return Some(t.clone());
    }
    let found = {
        let (vars, cli): (&[&str], Option<[&str; 3]>) =
            if kind == "github.com" { (&["GH_TOKEN", "GITHUB_TOKEN"], Some(["gh", "auth", "token"])) } else { (&["GITLAB_TOKEN"], None) };
        vars.iter().find_map(|v| std::env::var(v).ok().filter(|t| !t.is_empty())).or_else(|| {
            let cli = cli?;
            let mut cmd = Command::new(cli[0]);
            // Started from the Dock, macOS apps get a bare PATH without Homebrew's gh.
            if let Some(path) = crate::ai::user_path().filter(|_| cfg!(unix)) {
                cmd.env("PATH", path);
            }
            let out = crate::ai::no_console(cmd.args(&cli[1..])).stderr(Stdio::null()).output().ok()?;
            let t = String::from_utf8_lossy(&out.stdout).trim().to_string();
            (out.status.success() && !t.is_empty()).then_some(t)
        })
    };
    cache.push((host.to_string(), found.clone()));
    Some(found)
}

fn get(url: &str, token: Option<&str>) -> Option<serde_json::Value> {
    let mut cmd = Command::new("curl");
    crate::ai::no_console(&mut cmd).args(["-fsSL", "--max-time", "10", "-H", "Accept: application/json", url]);
    // The token goes in through stdin, so it never shows up in a process list.
    if token.is_some() {
        cmd.args(["-H", "@-"]);
    }
    let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    if let (Some(t), Some(mut stdin)) = (token, child.stdin.take()) {
        use std::io::Write;
        let _ = writeln!(stdin, "Authorization: Bearer {t}");
    }
    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        log::warn!("ci: request failed: {url}");
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

/// The newest commit's runs (one per workflow): running if any is, failed if any failed.
/// `jobs` fetches a run's `jobs_url` (at most `MAX_JOB_FETCHES` runs, newest first).
fn github_status(v: &serde_json::Value, jobs: impl Fn(&str) -> Option<serde_json::Value>) -> Option<Status> {
    let runs = v["workflow_runs"].as_array()?;
    let head = runs.first()?;
    let same: Vec<_> = runs.iter().filter(|r| r["head_sha"] == head["head_sha"]).collect();
    let state_of = |r: &serde_json::Value| match (r["status"].as_str(), r["conclusion"].as_str()) {
        (Some("completed"), Some("success")) => State::Passed,
        (Some("completed"), Some("failure" | "timed_out" | "startup_failure")) => State::Failed,
        (Some("completed"), _) => State::Other,
        _ => State::Running,
    };
    let pick = [State::Failed, State::Running, State::Passed, State::Other].into_iter().find_map(|s| same.iter().find(|r| state_of(r) == s))?;
    let name = |r: &serde_json::Value| r["name"].as_str().unwrap_or("Workflow").to_string();
    // Newest first, so a re-run shadows the older run of the same workflow.
    let mut runs: Vec<Run> = vec![];
    for r in &same {
        if !runs.iter().any(|x| x.name == name(r)) {
            let fetched = (runs.len() < MAX_JOB_FETCHES).then(|| r["jobs_url"].as_str()).flatten().and_then(&jobs);
            let jobs = fetched.as_ref().and_then(|v| v["jobs"].as_array()).map(|a| a.iter().map(|j| Job { name: name_of(j, "name", "Job"), state: state_of(j), url: j["html_url"].as_str().unwrap_or_default().to_string() }).collect()).unwrap_or_default();
            runs.push(Run { name: name(r), state: state_of(r), url: r["html_url"].as_str().unwrap_or_default().to_string(), jobs });
        }
    }
    Some(Status { state: state_of(pick), url: pick["html_url"].as_str()?.to_string(), name: name(pick), runs })
}

/// Runs whose jobs are listed; the rest show only their own state.
const MAX_JOB_FETCHES: usize = 5;

fn name_of(v: &serde_json::Value, key: &str, fallback: &str) -> String {
    v[key].as_str().unwrap_or(fallback).to_string()
}

/// `jobs` fetches the pipeline's job list by pipeline id.
fn gitlab_status(v: &serde_json::Value, jobs: impl Fn(u64) -> Option<serde_json::Value>) -> Option<Status> {
    let p = v.as_array()?.first()?;
    let state_of = |s: &str| match s {
        "success" => State::Passed,
        "failed" => State::Failed,
        "created" | "waiting_for_resource" | "preparing" | "pending" | "running" => State::Running,
        _ => State::Other,
    };
    let state = state_of(p["status"].as_str()?);
    let url = p["web_url"].as_str()?.to_string();
    let jobs = p["id"].as_u64().and_then(jobs).and_then(|v| v.as_array().cloned()).unwrap_or_default();
    let jobs = jobs.iter().map(|j| Job { name: name_of(j, "name", "Job"), state: state_of(j["status"].as_str().unwrap_or_default()), url: name_of(j, "web_url", "") }).collect();
    Some(Status { state, runs: vec![Run { name: "Pipeline".into(), state, url: url.clone(), jobs }], url, name: "Pipeline".into() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_remotes() {
        let r = |u: &str| parse_remote(u).map(|(h, p)| format!("{h} {p}"));
        assert_eq!(r("git@github.com:alminisl/stecak.git").as_deref(), Some("github.com alminisl/stecak"));
        assert_eq!(r("https://github.com/alminisl/stecak").as_deref(), Some("github.com alminisl/stecak"));
        assert_eq!(r("ssh://git@Git.Company.com:2222/grp/sub/app.git").as_deref(), Some("git.company.com grp/sub/app"));
        assert_eq!(r("https://user@gitlab.com/grp/app.git/").as_deref(), Some("gitlab.com grp/app"));
        assert_eq!(r("/local/path"), None);
        assert_eq!(provider("git.company.com", &["GIT.company.com".into()]), Some(Host::GitLab("git.company.com".into())));
        assert_eq!(provider("bitbucket.org", &[]), None);
        assert_eq!(encode("grp/sub app"), "grp%2Fsub%20app");
    }

    #[test]
    fn reads_statuses() {
        let run = |sha: &str, status: &str, conclusion: Option<&str>, name: &str| json!({"head_sha": sha, "status": status, "conclusion": conclusion, "name": name, "html_url": format!("u/{name}")});
        let run = |sha: &str, status: &str, conclusion: Option<&str>, name: &str| {
            let mut r = run(sha, status, conclusion, name);
            r["jobs_url"] = json!(format!("jobs/{name}"));
            r
        };
        let no_jobs = |_: &str| None;
        let v = json!({"workflow_runs": [run("b", "completed", Some("success"), "ci"), run("b", "in_progress", None, "release"), run("a", "completed", Some("failure"), "old")]});
        let s = github_status(&v, no_jobs).unwrap();
        assert_eq!((s.state, s.name.as_str()), (State::Running, "release"));
        assert_eq!(s.runs.iter().map(|r| (r.name.as_str(), r.state)).collect::<Vec<_>>(), [("ci", State::Passed), ("release", State::Running)]);
        assert_eq!(s.jobs_done(), None);
        // Jobs: fetched per run, counted across runs; queued ones aren't done.
        let job = |name: &str, status: &str, conclusion: Option<&str>| json!({"name": name, "status": status, "conclusion": conclusion, "html_url": format!("j/{name}")});
        let s = github_status(&v, |url| match url {
            "jobs/release" => Some(json!({"jobs": [job("macos", "completed", Some("success")), job("linux", "in_progress", None), job("windows", "queued", None)]})),
            _ => Some(json!({"jobs": [job("test", "completed", Some("success"))]})),
        })
        .unwrap();
        assert_eq!(s.jobs_done(), Some((2, 4)));
        let rows: Vec<_> = s.rows().iter().map(|r| (r.depth, r.name.to_string(), r.state)).collect();
        assert_eq!(rows[..3], [(0, "ci".into(), State::Passed), (1, "test".into(), State::Passed), (0, "release".into(), State::Running)]);
        assert_eq!(rows[5], (1, "windows".into(), State::Running));
        assert_eq!(s.rows()[4].url, "j/linux");
        let v = json!({"workflow_runs": [run("b", "completed", Some("success"), "ci"), run("a", "completed", Some("failure"), "old")]});
        assert_eq!(github_status(&v, no_jobs).map(|s| s.state), Some(State::Passed));
        assert_eq!(github_status(&json!({"workflow_runs": []}), no_jobs), None);
        let v = json!([{"id": 7, "status": "failed", "web_url": "https://gitlab.com/p/-/pipelines/1"}]);
        let s = gitlab_status(&v, |id| Some(json!([{"name": "build", "status": "success", "web_url": format!("p/{id}/1")}, {"name": "test", "status": "failed", "web_url": "p/7/2"}]))).unwrap();
        assert_eq!((s.state, s.jobs_done()), (State::Failed, Some((2, 2))));
        assert_eq!((s.runs[0].jobs[0].url.as_str(), s.runs[0].jobs[1].state), ("p/7/1", State::Failed));
    }
}
