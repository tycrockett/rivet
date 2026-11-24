//! The shell's state: Bolt's settings and the terminals of every
//! workspace, laid out in columns, as signals every view reads through
//! [`shell()`]. The PTYs themselves are not signals: they live in a
//! per-thread table keyed by terminal id, on the engine thread that owns
//! the views, with each terminal's emulator, so a terminal moved to another
//! column keeps its screen and scrollback.

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    path::Path,
    rc::Rc,
    time::Duration,
};

use tokio::sync::mpsc::UnboundedReceiver;

use nsh_base::{Terminal, TerminalApi, TerminalEmulator, TerminalEmulatorEvent, TerminalSize, VtEmulator};
use nsh_core::{prelude::*, time::unix_ms};

use crate::{
    collapse::{Collapse, Strip},
    git::{self, BranchStatus},
    hub::engine::spawn_root,
    panes::{Columns, MaximizeSnapshot, PaneAction},
    pty::{self, Output, Pty},
    ptyd::Modes,
    settings::{self, Settings},
};

/// A terminal of a workspace (Bolt's terminal tab: `mode: "terminal"`,
/// `filePath` = its cwd).
#[derive(Debug, Clone, PartialEq)]
pub struct Term {
    pub id: u64,
    /// Bolt's pty session id shape, `pty-<ms>-<n>`: what hub handles hash
    /// (`sessionTag`) and what the thread stores.
    pub session_id: String,
    pub cwd: String,
    /// What the program last set as its title.
    pub title: String,
}

/// A file open for editing in a workspace's columns (notes,
/// `keybindings.json`, what the MCP `openFile` shows; Bolt's file tab:
/// `mode: "file"`), shown as its text or as its changes against `HEAD`; or,
/// with a [`Proposal`], a diff tab (Bolt's `mode: "diff"`, `openDiffAtom`).
#[derive(Debug, Clone, PartialEq)]
pub struct Doc {
    pub id: u64,
    /// The workspace whose columns hold it.
    pub cwd: String,
    /// The file, absolute. A proposal's is the file it proposes changes to,
    /// which nothing reads or writes for the tab.
    pub path: String,
    pub mode: DocMode,
    /// Set: a diff tab over this text instead of the file.
    pub proposal: Option<Proposal>,
    /// A range the file's editor selects once it shows, then clears
    /// (`pendingRevealAtom`).
    pub select: Option<Select>,
}

/// What a diff tab compares: the file as it was when the tab opened and the
/// text proposed for it (`openDiffAtom`'s `original` and `modified`).
#[derive(Debug, Clone, PartialEq)]
pub struct Proposal {
    /// The `tab_name` the caller gave, which `close_tab` also answers to.
    pub name: Option<String>,
    pub original: String,
    pub modified: String,
}

