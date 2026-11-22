//! Running Bolt command scripts (`src/stores/boltCommandStore.ts`): the
//! node runner, the targets a node names, the in-app actions mapped onto
//! the shell, and the ways a line becomes a script run (a keyword, a `#`
//! script, a `@bolt` chat).

use std::{cell::RefCell, collections::HashMap, future::Future, pin::Pin, time::Duration};

use nsh_core::{notifications::Notification, prelude::*, time::unix_ms};
use serde_json::json;
use tokio::sync::oneshot;

use crate::{
    commands, git,
    hub::{Hop, Hub, HubEntry, HubMember, HubStep, JobStatus, ME, StepStatus, TargetKind, next_id, parse_post, targets},
    settings::{Tab, tab_name_for},
    state::{self, Shell},
};

use super::{
    ComposeSelf, agents,
    fleet::{self, agent_command, expand_board_refs, hub_members, new_job, open_workspace_terminal, patch_job, say, say_error, submit_post},
    hop::{HopResult, Role, fork_from, is_cancelled, kill_hops, mark_cancelled, run_hop},
    send::{self, Readiness, dispatch_to_terminals, readiness, status_by_session},
    types::{
        BoltNode, FetchMode, FetchType, IfOp, InAppAction, SendMode, Var, Vars, answer_tail, fill, fill_plain, list_of, match_command, new_id, palette_safe,
        parse_answer, parse_script, truthy,
    },
};

const POLL_MS: u64 = 1500;
/// A send whose command outruns the poll is never seen busy, so settling has a floor too.
const SETTLE_MS: f64 = 6000.0;
const AGENT_BOOT_MS: f64 = 30000.0;
const SESSION: &str = "session:";

type Task<T> = Pin<Box<dyn Future<Output = Result<T, String>>>>;

thread_local! {
    /// Approvals waiting for Continue, by `<job>:<step>` (`approvals`).
    static APPROVALS: RefCell<HashMap<String, oneshot::Sender<()>>> = RefCell::new(HashMap::new());
}

async fn sleep(ms: u64) {
    timer(Duration::from_millis(ms)).await;
}

/// `Promise.all`: every task runs; the first failure is the result once all
/// have settled.
async fn all<T: 'static>(tasks: Vec<Task<T>>) -> Result<Vec<T>, String> {
    let handles: Vec<_> = tasks.into_iter().map(spawn).collect();
    let mut out = Vec::new();
    let mut failure = None;
    for h in handles {
        match h.await {
            Ok(v) => out.push(v),
            Err(e) => {
                failure.get_or_insert(e);
            }
        }
    }
    failure.map_or(Ok(out), Err)
}

// ---- steps -----------------------------------------------------------------

fn open_step(hub: Hub, job_id: &str, label: &str, status: StepStatus) -> usize {
    let mut at = usize::MAX;
    patch_job(hub, job_id, |j| {
        at = j.steps.len();
        j.steps.push(HubStep {
            handle: String::new(),
            prompt: String::new(),
            status,
            reply: String::new(),
            label: Some(label.to_owned()),
            started_at: Some(unix_ms()),
            ended_at: None,
        });
    });
    at
}

fn close_step(hub: Hub, job_id: &str, at: usize, status: StepStatus, reply: &str) {
    patch_job(hub, job_id, |j| {
        if let Some(s) = j.steps.get_mut(at) {
            s.status = status;
            s.reply = reply.to_owned();
            s.ended_at = Some(unix_ms());
        }
    });
}

fn finish_job(hub: Hub, job_id: &str, error: Option<String>) {
    patch_job(hub, job_id, |j| {
        *j.status = if error.is_some() { JobStatus::Failed } else { JobStatus::Done };
        *j.error = error;
        *j.ended_at = Some(unix_ms());
    });
}

/// An approval step's Continue (`continueApprovalAtom`).
pub fn continue_approval(job_id: &str, at: usize) {
    if let Some(tx) = APPROVALS.with(|a| a.borrow_mut().remove(&format!("{job_id}:{at}"))) {
        let _ = tx.send(());
    }
}

/// A job card's Cancel (`cancelJobAtom`).
pub fn cancel_job(hub: Hub, id: &str) {
    patch_job(hub, id, |j| {
        *j.status = JobStatus::Failed;
        *j.error = Some("Cancelled".into());
        *j.ended_at = Some(unix_ms());
        for s in j.steps.iter_mut() {
            if matches!(s.status, StepStatus::Running | StepStatus::Waiting) {
                s.status = StepStatus::Failed;
                s.ended_at = Some(unix_ms());
            }
        }
    });
    mark_cancelled(id);
    kill_hops(id);
    // Dropping a waiting approval's sender releases it.
    let prefix = format!("{id}:");
    APPROVALS.with(|a| a.borrow_mut().retain(|k, _| !k.starts_with(&prefix)));
}

// ---- targets -----------------------------------------------------------------

/// A workspace member's cwd is its newest terminal's, which can sit below the tab it belongs to.
fn tab_path_of(hub: Hub, cwd: &str) -> Result<String, String> {
    let s = hub.shell.settings.get_untracked();
    s.tabs
        .iter()
        .chain(&s.pinned)
        .map(|t| t.path.as_str())
        .filter(|p| cwd == *p || cwd.strip_prefix(p).is_some_and(|r| r.starts_with('/')))
        .max_by_key(|p| p.len())
        .map(str::to_owned)
        .ok_or_else(|| format!("{cwd} is not a workspace tab"))
}

