//! Bolt's hint overlay (`HintOverlay`, ⌘⏎, `hints.activate`): every
//! clickable thing on screen gets a short label; typing a label clicks it,
//! Escape leaves. Bolt finds the targets by scanning the DOM; here views
//! register theirs ([`hint`]) with what a click on them does, and the
//! overlay measures where each one is when it opens.

use std::{cell::RefCell, rc::Rc};

use nsh_core::{keymap::When, prelude::*};

// Ordered by finger reach: home row, then top, then bottom.
const CHARS: &str = "ASDFGHJKLQWERTYUIOPZXCVBNM";

/// A location's first letter never changes, so labels become muscle memory
/// (`GROUP_PREFIXES`).
const GROUP_PREFIXES: &[(&str, char)] = &[
    ("editor-view", 'J'),
    ("editor-tabs", 'F'),
    ("editor-column", 'D'),
    ("file-tree", 'E'),
    ("search", 'S'),
    ("changes", 'G'),
    ("workspaces", 'A'),
    ("palette", 'R'),
    ("sidebar-chrome", 'K'),
    ("sidebar-footer", 'L'),
    ("overlay", 'H'),
    ("row-controls", 'X'),
    ("web", 'W'),
    ("graph", 'M'),
    ("settings", 'P'),
    ("bookmarks", 'O'),
    ("bookmark-open", 'I'),
    ("bookmark-tab", 'T'),
    ("bookmark-nav", 'U'),
];
const FALLBACK_PREFIX: char = 'Y';

/// Group headers are an ordered list: their hints are ordinals.
const GROUP_ALPHABETS: &[(&str, &str)] = &[("workspace-group", "123456789")];

/// `count` labels of one length over `alphabet` (less `skip`): uniform
/// length keeps the set prefix-free, which matching by prefix needs
/// (`generateLabels`).
pub fn generate_labels(count: usize, skip: &[char], alphabet: &str) -> Vec<String> {
    let chars: Vec<char> = alphabet.chars().filter(|c| !skip.contains(c)).collect();
    if count == 0 || chars.is_empty() {
        return Vec::new();
    }
    let mut length = 1;
    while chars.len().pow(length) < count {
        length += 1;
    }
    fn build(prefix: String, depth: u32, chars: &[char], count: usize, out: &mut Vec<String>) {
        if out.len() >= count {
            return;
        }
        if depth == 0 {
            out.push(prefix);
            return;
        }
        for c in chars {
            build(format!("{prefix}{c}"), depth - 1, chars, count, out);
            if out.len() >= count {
                return;
            }
        }
    }
    let mut labels = Vec::with_capacity(count);
    build(String::new(), length, &chars, count, &mut labels);
    labels
}

/// A group's labels: its ordinals, or its prefix and letters
/// (`labelsForGroup`).
pub fn labels_for_group(group: &str, count: usize) -> Vec<String> {
    if let Some((_, alphabet)) = GROUP_ALPHABETS.iter().find(|(g, _)| *g == group) {
        return generate_labels(count, &[], alphabet);
    }
    let prefix = GROUP_PREFIXES.iter().find(|(g, _)| *g == group).map_or(FALLBACK_PREFIX, |(_, p)| *p);
    generate_labels(count, &[], CHARS).into_iter().map(|s| format!("{prefix}{s}")).collect()
}

/// How a target is labelled.
#[derive(Clone, Copy, Default)]
pub struct Opts {
    /// Rows claim a group's first labels, so controls nested in them
    /// cannot shift the label of the row below (`data-hint-primary`).
    pub primary: bool,
    /// The label sits on the target's middle (a column), not its corner.
    pub center: bool,
}

struct Target {
    group: &'static str,
    opts: Opts,
    el: ElementRef,
    /// The scroll view it sits in: scrolled out of it, it is not on screen.
    clip: Option<ElementRef>,
    action: Rc<dyn Fn()>,
}

thread_local! {
    static TARGETS: RefCell<Vec<(u64, Target)>> = const { RefCell::new(Vec::new()) };
    static NEXT: std::cell::Cell<u64> = const { std::cell::Cell::new(1) };
}

