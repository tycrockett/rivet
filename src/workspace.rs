//! The workspace strip's rules, ported from Bolt's
//! `src/services/workspaces/workspaceActions.ts` with the same index
//! semantics: the active tab is the one whose path is the current
//! directory, pinned rows come first, and the stored `tabs` order is the
//! drawn order (grouped, groups in `groups` order, ungrouped last).

use crate::{
    collapse::Collapse,
    settings::{Settings, Tab},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum List {
    Pinned,
    Tabs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    MoveUp,
    MoveDown,
    Pin,
    Hide,
    Group,
}

fn shown(list: &[Tab]) -> impl Iterator<Item = &Tab> {
    list.iter().filter(|t| !t.hidden)
}

fn list<'a>(s: &'a Settings, which: List) -> &'a [Tab] {
    match which {
        List::Pinned => &s.pinned,
        List::Tabs => &s.tabs,
    }
}

/// Where the active tab is among the visible rows of its list.
pub fn cursor(s: &Settings) -> Option<(List, usize)> {
    [List::Pinned, List::Tabs].into_iter().find_map(|which| {
        shown(list(s, which))
            .position(|t| t.path == s.directory)
            .map(|i| (which, i))
    })
}

/// The group a tab renders in: a group name no longer in `groups` reads as
/// ungrouped.
pub fn group_key<'a>(tab: &'a Tab, groups: &[String]) -> Option<&'a str> {
    tab.group.as_deref().filter(|g| groups.iter().any(|x| x == g))
}

/// The tab a group's session lives in (`groupHomeOf`): a hidden home tab
/// of that group.
pub fn group_home<'a>(s: &'a Settings, group: &str) -> Option<&'a Tab> {
    s.tabs.iter().find(|t| t.home && t.hidden && group_key(t, &s.groups) == Some(group))
}

/// The rows the active tab can trade places with: all of pinned, or its own
/// group of tabs.
fn cohort(s: &Settings) -> Option<(Vec<&Tab>, usize)> {
    let (which, index) = cursor(s)?;
    let rows: Vec<&Tab> = shown(list(s, which)).collect();
    if which == List::Pinned {
        return Some((rows, index));
    }
    let key = group_key(rows[index], &s.groups);
    let mine: Vec<&Tab> = rows.into_iter().filter(|t| group_key(t, &s.groups) == key).collect();
    let at = mine.iter().position(|t| t.path == s.directory)?;
    Some((mine, at))
}

/// Pinned then regular visible rows: what prev / next walk.
pub fn flat_visible(s: &Settings) -> Vec<&Tab> {
    shown(&s.pinned).chain(shown(&s.tabs)).collect()
}

pub fn neighbor_path(s: &Settings, dir: isize) -> Option<String> {
    let flat = flat_visible(s);
    let at = flat.iter().position(|t| t.path == s.directory)? as isize;
    flat.get(usize::try_from(at + dir).ok()?).map(|t| t.path.clone())
}

pub struct Bucket<'a> {
    pub name: String,
    pub rows: Vec<&'a Tab>,
}

/// Tabs by group in `groups` order, then the ungrouped ones; a tab naming a
/// missing group falls to ungrouped.
pub fn bucket_by_group<'a>(tabs: &'a [Tab], groups: &[String]) -> (Vec<Bucket<'a>>, Vec<&'a Tab>) {
    let buckets = groups
        .iter()
        .map(|name| Bucket {
            name: name.clone(),
            rows: tabs.iter().filter(|t| group_key(t, groups) == Some(name.as_str())).collect(),
        })
        .collect();
    let ungrouped = tabs.iter().filter(|t| group_key(t, groups).is_none()).collect();
    (buckets, ungrouped)
}

/// Stores tabs in their drawn order, so ⌘N numbering matches the strip.
pub fn regroup(tabs: Vec<Tab>, groups: &[String]) -> Vec<Tab> {
    let (buckets, ungrouped) = bucket_by_group(&tabs, groups);
    buckets
        .into_iter()
        .flat_map(|b| b.rows)
        .chain(ungrouped)
        .cloned()
        .collect()
}

/// The rows ⌘2–⌘9 number, top to bottom (`numberedRows`): what the strip
/// shows of pinned, then each group's shown rows (a collapsed group keeps
/// only its chipped ones, `chipped` telling them), then the ungrouped.
pub fn numbered_rows<'a>(s: &'a Settings, collapse: &Collapse, chipped: impl Fn(&str) -> bool) -> Vec<&'a Tab> {
    let pinned = shown(&s.pinned).filter(|t| collapse.pinned_shown(chipped(&t.path)));
    let visible: Vec<Tab> = shown(&s.tabs).cloned().collect();
    let order: Vec<String> = {
        let (buckets, ungrouped) = bucket_by_group(&visible, &s.groups);
        buckets
            .into_iter()
            .flat_map(|b| {
                let name = b.name;
                b.rows.into_iter().filter(|t| collapse.row_shown(&name, chipped(&t.path))).collect::<Vec<_>>()
            })
            .chain(ungrouped)
            .map(|t| t.path.clone())
            .collect()
    };
    pinned
        .chain(order.iter().filter_map(|p| s.tabs.iter().find(|t| &t.path == p)))
        .collect()
}

