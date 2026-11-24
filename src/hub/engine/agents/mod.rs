//! What runs in a terminal: Bolt's `pty:status` (`src/main.js`) and the
//! agent adapters behind it (`src/agents/claude.js`, `omp.js`). Any program
//! on [`OTHER_AGENTS`] or a [`Harness`] counts as an agent for the status;
//! a harness also has its session read: the chip's label, whether it waits
//! for you, its transcript (`claude:session-brief`) and what a hop forks.
//! Everything here runs on the engine thread and must stay small:
//! transcripts are cached by mtime, and the process table (`ps`, 100+ ms)
//! and codex's open files (`lsof`) are read on threads of their own.

mod claude;
mod codex;
mod omp;

pub use codex::codex_home;

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
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::state;

use super::TermStatus;

/// A pty streak counts as the agent working once it has lasted this long,
/// and a streak breaks after this long without output.
const ACTIVITY_WINDOW_MS: f64 = 5000.0;
const PULSE_DELAY_MS: f64 = 1000.0;
/// How old the process table gets before it is read again.
const PS_TTL_MS: f64 = 1000.0;
/// The last turns a brief keeps.
const BRIEF_MESSAGES: usize = 8;
/// How much of a session file's end is read for its latest turns.
const TAIL_BYTES: u64 = 256 * 1024;

/// An agent CLI whose sessions Rivet reads: what a terminal running one
/// shows, what a headless hop runs, what it forks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Harness {
    #[default]
    Claude,
    Codex,
    Omp,
}

impl Harness {
    pub const ALL: [Harness; 3] = [Harness::Claude, Harness::Codex, Harness::Omp];

    /// Its program's name, and its id in settings and statuses.
    pub fn id(self) -> &'static str {
        match self {
            Harness::Claude => "claude",
            Harness::Codex => "codex",
            Harness::Omp => "omp",
        }
    }

    pub fn from_id(id: &str) -> Option<Harness> {
        Harness::ALL.into_iter().find(|h| h.id() == id)
    }

    /// Whether a headless hop can fork a live session of it without
    /// touching that session. omp has no fork, so its terminals are asked
    /// fresh.
    pub fn forks(self) -> bool {
        matches!(self, Harness::Claude | Harness::Codex)
    }
}

/// Agent CLIs known by name only: a terminal running one shows as an
/// agent, and its output is read from the screen.
const OTHER_AGENTS: &[&str] = &["gemini", "opencode", "aider", "amp", "cursor-agent", "crush", "qwen", "droid", "copilot"];

/// What starts an agent CLI rather than being one: the runtimes it is a
/// script of (`node /opt/homebrew/bin/codex`) and the wrappers it runs
/// under (`caffeinate -i claude`).
const LAUNCHERS: &[&str] = &["node", "bun", "deno", "python", "python3", "env", "caffeinate", "nice", "nohup", "time"];

struct Proc {
    tpgid: i64,
    tty: String,
    args: String,
}

/// What a harness adapter reads about its session (`info`).
#[derive(Clone, Default)]
struct AgentInfo {
    label: Option<String>,
    needs_input: bool,
    mid_turn: bool,
    session_id: Option<String>,
    resume: Option<String>,
    prompt: Option<String>,
    transcript: Option<PathBuf>,
}

/// A claude question still waiting for its answer (`pendingQuestion`).
#[derive(Clone)]
pub struct PendingQuestion {
    pub tool_use_id: String,
    pub header: Option<String>,
    pub question: String,
}

/// What a terminal's agent is doing, read from its transcript rather than
/// the screen (`claude:session-brief`).
#[derive(Clone, Default)]
pub struct Brief {
    pub pending_question: Option<PendingQuestion>,
    /// `(role, text)`, the last few turns; role is `user` or `assistant`.
    pub messages: Vec<(String, String)>,
}

/// The last process table read and when.
static PS: Mutex<Option<(f64, Arc<HashMap<u32, Proc>>)>> = Mutex::new(None);
/// A read of the process table is under way.
static PS_READING: AtomicBool = AtomicBool::new(false);

thread_local! {
    static STREAKS: RefCell<HashMap<u64, (f64, f64)>> = RefCell::new(HashMap::new());
    static BRIEFS: RefCell<HashMap<PathBuf, (SystemTime, Brief)>> = RefCell::new(HashMap::new());
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

/// The program a process line runs: the first word that is not a launcher,
/// a flag, a `K=V` or a number, by its name.
fn program_of(args: &str) -> Option<&str> {
    args.split_whitespace()
        .filter(|w| !w.starts_with('-') && !w.contains('=') && w.parse::<f64>().is_err())
        .map(|w| w.rsplit('/').next().unwrap_or(w))
        .find(|w| !LAUNCHERS.contains(w))
}

/// The agent a process line runs, by its id.
fn agent_for(args: &str) -> Option<&'static str> {
    let program = program_of(args)?;
    Harness::ALL.iter().map(|h| h.id()).chain(OTHER_AGENTS.iter().copied()).find(|a| *a == program)
}

