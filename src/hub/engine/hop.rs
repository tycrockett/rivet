//! Headless agent hops (`runHop`, `killHops`, `shouldFork`, `forkFrom` of
//! `src/stores/fleetStore.ts`, and `agents:run` / `agents:cancel` of
//! `src/main.js`): one headless run of a [`Harness`] per invocation
//! (`claude -p`, `codex exec`, `omp -p`), resolved when it exits.

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    ffi::OsString,
    io::{BufRead, BufReader, Read},
    path::PathBuf,
    process::{Command, Stdio},
    sync::Mutex,
};

use serde_json::Value;

use crate::{git, hub::{HubMember, next_id}, state::Shell};

use super::{TermStatus, agents::{self, Harness}, fleet, send};

/// Whose context a fork is for: the member named, or one merely consulted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Addressee,
    #[allow(dead_code)]
    Consulted,
}

/// What a hop runs on, and the session it carries on from: a fork of it
/// where the harness forks (claude, codex), else that session itself
/// (omp, whose hops resume only their own).
#[derive(Debug, Clone, PartialEq)]
pub struct HopSpec {
    pub harness: Harness,
    pub resume: Option<String>,
}

impl HopSpec {
    /// A new session on the harness the agent command starts.
    pub fn fresh(shell: Shell) -> HopSpec {
        HopSpec { harness: fleet::agent_harness(shell), resume: None }
    }
}

/// What a hop said, and the session it ran in.
#[derive(Debug, Clone, Default)]
pub struct HopResult {
    pub text: String,
    pub error: Option<String>,
    pub session_id: Option<String>,
    pub harness: Harness,
}

thread_local! {
    static CANCELLED: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
    static HOPS: RefCell<HashMap<String, HashSet<String>>> = RefCell::new(HashMap::new());
}

/// Hop id → the child's pid while it runs; hops killed before they had one.
static PIDS: Mutex<Option<HashMap<String, u32>>> = Mutex::new(None);
static KILLED: Mutex<Option<HashSet<String>>> = Mutex::new(None);

pub fn is_cancelled(job_id: &str) -> bool {
    CANCELLED.with(|c| c.borrow().contains(job_id))
}

pub fn mark_cancelled(job_id: &str) {
    CANCELLED.with(|c| {
        c.borrow_mut().insert(job_id.to_owned());
    });
}

fn kill_pid(pid: u32) {
    let _ = Command::new("kill").arg(pid.to_string()).stdout(Stdio::null()).stderr(Stdio::null()).status();
}

/// Stops every hop `owner` started (`killHops`).
pub fn kill_hops(owner: &str) {
    let ids = HOPS.with(|h| h.borrow_mut().remove(owner)).unwrap_or_default();
    for id in ids {
        let pid = PIDS.lock().unwrap().as_mut().and_then(|m| m.get(&id).copied());
        match pid {
            Some(pid) => kill_pid(pid),
            None => {
                KILLED.lock().unwrap().get_or_insert_with(HashSet::new).insert(id);
            }
        }
    }
}

/// Only a harness that forks forks (`claude --resume --fork-session`,
/// `codex exec fork`). Forking costs a transcript replay and carries
/// whatever that session was arguing about, so it has to earn its place:
/// the addressee was named, so its context is the point; a member merely
/// consulted only needs it while mid-task, and answers cheaper fresh when
/// idle (`shouldFork`).
pub fn should_fork(info: Option<&TermStatus>, role: Role) -> bool {
    let Some(info) = info else { return false };
    if !info.harness().is_some_and(Harness::forks) || info.agent_pid.is_none() {
        return false;
    }
    role == Role::Addressee || info.status == "agent" || info.status == "working"
}

/// The session behind a terminal, which is what a fork resumes from
/// (`forkFrom`); none means the hop starts fresh.
pub fn fork_from(shell: Shell, member: &HubMember, role: Role) -> Option<HopSpec> {
    let info = member.session_id.as_deref().and_then(|s| send::status_by_session(shell, s));
    if !should_fork(info.as_ref(), role) {
        return None;
    }
    let info = info?;
    Some(HopSpec { harness: info.harness()?, resume: Some(agents::fork_session(&info, &member.cwd)?) })
}

/// Where a harness is installed when the app's `PATH` (launchd's, when
/// started from the Finder) does not name it: its own installer's place,
/// then Homebrew's and the usual prefixes.
fn candidates(harness: Harness) -> Vec<PathBuf> {
    let home = dirs::home_dir().unwrap_or_default();
    let name = harness.id();
    let own = match harness {
        Harness::Claude => vec![home.join(".claude/local/claude")],
        Harness::Codex => Vec::new(),
        Harness::Omp => vec![home.join(".bun/bin/omp")],
    };
    [PathBuf::from(name)]
        .into_iter()
        .chain(own)
        .chain([home.join(".local/bin").join(name), PathBuf::from("/opt/homebrew/bin").join(name), PathBuf::from("/usr/local/bin").join(name)])
        .collect()
}

