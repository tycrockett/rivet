//! The hub's posts and what comes back (`src/stores/fleetStore.ts`): a
//! post typed into the addressed terminals, board notes, jobs run by a
//! headless hop, and the answers a terminal owes after it was addressed.

use std::{cell::RefCell, collections::HashMap, time::Duration};

use nsh_core::{prelude::*, time::unix_ms};

use crate::{
    commands,
    hub::{Hop, Hub, HubEntry, HubMember, HubStep, HubTarget, JobStatus, ME, StepStatus, next_id, parse_post, targets},
    state::{self, Shell},
};

use super::{
    agents,
    hop::{HopResult, Role, fork_from, run_hop},
    send::{self, dispatch_to_terminals},
    spawn_root,
};

/// How often terminals are polled for what they owe (`pty:status`'s poll).
const POLL: Duration = Duration::from_millis(1000);

pub fn say(from: &str, text: impl Into<String>) -> HubEntry {
    HubEntry::Say { id: next_id(), from: from.to_owned(), text: text.into(), error: false, to: None }
}

pub fn say_error(from: &str, text: impl Into<String>) -> HubEntry {
    HubEntry::Say { id: next_id(), from: from.to_owned(), text: text.into(), error: true, to: None }
}

/// Every concrete place a line can land (`hubMembersAtom`).
pub fn hub_members(shell: Shell) -> Vec<HubMember> {
    targets(shell).into_iter().flat_map(|t| t.members).collect()
}

/// The agent command typed to start one (`settings.agentCommand || "c"`).
pub fn agent_command(shell: Shell) -> String {
    shell
        .settings
        .with_untracked(|s| s.extra.get("agentCommand").and_then(|v| v.as_str()).filter(|c| !c.is_empty()).map(str::to_owned))
        .unwrap_or_else(|| "c".into())
}

/// A new terminal in `cwd`, shown; its session id (`openWorkspaceTerminalAtom`).
pub fn open_workspace_terminal(shell: Shell, cwd: &str) -> Result<String, String> {
    let id = shell.open_terminal(cwd.to_owned())?;
    shell.terms.with_untracked(|t| t.iter().find(|t| t.id == id).map(|t| t.session_id.clone())).ok_or_else(|| "the terminal closed".to_owned())
}

// ---- the board ---------------------------------------------------------------

fn card_of<'a>(thread: &'a [HubEntry], targets: &[HubTarget], reference: &str) -> Option<&'a HubEntry> {
    let prefix = format!("{reference}/");
    let sessions: Vec<&str> = targets
        .iter()
        .filter(|t| t.handle == reference || t.handle.starts_with(&prefix))
        .flat_map(|t| t.members.iter().filter_map(|m| m.session_id.as_deref()))
        .collect();
    thread.iter().rev().find(|e| match e {
        HubEntry::Say { from, text, error, .. } => {
            from != ME && !error && (reference == "latest" || from == reference || sessions.contains(&from.as_str()) || text.starts_with(&format!("@{reference}:")))
        }
        _ => false,
    })
}

/// The newest note of `latest` and of each target (`boardCards`): its ref
/// and the `say` entry.
#[allow(dead_code)]
pub fn board_cards(thread: &[HubEntry], targets: &[HubTarget]) -> Vec<(String, HubEntry)> {
    std::iter::once("latest".to_owned())
        .chain(targets.iter().map(|t| t.handle.clone()))
        .filter_map(|r| card_of(thread, targets, &r).map(|c| (r, c.clone())))
        .collect()
}