fn mtime_of(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn read_range(path: &Path, start: u64, length: u64) -> Option<String> {
    let mut f = fs::File::open(path).ok()?;
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    f.take(length).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// The whole lines in the last [`TAIL_BYTES`] of `path`, and whether that
/// left out its start.
fn tail_lines(path: &Path) -> Option<(Vec<String>, bool)> {
    let size = fs::metadata(path).ok()?.len();
    let start = size.saturating_sub(TAIL_BYTES);
    let tail = read_range(path, start, size - start)?;
    let mut lines: Vec<String> = tail.split('\n').map(str::to_owned).collect();
    if start > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    Some((lines, start > 0))
}

/// `title · prompt`, else whichever there is.
fn label_of(title: Option<&str>, prompt: Option<&str>) -> Option<String> {
    match (title, prompt) {
        (Some(t), Some(p)) => Some(format!("{t} · {p}")),
        (Some(t), None) => Some(t.to_owned()),
        (None, p) => p.map(str::to_owned),
    }
}

/// The text of the first `text` block of `content`, or `content` itself
/// when it is a string; trimmed, none when empty.
fn first_text(content: &Value) -> Option<String> {
    let text = match content {
        Value::String(s) => Some(s.as_str()),
        Value::Array(blocks) => blocks.iter().find(|b| b["type"].as_str() == Some("text")).and_then(|b| b["text"].as_str()),
        _ => None,
    }?;
    let t = text.trim();
    (!t.is_empty()).then(|| t.to_owned())
}

/// The last [`BRIEF_MESSAGES`] of `messages`.
fn last_turns(mut messages: Vec<(String, String)>) -> Vec<(String, String)> {
    let keep = messages.len().saturating_sub(BRIEF_MESSAGES);
    messages.split_off(keep)
}

/// The agent in terminal `info`'s brief: its harness's read of the
/// transcript, again only when the file changed. None for a terminal with
/// no harness session.
pub fn session_brief(info: &TermStatus) -> Option<Brief> {
    let harness = info.harness()?;
    let file = info.agent_transcript.as_deref()?;
    let mtime = mtime_of(file)?;
    if let Some(hit) = BRIEFS.with(|b| b.borrow().get(file).filter(|(m, _)| *m == mtime).map(|(_, b)| b.clone())) {
        return Some(hit);
    }
    let brief = match harness {
        Harness::Claude => claude::brief(file),
        Harness::Codex => codex::brief(file),
        Harness::Omp => omp::brief(file),
    }?;
    BRIEFS.with(|b| {
        b.borrow_mut().insert(file.to_owned(), (mtime, brief.clone()));
    });
    Some(brief)
}

/// The text of the last assistant turn of terminal `info`'s agent; none
/// when its transcript can't be read, so the caller reads the screen.
pub fn last_message(info: &TermStatus) -> Option<String> {
    let brief = session_brief(info)?;
    Some(brief.messages.into_iter().rev().find(|(r, _)| r == "assistant").map(|(_, t)| t).unwrap_or_default())
}

/// The session a headless hop forks to carry terminal `info`'s context,
/// when its agent is still that session and runs in `cwd`.
pub fn fork_session(info: &TermStatus, cwd: &str) -> Option<String> {
    match info.harness()? {
        Harness::Claude => claude::session_of(info.agent_pid?, cwd),
        Harness::Codex => info.agent_session_id.clone(),
        Harness::Omp => None,
    }
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
    let agent = foreground.and_then(|(fg, p)| agent_for(&p.args).map(|a| (a, fg, p.tty.clone())));

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
    let found = agent.as_ref().and_then(|(a, fg, tty)| match Harness::from_id(a)? {
        Harness::Claude => claude::info(*fg),
        Harness::Codex => codex::info(*fg, &procs),
        Harness::Omp => omp::info(tty),
    });
    let found = found.unwrap_or_default();
    TermStatus {
        status: status.into(),
        agent: agent.as_ref().map(|(a, ..)| (*a).to_owned()),
        agent_pid: agent.as_ref().map(|(_, fg, _)| *fg),
        label: found.label.unwrap_or_default(),
        needs_input: found.needs_input,
        mid_turn: found.mid_turn,
        prompt: found.prompt,
        agent_session_id: found.session_id,
        agent_resume: found.resume,
        agent_transcript: found.transcript,
        last_output_at: last_output,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agents_are_known_by_their_program_past_launchers() {
        assert_eq!(agent_for("claude --resume abc"), Some("claude"));
        assert_eq!(agent_for("/Users/me/.local/bin/omp --resume /x/s.jsonl"), Some("omp"));
        assert_eq!(agent_for("node /opt/homebrew/bin/codex resume 01a2"), Some("codex"));
        assert_eq!(agent_for("node --no-warnings /usr/local/bin/gemini -p hi"), Some("gemini"));
        assert_eq!(agent_for("caffeinate -i claude"), Some("claude"));
        assert_eq!(agent_for("env FOO=1 nice -n 10 omp"), Some("omp"));
        assert_eq!(agent_for("vim claude.md"), None);
        assert_eq!(agent_for("rg omp src"), None);
        assert_eq!(agent_for("node server.js"), None);
    }
}