/// `PATH` with `bin`'s directory in front and the usual prefixes after,
/// so a harness that is a script (`#!/usr/bin/env node`) finds its runtime
/// under launchd's bare `PATH`, while a shell's own order still wins.
fn path_for(bin: &std::path::Path) -> OsString {
    let home = dirs::home_dir().unwrap_or_default();
    let mut dirs: Vec<PathBuf> = bin.parent().filter(|d| !d.as_os_str().is_empty()).map(PathBuf::from).into_iter().collect();
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    dirs.extend([home.join(".local/bin"), PathBuf::from("/opt/homebrew/bin"), PathBuf::from("/usr/local/bin")]);
    std::env::join_paths(dirs).unwrap_or_default()
}

/// The headless run's arguments: JSON events on stdout, every tool allowed
/// (no one is there to approve), the prompt after `--` so a leading dash
/// stays prompt.
fn args(spec: &HopSpec, prompt: &str) -> Vec<String> {
    let mut args: Vec<String> = match spec.harness {
        // stream-json refuses to start without --verbose.
        Harness::Claude => ["-p", "--output-format", "stream-json", "--verbose", "--permission-mode", "bypassPermissions"].map(str::to_owned).to_vec(),
        Harness::Codex => {
            let mut a = vec!["exec".to_owned()];
            if spec.resume.is_some() {
                a.push("fork".into());
            }
            a.extend(["--json", "--skip-git-repo-check", "--dangerously-bypass-approvals-and-sandbox"].map(str::to_owned));
            a
        }
        Harness::Omp => ["-p", "--mode", "json", "--auto-approve"].map(str::to_owned).to_vec(),
    };
    match (spec.harness, &spec.resume) {
        (Harness::Claude, Some(r)) => args.extend(["--resume".into(), r.clone(), "--fork-session".into()]),
        (Harness::Omp, Some(r)) => args.extend(["--resume".into(), r.clone()]),
        _ => {}
    }
    args.push("--".into());
    if let (Harness::Codex, Some(r)) = (spec.harness, &spec.resume) {
        args.push(r.clone());
    }
    args.push(prompt.to_owned());
    args
}