/// Makes `el` a hint target of `group` for as long as the view lives: the
/// label typed runs `action`, what a click on it does.
pub fn hint<E: Element>(el: E, group: &'static str, opts: Opts, clip: Option<ElementRef>, action: impl Fn() + 'static) -> E {
    let handle = element_ref();
    let key = NEXT.with(|n| {
        let k = n.get();
        n.set(k + 1);
        k
    });
    TARGETS.with(|t| t.borrow_mut().push((key, Target { group, opts, el: handle, clip, action: Rc::new(action) })));
    cleanup(move || TARGETS.with(|t| t.borrow_mut().retain(|(k, _)| *k != key)));
    el.element_ref(&handle)
}

/// One label on screen, in root coordinates.
#[derive(Clone, PartialEq)]
struct Shown {
    label: String,
    x: f32,
    y: f32,
    key: u64,
}

/// A target found on screen: where its label goes.
struct Found {
    key: u64,
    group: &'static str,
    opts: Opts,
    x: f32,
    y: f32,
}

/// The overlay's state.
#[derive(Clone, Copy)]
pub struct Hints {
    shown: Signal<Option<Vec<Shown>>>,
    filter: Signal<String>,
}

pub fn provide() -> Hints {
    Hints { shown: signal(None), filter: signal(String::new()) }
}

