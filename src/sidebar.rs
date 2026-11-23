//! The workspace strip (Bolt's `WorkspacesPanel`): pinned rows, then each
//! group's rows, then the ungrouped ones, each with its git dot, its ports
//! and ⌘N (× to close on hover), and under a row the chips of its
//! terminals, with what each is doing, and of its open PR. Pinned and the
//! groups collapse from their headers and through ⌘0 ([`crate::collapse`]);
//! rows and group headings move by dragging ([`crate::drag`]).

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    path::Path,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use nsh_base::theme;
use nsh_core::{
    TouchEvent, TouchPhase,
    element::desktop::{MenuItem, popup_menu},
    prelude::*,
};

use crate::{
    changes,
    collapse::{Collapse, Step},
    commands,
    drag::{self, Item},
    git,
    hints::{self, Opts},
    hub::{self, ChoiceOption, Hub, engine::TermStatus},
    ports,
    prs::{self, Badge, Pr},
    settings::{Settings, Tab},
    state::{Shell, shell},
    workspace::{self, Action, bucket_by_group, group_home, numbered_rows},
};

/// What the strip shows that changes on its own clock, polled while it
/// lives: each terminal's status (`terminalStatusAtom`), each workspace's
/// listening ports (`usePorts`) and open PR (`useTabPrs`, cached with the
/// branch it was asked for).
#[derive(Clone, Copy)]
struct Live {
    terms: Signal<HashMap<u64, TermStatus>>,
    ports: Signal<HashMap<String, Vec<String>>>,
    prs: Signal<HashMap<String, (String, Option<Pr>)>>,
}

impl Live {
    fn start(shell: Shell) -> Live {
        let live = Live { terms: signal(HashMap::new()), ports: signal(HashMap::new()), prs: signal(HashMap::new()) };
        let poll_terms = move || {
            let next: HashMap<u64, TermStatus> =
                shell.terms.with_untracked(|t| t.iter().map(|t| (t.id, hub::engine::terminal_status(t.id))).collect());
            if live.terms.with_untracked(|m| *m != next) {
                live.terms.set(next);
            }
        };
        poll_terms();
        set_interval(Duration::from_secs(1), poll_terms);
        live.scan_ports();
        set_interval(Duration::from_secs(30), move || live.scan_ports());

        // Asked again when a branch moves (or first shows), and on timers.
        let asking: Rc<RefCell<HashSet<String>>> = Rc::default();
        let ask = {
            let asking = asking.clone();
            move |path: String, branch: String| {
                if !asking.borrow_mut().insert(path.clone()) {
                    return;
                }
                let asking = asking.clone();
                spawn(async move {
                    let dir = path.clone();
                    let pr = git::off_thread(move || prs::fetch(&dir)).await;
                    asking.borrow_mut().remove(&path);
                    live.prs.update(|m| {
                        m.insert(path, (branch, pr));
                    });
                });
            }
        };
        let remote = move || -> Vec<(String, String)> {
            shell.statuses.with_untracked(|m| m.iter().filter(|(_, s)| s.has_remote).map(|(p, s)| (p.clone(), s.branch.clone())).collect())
        };
        {
            let ask = ask.clone();
            effect(move || {
                shell.statuses.with(|_| ());
                for (path, branch) in remote() {
                    if live.prs.with_untracked(|m| m.get(&path).map(|(b, _)| b) != Some(&branch)) {
                        ask(path, branch);
                    }
                }
            });
        }
        {
            let ask = ask.clone();
            set_interval(Duration::from_secs(15), move || {
                for (path, branch) in remote() {
                    if live.prs.with_untracked(|m| m.get(&path).is_some_and(|(_, pr)| pr.as_ref().is_some_and(Pr::running))) {
                        ask(path, branch);
                    }
                }
            });
        }
        set_interval(Duration::from_secs(300), move || {
            for (path, branch) in remote() {
                ask(path, branch);
            }
        });
        live
    }

    fn scan_ports(self) {
        spawn(async move {
            let found = git::off_thread(ports::scan).await;
            if self.ports.with_untracked(|m| *m != found) {
                self.ports.set(found);
            }
        });
    }

    /// The terminals of `paths`, summed up for a collapsed heading
    /// (`termSummary`): how many, the busiest status (agent, working,
    /// idle), and whether one waits for the user.
    fn summary(self, shell: Shell, paths: &[String]) -> Option<(usize, String, bool)> {
        let ids: Vec<u64> = shell.terms.with(|t| t.iter().filter(|t| paths.contains(&t.cwd)).map(|t| t.id).collect());
        if ids.is_empty() {
            return None;
        }
        self.terms.with(|m| {
            let of = |s: &str| ids.iter().any(|id| m.get(id).is_some_and(|t| t.status == s));
            let status = if of("agent") { "agent" } else if of("working") { "working" } else { "idle" };
            Some((ids.len(), status.to_owned(), ids.iter().any(|id| m.get(id).is_some_and(|t| t.needs_input))))
        })
    }
}

/// A terminal's color (`termBg`; `WAITING_CYAN` while it waits for you).
fn status_color(status: &str, needs_input: bool) -> Hsla {
    let hex = if needs_input {
        0x39c5cf
    } else {
        match status {
            "agent" => 0x98c379,
            "working" => crate::theme::BOLT_THEME.vars.accent,
            _ => 0xe5c07b,
        }
    };
    rgb(hex).into()
}

/// The 12pt square a terminal chip or a collapsed heading leads with, in
/// `color`, with `glyph` cut out in the background color.
fn badge(color: impl Fn() -> Hsla + 'static, glyph: &'static str) -> View {
    view()
        .w(px(12.0))
        .h(px(12.0))
        .flex_shrink(0.0)
        .rounded(px(3.0))
        .items_center()
        .justify_center()
        .bg(color)
        .child(
            text(glyph)
                .text_size(px(9.0))
                .line_height(px(12.0))
                .font_weight(FontWeight::BOLD)
                .text_color(rgb(crate::theme::BOLT_THEME.xterm.background)),
        )
}

/// Bolt's `WAITING_CYAN` (a terminal waiting for the user), and the color
/// its dots have otherwise (`--theme-success`).
const WAITING_CYAN: u32 = 0x39c5cf;
const PULSE: u32 = 0x98c379;

/// `PulseDots`' dot (3px), the gap between it and the square, the room
/// between dots, and the animated values a ring's dots share.
const DOT: f32 = 3.0;
const GAP: f32 = DOT / 4.0;
const SPACE: f32 = 2.5;
const CHANNELS: usize = 8;