/// What the strip looks like under `collapse`, to tell whether a collapse
/// step changes anything: the rows it shows, Pinned's arrow, and the
/// arrow of each group that has rows.
pub fn strip_look(s: &Settings, collapse: &Collapse, chipped: impl Fn(&str) -> bool) -> (Vec<String>, bool, Vec<bool>) {
    let rows = numbered_rows(s, collapse, &chipped).iter().map(|t| t.path.clone()).collect();
    let visible: Vec<Tab> = shown(&s.tabs).cloned().collect();
    let (buckets, _) = bucket_by_group(&visible, &s.groups);
    let arrows = buckets.iter().filter(|b| !b.rows.is_empty()).map(|b| collapse.group_step(&b.name) == crate::collapse::Step::Open).collect();
    (rows, collapse.pinned_collapsed, arrows)
}

/// Applies a drag's drop to the settings (`reorderTabs`, `reorderPinned`,
/// `setTabGroup`, and the groups' own order): `path` is the dragged row,
/// or for a group the dragged group's name. False when nothing moved.
pub fn apply_drop(s: &mut Settings, path: &str, drop: &crate::drag::Drop) -> bool {
    use crate::drag::Drop;
    let before_index = |list: &[Tab], before: &Option<String>| before.as_ref().and_then(|b| list.iter().position(|t| &t.path == b));
    let was = s.clone();
    match drop {
        Drop::Pinned { before } => {
            let Some(i) = s.pinned.iter().position(|t| t.path == path) else { return false };
            let tab = s.pinned.remove(i);
            let at = before_index(&s.pinned, before).unwrap_or(s.pinned.len());
            s.pinned.insert(at, tab);
        }
        Drop::Tab { group, before } => {
            let Some(i) = s.tabs.iter().position(|t| t.path == path) else { return false };
            let mut tab = s.tabs.remove(i);
            tab.group = group.clone();
            // Without a row to land before, it goes last: `regroup` keeps
            // the order inside each group, so that is the group's end.
            let at = before_index(&s.tabs, before).unwrap_or(s.tabs.len());
            s.tabs.insert(at, tab);
            s.tabs = regroup(std::mem::take(&mut s.tabs), &s.groups);
        }
        Drop::Group { before } => {
            let Some(i) = s.groups.iter().position(|g| g == path) else { return false };
            let name = s.groups.remove(i);
            let at = before.as_ref().and_then(|b| s.groups.iter().position(|g| g == b)).unwrap_or(s.groups.len());
            s.groups.insert(at, name);
            s.tabs = regroup(std::mem::take(&mut s.tabs), &s.groups);
        }
    }
    s.pinned != was.pinned || s.tabs != was.tabs || s.groups != was.groups
}

pub fn is_available(s: &Settings, action: Action) -> bool {
    let Some((which, _)) = cursor(s) else {
        return false;
    };
    match action {
        Action::MoveUp => cohort(s).is_some_and(|(_, i)| i > 0),
        Action::MoveDown => cohort(s).is_some_and(|(rows, i)| i + 1 < rows.len()),
        Action::Group => !s.groups.is_empty() && which == List::Tabs,
        Action::Pin | Action::Hide => true,
    }
}

