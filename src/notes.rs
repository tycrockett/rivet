//! Bolt notes (`notesStore`, `useNotesPoll`, `NotesStrip`): `.md` files in
//! the Electron Bolt's `bolt-notes` folder, shared by every terminal and
//! written by agents through its `saveNote` tool. Under an omp terminal a
//! strip shows one tile per note, newest edit first; a note that
//! @mentions this terminal or its workspace is outlined and moved to the
//! front. A terminal in a group's home or one of its tabs also gets that
//! group's memories, as one tile with a count. Tiles that do not fit the
//! row go behind a ⋯ tile.

use std::{collections::HashMap, path::PathBuf, time::Duration};

use nsh_core::{
    element::{
        desktop::{MenuItem, popup_menu},
        measure_text,
    },
    prelude::*,
};

use crate::{
    git,
    hub::{self, TargetKind, engine},
    settings::{self, Tab},
    state::{self, Shell},
    theme::{BOLT_THEME, mix},
    workspace::group_key,
};

#[derive(Debug, Clone, PartialEq)]
pub struct Note {
    pub name: String,
    pub path: String,
    mtime: f64,
    pub mentions: Vec<String>,
    /// The signature's `@handle · repo (branch)`: who wrote it.
    pub from: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Memory {
    pub name: String,
    pub path: String,
    mtime: f64,
}

#[derive(Clone, Copy)]
pub struct Notes {
    pub notes: Signal<Vec<Note>>,
    /// Each group home's memories, by the home's path.
    pub memories: Signal<HashMap<String, Vec<Memory>>>,
    /// Terminals running omp: the only ones that show the strip.
    pub omp: Signal<Vec<u64>>,
}

fn notes_dir() -> PathBuf {
    settings::bolt_data_dir().join("bolt-notes")
}

/// `group-sessions/<home path, sanitized>/memories` (`groupMemoriesDir`).
fn memories_dir(home: &str) -> PathBuf {
    let key: String = home
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '-' })
        .collect();
    settings::bolt_data_dir().join("group-sessions").join(key).join("memories")
}

/// The @handles in a note (`noteMentions`): `@backend` or `@backend/k3f9`,
/// lowercased, not an email's `a@b` nor a path's `…/@scope`.
pub fn mentions(text: &str) -> Vec<String> {
    let segment = |s: &str| -> usize {
        let mut chars = s.char_indices();
        match chars.next() {
            Some((_, c)) if c.is_ascii_alphanumeric() => {}
            _ => return 0,
        }
        s.char_indices()
            .find(|(_, c)| !(c.is_ascii_alphanumeric() || *c == '-'))
            .map_or(s.len(), |(i, _)| i)
    };
    let mut out: Vec<String> = Vec::new();
    for (at, _) in text.match_indices('@') {
        let before = text[..at].chars().next_back();
        if before.is_some_and(|c| c.is_alphanumeric() || c == '_' || matches!(c, '.' | '@' | '/')) {
            continue;
        }
        let rest = &text[at + 1..];
        let first = segment(rest);
        if first == 0 {
            continue;
        }
        let mut len = first;
        if let Some(tail) = rest[first..].strip_prefix('/') {
            let second = segment(tail);
            if second > 0 {
                len += 1 + second;
            }
        }
        let h = rest[..len].to_lowercase();
        if !out.contains(&h) {
            out.push(h);
        }
    }
    out
}

/// The note without its last-line signature, and who signed it
/// (`splitSignature`: `\n*— from (.+?)\s*$`).
pub fn split_signature(text: &str) -> (&str, Option<String>) {
    let trimmed = text.trim_end();
    if let Some(at) = trimmed.rfind("— from ") {
        let from = &trimmed[at + "— from ".len()..];
        if !from.is_empty() && !from.contains('\n') {
            let body = trimmed[..at].trim_end_matches('\n');
            return (body, Some(from.to_owned()));
        }
    }
    (text, None)
}

fn mtime(path: &std::path::Path) -> Option<f64> {
    let meta = std::fs::metadata(path).ok().filter(|m| m.is_file())?;
    let t = meta.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(t.as_secs_f64() * 1000.0)
}

fn md_files(dir: &std::path::Path) -> Vec<(String, PathBuf, f64)> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<(String, PathBuf, f64)> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let stem = name.strip_suffix(".md").filter(|_| !name.starts_with('.'))?.to_owned();
            let path = e.path();
            mtime(&path).map(|m| (stem, path, m))
        })
        .collect();
    out.sort_by(|a, b| b.2.total_cmp(&a.2));
    out
}

