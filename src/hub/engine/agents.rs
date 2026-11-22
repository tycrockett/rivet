//! What runs in a terminal: Bolt's `pty:status` (`src/main.js`), the agent
//! adapters (`src/agents/claude.js`, `omp.js`) and claude's transcript
//! reads (`claude:sessions`, `claude:session-brief`). Everything here runs
//! on the engine thread and must stay small: transcripts are cached by
//! mtime, and the process table (`ps`, 100+ ms) is read on a thread of its
//! own.

use std::{
    cell::RefCell,
    collections::HashMap,
    fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::SystemTime,
};

use nsh_core::time::unix_ms;
use serde_json::Value;

use crate::state;

use super::TermStatus;

/// A pty streak counts as the agent working once it has lasted this long,
/// and a streak breaks after this long without output.
const ACTIVITY_WINDOW_MS: f64 = 5000.0;
const PULSE_DELAY_MS: f64 = 1000.0;
/// How old the process table gets before it is read again.
const PS_TTL_MS: f64 = 1000.0;
/// An active claude rewrites its transcript every tick, so the mtime key
/// alone never hits for the sessions being polled.
const LABEL_MIN_INTERVAL_MS: f64 = 10_000.0;
const BRIEF_MESSAGES: usize = 8;
const HEAD_BYTES: u64 = 4096;
const TAIL_BYTES: u64 = 256 * 1024;

struct Proc {
    tpgid: i64,
    tty: String,
    args: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Agent {
    Claude,
    Omp,
}

impl Agent {
    fn id(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Omp => "omp",
        }
    }
}

/// What an agent adapter reads about its session (`info`).
#[derive(Clone, Default)]
struct AgentInfo {
    label: Option<String>,
    needs_input: bool,
    session_id: Option<String>,
    resume: Option<String>,
    prompt: Option<String>,
}

struct Summary {
    mtime: SystemTime,
    label: Option<String>,
    prompt: Option<String>,
    at: f64,
}

struct OmpCached {
    mtime: SystemTime,
    size: u64,
    info: AgentInfo,
}

/// The last process table read and when.
static PS: Mutex<Option<(f64, Arc<HashMap<u32, Proc>>)>> = Mutex::new(None);
/// A read of the process table is under way.
static PS_READING: AtomicBool = AtomicBool::new(false);

thread_local! {
    static STREAKS: RefCell<HashMap<u64, (f64, f64)>> = RefCell::new(HashMap::new());
    static SUMMARIES: RefCell<HashMap<String, Summary>> = RefCell::new(HashMap::new());
    static OMP: RefCell<HashMap<PathBuf, OmpCached>> = RefCell::new(HashMap::new());
    static BRIEFS: RefCell<HashMap<String, (SystemTime, Brief)>> = RefCell::new(HashMap::new());
}

fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

/// Every process's `(pid, tpgid, tty, args)`, about a second old at most.
/// Only the very first call waits for `ps`; after that an old table starts
/// a read on another thread and is returned as it is, so the engine thread
/// never stalls on `ps`.
fn processes() -> Arc<HashMap<u32, Proc>> {
    let last = PS.lock().unwrap().clone();
    let Some((at, table)) = last else {
        let table = Arc::new(read_processes());
        *PS.lock().unwrap() = Some((unix_ms(), table.clone()));
        return table;
    };
    if unix_ms() - at >= PS_TTL_MS && !PS_READING.swap(true, Ordering::AcqRel) {
        std::thread::spawn(|| {
            let table = Arc::new(read_processes());
            *PS.lock().unwrap() = Some((unix_ms(), table));
            PS_READING.store(false, Ordering::Release);
        });
    }
    table
}

fn read_processes() -> HashMap<u32, Proc> {
    let out = Command::new("ps").args(["-A", "-o", "pid=", "-o", "tpgid=", "-o", "tty=", "-o", "args="]).output();
    let mut table = HashMap::new();
    if let Ok(out) = out.as_ref().map(|o| o.stdout.clone()) {
        for line in String::from_utf8_lossy(&out).lines() {
            let mut parts = line.split_whitespace();
            let (Some(pid), Some(tpgid), Some(tty)) = (parts.next(), parts.next(), parts.next()) else { continue };
            let args = parts.collect::<Vec<_>>().join(" ");
            if let (Ok(pid), Ok(tpgid)) = (pid.parse::<u32>(), tpgid.parse::<i64>()) {
                if pid > 0 {
                    table.insert(pid, Proc { tpgid, tty: tty.to_owned(), args });
                }
            }
        }
    }
    table
}

