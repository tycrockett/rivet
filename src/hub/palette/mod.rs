//! The palette (`components/BoltCompose/BoltPalette.tsx`): ⌘; opens a
//! frame over the terminals with the thread and the input; the line typed
//! is read by [`Env::plan`], which says what sending it will do.

mod cards;
mod editor;
mod help;

use std::{cell::RefCell, collections::HashMap, future::Future, rc::Rc};

use nsh_core::prelude::*;
use std::time::Duration;

use self::{
    cards::{FG, FINDER_TONE, chip, faint, mix, muted},
    editor::{Editor, Item, editor},
};
use crate::{
    commands::{self, REGISTRY},
    hub::{
        ComposeSelf, Hub, HubEntry, HubTarget, JobStatus, ME, PaletteOpen, engine, finder_matches, hub, next_id, parse_post,
        targets as hub_targets,
    },
    state::Shell,
};

/// How many of the thread's last entries show (`SHOWN`).
const SHOWN: usize = 60;

/// The palette's corners, its panels' and its cards': the terminal box's.
pub(super) const RADIUS: f32 = 12.0;

const FINDER_HELP: &str = "finder · folder name or cd <path> · .. · ~ · mkdir <name> · tab [<folder>] · bm <name> · bm add|rm <name> · exit";

/// Runs `f` under no owner, so closing the palette does not end it.
fn spawn_detached(f: impl Future<Output = ()> + 'static) {
    Owner::detached().run(|| {
        spawn(f);
    });
}

/// The palette while open; `PaletteOpen::n` remakes it.
pub fn palette() -> impl IntoElement {
    let hub = hub();
    dynamic(move || {
        let open = hub.palette.get();
        untracked(|| open.map(|o| frame(hub, o)).into_iter().collect::<Vec<AnyElement>>())
    })
}

/// What the typed line does when sent (`plan`).
struct Plan {
    say: String,
    send: Option<Rc<dyn Fn()>>,
    echo: bool,
    keyword: Option<String>,
    note: Option<String>,
}

impl Plan {
    fn says(say: impl Into<String>) -> Plan {
        Plan { say: say.into(), send: None, echo: false, keyword: None, note: None }
    }

    fn sends(say: impl Into<String>, send: impl Fn() + 'static) -> Plan {
        Plan { say: say.into(), send: Some(Rc::new(send)), echo: false, keyword: None, note: None }
    }

    fn echoed(mut self) -> Plan {
        self.echo = true;
        self
    }
}

struct Env {
    hub: Hub,
    shell: Shell,
    self_: Option<ComposeSelf>,
    text: Signal<String>,
    folders: Signal<Vec<String>>,
    targets: Memo<Vec<HubTarget>>,
    handles: Memo<Vec<String>>,
    self_path: Memo<Option<String>>,
    registry: Vec<String>,
    /// The echoed result card the run in flight settles (`pending`).
    pending: RefCell<Option<String>>,
    input_ref: ElementRef,
}

fn json(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_default()
}

/// A line whose command navigates: it keeps the palette open (`NAV`).
fn is_nav(cmd: &str) -> bool {
    matches!(cmd.split_whitespace().next().unwrap_or("").to_lowercase().as_str(), "cd" | ".." | "mkdir")
}

/// `^[/@#:]`.
fn starts_prefixed(s: &str) -> bool {
    s.starts_with(['/', '@', '#', ':'])
}

impl Env {
    fn open_finder(&self) -> Option<(String, u32)> {
        self.hub.thread.with(|t| {
            t.iter().rev().find_map(|e| match e {
                HubEntry::Finder { id, n, done: false } => Some((id.clone(), *n)),
                _ => None,
            })
        })
    }

    /// What the finder filters its folders by: the last word of the line,
    /// unless it is a command or a path (`finderQuery`).
    fn finder_query(&self, text: &str) -> String {
        if self.open_finder().is_none() || starts_prefixed(text) {
            return String::new();
        }
        let head = text.trim_start();
        let first = head.split_whitespace().next().unwrap_or("");
        if matches!(first, "mkdir" | "bm") && (head.len() == first.len() || head[first.len()..].starts_with(char::is_whitespace)) {
            return String::new();
        }
        if head.contains(['/', '~']) {
            return String::new();
        }
        for verb in ["cd", "tab"] {
            if let Some(rest) = head.strip_prefix(verb) {
                if rest.starts_with(char::is_whitespace) {
                    return rest.trim_start().to_owned();
                }
            }
        }
        head.to_owned()
    }

