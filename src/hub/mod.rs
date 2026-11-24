//! Bolt's hub (`src/stores/fleetStore.ts`): the thread the palette shows,
//! the handles a line can address, and open choices. The thread is saved
//! as Bolt saves it (`hub-thread.json`, `{ entries }`, the last 300), in
//! this app's data directory.
//!
//! - [`engine`]: what a sent line does — posts to terminals, headless
//!   agent hops, Bolt commands and scripts (`fleetStore`'s hop half,
//!   `boltCommandStore`, `terminalSendStore`).
//! - [`palette`]: the palette and its cards (`BoltPalette`, `HubCards`,
//!   `ScriptEditor`).

pub mod engine;
pub mod palette;

use std::{
    cell::RefCell,
    collections::HashMap,
    path::PathBuf,
    time::Duration,
};

use nsh_core::{prelude::*, time::unix_ms};
use serde::{Deserialize, Serialize};

use crate::{
    commands::Output,
    settings,
    state::{Shell, Term},
};

/// Who the user is in the thread (`ME`).
pub const ME: &str = "me";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StepStatus {
    Pending,
    Running,
    Waiting,
    Done,
    Failed,
}

/// One step of a job (`HubStep`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HubStep {
    pub handle: String,
    pub prompt: String,
    pub status: StepStatus,
    pub reply: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    Running,
    Done,
    Failed,
}

/// A headless agent a job ran, which `openHop` picks up in a terminal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hop {
    pub session_id: String,
    pub cwd: String,
    /// What it ran on; hops saved before Rivet ran others were claude's.
    #[serde(default)]
    pub harness: engine::agents::Harness,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChoiceOption {
    pub key: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tone: Option<String>,
}

