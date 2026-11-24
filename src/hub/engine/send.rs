//! Typing into terminals (`src/stores/terminalSendStore.ts`): writes queue
//! per terminal so two senders never interleave mid-prompt, and text a
//! terminal was not ready for is held until it reaches a prompt.

use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    time::Duration,
};

use nsh_core::prelude::*;

use crate::state::{self, Shell};

use super::{TermStatus, agents, spawn_root};

/// A paste settles this long before its Enter, which claude would swallow
/// if it came with the burst.
const PASTE_SETTLE_MS: u64 = 150;

#[derive(Default)]
struct WriteQueue {
    items: VecDeque<(u64, String)>,
    running: bool,
}

thread_local! {
    static WRITES: RefCell<HashMap<u64, WriteQueue>> = RefCell::new(HashMap::new());
    /// Text a terminal was not ready for (`pendingSendsAtom`), by session id.
    static PENDING: RefCell<HashMap<String, Vec<String>>> = RefCell::new(HashMap::new());
}

/// Writes `data` to terminal `id` after `delay_ms`, behind what is already
/// queued for it (`queueWrite`).
pub fn queue_write(id: u64, data: String, delay_ms: u64) {
    let start = WRITES.with(|w| {
        let mut w = w.borrow_mut();
        let q = w.entry(id).or_default();
        q.items.push_back((delay_ms, data));
        !std::mem::replace(&mut q.running, true)
    });
    if !start {
        return;
    }
    spawn_root(async move {
        loop {
            let next = WRITES.with(|w| {
                let mut w = w.borrow_mut();
                let q = w.get_mut(&id)?;
                let next = q.items.pop_front();
                if next.is_none() {
                    w.remove(&id);
                }
                next
            });
            let Some((delay, data)) = next else { break };
            if delay > 0 {
                timer(Duration::from_millis(delay)).await;
            }
            state::pty_write(id, &data);
        }
    });
}

/// Types `text` and presses Enter. A raw newline would submit mid-message,
/// so multi-line text goes in as one bracketed paste (`queueSubmit`).
pub fn queue_submit(id: u64, text: &str) {
    let raw = text.starts_with("\x1b[200~") || !text.contains('\n');
    queue_write(id, if raw { text.to_owned() } else { format!("\x1b[200~{text}\x1b[201~") }, 0);
    queue_write(id, "\r".into(), PASTE_SETTLE_MS);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readiness {
    Ready,
    Busy,
    Blocked,
    Gone,
}

/// A terminal takes typed input at a prompt, the shell's or an agent's.
/// A harness parked at its own prompt is `working` with an agent pid;
/// `agent` means streaming, where a send would land mid-turn, and so does
/// a turn its session says is still open (codex, whose approval dialogs
/// it can't tell apart). `working` otherwise is some other foreground job,
/// or an agent whose prompt can't be told from a dialog, which owns the
/// keyboard (`readiness`).
pub fn readiness(info: Option<&TermStatus>) -> Readiness {
    match info {
        None => Readiness::Gone,
        Some(i) if i.needs_input => Readiness::Blocked,
        Some(i) if i.status == "idle" || (i.status == "working" && i.agent_pid.is_some() && i.harness().is_some() && !i.mid_turn) => Readiness::Ready,
        Some(_) => Readiness::Busy,
    }
}

/// The live state of the terminal with `session_id`, `None` once it closed.
pub fn status_by_session(shell: Shell, session_id: &str) -> Option<TermStatus> {
    state::term_by_session(shell, session_id).map(|t| agents::status_of(t.id))
}

pub fn pending_has(session_id: &str, text: &str) -> bool {
    PENDING.with(|p| p.borrow().get(session_id).is_some_and(|q| q.iter().any(|t| t == text)))
}

/// Holds `text` for the terminal until it reaches a prompt.
pub fn push_pending(session_id: &str, text: String) {
    PENDING.with(|p| p.borrow_mut().entry(session_id.to_owned()).or_default().push(text));
}

/// One place decides write-now vs queue (`dispatchToTerminals`).
pub fn dispatch_to_terminals(shell: Shell, session_ids: &[String], text: &str) {
    for session_id in session_ids {
        let ready = readiness(status_by_session(shell, session_id).as_ref()) == Readiness::Ready;
        // Anything already queued here goes first, or a second send would jump the line.
        let queued = PENDING.with(|p| p.borrow().get(session_id).is_some_and(|q| !q.is_empty()));
        match state::term_by_session(shell, session_id) {
            Some(term) if ready && !queued => queue_submit(term.id, text),
            _ => push_pending(session_id, text.to_owned()),
        }
    }
}

/// Types the next held line into each terminal that is at a prompt, and
/// drops what was held for terminals that closed (`drainPendingAtom`): one
/// line per poll, since the prompt has to come back before the next can be
/// typed.
pub fn drain_pending(shell: Shell) {
    let ids: Vec<String> = PENDING.with(|p| p.borrow().keys().cloned().collect());
    for id in ids {
        let state = readiness(status_by_session(shell, &id).as_ref());
        let queue = PENDING.with(|p| p.borrow().get(&id).cloned().unwrap_or_default());
        if queue.is_empty() || state == Readiness::Gone {
            PENDING.with(|p| {
                p.borrow_mut().remove(&id);
            });
            continue;
        }
        if state != Readiness::Ready {
            continue;
        }
        if let Some(term) = state::term_by_session(shell, &id) {
            queue_submit(term.id, &queue[0]);
        }
        PENDING.with(|p| {
            let mut p = p.borrow_mut();
            if queue.len() > 1 {
                p.insert(id.clone(), queue[1..].to_vec());
            } else {
                p.remove(&id);
            }
        });
    }
}