    /// Tab in the finder completes the folder being typed (`completeFolder`).
    fn complete_folder(&self) -> bool {
        if self.open_finder().is_none() {
            return false;
        }
        let text = self.text.get_untracked();
        let query = self.finder_query(&text);
        if query.is_empty() {
            return true;
        }
        let folders = self.folders.get_untracked();
        let q = query.to_lowercase();
        let starts: Vec<&String> = folders.iter().filter(|f| f.to_lowercase().starts_with(&q)).collect();
        let pool: Vec<String> = if starts.is_empty() {
            finder_matches(&folders, &query)
        } else {
            starts.iter().map(|s| (*s).clone()).collect()
        };
        if pool.is_empty() {
            return true;
        }
        let mut done = pool[0].clone();
        if pool.len() > 1 {
            if starts.is_empty() {
                return true;
            }
            let first: Vec<char> = done.chars().collect();
            let mut n = 0;
            while n < first.len() && pool.iter().all(|f| f.chars().nth(n).is_some_and(|c| c.to_lowercase().eq(first[n].to_lowercase()))) {
                n += 1;
            }
            done = first[..n].iter().collect();
        }
        if done == query || done.chars().count() < query.chars().count() {
            return true;
        }
        let keep = text.len() - query.len();
        self.text.set(format!("{}{}", &text[..keep], done));
        true
    }

    /// Runs a line for the palette: the command, settled onto the card the
    /// echo posted (`runOrClose`, with the run's results).
    fn run_or_close(self: &Rc<Self>, cmd: String) {
        let nav = is_nav(&cmd);
        let id = self.pending.borrow_mut().take();
        let hub = self.hub;
        let shell = self.shell;
        let non_result = commands::is_non_result(&cmd);
        if non_result && !nav {
            hub.palette.set(None);
        }
        spawn_detached(async move {
            let outputs = if nav { hub.navigate_in_palette(commands::run(shell, cmd, None)).await } else { commands::run(shell, cmd, None).await };
            if let Some(id) = id {
                hub.settle_result(&id, if non_result { Vec::new() } else { outputs });
            }
        });
    }

    /// Runs lines in turn without closing the palette (`navigateInPalette`),
    /// then `after`.
    fn navigate(&self, cmds: Vec<String>, after: Option<Rc<dyn Fn()>>) {
        let (hub, shell) = (self.hub, self.shell);
        spawn_detached(async move {
            hub.navigate_in_palette(async {
                for c in cmds {
                    commands::run(shell, c, None).await;
                }
            })
            .await;
            if let Some(after) = after {
                after();
            }
        });
    }

    fn bump_finder(hub: Hub, id: String) -> Rc<dyn Fn()> {
        Rc::new(move || {
            hub.patch(&id, |e| {
                if let HubEntry::Finder { n, .. } = e {
                    *n += 1;
                }
            })
        })
    }

