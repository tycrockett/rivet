//! Bolt's PR card (`PrStatusBox`) as a header bar above the terminals: the
//! current workspace's open PR on one line (its number, checks, review,
//! comments: new from others since the branch's last push, unresolved threads, total)
//! and the merge button. A press on the line, or `pr.toggle` (⇧⌘P), opens
//! the rest in a dialog: title, link, reviewers, each check, each comment.
//! Nothing shows for a workspace without an open PR, except a PR just
//! merged from here, kept until dismissed or the workspace changes.
//!
//! The detail is three `gh` round trips ([`prs::fetch_detail`]), asked for
//! the current workspace only: when its git state is read again (a switch,
//! each command), every 15 s while checks run, every minute otherwise, and
//! on a press of the bar's refresh button.

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    rc::Rc,
    time::Duration,
};

use nsh_core::{prelude::*, time::unix_ms};

use crate::{
    commands, git,
    prs::{self, CheckCounts, CheckState, Comment, CommentCounts, Detail, Pr},
    sidebar::format_age,
    state::Shell,
    theme::BOLT_THEME,
};

// The strip's PR chip colors.
const GREEN: u32 = 0x3fb950;
const YELLOW: u32 = 0xe5c07b;
const MONO: &str = "ui-monospace";
/// How long the merge button waits for its second press (`ARM_MS`).
const ARM: Duration = Duration::from_secs(3);

fn red() -> u32 {
    BOLT_THEME.vars.error
}

#[derive(Clone, Copy)]
pub struct PrBar {
    shell: Shell,
    /// `provide`'s owner, for the merge and the timers: a press listener's
    /// own is the run of the region that drew its button, which the press
    /// itself ends (`armed`, `merging` draw the button again), cancelling
    /// what it spawned.
    owner: Owner,
    /// Each workspace's PR as last fetched, with the branch it was asked for.
    cache: Signal<HashMap<String, (String, Option<Detail>)>>,
    /// The workspaces with a fetch out: one swallows the next asks, and the
    /// refresh button shows it.
    asking: Signal<HashSet<String>>,
    /// Bumped by the refresh button: the asking effect reads it.
    refresh: Signal<u64>,
    /// A PR merged from here: the next fetch finds no open PR, so it shows
    /// from this copy until dismissed or the workspace changes.
    merged: Signal<Option<(String, Detail)>>,
    /// What the bar shows: the current workspace's PR, and whether it is
    /// the merged copy.
    shown: Memo<Option<(Detail, bool)>>,
    open: Signal<bool>,
    armed: Signal<bool>,
    /// Bumped by each arming: only the latest one's timer disarms.
    arm_tick: Signal<u64>,
    merging: Signal<bool>,
    /// The last failed merge's stderr.
    error: Signal<String>,
    copied: Signal<bool>,
}

pub fn provide(shell: Shell) -> PrBar {
    let cache = signal(HashMap::<String, (String, Option<Detail>)>::new());
    let merged = signal(None::<(String, Detail)>);
    let shown = computed(move || {
        let dir = shell.directory();
        if let Some(d) = merged.with(|m| m.as_ref().filter(|(p, _)| *p == dir).map(|(_, d)| d.clone())) {
            return Some((d, true));
        }
        let branch = shell.statuses.with(|m| m.get(&dir).filter(|s| s.has_remote).map(|s| s.branch.clone()))?;
        cache.with(|m| m.get(&dir).filter(|(b, _)| *b == branch).and_then(|(_, d)| d.clone())).map(|d| (d, false))
    });
    let bar = PrBar {
        shell,
        owner: Owner::current(),
        cache,
        asking: signal(HashSet::new()),
        refresh: signal(0),
        merged,
        shown,
        open: signal(false),
        armed: signal(false),
        arm_tick: signal(0),
        merging: signal(false),
        error: signal(String::new()),
        copied: signal(false),
    };
    bar.poll();
    // A workspace switch drops what belonged to the one left.
    let last = RefCell::new(String::new());
    effect(move || {
        let dir = shell.directory();
        if *last.borrow() == dir {
            return;
        }
        *last.borrow_mut() = dir.clone();
        untracked(|| {
            bar.open.set(false);
            bar.armed.set(false);
            bar.error.set(String::new());
            if bar.merged.with(|m| m.as_ref().is_some_and(|(p, _)| *p != dir)) {
                bar.merged.set(None);
            }
        });
    });
    bar
}