/// The top left corners of `PulseDots`' ring around a `w` × `h` square with
/// corner radius `r` (`sizedRing`): dots along each rounded corner and each
/// side, `GAP` out from the edge.
fn ring(w: f32, h: f32, r: f32) -> Vec<(f32, f32)> {
    use std::f32::consts::FRAC_PI_2;
    let mut dots = Vec::new();
    let mut dot = |x: f32, y: f32| dots.push((x - DOT / 2.0, y - DOT / 2.0));
    let arc = ((FRAC_PI_2 * (r + GAP) / SPACE).round().max(1.0)) as usize;
    for (cx, cy, sx, sy) in [(r, r, -1.0, -1.0), (w - r, r, 1.0, -1.0), (w - r, h - r, 1.0, 1.0), (r, h - r, -1.0, 1.0)] {
        for k in 0..=arc {
            let a = k as f32 / arc as f32 * FRAC_PI_2;
            dot(cx + sx * (r + GAP) * a.cos(), cy + sy * (r + GAP) * a.sin());
        }
    }
    // Top, bottom, left, right.
    for side in 0..4 {
        let len = if side < 2 { w - 2.0 * r } else { h - 2.0 * r };
        let n = (len / SPACE).round().max(1.0) as usize;
        for k in 1..n {
            let t = k as f32 / n as f32;
            let (x, y) = match side {
                0 => (r + t * (w - 2.0 * r), -GAP),
                1 => (r + t * (w - 2.0 * r), h + GAP),
                2 => (-GAP, r + t * (h - 2.0 * r)),
                _ => (w + GAP, r + t * (h - 2.0 * r)),
            };
            dot(x, y);
        }
    }
    dots
}

/// A number in 0..1 (the dots' random timings; nothing worth a seed).
fn random() -> f32 {
    use std::hash::{BuildHasher, RandomState};
    (RandomState::new().hash_one(0u8) >> 40) as f32 / (1u64 << 24) as f32
}

/// One dot's twinkle (`pulseDot`: opacity 0.3, up to 1 and back, `ease-in-out`,
/// over 1.2 to 2.4 s) on the animation graph, which plays it natively with no
/// engine work per frame. Bolt starts each dot at a random phase with a
/// negative `animation-delay`; here it starts at a random opacity on its way
/// up, which is as good as a phase once the periods differ. It stops with the
/// owner (a loop outlives the value it moves otherwise).
fn twinkle() -> AnimatedValue {
    let period = 1200.0 + 1200.0 * random();
    let from = 0.3 + 0.7 * random();
    let value = animated_value(from);
    let leg = move |to: f32, ms: f32| Animation::timing(&value, to, Timing::new(ms.max(1.0), Easing::EaseInOut).frame_rate(30.0));
    let cycle = Animation::sequence([leg(0.3, period / 2.0), leg(1.0, period / 2.0)]).looped(None).reset_before_iteration(false);
    let run = Animation::sequence([leg(1.0, period / 2.0 * (1.0 - from) / 0.7), cycle]);
    let _ = run.start();
    cleanup(move || run.stop());
    value
}

/// `PulseDots`' ring around a badge, `color` dots twinkling over it, each
/// bound to one of a few shared [`twinkle`]s. Bolt also bursts the dots
/// outwards when the pulse ends, and fires sparks from a waiting badge: not
/// drawn here (each would need its own run of animated values per spark).
fn pulse_dots(color: u32) -> View {
    let channels: Vec<AnimatedValue> = (0..CHANNELS).map(|_| twinkle()).collect();
    let dots: Vec<AnyElement> = ring(12.0, 12.0, 3.0)
        .into_iter()
        .enumerate()
        .map(|(i, (x, y))| {
            view()
                .absolute()
                .left(px(x))
                .top(px(y))
                .w(px(DOT))
                .h(px(DOT))
                .rounded_full()
                .bg(rgb(color))
                .bind_opacity(&channels[i % CHANNELS])
                .into_element()
        })
        .collect();
    view().absolute().top_0().left_0().w(px(12.0)).h(px(12.0)).pointer_events(nsh_core::PointerEvents::None).children(dots)
}

/// `badge`, ringed by [`pulse_dots`] of the color `pulse` gives while it
/// gives one (the terminal is an agent, or waits for the user).
fn pulsing(badge: View, pulse: impl Fn() -> Option<u32> + 'static) -> View {
    // A memo: the dots are made again when they start, stop or change
    // color, not each time what `pulse` reads is written.
    let pulse = computed(pulse);
    badge.relative().child(dynamic(move || pulse.get().map(pulse_dots)))
}

/// The strip; it is drawn again when the settings change (a `cd`, a pin).
/// It fills its panel, whose width the user drags (`lib.rs`).
pub fn sidebar() -> impl IntoElement {
    let shell = shell();
    let hub = hub::hub();
    // The terminal's background (the editor theme's), as Bolt's left pane
    // has `--theme-bg`; rounded like the terminal box on the side it faces.
    view()
        .flex_1()
        .bg(rgb(crate::theme::BOLT_THEME.xterm.background))
        .rounded_r(px(12.0))
        .overflow_hidden()
        // The hidden title bar's room: the traffic lights sit in it, and it
        // moves the window. Bolt's sidebar tabs bar holds the ⌘0 cycle's
        // dots, right of the traffic lights here.
        .child(
            view()
                .h(move || nsh_core::element::desktop::title_bar().top)
                .window_drag(true)
                .flex_row()
                .items_center()
                .pl(move || px(nsh_core::element::desktop::title_bar().left.0 + 8.0))
                .child(cycle_dots(shell)),
        )
        // The list, with the palette over it when open: Bolt's left pane
        // holds `BoltPalette` in the area between its tabs and its footer,
        // inset over the workspaces.
        .child(
            view()
                .relative()
                .flex_1()
                .child({
                    // The rows with terminals, as a set: the strip is drawn
                    // again when one gains or loses its first terminal, not
                    // on every title change.
                    let chipped = computed(move || shell.chipped_paths());
                    let live = Live::start(shell);
                    // Rows scrolled out of the list get no ⌘⏎ label.
                    let list = element_ref();
                    scroll().element_ref(&list).flex_1().child(view().py_2().child(dynamic(move || {
                        let s = shell.settings.get();
                        let c = shell.collapse.get();
                        chipped.with(|chipped| strip(hub, live, &s, &c, chipped, list))
                    })))
                })
                .child(hub::palette::palette()),
        )
        .child(footer(shell))
}