    /// The finder's lines (`plan`'s finder branch).
    fn finder_plan(self: &Rc<Self>, t: &str, id: &str) -> Plan {
        let env = self.clone();
        let bump = Env::bump_finder(self.hub, id.to_owned());
        let nav = {
            let env = env.clone();
            move |cmds: Vec<String>| {
                let (env, bump) = (env.clone(), bump.clone());
                move || env.navigate(cmds.clone(), Some(bump.clone()))
            }
        };
        let home = self.shell.settings.with_untracked(|s| s.home.clone());
        if t.is_empty() {
            return Plan::says(FINDER_HELP);
        }
        if t == "exit" || t == "q" {
            let hub = self.hub;
            return Plan::sends("closes the finder", move || hub.close_finders());
        }
        if t == ".." {
            return Plan::sends("goes up a folder", nav(vec!["..".into()]));
        }
        if t == "~" {
            return Plan::sends("goes home", nav(vec!["cd".into()]));
        }
        if let Some(name) = t.strip_prefix("mkdir").filter(|r| r.starts_with(char::is_whitespace)).map(str::trim_start).filter(|n| !n.is_empty()) {
            return Plan::sends(format!("makes {name}"), nav(vec![format!("mkdir {}", json(name))]));
        }
        if let Some(rest) = t.strip_prefix("tab").filter(|r| r.is_empty() || r.starts_with(char::is_whitespace)) {
            let arg = rest.trim_start();
            let reserved = arg.starts_with('-') || ["add", "rm", "group", "ungroup"].iter().any(|w| arg.strip_prefix(w).is_some_and(|r| r.is_empty() || r.starts_with(char::is_whitespace)));
            if !arg.is_empty() && !reserved {
                return Plan::sends(format!("adds {arg} as a tab"), nav(vec![format!("cd {}", json(arg)), "tab".into()]));
            }
            return Plan::sends("adds this folder as a tab", nav(vec![t.to_owned()]));
        }
        if let Some(rest) = t.strip_prefix("bm").filter(|r| r.is_empty() || r.starts_with(char::is_whitespace)) {
            let rest = rest.trim_start();
            let (sub, name) = match rest.split_once(char::is_whitespace) {
                Some((w, n)) if matches!(w, "add" | "rm" | "go") => (Some(w), n.trim_start()),
                _ if matches!(rest, "add" | "rm" | "go") => (Some(rest), ""),
                _ => (None, rest),
            };
            if name.is_empty() {
                return Plan::says("bm <name> · bm add <name> · bm rm <name>");
            }
            if let Some(sub @ ("add" | "rm")) = sub {
                let say = format!("{} {name}", if sub == "add" { "bookmarks this folder as" } else { "removes bookmark" });
                return Plan::sends(say, nav(vec![format!("bm {sub} {}", json(name))]));
            }
            let path = self.shell.settings.with_untracked(|s| s.bookmarks.get(name).cloned());
            let Some(path) = path else {
                return Plan::says(format!("no bookmark {name}"));
            };
            return Plan::sends(format!("goes to {path}"), nav(vec![format!("cd {} --absolute", json(&path))]));
        }
        let to = t.strip_prefix("cd").filter(|r| r.starts_with(char::is_whitespace)).map_or(t, str::trim_start);
        if to == "cd" {
            return Plan::sends("goes home", nav(vec!["cd".into()]));
        }
        if let (Some(rest), false) = (to.strip_prefix("~/"), home.is_empty()) {
            return Plan::sends(format!("goes to {to}"), nav(vec![format!("cd {} --absolute", json(&format!("{home}/{rest}")))]));
        }
        if to.starts_with('/') {
            return Plan::sends(format!("goes to {to}"), nav(vec![format!("cd {} --absolute", json(to))]));
        }
        Plan::sends(format!("goes to {to}"), nav(vec![format!("cd {}", json(to))]))
    }