/// `/(^|[\s/])claude(\s|$)/`.
fn is_claude(args: &str) -> bool {
    args.match_indices("claude").any(|(i, w)| {
        let before = args[..i].chars().next_back();
        let after = args[i + w.len()..].chars().next();
        before.is_none_or(|c| c.is_whitespace() || c == '/') && after.is_none_or(char::is_whitespace)
    })
}

/// argv[0]'s basename is `omp`, not a word anywhere in the line.
fn is_omp(args: &str) -> bool {
    args.split_whitespace().next().is_some_and(|a| a.rsplit('/').next() == Some("omp"))
}

fn agent_for(args: &str) -> Option<Agent> {
    if is_claude(args) {
        Some(Agent::Claude)
    } else if is_omp(args) {
        Some(Agent::Omp)
    } else {
        None
    }
}

fn mtime_of(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|m| m.modified()).ok()
}

// ---- claude -----------------------------------------------------------------

/// `~/.claude/sessions/<pid>.json`: `{ pid, sessionId, cwd, status }`.
pub struct Registry {
    pub session_id: String,
    pub cwd: String,
    pub status: String,
}

pub fn claude_registry(pid: u32) -> Option<Registry> {
    let text = fs::read_to_string(home().join(".claude").join("sessions").join(format!("{pid}.json"))).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    Some(Registry {
        session_id: v["sessionId"].as_str()?.to_owned(),
        cwd: v["cwd"].as_str()?.to_owned(),
        status: v["status"].as_str().unwrap_or_default().to_owned(),
    })
}

/// `~/.claude/projects/<cwd, non-alphanumerics dashed>/<session>.jsonl`.
fn transcript_file(session_id: &str, cwd: &str) -> PathBuf {
    let dir: String = cwd.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    home().join(".claude").join("projects").join(dir).join(format!("{session_id}.jsonl"))
}

/// The human-typed text of a `user` record, or none for tool results and
/// meta blocks (`promptFromUserRecord`).
fn prompt_from_user_record(rec: &Value) -> Option<String> {
    if rec["isMeta"].as_bool().unwrap_or(false) {
        return None;
    }
    let msg = rec.get("message")?;
    if msg["role"].as_str() != Some("user") {
        return None;
    }
    let text = match &msg["content"] {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => {
            if blocks.iter().any(|b| b["type"].as_str() == Some("tool_result")) {
                return None;
            }
            blocks.iter().find(|b| b["type"].as_str() == Some("text"))?["text"].as_str()?.to_owned()
        }
        _ => return None,
    };
    // A slash command is stored as <command-name>/foo</command-name>.
    if let Some(name) = between(&text, "<command-name>", "</command-name>") {
        let args = between(&text, "<command-args>", "</command-args>").map(str::trim).filter(|a| !a.is_empty());
        return Some(match args {
            Some(a) => format!("{name} {a}"),
            None => name.to_owned(),
        }
        .trim()
        .to_owned());
    }
    let t = text.trim();
    (!t.is_empty() && !t.starts_with('<') && !t.starts_with("Caveat:")).then(|| t.to_owned())
}

fn between<'a>(text: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = text.find(open)? + open.len();
    let end = text[start..].find(close)?;
    Some(&text[start..start + end])
}