/// A handle names a workspace's newest terminal, so a terminal #create made is held by its session id instead.
fn session_member(hub: Hub, session_id: &str) -> Result<HubMember, String> {
    if let Some(known) = hub_members(hub.shell).into_iter().find(|m| m.session_id.as_deref() == Some(session_id)) {
        return Ok(known);
    }
    let term = state::term_by_session(hub.shell, session_id).ok_or("the terminal it made has closed")?;
    Ok(HubMember {
        handle: term.cwd.rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or("terminal").to_owned(),
        name: "terminal".into(),
        cwd: term.cwd,
        session_id: Some(session_id.to_owned()),
    })
}

/// A slot name, `{slot}`, a `#create … as $x` var, or a literal `@handle` all name a target.
fn members_of(hub: Hub, reference: &str, vars: &Vars) -> Result<Vec<HubMember>, String> {
    let filled = fill_plain(reference, vars);
    let raw = filled.trim();
    let held: Option<&Var> = if raw.starts_with(SESSION) {
        None
    } else {
        let key = raw.strip_prefix(['{', '@']).unwrap_or(raw);
        let key = key.strip_suffix('}').unwrap_or(key);
        vars.get(key)
    };
    let held_str = if raw.starts_with(SESSION) { Some(raw) } else { held.and_then(Var::as_str) };
    if let Some(sid) = held_str.and_then(|h| h.strip_prefix(SESSION)) {
        return Ok(vec![session_member(hub, sid)?]);
    }
    let home = hub.shell.settings.with_untracked(|s| s.home.clone());
    let value = held_str.unwrap_or(raw);
    let value = match value.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("{}{rest}", if home.is_empty() { "~" } else { &home }),
        _ => value.to_owned(),
    };
    if value.starts_with('/') {
        let trimmed = value.trim_end_matches('/');
        let path = if trimmed.is_empty() { "/".to_owned() } else { trimmed.to_owned() };
        let tab = targets(hub.shell).into_iter().find(|t| t.kind == TargetKind::Workspace && t.path.as_deref() == Some(&path));
        return Ok(match tab {
            Some(t) => t.members,
            None => vec![HubMember {
                handle: path.rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or(&path).to_owned(),
                name: path.clone(),
                cwd: path,
                session_id: None,
            }],
        });
    }
    let mut h = raw.strip_prefix('@').unwrap_or(raw).to_lowercase();
    if let Some(Var::Str(bound)) = vars.get(&h) {
        h = bound.clone();
    }
    targets(hub.shell)
        .into_iter()
        .find(|t| t.handle == h)
        .map(|t| t.members)
        .ok_or_else(|| format!("no target @{}", if h.is_empty() { reference } else { &h }))
}

// ---- terminals ----------------------------------------------------------------

/// A shell that has not exec'd the agent yet reads as ready, so the next #send would land at the shell prompt.
async fn wait_for_agent(hub: Hub, session_id: &str) -> Result<(), String> {
    let start = unix_ms();
    while unix_ms() - start < AGENT_BOOT_MS {
        if status_by_session(hub.shell, session_id).is_some_and(|s| s.agent_pid.is_some()) {
            return Ok(());
        }
        sleep(POLL_MS).await;
    }
    Err("the agent did not start within 30s".into())
}

/// Blocked counts as busy: a dialog the user answers in the terminal lets the node carry on.
async fn wait_idle(hub: Hub, session_id: &str, text: &str) -> Result<(), String> {
    let start = unix_ms();
    let (mut seen_busy, mut delivered, mut ticks) = (false, false, 0);
    loop {
        sleep(POLL_MS).await;
        let r = readiness(status_by_session(hub.shell, session_id).as_ref());
        // A shell that closes after taking the text was closed by it, e.g. `exit`.
        if r == Readiness::Gone && delivered {
            return Ok(());
        }
        if r == Readiness::Gone && unix_ms() - start > SETTLE_MS {
            return Err("the terminal closed".into());
        }
        if r != Readiness::Gone {
            delivered = !send::pending_has(session_id, text);
        }
        if r != Readiness::Ready {
            seen_busy = true;
            ticks = 0;
            continue;
        }
        ticks += 1;
        if if seen_busy { ticks >= 2 } else { unix_ms() - start > SETTLE_MS } {
            return Ok(());
        }
    }
}

/// The last `lines` lines a terminal printed, trimmed (`pty:tail`).
fn terminal_tail(hub: Hub, session_id: &str, lines: usize) -> String {
    let Some(term) = state::term_by_session(hub.shell, session_id) else { return String::new() };
    let text = state::terminal_text(term.id).unwrap_or_default();
    let all: Vec<&str> = text.trim_end().split('\n').collect();
    all[all.len().saturating_sub(lines)..].join("\n").trim().to_owned()
}

fn claude_pid_of(hub: Hub, session_id: &str) -> Option<u32> {
    let info = status_by_session(hub.shell, session_id)?;
    info.agent_pid.filter(|_| info.agent.as_deref() == Some("claude"))
}

/// A shell has no message boundary, so its latest message is its last 40 lines minus the prompt (`readLatest`).
fn read_latest(hub: Hub, session_id: &str) -> String {
    if let Some(pid) = claude_pid_of(hub, session_id) {
        return agents::last_claude_message(pid);
    }
    let tail = terminal_tail(hub, session_id, 41);
    let lines: Vec<&str> = tail.split('\n').collect();
    lines[..lines.len() - 1].join("\n").trim().to_owned()
}