/// The ⌘0 cycle's dots (`CycleDots`): one per step, the accent one sliding
/// from the step it left to the step it is at. They show on each ⌘0 or
/// header click and fade a second later.
fn cycle_dots(shell: Shell) -> impl IntoElement {
    const DOT: f32 = 10.0;
    const GAP: f32 = 3.0;
    let v = &crate::theme::BOLT_THEME.vars;
    let (surface, accent, faint) = (v.bg_surface, v.accent, crate::theme::mix(v.fg_muted, 0.6, v.bg));
    let shown = signal(false);
    let x = animated_value(0.0);
    let fade = animated_value(1.0);
    let last = std::rc::Rc::new(std::cell::Cell::new(None::<u8>));
    effect(move || {
        let tick = shell.collapse_tick.get();
        let step = untracked(|| shell.collapse.with(|c| c.cycle_step(&shell.strip())));
        let from = last.replace(Some(step));
        if tick == 0 {
            return;
        }
        // From the step it left, so the first press moves too.
        x.set(f32::from(from.unwrap_or(step)) * (DOT + GAP));
        let _ = x.spring(f32::from(step) * (DOT + GAP), Spring::default());
        fade.set(1.0);
        shown.set(true);
        // A later press restarts the clock: only the last one's timers act.
        set_timeout(std::time::Duration::from_millis(1000), move || {
            if shell.collapse_tick.get_untracked() == tick {
                let _ = fade.timing(0.0, 300.0, Easing::EaseOut);
            }
        });
        set_timeout(std::time::Duration::from_millis(1300), move || {
            if shell.collapse_tick.get_untracked() == tick {
                shown.set(false);
            }
        });
    });
    when(shown, move || {
        let dots = (0..4).map(|_| view().w(px(DOT)).h(px(DOT)).rounded_full().bg(rgb(faint)).into_element()).collect::<Vec<_>>();
        view().bind_opacity(&fade).rounded_full().px_1().py(px(2.0)).bg(rgb(surface)).child(
            view()
                .relative()
                .flex_row()
                .items_center()
                .gap(px(GAP))
                .h(px(DOT))
                .children(dots)
                .child(view().absolute().top_0().left_0().w(px(DOT)).h(px(DOT)).rounded_full().bg(rgb(accent)).bind_translate_x(&x)),
        )
    })
}

fn strip(hub: Hub, live: Live, s: &Settings, c: &Collapse, chipped: &HashSet<String>, list: ElementRef) -> Vec<AnyElement> {
    let shell = hub.shell;
    let has = |path: &str| chipped.contains(path);
    let numbers: Vec<String> = numbered_rows(s, c, has).iter().map(|t| t.path.clone()).collect();
    let number = |tab: &Tab| numbers.iter().position(|p| *p == tab.path).map(|i| i + 2).filter(|n| *n <= 9);
    let mut out = Vec::new();

    let pinned: Vec<&Tab> = s.pinned.iter().filter(|t| !t.hidden).collect();
    if !pinned.is_empty() {
        let pinned_chips = pinned.iter().any(|t| has(&t.path));
        // Only what the collapse hid: a surviving row shows its own chips.
        let hidden: Vec<String> = pinned.iter().filter(|t| !c.pinned_shown(has(&t.path))).map(|t| t.path.clone()).collect();
        out.push(section(
            "sidebar-chrome",
            list,
            if c.pinned_collapsed { "▸ Pinned" } else { "▾ Pinned" }.to_owned(),
            c.pinned_locked,
            c.pinned_collapsed.then(|| summary(live, shell, hidden)),
            move || shell.change_collapse(|c| c.toggle_pinned(pinned_chips, true)),
            move || shell.collapse.update(|c| c.pinned_locked = !c.pinned_locked),
        ));
        out.extend(
            pinned
                .iter()
                .filter(|t| c.pinned_shown(has(&t.path)))
                .map(|t| row(hub, live, t, number(t), Item::Pinned(t.path.clone()), list)),
        );
    }
    let visible: Vec<Tab> = s.tabs.iter().filter(|t| !t.hidden).cloned().collect();
    let (groups, ungrouped) = bucket_by_group(&visible, &s.groups);
    for bucket in groups.into_iter().filter(|b| !b.rows.is_empty()) {
        let name = bucket.name.clone();
        let home = group_home(s, &name);
        let has_chips = home.is_some_and(|h| has(&h.path)) || bucket.rows.iter().any(|t| has(&t.path));
        let has_home = home.is_some();
        let collapsed = c.group_step(&name) != Step::Open;
        // The session's terminals too: the strip draws no session row.
        let hidden: Vec<String> = bucket
            .rows
            .iter()
            .filter(|t| !c.row_shown(&name, has(&t.path)))
            .map(|t| t.path.clone())
            .chain(home.map(|h| h.path.clone()))
            .collect();
        let (press_name, lock_name) = (name.clone(), name.clone());
        let heading = section(
            "workspace-group",
            list,
            format!("{} {name}", if collapsed { "▸" } else { "▾" }),
            c.is_locked(&name),
            collapsed.then(|| summary(live, shell, hidden)),
            move || shell.change_collapse(|c| c.toggle_group(&press_name, has_chips, has_home, true)),
            move || shell.collapse.update(|c| c.toggle_group_lock(&lock_name)),
        );
        out.push(draggable(shell, heading, Item::Header(Some(name.clone()))));
        out.extend(bucket.rows.iter().filter(|t| c.row_shown(&name, has(&t.path))).map(|t| {
            let item = Item::Row { path: t.path.clone(), group: Some(name.clone()) };
            row(hub, live, t, number(t), item, list)
        }));
    }
    if !ungrouped.is_empty() {
        out.push(drag::slot(heading("Tabs"), Item::Header(None), None).into_element());
        out.extend(ungrouped.iter().map(|t| row(hub, live, t, number(t), Item::Row { path: t.path.clone(), group: None }, list)));
    }
    out
}

/// A collapsed heading's terminal badge (`StatusBadge`), for the terminals
/// of the rows it hides.
fn summary(live: Live, shell: Shell, paths: Vec<String>) -> AnyElement {
    // A memo: the badge (and its dots' phases) is made again when what it
    // shows changes, not on every poll of the terminals.
    let state = computed(move || live.summary(shell, &paths).map(|(_, status, waiting)| (status, waiting)));
    dynamic(move || {
        let (status, waiting) = state.get()?;
        let pulse = (status == "agent" || waiting).then_some(if waiting { WAITING_CYAN } else { PULSE });
        Some(pulsing(badge(move || status_color(&status, waiting), "›"), move || pulse))
    })
    .into_element()
}

/// What a drag of one card keeps between its events.
#[derive(Default)]
struct Held {
    /// The pointer's place (page y) when it went down, and how far it has
    /// moved since: the card's pan translation, which a pan reports only at
    /// its ends.
    down: f32,
    by: f32,
    /// The strip as measured a moment after the pan began: the rows aside
    /// of it slide out of the way meanwhile ([`drag::Session`]).
    session: Option<drag::Session>,
    /// A pan is going on (not yet let go).
    live: bool,
}