/// One entry of the thread (`HubEntry`), in Bolt's JSON shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum HubEntry {
    #[serde(rename_all = "camelCase")]
    Say {
        id: String,
        from: String,
        text: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        error: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    Ask { id: String, from: String, text: String, question: String },
    #[serde(rename_all = "camelCase")]
    Spawn { id: String, handle: String, cwd: String, session_id: String },
    #[serde(rename_all = "camelCase")]
    Job {
        id: String,
        ask: String,
        to: String,
        from: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        delivered: bool,
        /// The script a Bolt command ran, for its card.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        started_at: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ended_at: Option<f64>,
        steps: Vec<HubStep>,
        status: JobStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hop: Option<Hop>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    Result {
        id: String,
        cmd: String,
        items: Vec<Output>,
        done: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    Choice {
        id: String,
        title: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
        options: Vec<ChoiceOption>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        picked: Option<String>,
        done: bool,
    },
    #[serde(rename_all = "camelCase")]
    Finder { id: String, n: u32, done: bool },
}

impl HubEntry {
    pub fn id(&self) -> &str {
        match self {
            HubEntry::Say { id, .. }
            | HubEntry::Ask { id, .. }
            | HubEntry::Spawn { id, .. }
            | HubEntry::Job { id, .. }
            | HubEntry::Result { id, .. }
            | HubEntry::Choice { id, .. }
            | HubEntry::Finder { id, .. } => id,
        }
    }

    /// What a saved entry still running at shutdown comes back as: done,
    /// a running job failed (`settle`).
    fn settled(self) -> HubEntry {
        match self {
            HubEntry::Result { id, cmd, items, note, .. } => HubEntry::Result { id, cmd, items, note, done: true },
            HubEntry::Choice { id, title, detail, options, picked, .. } => {
                HubEntry::Choice { id, title, detail, options, picked, done: true }
            }
            HubEntry::Finder { id, n, .. } => HubEntry::Finder { id, n, done: true },
            HubEntry::Job { id, ask, to, from, run, started_at, ended_at, mut steps, status, hop, error, .. } => {
                for s in &mut steps {
                    if !matches!(s.status, StepStatus::Done | StepStatus::Failed) {
                        s.status = StepStatus::Failed;
                    }
                }
                let running = status == JobStatus::Running;
                HubEntry::Job {
                    id,
                    ask,
                    to,
                    from,
                    delivered: true,
                    run,
                    started_at,
                    ended_at,
                    steps,
                    status: if running { JobStatus::Failed } else { status },
                    hop,
                    error: if running { error.or_else(|| Some("Bolt closed while this was running".into())) } else { error },
                }
            }
            other => other,
        }
    }
}

/// A new thread id (`nextId`).
pub fn next_id() -> String {
    thread_local!(static SEQ: RefCell<u64> = const { RefCell::new(0) });
    let n = SEQ.with(|s| {
        *s.borrow_mut() += 1;
        *s.borrow()
    });
    format!("hub:{}-{n}", unix_ms() as u64)
}

/// A Bolt command (`BoltCommand`), from `bolt-commands.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoltCommand {
    pub id: String,
    pub name: String,
    pub keyword: String,
    /// Slot names; the @handles after the keyword bind to them in order.
    pub targets: Vec<String>,
    pub script: String,
}

/// Who the palette speaks as (`ComposeSelf`): the current workspace, and
/// its terminal when one of its own is shown.
#[derive(Debug, Clone, PartialEq)]
pub struct ComposeSelf {
    pub handle: String,
    pub session_id: Option<String>,
}

/// The palette while open (`boltPaletteAtom`); `n` remounts it.
#[derive(Debug, Clone, PartialEq)]
pub struct PaletteOpen {
    pub n: u64,
    pub self_: Option<ComposeSelf>,
}

/// One concrete place a line can land (`HubMember`): a workspace with no
/// session is asked headlessly or gets a new terminal.
#[derive(Debug, Clone, PartialEq)]
pub struct HubMember {
    pub handle: String,
    pub name: String,
    pub cwd: String,
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetKind {
    Group,
    Workspace,
    Terminal,
}

/// What an @ can name (`HubTarget`).
#[derive(Debug, Clone, PartialEq)]
pub struct HubTarget {
    pub handle: String,
    pub kind: TargetKind,
    pub label: String,
    pub detail: String,
    pub members: Vec<HubMember>,
    pub path: Option<String>,
}

#[derive(Clone, Copy)]
pub struct Hub {
    pub shell: Shell,
    pub thread: Signal<Vec<HubEntry>>,
    pub palette: Signal<Option<PaletteOpen>>,
    pub commands: Signal<Vec<BoltCommand>>,
    /// While true a directory change keeps the palette open
    /// (`navigateInPalette`).
    pub navigating: Signal<bool>,
}

/// An open choice's answer, until it is picked or cancelled: callbacks are
/// not saved with the thread.
struct ChoiceHandler {
    on_pick: Box<dyn FnOnce(Option<String>)>,
    opened: bool,
}

thread_local! {
    static CHOICES: RefCell<HashMap<String, ChoiceHandler>> = RefCell::new(HashMap::new());
}

const THREAD_FILE: &str = "hub-thread.json";
const THREAD_KEEP: usize = 300;

fn thread_path() -> PathBuf {
    settings::data_dir().join(THREAD_FILE)
}

#[derive(Serialize, Deserialize)]
struct SavedThread {
    entries: Vec<HubEntry>,
}

fn load_thread() -> Vec<HubEntry> {
    std::fs::read_to_string(thread_path())
        .ok()
        .and_then(|t| serde_json::from_str::<SavedThread>(&t).ok())
        .map(|s| s.entries.into_iter().map(HubEntry::settled).collect())
        .unwrap_or_default()
}

fn save_thread(entries: &[HubEntry]) {
    let keep = &entries[entries.len().saturating_sub(THREAD_KEEP)..];
    let path = thread_path();
    let write = || -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
        std::fs::write(&tmp, serde_json::to_vec(&SavedThread { entries: keep.to_vec() }).map_err(std::io::Error::other)?)?;
        std::fs::rename(tmp, &path)
    };
    if let Err(e) = write() {
        eprintln!("rivet: could not save the hub thread: {e}");
    }
}

/// Makes the hub and provides it below: the saved thread, the Bolt
/// commands, and the rules that tie them to the shell (a debounced save, a
/// closed palette cancelling choices and finders, a directory change
/// closing it unless the palette itself navigated).
pub fn provide(shell: Shell) -> Hub {
    let hub = Hub {
        shell,
        thread: signal(load_thread()),
        palette: signal(None),
        commands: signal(engine::load_commands()),
        navigating: signal(false),
    };
    provide_context(hub);
    engine::start(hub);

    let pending: RefCell<Option<Spawned<()>>> = RefCell::new(None);
    let first = std::cell::Cell::new(true);
    effect(move || {
        hub.thread.with(|_| ());
        if first.replace(false) {
            return;
        }
        if let Some(t) = pending.borrow_mut().take() {
            t.cancel();
        }
        *pending.borrow_mut() = Some(set_timeout(Duration::from_millis(500), move || {
            hub.thread.with_untracked(|t| save_thread(t));
        }));
    });

    effect(move || {
        let open = hub.palette.with(Option::is_some);
        if open {
            hub.shell.sidebar_open.set(true);
        } else {
            untracked(|| {
                hub.cancel_choices();
                hub.close_finders();
            });
        }
    });

    let last_dir = RefCell::new(shell.settings.with_untracked(|s| s.directory.clone()));
    effect(move || {
        let dir = shell.settings.with(|s| s.directory.clone());
        if *last_dir.borrow() != dir {
            *last_dir.borrow_mut() = dir;
            if !hub.navigating.get_untracked() {
                hub.palette.set(None);
            }
        }
    });
    hub
}

/// The hub, from a component under the root.
pub fn hub() -> Hub {
    use_context::<Hub>().expect("the hub is provided by App")
}

impl Hub {
    /// Opens the palette as Bolt's `bolt.palette` does: speaking as the
    /// current workspace, and as its shown terminal when it has one;
    /// pressing it again closes it.
    pub fn toggle_palette(&self) {
        if self.palette.with_untracked(Option::is_some) {
            self.palette.set(None);
            return;
        }
        let dir = self.shell.directory();
        let targets = targets(self.shell);
        let self_ = targets.iter().find(|t| t.kind == TargetKind::Workspace && t.path.as_deref() == Some(&dir)).map(|t| {
            let shown = self.shell.shown(&dir);
            let session_id = shown
                .and_then(|id| self.shell.terms.with_untracked(|ts| ts.iter().find(|x| x.id == id).map(|x| x.session_id.clone())))
                .or_else(|| t.members.first().and_then(|m| m.session_id.clone()));
            ComposeSelf { handle: t.handle.clone(), session_id }
        });
        self.palette.set(Some(PaletteOpen { n: unix_ms() as u64, self_ }));
    }

    pub fn post(&self, entry: HubEntry) {
        self.thread.update(|t| t.push(entry));
    }

    /// Replaces the entry `id` with what `f` makes of it.
    pub fn patch(&self, id: &str, f: impl FnOnce(&mut HubEntry)) {
        self.thread.update(|t| {
            if let Some(e) = t.iter_mut().find(|e| e.id() == id) {
                f(e);
            }
        });
    }

    /// A palette run's result card gets what the run printed.
    pub fn settle_result(&self, id: &str, outputs: Vec<Output>) {
        self.patch(id, |e| {
            if let HubEntry::Result { items, done, .. } = e {
                *items = outputs;
                *done = true;
            }
        });
    }

    /// Runs `go` (a `cd`) without closing the palette.
    pub async fn navigate_in_palette<F: Future>(&self, go: F) -> F::Output {
        self.navigating.set(true);
        let out = go.await;
        self.navigating.set(false);
        out
    }

    pub fn close_finders(&self) {
        if self.thread.with_untracked(|t| t.iter().any(|e| matches!(e, HubEntry::Finder { done: false, .. }))) {
            self.thread.update(|t| {
                for e in t.iter_mut() {
                    if let HubEntry::Finder { done, .. } = e {
                        *done = true;
                    }
                }
            });
        }
    }

    /// Asks the user to pick one of `options` in the palette, opening it if
    /// it is closed; `on_pick` gets the key, or `None` when cancelled.
    pub fn ask_choice(
        &self,
        title: String,
        detail: Option<String>,
        options: Vec<ChoiceOption>,
        on_pick: impl FnOnce(Option<String>) + 'static,
    ) -> String {
        let id = next_id();
        let opened = self.palette.with_untracked(Option::is_none);
        CHOICES.with(|c| c.borrow_mut().insert(id.clone(), ChoiceHandler { on_pick: Box::new(on_pick), opened }));
        self.post(HubEntry::Choice { id: id.clone(), title, detail, options, picked: None, done: false });
        if opened {
            self.palette.set(Some(PaletteOpen { n: unix_ms() as u64, self_: None }));
        }
        id
    }

    pub fn pick_choice(&self, id: &str, key: String) {
        let Some(handler) = CHOICES.with(|c| c.borrow_mut().remove(id)) else {
            return;
        };
        let picked = key.clone();
        self.patch(id, |e| {
            if let HubEntry::Choice { picked: p, done, .. } = e {
                *p = Some(picked);
                *done = true;
            }
        });
        if handler.opened {
            self.palette.set(None);
        }
        (handler.on_pick)(Some(key));
    }

    pub fn cancel_choices(&self) {
        let open: Vec<ChoiceHandler> = CHOICES.with(|c| c.borrow_mut().drain().map(|(_, h)| h).collect());
        if open.is_empty() {
            return;
        }
        self.thread.update(|t| {
            for e in t.iter_mut() {
                if let HubEntry::Choice { done, .. } = e {
                    *done = true;
                }
            }
        });
        for h in open {
            (h.on_pick)(None);
        }
    }
}

fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.to_lowercase().chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_owned()
}