/// The notes, newest edit first; a file's text is read again only when its
/// mtime changed (`refreshNotesAtom`).
fn read_notes(prev: Vec<Note>) -> Vec<Note> {
    md_files(&notes_dir())
        .into_iter()
        .map(|(name, path, m)| {
            let path = path.to_string_lossy().into_owned();
            if let Some(hit) = prev.iter().find(|n| n.path == path && n.mtime == m) {
                return hit.clone();
            }
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            let (body, from) = split_signature(&text);
            Note { name, mentions: mentions(body), path, mtime: m, from }
        })
        .collect()
}

/// A group's home: its hidden tab flagged `home` (`groupHomeOf`).
fn group_home<'a>(tabs: &'a [Tab], group: &str, groups: &[String]) -> Option<&'a Tab> {
    tabs.iter().find(|t| t.home && t.hidden && group_key(t, groups) == Some(group))
}

/// Makes the notes state and keeps it current: terminal agents every 1.5s
/// (Bolt's status poll), the notes and memories every 3s while an omp runs
/// (`useNotesPoll`).
pub fn provide(shell: Shell) -> Notes {
    let notes = Notes { notes: signal(Vec::new()), memories: signal(HashMap::new()), omp: signal(Vec::new()) };
    provide_context(notes);
    set_interval(Duration::from_millis(1500), move || {
        let ids: Vec<u64> = shell.terms.with_untracked(|t| t.iter().map(|t| t.id).collect());
        let omp: Vec<u64> = ids.into_iter().filter(|id| engine::terminal_status(*id).agent.as_deref() == Some("omp")).collect();
        notes.omp.set(omp);
    });
    let tick = signal(0_u32);
    set_interval(Duration::from_millis(3000), move || tick.update(|n| *n += 1));
    effect(move || {
        tick.get();
        if notes.omp.with(Vec::is_empty) {
            return;
        }
        let prev = notes.notes.get_untracked();
        let homes: Vec<String> = shell.settings.with_untracked(|s| {
            s.groups.iter().filter_map(|g| group_home(&s.tabs, g, &s.groups)).map(|t| t.path.clone()).collect()
        });
        spawn(async move {
            let (next, memories) = git::off_thread(move || {
                let memories: HashMap<String, Vec<Memory>> = homes
                    .into_iter()
                    .map(|h| {
                        let list = md_files(&memories_dir(&h))
                            .into_iter()
                            .map(|(name, path, m)| Memory { name, path: path.to_string_lossy().into_owned(), mtime: m })
                            .collect();
                        (h, list)
                    })
                    .collect();
                (read_notes(prev), memories)
            })
            .await;
            notes.notes.set(next);
            notes.memories.set(memories);
        });
    });
    notes
}

pub fn notes() -> Notes {
    use_context::<Notes>().expect("notes are provided by App")
}

const TILE_GAP: f32 = 6.0;
const MORE_WIDTH: f32 = 24.0;
const TILE_MAX: f32 = 180.0;
const OMP_START_TIMEOUT_MS: f64 = 30_000.0;
const OMP_READY_CHECK_MS: u64 = 250;

#[derive(Clone, PartialEq)]
enum Item {
    Note(Note, bool),
    Memories,
}

fn color(c: u32) -> Hsla {
    rgb(c).into()
}