/// The session's AI title with the last prompt appended, else the prompt
/// (`transcriptSummary`).
fn transcript_summary(session_id: &str, cwd: &str) -> Option<(Option<String>, Option<String>)> {
    let file = transcript_file(session_id, cwd);
    let mtime = mtime_of(&file)?;
    let now = unix_ms();
    if let Some(hit) = SUMMARIES.with(|s| s.borrow().get(session_id).filter(|c| c.mtime == mtime || now - c.at < LABEL_MIN_INTERVAL_MS).map(|c| (c.label.clone(), c.prompt.clone()))) {
        return Some(hit);
    }
    let (mut title, mut prompt) = (None, None);
    if let Ok(text) = fs::read_to_string(&file) {
        for line in text.lines().rev() {
            if title.is_some() && prompt.is_some() {
                break;
            }
            if line.is_empty() {
                continue;
            }
            let is_title = title.is_none() && line.contains("\"ai-title\"");
            let is_user = prompt.is_none() && line.contains("\"user\"");
            if !is_title && !is_user {
                continue;
            }
            let Ok(rec) = serde_json::from_str::<Value>(line) else { continue };
            if is_title {
                if let Some(t) = rec["aiTitle"].as_str().map(str::trim).filter(|t| !t.is_empty()) {
                    title = Some(t.to_owned());
                }
            } else if let Some(p) = prompt_from_user_record(&rec) {
                prompt = Some(p);
            }
        }
    }
    let label = match (&title, &prompt) {
        (Some(t), Some(p)) => Some(format!("{t} · {p}")),
        (Some(t), None) => Some(t.clone()),
        (None, p) => p.clone(),
    };
    SUMMARIES.with(|s| {
        s.borrow_mut().insert(session_id.to_owned(), Summary { mtime, label: label.clone(), prompt: prompt.clone(), at: now });
    });
    Some((label, prompt))
}

fn claude_info(pid: u32) -> Option<AgentInfo> {
    let reg = claude_registry(pid)?;
    let (label, prompt) = transcript_summary(&reg.session_id, &reg.cwd).unwrap_or((None, None));
    Some(AgentInfo { label, prompt, needs_input: reg.status == "waiting", session_id: Some(reg.session_id.clone()), resume: Some(reg.session_id) })
}

/// A claude question still waiting for its answer (`pendingQuestion`).
#[derive(Clone)]
pub struct PendingQuestion {
    pub tool_use_id: String,
    pub header: Option<String>,
    pub question: String,
}

/// What a terminal's claude is doing, read from the transcript rather than
/// the screen (`claude:session-brief`).
#[derive(Clone, Default)]
pub struct Brief {
    pub pending_question: Option<PendingQuestion>,
    /// `(role, text)`, the last few turns; role is `user` or `assistant`.
    pub messages: Vec<(String, String)>,
}

pub fn session_brief(pid: u32) -> Option<Brief> {
    let reg = claude_registry(pid)?;
    let file = transcript_file(&reg.session_id, &reg.cwd);
    let mtime = mtime_of(&file)?;
    if let Some(hit) = BRIEFS.with(|b| b.borrow().get(&reg.session_id).filter(|(m, _)| *m == mtime).map(|(_, b)| b.clone())) {
        return Some(hit);
    }
    let text = fs::read_to_string(&file).ok()?;
    let mut asked: Vec<Value> = Vec::new();
    let mut answered: Vec<String> = Vec::new();
    let mut messages: Vec<(String, String)> = Vec::new();
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        let Ok(rec) = serde_json::from_str::<Value>(line) else { continue };
        let role = rec["message"]["role"].as_str().unwrap_or_default();
        if let Some(blocks) = rec["message"]["content"].as_array() {
            for block in blocks {
                match block["type"].as_str() {
                    Some("tool_use") if block["name"].as_str() == Some("AskUserQuestion") => asked.push(block.clone()),
                    Some("tool_result") => {
                        if let Some(id) = block["tool_use_id"].as_str().filter(|i| !i.is_empty()) {
                            answered.push(id.to_owned());
                        }
                    }
                    Some("text") => {
                        if let Some(t) = block["text"].as_str().map(str::trim).filter(|t| !t.is_empty()) {
                            messages.push((if role == "user" { "user" } else { "assistant" }.to_owned(), t.to_owned()));
                        }
                    }
                    _ => {}
                }
            }
        }
        // A plain string user turn carries no content blocks.
        if role == "user" {
            if let Some(t) = rec["message"]["content"].as_str().map(str::trim).filter(|t| !t.is_empty()) {
                messages.push(("user".into(), t.to_owned()));
            }
        }
    }
    // Last one wins: an earlier unanswered ask was superseded, not queued.
    let live = asked.iter().rev().find(|b| !b["id"].as_str().is_some_and(|id| answered.iter().any(|a| a == id)));
    let pending_question = live.and_then(|b| {
        let q = b["input"]["questions"].get(0)?;
        Some(PendingQuestion {
            tool_use_id: b["id"].as_str().unwrap_or_default().to_owned(),
            header: q["header"].as_str().filter(|h| !h.is_empty()).map(str::to_owned),
            question: q["question"].as_str().unwrap_or_default().to_owned(),
        })
    });
    let keep = messages.len().saturating_sub(BRIEF_MESSAGES);
    let brief = Brief { pending_question, messages: messages.split_off(keep) };
    BRIEFS.with(|b| {
        b.borrow_mut().insert(reg.session_id.clone(), (mtime, brief.clone()));
    });
    Some(brief)
}