/// A terminal's handle suffix (`sessionTag`): FNV-1a of its session id in
/// base 36, the last 4 characters, or all 7 for a clash.
pub fn session_tag(session_id: &str, long: bool) -> String {
    let mut h: u32 = 0x811c9dc5;
    for unit in session_id.encode_utf16() {
        h = (h ^ u32::from(unit)).wrapping_mul(0x01000193);
    }
    let mut digits = Vec::new();
    let mut n = h;
    loop {
        digits.push(std::char::from_digit(n % 36, 36).unwrap());
        n /= 36;
        if n == 0 {
            break;
        }
    }
    let tag: String = digits.iter().rev().collect();
    let tag = format!("{tag:0>7}");
    if long { tag } else { tag[tag.len() - 4..].to_owned() }
}

/// Every @-addressable target (`hubTargetsAtom`): each tab (pinned first)
/// as a workspace, each of its terminals as `<tab>/<tag>`, each group of
/// tabs.
pub fn targets(shell: Shell) -> Vec<HubTarget> {
    let s = shell.settings.get_untracked();
    let terms: Vec<Term> = shell.terms.get_untracked();
    let tabs: Vec<_> = s.pinned.iter().chain(&s.tabs).collect();
    let mut seen: HashMap<String, u32> = HashMap::new();
    let mut unique = |name: &str, fallback: &str| {
        let base = [slug(name), slug(fallback)].into_iter().find(|b| !b.is_empty()).unwrap_or_else(|| "terminal".into());
        let n = seen.entry(base.clone()).or_insert(0);
        *n += 1;
        if *n == 1 { base } else { format!("{base}-{n}") }
    };
    let mut out = Vec::new();
    let mut primary: HashMap<String, HubMember> = HashMap::new();
    for tab in &tabs {
        let mine: Vec<&Term> = terms.iter().filter(|t| t.cwd == tab.path).collect();
        let leaf = tab.path.rsplit('/').find(|p| !p.is_empty()).unwrap_or("");
        let handle = unique(&tab.name, leaf);
        let member = HubMember { handle: handle.clone(), name: tab.name.clone(), cwd: tab.path.clone(), session_id: None };
        primary.insert(tab.path.clone(), member.clone());
        out.push(HubTarget {
            handle: handle.clone(),
            kind: TargetKind::Workspace,
            label: tab.name.clone(),
            detail: match mine.len() {
                0 => "no terminal yet".into(),
                1 => "1 terminal".into(),
                n => format!("{n} terminals"),
            },
            members: vec![member],
            path: Some(tab.path.clone()),
        });
        let mut taken: Vec<String> = Vec::new();
        for (i, t) in mine.iter().enumerate() {
            let suffix = [session_tag(&t.session_id, false), session_tag(&t.session_id, true), slug(&t.session_id)]
                .into_iter()
                .find(|s| !s.is_empty() && !taken.contains(s))
                .expect("a session id is unique");
            taken.push(suffix.clone());
            let own = format!("{handle}/{suffix}");
            out.push(HubTarget {
                handle: own.clone(),
                kind: TargetKind::Terminal,
                label: format!("{} · terminal {}", tab.name, i + 1),
                detail: engine::terminal_status(t.id).label(),
                members: vec![HubMember { handle: own, name: tab.name.clone(), cwd: t.cwd.clone(), session_id: Some(t.session_id.clone()) }],
                path: None,
            });
        }
    }
    for group in &s.groups {
        let members: Vec<HubMember> = tabs
            .iter()
            .filter(|t| t.group.as_deref() == Some(group))
            .filter_map(|t| primary.get(&t.path).cloned())
            .collect();
        if members.is_empty() {
            continue;
        }
        out.push(HubTarget {
            handle: unique(group, "group"),
            kind: TargetKind::Group,
            label: group.clone(),
            detail: format!("{} workspaces", members.len()),
            members,
            path: None,
        });
    }
    out
}

