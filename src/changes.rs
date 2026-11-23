//! The Changes tab (Bolt's `ChangesTab` and `changesStore`, read-only for
//! now): the files a diff touches on the left, the selected one's diff on
//! the right. `diff` opens it on the working tree, or on the branch's
//! commits when the tree is clean (`resolveChangesModeAtom`); `diff
//! --commit <hash>` and `--since <hash>` pin a commit or a range, as does
//! the header's commit menu.

use std::time::Duration;

use nsh_base::{diff_view, h_resizable, resizable_panel};
use nsh_core::{
    element::desktop::{MenuItem, popup_menu},
    prelude::*,
};

use crate::{
    git,
    hints::{self, Opts},
    state::{Shell, shell},
    theme::BOLT_THEME,
};

/// How often the working tree is looked at while the tab shows: Bolt
/// reloads on file system events (`useDirectoryWatch`, 500 ms debounce).
const POLL: Duration = Duration::from_secs(2);

fn color(c: u32) -> Hsla {
    rgb(c).into()
}

/// What the tab diffs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// The working tree against `HEAD` (`git status`).
    Working,
    /// One commit against its parent.
    Commit(String),
    /// `base..target`.
    Range(String, String),
}

/// One changed file (`ChangedFile`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedFile {
    pub path: String,
    /// A rename's source.
    pub old_path: Option<String>,
    /// Porcelain's X column (a commit's or range's status letter).
    pub staged: char,
    /// Porcelain's Y column.
    pub unstaged: char,
    pub additions: Option<u32>,
    pub deletions: Option<u32>,
}

impl ChangedFile {
    pub fn is_staged(&self) -> bool {
        self.staged != ' ' && self.staged != '?'
    }

    pub fn is_unstaged(&self) -> bool {
        self.unstaged != ' '
    }

    fn is_deleted(&self) -> bool {
        self.staged == 'D' || self.unstaged == 'D'
    }

    /// The letter and color the list shows (`getStatusInfo`): the staged
    /// status when staged, else the unstaged one.
    fn status(&self) -> (char, u32) {
        let code = if self.is_staged() { self.staged } else { self.unstaged };
        match code {
            'M' => ('M', 0xe5c07b),
            'A' => ('A', 0x98c379),
            'D' => ('D', 0xe06c75),
            'R' => ('R', 0x61afef),
            '?' => ('U', 0x98c379),
            'U' => ('C', 0xe06c75),
            other => (other, BOLT_THEME.vars.fg_muted),
        }
    }
}

/// What one load found.
#[derive(Debug, Clone, PartialEq)]
struct Listing {
    mode: Mode,
    files: Vec<ChangedFile>,
}

/// `base..target` as its two ends (`parseRange`).
pub fn parse_range(spec: &str) -> Option<(String, String)> {
    spec.split_once("..").map(|(a, b)| (a.to_owned(), b.to_owned()))
}

/// `git status --porcelain` (v1) as changed files, a rename (`R  a -> b`)
/// under its new path.
pub fn parse_porcelain(out: &str) -> Vec<ChangedFile> {
    out.lines()
        .filter(|l| l.len() >= 4)
        .map(|line| {
            let mut chars = line.chars();
            let (staged, unstaged) = (chars.next().unwrap_or(' '), chars.next().unwrap_or(' '));
            let path = &line[3..];
            let (old_path, path) = match path.split_once(" -> ") {
                Some((from, to)) => (Some(from.to_owned()), to.to_owned()),
                None => (None, path.to_owned()),
            };
            ChangedFile { path, old_path, staged, unstaged, additions: None, deletions: None }
        })
        .collect()
}

/// `git diff --name-status` / `diff-tree --name-status` as changed files,
/// each staged under its status letter.
pub fn parse_name_status(out: &str) -> Vec<ChangedFile> {
    out.lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() < 2 {
                return None;
            }
            let status = parts[0].chars().next()?;
            let path = (*parts.last()?).to_owned();
            let old_path = (status == 'R' && parts.len() >= 3).then(|| parts[1].to_owned());
            Some(ChangedFile { path, old_path, staged: status, unstaged: ' ', additions: None, deletions: None })
        })
        .collect()
}