/// Makes `el` draggable up and down (Bolt's `draggable` cards): it follows
/// the pointer while the rows it would pass slide out of the way, and where
/// it is let go decides where `item` lands ([`drag::drop_at`]); it settles
/// there, then the settings take the new order.
///
/// A pan reports only its start and end, and moves `dy` natively between
/// them. Where it is meanwhile comes from the touches of the pointer, which
/// the card hears as the platform reports them (on macOS each drag event,
/// `report(Move)` in `nsh-platform-macos`'s `input.rs`, in a pan too).
fn draggable(shell: Shell, el: AnyElement, item: Item) -> AnyElement {
    let dy = animated_value(0.0);
    // The pointer holds it (a little see-through), until it settles (over
    // the rows it passes).
    let held = signal(false);
    let lifted = signal(false);
    let state = Rc::new(RefCell::new(Held::default()));
    let (touch_state, start_state, end_state) = (state.clone(), state.clone(), state);
    let (started, dropped) = (item.clone(), item.clone());
    let wrapper = view()
        .pan_values(None, Some(&dy))
        .z_index(move || if lifted.get() { 10 } else { 0 })
        .opacity(move || if held.get() { 0.85_f32 } else { 1.0 })
        .on_touch(move |e: &TouchEvent| {
            let Some(p) = e.points.iter().find(|p| p.changed) else { return };
            let y = p.page.y.0;
            let mut h = touch_state.borrow_mut();
            match e.phase {
                TouchPhase::Start => {
                    h.down = y;
                    h.by = 0.0;
                }
                TouchPhase::Move => {
                    let by = y - h.down;
                    h.by = by;
                    if let Some(session) = h.session.as_mut() {
                        session.moved(by);
                    }
                }
                _ => {}
            }
        })
        .on_pan_start(move |_: &PanEvent| {
            held.set(true);
            lifted.set(true);
            {
                let mut h = start_state.borrow_mut();
                h.live = true;
                h.session = None;
            }
            let (state, item) = (start_state.clone(), started.clone());
            spawn(async move {
                let slots = drag::measure().await;
                let mut h = state.borrow_mut();
                if !h.live {
                    return;
                }
                h.session = drag::Session::start(slots, &item);
                let by = h.by;
                if let Some(session) = h.session.as_mut() {
                    session.moved(by);
                }
            });
        })
        .on_pan_end(move |e: &PanEvent| {
            held.set(false);
            let by = e.translation.y.0;
            let session = {
                let mut h = end_state.borrow_mut();
                h.live = false;
                h.session.take()
            };
            let item = dropped.clone();
            // Detached: a strip drawn again while the card settles (a
            // terminal's chip came or went) must not lose the drop.
            Owner::detached().run(|| {
                spawn(async move {
                    // Let go before the strip was measured: measure it now.
                    let session = match session {
                        Some(s) => Some(s),
                        None => drag::Session::start(drag::measure().await, &item),
                    };
                    let Some(mut session) = session else {
                        drag::settle(dy, 0.0).await;
                        lifted.try_set(false);
                        return;
                    };
                    let (drop, to) = session.release(by);
                    drag::settle(dy, to).await;
                    lifted.try_set(false);
                    let Some(drop) = drop else { return };
                    let key = match &item {
                        Item::Pinned(p) | Item::Row { path: p, .. } | Item::Header(Some(p)) => p.clone(),
                        Item::Header(None) => return,
                    };
                    let mut s = shell.settings.get_untracked();
                    if workspace::apply_drop(&mut s, &key, &drop) {
                        // The strip is drawn again at the order it showed.
                        shell.set_settings(s);
                    } else {
                        // Nothing to change after all: everything back.
                        session.cancel();
                        drag::settle(dy, 0.0).await;
                    }
                });
            });
        })
        .child(el);
    drag::slot(wrapper, item, Some(&dy)).into_element()
}

/// A section's heading: a click moves its collapse one step, the context
/// menu locks it against ⌘0. Hovered, it takes the rows' hover background;
/// its text sits where a plain heading's does, and collapsed, the badge of
/// what it hides sits at its end. ⌘⏎ labels it under `group`.
fn section(
    group: &'static str,
    list: ElementRef,
    label: String,
    locked: bool,
    badge: Option<AnyElement>,
    on_press: impl Fn() + Clone + 'static,
    on_lock: impl Fn() + 'static,
) -> AnyElement {
    let on_lock = Rc::new(on_lock);
    let heading = view()
        .mx_2()
        .mt_2()
        .px_1()
        .py_1()
        .flex_row()
        .items_center()
        .rounded_md()
        .hover(|s| s.bg(move || theme().sidebar_accent.opacity(0.6)))
        .on_press(on_press.clone())
        .on_context_menu(move |_: &ContextMenuEvent| {
            let on_lock = on_lock.clone();
            let item = MenuItem::new(if locked { "Unlock" } else { "Lock" }).on_select(move || on_lock());
            let _ = popup_menu(vec![item]);
        })
        .child(heading_text(label).flex_1())
        .when_some(badge, |v, b| v.child(b));
    hints::hint(heading, group, Opts::default(), Some(list), on_press).into_element()
}

fn heading(label: impl Into<SharedString>) -> View {
    view().px_3().pt_3().pb_1().child(heading_text(label))
}

fn heading_text(label: impl Into<SharedString>) -> Text {
    text(label.into())
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .number_of_lines(1)
        .text_color(move || theme().muted_foreground)
}

/// How far a tab card is lightened from the strip's background: every card
/// alike at rest (pinned and grouped too), lighter hovered, lightest active.
const CARD_REST: f32 = 0.07;
const CARD_HOVER: f32 = 0.11;
const CARD_ACTIVE: f32 = 0.16;

fn card_tone(amount: f32) -> Hsla {
    rgb(crate::theme::lighten(crate::theme::BOLT_THEME.xterm.background, amount)).into()
}