/// `@board:<ref>` becomes the note that ref last posted (`expandBoardRefs`).
pub fn expand_board_refs(text: &str, thread: &[HubEntry], targets: &[HubTarget]) -> String {
    const TAG: &str = "@board:";
    let lower = text.to_ascii_lowercase();
    let mut out = String::new();
    let mut at = 0;
    while let Some(p) = lower[at..].find(TAG) {
        let start = at + p;
        let name_start = start + TAG.len();
        let len = text[name_start..].find(|c: char| !(c.is_ascii_alphanumeric() || c == '/' || c == '-')).unwrap_or(text.len() - name_start);
        out.push_str(&text[at..start]);
        let end = name_start + len;
        match (len > 0).then(|| card_of(thread, targets, &text[name_start..end].to_lowercase())).flatten() {
            Some(HubEntry::Say { text: note, .. }) => out.push_str(&format!("\n\n[board note]\n{note}\n[/board note]\n\n")),
            _ => out.push_str(&text[start..end]),
        }
        at = end;
    }
    out.push_str(&text[at..]);
    out
}

/// A literal ` && ` inside a prompt splits it too (`splitSteps`).
pub fn split_steps(body: &str) -> Vec<String> {
    let mut parts: Vec<&str> = Vec::new();
    let mut from = 0;
    let mut search = 0;
    while let Some(p) = body[search..].find("&&") {
        let at = search + p;
        let before = &body[from..at];
        let after = &body[at + 2..];
        if before.ends_with(char::is_whitespace) && after.starts_with(char::is_whitespace) {
            parts.push(before);
            from = at + 2;
        }
        search = at + 1;
    }
    parts.push(&body[from..]);
    parts
        .into_iter()
        .map(|s| {
            let s = s.trim();
            s.strip_prefix('"').and_then(|r| r.strip_suffix('"')).filter(|_| s.len() >= 2).unwrap_or(s).to_owned()
        })
        .filter(|s| !s.is_empty())
        .collect()
}

// ---- jobs ----------------------------------------------------------------------

/// The parts of a job entry a patch may change.
pub struct JobMut<'a> {
    pub steps: &'a mut Vec<HubStep>,
    pub status: &'a mut JobStatus,
    pub error: &'a mut Option<String>,
    pub ended_at: &'a mut Option<f64>,
    pub hop: &'a mut Option<Hop>,
    pub delivered: &'a mut bool,
}

/// Changes job `id` unless it was cancelled (`patchJob`).
pub fn patch_job(hub: Hub, id: &str, f: impl FnOnce(JobMut)) {
    if super::hop::is_cancelled(id) {
        return;
    }
    hub.patch(id, |e| {
        if let HubEntry::Job { steps, status, error, ended_at, hop, delivered, .. } = e {
            f(JobMut { steps, status, error, ended_at, hop, delivered });
        }
    });
}

pub fn new_job(id: String, ask: String, to: String, run: Option<String>, from: Option<String>, steps: Vec<HubStep>) -> HubEntry {
    HubEntry::Job {
        id,
        ask,
        to,
        from,
        delivered: false,
        run,
        started_at: Some(unix_ms()),
        ended_at: None,
        steps,
        status: JobStatus::Running,
        hop: None,
        error: None,
    }
}

/// A headless claude asked `member` and the card shows it (`runJob`).
pub async fn run_job(hub: Hub, member: &HubMember, ask: String, prompt: String, from: Option<String>, role: Role) -> HopResult {
    let id = next_id();
    let step = HubStep {
        handle: member.handle.clone(),
        prompt: ask.clone(),
        status: StepStatus::Running,
        reply: String::new(),
        label: None,
        started_at: Some(unix_ms()),
        ended_at: None,
    };
    hub.post(new_job(id.clone(), ask, member.handle.clone(), None, from, vec![step]));
    let resume = fork_from(hub.shell, member, role);
    let hop = run_hop(&member.cwd, &prompt, resume, Some(&id)).await;
    patch_job(hub, &id, |j| {
        let failed = hop.error.is_some();
        *j.status = if failed { JobStatus::Failed } else { JobStatus::Done };
        *j.error = hop.error.clone();
        *j.ended_at = Some(unix_ms());
        if let Some(first) = j.steps.first_mut() {
            first.status = if failed { StepStatus::Failed } else { StepStatus::Done };
            first.reply = if hop.text.is_empty() { hop.error.clone().unwrap_or_default() } else { hop.text.clone() };
            first.ended_at = Some(unix_ms());
        }
    });
    hop
}