/// `--numstat` lines onto `files` by path; binary files (`-`) have none.
pub fn apply_numstat(files: &mut [ChangedFile], out: &str) {
    for line in out.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 3 {
            continue;
        }
        let (Ok(add), Ok(del)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>()) else { continue };
        let path = parts[2..].join("\t");
        if let Some(f) = files.iter_mut().find(|f| f.path == path) {
            f.additions = Some(add);
            f.deletions = Some(del);
        }
    }
}

/// The mode for `spec`, or, unpinned, the working tree when it has changes
/// and else the branch since it left `main` / `master` (`HEAD` alone with
/// neither).
fn resolve(dir: &str, spec: Option<&str>) -> Mode {
    if let Some(spec) = spec {
        return match parse_range(spec) {
            Some((base, target)) => Mode::Range(base, target),
            None => Mode::Commit(spec.to_owned()),
        };
    }
    let dirty = git::git(&["status", "--porcelain"], dir).is_ok_and(|s| !s.trim().is_empty());
    if dirty {
        return Mode::Working;
    }
    let base = ["main", "master"].iter().find_map(|b| {
        git::git(&["merge-base", "HEAD", b], dir).ok().map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
    });
    match base {
        Some(base) => Mode::Range(base, "HEAD".into()),
        None => Mode::Commit("HEAD".into()),
    }
}

/// Lists what `spec` touches in `dir` (`loadChangedFilesAtom`).
fn load(dir: &str, spec: Option<&str>) -> Result<Listing, String> {
    let mode = resolve(dir, spec);
    let files = match &mode {
        Mode::Working => {
            let mut files = parse_porcelain(&git::git(&["status", "--porcelain"], dir)?);
            // No HEAD yet (a first commit): no counts.
            if let Ok(stat) = git::git(&["diff", "HEAD", "--numstat"], dir) {
                apply_numstat(&mut files, &stat);
            }
            files
        }
        Mode::Range(base, target) => {
            let mut files = parse_name_status(&git::git(&["diff", "--name-status", base, target], dir)?);
            apply_numstat(&mut files, &git::git(&["diff", "--numstat", base, target], dir)?);
            files
        }
        Mode::Commit(hash) => {
            let tree = |what: &str| git::git(&["diff-tree", "--root", "--no-commit-id", what, "-r", hash], dir);
            let mut files = parse_name_status(&tree("--name-status")?);
            apply_numstat(&mut files, &tree("--numstat")?);
            files
        }
    };
    Ok(Listing { mode, files })
}

/// The file's two sides (`loadFileChangeDataAtom`): before and after, an
/// added file's before and a deleted file's after empty.
fn sides(dir: &str, mode: &Mode, file: &ChangedFile) -> (String, String) {
    let show = |rev: &str, path: &str| git::git(&["show".to_owned(), format!("{rev}:{path}")], dir).unwrap_or_default();
    match mode {
        Mode::Working => {
            let new_file = (file.staged == '?' && file.unstaged == '?') || file.staged == 'A';
            let before = if new_file { String::new() } else { show("HEAD", file.old_path.as_deref().unwrap_or(&file.path)) };
            let after = if file.is_deleted() {
                String::new()
            } else {
                std::fs::read_to_string(std::path::Path::new(dir).join(&file.path)).unwrap_or_default()
            };
            (before, after)
        }
        Mode::Commit(_) | Mode::Range(..) => {
            let (base, target) = match mode {
                Mode::Range(base, target) => (base.clone(), target.clone()),
                Mode::Commit(hash) => (format!("{hash}^"), hash.clone()),
                Mode::Working => unreachable!(),
            };
            let before = if file.staged == 'A' { String::new() } else { show(&base, file.old_path.as_deref().unwrap_or(&file.path)) };
            let after = if file.staged == 'D' { String::new() } else { show(&target, &file.path) };
            (before, after)
        }
    }
}

