//! The cards the thread shows (`components/BoltCompose/HubCards.tsx`): the
//! posts, a job's steps, what a command printed, a choice and the finder,
//! with the result rows' affordances of `SwitchResults.tsx`.

use std::{path::Path, rc::Rc};

use nsh_base::{input, theme};
use nsh_core::prelude::*;

use super::{RADIUS, editor::prefix_color};
use crate::{
    commands::{
        Output,
        rich::{BranchList, Commit, GitStatus, Rich},
    },
    hub::{ChoiceOption, Hub, HubEntry, HubStep, JobStatus, ME, StepStatus, engine, finder_matches},
    state::Shell,
};

pub const FG: u32 = 0xffffff;
const REPLY_LINES: usize = 6;

pub fn muted() -> Rgba {
    rgba(0xffffffb8)
}
pub fn faint() -> Rgba {
    rgba(0xffffff80)
}
fn line() -> Rgba {
    rgba(0xffffff1a)
}
fn warning() -> Rgba {
    rgb(0xe5c07b)
}
fn success() -> Rgba {
    rgb(0x98c379)
}
fn error() -> Rgba {
    rgb(0xf87171)
}

/// `color-mix(in srgb, <hex> <pct>%, <other>)`.
pub fn mix(hex: u32, other: u32, pct: f32) -> Rgba {
    let ch = |shift: u32| {
        let a = ((hex >> shift) & 0xff) as f32;
        let b = ((other >> shift) & 0xff) as f32;
        ((a * pct + b * (1.0 - pct)).round() as u32) << shift
    };
    rgb(ch(16) | ch(8) | ch(0))
}

fn rgb_of(c: Rgba) -> u32 {
    let ch = |v: f32| (v * 255.0).round() as u32;
    (ch(c.r) << 16) | (ch(c.g) << 8) | ch(c.b)
}

fn panel() -> AdaptiveColor {
    theme().card
}

/// A card's background: the panel, or an error's tint over it.
fn tint(error_card: bool) -> AdaptiveColor {
    if error_card { mix(0xf87171, 0x27272a, 0.3).into() } else { panel() }
}

/// A pill: icon and label; in a narrow palette the label is cut to fit.
fn chip(icon: &'static str, label: impl IntoReactive<SharedString>, tone: Rgba) -> View {
    view()
        .flex_row()
        .items_center()
        .gap_1()
        .min_w(px(0.0))
        .rounded_full()
        .px_2()
        .py(px(2.0))
        .bg(rgba(0xffffff24))
        .child(text(icon).flex_shrink(0.0).text_size(px(10.0)).text_color(tone))
        .child(
            text(label)
                .min_w(px(0.0))
                .number_of_lines(1)
                .text_size(px(10.0))
                .font_weight(FontWeight::MEDIUM)
                .text_color(tone),
        )
}

fn button(label: &'static str, on_press: impl Fn() + 'static) -> View {
    view()
        .px(px(10.0))
        .py(px(2.0))
        .rounded_full()
        .bg(rgba(0xffffff24))
        .hover(|s| s.bg(rgba(0xffffff40)))
        .on_press(on_press)
        .child(text(label).text_size(px(10.0)).font_weight(FontWeight::SEMIBOLD).text_color(rgb(FG)))
}

fn is_script(s: &str) -> bool {
    s.trim_start().starts_with('#')
}

/// A posted line: tokens painted as pills, `known` null (`Painted`).
fn painted(text_: &str, handle_style: bool) -> View {
    let mut out = paint(text_, None, rgb(FG), 0, rgb(FG), handle_style);
    if is_script(text_) {
        out = out.font_family("ui-monospace");
    }
    view().child(out)
}