impl PrBar {
    /// `pr.toggle`: the dialog, while there is a PR to show.
    pub fn toggle(self) {
        if self.shown.with_untracked(Option::is_some) {
            self.open.update(|o| *o = !*o);
        }
    }

    fn poll(self) {
        let (shell, cache, asking, refresh) = (self.shell, self.cache, self.asking, self.refresh);
        // The bar's owner, not the asking effect's: the effect runs again
        // on every git state read, which would cancel a fetch in flight and
        // leave its workspace in `asking` for good.
        let owner = self.owner;
        let ask = Rc::new(move |dir: String, branch: String| {
            // An ask already out for a workspace swallows the next ones: the
            // git state is read again in bursts.
            if asking.with_untracked(|a| a.contains(&dir)) {
                return;
            }
            asking.update(|a| {
                a.insert(dir.clone());
            });
            owner.run(|| spawn(async move {
                let at = dir.clone();
                let detail = git::off_thread(move || prs::fetch_detail(&at)).await;
                asking.update(|a| {
                    a.remove(&dir);
                });
                cache.update(|m| {
                    m.insert(dir, (branch, detail));
                });
            }));
        });
        let current = move || {
            let dir = shell.directory();
            shell.statuses.with(|m| m.get(&dir).filter(|s| s.has_remote).map(|s| (dir.clone(), s.branch.clone())))
        };
        {
            let ask = ask.clone();
            // Every read of the git state, the current workspace's or not,
            // and each press of the refresh button.
            effect(move || {
                refresh.get();
                if let Some((dir, branch)) = current() {
                    untracked(|| ask(dir, branch));
                }
            });
        }
        {
            let ask = ask.clone();
            set_interval(Duration::from_secs(15), move || {
                let running = self.shown.with_untracked(|s| s.as_ref().is_some_and(|(d, merged)| !merged && d.pr.running()));
                if let Some((dir, branch)) = untracked(current).filter(|_| running) {
                    ask(dir, branch);
                }
            });
        }
        set_interval(Duration::from_secs(60), move || {
            if let Some((dir, branch)) = untracked(current) {
                ask(dir, branch);
            }
        });
    }

    /// Admin when branch protection blocks the merge, or gh asked for it.
    fn admin(self, pr: &Pr) -> bool {
        pr.needs_admin() || self.error.with(|e| prs::merge_error(e).0)
    }

    /// The first press arms the button for [`ARM`]; the second merges.
    fn press_merge(self, detail: &Detail) {
        if self.merging.get_untracked() {
            return;
        }
        if !self.armed.get_untracked() {
            let tick = self.arm_tick.get_untracked() + 1;
            self.arm_tick.set(tick);
            self.armed.set(true);
            self.owner.run(|| {
                set_timeout(ARM, move || {
                    if self.arm_tick.get_untracked() == tick {
                        self.armed.set(false);
                    }
                })
            });
            return;
        }
        let admin = untracked(|| self.admin(&detail.pr));
        self.armed.set(false);
        self.merging.set(true);
        self.error.set(String::new());
        let dir = self.shell.directory();
        let detail = detail.clone();
        self.owner.run(|| {
            spawn(async move {
                let at = dir.clone();
                let result = git::off_thread(move || prs::merge(&at, admin)).await;
                self.merging.set(false);
                match result {
                    Ok(()) => {
                        self.merged.set(Some((dir.clone(), detail)));
                        self.shell.refresh_status(dir);
                    }
                    Err(e) => self.error.set(e),
                }
            })
        });
    }