/// A shell's output is what follows the last echo of the command, minus the fresh prompt; claude's is its last message.
fn output_of(hub: Hub, session_id: &str, text: &str) -> String {
    if let Some(pid) = claude_pid_of(hub, session_id) {
        return agents::last_claude_message(pid);
    }
    let tail = terminal_tail(hub, session_id, 400);
    let lines: Vec<&str> = tail.split('\n').collect();
    let first = text.trim().split('\n').next().unwrap_or_default();
    match lines.iter().rposition(|l| l.contains(first)) {
        Some(echo) if echo + 1 < lines.len() => lines[echo + 1..lines.len() - 1].join("\n").trim().to_owned(),
        _ => String::new(),
    }
}

async fn type_(hub: Hub, session_id: &str, text: &str) -> Result<(), String> {
    dispatch_to_terminals(hub.shell, &[session_id.to_owned()], text);
    wait_idle(hub, session_id, text).await
}

/// The palette's own handlers against a target's directory, off-screen: nothing moves the window or fills its results.
async fn run_palette_in(hub: Hub, member: &HubMember, line: &str) -> Result<String, String> {
    // A bare #set path has no tab; run in the folder itself.
    let dir = tab_path_of(hub, &member.cwd).unwrap_or_else(|_| member.cwd.clone());
    let out = commands::run(hub.shell, line.to_owned(), Some(dir)).await;
    let errors: Vec<&str> = out.iter().filter(|o| o.error).map(|o| o.text.as_str()).collect();
    if !errors.is_empty() {
        return Err(errors.join("; "));
    }
    let lines = std::iter::once(format!("ran {line} in {}", member.handle)).chain(out.into_iter().map(|o| o.text));
    Ok(lines.collect::<Vec<_>>().join("\n"))
}

/// `{:name}` runs the script whose keyword is name and splices its $return; ":calls" is unreachable by fill, so it carries the call chain.
async fn call_scripts(hub: Hub, job_id: &str, text: &str, vars: &Vars) -> Result<String, String> {
    let calls: Vec<String> = match vars.get(":calls") {
        Some(Var::List(l)) => l.clone(),
        _ => Vec::new(),
    };
    let mut out = text.to_owned();
    let mut at = 0;
    while let Some(p) = text[at..].find("{:") {
        let start = at + p;
        let after = &text[start + 2..];
        let len = after.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-')).unwrap_or(after.len());
        if len == 0 || !after[len..].starts_with('}') {
            at = start + 2;
            continue;
        }
        let (whole, name) = (&text[start..start + len + 3], &after[..len]);
        at = start + len + 3;
        let cmd = hub.commands.with_untracked(|cs| cs.iter().find(|c| c.keyword.to_lowercase() == name.to_lowercase()).cloned());
        let cmd = cmd.ok_or_else(|| format!("no script :{name}"))?;
        if calls.contains(&cmd.keyword) {
            return Err(format!(":{name} calls itself"));
        }
        let mut chain = calls.clone();
        chain.push(cmd.keyword.clone());
        let mut sub: Vars = HashMap::from([("input".to_owned(), Var::Str(String::new())), (":calls".to_owned(), Var::List(chain))]);
        run_nodes(hub, job_id, &parse_script(&cmd.script).0, &mut sub).await?;
        let returned = sub.get("return").map(Var::text).unwrap_or_default();
        out = out.replacen(whole, &palette_safe(&returned), 1);
    }
    Ok(out)
}

// ---- in-app actions --------------------------------------------------------------

/// `addWorkspaceTabs`: tabs for `paths` not yet open, in `group`.
pub(crate) fn add_workspace_tabs(shell: Shell, paths: &[String], group: Option<&str>) {
    shell.update_settings(|s| {
        let mut taken: Vec<String> = s.tabs.iter().chain(&s.pinned).map(|t| t.path.clone()).collect();
        for path in paths {
            if taken.contains(path) {
                continue;
            }
            taken.push(path.clone());
            let mut tab = Tab::new(tab_name_for(path), path.clone());
            tab.group = group.map(str::to_owned);
            s.tabs.push(tab);
        }
        if let Some(g) = group {
            if !s.groups.iter().any(|x| x == g) {
                s.groups.push(g.to_owned());
            }
        }
    });
}

pub(crate) struct Cut {
    pub(crate) branch: String,
    pub(crate) path: String,
    pub(crate) existed: bool,
    pub(crate) setup_failed: Vec<String>,
}

/// The headless sibling of `new <name> -bw`: a worktree for each branch,
/// serially (`git worktree add` takes the repo lock). Returns what landed
/// and the failure that stopped it (`handleCreateWorktrees`).
pub(crate) fn cut_branches(directory: &str, branches: &[String], parent: Option<&str>, home: &str) -> (Vec<Cut>, Option<String>) {
    let mut cuts = Vec::new();
    for branch in branches {
        match git::worktree_for_branch(directory, branch) {
            Ok(Some(path)) => {
                cuts.push(Cut { branch: branch.clone(), path, existed: true, setup_failed: Vec::new() });
                continue;
            }
            Ok(None) => {}
            Err(e) => return (cuts, Some(format!("'{branch}' failed: {e}."))),
        }
        let made = (|| -> git::Result<Cut> {
            let wt = git::worktree_context(directory)?;
            let home = std::env::var("HOME").unwrap_or_else(|_| home.to_owned());
            let repo = wt.main_repo_path.rsplit('/').next().filter(|r| !r.is_empty()).unwrap_or("repo");
            let root = format!("{home}/.worktrees/{repo}");
            let path = format!("{root}/{branch}");
            std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
            if git::git(&["rev-parse", "--verify", branch], directory).is_ok() {
                git::git(&["worktree", "add", &path, branch], directory)?;
            } else {
                let mut args = vec!["worktree", "add", "-b", branch, &path];
                args.extend(parent);
                git::git(&args, directory)?;
            }
            let setup_failed = git::setup_worktree(&wt.main_repo_path, &path)?;
            Ok(Cut { branch: branch.clone(), path, existed: false, setup_failed })
        })();
        match made {
            Ok(cut) => cuts.push(cut),
            Err(e) => return (cuts, Some(format!("'{branch}' failed: {e}."))),
        }
    }
    (cuts, None)
}