/// The clicked terminal's own context does the work, in a fork, so nothing
/// lands on its screen (`boltForkAtom`).
pub async fn bolt_fork(hub: Hub, session_id: String, text: String) {
    let Some(member) = hub_members(hub.shell).into_iter().find(|m| m.session_id.as_deref() == Some(&session_id)) else { return };
    let all = targets(hub.shell);
    let handles: Vec<String> = all.iter().map(|t| t.handle.clone()).collect();
    let body = parse_post(&text, &handles).body;
    if body.trim().is_empty() {
        return;
    }
    let prompt = hub.thread.with_untracked(|t| expand_board_refs(&body, t, &all));
    run_job(hub, &member, body, prompt, Some(session_id), Role::Addressee).await;
}

/// A job's reply typed into its addressee's terminal, as one bracketed
/// paste (`deliverJobAtom`).
pub fn deliver_job(hub: Hub, id: &str) {
    let Some((from, to, reply)) = hub.thread.with_untracked(|t| {
        t.iter().find_map(|e| match e {
            HubEntry::Job { id: i, from: Some(from), to, delivered: false, steps, .. } if i == id => {
                Some((from.clone(), to.clone(), steps.first().map(|s| s.reply.clone()).unwrap_or_default()))
            }
            _ => None,
        })
    }) else {
        return;
    };
    dispatch_to_terminals(hub.shell, &[from], &format!("\x1b[200~[@{to} replied] {reply}\x1b[201~"));
    patch_job(hub, id, |j| *j.delivered = true);
}

/// Picks a headless hop back up in a real terminal, where the user can keep
/// talking to it (`openHopAtom`).
pub fn open_hop(hub: Hub, job_id: &str) {
    let Some(hop) = hub.thread.with_untracked(|t| {
        t.iter().find_map(|e| match e {
            HubEntry::Job { id, hop: Some(hop), .. } if id == job_id => Some(hop.clone()),
            _ => None,
        })
    }) else {
        return;
    };
    let Ok(session_id) = open_workspace_terminal(hub.shell, &hop.cwd) else { return };
    dispatch_to_terminals(hub.shell, std::slice::from_ref(&session_id), &format!("{} --resume {}", agent_command(hub.shell), hop.session_id));
    reveal_member(hub, &hop.cwd, Some(&session_id));
}

/// Leaves the palette for that terminal (`revealMemberAtom`): the workspace
/// switches to its tab and the terminal is the one shown.
pub fn reveal_member(hub: Hub, cwd: &str, session_id: Option<&str>) {
    let shell = hub.shell;
    if let Some(term) = session_id.and_then(|s| state::term_by_session(shell, s)) {
        shell.show(term.id);
    }
    if shell.directory() != cwd {
        let quoted = cwd.replace('\\', "\\\\").replace('"', "\\\"");
        commands::submit(shell, format!("cd \"{quoted}\" --absolute"));
    }
    hub.palette.set(None);
}

// ---- posting ----------------------------------------------------------------