    /// Reads the typed line (`plan`): what it says it does, and what
    /// sending it does.
    fn plan(self: &Rc<Self>, text: &str) -> Plan {
        let t = text.trim();
        let hub = self.hub;
        let open_choice = hub.thread.with(|th| {
            th.iter().rev().find_map(|e| match e {
                HubEntry::Choice { id, title, options, done: false, .. } => Some((id.clone(), title.clone(), options.clone())),
                _ => None,
            })
        });
        if let Some((id, title, options)) = open_choice {
            if let Some(o) = options.iter().find(|o| o.key.to_lowercase() == t.to_lowercase()) {
                let key = o.key.clone();
                return Plan::sends(format!("picks {}", o.label), move || hub.pick_choice(&id, key.clone()));
            }
            if t.is_empty() {
                let keys: Vec<&str> = options.iter().map(|o| o.key.as_str()).collect();
                return Plan::says(format!("{title} · type {}", keys.join(" / ")));
            }
        }
        if let (Some((fid, _)), false) = (self.open_finder(), starts_prefixed(t)) {
            return self.finder_plan(t, &fid);
        }
        if t.is_empty() {
            return Plan::says("/command · @handle message · #script · :keyword · shell");
        }
        let self_ = self.self_.clone();
        if let Some(rest) = t.strip_prefix(':') {
            let line = match &self_ {
                Some(s) => format!("@{} {rest}", s.handle),
                None => rest.to_owned(),
            };
            let commands = hub.commands.get();
            let handles = self.handles.get();
            let Some(m) = engine::match_command(&line, &commands, &handles) else {
                return Plan::says(format!("no Bolt command :{}", rest.split_whitespace().next().unwrap_or("")));
            };
            let slots: Vec<String> = m
                .command
                .targets
                .iter()
                .map(|s| format!("{s}={}", m.bound.get(s).map_or("?".to_owned(), |h| format!("@{h}"))))
                .collect();
            let rest = rest.to_owned();
            let mut plan = Plan::sends(format!("runs {} · {}", m.command.keyword, slots.join(" ")), move || match &self_ {
                Some(s) => engine::compose_send(hub, rest.clone(), s.clone()),
                None => engine::submit_bolt(hub, rest.clone()),
            })
            .echoed();
            plan.keyword = Some(m.command.keyword.clone());
            return plan;
        }
        if engine::is_script(t) {
            let (nodes, errors) = engine::check_script(t);
            if let Some(e) = errors.first() {
                return Plan::says(format!("line {}: {}", e.line, e.message));
            }
            let tag = self.this_tag();
            let say = format!("script · {nodes} node{}{}", if nodes == 1 { "" } else { "s" }, if tag.is_empty() { String::new() } else { format!(" · {tag}") });
            let t = t.to_owned();
            return Plan::sends(say, move || match &self_ {
                Some(s) => engine::compose_send(hub, t.clone(), s.clone()),
                None => engine::submit_bolt(hub, t.clone()),
            });
        }
        if t == "@bolt" || t.strip_prefix("@bolt").is_some_and(|r| r.starts_with(char::is_whitespace)) {
            let ask = t["@bolt".len()..].trim().to_owned();
            if ask.is_empty() {
                return Plan::says("say what @bolt should do");
            }
            let handles = self.handles.get();
            let to = parse_post(&ask, &handles).to;
            let forks = to.as_ref().and_then(|to| {
                self.targets.with(|ts| ts.iter().find(|t| &t.handle == to).and_then(|t| t.members.first().and_then(|m| m.session_id.clone())))
            });
            let say = match (&forks, &to) {
                (Some(_), Some(to)) => format!("forks @{to}'s claude to do it"),
                (None, Some(to)) => format!("asks a new claude in @{to} to do it"),
                _ => "asks a new claude to do it".to_owned(),
            };
            return Plan::sends(say, move || engine::chat_bolt(hub, ask.clone())).echoed();
        }
        if t.starts_with('@') {
            let handles = self.handles.get();
            let post = parse_post(t, &handles);
            if post.to.is_none() {
                return Plan::says("address it with @handle");
            }
            let tos: Vec<String> = post.tos.iter().map(|h| format!("@{h}")).collect();
            let t = t.to_owned();
            return Plan::sends(format!("to {}", tos.join(", ")), move || engine::submit_bolt(hub, t.clone()));
        }
        if let Some(rest) = t.strip_prefix('/') {
            let word = rest.split_whitespace().next().unwrap_or("");
            if word.is_empty() {
                return Plan::says("name a command");
            }
            // Running jobs stay: their Cancel and Continue live on the card.
            if rest == "clear" {
                let env = self.clone();
                return Plan::sends("clears the thread", move || {
                    env.hub.thread.update(|xs| xs.retain(|e| matches!(e, HubEntry::Job { status: JobStatus::Running, .. })));
                    let (shell, hub) = (env.shell, env.hub);
                    let _ = hub;
                    spawn_detached(async move {
                        commands::run(shell, "clear".into(), None).await;
                    });
                });
            }
            if let Some(arg) = rest.strip_prefix("finder").filter(|r| r.is_empty() || r.starts_with(char::is_whitespace)) {
                let env = self.clone();
                let open = move || {
                    env.hub.close_finders();
                    env.hub.post(HubEntry::Finder { id: next_id(), n: 0, done: false });
                };
                let want = arg.trim();
                if want.is_empty() {
                    return Plan::sends("opens the finder", open);
                }
                let marks = self.shell.settings.with_untracked(|s| s.bookmarks.clone());
                let names: Vec<String> = {
                    let mut n: Vec<String> = marks.keys().cloned().collect();
                    n.sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()).then(a.cmp(b)));
                    n
                };
                let name = if names.iter().any(|n| n == want) { Some(want.to_owned()) } else { finder_matches(&names, want).into_iter().next() };
                let Some(name) = name else {
                    return Plan::says(format!("no bookmark {want}"));
                };
                let path = marks[&name].clone();
                let env = self.clone();
                return Plan::sends(format!("opens the finder at {name} · {path}"), move || {
                    open();
                    env.navigate(vec![format!("cd {} --absolute", json(&path))], None);
                });
            }
            let env = self.clone();
            if self.registry.iter().any(|r| r == word) {
                let rest = rest.to_owned();
                return Plan::sends(format!("runs {word}"), move || env.run_or_close(rest.clone())).echoed();
            }
            let custom = self.shell.settings.with_untracked(|s| s.commands.contains_key(&format!("/{word}")));
            if custom {
                let t = t.to_owned();
                return Plan::sends(format!("runs /{word}"), move || env.run_or_close(t.clone())).echoed();
            }
            return Plan::says(format!("no command /{word}"));
        }
        let env = self.clone();
        let line = format!(":{t}");
        let mut plan = Plan::sends("types into a terminal", move || env.run_or_close(line.clone())).echoed();
        plan.note = Some("sent to the terminal".into());
        plan
    }

    /// `{this}=@handle path`.
    fn this_tag(&self) -> String {
        match &self.self_ {
            None => String::new(),
            Some(s) => match self.self_path.get() {
                Some(p) => format!("{{this}}=@{} {p}", s.handle),
                None => format!("{{this}}=@{}", s.handle),
            },
        }
    }

    /// Sends the typed line: the echo, then what the plan does.
    fn send(self: &Rc<Self>) {
        let text = self.text.get_untracked();
        let plan = self.plan(&text);
        let Some(send) = plan.send else {
            return;
        };
        if plan.echo {
            let t = text.trim().to_owned();
            let id = next_id();
            let to = self.self_.as_ref().filter(|_| !t.starts_with('@')).map(|s| s.handle.clone());
            self.hub.post(HubEntry::Say { id: next_id(), from: ME.into(), text: t.clone(), error: false, to });
            self.hub.post(HubEntry::Result { id: id.clone(), cmd: t, items: Vec::new(), done: false, note: plan.note });
            *self.pending.borrow_mut() = Some(id);
        }
        send();
        // A send that closes the palette disposes its signals with it.
        if self.text.try_set(String::new()) {
            self.input_ref.focus();
        }
    }
}