    fn copy_link(self, url: String) {
        set_clipboard(url);
        self.copied.set(true);
        self.owner.run(|| set_timeout(Duration::from_millis(1500), move || self.copied.set(false)));
    }
}

/// The bar, and the dialog while open or going.
pub fn bar(bar: PrBar) -> impl IntoElement {
    let open = computed(move || bar.open.get() && bar.shown.with(Option::is_some));
    let motion = DialogMotion::new(bar.owner, open);
    view()
        .flex_shrink(0.0)
        .child(dynamic(move || {
            let (detail, _) = bar.shown.get()?;
            Some(untracked(|| line(bar, &detail)))
        }))
        .child(when(motion.mounted, move || dialog(bar, motion)))
}

fn line(bar: PrBar, d: &Detail) -> View {
    let v = &BOLT_THEME.vars;
    let summary = view()
        .flex_row()
        .items_center()
        .gap(px(16.0))
        .flex_1()
        .min_w(px(0.0))
        .h_full()
        .px_2()
        .overflow_hidden()
        .on_press(move || bar.open.set(true))
        // The title gives way first: the segments keep their size.
        .child(
            view()
                .flex_shrink(1.0)
                .min_w(px(0.0))
                .child(text(d.pr.title.clone()).text_size(px(12.0)).font_weight(FontWeight::MEDIUM).number_of_lines(1).text_color(rgb(v.fg))),
        )
        .child(seg("⑂", v.accent, vec![(format!("#{}", d.pr.number), v.fg)]));
    view()
        .flex_row()
        .items_center()
        .h(px(38.0))
        .py(px(6.0))
        .pr_2()
        .gap_3()
        .bg(rgb(BOLT_THEME.chrome()))
        .border_b_1()
        .border_color(rgb(v.border))
        .child(refresh_button(bar))
        .child(states(summary, d))
        .child(merge_error(bar).max_w(px(360.0)))
        .child(controls(bar))
}

/// `↻`, `…` while the current workspace's PR is being asked for.
fn refresh_button(bar: PrBar) -> View {
    let v = &BOLT_THEME.vars;
    let loading = move || {
        let dir = bar.shell.directory();
        bar.asking.with(|a| a.contains(&dir))
    };
    view()
        .flex_shrink(0.0)
        .items_center()
        .justify_center()
        .w(px(22.0))
        .h(px(22.0))
        .ml_2()
        .rounded(px(4.0))
        .hover(|s| s.bg(rgb(v.bg_hover)))
        .on_press(move || bar.refresh.update(|n| *n += 1))
        .child(text(move || if loading() { "…" } else { "↻" }).text_size(px(12.0)).font_family(MONO).text_color(rgb(v.fg_muted)))
        .tooltip(tip("Refresh the pull request"))
}

/// The checks, the review, the comments, conflicts and draft: the bar's
/// segments, and the dialog's.
fn states(row: View, d: &Detail) -> View {
    let pr = &d.pr;
    let muted = BOLT_THEME.vars.fg_muted;
    row.child(checks_seg(pr.check_counts()))
        .child(seg("◉", review_color(pr), vec![(pr.review_label(), BOLT_THEME.vars.fg)]))
        .child(comments_seg(d.counts()))
        .when(pr.mergeable == "CONFLICTING", |r| r.child(seg("⚠", red(), vec![("Conflicts".into(), red())])))
        .when(pr.is_draft, |r| r.child(seg("◇", muted, vec![("Draft".into(), muted)])))
}

fn mono(s: impl Into<SharedString>, c: u32) -> Text {
    text(s.into()).text_size(px(11.0)).font_family(MONO).number_of_lines(1).text_color(rgb(c))
}