/// One tab: its dot, its name, its ports, its ⌘N (× while hovered or
/// active), and under them the chips of its terminals and PR, all one card:
/// a click on it goes there (`cd <path> --absolute`, as Bolt's
/// `handleTabClick`), a right click opens its menu, a drag moves it; a
/// chip's own click shows its terminal.
fn row(hub: Hub, live: Live, tab: &Tab, number: Option<usize>, item: Item, list: ElementRef) -> AnyElement {
    let shell = hub.shell;
    let path = tab.path.clone();
    let pinned = matches!(item, Item::Pinned(_));
    let active = {
        let path = path.clone();
        move || shell.settings.with(|s| s.directory == path)
    };
    let hovered = signal(false);
    let show_close = {
        let active = active.clone();
        move || hovered.get() || active()
    };
    let (go_path, menu_path, dot_path, chips_path) = (path.clone(), path.clone(), path.clone(), path.clone());
    let accent = tab.color.as_deref().and_then(parse_hex);
    let hint_path = path.clone();
    let go = move || commands::submit(shell, format!("cd {hint_path} --absolute"));
    let card = view()
        .mx_2()
        .mb_1()
        .py(px(6.0))
        .rounded_md()
        .bg({
            let active = active.clone();
            move || card_tone(if active() { CARD_ACTIVE } else { CARD_REST })
        })
        .hover({
            let active = active.clone();
            move |s| s.bg(move || card_tone(if active() { CARD_ACTIVE } else { CARD_HOVER }))
        })
        .on_mouse_enter(move |_: &MouseEvent| hovered.set(true))
        .on_mouse_leave(move |_: &MouseEvent| hovered.set(false))
        .on_press(move || commands::submit(shell, format!("cd {go_path} --absolute")))
        .on_context_menu(move |_: &ContextMenuEvent| tab_menu(hub, menu_path.clone(), pinned))
        .child(
            view()
                .flex_row()
                .items_center()
                .gap_2()
                .px_2()
                .h(px(20.0))
                .child(status_dot(shell, dot_path))
                .when_some(accent, |v, color| v.child(view().w(px(3.0)).h(px(14.0)).rounded_full().bg(color)))
                .child(
                    text(tab.name.clone())
                        .flex_1()
                        .text_sm()
                        .number_of_lines(1)
                        .text_color({
                            let active = active.clone();
                            move || if active() { theme().sidebar_accent_foreground } else { theme().sidebar_foreground }
                        }),
                )
                .child(port_pill(live, path.clone()))
                .child(close_or_number(hub, path.clone(), number, show_close)),
        )
        .child(subtext(shell, tab, if accent.is_some() { 35.0 } else { 24.0 }))
        .child(chips(shell, live, chips_path, list));
    let card = hints::hint(card, "workspaces", Opts { primary: true, ..Opts::default() }, Some(list), go).into_element();
    draggable(shell, card, item)
}

/// The row's ⌘N, and over it the × that closes the row (Bolt's close
/// button, asking as ⇧⌘X does), shown while the row is hovered or active.
/// Both stay mounted and trade opacity, as Bolt's do: swapping the × in
/// and out under the pointer lost the click to the row.
fn close_or_number(hub: Hub, path: String, number: Option<usize>, show_close: impl Fn() -> bool + Clone + 'static) -> impl IntoElement {
    let hide_number = show_close.clone();
    // At least the ×'s 18pt, wider when ⌘N needs it: a fixed 18 wrapped it.
    view()
        .relative()
        .min_w(px(18.0))
        .h(px(18.0))
        .flex_shrink(0.0)
        .items_center()
        .justify_center()
        .when_some(number, |v, n| {
            v.child(
                text(format!("⌘{n}"))
                    .text_xs()
                    .number_of_lines(1)
                    .text_color(move || theme().muted_foreground)
                    .opacity(move || if hide_number() { 0.0_f32 } else { 1.0 }),
            )
        })
        .child(
            view()
                .absolute()
                .top_0()
                .right_0()
                .w(px(18.0))
                .h(px(18.0))
                .items_center()
                .justify_center()
                .rounded(px(4.0))
                .opacity(move || if show_close() { 1.0_f32 } else { 0.0 })
                .hover(|s| s.bg(Hsla::from(rgb(0xffffff)).opacity(0.08)))
                .on_press(move || request_close_tab(hub, path.clone()))
                .child(text("×").text_size(px(14.0)).line_height(px(18.0)).text_color(move || theme().sidebar_foreground)),
        )
}

/// The row's listening ports (Bolt's port pill): the first, and how many
/// more; a click lists them to kill.
fn port_pill(live: Live, path: String) -> impl IntoElement {
    dynamic(move || {
        let ports = live.ports.with(|m| m.get(&path).cloned())?;
        let first = ports.first()?.clone();
        let label = if ports.len() > 1 { format!("{first}+{}", ports.len() - 1) } else { first };
        let pill = view()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .flex_shrink(0.0)
            .rounded_full()
            .pl(px(4.0))
            .pr(px(6.0))
            .py(px(2.0))
            .bg(Hsla::from(rgb(0x000000)).opacity(0.35))
            .on_press(move || port_menu(live, ports.clone()))
            .child(view().w(px(6.0)).h(px(6.0)).rounded_full().bg(rgb(0x22c55e)))
            .child(text(label).text_size(px(10.0)).line_height(px(12.0)).text_color(move || theme().muted_foreground));
        Some(pill)
    })
}

/// The port pill's menu: a click kills what listens on that port.
fn port_menu(live: Live, open: Vec<String>) {
    let items = open
        .into_iter()
        .map(|port| {
            MenuItem::new(format!("Kill port {port}")).on_select(move || {
                let port = port.clone();
                spawn(async move {
                    git::off_thread(move || ports::kill(&port)).await;
                    live.scan_ports();
                });
            })
        })
        .collect();
    let _ = popup_menu(items);
}

/// The dimmed line under a row's name (`renderRow`'s second line): its
/// repo, a worktree's mark, its branch when the name and repo don't
/// already say it, and how long ago its folder was made. `indent` lines it
/// up under the name.
fn subtext(shell: Shell, tab: &Tab, indent: f32) -> impl IntoElement {
    let (path, name) = (tab.path.clone(), tab.name.clone());
    // Birth time where the file system keeps one, else the last change.
    let created = std::fs::metadata(&path).ok().and_then(|m| m.created().or_else(|_| m.modified()).ok());
    let age = created.and_then(|t| t.elapsed().ok()).map(|d| format_age(d.as_millis() as u64));
    let faint = move || theme().muted_foreground.opacity(0.6);
    dynamic(move || {
        let status = shell.statuses.with(|m| m.get(&path).cloned());
        let repo = status.as_ref().and_then(|s| s.repo.clone());
        let worktree = status.as_ref().is_some_and(|s| s.worktree);
        let branch = status.as_ref().map(|s| s.branch.clone()).filter(|b| {
            !b.is_empty() && (worktree || (*b != name && Some(b) != repo.as_ref()))
        });
        if status.is_none() && age.is_none() {
            return None;
        }
        let small = |t: String| text(t).text_size(px(10.0)).line_height(px(14.0)).text_color(faint);
        let mut line = view().flex_row().items_center().gap(px(6.0)).min_w(px(0.0)).pl(px(indent)).pr_1().pb_1();
        if let Some(repo) = repo.clone() {
            line = line.child(small(repo).number_of_lines(1).flex_shrink(1.0).min_w(px(0.0)));
        }
        if branch.is_some() || worktree {
            line = line.child(small("·".into()).flex_shrink(0.0));
        }
        if worktree {
            line = line.child(text("⎇").text_size(px(10.0)).line_height(px(14.0)).flex_shrink(0.0).text_color(rgb(0xfbbf24)));
        }
        if let Some(branch) = branch.clone() {
            // The tail (`-1423`, `/fix`) stays whole; the head is what cuts.
            let (head, tail) = split_branch(&branch);
            line = line.child(
                view()
                    .flex_row()
                    .min_w(px(0.0))
                    .flex_shrink(100.0)
                    .child(small(head).number_of_lines(1).min_w(px(0.0)).flex_shrink(1.0))
                    .child(small(tail).flex_shrink(0.0)),
            );
        }
        if let Some(age) = age.clone() {
            let lead = if repo.is_some() || branch.is_some() || worktree { "· " } else { "" };
            line = line.child(small(format!("{lead}{age}")).flex_shrink(0.0));
        }
        Some(line)
    })
}