/// The working tree's state, cheaply: `git status`, and the size and
/// modification time of each file it lists, so an edit to an already
/// changed file counts too. When it changes, the tab loads again.
pub fn fingerprint(dir: &str) -> String {
    let status = git::git(&["status", "--porcelain"], dir).unwrap_or_default();
    let mut print = status.clone();
    for file in parse_porcelain(&status) {
        if let Ok(meta) = std::fs::metadata(std::path::Path::new(dir).join(&file.path)) {
            print.push_str(&format!("\0{}:{:?}", meta.len(), meta.modified().ok()));
        }
    }
    print
}

/// The header's title (`FileListPanel`'s): `Changes`, `Branch a..b` or
/// `Commit a`, hashes cut to seven.
fn title(mode: &Mode) -> String {
    let short = |s: &str| s.chars().take(7).collect::<String>();
    match mode {
        Mode::Working => "Changes".into(),
        Mode::Range(base, target) => format!("Branch {}..{}", short(base), short(target)),
        Mode::Commit(hash) => format!("Commit {}", short(hash)),
    }
}

#[derive(Clone, Copy, PartialEq, Props)]
pub struct ChangesViewProps {
    /// The Changes tab ([`crate::state::Changes::id`]).
    pub tab: Prop<u32>,
}

/// One Changes tab: the list, a divider the user drags, the diff.
#[component]
pub fn ChangesView(props: ChangesViewProps) -> impl IntoElement {
    let shell = shell();
    let id = u64::from(props.tab.get_untracked());
    let tab = computed(move || shell.changes.with(|c| c.iter().find(|c| c.id == id).map(|c| (c.cwd.clone(), c.spec.clone(), c.n))));
    let listing = signal(None::<Result<Listing, String>>);
    let loading = signal(false);
    let selected = signal(None::<String>);
    let sides_shown = signal(None::<(String, String, String)>);

    // Loads on mount, on each `diff` and commit pick (`n`), and on refresh.
    // A working tree load notes what the tree looked like, for the watcher.
    let reload = signal_with(0_u64, SignalEquals::Never);
    let seen = std::rc::Rc::new(std::cell::RefCell::new(None::<String>));
    let noted = seen.clone();
    effect(move || {
        let Some((cwd, spec, _)) = tab.get() else { return };
        reload.get();
        loading.set(true);
        let noted = noted.clone();
        spawn(async move {
            let dir = cwd.clone();
            let (found, print) = git::off_thread(move || {
                let found = load(&dir, spec.as_deref());
                let print = matches!(found, Ok(Listing { mode: Mode::Working, .. })).then(|| fingerprint(&dir));
                (found, print)
            })
            .await;
            *noted.borrow_mut() = print;
            // Keep the selected file when it is still there, else the first.
            let keep = selected.get_untracked();
            let pick = match &found {
                Ok(l) => keep.filter(|p| l.files.iter().any(|f| f.path == *p)).or_else(|| l.files.first().map(|f| f.path.clone())),
                Err(_) => None,
            };
            listing.set(Some(found));
            loading.set(false);
            selected.set(pick);
            show_sides(cwd, listing, selected, sides_shown);
        });
    });

    // The working tree, looked at while the tab shows it (Bolt watches only
    // then, too: a commit or range does not change under it).
    let watcher = spawn(async move {
        loop {
            timer(POLL).await;
            let Some(was) = seen.borrow().clone() else { continue };
            let Some(cwd) = tab.with_untracked(|t| t.as_ref().map(|t| t.0.clone())) else { continue };
            let now = git::off_thread(move || fingerprint(&cwd)).await;
            if now != was {
                reload.set(0);
            }
        }
    });
    cleanup(move || watcher.cancel());

    let v = &BOLT_THEME.vars;
    let (bg, muted) = (v.bg, v.fg_muted);
    let list_width = signal(200.0_f32);
    h_resizable()
        .flex_1()
        .bg(color(bg))
        .handle_accessibility_label("Resize the file list")
        .child(
            resizable_panel()
                .size(move || Some(px(list_width.get())))
                .min_size(px(120.0))
                .fixed(true)
                .on_resize(move |w: &Points| list_width.set(w.0))
                .child(file_list(shell, id, tab, listing, loading, selected, sides_shown, reload)),
        )
        .child(resizable_panel().child(dynamic(move || {
            let empty = |s: &str| {
                view().flex_1().items_center().justify_center().child(text(s.to_owned()).text_sm().text_color(color(muted))).into_element()
            };
            match (sides_shown.get(), listing.get()) {
                (_, Some(Err(e))) => empty(&e),
                (Some((before, after, name)), _) => diff_view()
                    .old_value(before)
                    .new_value(after)
                    .language(name)
                    .font_family(crate::terminals::font_family())
                    .font_size(Points(12.0))
                    .line_height(Points(18.0))
                    .flex_1()
                    .into_element(),
                (None, Some(Ok(l))) if l.files.is_empty() => empty("No changes"),
                (None, None) => empty("Loading..."),
                (None, _) => empty("Select a file"),
            }
        })))
}