/// Paints a line's tokens as the editor's backdrop does (`paint`): `#word`
/// in `node_color`, `{var}` green when `known` (or no `known`) has it and
/// rose when not, `$var[:type]` violet, `@handle` amber or in the posted
/// bubble's style; the first `lead` characters as a keyword pill.
pub fn paint(
    text_: &str,
    known: Option<&[String]>,
    node_color: Rgba,
    lead: usize,
    lead_color: Rgba,
    handle_style: bool,
) -> Text {
    let mut out = text("").text_size(px(12.0)).line_height(px(20.0)).text_color(rgb(FG));
    let chars: Vec<(usize, char)> = text_.char_indices().collect();
    let word = |c: char| c.is_alphanumeric() || c == '_';
    let lead = lead.min(text_.len());
    let mut last = lead;
    let mut i = 0;
    let pill = |tok: &str, color: Rgba| {
        text(tok.to_owned()).text_color(color).bg(Rgba { a: 0.18, ..color }).rounded(px(3.0))
    };
    if lead > 0 {
        let color = lead_color;
        out = out.child(text(text_[..lead].to_owned()).text_color(color).bg(Rgba { a: 0.18, ..color }).rounded(px(3.0)));
    }
    while i < chars.len() {
        let (at, c) = chars[i];
        if at < lead {
            i += 1;
            continue;
        }
        let run = |from: usize, ok: &dyn Fn(char) -> bool| {
            let mut j = from;
            while j < chars.len() && ok(chars[j].1) {
                j += 1;
            }
            j
        };
        let end = match c {
            '#' => {
                let j = run(i + 1, &|c| word(c) || c == '-');
                (j > i + 1).then_some(j)
            }
            '{' => {
                let j = run(i + 1, &|c| word(c) || c == '.' || c == '-');
                (j > i + 1 && chars.get(j).is_some_and(|(_, c)| *c == '}')).then_some(j + 1)
            }
            '$' => {
                let j = run(i + 1, &|c| word(c) || c == '.' || c == '-');
                (j > i + 1).then(|| {
                    let tail = &text_[chars.get(j).map_or(text_.len(), |(b, _)| *b)..];
                    let ty = [":string[]", ":string", ":number", ":boolean"].iter().find(|t| tail.starts_with(**t));
                    j + ty.map_or(0, |t| t.chars().count())
                })
            }
            '@' => {
                let handle = |c: char| c.is_ascii_alphanumeric() || c == '/' || c == '-';
                let tail = &text_[at..];
                if tail.len() >= 7 && tail[..7].eq_ignore_ascii_case("@board:") {
                    Some(run(i + 7, &handle))
                } else {
                    let j = run(i + 1, &handle);
                    (j > i + 1).then_some(j)
                }
            }
            _ => None,
        };
        let Some(end) = end else {
            i += 1;
            continue;
        };
        let (from, to) = (at, chars.get(end).map_or(text_.len(), |(b, _)| *b));
        if from > last {
            out = out.child(text_[last..from].to_owned());
        }
        let tok = &text_[from..to];
        let pilled = match c {
            '#' => pill(tok, node_color),
            '{' => pill(
                tok,
                if known.is_none_or(|k| k.iter().any(|v| *v == tok[1..tok.len() - 1])) { rgb(0x22c55e) } else { rgb(0xfb7185) },
            ),
            '$' => pill(tok, rgb(0xc084fc)),
            _ if handle_style => text(tok.to_owned())
                .text_color(rgb(FG))
                .font_weight(FontWeight::BOLD)
                .bg(rgba(0x00000059))
                .rounded(px(15.0))
                .px(px(6.0)),
            _ => pill(tok, rgb(0xfbbf24)),
        };
        out = out.child(pilled);
        last = to;
        i = end;
    }
    if last < text_.len() {
        out = out.child(text_[last..].to_owned());
    }
    out
}

fn status_icon(status: &str) -> View {
    let (glyph, color) = match status {
        "pending" => ("○", faint()),
        "running" => ("◐", rgb(FG)),
        "waiting" => ("⏸", warning()),
        "done" => ("✓", success()),
        _ => ("✕", error()),
    };
    view().w(px(14.0)).items_center().child(text(glyph).text_size(px(13.0)).text_color(color))
}

fn step_status(s: StepStatus) -> &'static str {
    match s {
        StepStatus::Pending => "pending",
        StepStatus::Running => "running",
        StepStatus::Waiting => "waiting",
        StepStatus::Done => "done",
        StepStatus::Failed => "failed",
    }
}

/// `span`: a step's or job's time.
fn span(from: Option<f64>, to: Option<f64>, now: f64) -> String {
    let Some(from) = from.filter(|f| *f != 0.0) else {
        return String::new();
    };
    let s = ((to.unwrap_or(now) - from) / 1000.0).round().max(0.0) as u64;
    if s < 60 { format!("{s}s") } else { format!("{}m {}s", s / 60, s % 60) }
}

fn reply(body: String) -> View {
    let open = signal(false);
    let lines: Vec<&str> = body.split('\n').collect();
    let long = lines.len() > REPLY_LINES;
    let short = lines[..lines.len().min(REPLY_LINES)].join("\n");
    let full = body.clone();
    view()
        .pl(px(24.0))
        .child(text(move || if long && !open.get() { short.clone() } else { full.clone() }).text_size(px(11.0)).line_height(px(20.0)).text_color(muted()))
        .when(long, |v| {
            v.child(
                text(move || if open.get() { "show less" } else { "show all" })
                    .text_size(px(11.0))
                    .underline()
                    .text_color(faint())
                    .on_press(move || open.update(|o| *o = !*o)),
            )
        })
}

fn card(running: bool) -> View {
    view()
        .w_full()
        .max_w(px(450.0))
        .rounded(px(RADIUS))
        .overflow_hidden()
        .bg(move || panel())
        .when(running, |v| {
            v.child(view().absolute().top_0().left_0().h(px(2.0)).w(relative(0.33)).bg(rgb(FG)))
        })
}

fn job_kind(run: &Option<String>, to: &str) -> (&'static str, String) {
    match run.as_deref() {
        None => ("⑂", format!("fork → @{to}")),
        Some("board") => ("‹›", "script".into()),
        Some(r) if r.starts_with('@') => ("‹›", format!("script · {r}")),
        Some(r) => ("ϟ", format!("command · {r}")),
    }
}

