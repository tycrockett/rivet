//! The finder's lines: what it filters by, its Tab completion, and what
//! each line does.

use std::rc::Rc;

use nsh_core::prelude::*;

use super::{Plan, json};
use crate::{hub::finder_matches, state::Shell};

/// What the finder's lines do, as its empty line says.
pub(super) const HELP: &str = "folder name or cd <path> · .. · ~ · mkdir <name> · tab [<folder>] · bm <name> · bm add|rm <name>";

/// What the finder filters its folders by: the last word of the line,
/// unless it is a command or a path (`finderQuery`).
pub(super) fn query(text: &str) -> String {
    let head = text.trim_start();
    let first = head.split_whitespace().next().unwrap_or("");
    if matches!(first, "mkdir" | "bm") && (head.len() == first.len() || head[first.len()..].starts_with(char::is_whitespace)) {
        return String::new();
    }
    if head.contains(['/', '~']) || head == ".." {
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

/// Tab completes the folder being typed (`completeFolder`).
pub(super) fn complete(text: Signal<String>, folders: &[String]) {
    let line = text.get_untracked();
    let query = query(&line);
    if query.is_empty() {
        return;
    }
    let q = query.to_lowercase();
    let starts: Vec<&String> = folders.iter().filter(|f| f.to_lowercase().starts_with(&q)).collect();
    let pool: Vec<String> = if starts.is_empty() {
        finder_matches(folders, &query)
    } else {
        starts.iter().map(|s| (*s).clone()).collect()
    };
    if pool.is_empty() {
        return;
    }
    let mut done = pool[0].clone();
    if pool.len() > 1 {
        if starts.is_empty() {
            return;
        }
        let first: Vec<char> = done.chars().collect();
        let mut n = 0;
        while n < first.len() && pool.iter().all(|f| f.chars().nth(n).is_some_and(|c| c.to_lowercase().eq(first[n].to_lowercase()))) {
            n += 1;
        }
        done = first[..n].iter().collect();
    }
    if done == query || done.chars().count() < query.chars().count() {
        return;
    }
    let keep = line.len() - query.len();
    text.set(format!("{}{}", &line[..keep], done));
}

/// What a finder line does (`plan`'s finder branch), past the empty line
/// and `exit`. `nav` makes the send that runs its lines in turn.
pub(super) fn plan(shell: Shell, t: &str, nav: impl Fn(Vec<String>) -> Rc<dyn Fn()>) -> Plan {
    let sends = |say: String, cmds: Vec<String>| {
        let go = nav(cmds);
        Plan::sends(say, move || go())
    };
    let home = shell.settings.with_untracked(|s| s.home.clone());
    if t == ".." {
        return sends("goes up a folder".into(), vec!["..".into()]);
    }
    if t == "~" {
        return sends("goes home".into(), vec!["cd".into()]);
    }
    if let Some(name) = t.strip_prefix("mkdir").filter(|r| r.starts_with(char::is_whitespace)).map(str::trim_start).filter(|n| !n.is_empty()) {
        return sends(format!("makes {name}"), vec![format!("mkdir {}", json(name))]);
    }
    if let Some(rest) = t.strip_prefix("tab").filter(|r| r.is_empty() || r.starts_with(char::is_whitespace)) {
        let arg = rest.trim_start();
        let reserved = arg.starts_with('-') || ["add", "rm", "group", "ungroup"].iter().any(|w| arg.strip_prefix(w).is_some_and(|r| r.is_empty() || r.starts_with(char::is_whitespace)));
        if !arg.is_empty() && !reserved {
            return sends(format!("adds {arg} as a tab"), vec![format!("cd {}", json(arg)), "tab".into()]);
        }
        return sends("adds this folder as a tab".into(), vec![t.to_owned()]);
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
            return sends(say, vec![format!("bm {sub} {}", json(name))]);
        }
        let path = shell.settings.with_untracked(|s| s.bookmarks.get(name).cloned());
        let Some(path) = path else {
            return Plan::says(format!("no bookmark {name}"));
        };
        return sends(format!("goes to {path}"), vec![format!("cd {} --absolute", json(&path))]);
    }
    let to = t.strip_prefix("cd").filter(|r| r.starts_with(char::is_whitespace)).map_or(t, str::trim_start);
    if to == "cd" {
        return sends("goes home".into(), vec!["cd".into()]);
    }
    if let (Some(rest), false) = (to.strip_prefix("~/"), home.is_empty()) {
        return sends(format!("goes to {to}"), vec![format!("cd {} --absolute", json(&format!("{home}/{rest}")))]);
    }
    if to.starts_with('/') {
        return sends(format!("goes to {to}"), vec![format!("cd {} --absolute", json(to))]);
    }
    sends(format!("goes to {to}"), vec![format!("cd {}", json(to))])
}