/// The frame's colors: the current tab's `color`, else zinc.
fn frame_color(shell: Shell) -> u32 {
    shell.settings.with_untracked(|s| {
        s.tabs
            .iter()
            .chain(&s.pinned)
            .find(|t| t.path == s.directory)
            .and_then(|t| t.color.as_deref())
            .and_then(|c| c.strip_prefix('#'))
            .filter(|c| c.len() == 6)
            .and_then(|c| u32::from_str_radix(c, 16).ok())
            .unwrap_or(0x3f3f46)
    })
}

/// The slash menu: the registry then custom commands, the order `/`
/// resolves in (`menu`).
fn slash_menu(shell: Shell) -> (Vec<String>, Vec<Item>) {
    let registry: Vec<String> = REGISTRY.iter().map(|c| c.name.to_owned()).chain(["git".to_owned()]).collect();
    let mut seen: Vec<String> = Vec::new();
    let mut out: Vec<Item> = Vec::new();
    let mut add = |keyword: String, hint: String| {
        if keyword.is_empty() || seen.contains(&keyword) {
            return;
        }
        seen.push(keyword.clone());
        out.push(Item { keyword, hint });
    };
    for name in &registry {
        add(name.clone(), help::description(name).to_owned());
    }
    add("finder".into(), "Browse folders, change directory, add tabs · /finder <bookmark> opens there".into());
    for (k, v) in shell.settings.with_untracked(|s| s.commands.clone()) {
        add(k.trim_start_matches('/').to_owned(), v);
    }
    (registry, out)
}