/// A branch as a head that may be cut and a tail kept whole (`splitBranch`):
/// from the last `-` or `/` when what follows is 3–16 characters.
fn split_branch(b: &str) -> (String, String) {
    match b.rfind(['-', '/']).filter(|i| *i > 0) {
        Some(i) if (3..=16).contains(&b[i..].chars().count()) => (b[..i].to_owned(), b[i..].to_owned()),
        _ => (b.to_owned(), String::new()),
    }
}

/// How long ago, as Bolt writes it (`formatAge`): `45s`, `12m`, `3h 5m`,
/// `2d 4h`, `5mo 9d`, `1y 2mo` (months of 30 days, years of 12 months).
pub(crate) fn format_age(ms: u64) -> String {
    let sec = ms / 1000;
    if sec < 60 {
        return format!("{sec}s");
    }
    let min = sec / 60;
    if min < 60 {
        return format!("{min}m");
    }
    let pair = |big: u64, a: &str, small: u64, b: &str| if small > 0 { format!("{big}{a} {small}{b}") } else { format!("{big}{a}") };
    let hr = min / 60;
    if hr < 24 {
        return pair(hr, "h", min % 60, "m");
    }
    let day = hr / 24;
    if day < 30 {
        return pair(day, "d", hr % 24, "h");
    }
    let mon = day / 30;
    if mon < 12 {
        return pair(mon, "mo", day % 30, "d");
    }
    pair(mon / 12, "y", mon % 12, "mo")
}

/// The dot left of the name (`BranchStatusIndicator`): red on a conflict,
/// green with a remote, amber without; filled when clean, a ring when
/// dirty; none outside git.
fn status_dot(shell: Shell, path: String) -> impl IntoElement {
    let status = move || shell.statuses.with(|m| m.get(&path).cloned());
    let color = {
        let status = status.clone();
        move || match status() {
            Some(s) if s.conflict => rgb(0xef4444),
            Some(s) if s.has_remote => rgb(0x22c55e),
            Some(_) => rgb(0xf59e0b),
            None => rgb(0x000000),
        }
    };
    let (fill, ring, shown) = (color.clone(), color, status.clone());
    view()
        .w(px(8.0))
        .h(px(8.0))
        .rounded_full()
        .border_2()
        .border_color(move || Hsla::from(ring()))
        .bg(move || {
            let c: Hsla = fill().into();
            if status().is_some_and(|s| s.dirty) { c.opacity(0.0) } else { c }
        })
        .opacity(move || if shown().is_some() { 1.0_f32 } else { 0.0 })
}

/// A chip per terminal open in `path`, led by its status square
/// (`TerminalChip`), then the PR chip; a terminal chip's click shows it.
fn chips(shell: Shell, live: Live, path: String, list: ElementRef) -> impl IntoElement {
    let pr_path = path.clone();
    let items = move || shell.terms_of(&path);
    // Inside the row's card (its 8pt margin), lined up under the name.
    view()
        .pl(px(22.0))
        .pr_2()
        .child(each(items, |t| t.id, move |term, _| {
            let id = term.get_untracked().id;
            let cwd = term.get_untracked().cwd;
            // Shown in its column (Bolt's chip dot is the active column's).
            let shown = move || shell.shown(&cwd) == Some(id);
            let go = term.get_untracked().cwd;
            let open = move || {
                shell.show(id);
                commands::submit(shell, format!("cd {go} --absolute"));
            };
            let info = move || live.terms.with(|m| m.get(&id).cloned()).unwrap_or_default();
            // Its label shrinks and ends in "…" in the card, as the repo line
            // above it does: unshrinkable, a long title runs past the card.
            let chip = view()
                .flex_row()
                .items_center()
                .gap_1()
                .h(px(20.0))
                .min_w(px(0.0))
                .on_press(open.clone())
                .child(pulsing(
                    badge(
                        move || {
                            let i = info();
                            status_color(&i.status, i.needs_input)
                        },
                        "›",
                    ),
                    move || {
                        let i = info();
                        (i.status == "agent" || i.needs_input).then_some(if i.needs_input { WAITING_CYAN } else { PULSE })
                    },
                ))
                .child(
                    // Its title and what its agent says; else what it is doing.
                    text(move || {
                        let (t, i) = (term.get(), info());
                        let shown = chip_label(&t.title, &i.label);
                        if !shown.is_empty() {
                            shown
                        } else if i.status == "agent" {
                            format!("{} working", i.agent.as_deref().unwrap_or("agent"))
                        } else if i.status == "working" {
                            "working".into()
                        } else {
                            "idle".into()
                        }
                    })
                    .text_xs()
                    .number_of_lines(1)
                    .flex_shrink(1.0)
                    .min_w(px(0.0))
                    .text_color(move || if shown() { theme().sidebar_foreground } else { theme().muted_foreground }),
                );
            hints::hint(chip, "workspaces", Opts::default(), Some(list), open)
        }))
        .child(pr_chip(live, pr_path))
}

/// The workspace's open PR (`renderPr`): its checks' color, number and
/// title. Only shown: a PR has no tab to go to.
fn pr_chip(live: Live, path: String) -> impl IntoElement {
    dynamic(move || {
        let pr = live.prs.with(|m| m.get(&path).and_then(|(_, pr)| pr.clone()))?;
        let color = match pr.badge() {
            Badge::Failed => crate::theme::BOLT_THEME.vars.error,
            Badge::Running => 0xe5c07b,
            Badge::Approved => 0x3fb950,
            Badge::Idle => 0x238636,
        };
        let chip = view()
            .flex_row()
            .items_center()
            .gap_1()
            .h(px(20.0))
            .min_w(px(0.0))
            .child(badge(move || rgb(color).into(), "#"))
            .child(
                text(format!("#{} {}", pr.number, pr.title))
                    .text_xs()
                    .number_of_lines(1)
                    .flex_shrink(1.0)
                    .min_w(px(0.0))
                    .text_color(move || theme().muted_foreground),
            );
        Some(chip)
    })
}