/// Applies `action` to the active tab, returning the new `(tabs, pinned)`.
pub fn apply(s: &Settings, action: Action) -> (Vec<Tab>, Vec<Tab>) {
    let unchanged = (s.tabs.clone(), s.pinned.clone());
    let Some((which, _)) = cursor(s) else {
        return unchanged;
    };
    match action {
        Action::MoveUp | Action::MoveDown => {
            let dir: isize = if action == Action::MoveUp { -1 } else { 1 };
            let Some((rows, i)) = cohort(s) else {
                return unchanged;
            };
            let Some(target) = usize::try_from(i as isize + dir).ok().and_then(|j| rows.get(j)) else {
                return unchanged;
            };
            // Swap at the real indices, so hidden entries keep their place.
            let mut next = list(s, which).to_vec();
            let from = next.iter().position(|t| t.path == s.directory).expect("active row");
            let to = next.iter().position(|t| t.path == target.path).expect("target row");
            next.swap(from, to);
            match which {
                List::Pinned => (s.tabs.clone(), next),
                List::Tabs => (next, s.pinned.clone()),
            }
        }
        Action::Pin => {
            let moved = list(s, which).iter().find(|t| t.path == s.directory).cloned().expect("active row");
            let mut tabs: Vec<Tab> = s.tabs.iter().filter(|t| t.path != s.directory).cloned().collect();
            let mut pinned: Vec<Tab> = s.pinned.iter().filter(|t| t.path != s.directory).cloned().collect();
            match which {
                List::Pinned => {
                    tabs.push(moved);
                    (regroup(tabs, &s.groups), pinned)
                }
                List::Tabs => {
                    pinned.push(moved);
                    (tabs, pinned)
                }
            }
        }
        Action::Hide => {
            let flag = |t: &Tab| {
                let mut t = t.clone();
                if t.path == s.directory {
                    t.hidden = true;
                }
                t
            };
            (s.tabs.iter().map(flag).collect(), s.pinned.iter().map(flag).collect())
        }
        Action::Group => {
            let Some(current) = s.tabs.iter().find(|t| t.path == s.directory) else {
                return unchanged;
            };
            if s.groups.is_empty() {
                return unchanged;
            }
            // Through the groups in order, then back out to ungrouped.
            let at = current
                .group
                .as_ref()
                .and_then(|g| s.groups.iter().position(|x| x == g))
                .map_or(0, |i| i + 1);
            let next = s.groups.get(at).cloned();
            let moved = s
                .tabs
                .iter()
                .map(|t| {
                    let mut t = t.clone();
                    if t.path == s.directory {
                        t.group = next.clone();
                    }
                    t
                })
                .collect();
            (regroup(moved, &s.groups), s.pinned.clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn tab(path: &str, group: Option<&str>, hidden: bool) -> Tab {
        let mut t = Tab::new(path, path);
        t.group = group.map(str::to_owned);
        t.hidden = hidden;
        t
    }

    fn settings(directory: &str, tabs: Vec<Tab>, groups: &[&str]) -> Settings {
        Settings {
            directory: directory.into(),
            home: "/".into(),
            tabs,
            pinned: vec![tab("/p", None, false)],
            groups: groups.iter().map(|g| g.to_string()).collect(),
            bookmarks: BTreeMap::new(),
            commands: BTreeMap::new(),
            extra: Default::default(),
        }
    }

    fn paths(tabs: &[Tab]) -> Vec<&str> {
        tabs.iter().map(|t| t.path.as_str()).collect()
    }

    #[test]
    fn move_swaps_past_hidden_rows_and_stays_in_its_group() {
        let s = settings(
            "/c",
            vec![tab("/a", Some("g"), false), tab("/h", Some("g"), true), tab("/c", Some("g"), false), tab("/d", None, false)],
            &["g"],
        );
        let (tabs, _) = apply(&s, Action::MoveUp);
        assert_eq!(paths(&tabs), ["/c", "/h", "/a", "/d"]);
        assert!(!is_available(&s, Action::MoveDown), "/d is in another cohort");
    }

    #[test]
    fn group_cycles_through_groups_then_ungrouped_keeping_drawn_order() {
        let tabs = vec![tab("/a", Some("x"), false), tab("/b", Some("y"), false), tab("/c", None, false)];
        // x → y: buckets keep stored order, so /a stays ahead of /b.
        let (tabs, _) = apply(&settings("/a", tabs, &["x", "y"]), Action::Group);
        assert_eq!(tabs[0].group.as_deref(), Some("y"));
        assert_eq!(paths(&tabs), ["/a", "/b", "/c"]);
        // Past the last group it leaves groups: grouped rows draw first.
        let (tabs, _) = apply(&settings("/a", tabs, &["x", "y"]), Action::Group);
        assert_eq!(tabs.iter().find(|t| t.path == "/a").unwrap().group, None);
        assert_eq!(paths(&tabs), ["/b", "/a", "/c"]);
    }

    #[test]
    fn numbering_skips_hidden_rows_and_what_collapse_hides() {
        let s = settings("/a", vec![tab("/u", None, false), tab("/a", Some("g"), false), tab("/z", None, true)], &["g"]);
        let rows = |c: &Collapse, chipped: &[&str]| -> Vec<String> {
            numbered_rows(&s, c, |p| chipped.contains(&p)).iter().map(|t| t.path.clone()).collect()
        };
        assert_eq!(rows(&Collapse::default(), &[]), ["/p", "/a", "/u"]);
        let pinned = Collapse { pinned_collapsed: true, ..Default::default() };
        assert_eq!(rows(&pinned, &[]), ["/a", "/u"]);
        // A collapsed group gives up its rows' numbers, except a chipped one.
        let group = Collapse { collapsed_groups: vec!["g".into()], ..Default::default() };
        assert_eq!(rows(&group, &[]), ["/p", "/u"]);
        assert_eq!(rows(&group, &["/a"]), ["/p", "/a", "/u"]);
    }
}
