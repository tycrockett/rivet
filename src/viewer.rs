//! A file tab for the files edited in place (notes, `keybindings.json`;
//! Bolt's Monaco file and diff tabs): the file in nsh-base's `Editor`,
//! saved as it is typed, or its changes against `HEAD` in its `DiffView`.

use std::{path::Path, rc::Rc};

use nsh_base::{DiffLayout, diff_view, editor};
use nsh_core::prelude::*;

use crate::{
    git,
    state::{DocMode, Shell, shell},
    theme::BOLT_THEME,
};

/// Files past this size are not opened: the viewer holds the whole text.
const MAX_BYTES: u64 = 16 << 20;

fn color(c: u32) -> Hsla {
    rgb(c).into()
}

#[derive(Clone, Copy, PartialEq, Props)]
pub struct DocViewProps {
    /// The file tab ([`crate::state::Doc::id`]).
    pub doc: Prop<u32>,
}

/// What a file tab read: the working text, and the text at `HEAD` when
/// the file is tracked there.
#[derive(Debug, Clone, PartialEq)]
struct Loaded {
    text: String,
    head: Option<String>,
}

/// One file tab: a header (its path, File / Diff) over the file or its
/// diff. The text is read each time the tab shows, so switching back to it
/// shows the file as it is now.
#[component]
pub fn DocView(props: DocViewProps) -> impl IntoElement {
    let shell = shell();
    let id = u64::from(props.doc.get_untracked());
    let doc = computed(move || shell.docs.with(|d| d.iter().find(|d| d.id == id).cloned()));
    let Some(first) = doc.get_untracked() else {
        return view().into_element();
    };
    let (cwd, path) = (first.cwd.clone(), first.path.clone());
    let loaded = signal(None::<Result<Loaded, String>>);
    // Saved 400 ms after typing stops (Bolt saves on ⌘S; here nothing
    // asks), and at once if the tab goes away before then.
    let pending = std::rc::Rc::new(std::cell::RefCell::new(None::<String>));
    let write = {
        let path = path.clone();
        move |text: String| {
            if let Err(e) = std::fs::write(&path, text) {
                eprintln!("rivet: could not save {path}: {e}");
            }
        }
    };
    cleanup({
        let (pending, write) = (pending.clone(), write.clone());
        move || {
            if let Some(text) = pending.borrow_mut().take() {
                write(text);
            }
        }
    });
    let save: Rc<dyn Fn(String)> = {
        let generation = Rc::new(std::cell::Cell::new(0_u64));
        Rc::new(move |text: String| {
            *pending.borrow_mut() = Some(text);
            let n = generation.get() + 1;
            generation.set(n);
            let (pending, generation, write) = (pending.clone(), generation.clone(), write.clone());
            set_timeout(std::time::Duration::from_millis(400), move || {
                if generation.get() != n {
                    return;
                }
                if let Some(text) = pending.borrow_mut().take() {
                    let write = write.clone();
                    spawn(async move { git::off_thread(move || write(text)).await });
                }
            });
        })
    };

    spawn({
        let path = path.clone();
        async move {
            let result = git::off_thread(move || load(&path)).await;
            loaded.set(Some(result));
        }
    });

    let changed = computed(move || loaded.with(|l| matches!(l, Some(Ok(Loaded { text, head: Some(head) })) if text != head)));
    let mode = computed(move || doc.with(|d| d.as_ref().map_or(DocMode::File, |d| d.mode)));
    let rel = Path::new(&path).strip_prefix(&cwd).map_or_else(|_| path.clone(), |p| p.to_string_lossy().into_owned());
    let language = Path::new(&path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let v = &BOLT_THEME.vars;
    let (bg, muted) = (v.bg, v.fg_muted);

    let body = dynamic(move || {
        let mode = mode.get();
        let language = language.clone();
        loaded.with(|l| match l {
            None => view().into_element(),
            Some(Err(e)) => text(e.clone()).p_4().text_sm().text_color(color(muted)).into_element(),
            Some(Ok(Loaded { text, head })) => match mode {
                DocMode::Diff { unified } if head.is_some() => diff_view()
                    .old_value(head.clone().unwrap_or_default())
                    .new_value(text.clone())
                    .language(language)
                    .layout(if unified { DiffLayout::Unified } else { DiffLayout::Split })
                    .font_family(crate::terminals::font_family())
                    .font_size(Points(12.0))
                    .line_height(Points(18.0))
                    .flex_1()
                    .into_element(),
                _ => {
                    // The editor owns the text once typed into: what it
                    // starts from, not what it is held to.
                    let save = save.clone();
                    editor()
                        .language(language)
                        .font_family(crate::terminals::font_family())
                        .font_size(Points(12.0))
                        .line_height(Points(18.0))
                        .default_value(text.clone())
                        .on_change(move |t: &SharedString| save(t.to_string()))
                        .flex_1()
                        .into_element()
                }
            },
        })
    });

    view()
        .flex_1()
        .bg(color(bg))
        .child(
            view()
                .flex_row()
                .items_center()
                .gap_2()
                .h(px(32.0))
                .px_3()
                .child(text(rel).text_xs().number_of_lines(1).flex_shrink(1.0).min_w(px(0.0)).text_color(color(muted)))
                .child(view().flex_1())
                .child(when(changed, move || mode_switch(shell, id, mode))),
        )
        .child(body)
        .into_element()
}

/// File / Diff, and Split / Unified while the diff shows.
fn mode_switch(shell: Shell, id: u64, mode: Memo<DocMode>) -> impl IntoElement {
    let v = &BOLT_THEME.vars;
    let (fg, muted, widget) = (v.fg, v.fg_muted, v.bg_widget);
    let pill = move |label: &'static str, on: fn(DocMode) -> bool, to: DocMode| {
        text(label)
            .text_xs()
            .px_2()
            .py(px(2.0))
            .rounded(px(6.0))
            .bg(move || if on(mode.get()) { color(widget) } else { color(widget).opacity(0.0) })
            .text_color(move || color(if on(mode.get()) { fg } else { muted }))
            .on_press(move || shell.set_doc_mode(id, to))
    };
    view()
        .flex_row()
        .gap_1()
        .child(pill("File", |m| m == DocMode::File, DocMode::File))
        .child(pill("Diff", |m| matches!(m, DocMode::Diff { .. }), DocMode::Diff { unified: false }))
        .child(when(
            move || matches!(mode.get(), DocMode::Diff { .. }),
            move || {
                view()
                    .flex_row()
                    .gap_1()
                    .ml_2()
                    .child(pill("Split", |m| m == DocMode::Diff { unified: false }, DocMode::Diff { unified: false }))
                    .child(pill("Unified", |m| m == DocMode::Diff { unified: true }, DocMode::Diff { unified: true }))
            },
        ))
}

/// Reads the file and, when git tracks it, its text at `HEAD`.
fn load(path: &str) -> Result<Loaded, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("Cannot read {path}: {e}"))?;
    if meta.len() > MAX_BYTES {
        return Err(format!("{path} is too large to show ({} MB)", meta.len() >> 20));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("Cannot read {path}: {e}"))?;
    let text = String::from_utf8(bytes).map_err(|_| format!("{path} is not a text file"))?;
    let file = Path::new(path);
    let head = match (file.parent(), file.file_name()) {
        (Some(dir), Some(name)) => {
            git::git(&["show".to_owned(), format!("HEAD:./{}", name.to_string_lossy())], &dir.to_string_lossy()).ok()
        }
        _ => None,
    };
    Ok(Loaded { text, head })
}