/// A job: its steps with their times, Cancel while it runs, Continue on an
/// approval, Open in terminal once it has a hop, a reply under a `@bolt`
/// job.
pub fn job_card(hub: Hub, entry: &HubEntry, now: Signal<f64>) -> AnyElement {
    let HubEntry::Job { id, ask, to, run, started_at, ended_at, steps, status, hop, error: err, .. } = entry else {
        return view().into_element();
    };
    let waiting = steps.iter().any(|s| s.status == StepStatus::Waiting);
    let running = *status == JobStatus::Running;
    let shown = match (status, waiting) {
        (JobStatus::Running, true) => "waiting",
        (JobStatus::Running, false) => "running",
        (JobStatus::Done, _) => "done",
        (JobStatus::Failed, _) => "failed",
    };
    let (icon, label) = job_kind(run, to);
    let (started, ended) = (*started_at, *ended_at);
    let jid = id.clone();
    let mut c = card(running && !waiting).child(
        view()
            .flex_row()
            .items_center()
            .gap_2()
            .px_3()
            .pt(px(10.0))
            .pb_2()
            .when(!running, |v| v.child(status_icon(shown)))
            .child(chip(icon, label, rgb(FG)))
            .child(
                view().flex_1().flex_row().justify_end().child(
                    text(move || if waiting { "waiting on you".to_owned() } else { span(started, ended, now.get()) })
                        .text_size(px(10.0))
                        .tabular_nums()
                        .text_color(faint()),
                ),
            )
            .when(running, {
                let jid = jid.clone();
                move |v| v.child(button("Cancel", move || engine::cancel_job(hub, &jid)))
            })
            .when(!running && hop.is_some(), {
                let jid = jid.clone();
                move |v| v.child(button("Open in terminal", move || engine::open_hop(hub, &jid)))
            }),
    );
    if run.is_some() {
        c = c.child(view().px_3().pb_2().child(painted(ask, false)));
    }
    for (i, s) in steps.iter().enumerate() {
        c = c.child(step_row(hub, &jid, i, s, now));
    }
    if run.as_deref() == Some("bolt") && hop.is_some() && !running {
        let draft = signal(String::new());
        let jid = jid.clone();
        c = c.child(
            input()
                .value(move || draft.get())
                .on_change(move |t: &str| draft.set(t.to_owned()))
                .submit_behavior(SubmitBehavior::Submit)
                .on_submit(move |t: &str| {
                    if t.trim().is_empty() {
                        return;
                    }
                    engine::reply_bolt(hub, &jid, t.to_owned());
                    draft.set(String::new());
                })
                .placeholder("Reply…")
                .placeholder_color(faint())
                .text_color(rgb(FG))
                .text_size(px(12.0))
                .w_full()
                .px_3()
                .py(px(6.0))
                .border_t_1()
                .border_color(line()),
        );
    }
    if let Some(message) = err {
        c = c.child(
            view()
                .flex_row()
                .items_start()
                .gap_2()
                .px_3()
                .py_2()
                .border_t_1()
                .border_color(line())
                .bg(mix(0xf87171, 0x27272a, 0.3))
                .child(text("✕").text_size(px(11.0)).text_color(rgb(FG)))
                .child(text(message.clone()).flex_1().text_size(px(11.0)).text_color(rgb(FG))),
        );
    }
    c.into_element()
}

fn step_row(hub: Hub, job: &str, at: usize, s: &HubStep, now: Signal<f64>) -> View {
    let jid = job.to_owned();
    let waiting = s.status == StepStatus::Waiting;
    let (started, ended) = (s.started_at, s.ended_at);
    view()
        .px_3()
        .py(px(6.0))
        .gap_1()
        .border_t_1()
        .border_color(line())
        .child(
            view()
                .flex_row()
                .items_center()
                .gap_2()
                .child(status_icon(step_status(s.status)))
                .child(
                    text(s.label.clone().unwrap_or_else(|| format!("@{}", s.handle)))
                        .flex_1()
                        .min_w(px(0.0))
                        .number_of_lines(1)
                        .text_size(px(12.0))
                        .text_color(rgb(FG)),
                )
                .child(if waiting {
                    view()
                        .px(px(10.0))
                        .py(px(2.0))
                        .rounded_full()
                        .bg(rgb(FG))
                        .on_press(move || engine::continue_approval(hub, &jid, at))
                        .child(text("Continue").text_size(px(10.0)).font_weight(FontWeight::SEMIBOLD).text_color(move || theme().primary))
                } else {
                    view().child(text(move || span(started, ended, now.get())).text_size(px(10.0)).tabular_nums().text_color(faint()))
                }),
        )
        .when(!s.reply.is_empty(), |v| v.child(reply(s.reply.clone())))
}

/// What a command printed, each output a card; a directory listing's
/// folders and a branch list's branches are clickable
/// (`SwitchResults`' `DirectoryList`, `Branches`).
pub fn result_card(shell: Shell, entry: &HubEntry, run: Rc<dyn Fn(String)>) -> AnyElement {
    let HubEntry::Result { cmd, items, done, note, .. } = entry else {
        return view().into_element();
    };
    if !done {
        return text(format!("running {cmd}…")).text_size(px(10.0)).text_color(faint()).into_element();
    }
    if items.is_empty() {
        return match note {
            Some(n) => view().flex_row().child(chip("⌨", n.clone(), muted())).into_element(),
            None => view().into_element(),
        };
    }
    let mut col = view().w_full().max_w(px(450.0)).gap_2();
    for item in items {
        col = col.child(output_card(shell, item, run.clone()));
    }
    col.into_element()
}