/// Bolt's right-click menu (Pin / Unpin, Hide) plus the ⌘; overlay's moves
/// and group cycling, applied to the clicked tab, and Close.
fn tab_menu(hub: Hub, path: String, pinned: bool) {
    let shell = hub.shell;
    let mut at = shell.settings.get_untracked();
    at.directory = path.clone();
    let act = move |action: Action| {
        let path = path.clone();
        move || {
            let mut at = shell.settings.get_untracked();
            at.directory = path.clone();
            let (tabs, pinned) = workspace::apply(&at, action);
            shell.update_settings(|s| {
                s.tabs = tabs;
                s.pinned = pinned;
            });
        }
    };
    let close_path = at.directory.clone();
    let items = vec![
        MenuItem::new(if pinned { "Unpin" } else { "Pin" }).on_select(act(Action::Pin)),
        MenuItem::new("Hide").on_select(act(Action::Hide)),
        MenuItem::separator(),
        MenuItem::new("Move Up")
            .disabled(!workspace::is_available(&at, Action::MoveUp))
            .on_select(act(Action::MoveUp)),
        MenuItem::new("Move Down")
            .disabled(!workspace::is_available(&at, Action::MoveDown))
            .on_select(act(Action::MoveDown)),
        MenuItem::new("Next Group")
            .disabled(!workspace::is_available(&at, Action::Group))
            .on_select(act(Action::Group)),
        MenuItem::separator(),
        MenuItem::new("Close…").on_select(move || request_close_tab(hub, close_path.clone())),
    ];
    let _ = popup_menu(items);
}

/// Bolt's close-tab choice in the palette (`requestCloseTab`): `a` closes
/// the tab, kills its terminals and deletes its worktree and branch; `s`
/// closes the tab and keeps its files and terminals; `d` cancels.
pub fn request_close_tab(hub: Hub, path: String) {
    let shell = hub.shell;
    let name = shell.settings.with_untracked(|s| {
        s.pinned.iter().chain(&s.tabs).find(|t| t.path == path).map(|t| t.name.clone())
    });
    let Some(name) = name else { return };
    let option = |key: &str, label: &str, hint: &str, tone: &str| ChoiceOption {
        key: key.into(),
        label: label.into(),
        hint: Some(hint.into()),
        tone: Some(tone.into()),
    };
    hub.ask_choice(
        "Close tab".into(),
        Some(name),
        vec![
            option("a", "Close all", "Close the tab, its files & terminals, and delete its worktree & branch", "var(--theme-error)"),
            option("s", "Close tab", "Remove the tab but keep its files & terminals", "var(--theme-accent)"),
            option("d", "Cancel", "Keep the tab", "var(--theme-fg-muted)"),
        ],
        move |key| match key.as_deref() {
            Some("a") => close_tab_all(shell, path),
            Some("s") => drop_tab(shell, &path),
            _ => {}
        },
    );
}

fn drop_tab(shell: Shell, path: &str) {
    shell.update_settings(|s| {
        s.tabs.retain(|t| t.path != path);
        s.pinned.retain(|t| t.path != path);
    });
}

/// `closeTabAll` + `deleteTabWorktree`: the tab and its columns' tabs go,
/// then a worktree's checkout and branch; a failed delete puts the tab
/// back.
fn close_tab_all(shell: Shell, path: String) {
    let removed = shell.settings.with_untracked(|s| (s.tabs.clone(), s.pinned.clone()));
    drop_tab(shell, &path);
    for id in shell.tabs_of(&path) {
        shell.close_tab(id);
    }
    spawn(async move {
        let at = path.clone();
        let deleted = git::off_thread(move || -> git::Result<()> {
            let Ok(ctx) = git::worktree_context(&at) else { return Ok(()) };
            if !ctx.is_worktree {
                return Ok(());
            }
            let branch = git::current_branch(&at)?;
            git::delete_worktree_and_branch(&ctx.main_repo_path, &at, &branch)
        })
        .await;
        if let Err(e) = deleted {
            eprintln!("rivet: close all: worktree delete failed: {e}");
            shell.update_settings(|s| (s.tabs, s.pinned) = removed);
        }
        shell.refresh_status(path);
    });
}

/// A terminal chip's text: its title, then what its agent's label adds. The
/// label (`session title · last prompt`) repeats the session title omp ends
/// the terminal's title with (`π > title`), so that part goes; a prompt that
/// merely appears inside the title stays.
fn chip_label(title: &str, label: &str) -> String {
    let title = title.trim();
    let mut parts: Vec<&str> = vec![title];
    parts.extend(label.split(" · ").map(str::trim).filter(|p| !title.ends_with(p)));
    parts.retain(|p| !p.is_empty());
    parts.join(" · ")
}

/// The strip's foot: the current directory's `branch · repo`, and at its
/// right the lines its branch changed ([`loc_badge`]).
fn footer(shell: Shell) -> impl IntoElement {
    let counts = loc_counts(shell);
    view()
        .flex_row()
        .items_center()
        .gap_2()
        .px_3()
        .py_2()
        .child(
            text(move || {
                let dir = shell.directory();
                match shell.statuses.with(|m| m.get(&dir).cloned()) {
                    Some(s) => format!("{} · {}", s.branch, s.repo.unwrap_or_default()),
                    None => dir,
                }
            })
            .flex_1()
            .text_xs()
            .number_of_lines(1)
            .text_color(move || theme().muted_foreground),
        )
        .child({
            // `Some(None)` while the directory's first count runs; a memo, so
            // the spinner isn't made again on every status read.
            let shown = computed(move || {
                let dir = shell.directory();
                shell.statuses.with(|m| m.contains_key(&dir)).then(|| {
                    counts.with(|c| c.as_ref().filter(|(d, _)| *d == dir).map(|(_, loc)| loc.clone()))
                })
            });
            dynamic(move || shown.get().and_then(loc_badge))
        })
}

/// How often the footer looks for a new directory or branch to count and
/// at whether the watcher saw a change (a flag; no git), and how often it
/// looks at the working tree and HEAD when the directory couldn't be
/// watched.
const LOC_TICK: Duration = Duration::from_millis(250);
const LOC_POLL: Duration = Duration::from_secs(2);

/// The current directory's line counts (`useGitStatus`' `locStats`), with
/// the directory they were counted for. Counted again when the directory
/// or its branch changes, and when its working tree or HEAD does: a file
/// watcher ([`watch`]) flags each change, looked at every [`LOC_TICK`]; a
/// directory it can't watch is looked at every [`LOC_POLL`] instead. A
/// directory that isn't a repo is never counted. The counts of the
/// directory left stay until the new one's land, which the footer shows as
/// counting.
fn loc_counts(shell: Shell) -> Signal<Option<(String, git::Loc)>> {
    let counts = signal(None::<(String, git::Loc)>);
    let task = spawn(async move {
        // The directory, its branch and what its tree and HEAD looked like
        // at the last count.
        let mut seen: Option<(String, String, String)> = None;
        let mut looked: Option<Instant> = None;
        // The directory watched, and its watcher (none when watching failed).
        let mut watching: Option<(String, Option<notify::RecommendedWatcher>)> = None;
        let changed = Arc::new(AtomicBool::new(false));
        loop {
            let dir = shell.directory_untracked();
            if let Some(branch) = shell.statuses.with_untracked(|m| m.get(&dir).map(|s| s.branch.clone())) {
                if watching.as_ref().is_none_or(|(d, _)| *d != dir) {
                    drop(watching.take());
                    let (at, flag) = (dir.clone(), changed.clone());
                    watching = Some((dir.clone(), git::off_thread(move || watch(&at, flag)).await));
                }
                let watched = watching.as_ref().is_some_and(|(_, w)| w.is_some());
                let moved = seen.as_ref().is_none_or(|(d, b, _)| *d != dir || *b != branch);
                let touched = changed.swap(false, Ordering::Relaxed);
                if moved || touched || (!watched && looked.is_none_or(|t| t.elapsed() >= LOC_POLL)) {
                    looked = Some(Instant::now());
                    let at = dir.clone();
                    let print = git::off_thread(move || {
                        let head = git::git(&["rev-parse", "HEAD"], &at).unwrap_or_default();
                        changes::fingerprint(&at) + &head
                    })
                    .await;
                    let now = Some((dir.clone(), branch, print));
                    if now != seen {
                        seen = now;
                        let at = dir.clone();
                        let loc = git::off_thread(move || git::loc_stats(&at)).await;
                        counts.set(Some((dir, loc)));
                    }
                }
            }
            timer(LOC_TICK).await;
        }
    });
    cleanup(move || task.cancel());
    counts
}