thread_local! {
    /// Sessions the hub spoke to: each owes one answer back, watched from
    /// the moment it was asked until it gives one. The value is the text of
    /// the last turn read (`awaiting`).
    static AWAITING: RefCell<HashMap<String, Option<String>>> = RefCell::new(HashMap::new());
    /// When each terminal was last read, so only one that has printed since
    /// is worth parsing a transcript for (`readAt`).
    static READ_AT: RefCell<HashMap<String, f64>> = RefCell::new(HashMap::new());
    static STARTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// A post to the addressed members, typed into their own terminals
/// (`submitPostAtom`). Named references make it a briefing, not an errand:
/// the post goes to the live session so the work is visible and resumable.
pub async fn submit_post(hub: Hub, text: String) {
    let all = targets(hub.shell);
    let handles: Vec<String> = all.iter().map(|t| t.handle.clone()).collect();
    let post = parse_post(&text, &handles);
    hub.post(say(ME, text));
    if post.body.is_empty() {
        return;
    }
    let recipients: Vec<&HubTarget> = post.tos.iter().filter_map(|h| all.iter().find(|t| &t.handle == h)).collect();
    if recipients.is_empty() {
        hub.post(say(ME, "No member matched — address a post with @handle."));
        return;
    }
    let steps = hub.thread.with_untracked(|t| split_steps(&expand_board_refs(&post.body, t, &all)));
    for target in recipients {
        for member in &target.members {
            deliver(hub, member, &steps);
        }
    }
}

/// Typed into the member's own terminal, spawning one if the workspace has
/// none. A fresh shell has no prompt yet, so the later steps are held until
/// it shows one (`deliverAtom`).
pub fn deliver(hub: Hub, member: &HubMember, steps: &[String]) {
    let session_id = match &member.session_id {
        Some(s) => s.clone(),
        None => match open_workspace_terminal(hub.shell, &member.cwd) {
            Ok(s) => {
                // A real terminal in the real directory, so it can be opened and watched.
                hub.post(HubEntry::Spawn { id: next_id(), handle: member.handle.clone(), cwd: member.cwd.clone(), session_id: s.clone() });
                s
            }
            Err(_) => {
                hub.post(say(ME, format!("Could not start a terminal in {}.", member.name)));
                return;
            }
        },
    };
    // Addressed, so it owes one answer back. Watched from here until it gives one.
    AWAITING.with(|a| {
        a.borrow_mut().insert(session_id.clone(), None);
    });
    let Some((first, rest)) = steps.split_first() else { return };
    dispatch_to_terminals(hub.shell, std::slice::from_ref(&session_id), first);
    // Queued, not dispatched: the terminal still reads ready until the poll sees the first step running.
    for step in rest {
        send::push_pending(&session_id, step.clone());
    }
}

// ---- what comes back -----------------------------------------------------------

/// The assistant turns after the one last read. `messages` is only the last
/// few turns, so the cursor is the text of the turn last read rather than an
/// index into a window that slides away; the first look anchors without
/// appending, since the window still holds turns from before the question
/// (`freshReplies`).
pub fn fresh_replies(messages: &[(String, String)], seen: Option<&str>) -> (Vec<String>, Option<String>) {
    let Some(seen) = seen else {
        return (Vec::new(), messages.last().map(|m| m.1.clone()));
    };
    // An anchor that aged out of the window costs the turns before the last one.
    let after: &[(String, String)] = match messages.iter().rposition(|m| m.1 == seen) {
        Some(i) => &messages[i + 1..],
        None => &messages[messages.len().saturating_sub(1)..],
    };
    let replies = after.iter().filter(|m| m.0 == "assistant").map(|m| m.1.clone()).collect();
    (replies, Some(after.last().map_or_else(|| seen.to_owned(), |m| m.1.clone())))
}

/// Surfaces a blocked claude's question, and the answer a terminal owes
/// once it gives it (`ingestAsksAtom`).
async fn ingest_asks(hub: Hub) {
    let shell = hub.shell;
    for term in shell.terms.get_untracked() {
        let session_id = term.session_id.clone();
        let info = agents::status_of(term.id);
        let watched = AWAITING.with(|a| a.borrow().contains_key(&session_id));
        // Blocked is always worth surfacing; anything else only while an answer is owed.
        let Some(pid) = info.agent_pid.filter(|_| info.agent.as_deref() == Some("claude")) else { continue };
        if !info.needs_input && !watched {
            continue;
        }
        if READ_AT.with(|r| r.borrow().get(&session_id) == Some(&info.last_output_at)) {
            continue;
        }
        READ_AT.with(|r| {
            r.borrow_mut().insert(session_id.clone(), info.last_output_at);
        });
        let Some(brief) = agents::session_brief(pid) else { continue };

        if let Some(q) = brief.pending_question {
            // toolUseId is stable while it waits and gone once answered, so it dedupes by itself.
            let id = format!("ask:{}", q.tool_use_id);
            if !hub.thread.with_untracked(|t| t.iter().any(|e| e.id() == id)) {
                let text = [q.header.clone().unwrap_or_default(), q.question.clone()].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" — ");
                hub.post(HubEntry::Ask { id, from: session_id.clone(), text, question: q.question });
            }
            // A question is not the answer it owes, so it stays watched.
            continue;
        }
        if !watched {
            continue;
        }
        let seen = AWAITING.with(|a| a.borrow().get(&session_id).cloned().flatten());
        let (replies, anchor) = fresh_replies(&brief.messages, seen.as_deref());
        if anchor.is_some() {
            AWAITING.with(|a| {
                a.borrow_mut().insert(session_id.clone(), anchor);
            });
        }
        if replies.is_empty() {
            continue;
        }
        for text in replies {
            hub.post(say(&session_id, text));
        }
        // It answered. Back to silence until it is addressed again.
        AWAITING.with(|a| {
            a.borrow_mut().remove(&session_id);
        });
    }
}