fn workspace_handle(hub: Hub, path: &str) -> String {
    targets(hub.shell)
        .into_iter()
        .find(|t| t.kind == TargetKind::Workspace && t.members.first().is_some_and(|m| m.cwd == path))
        .map_or_else(|| path.to_owned(), |t| t.handle)
}

async fn in_app(hub: Hub, job_id: &str, action: InAppAction, args: &std::collections::BTreeMap<String, String>, vars: &mut Vars) -> Result<String, String> {
    let arg = |k: &str, vars: &Vars| fill_plain(args.get(k).map_or("", String::as_str), vars).trim().to_owned();
    let raw = |k: &str| args.get(k).map_or("", String::as_str);
    if action == InAppAction::Palette {
        let command = call_scripts(hub, job_id, raw("command"), vars).await?;
        let line = fill(&command, vars, &palette_safe).trim().to_owned();
        let Some(target) = args.get("target").filter(|t| !t.is_empty()) else {
            let out = commands::run(hub.shell, line.clone(), None).await;
            let errors: Vec<&str> = out.iter().filter(|o| o.error).map(|o| o.text.as_str()).collect();
            if !errors.is_empty() {
                return Err(errors.join("; "));
            }
            return Ok(format!("ran {line}"));
        };
        let member = members_of(hub, target, vars)?.into_iter().next().ok_or("no target")?;
        return run_palette_in(hub, &member, &line).await;
    }
    if action == InAppAction::Tab {
        let path = arg("path", vars).trim_end_matches('/').to_owned();
        if !std::path::Path::new(&path).is_dir() {
            return Err(format!("no folder at {path}"));
        }
        let group = arg("group", vars);
        add_workspace_tabs(hub.shell, std::slice::from_ref(&path), (!group.is_empty()).then_some(group.as_str()));
        if let Some(name) = args.get("as") {
            vars.insert(name.clone(), Var::Str(workspace_handle(hub, &path)));
        }
        return Ok(format!("tab {path}{}", if group.is_empty() { String::new() } else { format!(" in {group}") }));
    }
    if action == InAppAction::Group {
        let name = arg("name", vars);
        let handles = list_of(raw("tabs"), vars);
        let mut paths = Vec::new();
        for h in &handles {
            let member = members_of(hub, h, vars)?.into_iter().next().ok_or("no target")?;
            paths.push(tab_path_of(hub, &member.cwd)?);
        }
        hub.shell.update_settings(|s| {
            for t in s.tabs.iter_mut().chain(s.pinned.iter_mut()) {
                if paths.contains(&t.path) {
                    t.group = Some(name.clone());
                }
            }
            if !s.groups.contains(&name) {
                s.groups.push(name.clone());
            }
        });
        if let Some(as_) = args.get("as") {
            let handle = targets(hub.shell).into_iter().find(|t| t.kind == TargetKind::Group && t.label == name).map_or_else(|| name.clone(), |t| t.handle);
            vars.insert(as_.clone(), Var::Str(handle));
        }
        return Ok(format!("group {name}: {}", handles.join(", ")));
    }
    let member = members_of(hub, raw("target"), vars)?.into_iter().next().ok_or("no target")?;
    match action {
        InAppAction::CreateWorktrees => {
            let branches: Vec<String> = list_of(raw("branches"), vars).into_iter().map(|b| b.trim().to_owned()).filter(|b| !b.is_empty()).collect();
            if branches.is_empty() {
                return Err("branches is required, e.g. [\"auth-intake\", \"auth-callback\"].".into());
            }
            let parent = Some(arg("parent", vars)).filter(|p| !p.is_empty());
            let group = Some(arg("group", vars)).filter(|g| !g.is_empty());
            let (directory, home) = (member.cwd.clone(), hub.shell.settings.with_untracked(|s| s.home.clone()));
            let (b, p) = (branches.clone(), parent.clone());
            let (cuts, failure) = git::off_thread(move || cut_branches(&directory, &b, p.as_deref(), &home)).await;
            // One settings write for everything that landed, even when a later branch failed.
            if !cuts.is_empty() {
                add_workspace_tabs(hub.shell, &cuts.iter().map(|c| c.path.clone()).collect::<Vec<_>>(), group.as_deref());
            }
            if let Some(failure) = failure {
                let made = if cuts.is_empty() { String::new() } else { format!(" Made: {}.", cuts.iter().map(|c| c.branch.as_str()).collect::<Vec<_>>().join(", ")) };
                return Err(format!("{failure}{made}"));
            }
            if let Some(name) = args.get("as") {
                vars.insert(name.clone(), Var::List(cuts.iter().map(|c| workspace_handle(hub, &c.path)).collect()));
            }
            let created: Vec<serde_json::Value> = cuts
                .iter()
                .map(|c| {
                    let mut v = json!({ "branch": c.branch, "path": c.path, "existed": c.existed });
                    if !c.setup_failed.is_empty() {
                        v["setupFailed"] = json!(c.setup_failed);
                    }
                    v
                })
                .collect();
            Ok(json!({ "directory": member.cwd, "group": group, "created": created }).to_string())
        }
        InAppAction::StartAgents => {
            let path = member.cwd.trim_end_matches('/').to_owned();
            let known = hub.shell.settings.with_untracked(|s| s.tabs.iter().chain(&s.pinned).any(|t| t.path == path));
            if !known {
                return Err(format!("Not workspace tabs: {path}. Nothing was started."));
            }
            let prompt = arg("prompt", vars);
            let command = agent_command(hub.shell) + &if prompt.is_empty() { String::new() } else { format!(" {}", git::shell_quote(&prompt)) };
            let session_id = open_workspace_terminal(hub.shell, &path).map_err(|e| format!("'{path}' failed: {e}."))?;
            if let Some(term) = state::term_by_session(hub.shell, &session_id) {
                state::pty_write(term.id, &format!("{command}\r"));
            }
            wait_for_agent(hub, &session_id).await?;
            if let Some(name) = args.get("as") {
                vars.insert(name.clone(), Var::Str(format!("{SESSION}{session_id}")));
            }
            Ok(format!("agent started in {}", member.handle))
        }
        InAppAction::Terminal => {
            let session_id = open_workspace_terminal(hub.shell, &member.cwd)?;
            if let Some(name) = args.get("as") {
                vars.insert(name.clone(), Var::Str(format!("{SESSION}{session_id}")));
            }
            let command = arg("command", vars);
            if !command.is_empty() {
                dispatch_to_terminals(hub.shell, &[session_id], &command);
            }
            Ok(format!("terminal opened in {}{}", member.handle, if command.is_empty() { String::new() } else { format!(": {command}") }))
        }
        InAppAction::Palette | InAppAction::Tab | InAppAction::Group => unreachable!("handled above"),
    }
}