/// The strip under terminal `id`, whose shell runs in `cwd`.
pub fn strip(shell: Shell, id: u64, session_id: String, cwd: String) -> impl IntoElement {
    let state = notes();
    let width = signal(0.0_f32);
    let is_omp = move || state.omp.with(|o| o.contains(&id));
    // The deepest tab holding this cwd decides the group, so a home nested
    // under a member still wins.
    let group_memories = {
        let cwd = cwd.clone();
        move || -> (Option<String>, Vec<Memory>) {
            let s = shell.settings.get();
            let owner = s
                .tabs
                .iter()
                .filter(|t| cwd == t.path || cwd.starts_with(&format!("{}/", t.path)))
                .max_by_key(|t| t.path.len());
            let Some(group) = owner.and_then(|t| group_key(t, &s.groups)).map(str::to_owned) else {
                return (None, Vec::new());
            };
            let home = group_home(&s.tabs, &group, &s.groups).map(|t| t.path.clone());
            let memories = home.and_then(|h| state.memories.with(|m| m.get(&h).cloned())).unwrap_or_default();
            (Some(group), memories)
        }
    };
    let items = {
        let group_memories = group_memories.clone();
        move || -> Vec<Item> {
            let handle = hub::targets(shell)
                .into_iter()
                .find(|t| t.kind == TargetKind::Terminal && t.members[0].session_id.as_deref() == Some(session_id.as_str()))
                .map(|t| t.handle);
            let mine = |m: &[String]| {
                handle.as_ref().is_some_and(|h| m.contains(h) || m.iter().any(|x| Some(x.as_str()) == h.split('/').next()))
            };
            let all = state.notes.get();
            let (hits, rest): (Vec<Note>, Vec<Note>) = all.into_iter().partition(|n| mine(&n.mentions));
            let mut out: Vec<Item> = hits.into_iter().map(|n| Item::Note(n, true)).collect();
            if !group_memories().1.is_empty() {
                out.push(Item::Memories);
            }
            out.extend(rest.into_iter().map(|n| Item::Note(n, false)));
            out
        }
    };
    let font = "11px system-ui".to_owned();
    // How many tiles fit the row beside the ⋯, from their widths.
    let shown = {
        let items = items.clone();
        move || -> usize {
            let room = width.get();
            let widths: Vec<f32> = items()
                .iter()
                .map(|it| match it {
                    Item::Note(n, _) => (measure_text(&font, &n.name).0 + 16.0).min(TILE_MAX),
                    Item::Memories => 24.0,
                })
                .collect();
            let total: f32 = widths.iter().enumerate().map(|(i, w)| w + if i > 0 { TILE_GAP } else { 0.0 }).sum();
            if total <= room {
                return widths.len();
            }
            let mut used = MORE_WIDTH;
            let mut k = 0;
            while k < widths.len() && used + TILE_GAP + widths[k] <= room {
                used += TILE_GAP + widths[k];
                k += 1;
            }
            k
        }
    };
    let visible = {
        let items = items.clone();
        move || is_omp() && !items().is_empty()
    };
    let v = &BOLT_THEME.vars;
    let (surface, fg, accent, info) = (v.bg_surface, v.fg, v.accent, v.info);
    let tile_bg = move |on: bool| if on { mix(accent, 0.18, surface) } else { surface };
    let row_items = items.clone();
    view()
        .h(px(24.0))
        .flex_row()
        .gap(px(TILE_GAP))
        .overflow_hidden()
        .on_layout(move |b: &Bounds<Points>| width.set(b.size.width.0))
        .app_class(move || Some(SharedString::from(if visible() { "flex" } else { "hidden" })))
        .child(dynamic(move || {
            let all = row_items();
            let n = shown();
            let cwd = cwd.clone();
            let gm = group_memories.clone();
            let mut tiles: Vec<AnyElement> = all
                .iter()
                .take(n)
                .map(|it| match it {
                    Item::Note(note, hit) => {
                        let (path, hit) = (note.path.clone(), *hit);
                        let cwd = cwd.clone();
                        text(note.name.clone())
                            .h(px(24.0))
                            .px_2()
                            .max_w(px(TILE_MAX))
                            .rounded(px(4.0))
                            .number_of_lines(1)
                            .text_size(px(11.0))
                            .line_height(px(24.0))
                            .bg(color(tile_bg(hit)))
                            .text_color(color(fg))
                            .when(hit, |t| t.border_1().border_color(color(accent)))
                            .hover(move |s| s.bg(color(tile_bg(true))))
                            .on_press(move || note_menu(shell, id, &cwd, &path))
                            .into_element()
                    }
                    Item::Memories => {
                        let gm = gm.clone();
                        let count = gm().1.len();
                        view()
                            .w(px(24.0))
                            .h(px(24.0))
                            .rounded(px(4.0))
                            .items_center()
                            .justify_center()
                            .bg(color(surface))
                            .hover(move |s| s.bg(color(tile_bg(true))))
                            .on_press(move || memories_menu(gm().1))
                            .child(text("◈").text_size(px(13.0)).text_color(color(info)))
                            .child(
                                text(count.to_string())
                                    .absolute()
                                    .top(px(1.0))
                                    .right(px(1.0))
                                    .px(px(2.0))
                                    .rounded_full()
                                    .text_size(px(8.0))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .bg(color(info))
                                    .text_color(color(surface)),
                            )
                            .into_element()
                    }
                })
                .collect();
            if n < all.len() {
                let hidden: Vec<Item> = all[n..].to_vec();
                let cwd = cwd.clone();
                let gm = gm.clone();
                tiles.push(
                    text("⋯")
                        .w(px(MORE_WIDTH))
                        .h(px(24.0))
                        .rounded(px(4.0))
                        .text_center()
                        .line_height(px(24.0))
                        .bg(color(surface))
                        .text_color(color(fg))
                        .hover(move |s| s.bg(color(tile_bg(true))))
                        .on_press(move || more_menu(shell, id, &cwd, &hidden, gm().1))
                        .into_element(),
                );
            }
            tiles
        }))
}

