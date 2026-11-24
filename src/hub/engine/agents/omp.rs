//! omp (`src/agents/omp.js`): `~/.omp/agent/terminal-sessions/<tty>` names
//! the session file a terminal's omp writes, whose first lines carry its
//! title and id and whose tail its latest turns.

use std::{
    cell::RefCell,
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    time::SystemTime,
};

use serde_json::Value;

use super::{AgentInfo, Brief, first_text, home, label_of, last_turns, read_range, tail_lines};

const HEAD_BYTES: u64 = 4096;

struct Cached {
    mtime: SystemTime,
    size: u64,
    info: AgentInfo,
}

thread_local! {
    static CACHE: RefCell<HashMap<PathBuf, Cached>> = RefCell::new(HashMap::new());
}

fn session_file(tty: &str) -> Option<PathBuf> {
    if tty.is_empty() || tty == "??" {
        return None;
    }
    let text = fs::read_to_string(home().join(".omp").join("agent").join("terminal-sessions").join(tty)).ok()?;
    let line = text.split('\n').nth(1)?.trim();
    (!line.is_empty()).then(|| PathBuf::from(line))
}

pub(super) fn info(tty: &str) -> Option<AgentInfo> {
    let file = session_file(tty)?;
    let meta = fs::metadata(&file).ok()?;
    let (mtime, size) = (meta.modified().ok()?, meta.len());
    if let Some(hit) = CACHE.with(|c| c.borrow().get(&file).filter(|c| c.mtime == mtime && c.size == size).map(|c| c.info.clone())) {
        return Some(hit);
    }
    let head = read_range(&file, 0, HEAD_BYTES)?;
    let (lines, cut) = tail_lines(&file)?;
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
                    prompt = first_text(&msg["content"]);
                }
            }
            _ => {}
        }
    }
    // A long autonomous run can push the last prompt out of the tail.
    let before = CACHE.with(|c| c.borrow().get(&file).map(|c| (c.info.session_id.clone(), c.info.prompt.clone())));
    if prompt.is_none() {
        if let Some((sid, p)) = &before {
            if *sid == session_id {
                prompt = p.clone();
            }
        }
    }
    if prompt.is_none() && before.is_none() && cut {
        if let Ok(all) = fs::read_to_string(&file) {
            for line in all.lines().rev() {
                if !line.contains("\"role\":\"user\"") {
                    continue;
                }
                let Ok(rec) = serde_json::from_str::<Value>(line) else { continue };
                if rec["message"]["role"].as_str() == Some("user") {
                    prompt = first_text(&rec["message"]["content"]);
                    if prompt.is_some() {
                        break;
                    }
                }
            }
        }
    }
    let info = AgentInfo {
        label: label_of(title.as_deref(), prompt.as_deref()),
        prompt,
        needs_input: needs_input.unwrap_or(false),
        mid_turn: false,
        session_id,
        // By path, not id: an id resolves only against omp's sessions for the cwd it is started in.
        resume: Some(file.to_string_lossy().into_owned()),
        transcript: Some(file.clone()),
    };
    CACHE.with(|c| {
        c.borrow_mut().insert(file, Cached { mtime, size, info: info.clone() });
    });
    Some(info)
}

/// The session's last turns: the text of each user and assistant message.
/// omp's `ask` is a form, not a question typed back, so none is pending.
pub(super) fn brief(file: &Path) -> Option<Brief> {
    let (lines, _) = tail_lines(file)?;
    let mut messages = Vec::new();
    for line in &lines {
        if !line.contains("\"message\"") {
            continue;
        }
        let Ok(rec) = serde_json::from_str::<Value>(line) else { continue };
        let msg = &rec["message"];
        let role = match msg["role"].as_str() {
            Some("user") => "user",
            Some("assistant") => "assistant",
            _ => continue,
        };
        let text: Vec<&str> = match &msg["content"] {
            Value::String(s) => vec![s.as_str()],
            Value::Array(blocks) => blocks.iter().filter(|b| b["type"].as_str() == Some("text")).filter_map(|b| b["text"].as_str()).collect(),
            _ => continue,
        };
        let text = text.join("\n").trim().to_owned();
        if !text.is_empty() {
            messages.push((role.to_owned(), text));
        }
    }
    Some(Brief { pending_question: None, messages: last_turns(messages) })
}