/// A range of a file's text, as (line, column) from 0, columns in UTF-16
/// code units (the editor's).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Select {
    pub anchor: (u32, u32),
    pub head: (u32, u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocMode {
    File,
    /// The working file against `HEAD`, side by side or unified.
    Diff { unified: bool },
}

/// A workspace's Changes tab (Bolt's `ChangesTab`, opened by `diff`): the
/// files a diff touches, one shown as a diff.
#[derive(Debug, Clone, PartialEq)]
pub struct Changes {
    pub id: u64,
    pub cwd: String,
    /// What it diffs, as `diff` pinned it: a commit, or `base..target`;
    /// `None` decides on each load (`resolveChangesModeAtom`).
    pub spec: Option<String>,
    /// Changes each time `diff` runs, so the tab loads again.
    pub n: u64,
}

/// A web tab of a workspace (Bolt's `WebTab`, `mode: "web"`): its page
/// stays loaded while the tab is open, shown or not (`web.rs`).
#[derive(Debug, Clone, PartialEq)]
pub struct Web {
    pub id: u64,
    pub cwd: String,
    /// The URL it opened on, empty for a blank tab (Bolt's tab id
    /// `web:<url>`): `browser <url>` again shows this tab.
    pub opened: String,
    /// The page's URL now, empty while none loaded.
    pub url: String,
    pub loading: bool,
    pub back: bool,
    pub forward: bool,
    /// The `browse` MCP tool may navigate, click and type in it (Bolt's
    /// robot button).
    pub armed: bool,
    /// Bumped by each shift-click in the page, for the toolbar's flash.
    pub inspected: u64,
}

#[derive(Clone, Copy)]
pub struct Shell {
    pub settings: Signal<Settings>,
    pub terms: Signal<Vec<Term>>,
    /// The files open in columns.
    pub docs: Signal<Vec<Doc>>,
    /// The web tabs, every workspace's.
    pub webs: Signal<Vec<Web>>,
    /// The Changes tabs open in columns, one per workspace at most.
    pub changes: Signal<Vec<Changes>>,
    /// The Settings tab, and the workspace whose columns hold it.
    pub settings_tab: Signal<Option<(u64, String)>>,
    /// Each workspace's columns, by its path.
    pub columns: Signal<HashMap<String, Columns>>,
    pub statuses: Signal<HashMap<String, BranchStatus>>,
    pub sidebar_open: Signal<bool>,
    /// The strip's collapse state (Pinned, the groups, the ⌘0 cycle).
    pub collapse: Signal<Collapse>,
    /// Bumped by each ⌘0 and each header click (`collapseTickAtom`): the
    /// cycle's dots show for a moment.
    pub collapse_tick: Signal<u64>,
    /// The pane overlay (⌘'), up.
    pub panes_overlay: Signal<bool>,
    /// A tab or column was chosen (a click, a hint) since the palette last
    /// took the keys: the active column's terminal takes them, even from
    /// the open palette, and clears it.
    pub focus_pending: Signal<bool>,
    /// Bumped by each submit: a command still running when a newer one
    /// starts must not move the directory (`useCommandExecution`'s
    /// `runIdRef`).
    pub run_id: Signal<u64>,
}

struct Live {
    pty: Pty,
    /// When the program last printed (Unix ms), `pty:status`'s
    /// `lastOutputAt`.
    last_output_at: f64,
    /// The mounted view, for its screen's text and the program's output,
    /// with the token its mount got: a view unmounting clears only its own.
    view: Option<(u64, ComponentRef<Terminal>)>,
    /// The emulator, shared by whichever view shows the terminal and fed
    /// directly while none does. Made by the first view, at its size.
    emulator: Option<Rc<RefCell<VtEmulator>>>,
    /// Output from before any view showed the terminal (a shell's first
    /// prompt, a terminal opened in another workspace, the daemon's replay
    /// of a reattached one), fed to the emulator the first view makes.
    pending: String,
    /// The input modes `pending`'s output turned on, which its cap may
    /// have cut from its start (the daemon's replay begins with them).
    pending_modes: Modes,
    /// The program asked for bracketed paste (mode 2004), as xterm's
    /// `modes.bracketedPasteMode` reports.
    bracketed: bool,
}

/// Lines a terminal keeps above its screen.
pub const SCROLLBACK: usize = 5000;

/// The `settings.extra` key of the code text size, its default (Bolt's
/// xterm and Monaco size) and the sizes ⌘+ / ⌘- step within.
const FONT_SIZE_KEY: &str = "fontSize";
pub const DEFAULT_FONT_SIZE: f32 = 12.0;
const FONT_SIZES: std::ops::RangeInclusive<f32> = 8.0..=32.0;

fn font_size_of(s: &Settings) -> f32 {
    s.extra.get(FONT_SIZE_KEY).and_then(|v| v.as_f64()).map_or(DEFAULT_FONT_SIZE, |v| (v as f32).clamp(*FONT_SIZES.start(), *FONT_SIZES.end()))
}

thread_local! {
    static PTYS: RefCell<HashMap<u64, Live>> = RefCell::new(HashMap::new());
    /// Tab ids, terminals' and files' alike: a column holds both.
    static NEXT_ID: std::cell::Cell<u64> = const { std::cell::Cell::new(1) };
    static NEXT_VIEW: std::cell::Cell<u64> = const { std::cell::Cell::new(1) };
    /// Where each workspace's maximize cycle is.
    static MAXIMIZE: RefCell<HashMap<String, MaximizeSnapshot>> = RefCell::new(HashMap::new());
}

fn next_id() -> u64 {
    NEXT_ID.with(|n| {
        let id = n.get();
        n.set(id + 1);
        id
    })
}

/// Makes the shell's state and provides it to everything below.
pub fn provide() -> Shell {
    let loaded = settings::load();
    crate::notes::migrate_home_memories(&loaded);
    let shell = Shell {
        settings: signal(loaded),
        terms: signal(Vec::new()),
        docs: signal(Vec::new()),
        webs: signal(Vec::new()),
        changes: signal(Vec::new()),
        settings_tab: signal(None),
        columns: signal(HashMap::new()),
        statuses: signal(HashMap::new()),
        sidebar_open: signal(true),
        collapse: signal(Collapse::load()),
        collapse_tick: signal(0),
        panes_overlay: signal(false),
        focus_pending: signal(false),
        run_id: signal(0),
    };
    provide_context(shell);
    // Saved as it changes (Bolt debounces 500 ms; the file is small).
    let first = std::cell::Cell::new(true);
    effect(move || {
        let c = shell.collapse.get();
        if !first.replace(false) {
            c.save();
        }
    });
    restore_terminals(shell);
    save_terminals(shell);
    shell
}

/// Puts back the terminals of the last run (Bolt's `loadSession`): each
/// saved session the daemon still holds is attached again, into the
/// workspace and column it was in, its scrollback replayed. A live session
/// nothing saved refers to is attached too, in a column of its own in its
/// workspace: Bolt kills those, but here a lost or stale layout file must
/// never end a shell (an agent in it with it). Only shells that already
/// exited are forgotten.
fn restore_terminals(shell: Shell) {
    let layout = match saved::load() {
        Ok(layout) => layout,
        Err(e) => {
            eprintln!("rivet: could not read the saved terminals, attaching every live one: {e}");
            saved::Layout { version: 0, workspaces: Vec::new() }
        }
    };
    let sessions = match Pty::list() {
        Ok(sessions) => sessions,
        Err(e) => return eprintln!("rivet: no pty daemon, terminals not restored: {e}"),
    };
    let alive: HashSet<&str> = sessions.iter().filter(|s| s.alive).map(|s| s.session_id.as_str()).collect();
    let mut kept: HashSet<String> = HashSet::new();
    let mut restored = HashMap::new();
    for ws in layout.workspaces {
        let mut cols = Columns::default();
        let mut active = None;
        for (index, col) in ws.columns.iter().enumerate() {
            let mut ids = Vec::new();
            let mut shown = None;
            for t in &col.terminals {
                if !alive.contains(t.session_id.as_str()) || !kept.insert(t.session_id.clone()) {
                    continue;
                }
                let Ok((pty, attached, output)) = Pty::attach(&t.session_id) else { continue };
                if col.shown.as_deref() == Some(&t.session_id) {
                    shown = Some(ids.len());
                }
                ids.push(shell.adopt(pty, output, &t.cwd, t.title.clone(), attached.last_output_at, attached.replay));
            }
            let Some((&first, rest)) = ids.split_first() else { continue };
            cols.add(first, true);
            for id in rest {
                cols.add(*id, false);
            }
            let group = cols.active_group;
            if let Some(g) = cols.groups.iter_mut().find(|g| g.id == group) {
                g.active = shown.unwrap_or(ids.len() - 1);
                g.width = col.width;
            }
            if index == ws.active_column {
                active = Some(group);
            }
        }
        if let Some(group) = active {
            cols.active_group = group;
        }
        // Widths are shares of the row: with a column gone (a file's, or a
        // dead shell's) they no longer add up, and the columns share evenly.
        if (cols.groups.iter().map(|g| g.width).sum::<f32>() - 1.0).abs() > 0.01 {
            cols.groups.iter_mut().for_each(|g| g.width = 0.0);
        }
        if !cols.groups.is_empty() {
            restored.insert(ws.cwd, cols);
        }
    }
    let unsaved: Vec<&pty::SessionInfo> = sessions.iter().filter(|s| s.alive && !kept.contains(&s.session_id)).collect();
    for s in unsaved {
        let Ok((pty, attached, output)) = Pty::attach(&s.session_id) else { continue };
        let id = shell.adopt(pty, output, &s.cwd, String::new(), attached.last_output_at, attached.replay);
        restored.entry(s.cwd.clone()).or_insert_with(Columns::default).add(id, true);
        kept.insert(s.session_id.clone());
    }
    shell.columns.set(restored);
    for s in sessions.iter().filter(|s| !s.alive) {
        pty::forget_session(&s.session_id);
    }
    eprintln!("rivet: {} terminal(s) reattached", kept.len());
}

/// Saves the terminals' layout as it changes, to what [`restore_terminals`]
/// reads. A change of which shells sit where is written at once (a shell
/// opened just before quitting comes back in its own column, not its
/// place, otherwise); titles alone, which an agent rewrites constantly, at
/// most once a second.
fn save_terminals(shell: Shell) {
    let written = Rc::new(RefCell::new(None::<saved::Layout>));
    let timer = Rc::new(std::cell::Cell::new(false));
    effect(move || {
        let layout = saved::of(shell);
        let mut file = written.borrow_mut();
        let Some(last) = file.as_ref() else {
            // The first run is the restored state, already what the file says
            // (or all there is, should restoring have failed).
            *file = Some(layout);
            return;
        };
        if *last == layout {
            return;
        }
        if last.without_titles() != layout.without_titles() {
            layout.save();
            *file = Some(layout);
        } else if !timer.replace(true) {
            let (written, timer) = (written.clone(), timer.clone());
            set_timeout(Duration::from_secs(1), move || {
                timer.set(false);
                let layout = saved::of(shell);
                layout.save();
                *written.borrow_mut() = Some(layout);
            });
        }
    });
}

/// The terminals' layout on disk (`terminals.json` in the data directory):
/// each workspace's columns, left to right, with the terminals of each in
/// order and the one shown, as the daemon's session ids. Files and Changes
/// tabs are not kept.
mod saved {
    use std::collections::HashMap;

    use serde::{Deserialize, Serialize};

    use super::{Shell, settings};

    const FILE: &str = "terminals.json";
    const VERSION: u32 = 1;

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct Layout {
        pub version: u32,
        pub workspaces: Vec<Workspace>,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct Workspace {
        pub cwd: String,
        /// Index into `columns` of the active one.
        pub active_column: usize,
        pub columns: Vec<Column>,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct Column {
        /// Share of the row (0: unsized).
        pub width: f32,
        /// Session id of the terminal shown, if one is.
        pub shown: Option<String>,
        pub terminals: Vec<SavedTerm>,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct SavedTerm {
        pub session_id: String,
        pub cwd: String,
        pub title: String,
    }

    impl Layout {
        pub fn without_titles(&self) -> Layout {
            let mut l = self.clone();
            for t in l.workspaces.iter_mut().flat_map(|w| &mut w.columns).flat_map(|c| &mut c.terminals) {
                t.title.clear();
            }
            l
        }

        pub fn save(&self) {
            let dir = settings::data_dir();
            let write = std::fs::create_dir_all(&dir).and_then(|()| {
                let tmp = dir.join(format!("{FILE}.tmp"));
                std::fs::write(&tmp, serde_json::to_string_pretty(self).unwrap_or_default())?;
                std::fs::rename(&tmp, dir.join(FILE))
            });
            if let Err(e) = write {
                eprintln!("rivet: could not save the terminals: {e}");
            }
        }
    }

    /// The saved layout; none saved yet is an empty one.
    pub fn load() -> Result<Layout, String> {
        let text = match std::fs::read_to_string(settings::data_dir().join(FILE)) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Layout { version: VERSION, workspaces: Vec::new() }),
            Err(e) => return Err(e.to_string()),
        };
        let layout: Layout = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        if layout.version != VERSION {
            return Err(format!("version {} is not {VERSION}", layout.version));
        }
        Ok(layout)
    }

    /// The terminals as the columns hold them now. Reads the signals, for an
    /// effect to follow.
    pub fn of(shell: Shell) -> Layout {
        let terms: HashMap<u64, super::Term> = shell.terms.with(|t| t.iter().map(|t| (t.id, t.clone())).collect());
        let mut workspaces: Vec<Workspace> = shell.columns.with(|all| {
            all.iter()
                .filter_map(|(cwd, cols)| {
                    let mut columns = Vec::new();
                    let mut active_column = 0;
                    for g in &cols.groups {
                        let list: Vec<SavedTerm> = g
                            .tabs
                            .iter()
                            .filter_map(|id| terms.get(id))
                            .map(|t| SavedTerm { session_id: t.session_id.clone(), cwd: t.cwd.clone(), title: t.title.clone() })
                            .collect();
                        if list.is_empty() {
                            continue;
                        }
                        if g.id == cols.active_group {
                            active_column = columns.len();
                        }
                        let shown = g.tabs.get(g.active).and_then(|id| terms.get(id)).map(|t| t.session_id.clone());
                        columns.push(Column { width: g.width, shown, terminals: list });
                    }
                    (!columns.is_empty()).then(|| Workspace { cwd: cwd.clone(), active_column, columns })
                })
                .collect()
        });
        workspaces.sort_by(|a, b| a.cwd.cmp(&b.cwd));
        Layout { version: VERSION, workspaces }
    }
}

/// The shell's state, from a component under the root.
pub fn shell() -> Shell {
    use_context::<Shell>().expect("the shell's state is provided by App")
}

impl Shell {
    pub fn directory(&self) -> String {
        self.settings.with(|s| s.directory.clone())
    }

    /// Replaces the settings and writes them to disk.
    pub fn set_settings(&self, mut next: Settings) {
        next.assign_group_ids();
        if let Err(e) = settings::save(&next) {
            eprintln!("rivet: could not save settings: {e}");
        }
        self.settings.set(next);
    }

    pub fn update_settings(&self, f: impl FnOnce(&mut Settings)) {
        let mut next = self.settings.get_untracked();
        f(&mut next);
        self.set_settings(next);
    }

    pub fn set_directory(&self, path: String) {
        self.update_settings(|s| s.directory = path.clone());
        self.refresh_status(path);
    }

    /// Reads `path`'s git state off the engine thread (Bolt refetches it on
    /// a directory change and after each command, without polling).
    pub fn refresh_status(&self, path: String) {
        let statuses = self.statuses;
        spawn(async move {
            let key = path.clone();
            match git::off_thread(move || git::branch_status(&path)).await {
                Some(status) => statuses.update(|m| {
                    m.insert(key, status);
                }),
                None => statuses.update(|m| {
                    m.remove(&key);
                }),
            }
        });
    }

    /// Opens a terminal in `cwd` and shows it (`openTerminalAtom`): in the
    /// active column, or a new one after it with `openInNewColumn`. One
    /// opened in the current workspace takes the keys, even from the open
    /// palette that ran it.
    pub fn open_terminal(&self, cwd: String) -> Result<u64, String> {
        let (pty, output) = Pty::spawn(&cwd).map_err(|e| format!("Could not start a shell: {e}"))?;
        let id = self.adopt(pty, output, &cwd, String::new(), unix_ms(), String::new());
        let new_column = self.opens_new_column();
        let here = cwd == self.directory_untracked();
        self.columns.update(|c| c.entry(cwd).or_default().add(id, new_column));
        if here {
            self.focus_pending.set(true);
        }
        Ok(id)
    }

    /// Makes the daemon's session `pty` terminal `id` of `cwd`'s: its table
    /// entry and the task that feeds its output to whichever view shows it
    /// for as long as the shell lives, not as long as one view does. Only
    /// the daemon reporting that the shell exited closes the tab: an output
    /// that merely stops (the connection to the daemon lost) leaves the tab
    /// and the shell alone, which a close would kill with whatever it runs.
    /// `replay` waits for the first view, ahead of the live output. The
    /// caller puts the tab in a column.
    fn adopt(&self, pty: Pty, mut output: UnboundedReceiver<Output>, cwd: &str, title: String, last_output_at: f64, replay: String) -> u64 {
        let id = next_id();
        let session_id = pty.session_id().to_owned();
        PTYS.with(|p| {
            let mut live = Live { pty, last_output_at, view: None, emulator: None, pending: String::new(), pending_modes: Modes::default(), bracketed: false };
            live.hold(&replay);
            p.borrow_mut().insert(id, live);
        });
        self.terms.update(|t| t.push(Term { id, session_id, cwd: cwd.to_owned(), title }));
        let shell = *self;
        spawn_root(async move {
            while let Some(out) = output.recv().await {
                match out {
                    Output::Data(mut chunk) => {
                        let exited = gather_redraw(&mut output, &mut chunk).await;
                        note_output(id);
                        write_output(shell, id, chunk);
                        if exited {
                            eprintln!("rivet: terminal {id}: its shell exited");
                            return shell.close_tab(id);
                        }
                    }
                    Output::Exit => {
                        eprintln!("rivet: terminal {id}: its shell exited");
                        return shell.close_tab(id);
                    }
                }
            }
            eprintln!("rivet: terminal {id} lost its output; its shell is left running");
        });
        id
    }

    /// Opens `path` in a new terminal in `cwd` running Helix (`hx`), at
    /// `at` (line and column from 0) when given. A file inside `cwd` goes
    /// relative to it, as Helix's status line then shows it.
    pub fn open_in_helix(&self, cwd: String, path: &Path, at: Option<(u32, u32)>) -> Result<(), String> {
        let dir = Path::new(&cwd);
        // Canonical paths resolve links (`/tmp` is `/private/tmp`).
        let canonical = dir.canonicalize().ok();
        let target = path.strip_prefix(dir).ok().or_else(|| path.strip_prefix(canonical.as_ref()?).ok()).unwrap_or(path);
        let position = at.map_or_else(String::new, |(line, column)| format!(":{}:{}", line + 1, column + 1));
        let line = format!("hx {}{position}", git::shell_quote(&target.to_string_lossy()));
        self.run_in_terminal(cwd, &line)
    }

    /// A new terminal in `cwd` running `line`, typed once its shell is up.
    pub fn run_in_terminal(&self, cwd: String, line: &str) -> Result<(), String> {
        let id = self.open_terminal(cwd)?;
        let line = format!("{line}\n");
        set_timeout(Duration::from_millis(300), move || pty_write(id, &line));
        Ok(())
    }

    /// Opens `path` for editing (Monaco's file tab: notes,
    /// `keybindings.json`), saved as it is typed, and shows it: the tab
    /// already open on it if there is one, else a new one.
    pub fn open_editable(&self, cwd: String, path: String) {
        self.open_editable_at(cwd, path, None);
    }

    /// [`Self::open_editable`], then `select` selected once the editor shows
    /// (`openFileAtLineAtom`).
    pub fn open_editable_at(&self, cwd: String, path: String, select: Option<Select>) {
        let open = self.docs.with_untracked(|d| d.iter().find(|d| d.cwd == cwd && d.path == path && d.proposal.is_none()).map(|d| d.id));
        if let Some(id) = open {
            self.docs.update(|d| {
                if let Some(doc) = d.iter_mut().find(|d| d.id == id) {
                    doc.mode = DocMode::File;
                    doc.select = select;
                }
            });
            self.show(id);
            return;
        }
        let id = next_id();
        self.docs.update(|d| d.push(Doc { id, cwd: cwd.clone(), path, mode: DocMode::File, proposal: None, select }));
        let new_column = self.opens_new_column();
        self.columns.update(|c| c.entry(cwd).or_default().add(id, new_column));
    }

    /// Opens a diff tab of `path` against proposed text and shows it
    /// (`openDiffAtom`): the tab already open on `path` is shown again, with
    /// the new text (Bolt's keeps the first).
    pub fn open_proposal(&self, cwd: String, path: String, proposal: Proposal) {
        let open = self.docs.with_untracked(|d| d.iter().find(|d| d.cwd == cwd && d.path == path && d.proposal.is_some()).map(|d| d.id));
        if let Some(id) = open {
            self.docs.update(|d| {
                if let Some(doc) = d.iter_mut().find(|d| d.id == id) {
                    doc.proposal = Some(proposal);
                }
            });
            self.show(id);
            return;
        }
        let id = next_id();
        self.docs.update(|d| {
            d.push(Doc { id, cwd: cwd.clone(), path, mode: DocMode::Diff { unified: false }, proposal: Some(proposal), select: None })
        });
        let new_column = self.opens_new_column();
        self.columns.update(|c| c.entry(cwd).or_default().add(id, new_column));
    }

    /// `openInNewColumn` (Settings): new terminals, files and panels open
    /// in a column of their own after the active one (`opensNewColumn`).
    pub fn opens_new_column(&self) -> bool {
        self.settings.with_untracked(|s| s.extra.get("openInNewColumn").and_then(|v| v.as_bool()) == Some(true))
    }

    /// [`Self::opens_new_column`], for a view that follows it.
    pub fn opens_new_column_tracked(&self) -> bool {
        self.settings.with(|s| s.extra.get("openInNewColumn").and_then(|v| v.as_bool()) == Some(true))
    }

    /// `fontSize` (⌘+ / ⌘-): the code text size of terminals, files and
    /// diffs, [`DEFAULT_FONT_SIZE`] while unset; for a view that follows it.
    pub fn font_size_tracked(&self) -> f32 {
        self.settings.with(font_size_of)
    }

    /// Steps the code text size by `by` points, within [`FONT_SIZES`]; back
    /// at the default, the setting is removed.
    pub fn zoom_text(&self, by: f32) {
        let size = (self.settings.with_untracked(font_size_of) + by).clamp(*FONT_SIZES.start(), *FONT_SIZES.end());
        if size == self.settings.with_untracked(font_size_of) {
            return;
        }
        self.update_settings(|s| {
            if size == DEFAULT_FONT_SIZE {
                s.extra.remove(FONT_SIZE_KEY);
            } else {
                s.extra.insert(FONT_SIZE_KEY.into(), size.into());
            }
        });
    }

    /// Shows the Settings tab (`openSettingsAtom`): the one open, else a new
    /// one. Columns are per workspace here, so one open in another
    /// workspace's columns moves to the current one's.
    pub fn open_settings(&self) {
        let cwd = self.directory();
        if let Some((id, at)) = self.settings_tab.get_untracked() {
            if at == cwd {
                return self.show(id);
            }
            self.close_tab(id);
        }
        let id = next_id();
        self.settings_tab.set(Some((id, cwd.clone())));
        let new_column = self.opens_new_column();
        self.columns.update(|c| c.entry(cwd).or_default().add(id, new_column));
    }

    pub fn set_doc_mode(&self, id: u64, mode: DocMode) {
        self.docs.update(|d| {
            if let Some(doc) = d.iter_mut().find(|d| d.id == id) {
                doc.mode = mode;
            }
        });
    }

    /// Opens `cwd`'s Changes tab on `spec` and shows it (`openChangesAtom`
    /// and `loadChangedFilesAtom`): the one already open loads again.
    pub fn open_changes(&self, cwd: String, spec: Option<String>) {
        let n = next_id();
        let open = self.changes.with_untracked(|c| c.iter().find(|c| c.cwd == cwd).map(|c| c.id));
        if let Some(id) = open {
            self.set_changes_spec(id, spec);
            self.show(id);
            return;
        }
        let id = next_id();
        self.changes.update(|c| c.push(Changes { id, cwd: cwd.clone(), spec, n }));
        let new_column = self.opens_new_column();
        self.columns.update(|c| c.entry(cwd).or_default().add(id, new_column));
    }

    /// Pins Changes tab `id` to `spec` (`None` hands the choice back) and
    /// loads it again (`pickCommitAtom`).
    pub fn set_changes_spec(&self, id: u64, spec: Option<String>) {
        let n = next_id();
        self.changes.update(|c| {
            if let Some(tab) = c.iter_mut().find(|c| c.id == id) {
                tab.spec = spec;
                tab.n = n;
            }
        });
    }

    /// Opens a web tab on `url` (blank when empty) and shows it
    /// (`openWebAtom`): the tab of `cwd` that opened on the same URL is
    /// shown again instead; blank tabs are always new.
    pub fn open_web(&self, cwd: String, url: String) {
        let open = (!url.is_empty())
            .then(|| self.webs.with_untracked(|w| w.iter().find(|w| w.cwd == cwd && w.opened == url).map(|w| w.id)))
            .flatten();
        if let Some(id) = open {
            return self.show(id);
        }
        let id = next_id();
        self.webs.update(|w| {
            w.push(Web {
                id,
                cwd: cwd.clone(),
                opened: url.clone(),
                url,
                loading: false,
                back: false,
                forward: false,
                armed: false,
                inspected: 0,
            })
        });
        let new_column = self.opens_new_column();
        self.columns.update(|c| c.entry(cwd).or_default().add(id, new_column));
    }

    /// Changes web tab `id`, if it is still open.
    pub fn update_web(&self, id: u64, f: impl FnOnce(&mut Web)) {
        self.webs.update(|w| {
            if let Some(web) = w.iter_mut().find(|w| w.id == id) {
                f(web);
            }
        });
    }

    pub fn web(&self, id: u64) -> Option<Web> {
        self.webs.with_untracked(|w| w.iter().find(|w| w.id == id).cloned())
    }

    /// Closes a tab (`closeTabAtom`): a terminal's shell is killed, a
    /// file's, web page's, the Changes or the Settings tab dropped.
    pub fn close_tab(&self, id: u64) {
        if let Some(live) = PTYS.with(|p| p.borrow_mut().remove(&id)) {
            live.pty.kill();
        }
        let Some(cwd) = self.cwd_of(id) else {
            return;
        };
        self.terms.update(|t| t.retain(|t| t.id != id));
        self.docs.update(|d| d.retain(|d| d.id != id));
        self.webs.update(|w| w.retain(|w| w.id != id));
        self.changes.update(|c| c.retain(|c| c.id != id));
        if self.settings_tab.with_untracked(|s| s.as_ref().is_some_and(|(s, _)| *s == id)) {
            self.settings_tab.set(None);
        }
        self.columns.update(|c| {
            if let Some(cols) = c.get_mut(&cwd) {
                cols.remove(id);
            }
        });
    }

    /// The workspace holding tab `id`.
    fn cwd_of(&self, id: u64) -> Option<String> {
        self.terms
            .with_untracked(|t| t.iter().find(|t| t.id == id).map(|t| t.cwd.clone()))
            .or_else(|| self.docs.with_untracked(|d| d.iter().find(|d| d.id == id).map(|d| d.cwd.clone())))
            .or_else(|| self.webs.with_untracked(|w| w.iter().find(|w| w.id == id).map(|w| w.cwd.clone())))
            .or_else(|| self.changes.with_untracked(|c| c.iter().find(|c| c.id == id).map(|c| c.cwd.clone())))
            .or_else(|| self.settings_tab.with_untracked(|s| s.as_ref().filter(|(s, _)| *s == id).map(|(_, cwd)| cwd.clone())))
    }

    /// Every tab of `cwd`'s columns.
    pub fn tabs_of(&self, cwd: &str) -> Vec<u64> {
        self.columns.with_untracked(|c| c.get(cwd).map(|c| c.groups.iter().flat_map(|g| g.tabs.clone()).collect()).unwrap_or_default())
    }

    /// The workspaces with terminals: the rows that draw a chip list
    /// (`chippedWorkspacesAtom`, which also counts web tabs and PRs).
    pub fn chipped_paths(&self) -> std::collections::HashSet<String> {
        self.terms.with(|t| t.iter().map(|t| t.cwd.clone()).collect())
    }

    /// What the collapse cycle reads: the groups, those with a session
    /// home, and whether any row draws chips.
    pub fn strip(&self) -> Strip {
        let chipped = self.chipped_paths();
        self.settings.with(|s| {
            let homes = s
                .groups
                .iter()
                .filter(|g| crate::workspace::group_home(s, g).is_some())
                .cloned()
                .collect();
            let any = s
                .pinned
                .iter()
                .chain(&s.tabs)
                .any(|t| (!t.hidden || t.home) && chipped.contains(&t.path));
            Strip { groups: s.groups.clone(), homes, chipped: any }
        })
    }

    /// ⌘0 (`togglePinnedCollapsedAtom`): with the strip hidden, shows it.
    pub fn cycle_collapse(&self) {
        let (strip, visible) = (self.strip(), self.sidebar_open.get_untracked());
        self.change_collapse(|c| c.cycle(&strip, visible));
        self.sidebar_open.set(true);
    }

    /// A collapse step the cycle's dots show (⌘0, a header click). A step
    /// that leaves the strip looking the same is skipped: `step` runs again
    /// (at most once per step there is) until something shows or hides.
    pub fn change_collapse(&self, step: impl Fn(&mut Collapse)) {
        let chipped = self.chipped_paths();
        let look = |c: &Collapse| self.settings.with(|s| crate::workspace::strip_look(s, c, |p| chipped.contains(p)));
        let mut c = self.collapse.get_untracked();
        let before = look(&c);
        step(&mut c);
        let first = c.clone();
        for _ in 0..3 {
            if look(&c) != before {
                break;
            }
            step(&mut c);
        }
        // No step changes anything: take the first rather than wherever
        // the loop stopped.
        self.collapse.set(if look(&c) != before { c } else { first });
        self.collapse_tick.update(|t| *t += 1);
    }

    /// The tab the active column of `cwd` shows.
    pub fn shown(&self, cwd: &str) -> Option<u64> {
        self.columns.with(|c| c.get(cwd).and_then(Columns::shown))
    }

    /// Shows tab `id` in its column, and makes that column active and its
    /// terminal take the keys.
    pub fn show(&self, id: u64) {
        let Some(cwd) = self.cwd_of(id) else {
            return;
        };
        self.columns.update(|c| {
            if let Some(cols) = c.get_mut(&cwd) {
                cols.show(id);
            }
        });
        self.focus_pending.set(true);
    }

    /// Makes column `group` of the current workspace active, its terminal
    /// taking the keys.
    pub fn focus_column(&self, group: u64) {
        self.focus_pending.set(true);
        let dir = self.directory_untracked();
        if self.columns.with_untracked(|c| c.get(&dir).map(|c| c.active_group)) == Some(group) {
            return;
        }
        self.columns.update(|c| {
            if let Some(cols) = c.get_mut(&dir) {
                cols.active_group = group;
            }
        });
    }

    /// A pane action on the current workspace's columns (`paneActionAtom`).
    pub fn pane_action(&self, action: PaneAction) {
        let dir = self.directory_untracked();
        MAXIMIZE.with(|m| {
            let mut m = m.borrow_mut();
            let mut snapshot = m.remove(&dir);
            self.columns.update(|c| {
                if let Some(cols) = c.get_mut(&dir) {
                    cols.apply(action, &mut snapshot);
                }
            });
            if let Some(s) = snapshot {
                m.insert(dir, s);
            }
        });
    }

    /// The columns' widths after a drag, as shares of the row.
    pub fn set_column_widths(&self, widths: &[f32]) {
        let total: f32 = widths.iter().sum();
        if total <= 0.0 {
            return;
        }
        let dir = self.directory_untracked();
        MAXIMIZE.with(|m| m.borrow_mut().remove(&dir));
        self.columns.update(|c| {
            if let Some(cols) = c.get_mut(&dir) {
                for (g, w) in cols.groups.iter_mut().zip(widths) {
                    g.width = w / total;
                }
            }
        });
    }

    pub fn directory_untracked(&self) -> String {
        self.settings.with_untracked(|s| s.directory.clone())
    }

    /// The terminals of `cwd`, oldest first.
    pub fn terms_of(&self, cwd: &str) -> Vec<Term> {
        self.terms.with(|t| t.iter().filter(|t| t.cwd == cwd).cloned().collect())
    }

    pub fn set_title(&self, id: u64, title: String) {
        self.terms.update(|t| {
            if let Some(t) = t.iter_mut().find(|t| t.id == id) {
                t.title = title;
            }
        });
    }
}

/// The mounted view's methods, if a view shows terminal `id`.
fn view_api(id: u64) -> Option<Rc<TerminalApi>> {
    PTYS.with(|p| p.borrow().get(&id).and_then(|l| l.view.map(|(_, v)| v)))?.api::<TerminalApi>()
}

/// Clears the scrollback (`ED 3`): what a program sends before drawing
/// everything again (omp after a resize: `ESC[2J ESC[3J`, then its whole
/// history).
const CLEAR_SCROLLBACK: &str = "\x1b[3J";

/// Frames a redraw is waited for once its output stops coming.
const REDRAW_QUIET_FRAMES: u32 = 2;

/// Frames a redraw is waited for at most.
const REDRAW_MAX_FRAMES: u32 = 15;

/// When `chunk` clears the scrollback, adds what follows it until the
/// output pauses, so the clear and the redraw reach the view as one
/// write: shown apart, the list sees its content shrink to a screen and
/// grow back, and loses its place at the end (scrolled to the top, or
/// part way). True when the shell exited meanwhile.
async fn gather_redraw(output: &mut UnboundedReceiver<Output>, chunk: &mut String) -> bool {
    if !chunk.contains(CLEAR_SCROLLBACK) {
        return false;
    }
    let (mut quiet, mut waited) = (0, 0);
    loop {
        while let Ok(more) = output.try_recv() {
            match more {
                Output::Data(more) => {
                    chunk.push_str(&more);
                    quiet = 0;
                }
                Output::Exit => return true,
            }
        }
        if quiet >= REDRAW_QUIET_FRAMES || waited >= REDRAW_MAX_FRAMES {
            return false;
        }
        next_frame().await;
        quiet += 1;
        waited += 1;
    }
}

/// What [`Live::pending`] keeps at most: the end of it.
const PENDING_MAX: usize = 1 << 20;

impl Live {
    /// Holds `chunk` for the first view, keeping [`PENDING_MAX`] of the
    /// end and the input modes of all of it.
    fn hold(&mut self, chunk: &str) {
        self.pending_modes.scan(chunk);
        let pending = &mut self.pending;
        pending.push_str(chunk);
        if pending.len() > PENDING_MAX {
            let cut = pending.len() - PENDING_MAX;
            let cut = (cut..pending.len()).find(|i| pending.is_char_boundary(*i)).unwrap_or(cut);
            pending.drain(..cut);
        }
    }
}

/// Shows output in the mounted view; with none mounted it goes straight
/// into the terminal's emulator, and the emulator's answers to the
/// program and its title change are handled here as the view would; with
/// no emulator yet it waits for the first view.
fn write_output(shell: Shell, id: u64, chunk: String) {
    if let Some(api) = view_api(id) {
        api.write(chunk);
        return;
    }
    let Some(emu) = PTYS.with(|p| {
        let mut p = p.borrow_mut();
        let l = p.get_mut(&id)?;
        if l.emulator.is_none() {
            l.hold(&chunk);
        }
        l.emulator.clone()
    }) else {
        return;
    };
    let (events, bracketed) = {
        let mut emu = emu.borrow_mut();
        emu.feed(chunk.as_bytes());
        (emu.take_events(), emu.modes().bracketed_paste)
    };
    set_bracketed(id, bracketed);
    for event in events {
        match event {
            TerminalEmulatorEvent::Reply(bytes) => pty_write(id, &String::from_utf8_lossy(&bytes)),
            TerminalEmulatorEvent::Title(title) => shell.set_title(id, title),
            _ => {}
        }
    }
}

/// Terminal `id`'s emulator, made by the first view to show it, which
/// takes the output held until then, in order: every view of the terminal
/// draws the same one.
pub fn emulator(id: u64, size: TerminalSize, scrollback: usize) -> Rc<RefCell<VtEmulator>> {
    PTYS.with(|p| {
        let mut p = p.borrow_mut();
        let Some(l) = p.get_mut(&id) else {
            return Rc::new(RefCell::new(VtEmulator::new(size, scrollback)));
        };
        if let Some(emu) = &l.emulator {
            return emu.clone();
        }
        let mut emu = VtEmulator::new(size, scrollback);
        emu.feed(l.pending_modes.replay().as_bytes());
        emu.feed(std::mem::take(&mut l.pending).as_bytes());
        l.bracketed = emu.modes().bracketed_paste;
        let emu = Rc::new(RefCell::new(emu));
        l.emulator = Some(emu.clone());
        emu
    })
}

pub fn pty_write(id: u64, data: &str) {
    PTYS.with(|p| {
        if let Some(l) = p.borrow_mut().get_mut(&id) {
            l.pty.write(data);
        }
    });
}

pub fn pty_resize(id: u64, cols: u32, rows: u32) {
    PTYS.with(|p| {
        if let Some(l) = p.borrow().get(&id) {
            l.pty.resize(cols, rows);
        }
    });
}

pub fn pty_pid(id: u64) -> Option<u32> {
    PTYS.with(|p| p.borrow().get(&id).and_then(|l| l.pty.pid()))
}

/// The terminal whose session id is `session_id`.
pub fn term_by_session(shell: Shell, session_id: &str) -> Option<Term> {
    shell.terms.with_untracked(|t| t.iter().find(|t| t.session_id == session_id).cloned())
}

/// As the program prints.
pub fn note_output(id: u64) {
    PTYS.with(|p| {
        if let Some(l) = p.borrow_mut().get_mut(&id) {
            l.last_output_at = unix_ms();
        }
    });
}

/// When terminal `id`'s program last printed (Unix ms).
pub fn last_output_at(id: u64) -> Option<f64> {
    PTYS.with(|p| p.borrow().get(&id).map(|l| l.last_output_at))
}

/// Called by a view once mounted: output goes to it. The token it gets
/// back clears it again, and only it ([`clear_view`]). The view is drawn
/// again: output from between its mount and now went into the emulator
/// behind its back.
pub fn set_view(id: u64, view: ComponentRef<Terminal>) -> u64 {
    let token = NEXT_VIEW.with(|n| {
        let t = n.get();
        n.set(t + 1);
        t
    });
    PTYS.with(|p| {
        if let Some(l) = p.borrow_mut().get_mut(&id) {
            l.view = Some((token, view));
        }
    });
    if let Some(api) = view.api::<TerminalApi>() {
        api.write(String::new());
    }
    token
}

/// Called by a view as it unmounts: output goes straight to the emulator
/// until another view mounts.
pub fn clear_view(id: u64, token: u64) {
    PTYS.with(|p| {
        if let Some(l) = p.borrow_mut().get_mut(&id) {
            if l.view.is_some_and(|(t, _)| t == token) {
                l.view = None;
            }
        }
    });
}

/// Every line of terminal `id`, scrollback and screen.
pub fn terminal_text(id: u64) -> Option<String> {
    let emu = PTYS.with(|p| p.borrow().get(&id).and_then(|l| l.emulator.clone()))?;
    let emu = emu.borrow();
    Some((0..emu.line_count()).map(|i| emu.line(i).text().trim_end().to_owned()).collect::<Vec<_>>().join("\n"))
}

/// Called by the terminal's emulator as the program sets or clears mode 2004.
pub fn set_bracketed(id: u64, on: bool) {
    PTYS.with(|p| {
        if let Some(l) = p.borrow_mut().get_mut(&id) {
            l.bracketed = on;
        }
    });
}

pub fn bracketed(id: u64) -> bool {
    PTYS.with(|p| p.borrow().get(&id).is_some_and(|l| l.bracketed))
}

/// Pastes `text` as xterm's `paste` does: bracketed when the program asked
/// for it, so a multi-line text lands in an editor instead of submitting.
pub fn paste(id: u64, text: &str) {
    let text = text.replace("\r\n", "\r").replace('\n', "\r");
    if bracketed(id) {
        pty_write(id, &format!("\x1b[200~{text}\x1b[201~"));
    } else {
        pty_write(id, &text);
    }
}

/// Gives terminal `id` the keys, once its view has mounted.
pub fn focus_terminal(id: u64) {
    if let Some(api) = view_api(id) {
        api.focus();
    }
}