/// A note's actions (the bubble's): inject and remove, inject, inject into
/// a new omp, open, remove.
fn note_actions(shell: Shell, id: u64, cwd: &str, path: &str) -> Vec<MenuItem> {
    let (p1, p2, p3, p4, p5) = (path.to_owned(), path.to_owned(), path.to_owned(), path.to_owned(), path.to_owned());
    let cwd = cwd.to_owned();
    vec![
        MenuItem::new("Inject and remove").on_select(move || {
            if inject(id, &p1) {
                remove(&p1);
            }
        }),
        MenuItem::new("Inject").on_select(move || {
            inject(id, &p2);
        }),
        MenuItem::new("Inject into new omp").on_select(move || inject_into_new_omp(shell, &cwd, &p3)),
        MenuItem::new("Open").on_select(move || {
            let _ = open_path(p4.clone());
        }),
        MenuItem::new("Remove").destructive(true).on_select(move || remove(&p5)),
    ]
}

fn note_menu(shell: Shell, id: u64, cwd: &str, path: &str) {
    let _ = popup_menu(note_actions(shell, id, cwd, path));
}

fn memories_menu(memories: Vec<Memory>) {
    let _ = popup_menu(memory_items(memories));
}

fn memory_items(memories: Vec<Memory>) -> Vec<MenuItem> {
    memories
        .into_iter()
        .map(|m| {
            let path = m.path;
            MenuItem::new(m.name).on_select(move || {
                let _ = open_path(path.clone());
            })
        })
        .collect()
}

/// The ⋯ tile's list: a hidden note opens its actions, the memories their
/// files.
fn more_menu(shell: Shell, id: u64, cwd: &str, hidden: &[Item], memories: Vec<Memory>) {
    let mut items = Vec::new();
    for it in hidden {
        match it {
            Item::Note(n, _) => items.push(MenuItem::submenu(n.name.clone(), note_actions(shell, id, cwd, &n.path))),
            Item::Memories => items.extend(memory_items(memories.clone())),
        }
    }
    let _ = popup_menu(items);
}

/// Pastes the note into terminal `id`'s prompt without submitting it.
fn inject(id: u64, path: &str) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else { return false };
    state::paste(id, text.trim_end());
    state::focus_terminal(id);
    true
}

fn remove(path: &str) {
    if let Err(e) = std::fs::remove_file(path) {
        eprintln!("rivet: could not remove note {path}: {e}");
    }
}

/// A fresh terminal here, `omp` typed into it, and the note pasted once omp
/// is up: the status names omp and the program turned bracketed paste on
/// (the shell turns it off to run a command, so it being on again means
/// omp's editor asked for it).
fn inject_into_new_omp(shell: Shell, cwd: &str, path: &str) {
    let Ok(text) = std::fs::read_to_string(path) else { return };
    let text = text.trim_end().to_owned();
    let Ok(new) = shell.open_terminal(cwd.to_owned()) else { return };
    state::pty_write(new, "omp\r");
    let started = nsh_core::time::unix_ms();
    spawn(async move {
        while nsh_core::time::unix_ms() - started <= OMP_START_TIMEOUT_MS {
            timer(Duration::from_millis(OMP_READY_CHECK_MS)).await;
            if engine::terminal_status(new).agent.as_deref() == Some("omp") && state::bracketed(new) {
                state::paste(new, &text);
                state::focus_terminal(new);
                return;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mentions_skip_emails_and_scoped_paths() {
        let text = "ask @Backend/k3f9 and @web, not a@b.com or node_modules/@scope/pkg; @web again";
        assert_eq!(mentions(text), ["backend/k3f9", "web"]);
    }

    #[test]
    fn signature_is_the_last_line() {
        let (body, from) = split_signature("do the thing\n\n— from @api · backend (main)\n");
        assert_eq!(body, "do the thing");
        assert_eq!(from.as_deref(), Some("@api · backend (main)"));
        assert_eq!(split_signature("no signature").1, None);
    }
}