/// A glyph and its words, which keep their size.
fn seg(glyph: &str, glyph_color: u32, parts: Vec<(String, u32)>) -> View {
    let row = view().flex_row().items_center().gap(px(5.0)).flex_shrink(0.0).child(mono(glyph.to_owned(), glyph_color));
    parts.into_iter().fold(row, |r, (s, c)| r.child(mono(s, c)))
}

fn checks_seg(c: CheckCounts) -> View {
    let (fg, muted) = (BOLT_THEME.vars.fg, BOLT_THEME.vars.fg_muted);
    if c.running > 0 {
        seg("◐", YELLOW, vec![(c.running.to_string(), fg), ("running".into(), muted)])
    } else if c.failed > 0 {
        seg("✗", red(), vec![(c.failed.to_string(), fg), ("failed".into(), muted)])
    } else if c.passed > 0 {
        seg("✓", GREEN, vec![("Jobs".into(), fg)])
    } else {
        seg("○", muted, vec![("No jobs".into(), muted)])
    }
}

fn review_color(pr: &Pr) -> u32 {
    match pr.review_decision.as_deref() {
        Some("APPROVED") => GREEN,
        Some("CHANGES_REQUESTED") => red(),
        Some("REVIEW_REQUIRED") => YELLOW,
        _ => BOLT_THEME.vars.fg_muted,
    }
}

/// `◌ 2 new  3 unresolved  12 total`; no `new` while the last push is
/// unknown.
fn comments_seg(c: CommentCounts) -> View {
    let v = &BOLT_THEME.vars;
    let pair = |n: usize, hot: u32, label: &str| {
        view()
            .flex_row()
            .gap(px(4.0))
            .child(mono(n.to_string(), if n > 0 { hot } else { v.fg }))
            .child(mono(label.to_owned(), v.fg_muted))
    };
    view()
        .flex_row()
        .items_center()
        .gap(px(10.0))
        .flex_shrink(0.0)
        .child(mono("◌", v.fg_muted))
        .when_some(c.new, |r, n| r.child(pair(n, v.accent, "new")))
        .child(pair(c.unresolved, YELLOW, "unresolved"))
        .child(pair(c.total, v.fg, "total"))
}

fn merge_error(bar: PrBar) -> View {
    view().flex_shrink(1.0).min_w(px(0.0)).child(text(move || bar.error.with(|e| prs::merge_error(e).1)).text_size(px(11.0)).font_family(MONO).number_of_lines(1).text_color(rgb(red())))
}

fn button(label: &str, primary: bool, enabled: bool, on_press: impl Fn() + 'static) -> View {
    let v = &BOLT_THEME.vars;
    view()
        .flex_row()
        .items_center()
        .flex_shrink(0.0)
        .h(px(18.0))
        .px_2()
        .rounded(px(4.0))
        .border_1()
        .border_color(rgb(if primary { v.accent } else { v.border }))
        .bg(rgb(if primary { v.accent } else { v.bg_surface }))
        .opacity(if enabled { 1.0 } else { 0.4 })
        .when(enabled && !primary, |b| b.hover(|s| s.bg(rgb(v.bg_hover))))
        .when(enabled, |b| b.on_press(on_press))
        .child(mono(label.to_owned(), if primary { v.bg } else { v.fg_muted }))
}

/// A tooltip in Bolt's widget look.
pub fn tip(s: &str) -> AnyElement {
    let v = &BOLT_THEME.vars;
    view()
        .px_2()
        .py_1()
        .rounded(px(4.0))
        .border_1()
        .border_color(rgb(v.border))
        .bg(rgb(v.bg_widget))
        .child(text(s.to_owned()).text_size(px(11.0)).text_color(rgb(v.fg)))
        .into_element()
}

