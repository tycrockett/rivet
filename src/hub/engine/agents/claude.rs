//! Claude Code (`src/agents/claude.js`, `claude:sessions`): its registry
//! `~/.claude/sessions/<pid>.json` names the session, and the transcript
//! `~/.claude/projects/<cwd>/<session>.jsonl` holds the rest.

use std::{
    cell::RefCell,
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    time::SystemTime,
};

use nsh_core::time::unix_ms;
use serde_json::Value;

use super::{AgentInfo, Brief, PendingQuestion, home, label_of, last_turns, mtime_of, processes};

/// An active claude rewrites its transcript every tick, so the mtime key
/// alone never hits for the sessions being polled.
const LABEL_MIN_INTERVAL_MS: f64 = 10_000.0;

struct Summary {
    mtime: SystemTime,
    label: Option<String>,
    prompt: Option<String>,
    at: f64,
}

thread_local! {
    static SUMMARIES: RefCell<HashMap<String, Summary>> = RefCell::new(HashMap::new());
}

/// `~/.claude/sessions/<pid>.json`: `{ pid, sessionId, cwd, status }`.
struct Registry {
    session_id: String,
    cwd: String,
    status: String,
}

fn registry(pid: u32) -> Option<Registry> {
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
fn transcript_summary(session_id: &str, file: &Path) -> Option<(Option<String>, Option<String>)> {
    let mtime = mtime_of(file)?;
    let now = unix_ms();
    if let Some(hit) = SUMMARIES.with(|s| s.borrow().get(session_id).filter(|c| c.mtime == mtime || now - c.at < LABEL_MIN_INTERVAL_MS).map(|c| (c.label.clone(), c.prompt.clone()))) {
        return Some(hit);
    }
    let (mut title, mut prompt) = (None, None);
    if let Ok(text) = fs::read_to_string(file) {
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
    let label = label_of(title.as_deref(), prompt.as_deref());
    SUMMARIES.with(|s| {
        s.borrow_mut().insert(session_id.to_owned(), Summary { mtime, label: label.clone(), prompt: prompt.clone(), at: now });
    });
    Some((label, prompt))
}

pub(super) fn info(pid: u32) -> Option<AgentInfo> {
    let reg = registry(pid)?;
    let file = transcript_file(&reg.session_id, &reg.cwd);
    let (label, prompt) = transcript_summary(&reg.session_id, &file).unwrap_or((None, None));
    Some(AgentInfo {
        label,
        prompt,
        needs_input: reg.status == "waiting",
        mid_turn: false,
        session_id: Some(reg.session_id.clone()),
        resume: Some(reg.session_id),
        transcript: Some(file),
    })
}

/// The transcript's last turns and the `AskUserQuestion` still unanswered.
pub(super) fn brief(file: &Path) -> Option<Brief> {
    let text = fs::read_to_string(file).ok()?;
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
    Some(Brief { pending_question, messages: last_turns(messages) })
}

/// The session id of the live claude `pid` when its cwd is `directory`
/// (`claude:sessions`, filtered to one pid).
pub(super) fn session_of(pid: u32, directory: &str) -> Option<String> {
    let reg = registry(pid)?;
    let same = |a: &str, b: &str| a.trim_end_matches('/') == b.trim_end_matches('/');
    (same(&reg.cwd, directory) && processes().contains_key(&pid)).then_some(reg.session_id)
}