/// The text of `content`'s text blocks (`textOf`).
fn text_of(content: &Value) -> String {
    content
        .as_array()
        .map(|blocks| blocks.iter().filter(|b| b["type"].as_str() == Some("text")).filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n"))
        .unwrap_or_default()
        .trim()
        .to_owned()
}

/// What one JSON event says: an answer's text, the session id, an error.
#[derive(Default)]
struct Event {
    text: Option<String>,
    session_id: Option<String>,
    error: Option<String>,
}

fn event_of(harness: Harness, e: &Value) -> Event {
    let kind = e["type"].as_str().unwrap_or_default();
    let some = |s: &Value| s.as_str().filter(|s| !s.is_empty()).map(str::to_owned);
    match harness {
        Harness::Claude => Event {
            text: (kind == "assistant").then(|| text_of(&e["message"]["content"])),
            session_id: some(&e["session_id"]),
            error: None,
        },
        Harness::Codex => Event {
            text: (kind == "item.completed" && e["item"]["type"].as_str() == Some("agent_message")).then(|| e["item"]["text"].as_str().unwrap_or_default().trim().to_owned()),
            session_id: (kind == "thread.started").then(|| some(&e["thread_id"])).flatten(),
            error: match kind {
                "error" => some(&e["message"]),
                "turn.failed" => some(&e["error"]["message"]),
                _ => None,
            },
        },
        Harness::Omp => Event {
            text: (kind == "message_end" && e["message"]["role"].as_str() == Some("assistant")).then(|| text_of(&e["message"]["content"])),
            session_id: (kind == "session").then(|| some(&e["id"])).flatten(),
            error: None,
        },
    }
}

fn run(hop_id: &str, cwd: &str, prompt: &str, spec: &HopSpec) -> HopResult {
    let name = spec.harness.id();
    let fail = |error: String| HopResult { error: Some(error), harness: spec.harness, ..Default::default() };
    let args = args(spec, prompt);
    let mut child = None;
    let mut failure = String::new();
    for bin in candidates(spec.harness) {
        match Command::new(&bin).args(&args).env("PATH", path_for(&bin)).current_dir(cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
            Ok(c) => {
                child = Some(c);
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if failure.is_empty() {
                    failure = format!("spawn {name} ENOENT");
                }
            }
            Err(e) => {
                failure = e.to_string();
                break;
            }
        }
    }
    let Some(mut child) = child else {
        return fail(if failure.is_empty() { format!("{name} could not be started.") } else { failure });
    };
    let pid = child.id();
    PIDS.lock().unwrap().get_or_insert_with(HashMap::new).insert(hop_id.to_owned(), pid);
    if KILLED.lock().unwrap().as_mut().is_some_and(|k| k.remove(hop_id)) {
        kill_pid(pid);
    }

    let mut stderr = child.stderr.take();
    let stderr_reader = std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(s) = stderr.as_mut() {
            let _ = s.read_to_string(&mut text);
        }
        text
    });

    // The events are NDJSON; a line that is not json is the agent talking to a human.
    let (mut out, mut session_id, mut reported) = (String::new(), None, None);
    if let Some(stdout) = child.stdout.take() {
        for line in BufReader::new(stdout).split(b'\n').map_while(Result::ok) {
            let line = String::from_utf8_lossy(&line);
            let Ok(event) = serde_json::from_str::<Value>(line.trim()) else { continue };
            let event = event_of(spec.harness, &event);
            session_id = event.session_id.or(session_id);
            reported = event.error.or(reported);
            if let Some(t) = event.text.filter(|t| !t.is_empty()) {
                out = if out.is_empty() { t } else { format!("{out}\n{t}") };
            }
        }
    }
    let status = child.wait();
    let stderr = stderr_reader.join().unwrap_or_default();
    PIDS.lock().unwrap().as_mut().map(|m| m.remove(hop_id));
    let error = match status {
        Ok(s) if s.success() => None,
        Ok(s) => {
            let code = s.code().map_or("null".to_owned(), |c| c.to_string());
            Some(reported.unwrap_or_else(|| if stderr.trim().is_empty() { format!("{name} exited {code}") } else { stderr.trim().to_owned() }))
        }
        Err(e) => Some(e.to_string()),
    };
    HopResult { text: out, error, session_id, harness: spec.harness }
}

/// One headless agent, resolved when it exits. A `resume` carries on from
/// that session, so the hop reads everything a live terminal's session
/// knows without typing into its screen (`runHop`).
pub async fn run_hop(cwd: &str, prompt: &str, spec: HopSpec, owner: Option<&str>) -> HopResult {
    if owner.is_some_and(is_cancelled) {
        return HopResult { error: Some("cancelled".into()), harness: spec.harness, ..Default::default() };
    }
    let hop_id = next_id();
    if let Some(owner) = owner {
        HOPS.with(|h| {
            h.borrow_mut().entry(owner.to_owned()).or_default().insert(hop_id.clone());
        });
    }
    let (cwd, prompt, id) = (cwd.to_owned(), prompt.to_owned(), hop_id.clone());
    let result = git::off_thread(move || run(&id, &cwd, &prompt, &spec)).await;
    if let Some(owner) = owner {
        HOPS.with(|h| {
            if let Some(set) = h.borrow_mut().get_mut(owner) {
                set.remove(&hop_id);
            }
        });
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_codex_fork_names_the_session_before_the_prompt() {
        let spec = HopSpec { harness: Harness::Codex, resume: Some("01a2".into()) };
        let a = args(&spec, "-v please");
        assert_eq!(&a[..2], ["exec", "fork"]);
        assert_eq!(&a[a.len() - 3..], ["--", "01a2", "-v please"]);
    }

    #[test]
    fn each_harness_reports_its_answer_and_session() {
        let codex = [
            serde_json::json!({"type":"thread.started","thread_id":"t1"}),
            serde_json::json!({"type":"item.completed","item":{"type":"agent_message","text":"pong"}}),
        ];
        assert_eq!(event_of(Harness::Codex, &codex[0]).session_id.as_deref(), Some("t1"));
        assert_eq!(event_of(Harness::Codex, &codex[1]).text.as_deref(), Some("pong"));
        let omp = [
            serde_json::json!({"type":"session","id":"s1"}),
            serde_json::json!({"type":"message_end","message":{"role":"user","content":[{"type":"text","text":"ping"}]}}),
            serde_json::json!({"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"pong"}]}}),
        ];
        assert_eq!(event_of(Harness::Omp, &omp[0]).session_id.as_deref(), Some("s1"));
        assert_eq!(event_of(Harness::Omp, &omp[1]).text, None);
        assert_eq!(event_of(Harness::Omp, &omp[2]).text.as_deref(), Some("pong"));
    }
}