/// The merge button (`merge` → `confirm` → `merging…`), or, after a merge,
/// `clean up` (`delete`: the worktree, the branch, its tab) and dismiss.
fn controls(bar: PrBar) -> AnyElement {
    dynamic(move || {
        let (d, merged) = bar.shown.get()?;
        let row = view().flex_row().items_center().gap_2().flex_shrink(0.0);
        if merged {
            return Some(
                row.child(mono("✓ merged", GREEN))
                    .child(
                        button("clean up", false, true, move || {
                            // The bar's owner: dismissing redraws this button,
                            // which would cancel the command it spawned. The
                            // PR's branch by name: after gh's own local delete
                            // the checkout stands on the default branch.
                            let branch = d.pr.head_ref_name.clone();
                            bar.owner.run(|| commands::submit(bar.shell, format!("delete {branch}")));
                            bar.merged.set(None);
                        })
                        .tooltip(tip("Remove the worktree, delete the branch and close its workspace tab")),
                    )
                    .child(button("×", false, true, move || bar.merged.set(None)).tooltip(tip("Dismiss"))),
            );
        }
        let (armed, merging, admin) = (bar.armed.get(), bar.merging.get(), bar.admin(&d.pr));
        let label = match (merging, armed, admin) {
            (true, ..) => "merging…",
            (_, true, true) => "confirm admin",
            (_, true, false) => "confirm",
            (_, false, true) => "admin merge",
            (_, false, false) => "merge",
        };
        let reason = match d.pr.blocked_reason() {
            "" if armed => "Click again to merge",
            "" if admin => "Squash merge with admin privileges, bypassing branch protection",
            "" => "Squash merge and delete the branch",
            blocked => blocked,
        };
        let enabled = d.pr.can_merge() && !merging;
        Some(row.child(button(label, armed, enabled, move || bar.press_merge(&d)).tooltip(tip(reason))))
    })
}

/// How long the dialog takes to come in and to go (`nsh_base::sheet`'s).
const DIALOG_IN_MS: f32 = 280.0;
const DIALOG_OUT_MS: f32 = 200.0;

/// The dialog's own motion, as `nsh_base::sheet` makes its own: the
/// modal layer moves as one, so its slide would carry the backdrop up with
/// the panel. The backdrop fades while the panel slides down from above
/// the window, and back; the dialog stays in the view until it went.
#[derive(Clone, Copy)]
struct DialogMotion {
    /// In the view: while open, and while it goes.
    mounted: Signal<bool>,
    /// 0 gone, 1 there: the backdrop's opacity.
    progress: AnimatedValue,
    /// Where the panel starts: a window's height above its place.
    away: AnimatedValue,
    /// The panel's translation: `away` at 0, none at 1.
    translate: AnimatedNode,
    /// Only the latest close unmounts: opened again on its way out, it stays.
    generation: Stored<u64>,
}

impl DialogMotion {
    fn new(owner: Owner, open: Memo<bool>) -> Self {
        let progress = animated_value(0.0);
        let away = animated_value(0.0);
        let motion = Self {
            mounted: signal(false),
            progress,
            away,
            translate: away.mul(&progress.interpolate(&[0.0, 1.0], &[1.0, 0.0])),
            generation: stored(0),
        };
        effect(move || {
            let open = open.get();
            untracked(|| motion.follow(owner, open));
        });
        motion
    }

    fn follow(self, owner: Owner, open: bool) {
        let generation = self.generation.get().wrapping_add(1);
        self.generation.replace(generation);
        if open {
            if !self.mounted.get() {
                self.progress.set(0.0);
                self.away.set(-window().height.0);
                self.mounted.set(true);
            }
            if reduce_motion() {
                self.progress.set(1.0);
            } else {
                let _ = self.progress.timing(1.0, DIALOG_IN_MS, Easing::EaseOut);
            }
        } else if self.mounted.get() {
            if reduce_motion() {
                self.mounted.set(false);
                return;
            }
            let end = self.progress.timing(0.0, DIALOG_OUT_MS, Easing::EaseIn);
            let (mounted, latest) = (self.mounted, self.generation);
            let gone = move || {
                if latest.get() == generation {
                    mounted.set(false);
                }
            };
            // The bar's owner: this effect's run ends with the next change.
            // Gone when the motion ends, or after its time if no end is
            // reported.
            owner.run(|| {
                spawn(async move {
                    if end.await {
                        gone();
                    }
                });
                set_timeout(Duration::from_millis(DIALOG_OUT_MS as u64), gone);
            });
        }
    }
}

