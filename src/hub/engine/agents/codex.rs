//! Codex: a running codex holds its rollout
//! (`$CODEX_HOME/sessions/YYYY/MM/DD/rollout-<time>-<session>.jsonl`) open
//! for as long as that session lasts, so `lsof` on the terminal's
//! foreground process group names it; `session_index.jsonl` has the
//! session's title.

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::Command,
    sync::Mutex,
    time::SystemTime,
};

use nsh_core::time::unix_ms;
use serde_json::Value;

use super::{AgentInfo, Brief, Proc, home, label_of, last_turns, mtime_of, tail_lines};

/// How old a process group's open rollout gets before `lsof` is asked
/// again; a `/new` in codex opens another.
const LSOF_TTL_MS: f64 = 5000.0;

struct Cached {
    mtime: SystemTime,
    size: u64,
    info: AgentInfo,
}

/// Process group → when its rollout was looked up, and what was found.
type Lookups = HashMap<u32, (f64, Option<PathBuf>)>;
static ROLLOUTS: Mutex<Option<Lookups>> = Mutex::new(None);
/// Process groups an `lsof` is running for.
static READING: Mutex<Option<HashSet<u32>>> = Mutex::new(None);

thread_local! {
    static CACHE: RefCell<HashMap<PathBuf, Cached>> = RefCell::new(HashMap::new());
    static TITLES: RefCell<Option<(SystemTime, HashMap<String, String>)>> = const { RefCell::new(None) };
}

/// Where codex keeps its config and sessions: `$CODEX_HOME`, else `~/.codex`.
pub fn codex_home() -> PathBuf {
    std::env::var_os("CODEX_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".codex"))
}

/// The newest rollout among the files `lsof` lists for process group `pgid`.
fn read_rollout(pgid: u32) -> Option<PathBuf> {
    let out = Command::new("lsof").args(["-g", &pgid.to_string(), "-Fn"]).output().ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.strip_prefix('n'))
        .map(PathBuf::from)
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl") && p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("rollout-")))
        .filter(|p| !is_subagent(p))
        .max_by_key(|p| mtime_of(p))
}

/// A subagent's rollout, which codex writes from the same process as its
/// parent's: its `session_meta` names a parent thread.
fn is_subagent(file: &Path) -> bool {
    let mut first = String::new();
    let read = fs::File::open(file).map(BufReader::new).and_then(|mut r| r.read_line(&mut first));
    read.is_ok() && serde_json::from_str::<Value>(&first).is_ok_and(|meta| !meta["payload"]["parent_thread_id"].is_null())
}

/// Process group `pgid`'s open rollout as last looked up; a stale or
/// missing one starts a lookup on another thread, so the first poll of a
/// codex finds none.
fn rollout_of(pgid: u32, procs: &HashMap<u32, Proc>) -> Option<PathBuf> {
    let now = unix_ms();
    let hit = {
        let mut map = ROLLOUTS.lock().unwrap();
        let map = map.get_or_insert_with(HashMap::new);
        map.retain(|pid, _| procs.contains_key(pid));
        map.get(&pgid).cloned()
    };
    let stale = hit.as_ref().is_none_or(|(at, _)| now - at >= LSOF_TTL_MS);
    if stale && READING.lock().unwrap().get_or_insert_with(HashSet::new).insert(pgid) {
        std::thread::spawn(move || {
            let found = read_rollout(pgid);
            ROLLOUTS.lock().unwrap().get_or_insert_with(HashMap::new).insert(pgid, (unix_ms(), found));
            if let Some(r) = READING.lock().unwrap().as_mut() {
                r.remove(&pgid);
            }
        });
    }
    hit.and_then(|(_, p)| p)
}

/// The session id a rollout's name ends with.
fn session_id_of(file: &Path) -> Option<String> {
    let stem = file.file_stem()?.to_str()?;
    let id = stem.get(stem.len().checked_sub(36)?..)?;
    (id.matches('-').count() == 4).then(|| id.to_owned())
}

/// The session's name in `session_index.jsonl`, the last one given.
fn title_of(session_id: &str) -> Option<String> {
    let file = codex_home().join("session_index.jsonl");
    let mtime = mtime_of(&file)?;
    TITLES.with(|t| {
        let mut t = t.borrow_mut();
        if t.as_ref().is_none_or(|(m, _)| *m != mtime) {
            let mut names = HashMap::new();
            for line in fs::read_to_string(&file).unwrap_or_default().lines() {
                let Ok(rec) = serde_json::from_str::<Value>(line) else { continue };
                if let (Some(id), Some(name)) = (rec["id"].as_str(), rec["thread_name"].as_str().map(str::trim).filter(|n| !n.is_empty())) {
                    names.insert(id.to_owned(), name.to_owned());
                }
            }
            *t = Some((mtime, names));
        }
        t.as_ref().and_then(|(_, names)| names.get(session_id).cloned())
    })
}