fn output_card(shell: Shell, o: &Output, run: Rc<dyn Fn(String)>) -> View {
    let body = match (&o.rich, o.key.as_str()) {
        _ if o.error => view()
            .flex_row()
            .items_start()
            .gap_2()
            .child(text("✕").text_size(px(11.0)).text_color(rgb(FG)))
            .child(text(o.text.clone()).flex_1().text_size(px(11.0)).line_height(px(20.0)).text_color(rgb(FG))),
        (Some(Rich::Git { icon, detail }), _) => git_message(icon, &o.text, detail.as_deref()),
        (Some(Rich::Direct), _) => mono(&o.text),
        (Some(Rich::Conflict { files, directory, hint }), _) => conflict(shell, &o.text, files, directory, hint.as_deref()),
        (Some(Rich::Status(status)), _) => git_status(shell, status),
        (Some(Rich::Branches(list)), _) => branch_list(list, run),
        (Some(Rich::CommitLogs { commits }), _) => commit_logs(commits),
        (Some(Rich::Bookmarks { items }), _) => bookmarks(items),
        (Some(Rich::Notes { items }), _) => notes_list(shell, items),
        (None, "branches") => branches(&o.text, run),
        (None, "directoryList") => directory_list(shell, &o.text, run),
        (None, "statusShow" | "stashShow") => diff_text(&o.text),
        (None, "git" | "trace" | "gitStatus" | "pwd") => mono(&o.text),
        (None, _) => view().child(text(o.text.clone()).text_size(px(12.0)).line_height(px(20.0)).text_color(rgb(FG))),
    };
    view().w_full().px_3().py_2().rounded(px(RADIUS)).overflow_hidden().bg(tint(o.error)).child(body)
}

fn mono(body: &str) -> View {
    view().child(text(body.to_owned()).font_family("ui-monospace").text_size(px(12.0)).line_height(px(20.0)).text_color(rgb(FG)))
}

/// `gitMessageIconMap`: each git step's glyph and color (Phosphor's icons
/// as the nearest characters).
fn git_icon(icon: &str) -> (&'static str, u32) {
    match icon {
        "fetch" | "fetching" => ("⇣", 0x60a5fa),
        "pull" | "pulling" => ("⬇", 0x60a5fa),
        "push" | "pushing" => ("⬆", 0x4ade80),
        "merge" | "merging" => ("⑃", 0xc084fc),
        "undo" => ("↺", 0xfb923c),
        "add" => ("+", 0x4ade80),
        "commit" => ("●", 0x34d399),
        "upstream" => ("⇡", 0x38bdf8),
        "terminal" => ("›", 0xa3a3a3),
        "stash" => ("▤", 0xfbbf24),
        "warning" => ("⚠", 0xfbbf24),
        // `checkout`, and Bolt's fallback: a branch in cyan.
        _ => ("⑂", 0x06b6d4),
    }
}

/// `type: "git-message"`: the step's icon, its text, and a faint detail at
/// the right.
fn git_message(icon: &str, body: &str, detail: Option<&str>) -> View {
    let (glyph, color) = git_icon(icon);
    // The text ends with ` — <detail>` for readers of text alone; the card
    // draws the detail apart, so not twice.
    let body = detail.and_then(|d| body.strip_suffix(&format!(" — {d}"))).unwrap_or(body);
    view()
        .flex_row()
        .items_start()
        .gap_2()
        .child(text(glyph).text_size(px(12.0)).line_height(px(20.0)).text_color(rgb(color)))
        .child(text(body.to_owned()).flex_1().text_size(px(12.0)).line_height(px(20.0)).text_color(rgb(FG)))
        .when_some(detail.map(str::to_owned), |v, d| v.child(text(d).text_size(px(12.0)).line_height(px(20.0)).text_color(faint())))
}

/// A file a card lists: a click opens it in Helix (`openFileAtom`).
fn file_row(shell: Shell, directory: String, file: String) -> View {
    let path = Path::new(&directory).join(&file);
    view()
        .px_2()
        .py_1()
        .rounded_md()
        .hover(|s| s.bg(rgba(0x0000004d)))
        .on_press(move || {
            if let Err(e) = shell.open_in_helix(directory.clone(), &path, None) {
                eprintln!("rivet: {e}");
            }
        })
        .child(text(file).text_size(px(14.0)).text_color(move || theme().foreground))
}

/// A section's heading: its glyph and name, bold, in its color.
fn section_head(glyph: &'static str, label: &str, color: Rgba) -> View {
    view()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .mb(px(2.0))
        .child(text(glyph).text_size(px(14.0)).text_color(color))
        .child(text(label.to_owned()).text_size(px(14.0)).font_weight(FontWeight::BOLD).text_color(color))
}

/// `type: "conflict"`: the title, the conflicted files to open, what next.
fn conflict(shell: Shell, title: &str, files: &[String], directory: &str, hint: Option<&str>) -> View {
    let mut list = view().ml(px(24.0));
    for f in files {
        list = list.child(file_row(shell, directory.to_owned(), f.clone()));
    }
    view()
        .child(
            view()
                .flex_row()
                .items_center()
                .gap_2()
                .mb_2()
                .child(text("⚠").text_size(px(12.0)).text_color(rgb(0xfbbf24)))
                .child(text(title.to_owned()).text_size(px(12.0)).font_weight(FontWeight::SEMIBOLD).text_color(rgb(FG))),
        )
        .child(list)
        .when_some(hint.map(str::to_owned), |v, h| {
            v.child(
                view()
                    .flex_row()
                    .items_start()
                    .gap_2()
                    .mt_2()
                    .pt_2()
                    .border_t_1()
                    .border_color(line())
                    .child(text("→").text_size(px(12.0)).line_height(px(20.0)).text_color(rgb(0x22d3ee)))
                    .child(text(h).flex_1().text_size(px(12.0)).line_height(px(20.0)).text_color(rgb(FG))),
            )
        })
}