fn dialog(bar: PrBar, motion: DialogMotion) -> impl IntoElement {
    let close = move || bar.open.set(false);
    nsh_base::dialog()
        .label("Pull request")
        .animation(ModalAnimation::None)
        .close_on_escape(false)
        .close_on_swipe(false)
        .on_cancel(close)
        .child(view().absolute().inset_0().bg(rgba(0x00000099)).bind_opacity(&motion.progress).on_press(close))
        .child(
            // Presses beside the panel reach the backdrop.
            view()
                .absolute()
                .inset_0()
                .items_center()
                .justify_center()
                .pointer_events(PointerEvents::BoxNone)
                .bind_translate_y(&motion.translate)
                .child(dynamic(move || {
                    let (detail, _) = bar.shown.get()?;
                    Some(untracked(|| panel(bar, detail)))
                })),
        )
}

fn heading(s: &str) -> Text {
    text(s.to_owned()).uppercase().text_size(px(10.0)).font_weight(FontWeight::SEMIBOLD).text_color(rgb(BOLT_THEME.vars.fg_muted))
}

fn panel(bar: PrBar, d: Detail) -> View {
    let v = &BOLT_THEME.vars;
    let pr = d.pr.clone();
    let (copy_url, open_link) = (pr.url.clone(), pr.url.clone());
    let header = view()
        .flex_row()
        .items_center()
        .gap_2()
        .px_4()
        .py_3()
        .border_b_1()
        .border_color(rgb(v.border))
        .child(mono("⑂", v.accent))
        .child(text(pr.title.clone()).flex_1().min_w(px(0.0)).number_of_lines(2).text_size(px(13.0)).font_weight(FontWeight::SEMIBOLD).text_color(rgb(v.fg)))
        .child(mono(format!("#{}", pr.number), v.fg_muted))
        .child(
            view()
                .flex_shrink(0.0)
                .px_2()
                .rounded(px(4.0))
                .hover(|s| s.bg(rgb(v.bg_hover)))
                .on_press(move || bar.copy_link(copy_url.clone()))
                .child(text(move || if bar.copied.get() { "✓ copied" } else { "copy link" }).text_size(px(11.0)).font_family(MONO).text_color(rgb(v.fg_muted))),
        )
        .child(button("open ↗", false, true, move || drop(open_url(open_link.clone()))))
        .child(button("×", false, true, move || bar.open.set(false)));

    let mut reviewers = view().flex_row().items_center().gap_2().flex_wrap();
    for (login, state) in pr.reviewers() {
        let (glyph, c) = match state.as_str() {
            "APPROVED" => ("✓", GREEN),
            "CHANGES_REQUESTED" => ("✗", red()),
            _ => ("–", v.fg_muted),
        };
        reviewers = reviewers.child(seg(glyph, c, vec![(login, v.fg)]));
    }
    let pushed = d.pushed_at.as_deref().and_then(prs::unix_ms_of).map(|at| format!("last push {} ago", ago(at)));
    let summary = view()
        .flex_col()
        .gap_2()
        .child(states(view().flex_row().items_center().gap(px(16.0)).flex_wrap(), &d))
        .child(reviewers)
        .when_some(pushed, |s, p| s.child(mono(p, v.fg_muted)))
        .when(pr.mergeable == "CONFLICTING", |s| {
            s.child(
                view()
                    .px_3()
                    .py_2()
                    .rounded(px(6.0))
                    .border_1()
                    .border_color(rgb(red()))
                    .child(mono("⚠ Fix merge conflicts", red())),
            )
        });

    let mut body = view().flex_col().gap_4().p_4().child(summary);
    if !pr.checks().is_empty() {
        let mut list = view().flex_col().gap_1().child(heading("Checks"));
        for check in pr.checks() {
            let (glyph, c) = match check.state() {
                CheckState::Running => ("◐", YELLOW),
                CheckState::Failed => ("✗", red()),
                CheckState::Passed => ("✓", GREEN),
            };
            let row = view().flex_row().items_center().gap_2().px_1().rounded(px(4.0)).child(mono(glyph, c)).child(mono(check.name().to_owned(), v.fg_muted));
            list = list.child(match check.url().map(str::to_owned) {
                Some(url) => row.hover(|s| s.bg(rgb(v.bg_hover))).on_press(move || drop(open_url(url.clone()))),
                None => row,
            });
        }
        body = body.child(list);
    }
    let comments = d.comments();
    if !comments.is_empty() {
        body = comments.into_iter().fold(body.child(heading("Comments")), |b, c| b.child(comment_card(c)));
    }

    view()
        .flex_col()
        .w(px(760.0))
        .max_w(relative(0.9))
        .h(relative(0.8))
        .rounded(px(10.0))
        .border_1()
        .border_color(rgb(v.border))
        .bg(rgb(v.bg_widget))
        .shadow_2xl()
        .overflow_hidden()
        .child(header)
        .child(scroll().flex_1().min_h(px(0.0)).child(body))
        .child(
            view()
                .flex_row()
                .items_center()
                .justify_end()
                .gap_3()
                .px_4()
                .py_2()
                .border_t_1()
                .border_color(rgb(v.border))
                .child(merge_error(bar))
                .child(controls(bar)),
        )
}