/// Starts the poll that types held lines into terminals as they reach a
/// prompt and brings back what addressed terminals answer. Once.
pub fn start(hub: Hub) {
    if STARTED.with(|s| s.replace(true)) {
        return;
    }
    spawn_root(async move {
        loop {
            timer(POLL).await;
            if hub.shell.terms.with_untracked(Vec::is_empty) {
                continue;
            }
            send::drain_pending(hub.shell);
            ingest_asks(hub).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(role: &str, text: &str) -> (String, String) {
        (role.to_owned(), text.to_owned())
    }

    #[test]
    fn splits_steps_on_double_ampersand_and_unquotes() {
        assert_eq!(split_steps("one && \"two three\" && four"), ["one", "two three", "four"]);
        assert_eq!(split_steps("a&&b && c"), ["a&&b", "c"]);
        assert_eq!(split_steps("  && "), Vec::<String>::new());
        assert_eq!(split_steps("\""), ["\""]);
    }

    #[test]
    fn first_look_anchors_without_replies() {
        let msgs = vec![m("user", "q"), m("assistant", "old")];
        assert_eq!(fresh_replies(&msgs, None), (vec![], Some("old".into())));
        assert_eq!(fresh_replies(&[], None), (vec![], None));
    }

    #[test]
    fn replies_are_assistant_turns_after_the_anchor() {
        let msgs = vec![m("assistant", "old"), m("user", "q"), m("assistant", "a1"), m("assistant", "a2")];
        assert_eq!(fresh_replies(&msgs, Some("old")), (vec!["a1".into(), "a2".into()], Some("a2".into())));
        assert_eq!(fresh_replies(&msgs, Some("a2")), (vec![], Some("a2".into())));
        // An anchor that slid out of the window costs all but the last turn.
        assert_eq!(fresh_replies(&msgs, Some("gone")), (vec!["a2".into()], Some("a2".into())));
    }

    #[test]
    fn board_refs_expand_to_the_latest_note() {
        let thread = vec![
            say("me", "hi"),
            say("pty-1", "first"),
            HubEntry::Say { id: "x".into(), from: "pty-1".into(), text: "failed".into(), error: true, to: None },
            say("pty-1", "second"),
        ];
        let targets = vec![HubTarget {
            handle: "web/abcd".into(),
            kind: crate::hub::TargetKind::Terminal,
            label: String::new(),
            detail: String::new(),
            members: vec![HubMember { handle: "web/abcd".into(), name: "web".into(), cwd: "/w".into(), session_id: Some("pty-1".into()) }],
            path: None,
        }];
        assert_eq!(expand_board_refs("see @board:web/abcd now", &thread, &targets), "see \n\n[board note]\nsecond\n[/board note]\n\n now");
        assert_eq!(expand_board_refs("@board:latest", &thread, &targets), "\n\n[board note]\nsecond\n[/board note]\n\n");
        assert_eq!(expand_board_refs("@board:nobody x", &thread, &targets), "@board:nobody x");
        let cards = board_cards(&thread, &targets);
        assert_eq!(cards.iter().map(|c| c.0.as_str()).collect::<Vec<_>>(), ["latest", "web/abcd"]);
    }
}