/// Who a post is for and whom it references (`parsePost`): the leading
/// known @handles address it; later known ones are references.
#[derive(Debug, Clone, PartialEq)]
pub struct Post {
    pub to: Option<String>,
    pub tos: Vec<String>,
    pub refs: Vec<String>,
    pub body: String,
}

fn handle_at(s: &str) -> Option<(usize, usize)> {
    // `@[a-z0-9/-]+`, case-insensitive: the span of the name after `@`.
    let start = s.find('@')? + 1;
    let len = s[start..]
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '/' || c == '-'))
        .unwrap_or(s.len() - start);
    Some((start, len))
}

pub fn parse_post(text: &str, handles: &[String]) -> Post {
    let known = |h: &str| handles.iter().any(|k| k == h);
    let mut tos: Vec<String> = Vec::new();
    let mut rest = text;
    loop {
        let trimmed = rest.trim_start();
        let Some(after) = trimmed.strip_prefix('@') else { break };
        let len = after.find(|c: char| !(c.is_ascii_alphanumeric() || c == '/' || c == '-')).unwrap_or(after.len());
        let h = after[..len].to_lowercase();
        if len == 0 || !known(&h) {
            break;
        }
        if !tos.contains(&h) {
            tos.push(h);
        }
        rest = after[len..].trim_start();
    }
    let to = tos.first().cloned();
    let body = if to.is_some() { rest.to_owned() } else { text.trim().to_owned() };
    let mut refs = Vec::new();
    let mut scan = body.as_str();
    while let Some((start, len)) = handle_at(scan) {
        let h = scan[start..start + len].to_lowercase();
        if len > 0 && !tos.contains(&h) && known(&h) && !refs.contains(&h) {
            refs.push(h);
        }
        scan = &scan[start + len..];
    }
    Post { to, tos, refs, body }
}