/// Watches `dir` and everything under it, and its git directories where
/// they live elsewhere (a linked worktree's), raising `changed` on each
/// event that can move its line counts: anything in the working tree, and
/// in a git directory the index, HEAD and refs (a stage, commit or
/// checkout). The rest of git's files and reads are left out, so the
/// counting's own git calls don't keep raising it. None when the watch
/// can't be set up.
fn watch(dir: &str, changed: Arc<AtomicBool>) -> Option<notify::RecommendedWatcher> {
    use notify::Watcher;
    let counts = |path: &Path| {
        let mut parts = path.components().map(|c| c.as_os_str());
        if !parts.by_ref().any(|p| p == ".git") {
            return true;
        }
        let named = path.file_name().is_some_and(|n| n == "index" || n == "HEAD" || n == "packed-refs");
        named || parts.any(|p| p == "refs")
    };
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        let Ok(event) = event else { return };
        if event.need_rescan() || (!event.kind.is_access() && (event.paths.is_empty() || event.paths.iter().any(|p| counts(p)))) {
            changed.store(true, Ordering::Relaxed);
        }
    })
    .ok()?;
    let root = Path::new(dir);
    watcher.watch(root, notify::RecursiveMode::Recursive).ok()?;
    let git_dirs = git::git(&["rev-parse", "--path-format=absolute", "--git-dir", "--git-common-dir"], dir).unwrap_or_default();
    for git_dir in git_dirs.lines().map(Path::new).filter(|d| !d.starts_with(root)) {
        watcher.watch(git_dir, notify::RecursiveMode::Recursive).ok()?;
    }
    Some(watcher)
}

/// `SidebarFooter`'s line count: `+added / -deleted` since the branch's
/// base, the uncommitted in parentheses after each when there are both,
/// else only the uncommitted; nothing when no line changed, a spinner
/// (`None`) while counting.
fn loc_badge(loc: Option<git::Loc>) -> Option<AnyElement> {
    let v = &crate::theme::BOLT_THEME.vars;
    let Some(loc) = loc else {
        return Some(spinner(v.fg_faint, v.fg_muted).tooltip(crate::pr_bar::tip("Counting changed lines…")).into_element());
    };
    if loc.is_empty() {
        return None;
    }
    let ((main_add, main_del), both) = loc.lead();
    let (c, u) = (loc.committed, loc.uncommitted);
    let base = loc.base.as_deref().unwrap_or_default();
    let tip = match (both, c != (0, 0)) {
        (true, _) => format!("Since {base}: +{} / -{} · Uncommitted: +{} / -{}", c.0, c.1, u.0, u.1),
        (false, true) => format!("+{} / -{} since {base}", c.0, c.1),
        (false, false) => format!("Uncommitted: +{} / -{}", u.0, u.1),
    };
    let num = |t: String, c: u32| text(t).font_family("ui-monospace").text_size(px(12.0)).tabular_nums().text_color(rgb(c));
    let mut row = view().flex_row().items_center().gap_1().flex_shrink(0.0).child(num(format!("+{main_add}"), v.success));
    if both {
        row = row.child(num(format!("(+{})", u.0), v.fg_faint));
    }
    row = row.child(num("/".into(), v.fg_faint)).child(num(format!("-{main_del}"), v.error));
    if both {
        row = row.child(num(format!("(-{})", u.1), v.fg_faint));
    }
    Some(row.tooltip(crate::pr_bar::tip(&tip)).into_element())
}

/// `SpinnerIcon` with `animate-spin`: a ring, one quarter bright, turning
/// once a second until it goes.
fn spinner(ring: u32, quarter: u32) -> View {
    let angle = animated_value(0.0);
    let spin = Animation::timing(&angle, 360.0, Timing::new(1000.0, Easing::Linear)).looped(None);
    let _ = spin.start();
    cleanup(move || spin.stop());
    view()
        .w(px(12.0))
        .h(px(12.0))
        .flex_shrink(0.0)
        .rounded_full()
        .border_2()
        .border_color(rgb(ring))
        .border_t_color(rgb(quarter))
        .bind_rotate(&angle)
}

fn parse_hex(s: &str) -> Option<Rgba> {
    let hex = s.strip_prefix('#')?;
    (hex.len() == 6).then(|| u32::from_str_radix(hex, 16).ok().map(rgb)).flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ring_circles_a_12pt_square() {
        let dots = ring(12.0, 12.0, 3.0);
        // Three on each corner's arc, one on the middle of each side.
        assert_eq!(dots.len(), 16);
        // Every dot's middle is `GAP` out from the rounded square's edge.
        for (x, y) in dots {
            let (cx, cy) = (x + DOT / 2.0 - 6.0, y + DOT / 2.0 - 6.0);
            let outside = (cx.abs() - 3.0).max(0.0).hypot((cy.abs() - 3.0).max(0.0)) - 3.0;
            assert!((outside - GAP).abs() < 0.01, "({cx}, {cy}) is {outside} out");
        }
    }

    #[test]
    fn a_chip_shows_the_session_title_once() {
        let title = "π > 🐚 NSH: Port Bolt sidebar";
        assert_eq!(chip_label(title, "🐚 NSH: Port Bolt sidebar · fix the overflow"), "π > 🐚 NSH: Port Bolt sidebar · fix the overflow");
        assert_eq!(chip_label(title, "🐚 NSH: Port Bolt sidebar"), title);
        assert_eq!(chip_label("", "a · b"), "a · b");
        assert_eq!(chip_label("zsh", ""), "zsh");
        assert_eq!(chip_label("π > fix the build", "fix the build · fix"), "π > fix the build · fix");
        assert_eq!(chip_label("", ""), "");
    }
}