/// A rollout record's turn: `(role, text)` for what the user typed and
/// what codex answered (`item_completed`'s `UserMessage` and
/// `AgentMessage`, or the older `user_message` and `agent_message`).
fn turn_of(rec: &Value) -> Option<(&'static str, String)> {
    if rec["type"].as_str() != Some("event_msg") {
        return None;
    }
    let p = &rec["payload"];
    let (role, text) = match p["type"].as_str()? {
        "item_completed" => {
            let role = match p["item"]["type"].as_str()? {
                "UserMessage" => "user",
                "AgentMessage" => "assistant",
                _ => return None,
            };
            let blocks = p["item"]["content"].as_array()?;
            let text: Vec<&str> = blocks.iter().filter(|b| b["type"].as_str().is_some_and(|t| t.eq_ignore_ascii_case("text"))).filter_map(|b| b["text"].as_str()).collect();
            (role, text.join("\n"))
        }
        "user_message" => ("user", p["message"].as_str()?.to_owned()),
        "agent_message" => ("assistant", p["message"].as_str()?.to_owned()),
        _ => return None,
    };
    let text = text.trim();
    (!text.is_empty()).then(|| (role, text.to_owned()))
}

fn turns(lines: &[String]) -> impl DoubleEndedIterator<Item = (&'static str, String)> + '_ {
    lines.iter().filter(|l| l.contains("\"event_msg\"")).filter_map(|l| serde_json::from_str::<Value>(l).ok()).filter_map(|rec| turn_of(&rec))
}

pub(super) fn info(pgid: u32, procs: &HashMap<u32, Proc>) -> Option<AgentInfo> {
    let file = rollout_of(pgid, procs)?;
    let meta = fs::metadata(&file).ok()?;
    let (mtime, size) = (meta.modified().ok()?, meta.len());
    let session_id = session_id_of(&file);
    let title = session_id.as_deref().and_then(title_of);
    if let Some(mut hit) = CACHE.with(|c| c.borrow().get(&file).filter(|c| c.mtime == mtime && c.size == size).map(|c| c.info.clone())) {
        hit.label = label_of(title.as_deref(), hit.prompt.as_deref());
        return Some(hit);
    }
    let (lines, _) = tail_lines(&file)?;
    // A long autonomous run can push the last prompt out of the tail.
    let prompt = turns(&lines)
        .rev()
        .find(|(r, _)| *r == "user")
        .map(|(_, t)| t)
        .or_else(|| CACHE.with(|c| c.borrow().get(&file).and_then(|c| c.info.prompt.clone())));
    // A turn opens with `task_started` and closes with `task_complete`, or `turn_aborted` when interrupted.
    let mid_turn = lines
        .iter()
        .rev()
        .filter(|l| l.contains("\"task_started\"") || l.contains("\"task_complete\"") || l.contains("\"turn_aborted\""))
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find_map(|rec| match rec["payload"]["type"].as_str() {
            Some("task_started") => Some(true),
            Some("task_complete" | "turn_aborted") => Some(false),
            _ => None,
        })
        .unwrap_or(false);
    let info = AgentInfo {
        label: label_of(title.as_deref(), prompt.as_deref()),
        prompt,
        needs_input: false,
        mid_turn,
        resume: session_id.clone(),
        session_id,
        transcript: Some(file.clone()),
    };
    CACHE.with(|c| {
        c.borrow_mut().insert(file, Cached { mtime, size, info: info.clone() });
    });
    Some(info)
}

/// The rollout's last turns. Codex asks nothing a reply typed back answers,
/// so none is pending.
pub(super) fn brief(file: &Path) -> Option<Brief> {
    let (lines, _) = tail_lines(file)?;
    let messages = turns(&lines).map(|(r, t)| (r.to_owned(), t)).collect();
    Some(Brief { pending_question: None, messages: last_turns(messages) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turns_come_from_both_rollout_formats() {
        let user = serde_json::json!({"type":"event_msg","payload":{"type":"item_completed","item":{"type":"UserMessage","content":[{"type":"text","text":" fix it "}]}}});
        let agent = serde_json::json!({"type":"event_msg","payload":{"type":"item_completed","item":{"type":"AgentMessage","content":[{"type":"Text","text":"done"}]}}});
        let old = serde_json::json!({"type":"event_msg","payload":{"type":"agent_message","message":"ok"}});
        let tool = serde_json::json!({"type":"event_msg","payload":{"type":"item_completed","item":{"type":"CommandExecution"}}});
        assert_eq!(turn_of(&user), Some(("user", "fix it".to_owned())));
        assert_eq!(turn_of(&agent), Some(("assistant", "done".to_owned())));
        assert_eq!(turn_of(&old), Some(("assistant", "ok".to_owned())));
        assert_eq!(turn_of(&tool), None);
    }

    #[test]
    fn the_session_id_ends_the_rollout_name() {
        let f = Path::new("/x/rollout-2026-11-24T10-59-32-01a20eb6-859a-7a60-8af5-a1800c40625f.jsonl");
        assert_eq!(session_id_of(f).as_deref(), Some("01a20eb6-859a-7a60-8af5-a1800c40625f"));
    }
}