// ---- #fetch ---------------------------------------------------------------------

fn last_chars(s: &str, n: usize) -> String {
    let count = s.chars().count();
    s.chars().skip(count.saturating_sub(n)).collect()
}

#[allow(clippy::too_many_arguments)]
async fn fetch_node(
    hub: Hub,
    job_id: &str,
    mode: FetchMode,
    target: &str,
    prompt: &str,
    as_: Option<FetchType>,
    into: Option<&str>,
    vars: &Vars,
) -> Result<Var, String> {
    let typed = as_.zip(into);
    let prompt = fill_plain(prompt, vars) + &typed.map(|(t, k)| answer_tail(k, t)).unwrap_or_default();
    let answer = |text: String, who: &str| -> Result<Var, String> {
        let Some((t, key)) = typed else { return Ok(Var::Str(text)) };
        parse_answer(&text, key, t).ok_or_else(|| format!("no \"{key}\" in {who}: {}", last_chars(&text, 200)))
    };
    if mode == FetchMode::Fresh {
        let hop = run_hop(&hub.shell.directory(), &prompt, None, Some(job_id)).await;
        if let Some(e) = hop.error {
            return Err(e);
        }
        return answer(hop.text, "the answer");
    }
    let member = members_of(hub, target, vars)?.into_iter().next().ok_or("no target")?;
    let text = if mode == FetchMode::Live {
        let pid = member.session_id.as_deref().and_then(|s| claude_pid_of(hub, s));
        let (Some(session_id), Some(pid)) = (member.session_id.as_deref(), pid) else {
            return Err(format!("@{} has no live claude session", member.handle));
        };
        type_(hub, session_id, &prompt).await?;
        agents::last_claude_message(pid)
    } else {
        let resume = fork_from(hub.shell, &member, Role::Addressee);
        let hop = run_hop(&member.cwd, &prompt, resume, Some(job_id)).await;
        if let Some(e) = hop.error {
            return Err(e);
        }
        hop.text
    };
    answer(text, &format!("@{}'s answer", member.handle))
}

// ---- running nodes ---------------------------------------------------------------

fn label(node: &BoltNode, vars: &Vars) -> String {
    let f = |t: &str| fill_plain(t, vars);
    match node {
        BoltNode::InApp { action, args, .. } => {
            let name = match action {
                InAppAction::CreateWorktrees => "createWorktrees",
                InAppAction::StartAgents => "startAgents",
                InAppAction::Terminal => "terminal",
                InAppAction::Tab => "tab",
                InAppAction::Group => "group",
                InAppAction::Palette => "palette",
            };
            let which = args.get("target").filter(|t| !t.is_empty()).or_else(|| args.get("command")).map_or("", String::as_str);
            format!("{name} {}", f(which)).trim().to_owned()
        }
        BoltNode::Fetch { mode, target, into, as_, .. } => format!(
            "ask{}{}",
            if *mode == FetchMode::Fresh { String::new() } else { format!(" {}", f(target)) },
            match into {
                Some(i) => format!(" → {i}{}", as_.map_or(String::new(), |t| format!(": {}", t.name()))),
                None => String::new(),
            }
        ),
        BoltNode::Send { target, text, .. } => format!("send {}: {}", f(target), f(text)),
        BoltNode::Read { target, .. } => format!("read {}", f(target)),
        BoltNode::ForEach { item, over, .. } => format!("for each {item} in {over}"),
        BoltNode::If { var, op, value, .. } => format!(
            "if {var} {}{}",
            match op {
                IfOp::Empty => "empty",
                IfOp::NotEmpty => "not-empty",
                IfOp::Equals => "equals",
            },
            if *op == IfOp::Equals { format!(" {value}") } else { String::new() }
        ),
        BoltNode::Approve { .. } => "approve".into(),
        BoltNode::Alert { .. } => "alert".into(),
        BoltNode::Set { into, .. } => format!("set {into}"),
    }
}

