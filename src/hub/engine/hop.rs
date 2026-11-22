//! Headless claude hops (`runHop`, `killHops`, `shouldFork`, `forkFrom` of
//! `src/stores/fleetStore.ts`, and `agents:run` / `agents:cancel` of
//! `src/main.js`): one `claude -p` per invocation, resolved when it exits.

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    io::{BufRead, BufReader, Read},
    path::PathBuf,
    process::{Command, Stdio},
    sync::Mutex,
};

use serde_json::Value;

use crate::{git, hub::{HubMember, next_id}, state::Shell};

use super::{TermStatus, agents, send};

/// Whose context a fork is for: the member named, or one merely consulted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Addressee,
    #[allow(dead_code)]
    Consulted,
}

/// What a hop said, and the session it ran in.
#[derive(Debug, Clone, Default)]
pub struct HopResult {
    pub text: String,
    pub error: Option<String>,
    pub session_id: Option<String>,
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

/// Only a claude session forks: the hop is `claude --resume --fork-session`.
/// Forking costs a transcript replay and carries whatever that session was
/// arguing about, so it has to earn its place: the addressee was named, so
/// its context is the point; a member merely consulted only needs it while
/// mid-task, and answers cheaper fresh when idle (`shouldFork`).
pub fn should_fork(info: Option<&TermStatus>, role: Role) -> bool {
    let Some(info) = info else { return false };
    if info.agent.as_deref() != Some("claude") || info.agent_pid.is_none() {
        return false;
    }
    role == Role::Addressee || info.status == "agent" || info.status == "working"
}

/// The claude transcript behind a terminal, which is what a fork resumes
/// from (`forkFrom`).
pub fn fork_from(shell: Shell, member: &HubMember, role: Role) -> Option<String> {
    let info = member.session_id.as_deref().and_then(|s| send::status_by_session(shell, s));
    if !should_fork(info.as_ref(), role) {
        return None;
    }
    agents::claude_session_of(info?.agent_pid?, &member.cwd)
}

/// The text of an assistant event's text blocks (`textOf`).
fn text_of(event: &Value) -> String {
    event["message"]["content"]
        .as_array()
        .map(|blocks| {
            blocks.iter().filter(|b| b["type"].as_str() == Some("text")).filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n")
        })
        .unwrap_or_default()
        .trim()
        .to_owned()
}

fn claude_candidates() -> Vec<PathBuf> {
    let home = dirs::home_dir().unwrap_or_default();
    vec![
        PathBuf::from("claude"),
        home.join(".claude/local/claude"),
        home.join(".local/bin/claude"),
        PathBuf::from("/opt/homebrew/bin/claude"),
        PathBuf::from("/usr/local/bin/claude"),
    ]
}

fn run_claude(hop_id: &str, cwd: &str, prompt: &str, resume: Option<&str>) -> HopResult {
    let mut args = vec![
        "-p".to_owned(),
        prompt.to_owned(),
        "--output-format".into(),
        "stream-json".into(),
        // stream-json refuses to start without it.
        "--verbose".into(),
        "--permission-mode".into(),
        "bypassPermissions".into(),
    ];
    if let Some(resume) = resume {
        args.extend(["--resume".into(), resume.to_owned(), "--fork-session".into()]);
    }
    let mut child = None;
    let mut failure = String::new();
    for bin in claude_candidates() {
        match Command::new(&bin).args(&args).current_dir(cwd).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
            Ok(c) => {
                child = Some(c);
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if failure.is_empty() {
                    failure = "spawn claude ENOENT".into();
                }
            }
            Err(e) => {
                failure = e.to_string();
                break;
            }
        }
    }
    let Some(mut child) = child else {
        let error = if failure.is_empty() { "claude could not be started.".to_owned() } else { failure };
        return HopResult { text: String::new(), error: Some(error), session_id: None };
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

    // stream-json is NDJSON; a line that is not json is claude talking to a human.
    let mut out = String::new();
    let mut session_id = None;
    if let Some(stdout) = child.stdout.take() {
        for line in BufReader::new(stdout).split(b'\n').map_while(Result::ok) {
            let line = String::from_utf8_lossy(&line);
            let Ok(event) = serde_json::from_str::<Value>(line.trim()) else { continue };
            if let Some(s) = event["session_id"].as_str().filter(|s| !s.is_empty()) {
                session_id = Some(s.to_owned());
            }
            if event["type"].as_str() == Some("assistant") {
                let t = text_of(&event);
                if !t.is_empty() {
                    if out.is_empty() {
                        out = t;
                    } else {
                        out = format!("{out}\n{t}");
                    }
                }
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
            Some(if stderr.trim().is_empty() { format!("claude exited {code}") } else { stderr.trim().to_owned() })
        }
        Err(e) => Some(e.to_string()),
    };
    HopResult { text: out, error, session_id }
}

/// One headless claude, resolved when it exits. `resume` forks a live
/// terminal's transcript, so the hop reads everything that session knows
/// without typing into its screen (`runHop`).
pub async fn run_hop(cwd: &str, prompt: &str, resume: Option<String>, owner: Option<&str>) -> HopResult {
    if owner.is_some_and(is_cancelled) {
        return HopResult { error: Some("cancelled".into()), ..Default::default() };
    }
    let hop_id = next_id();
    if let Some(owner) = owner {
        HOPS.with(|h| {
            h.borrow_mut().entry(owner.to_owned()).or_default().insert(hop_id.clone());
        });
    }
    let (cwd, prompt, id) = (cwd.to_owned(), prompt.to_owned(), hop_id.clone());
    let result = git::off_thread(move || run_claude(&id, &cwd, &prompt, resume.as_deref())).await;
    if let Some(owner) = owner {
        HOPS.with(|h| {
            if let Some(set) = h.borrow_mut().get_mut(owner) {
                set.remove(&hop_id);
            }
        });
    }
    result
}