/// The text of the last assistant turn of the claude with `pid`.
pub fn last_claude_message(pid: u32) -> String {
    session_brief(pid).and_then(|b| b.messages.into_iter().rev().find(|(r, _)| r == "assistant").map(|(_, t)| t)).unwrap_or_default()
}

/// The session id of the live claude `pid` when its cwd is `directory`
/// (`claude:sessions`, filtered to one pid).
pub fn claude_session_of(pid: u32, directory: &str) -> Option<String> {
    let reg = claude_registry(pid)?;
    let same = |a: &str, b: &str| a.trim_end_matches('/') == b.trim_end_matches('/');
    (same(&reg.cwd, directory) && processes().contains_key(&pid)).then_some(reg.session_id)
}

// ---- omp --------------------------------------------------------------------

fn omp_session_file(tty: &str) -> Option<PathBuf> {
    if tty.is_empty() || tty == "??" {
        return None;
    }
    let text = fs::read_to_string(home().join(".omp").join("agent").join("terminal-sessions").join(tty)).ok()?;
    let line = text.split('\n').nth(1)?.trim();
    (!line.is_empty()).then(|| PathBuf::from(line))
}

fn read_range(path: &Path, start: u64, length: u64) -> Option<String> {
    let mut f = fs::File::open(path).ok()?;
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    f.take(length).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

fn omp_prompt_of(msg: &Value) -> Option<String> {
    let text = match &msg["content"] {
        Value::String(s) => Some(s.as_str()),
        Value::Array(blocks) => blocks.iter().find(|b| b["type"].as_str() == Some("text")).and_then(|b| b["text"].as_str()),
        _ => None,
    }?;
    let t = text.trim();
    (!t.is_empty()).then(|| t.to_owned())
}

fn omp_info(tty: &str) -> Option<AgentInfo> {
    let file = omp_session_file(tty)?;
    let meta = fs::metadata(&file).ok()?;
    let (mtime, size) = (meta.modified().ok()?, meta.len());
    if let Some(hit) = OMP.with(|c| c.borrow().get(&file).filter(|c| c.mtime == mtime && c.size == size).map(|c| c.info.clone())) {
        return Some(hit);
    }
    let start = size.saturating_sub(TAIL_BYTES);
    let head = read_range(&file, 0, HEAD_BYTES)?;
    let tail = read_range(&file, start, size - start)?;
    let (mut title, mut session_id) = (None, None);
    for line in head.split('\n').take(2) {
        let Ok(rec) = serde_json::from_str::<Value>(line) else { continue };
        if rec["type"].as_str() == Some("title") {
            if let Some(t) = rec["title"].as_str().map(str::trim).filter(|t| !t.is_empty()) {
                title = Some(t.to_owned());
            }
        }
        if rec["type"].as_str() == Some("session") {
            if let Some(id) = rec["id"].as_str() {
                session_id = Some(id.to_owned());
            }
        }
    }
    // Walk back to the latest assistant turn: it is waiting on you when it called `ask`
    // and no toolResult carrying that call's id has landed since.
    let mut lines: Vec<&str> = tail.split('\n').collect();
    if start > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    let mut answered: Vec<String> = Vec::new();
    let mut needs_input: Option<bool> = None;
    let mut prompt: Option<String> = None;
    for line in lines.iter().rev() {
        if needs_input.is_some() && prompt.is_some() {
            break;
        }
        if !line.contains("\"message\"") {
            continue;
        }
        let Ok(rec) = serde_json::from_str::<Value>(line) else { continue };
        let msg = &rec["message"];
        if !msg.is_object() {
            continue;
        }
        match msg["role"].as_str() {
            Some("toolResult") => {
                if needs_input.is_none() {
                    if let Some(id) = msg["toolCallId"].as_str().filter(|i| !i.is_empty()) {
                        answered.push(id.to_owned());
                    }
                }
            }
            Some("assistant") => {
                if needs_input.is_none() {
                    needs_input = Some(msg["content"].as_array().is_some_and(|c| {
                        c.iter().any(|b| {
                            b["type"].as_str() == Some("toolCall")
                                && b["name"].as_str() == Some("ask")
                                && !b["id"].as_str().is_some_and(|id| answered.iter().any(|a| a == id))
                        })
                    }));
                }
            }
            Some("user") => {
                if needs_input.is_none() {
                    needs_input = Some(false);
                }
                if prompt.is_none() {
                    prompt = omp_prompt_of(msg);
                }
            }
            _ => {}
        }
    }
    // A long autonomous run can push the last prompt out of the tail.
    let before = OMP.with(|c| c.borrow().get(&file).map(|c| (c.info.session_id.clone(), c.info.prompt.clone())));
    if prompt.is_none() {
        if let Some((sid, p)) = &before {
            if *sid == session_id {
                prompt = p.clone();
            }
        }
    }
    if prompt.is_none() && before.is_none() && start > 0 {
        if let Ok(all) = fs::read_to_string(&file) {
            for line in all.lines().rev() {
                if !line.contains("\"role\":\"user\"") {
                    continue;
                }
                let Ok(rec) = serde_json::from_str::<Value>(line) else { continue };
                if rec["message"]["role"].as_str() == Some("user") {
                    prompt = omp_prompt_of(&rec["message"]);
                    if prompt.is_some() {
                        break;
                    }
                }
            }
        }
    }
    let label = match (&title, &prompt) {
        (Some(t), Some(p)) => Some(format!("{t} · {p}")),
        (Some(t), None) => Some(t.clone()),
        (None, p) => p.clone(),
    };
    let info = AgentInfo {
        label,
        prompt,
        needs_input: needs_input.unwrap_or(false),
        session_id,
        // By path, not id: an id resolves only against omp's sessions for the cwd it is started in.
        resume: Some(file.to_string_lossy().into_owned()),
    };
    OMP.with(|c| {
        c.borrow_mut().insert(file, OmpCached { mtime, size, info: info.clone() });
    });
    Some(info)
}

// ---- pty:status ---------------------------------------------------------------

/// Terminal `id`'s live state as `pty:status` reports it: `idle` at the
/// shell's prompt, `working` while something runs in front (or an agent
/// rests at its prompt), `agent` while an agent streams output.
pub fn status_of(id: u64) -> TermStatus {
    let last_output = state::last_output_at(id).unwrap_or(0.0);
    let now = unix_ms();
    let Some(pid) = state::pty_pid(id) else {
        return TermStatus { status: "idle".into(), last_output_at: last_output, ..Default::default() };
    };
    let procs = processes();
    let tpgid = procs.get(&pid).map(|p| p.tpgid);
    let has_foreground = tpgid.is_some_and(|t| t > 0 && t != i64::from(pid));
    let foreground = tpgid.filter(|_| has_foreground).and_then(|t| u32::try_from(t).ok()).and_then(|t| procs.get(&t).map(|p| (t, p)));
    let agent = foreground.and_then(|(_, p)| agent_for(&p.args).map(|a| (a, p.tty.clone())));

    // The streak's start: a gap longer than the activity window begins a new one.
    let since = STREAKS.with(|s| {
        let mut s = s.borrow_mut();
        let e = s.entry(id).or_insert((last_output, last_output));
        if e.0 != last_output {
            if last_output - e.0 > ACTIVITY_WINDOW_MS {
                e.1 = last_output;
            }
            e.0 = last_output;
        }
        e.1
    });
    let active = now - last_output <= ACTIVITY_WINDOW_MS;
    let sustained = active && now - since >= PULSE_DELAY_MS;
    let status = if agent.is_some() {
        if sustained { "agent" } else { "working" }
    } else if has_foreground {
        "working"
    } else {
        "idle"
    };
    let found = agent.as_ref().and_then(|(a, tty)| match a {
        Agent::Claude => foreground.and_then(|(pid, _)| claude_info(pid)),
        Agent::Omp => omp_info(tty),
    });
    let found = found.unwrap_or_default();
    TermStatus {
        status: status.into(),
        agent: agent.as_ref().map(|(a, _)| a.id().to_owned()),
        agent_pid: agent.as_ref().and(foreground.map(|(p, _)| p)),
        label: found.label.unwrap_or_default(),
        needs_input: found.needs_input,
        prompt: found.prompt,
        agent_session_id: found.session_id,
        agent_resume: found.resume,
        last_output_at: last_output,
    }
}