fn notify(title: &str, body: &str) {
    let (title, body) = (title.to_owned(), body.to_owned());
    super::spawn_root(async move {
        let _ = request_notification_permission().await;
        let _ = show_notification(Notification::new(next_id(), title, body)).await;
    });
}

/// Runs `nodes` in order; a failing node closes its step failed and stops
/// the rest (`runNodes`).
fn run_nodes<'a>(hub: Hub, job_id: &'a str, nodes: &'a [BoltNode], vars: &'a mut Vars) -> Pin<Box<dyn Future<Output = Result<(), String>> + 'a>> {
    Box::pin(async move {
        for node in nodes {
            if is_cancelled(job_id) {
                return Err("cancelled".into());
            }
            let at = open_step(hub, job_id, &label(node, vars), StepStatus::Running);
            if let Err(e) = run_node(hub, job_id, node, vars, at).await {
                close_step(hub, job_id, at, StepStatus::Failed, &e);
                return Err(e);
            }
        }
        Ok(())
    })
}

async fn run_node(hub: Hub, job_id: &str, node: &BoltNode, vars: &mut Vars, at: usize) -> Result<(), String> {
    match node {
        BoltNode::InApp { action, args, .. } => {
            let reply = in_app(hub, job_id, *action, args, vars).await?;
            close_step(hub, job_id, at, StepStatus::Done, &reply);
        }
        BoltNode::Fetch { mode, target, prompt, as_, into, .. } => {
            let v = fetch_node(hub, job_id, *mode, target, prompt, *as_, into.as_deref(), vars).await?;
            let shown = match &v {
                Var::List(l) => serde_json::to_string(l).unwrap_or_default(),
                Var::Str(s) => s.clone(),
            };
            if let Some(into) = into {
                vars.insert(into.clone(), v);
            }
            close_step(hub, job_id, at, StepStatus::Done, &shown);
        }
        BoltNode::Send { target, text, mode, into, .. } => {
            let text = fill_plain(text, vars);
            let members = members_of(hub, target, vars)?;
            let reply = if *mode == SendMode::Headless {
                let tasks: Vec<Task<String>> = members
                    .into_iter()
                    .map(|m| {
                        let (text, job) = (text.clone(), job_id.to_owned());
                        Box::pin(async move {
                            let resume = fork_from(hub.shell, &m, Role::Addressee);
                            let hop = run_hop(&m.cwd, &text, resume, Some(&job)).await;
                            match hop.error {
                                Some(e) => Err(format!("@{}: {e}", m.handle)),
                                None => Ok(hop.text),
                            }
                        }) as Task<String>
                    })
                    .collect();
                all(tasks).await?.join("\n\n")
            } else {
                let want_output = into.is_some();
                let tasks: Vec<Task<String>> = members
                    .into_iter()
                    .map(|m| {
                        let text = text.clone();
                        Box::pin(async move {
                            let session_id = match m.session_id {
                                Some(s) => s,
                                None => open_workspace_terminal(hub.shell, &m.cwd)?,
                            };
                            type_(hub, &session_id, &text).await?;
                            Ok(if want_output { output_of(hub, &session_id, &text) } else { String::new() })
                        }) as Task<String>
                    })
                    .collect();
                let replies = all(tasks).await?.join("\n");
                if want_output { replies } else { String::new() }
            };
            if let Some(into) = into {
                vars.insert(into.clone(), Var::Str(reply.clone()));
            }
            close_step(hub, job_id, at, StepStatus::Done, &reply);
        }
        BoltNode::Read { target, into, .. } => {
            let members: Vec<HubMember> = members_of(hub, target, vars)?.into_iter().filter(|m| m.session_id.is_some()).collect();
            if members.is_empty() {
                return Err(format!("{} has no open terminal", fill_plain(target, vars)));
            }
            let text = members.iter().map(|m| read_latest(hub, m.session_id.as_deref().unwrap_or_default())).collect::<Vec<_>>().join("\n\n");
            if let Some(into) = into {
                vars.insert(into.clone(), Var::Str(text.clone()));
            }
            close_step(hub, job_id, at, StepStatus::Done, &text);
        }
        BoltNode::ForEach { over, item, body, .. } => {
            let items = list_of(over, vars);
            close_step(hub, job_id, at, StepStatus::Done, &if items.is_empty() { "nothing to run".to_owned() } else { items.join(", ") });
            let name = if item.is_empty() { "item".to_owned() } else { item.clone() };
            let tasks: Vec<Task<()>> = items
                .into_iter()
                .map(|it| {
                    let (job, body, name) = (job_id.to_owned(), body.clone(), name.clone());
                    let mut inner = vars.clone();
                    inner.insert(name, Var::Str(it));
                    Box::pin(async move { run_nodes(hub, &job, &body, &mut inner).await }) as Task<()>
                })
                .collect();
            all(tasks).await?;
        }
        BoltNode::If { var, op, value, then, else_, .. } => {
            let v = vars.get(var.as_str());
            let yes = match op {
                IfOp::Equals => v.map(Var::text).unwrap_or_default() == fill_plain(value, vars),
                IfOp::Empty => !truthy(v),
                IfOp::NotEmpty => truthy(v),
            };
            let shown = match v {
                Some(Var::List(l)) => l.join(", "),
                Some(Var::Str(s)) => s.clone(),
                None => String::new(),
            };
            let reply = format!(
                "{} ({var} = {})",
                if yes { "true → then" } else { "false → else" },
                if shown.is_empty() { "empty".to_owned() } else { serde_json::to_string(&shown).unwrap_or_default() }
            );
            close_step(hub, job_id, at, StepStatus::Done, &reply);
            run_nodes(hub, job_id, if yes { then } else { else_ }, vars).await?;
        }
        BoltNode::Set { into, value, .. } => {
            let v = fill_plain(value, vars);
            vars.insert(into.clone(), Var::Str(v.clone()));
            close_step(hub, job_id, at, StepStatus::Done, &v);
        }
        BoltNode::Alert { say, .. } => {
            let say = fill_plain(say, vars);
            notify("Bolt", &say);
            close_step(hub, job_id, at, StepStatus::Done, &say);
        }
        BoltNode::Approve { say, .. } => {
            let say = fill_plain(say, vars);
            patch_job(hub, job_id, |j| {
                if let Some(s) = j.steps.get_mut(at) {
                    s.status = StepStatus::Waiting;
                    s.reply = say.clone();
                }
            });
            let (tx, rx) = oneshot::channel();
            APPROVALS.with(|a| a.borrow_mut().insert(format!("{job_id}:{at}"), tx));
            let _ = rx.await;
            close_step(hub, job_id, at, StepStatus::Done, &say);
        }
    }
    Ok(())
}