impl Hints {
    /// ⌘⏎ (`activateHintsAtom`): measures every target and labels those on
    /// screen; with none, nothing opens (staying open would swallow keys).
    /// Pressed again while open, it closes.
    pub fn activate(self) {
        if self.shown.with_untracked(Option::is_some) {
            self.deactivate();
            return;
        }
        let targets: Vec<(u64, &'static str, Opts, ElementRef, Option<ElementRef>)> =
            TARGETS.with(|t| t.borrow().iter().map(|(k, t)| (*k, t.group, t.opts, t.el, t.clip)).collect());
        spawn(async move {
            let mut found = Vec::new();
            for (key, group, opts, el, clip) in targets {
                let Some(m) = el.measure().await else { continue };
                let (w, h) = (m.frame.size.width.0, m.frame.size.height.0);
                let (left, top) = (m.page.x.0, m.page.y.0);
                if w <= 0.0 || h <= 0.0 {
                    continue;
                }
                if let Some(clip) = clip {
                    let Some(c) = clip.measure().await else { continue };
                    if top + h <= c.page.y.0 || top >= c.page.y.0 + c.frame.size.height.0 {
                        continue;
                    }
                }
                let (x, y) = if opts.center { (left + w / 2.0, top + h / 2.0) } else { (left, top) };
                found.push(Found { key, group, opts, x, y });
            }
            let shown = label(found);
            if !shown.is_empty() {
                self.filter.set(String::new());
                self.shown.set(Some(shown));
            }
        });
    }

    pub fn deactivate(self) {
        self.shown.set(None);
        self.filter.set(String::new());
    }

    /// A key while open: the label typed so far grows; one complete match
    /// runs its target.
    fn type_char(self, c: char) {
        let next = format!("{}{c}", self.filter.get_untracked());
        let hit = self.shown.with_untracked(|s| {
            let matches: Vec<&Shown> = s.iter().flatten().filter(|h| h.label.starts_with(&next)).collect();
            (matches.len() == 1 && matches[0].label == next).then(|| matches[0].key)
        });
        match hit {
            Some(key) => {
                self.deactivate();
                let action = TARGETS.with(|t| t.borrow().iter().find(|(k, _)| *k == key).map(|(_, t)| t.action.clone()));
                // Under the app: this key's handler belongs to the overlay,
                // gone now, and what the action starts would go with it.
                if let Some(action) = action {
                    crate::hub::engine::spawn_root(async move { action() });
                }
            }
            None => self.filter.set(next),
        }
    }
}

/// Labels per group: rows first, then top to bottom, left to right; a
/// centered label put with its own middle on the target's (a character of
/// the 11pt monospace is about 7pt wide).
fn label(found: Vec<Found>) -> Vec<Shown> {
    let mut groups: Vec<&'static str> = Vec::new();
    for f in &found {
        if !groups.contains(&f.group) {
            groups.push(f.group);
        }
    }
    let mut shown = Vec::new();
    for group in groups {
        let mut members: Vec<&Found> = found.iter().filter(|f| f.group == group).collect();
        members.sort_by(|a, b| b.opts.primary.cmp(&a.opts.primary).then(a.y.total_cmp(&b.y)).then(a.x.total_cmp(&b.x)));
        for (m, label) in members.iter().zip(labels_for_group(group, members.len())) {
            let (x, y) = if m.opts.center { (m.x - (label.len() as f32 * 7.0 + 8.0) / 2.0, m.y - 7.5) } else { (m.x, m.y) };
            shown.push(Shown { label, x, y, key: m.key });
        }
    }
    shown
}

/// The labels, over everything; while open, letters, digits, Backspace
/// and Escape are the overlay's.
pub fn overlay(hints: Hints) -> impl IntoElement {
    when(move || hints.shown.with(Option::is_some), move || {
        let always = KeyOptions { when: When::Always, ..Default::default() };
        on_key_with("escape", always, move |_| hints.deactivate());
        on_key_with("backspace", always, move |_| {
            hints.filter.update(|f| {
                f.pop();
            })
        });
        for c in CHARS.chars().chain('0'..='9') {
            on_key_with(&c.to_ascii_lowercase().to_string(), always, move |_| hints.type_char(c));
        }
        let labels = move || hints.shown.get().unwrap_or_default();
        view().absolute().inset(px(0.0)).pointer_events(nsh_core::PointerEvents::None).child(each(labels, |h| h.key, move |h, _| {
            let h = h.get_untracked();
            let matching = {
                let label = h.label.clone();
                move || hints.filter.with(|f| label.starts_with(f.as_str()))
            };
            let typed = move || hints.filter.with(String::len);
            let mut badge = view()
                .absolute()
                .left(px(h.x))
                .top(px(h.y))
                .flex_row()
                .px(px(4.0))
                .py(px(2.0))
                .rounded(px(3.0))
                .bg(rgb(0xff8c1a))
                .opacity({
                    let matching = matching.clone();
                    move || if matching() { 1.0_f32 } else { 0.15 }
                });
            // The typed part of a matching label dims.
            for (n, ch) in h.label.chars().enumerate() {
                let matching = matching.clone();
                badge = badge.child(
                    text(ch.to_string())
                        .font_family("ui-monospace")
                        .text_size(px(11.0))
                        .font_weight(FontWeight::BOLD)
                        .line_height(px(11.0))
                        .text_color(rgb(0x1a0d00))
                        .opacity(move || if n < typed() && matching() { 0.5_f32 } else { 1.0 }),
                );
            }
            badge
        }))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // `hintLabels.check.ts`.
    #[test]
    fn labels_keep_their_prefixes_lengths_and_order() {
        assert_eq!(labels_for_group("editor-tabs", 3), ["FA", "FS", "FD"]);
        let tree = labels_for_group("file-tree", 14);
        assert_eq!(tree[8], "EL");
        assert_eq!(tree[9], "EQ");
        assert!(tree.iter().all(|l| l.len() == tree[0].len()));
        assert_eq!(labels_for_group("nope", 1)[0], format!("{FALLBACK_PREFIX}A"));
        assert_eq!(labels_for_group("workspace-group", 3), ["1", "2", "3"]);
        let overflow = labels_for_group("workspace-group", 10);
        assert!(overflow.iter().all(|l| l.len() == 2));
        assert_eq!(overflow[9], "21");
        assert_eq!(generate_labels(3, &['A'], CHARS), ["S", "D", "F"]);
    }

    #[test]
    fn no_label_is_a_prefix_of_another_across_groups() {
        let prefixes: Vec<char> = GROUP_PREFIXES.iter().map(|(_, p)| *p).collect();
        let mut unique = prefixes.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), prefixes.len());
        assert!(!prefixes.contains(&FALLBACK_PREFIX));
        assert!(!prefixes.iter().chain([&FALLBACK_PREFIX]).any(|p| *p == 'Q' || *p == 'Z'));
        let all: Vec<String> = GROUP_PREFIXES
            .iter()
            .map(|(g, _)| *g)
            .chain(["nope", "workspace-group"])
            .flat_map(|g| labels_for_group(g, 30))
            .collect();
        for a in &all {
            for b in &all {
                assert!(a == b || !b.starts_with(a.as_str()), "{a} is a prefix of {b}");
            }
        }
    }

    #[test]
    fn rows_take_a_groups_first_labels_then_screen_order() {
        let f = |key, primary, x, y| Found { key, group: "workspaces", opts: Opts { primary, center: false }, x, y };
        let shown = label(vec![f(1, false, 40.0, 10.0), f(2, true, 10.0, 50.0), f(3, true, 10.0, 20.0)]);
        let order: Vec<(u64, &str)> = shown.iter().map(|s| (s.key, s.label.as_str())).collect();
        assert_eq!(order, [(3, "AA"), (2, "AS"), (1, "AD")]);
    }
}