/// Loads the selected file's two sides into `shown`.
fn show_sides(
    cwd: String,
    listing: Signal<Option<Result<Listing, String>>>,
    selected: Signal<Option<String>>,
    shown: Signal<Option<(String, String, String)>>,
) {
    let found = listing.with_untracked(|l| {
        let l = l.as_ref()?.as_ref().ok()?;
        let path = selected.get_untracked()?;
        Some((l.mode.clone(), l.files.iter().find(|f| f.path == path)?.clone()))
    });
    let Some((mode, file)) = found else {
        shown.set(None);
        return;
    };
    spawn(async move {
        let name = file.path.rsplit('/').next().unwrap_or(&file.path).to_owned();
        let (before, after) = git::off_thread(move || sides(&cwd, &mode, &file)).await;
        shown.set(Some((before, after, name)));
    });
}

/// The header (title and commit menu, refresh) over the files: in the
/// working tree, Staged Changes then Changes, a file staged and changed
/// again in both.
#[allow(clippy::too_many_arguments)]
fn file_list(
    shell: Shell,
    id: u64,
    tab: Memo<Option<(String, Option<String>, u64)>>,
    listing: Signal<Option<Result<Listing, String>>>,
    loading: Signal<bool>,
    selected: Signal<Option<String>>,
    shown: Signal<Option<(String, String, String)>>,
    reload: Signal<u64>,
) -> impl IntoElement {
    let v = &BOLT_THEME.vars;
    let (muted, faint) = (v.fg_muted, crate::theme::mix(v.fg_muted, 0.65, v.bg));
    let cwd = move || tab.with_untracked(|t| t.as_ref().map(|t| t.0.clone()).unwrap_or_default());
    let select = move |path: String| {
        selected.set(Some(path));
        show_sides(cwd(), listing, selected, shown);
    };
    let header_title = move || {
        listing.with(|l| match l {
            Some(Ok(l)) => format!("{} ({}) ▾", title(&l.mode), l.files.len()),
            _ => "Changes ▾".to_owned(),
        })
    };
    let rows = move || {
        listing.with(|l| {
            let Some(Ok(l)) = l else { return Vec::new() };
            let mut rows = Vec::new();
            if l.mode != Mode::Working {
                rows.extend(l.files.iter().map(|f| Row::File(f.clone(), false)));
                return rows;
            }
            let staged: Vec<&ChangedFile> = l.files.iter().filter(|f| f.is_staged()).collect();
            let unstaged: Vec<&ChangedFile> = l.files.iter().filter(|f| f.is_unstaged()).collect();
            if !staged.is_empty() {
                rows.push(Row::Section(format!("Staged Changes ({})", staged.len())));
                rows.extend(staged.into_iter().map(|f| Row::File(f.clone(), true)));
            }
            if !unstaged.is_empty() {
                rows.push(Row::Section(format!("Changes ({})", unstaged.len())));
                rows.extend(unstaged.into_iter().map(|f| Row::File(f.clone(), false)));
            }
            rows
        })
    };
    view()
        .flex_1()
        .child(
            view()
                .flex_row()
                .items_center()
                .gap_2()
                .px_3()
                .py(px(6.0))
                .child(hints::hint(
                    text(header_title)
                        .text_xs()
                        .font_weight(FontWeight::MEDIUM)
                        .number_of_lines(1)
                        .flex_1()
                        .min_w(px(0.0))
                        .text_color(color(muted))
                        .on_press(move || commit_menu(shell, id, cwd())),
                    "changes",
                    Opts::default(),
                    None,
                    move || commit_menu(shell, id, cwd()),
                ))
                .child(hints::hint(
                    text(move || if loading.get() { "..." } else { "↻" })
                        .text_xs()
                        .px_1()
                        .text_color(color(faint))
                        .on_press(move || reload.set(0)),
                    "changes",
                    Opts::default(),
                    None,
                    move || reload.set(0),
                )),
        )
        .child({
            // Files scrolled out of the list get no ⌘⏎ label.
            let list = element_ref();
            scroll().element_ref(&list).flex_1().child(view().child(each(rows, Row::key, move |row, _| match row.get_untracked() {
                Row::Section(label) => text(label)
                    .uppercase()
                    .text_size(px(10.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .px_3()
                    .py_1()
                    .text_color(color(faint))
                    .into_element(),
                Row::File(file, _) => file_row(shell, cwd(), file, selected, select, list).into_element(),
            })))
        })
}

/// A row of the list: a section's heading, or a file (in the staged
/// section or not: a file can be in both).
#[derive(Debug, Clone, PartialEq)]
enum Row {
    Section(String),
    File(ChangedFile, bool),
}

impl Row {
    /// Everything the row shows: rows are made once, so a row whose
    /// counts or status changed is made again under a new key.
    fn key(&self) -> String {
        match self {
            Row::Section(label) => format!("section:{label}"),
            Row::File(f, staged) => format!("{staged}:{}:{}{}:{:?}:{:?}", f.path, f.staged, f.unstaged, f.additions, f.deletions),
        }
    }
}

/// One file (`FileRow`): a bar at the left edge when selected, its status
/// letter, its name, its counts. A click selects it, a double click opens
/// it; the context menu opens it or copies its path.
fn file_row(
    shell: Shell,
    cwd: String,
    file: ChangedFile,
    selected: Signal<Option<String>>,
    select: impl Fn(String) + Copy + 'static,
    list: ElementRef,
) -> impl IntoElement {
    let v = &BOLT_THEME.vars;
    let (fg, muted, accent) = (v.fg, v.fg_muted, v.accent);
    let (letter, letter_color) = file.status();
    let name = file.path.rsplit('/').next().unwrap_or(&file.path).to_owned();
    let path = file.path.clone();
    let is_selected = {
        let path = path.clone();
        move || selected.with(|s| s.as_deref() == Some(path.as_str()))
    };
    let deleted = file.is_deleted();
    let open = {
        let (cwd, path) = (cwd.clone(), path.clone());
        move || {
            if !deleted {
                let full = std::path::Path::new(&cwd).join(&path);
                if let Err(e) = shell.open_in_helix(cwd.clone(), &full, None) {
                    eprintln!("rivet: {e}");
                }
            }
        }
    };
    let counts = view()
        .flex_row()
        .gap_1()
        .flex_1()
        .justify_end()
        .when_some(file.additions.filter(|n| *n > 0), |v, n| {
            v.child(text(format!("+{n}")).text_size(px(10.0)).font_family("ui-monospace").text_color(color(0x98c379)))
        })
        .when_some(file.deletions.filter(|n| *n > 0), |v, n| {
            v.child(text(format!("-{n}")).text_size(px(10.0)).font_family("ui-monospace").text_color(color(0xe06c75)))
        });
    let press_open = open.clone();
    let menu_path = path.clone();
    let hint_path = path.clone();
    let row = view()
        .flex_row()
        .items_center()
        .gap(px(6.0))
        .pr_3()
        .h(px(24.0))
        .on_press(move |e: &PressEvent| {
            select(path.clone());
            if e.clicks >= 2 {
                press_open();
            }
        })
        .on_context_menu(move |_: &ContextMenuEvent| {
            select(menu_path.clone());
            let open = open.clone();
            let copied = menu_path.clone();
            let mut items = Vec::new();
            if !deleted {
                items.push(MenuItem::new("Open in Editor").on_select(move || open()));
            }
            items.push(MenuItem::new("Copy Path").on_select(move || set_clipboard(copied.clone())));
            let _ = popup_menu(items);
        })
        .child(view().w(px(2.0)).h_full().bg({
            let is_selected = is_selected.clone();
            move || if is_selected() { color(accent) } else { color(accent).opacity(0.0) }
        }))
        .child(
            text(letter.to_string())
                .w(px(12.0))
                .ml(px(10.0))
                .text_size(px(10.0))
                .font_weight(FontWeight::BOLD)
                .font_family("ui-monospace")
                .text_color(color(letter_color)),
        )
        .child(
            text(name)
                .text_xs()
                .number_of_lines(1)
                .flex_shrink(1.0)
                .min_w(px(0.0))
                .text_color(move || color(if is_selected() { fg } else { muted })),
        )
        .child(counts);
    hints::hint(row, "changes", Opts { primary: true, ..Opts::default() }, Some(list), move || select(hint_path.clone()))
}

/// The header's menu (`openCommitMenu`): back to the working tree / branch,
/// or one of the last 30 commits.
fn commit_menu(shell: Shell, id: u64, cwd: String) {
    spawn(async move {
        let log = git::off_thread(move || git::git(&["log", "-n", "30", "--format=%H%x09%s"], &cwd).unwrap_or_default()).await;
        let mut items = vec![MenuItem::new("Working tree / branch").on_select(move || shell.set_changes_spec(id, None))];
        for line in log.lines() {
            let Some((hash, subject)) = line.split_once('\t') else { continue };
            let hash = hash.to_owned();
            let label = format!("{} {subject}", hash.chars().take(7).collect::<String>());
            items.push(MenuItem::new(label).on_select(move || shell.set_changes_spec(id, Some(hash.clone()))));
        }
        let _ = popup_menu(items);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porcelain_keeps_both_columns_and_renames_under_their_new_path() {
        let files = parse_porcelain("MM a.rs\n?? new.txt\nR  old.rs -> moved.rs\n D gone.rs\n");
        assert_eq!(files.len(), 4);
        assert!(files[0].is_staged() && files[0].is_unstaged());
        assert!(!files[1].is_staged() && files[1].is_unstaged());
        assert_eq!(files[1].status().0, 'U');
        assert_eq!((files[2].path.as_str(), files[2].old_path.as_deref()), ("moved.rs", Some("old.rs")));
        assert!(files[3].is_deleted() && !files[3].is_staged());
    }

    #[test]
    fn name_status_and_numstat_give_a_commit_its_files_and_counts() {
        let mut files = parse_name_status("M\tsrc/a.rs\nR100\told.rs\tnew.rs\nA\tbin.png\n");
        apply_numstat(&mut files, "3\t1\tsrc/a.rs\n0\t0\tnew.rs\n-\t-\tbin.png\n");
        assert_eq!((files[0].additions, files[0].deletions), (Some(3), Some(1)));
        assert_eq!((files[1].staged, files[1].old_path.as_deref()), ('R', Some("old.rs")));
        assert_eq!(files[2].additions, None);
    }

    #[test]
    fn titles_name_the_mode_with_short_hashes() {
        assert_eq!(title(&Mode::Working), "Changes");
        assert_eq!(title(&Mode::Commit("abcdef123456".into())), "Commit abcdef1");
        assert_eq!(title(&Mode::Range("1234567890".into(), "HEAD".into())), "Branch 1234567..HEAD");
        assert_eq!(parse_range("a..b"), Some(("a".into(), "b".into())));
        assert_eq!(parse_range("abc"), None);
    }
}