/// A palette line's `{:name}`s run as a board job, so their steps show and a failure says why (`expandScripts`).
pub async fn expand_scripts(hub: Hub, line: &str) -> Result<String, String> {
    let has = line.match_indices("{:").any(|(i, _)| {
        let after = &line[i + 2..];
        let len = after.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-')).unwrap_or(after.len());
        len > 0 && after[len..].starts_with('}')
    });
    if !has {
        return Ok(line.to_owned());
    }
    let job_id = new_id("run:");
    hub.post(new_job(job_id.clone(), line.to_owned(), "palette".into(), Some("palette".into()), None, Vec::new()));
    match call_scripts(hub, &job_id, line, &HashMap::new()).await {
        Ok(out) => {
            finish_job(hub, &job_id, None);
            Ok(out)
        }
        Err(e) => {
            finish_job(hub, &job_id, Some(e.clone()));
            Err(e)
        }
    }
}

async fn run_script(hub: Hub, label: String, script: String, text: String, mut vars: Vars) {
    let job_id = new_id("run:");
    hub.post(new_job(job_id.clone(), text, label.clone(), Some(label), None, Vec::new()));
    let result = run_nodes(hub, &job_id, &parse_script(&script).0, &mut vars).await;
    finish_job(hub, &job_id, result.err().map(|e| if e.is_empty() { "failed".to_owned() } else { e }));
}

// ---- @bolt ------------------------------------------------------------------------

fn finish_hop(hub: Hub, job_id: &str, at: usize, hop: &HopResult, cwd: &str) {
    let said = hop.text.trim().split('\n').next_back().unwrap_or_default().to_owned();
    close_step(hub, job_id, at, if hop.error.is_some() { StepStatus::Failed } else { StepStatus::Done }, hop.error.as_deref().unwrap_or(&said));
    let hop_ref = hop.session_id.as_ref().map(|s| Hop { session_id: s.clone(), cwd: cwd.to_owned() });
    patch_job(hub, job_id, |j| {
        if hop_ref.is_some() {
            *j.hop = hop_ref;
        }
        *j.status = if hop.error.is_some() { JobStatus::Failed } else { JobStatus::Done };
        *j.error = hop.error.clone();
        *j.ended_at = Some(unix_ms());
    });
}

/// `@bolt …` (`chatBoltAtom`): a leading @terminal forks its claude; a
/// @workspace or no handle is a fresh session there.
pub async fn chat_bolt(hub: Hub, text: String) {
    let all = targets(hub.shell);
    let handles: Vec<String> = all.iter().map(|t| t.handle.clone()).collect();
    let post = parse_post(&text, &handles);
    let prompt = hub.thread.with_untracked(|t| expand_board_refs(post.body.trim(), t, &all));
    if prompt.is_empty() {
        return;
    }
    let job_id = new_id("run:");
    let to = post.to.clone();
    hub.post(new_job(job_id.clone(), post.body.trim().to_owned(), to.as_ref().map_or("bolt".to_owned(), |t| format!("bolt @{t}")), Some("bolt".into()), None, Vec::new()));
    let fail = |error: String| finish_job(hub, &job_id, Some(error));
    let cwd = hub.shell.directory();
    let mut run_in = cwd.clone();
    let mut resume = None;
    if let Some(to) = &to {
        let Some(member) = all.iter().find(|t| &t.handle == to).and_then(|t| t.members.first().cloned()) else {
            return fail(format!("no target @{to}"));
        };
        run_in = member.cwd.clone();
        if member.session_id.is_some() {
            resume = fork_from(hub.shell, &member, Role::Addressee);
            if resume.is_none() {
                return fail(format!("@{to} has no claude running to fork"));
            }
        }
    }
    let at = open_step(hub, &job_id, "claude", StepStatus::Running);
    let hop = run_hop(
        &run_in,
        &format!(
            "You are driving the Bolt IDE open at {cwd}. Do this using only the mcp__bolt__* tools, passing cwd \"{cwd}\". A \"tab\" means a workspace tab on its own branch and worktree: create whichever of the three is missing, with createWorktrees. Then reply with one line saying what you did:\n\n{prompt}"
        ),
        resume,
        Some(&job_id),
    )
    .await;
    finish_hop(hub, &job_id, at, &hop, &run_in);
}