/// `GitStatus`: lines changed, ahead/behind, unpushed commits, then each
/// kind of change with its files, a click opening one.
fn git_status(shell: Shell, s: &GitStatus) -> View {
    let dir = shell.directory();
    let mut col = view().gap_1();
    if let Some(loc) = s.loc.filter(|l| l.committed != (0, 0) || l.uncommitted != (0, 0)) {
        // Since the default branch when there is any, else uncommitted; both
        // shown when both are.
        let both = loc.committed != (0, 0) && loc.uncommitted != (0, 0);
        let main = if loc.committed != (0, 0) { loc.committed } else { loc.uncommitted };
        let num = |t: String, c: Rgba| text(t).font_family("ui-monospace").text_size(px(12.0)).tabular_nums().text_color(c);
        let mut row = view().flex_row().items_center().gap_1().mb_1().child(num(format!("+{}", main.0), success()));
        if both {
            row = row.child(num(format!("(+{})", loc.uncommitted.0), faint()));
        }
        row = row.child(num("/".into(), faint())).child(num(format!("-{}", main.1), error()));
        if both {
            row = row.child(num(format!("(-{})", loc.uncommitted.1), faint()));
        }
        col = col.child(row);
    }
    if let Some(stats) = &s.branch_stats {
        col = col.child(view().mb_2().child(text(stats.clone()).text_size(px(14.0)).text_color(muted())));
    }
    if !s.unpushed.is_empty() {
        let mut list = view().ml(px(24.0));
        for c in &s.unpushed {
            list = list.child(
                view()
                    .flex_row()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .child(text(c.hash.clone()).font_family("ui-monospace").text_size(px(14.0)).text_color(faint()))
                    .child(text(c.subject.clone()).flex_1().text_size(px(14.0)).text_color(move || theme().foreground)),
            );
        }
        col = col.child(view().mb_2().child(section_head("⇡", "unpushed", rgb(FG))).child(list));
    }
    let sections: [(&str, &Vec<String>, &'static str, u32); 6] = [
        ("modified", &s.modified, "✎", 0x61afef),
        ("added", &s.added, "+", 0x98c379),
        ("deleted", &s.deleted, "−", 0xe06c75),
        ("renamed", &s.renamed, "⇄", 0xd8b4fe),
        ("copied", &s.copied, "⧉", 0xfde047),
        ("untracked", &s.untracked, "?", 0x93c5fd),
    ];
    let mut any = false;
    for (label, files, glyph, color) in sections {
        if files.is_empty() {
            continue;
        }
        any = true;
        let mut list = view().ml(px(24.0));
        for f in files {
            list = list.child(file_row(shell, dir.clone(), f.clone()));
        }
        col = col.child(view().mb_2().child(section_head(glyph, label, rgb(color))).child(list));
    }
    if !any && s.branch_stats.is_none() {
        col = col.child(text("Nothing to commit, working tree clean").text_size(px(14.0)).text_color(faint()));
    }
    col
}

/// A small label beside a branch's name (`default`, `worktree`).
fn badge(label: &'static str, bg: Rgba, fg: Rgba) -> View {
    view().px(px(6.0)).py(px(2.0)).rounded(px(4.0)).bg(bg).child(text(label).text_size(px(12.0)).text_color(fg))
}

/// `Branches`: each branch, the current one outlined; its badges, last
/// commit and whether it has a remote; a click switches to it (`go`).
fn branch_list(b: &BranchList, run: Rc<dyn Fn(String)>) -> View {
    let mut col = view().gap(px(2.0)).child(
        view()
            .flex_row()
            .items_center()
            .gap_2()
            .pb_2()
            .child(text("⑂").text_size(px(14.0)).text_color(muted()))
            .child(text("Branches").text_size(px(14.0)).font_weight(FontWeight::BOLD).text_color(rgb(FG))),
    );
    let surface = crate::theme::BOLT_THEME.vars.bg;
    for item in &b.list {
        let current = item.name == b.current;
        let default = b.default_branch.as_deref() == Some(item.name.as_str());
        let (go, run) = (item.name.clone(), run.clone());
        let mut names = view().flex_row().items_center().gap_2().child(
            text(item.name.clone())
                .min_w(px(0.0))
                .number_of_lines(1)
                .flex_shrink(1.0)
                .text_size(px(14.0))
                .font_weight(FontWeight::MEDIUM)
                .text_color(move || if current { theme().primary } else { theme().foreground }),
        );
        if default {
            names = names.child(badge("default", mix(FG, surface, 0.2), muted()));
        }
        if item.is_worktree {
            let accent = crate::theme::BOLT_THEME.vars.accent;
            names = names.child(badge("worktree", mix(accent, surface, 0.25), rgb(accent)));
        }
        let (cloud, cloud_color) = if item.has_remote { ("☁", rgb(crate::theme::BOLT_THEME.vars.accent)) } else { ("⊘", rgb(0xec4899)) };
        col = col.child(
            view()
                .flex_row()
                .items_center()
                .gap_2()
                .px_2()
                .py(px(6.0))
                .rounded_md()
                .border_1()
                .border_color(if current { theme().primary } else { theme().primary.opacity(0.0) })
                .hover(|s| s.bg(rgba(0xffffff1a)))
                .on_press(move || {
                    if !current {
                        run(format!("go {go}"));
                    }
                })
                .child(
                    view()
                        .flex_1()
                        .min_w(px(0.0))
                        .child(names)
                        .when(!item.last_commit.is_empty(), |v| {
                            v.child(text(item.last_commit.clone()).number_of_lines(1).mt(px(2.0)).text_size(px(12.0)).text_color(faint()))
                        }),
                )
                .child(text(cloud).text_size(px(14.0)).text_color(cloud_color)),
        );
    }
    col
}

/// `CommitLogs`: each commit's subject, its hash at the right; a click on
/// the hash copies it.
fn commit_logs(commits: &[Commit]) -> View {
    let mut col = view();
    for c in commits {
        let hash = c.hash.clone();
        col = col.child(
            view()
                .flex_row()
                .items_start()
                .gap_2()
                .p_1()
                .mb_1()
                .rounded_md()
                .hover(|s| s.bg(rgba(0xffffff1a)))
                .child(text(c.subject.clone()).flex_1().text_size(px(14.0)).text_color(move || theme().foreground))
                .child(
                    view()
                        .px_1()
                        .rounded_md()
                        .hover(|s| s.bg(panel()))
                        .on_press(move || set_clipboard(hash.clone()))
                        .child(text(c.hash.clone()).text_size(px(12.0)).text_color(muted())),
                ),
        );
    }
    col
}

/// `BookmarkList`: each bookmark's path, its key at the right.
fn bookmarks(items: &[(String, String)]) -> View {
    let mut col = view();
    for (key, path) in items {
        col = col.child(
            view()
                .flex_row()
                .items_center()
                .justify_between()
                .gap_2()
                .child(text(path.clone()).flex_1().min_w(px(0.0)).number_of_lines(1).text_size(px(14.0)).text_color(move || theme().foreground))
                .child(text(key.clone()).text_size(px(14.0)).text_color(rgb(crate::theme::BOLT_THEME.vars.accent))),
        );
    }
    col
}

/// `NotesList`: each note by the directory it belongs to; a click opens it
/// for editing, its × deletes the file and the row.
fn notes_list(shell: Shell, items: &[(String, String)]) -> View {
    if items.is_empty() {
        return view().child(text("No notes found").text_size(px(14.0)).text_color(faint()));
    }
    let mut col = view();
    for (name, path) in items {
        let gone = signal(false);
        let (open, delete) = (path.clone(), path.clone());
        col = col.child(when(move || !gone.get(), {
            let name = name.clone();
            move || {
                let (open, delete) = (open.clone(), delete.clone());
                view()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .p_1()
                    .mb_1()
                    .rounded_md()
                    .hover(|s| s.bg(rgba(0xffffff1a)))
                    .on_press(move || shell.open_editable(shell.directory(), open.clone()))
                    .child(text(name.clone()).flex_1().min_w(px(0.0)).number_of_lines(1).text_size(px(14.0)).text_color(move || theme().foreground))
                    .child(
                        text("×")
                            .text_size(px(14.0))
                            .text_color(faint())
                            .hover(|s| s.text_color(error()))
                            .on_press(move || {
                                if std::fs::remove_file(&delete).is_ok() {
                                    gone.set(true);
                                }
                            }),
                    )
            }
        }));
    }
    col
}

/// `DiffView`: a diff's lines in its colors: headers faint and bold, adds,
/// deletes, hunks, file starts.
fn diff_text(body: &str) -> View {
    let accent = crate::theme::BOLT_THEME.vars.accent;
    let mut col = view();
    for l in body.lines() {
        let (color, bold) = if l.starts_with("+++") || l.starts_with("---") {
            (faint(), true)
        } else if l.starts_with('+') {
            (success(), false)
        } else if l.starts_with('-') {
            (error(), false)
        } else if l.starts_with("@@") {
            (rgb(accent), false)
        } else if l.starts_with("diff --git") || l.starts_with("━━━") {
            (mix(accent, 0x27272a, 0.7), true)
        } else if l.starts_with("index ") {
            (faint(), false)
        } else {
            (rgb(FG), false)
        };
        let t = text(if l.is_empty() { " ".to_owned() } else { l.to_owned() })
            .font_family("ui-monospace")
            .text_size(px(12.0))
            .line_height(px(18.0))
            .text_color(color);
        col = col.child(if bold { t.font_weight(FontWeight::BOLD) } else { t });
    }
    col
}

fn branches(body: &str, run: Rc<dyn Fn(String)>) -> View {
    let list = body
        .lines()
        .map(|l| (l.starts_with("* "), l.get(2..).unwrap_or("").to_owned()))
        .collect::<Vec<_>>();
    let mut col = view().gap(px(2.0)).child(
        view()
            .flex_row()
            .items_center()
            .gap_2()
            .pb_2()
            .child(text("⑂").text_size(px(14.0)).text_color(muted()))
            .child(text("Branches").text_size(px(14.0)).font_weight(FontWeight::BOLD).text_color(rgb(FG))),
    );
    for (current, name) in list {
        let go = name.clone();
        let run = run.clone();
        col = col.child(
            view()
                .flex_row()
                .items_center()
                .px_2()
                .py(px(6.0))
                .rounded_md()
                .border_1()
                .border_color(if current { theme().primary } else { theme().primary.opacity(0.0) })
                .hover(|s| s.bg(rgba(0xffffff1a)))
                .on_press(move || {
                    if !current {
                        run(format!("go {go}"));
                    }
                })
                .child(
                    text(name)
                        .flex_1()
                        .min_w(px(0.0))
                        .number_of_lines(1)
                        .text_size(px(14.0))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(move || if current { theme().primary } else { theme().foreground }),
                ),
        );
    }
    col
}

fn directory_list(shell: Shell, body: &str, run: Rc<dyn Fn(String)>) -> View {
    let dir = shell.directory();
    let mut wrap = view().flex_row().flex_wrap().gap_1();
    for item in body.lines().filter(|l| !l.is_empty()) {
        let is_dir = Path::new(&dir).join(item).is_dir();
        let go = format!("cd {}", serde_json::to_string(item).unwrap_or_default());
        let run = run.clone();
        wrap = wrap.child(
            view()
                .p_1()
                .min_w(px(0.0))
                .max_w(relative(1.0))
                .rounded_md()
                .hover(|s| s.bg(rgba(0xffffff1a)))
                .when(is_dir, move |v| v.on_press(move || run(go.clone())))
                .child(text(item.to_owned()).number_of_lines(1).text_size(px(14.0)).text_color(move || theme().foreground)),
        );
    }
    wrap
}

/// An open question: click an option, or type its key in the input.
pub fn choice_card(hub: Hub, entry: &HubEntry) -> AnyElement {
    let HubEntry::Choice { id, title, detail, options, picked, done } = entry else {
        return view().into_element();
    };
    let done = *done;
    let tone_of = |o: &ChoiceOption| o.tone.as_deref().and_then(parse_hex).unwrap_or_else(|| rgb(0x38bdf8));
    let label = if done {
        if picked.is_some() { "answered".to_owned() } else { "cancelled".to_owned() }
    } else {
        format!("{title} · needs you")
    };
    let mut c = card(false).child(
        view()
            .flex_row()
            .items_center()
            .gap_2()
            .px_3()
            .pt(px(10.0))
            .pb_2()
            .child(chip("✋", label, if done { rgb(FG) } else { warning() }))
            .when_some(detail.clone(), |v, d| {
                v.child(text(d).flex_1().min_w(px(0.0)).number_of_lines(1).text_size(px(11.0)).text_color(muted()))
            }),
    );
    for o in options {
        let chosen = picked.as_deref() == Some(o.key.as_str());
        let tone = tone_of(o);
        let (cid, key) = (id.clone(), o.key.clone());
        c = c.child(
            view()
                .w_full()
                .flex_row()
                .items_center()
                .gap_2()
                .px_3()
                .py(px(6.0))
                .border_t_1()
                .border_color(line())
                .opacity(if done && !chosen { 0.4 } else { 1.0 })
                .bg(if chosen { Rgba { a: 0.24, ..tone } } else { Rgba { a: 0.0, ..tone } })
                .when(!done, |v| v.hover(|s| s.bg(rgba(0xffffff1a))).on_press(move || hub.pick_choice(&cid, key.clone())))
                .child(
                    view()
                        .h(px(20.0))
                        .min_w(px(20.0))
                        .items_center()
                        .justify_center()
                        .rounded_md()
                        .border_1()
                        .border_color(mix(rgb_of(tone), 0x27272a, 0.5))
                        .bg(move || panel())
                        .px_1()
                        .child(text(o.key.clone()).uppercase().text_size(px(10.0)).font_weight(FontWeight::BOLD).text_color(tone)),
                )
                .child(text(o.label.clone()).text_size(px(12.0)).font_weight(FontWeight::SEMIBOLD).text_color(rgb(FG)))
                .when_some(o.hint.clone(), |v, h| {
                    v.child(text(h).flex_1().min_w(px(0.0)).number_of_lines(1).text_size(px(11.0)).text_color(faint()))
                }),
        );
    }
    c.into_element()
}

const FINDER_HELP: &str =
    "folder name or cd <path> · .. · ~ · mkdir <name> · tab [<folder>] · bm <name> · bm add|rm <name> · exit";

/// The folders of the current directory, filtered by what the input says,
/// and the bookmarks; a click goes there.
pub fn finder_card(
    shell: Shell,
    hub: Hub,
    entry: &HubEntry,
    folders: Signal<Vec<String>>,
    go: Rc<dyn Fn(Vec<String>)>,
) -> AnyElement {
    let HubEntry::Finder { done, n, .. } = entry else {
        return view().into_element();
    };
    let (done, n) = (*done, *n);
    let dir_of = move || shell.settings.with(|s| s.directory.clone());
    effect(move || {
        let dir = dir_of();
        let _ = n;
        if dir.is_empty() || done {
            return;
        }
        folders.set(read_folders(&dir));
    });
    let query = hub.finder_query;
    let row = {
        let go = go.clone();
        move |label: String, cd: String, hint: Option<String>, icon: &'static str| {
            let (a, b) = (go.clone(), go.clone());
            let (cd2, cd1) = (cd.clone(), cd);
            view()
                .flex_row()
                .items_center()
                .min_w(px(0.0))
                .hover(|s| s.bg(rgba(0xffffff1a)))
                .child(
                    view()
                        .flex_1()
                        .min_w(px(0.0))
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .px_3()
                        .py(px(4.0))
                        .on_press(move || a(vec![cd1.clone()]))
                        .child(text(icon).text_size(px(12.0)).text_color(muted()))
                        .child(text(label).flex_shrink(1.0).min_w(px(0.0)).number_of_lines(1).text_size(px(12.0)).text_color(rgb(FG)))
                        .when_some(hint, |v, h| {
                            v.child(text(h).flex_1().min_w(px(0.0)).number_of_lines(1).text_size(px(11.0)).text_color(faint()))
                        }),
                )
                .child(
                    view()
                        .mr_2()
                        .px(px(6.0))
                        .rounded(px(4.0))
                        .hover(|s| s.bg(rgba(0xffffff33)))
                        .on_press(move || b(vec![cd2.clone(), "tab".to_owned()]))
                        .child(text("+ tab").text_size(px(10.0)).text_color(muted())),
                )
        }
    };
    let marks: Vec<(String, String)> = shell.settings.with_untracked(|s| s.bookmarks.iter().map(|(k, v)| (k.clone(), v.clone())).collect());
    let mut c = card(false).child(
        view()
            .flex_row()
            .items_center()
            .gap_2()
            .px_3()
            .pt(px(10.0))
            .pb_2()
            .child(chip("▸", if done { "finder closed" } else { "finder" }, if done { rgb(FG) } else { rgb(0x38bdf8) }))
            .child(text(move || dir_of()).flex_1().min_w(px(0.0)).number_of_lines(1).text_size(px(11.0)).text_color(muted())),
    );
    if done {
        return c.into_element();
    }
    let shown = move || finder_matches(&folders.get(), &query.get());
    c = c
        .child(view().px_3().pb_2().child(text(FINDER_HELP).text_size(px(11.0)).line_height(px(20.0)).text_color(faint())))
        .child(
            scroll().max_h(px(280.0)).border_t_1().border_color(line()).child(
                view()
                    .flex_row()
                    .flex_wrap()
                    .child(when(move || shown().is_empty(), move || {
                        view().px_3().py(px(6.0)).child(
                            text(move || {
                                let q = query.get();
                                if q.is_empty() { "no folders".to_owned() } else { format!("no folders match {q}") }
                            })
                            .text_size(px(11.0))
                            .text_color(faint()),
                        )
                    }))
                    .child(each(shown, |name| name.clone(), {
                        let row = row.clone();
                        move |name, _| {
                            let name = name.get_untracked();
                            let cd = format!("cd {}", serde_json::to_string(&name).unwrap_or_default());
                            view().w(px(140.0)).child(row(name, cd, None, "📁"))
                        }
                    })),
            ),
        );
    if !marks.is_empty() {
        let mut list = view();
        for (name, path) in marks {
            let cd = format!("cd {} --absolute", serde_json::to_string(&path).unwrap_or_default());
            list = list.child(row(name, cd, Some(path), "🔖"));
        }
        c = c.child(scroll().max_h(px(200.0)).border_t_1().border_color(line()).child(list));
    }
    c.into_element()
}