fn ago(at_ms: u64) -> String {
    format_age((unix_ms() as u64).saturating_sub(at_ms))
}

fn pill(s: &str, c: u32) -> View {
    view().flex_shrink(0.0).px(px(6.0)).rounded_full().border_1().border_color(rgb(c)).child(text(s.to_owned()).text_size(px(10.0)).text_color(rgb(c)))
}

/// One comment, its body cut to two lines until pressed.
fn comment_card(c: Comment) -> View {
    let v = &BOLT_THEME.vars;
    let expanded = signal(false);
    let flat = c.body.split_whitespace().collect::<Vec<_>>().join(" ");
    let full = c.body.clone();
    let place = c.path.clone().map(|p| match c.line {
        Some(l) => format!("{p}:{l}"),
        None => p,
    });
    let age = prs::unix_ms_of(&c.at).map(|at| format!("{} ago", ago(at))).unwrap_or_default();
    let top = view()
        .flex_row()
        .items_center()
        .gap_2()
        .child(mono(c.author.clone(), v.accent))
        .when(c.state == "CHANGES_REQUESTED", |r| r.child(pill("changes requested", red())))
        .when(c.state == "APPROVED", |r| r.child(pill("approved", GREEN)))
        .when_some(c.resolved, |r, resolved| r.child(if resolved { pill("resolved", GREEN) } else { pill("unresolved", YELLOW) }))
        .when(c.fresh, |r| r.child(pill("new", v.accent)))
        .child(view().flex_1())
        .child(mono(age, v.fg_muted));
    view()
        .flex_col()
        .gap_1()
        .p(px(10.0))
        .rounded(px(6.0))
        .border_1()
        .border_color(rgb(if c.fresh { v.accent } else { v.border }))
        .bg(rgb(v.bg))
        .opacity(if c.resolved == Some(true) { 0.55 } else { 1.0 })
        .on_press(move || expanded.update(|e| *e = !*e))
        .child(top)
        .when_some(place, |card, p| card.child(mono(p, v.fg_muted)))
        .child(dynamic(move || {
            let body = if expanded.get() { full.clone() } else { flat.clone() };
            let t = text(body).text_size(px(12.0)).line_height(px(18.0)).text_color(rgb(v.fg));
            if expanded.get() { t } else { t.number_of_lines(2) }
        }))
}