/// A reply typed under a `@bolt` job (`replyBoltAtom`).
pub async fn reply_bolt(hub: Hub, job_id: String, text: String) {
    let job = hub.thread.with_untracked(|t| {
        t.iter().find_map(|e| match e {
            HubEntry::Job { id, hop, status, .. } if *id == job_id => Some((hop.clone(), *status)),
            _ => None,
        })
    });
    let text = text.trim().to_owned();
    let Some((Some(hop), status)) = job else { return };
    if status == JobStatus::Running || text.is_empty() {
        return;
    }
    patch_job(hub, &job_id, |j| {
        *j.status = JobStatus::Running;
        *j.error = None;
        *j.ended_at = None;
    });
    let you = open_step(hub, &job_id, "you", StepStatus::Running);
    close_step(hub, &job_id, you, StepStatus::Done, &text);
    let at = open_step(hub, &job_id, "claude", StepStatus::Running);
    let result = run_hop(&hop.cwd, &text, Some(hop.session_id.clone()), Some(&job_id)).await;
    finish_hop(hub, &job_id, at, &result, &hop.cwd);
}

// ---- how a line becomes a run -----------------------------------------------------

/// A terminal is held by its session so `{this}` stays that exact terminal; a directory tab by its handle (`selfRef`).
fn self_ref(self_: &ComposeSelf) -> String {
    match &self_.session_id {
        Some(s) => format!("{SESSION}{s}"),
        None => self_.handle.clone(),
    }
}

/// Returns true when the text named a Bolt command, so the caller does not
/// also deliver it (`tryRun`).
pub fn try_run(hub: Hub, text: &str, self_: Option<&ComposeSelf>) -> bool {
    let handles: Vec<String> = targets(hub.shell).into_iter().map(|t| t.handle).collect();
    let commands = hub.commands.get_untracked();
    let Some(mut m) = match_command(text, &commands, &handles) else { return false };
    // A handle resolves to the tab's most recently active terminal; the box was opened on a specific one.
    if let (Some(s), Some(first)) = (self_, m.command.targets.first()) {
        if s.session_id.is_some() && m.bound.get(first) == Some(&s.handle) {
            m.bound.insert(first.clone(), self_ref(s));
        }
    }
    let bad = parse_script(&m.command.script).1.into_iter().next();
    if bad.is_some() || !m.missing.is_empty() {
        hub.post(say(ME, text));
        hub.post(say_error(
            ME,
            match bad {
                Some(b) => format!("{} has an error on line {}: {}", m.command.keyword, b.line, b.message),
                None => format!("{} needs {}.", m.command.keyword, m.missing.iter().map(|s| format!("@{s}")).collect::<Vec<_>>().join(", ")),
            },
        ));
        return true;
    }
    let mut vars: Vars = m.bound.iter().map(|(k, v)| (k.clone(), Var::Str(v.clone()))).collect();
    vars.insert("input".into(), Var::Str(m.input.clone()));
    let name = if m.command.name.is_empty() { m.command.keyword.clone() } else { m.command.name.clone() };
    let (script, text) = (m.command.script.clone(), text.to_owned());
    super::spawn_root(run_script(hub, name, script, text, vars));
    true
}

/// A post or a Bolt command from the board (`submitBoltAtom`).
pub fn submit_bolt(hub: Hub, text: String) {
    if super::is_script(&text) {
        let t = text.trim().to_owned();
        super::spawn_root(run_script(hub, "board".into(), t.clone(), t, HashMap::from([("input".to_owned(), Var::Str(String::new()))])));
        return;
    }
    if !try_run(hub, &text, None) {
        super::spawn_root(submit_post(hub, text));
    }
}

/// The right-click box: a script runs here, a keyword runs its command with
/// this as t1, anything else forks this (`composeSendAtom`).
pub fn compose_send(hub: Hub, text: String, self_: ComposeSelf) {
    let text = text.trim().to_owned();
    if text.is_empty() {
        return;
    }
    if super::is_script(&text) {
        let vars = HashMap::from([("this".to_owned(), Var::Str(self_ref(&self_))), ("input".to_owned(), Var::Str(String::new()))]);
        super::spawn_root(run_script(hub, format!("@{}", self_.handle), text.clone(), text, vars));
        return;
    }
    if text.starts_with('@') {
        return submit_bolt(hub, text);
    }
    let addressed = format!("@{} {text}", self_.handle);
    if try_run(hub, &addressed, Some(&self_)) {
        return;
    }
    match self_.session_id {
        Some(session_id) => super::spawn_root(fleet::bolt_fork(hub, session_id, text)),
        None => super::spawn_root(submit_post(hub, addressed)),
    }
}