fn frame(hub: Hub, open: PaletteOpen) -> AnyElement {
    let shell = hub.shell;
    let self_ = open.self_;
    let typed = signal(String::new());
    let folders = signal(Vec::<String>::new());
    let input_ref = element_ref();
    let targets = computed(move || hub_targets(shell));
    let handles = computed(move || targets.with(|ts| ts.iter().map(|t| t.handle.clone()).collect::<Vec<_>>()));
    let handle_of = computed(move || {
        targets.with(|ts| {
            let mut m = HashMap::new();
            for t in ts {
                for mem in &t.members {
                    if let Some(s) = &mem.session_id {
                        m.insert(s.clone(), t.handle.clone());
                    }
                }
            }
            m
        })
    });
    let self_path = {
        let self_ = self_.clone();
        computed(move || {
            let s = self_.as_ref()?;
            let cwd = targets.with(|ts| {
                let t = ts.iter().find(|t| t.handle == s.handle)?;
                t.members
                    .iter()
                    .find(|m| m.session_id.is_some() && m.session_id == s.session_id)
                    .map(|m| m.cwd.clone())
                    .or_else(|| t.path.clone())
                    .or_else(|| t.members.first().map(|m| m.cwd.clone()))
            })?;
            let home = shell.settings.with(|s| s.home.clone());
            Some(if !home.is_empty() && cwd.starts_with(&home) { format!("~{}", &cwd[home.len()..]) } else { cwd })
        })
    };
    let (registry, menu) = slash_menu(shell);
    let env = Rc::new(Env { hub, shell, self_: self_.clone(), text: typed, folders, targets, handles, self_path, registry, pending: RefCell::new(None), input_ref });

    let scripts = computed(move || {
        hub.commands.with(|cs| {
            cs.iter()
                .map(|c| {
                    let slots: Vec<String> = c.targets.iter().skip(1).map(|s| format!("@{s}")).collect();
                    let hint = [c.name.clone(), slots.join(" ")].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · ");
                    Item { keyword: c.keyword.clone(), hint }
                })
                .collect::<Vec<_>>()
        })
    });

    // The workspace's files for `/open`'s menu: listed off the engine
    // thread the first time the line asks, once per opening, so files made
    // since the last opening are there.
    let files = signal(Rc::new(Vec::<String>::new()));
    let listing = std::cell::Cell::new(false);
    effect(move || {
        if listing.get() || !typed.with(|t| t.starts_with("/open ")) {
            return;
        }
        listing.set(true);
        let dir = shell.settings.with_untracked(|s| s.directory.clone());
        spawn(async move {
            let listed = crate::git::off_thread(move || crate::files::list(&dir)).await;
            files.set(Rc::new(listed));
        });
    });

    // What the finder filters by follows the line; it is cleared on close.
    let query = computed({
        let env = env.clone();
        move || {
            let t = typed.get();
            let q = env.finder_query(&t);
            if q == ".." { String::new() } else { q }
        }
    });
    effect(move || hub.finder_query.set(query.get()));
    cleanup(move || hub.finder_query.set(String::new()));

    // The plan, as the line says it: re-read on every keystroke and on
    // anything the thread opens or closes.
    let said = computed({
        let env = env.clone();
        move || {
            let t = typed.get();
            let plan = env.plan(&t);
            let say = {
                let tag = env.this_tag();
                if !tag.is_empty() && t.contains("{this}") && !plan.say.contains("{this}") { format!("{} · {tag}", plan.say) } else { plan.say.clone() }
            };
            (say, plan.send.is_some(), plan.keyword)
        }
    });
    // `^\s*:?keyword(?=\s|$)`: the keyword the plan found, as the line starts with it.
    let lead = computed(move || {
        let (_, _, keyword) = said.get();
        let Some(kw) = keyword else {
            return 0;
        };
        let t = typed.get();
        let head = t.trim_start();
        let skip = t.len() - head.len();
        let body = head.strip_prefix(':').unwrap_or(head);
        let colon = head.len() - body.len();
        match body.strip_prefix(kw.as_str()) {
            Some(rest) if rest.is_empty() || rest.starts_with(char::is_whitespace) => skip + colon + kw.len(),
            _ => 0,
        }
    });

    let now = signal(unix_now());
    let running = computed(move || hub.thread.with(|t| t.iter().any(|e| matches!(e, HubEntry::Job { status: JobStatus::Running, .. }))));
    effect(move || {
        if !running.get() {
            return;
        }
        let ticker = spawn(async move {
            loop {
                timer(Duration::from_secs(1)).await;
                now.set(unix_now());
            }
        });
        cleanup(move || ticker.cancel());
    });

    // The thread's last entries.
    let shown = move || hub.thread.with(|t| t[t.len().saturating_sub(SHOWN)..].to_vec());
    let empty = move || hub.thread.with(Vec::is_empty);

    // The user's latest line or finder sits at the top of the log: its
    // position is learned from its layout. Until the log has scrolled there
    // on opening it stays hidden: its first frame would show it scrolled
    // elsewhere, and the rows would seem to blink in.
    let log_ref = element_ref();
    let positions = signal(HashMap::<String, f32>::new());
    let viewport = signal(0.0_f32);
    let wanted = signal(None::<(String, bool)>);
    let placed = signal(false);
    // Shown regardless after a moment, should the row never lay out.
    let reveal = set_timeout(Duration::from_millis(250), move || placed.set(true));
    cleanup(move || reveal.cancel());
    let pinned: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    effect(move || {
        let mine = hub.thread.with(|t| {
            t.iter().rev().find_map(|e| match e {
                HubEntry::Say { id, from, .. } if from == ME => Some(id.clone()),
                HubEntry::Finder { id, .. } => Some(id.clone()),
                _ => None,
            })
        });
        let Some(mine) = mine else {
            untracked(|| placed.set(true));
            return;
        };
        let mut p = pinned.borrow_mut();
        if p.as_deref() == Some(mine.as_str()) {
            return;
        }
        let opening = p.is_none();
        *p = Some(mine.clone());
        untracked(|| wanted.set(Some((mine, !opening))));
    });
    effect(move || {
        let Some((id, animated)) = wanted.get() else {
            return;
        };
        let Some(y) = positions.with(|p| p.get(&id).copied()) else {
            return;
        };
        log_ref.scroll_to(point(px(0.0), px((y - 16.0).max(0.0))), animated);
        untracked(|| {
            wanted.set(None);
            placed.set(true);
        });
    });

    let env_row = env.clone();
    let rows = each(shown, |e| e.id().to_owned(), move |entry, _| {
        let id = entry.get_untracked().id().to_owned();
        let env = env_row.clone();
        let kind = move || entry.get();
        view()
            .on_layout(move |b: &Bounds<Points>| {
                let y = b.origin.y.0;
                positions.update(|p| {
                    p.insert(id.clone(), y);
                });
            })
            .child(dynamic(move || {
                let e = kind();
                let env = env.clone();
                untracked(|| row(&env, handle_of, now, folders, &e))
            }))
    });

    let color = frame_color(shell);
    let (from, to) = (mix(color, 0x000000, 0.45), mix(color, 0x000000, 0.80));
    let close = move || hub.palette.set(None);
    let can_send = move || said.get().1;
    // While a finder is open the line goes to it: the header says so, and
    // the input is framed in the finder's color.
    let in_finder = {
        let env = env.clone();
        computed(move || env.open_finder().is_some())
    };
    let placeholder = computed(move || {
        if in_finder.get() { "folder name, cd <path>, .., ~, mkdir, tab, bm · exit leaves the finder" } else { "/command, @handle message, #script, :keyword, or a shell command" }
    });

    let send_env = env.clone();
    let enter_env = env.clone();
    let tab_env = env.clone();
    let header_tag = self_.as_ref().map(|s| format!("@{}", s.handle)).unwrap_or_else(|| "Bolt".to_owned());
    view()
        .absolute()
        .inset(px(8.0))
        .flex_col()
        .p_3()
        .rounded(px(RADIUS))
        .shadow_2xl()
        .bg_linear(deg(135.0))
        .gradient_from(from)
        .gradient_to(to)
        // Nothing drawn past the frame, however narrow the strip is dragged.
        .overflow_hidden()
        .child(
            // The tag and the close button keep their size; the path between
            // them is cut to fit (`truncate`).
            view()
                .flex_row()
                .items_center()
                .gap_2()
                .px_2()
                .pb_2()
                .min_w(px(0.0))
                .child(
                    text(header_tag)
                        .uppercase()
                        .flex_shrink(0.0)
                        .number_of_lines(1)
                        .text_size(px(10.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgba(0xffffffb3)),
                )
                .child(when(move || in_finder.get(), || chip("▸", "finder", rgb(FINDER_TONE)).flex_shrink(0.0)))
                .child(
                    text(move || self_path.get().unwrap_or_default())
                        .flex_1()
                        .min_w(px(0.0))
                        .number_of_lines(1)
                        .text_size(px(10.0))
                        .text_color(faint()),
                )
                .child(
                    view()
                        .flex_shrink(0.0)
                        .px_2()
                        .py(px(2.0))
                        .rounded_full()
                        .hover(|s| s.bg(rgba(0xffffff33)))
                        .on_press(close)
                        .child(text("×").text_size(px(14.0)).font_weight(FontWeight::BOLD).text_color(rgb(FG))),
                ),
        )
        .child(
            // `var(--theme-bg)`: the editor background, as Bolt's log has it.
            view().flex_1().min_h(px(0.0)).rounded(px(RADIUS)).overflow_hidden().bg(rgb(crate::theme::BOLT_THEME.vars.bg)).child(
                scroll()
                    .element_ref(&log_ref)
                    .flex_1()
                    .opacity(move || if placed.get() { 1.0_f32 } else { 0.0 })
                    .on_layout(move |b: &Bounds<Points>| viewport.set(b.size.height.0))
                    .child(
                        view()
                            .w_full()
                            .max_w(px(720.0))
                            .self_center()
                            .px_4()
                            .pt_4()
                            .pb(move || px(viewport.get()))
                            .gap_3()
                            .child(when(empty, || {
                                text("/ runs a command or a Bolt command, @handle posts to a terminal, # runs a script, : types into a terminal.")
                                    .text_size(px(12.0))
                                    .line_height(px(20.0))
                                    .text_color(rgba(0xffffffb3))
                            }))
                            .child(rows),
                    ),
            ),
        )
        .child(
            view()
                .mt_2()
                .gap_2()
                .rounded(px(RADIUS))
                .px_2()
                .pt_2()
                .pb(px(10.0))
                .bg(move || if in_finder.get() { mix(FINDER_TONE, 0x000000, 0.25).opacity(0.6) } else { rgba(0x00000059) })
                .border_1()
                .border_color(move || if in_finder.get() { rgb(FINDER_TONE).opacity(0.7) } else { rgba(0x00000000) })
                .child(editor(Editor {
                    value: typed,
                    commands: menu,
                    scripts,
                    targets,
                    files,
                    on_enter: Rc::new(move || enter_env.send()),
                    on_tab: Rc::new(move || tab_env.complete_folder()),
                    close: Rc::new(close),
                    input_ref,
                    lead,
                    placeholder,
                }))
                .child(
                    view()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .child(
                            text(move || said.get().0)
                                .flex_1()
                                .min_w(px(0.0))
                                .number_of_lines(1)
                                .text_size(px(11.0))
                                .text_color(move || {
                                    if said.get().1 || typed.get().trim().is_empty() { rgba(0xffffffb3) } else { rgb(0xfecdd3) }
                                }),
                        )
                        .child(
                            view()
                                .w(px(32.0))
                                .h(px(32.0))
                                .items_center()
                                .justify_center()
                                .rounded_full()
                                .bg(rgb(FG))
                                .opacity(move || if can_send() { 1.0_f32 } else { 0.4_f32 })
                                .on_press(move || {
                                    if can_send() {
                                        send_env.send();
                                    }
                                })
                                .child(text("↑").text_size(px(16.0)).font_weight(FontWeight::BOLD).text_color(rgb(color))),
                        ),
                ),
        )
        .into_element()
}

fn unix_now() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_millis() as f64)
}