/// Folders matching a finder query: prefix matches first, then the rest
/// that contain it (`finderMatches`).
pub fn finder_matches(folders: &[String], query: &str) -> Vec<String> {
    let q = query.to_lowercase();
    if q.is_empty() {
        return folders.to_vec();
    }
    let starts: Vec<String> = folders.iter().filter(|f| f.to_lowercase().starts_with(&q)).cloned().collect();
    let rest = folders.iter().filter(|f| !starts.contains(f) && f.to_lowercase().contains(&q)).cloned();
    starts.iter().cloned().chain(rest).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_tags_match_bolts_fnv_hash() {
        // Bolt's `sessionTag` (node) gives these for the same ids.
        assert_eq!(session_tag("pty-1-1", true), "1hyxdss");
        assert_eq!(session_tag("pty-1-1", false), "xdss");
        assert_eq!(session_tag("pty-1785988222668-3", false), "hr5h");
    }

    #[test]
    fn posts_split_addressees_from_references() {
        let handles: Vec<String> = ["backend", "web", "web/k3f9"].iter().map(|s| s.to_string()).collect();
        let p = parse_post("@backend @Web fix it, ask @web/k3f9 and @nobody", &handles);
        assert_eq!(p.tos, ["backend", "web"]);
        assert_eq!(p.body, "fix it, ask @web/k3f9 and @nobody");
        assert_eq!(p.refs, ["web/k3f9"]);
        assert_eq!(parse_post("@nobody hi", &handles).to, None);
    }
}