/// The folders of `dir` that are not hidden, by name.
pub fn read_folders(dir: &str) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()) || e.path().is_dir())
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| !n.starts_with('.'))
                .collect()
        })
        .unwrap_or_default();
    out.sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()).then(a.cmp(b)));
    out
}

fn parse_hex(s: &str) -> Option<Rgba> {
    let hex = s.strip_prefix('#')?;
    (hex.len() == 6).then(|| u32::from_str_radix(hex, 16).ok().map(rgb)).flatten()
}

/// A post: the user's as a bubble on the right, a terminal's with its
/// handle on the left, an ask outlined (`PostCard`).
pub fn post_card(entry: &HubEntry, handle: Option<String>) -> AnyElement {
    let (from, body, ask, to, is_error) = match entry {
        HubEntry::Say { from, text, to, error, .. } => (from, text, false, to.clone(), *error),
        HubEntry::Ask { from, text, .. } => (from, text, true, None, false),
        _ => return view().into_element(),
    };
    let mine = from == ME;
    let bubble = {
        let t = body.trim_start();
        if is_error {
            mix(0xe06c75, 0xffffff, 0.84)
        } else {
            match t.chars().next().and_then(prefix_color) {
                Some(c) => mix(rgb_of(c), 0x000000, 0.84),
                None => rgb(0x3a3d42),
            }
        }
    };
    let mut col = view().w_full().gap_1().when(mine, |v| v.items_end()).when(!mine, |v| v.items_start());
    if !mine {
        let who = format!("@{}{}", handle.unwrap_or_else(|| "terminal".into()), if ask { " · needs you" } else { "" });
        col = col.child(chip(if ask { "✋" } else { "▤" }, who, if ask { warning() } else { rgb(FG) }));
    }
    let inner = if mine {
        let said = match &to {
            Some(to) => format!("@{to} {body}"),
            None => body.clone(),
        };
        painted(&said, true)
    } else {
        view().child(text(body.clone()).text_size(px(12.0)).line_height(px(20.0)).text_color(muted()))
    };
    col.child(
        view()
            .max_w(px(450.0))
            .rounded(px(RADIUS))
            .px(px(14.0))
            .py_2()
            .bg(if mine { bubble.into() } else { panel() })
            .when(ask, |v| v.border_1().border_color(warning()))
            .child(inner),
    )
    .into_element()
}