/// One entry of the thread as its card.
fn row(env: &Rc<Env>, handle_of: Memo<HashMap<String, String>>, now: Signal<f64>, folders: Signal<Vec<String>>, e: &HubEntry) -> AnyElement {
    let hub = env.hub;
    match e {
        HubEntry::Job { .. } => cards::job_card(hub, e, now),
        HubEntry::Result { .. } => {
            let shell = env.shell;
            let run: Rc<dyn Fn(String)> = Rc::new(move |cmd| {
                spawn_detached(async move {
                    commands::run(shell, cmd, None).await;
                })
            });
            cards::result_card(shell, e, run)
        }
        HubEntry::Choice { .. } => cards::choice_card(hub, e),
        HubEntry::Finder { .. } => {
            let env = env.clone();
            let go: Rc<dyn Fn(Vec<String>)> = Rc::new(move |cmds| env.navigate(cmds, None));
            cards::finder_card(hub.shell, hub, e, folders, go)
        }
        HubEntry::Spawn { handle, cwd, session_id, .. } => {
            let (cwd, session) = (cwd.clone(), session_id.clone());
            view()
                .flex_row()
                .child(
                    view()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .min_w(px(0.0))
                        .px_3()
                        .py_1()
                        .rounded_full()
                        .bg(rgba(0xffffff1f))
                        .hover(|s| s.bg(rgba(0xffffff33)))
                        .on_press(move || engine::reveal_member(hub, &cwd, Some(&session)))
                        .child(text("⌨").flex_shrink(0.0).text_size(px(11.0)).text_color(muted()))
                        .child(
                            text("started a terminal in ")
                                .min_w(px(0.0))
                                .number_of_lines(1)
                                .text_size(px(11.0))
                                .text_color(muted())
                                .child(text(format!("@{handle}")).font_weight(FontWeight::MEDIUM).text_color(rgb(FG)))
                                .child(" — open it"),
                        ),
                )
                .into_element()
        }
        HubEntry::Say { from, .. } | HubEntry::Ask { from, .. } => {
            let handle = handle_of.with(|m| m.get(from).cloned());
            cards::post_card(e, handle)
        }
    }
}
