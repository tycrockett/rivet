//! The editor tools of Bolt's MCP server (`useMcpBridge.ts`'s `openFile`,
//! `getOpenEditors`, `saveDocument`, `checkDocumentDirty`, `openDiff`,
//! `close_tab`, `closeAllDiffTabs`) over the current workspace's columns: a
//! file opens as a file tab ([`Shell::open_editable_at`], `viewer.rs`), a
//! diff as a tab over the text proposed ([`Shell::open_proposal`]).
//!
//! Bolt's tabs are named by their path (a diff's `diff:<path>`); Rivet's
//! are numbered, so each tool finds a tab by the path or name it was given.

use std::path::Path;

use serde_json::{Value, json};

use super::ToolResult;
use crate::{
    git::off_thread,
    state::{Proposal, Select, Shell},
    viewer,
};

/// Files past this size are not read for a range (`viewer.rs`'s limit).
const MAX_BYTES: u64 = 16 << 20;

pub async fn call(shell: Shell, name: &str, args: &Value) -> Option<ToolResult> {
    Some(match name {
        "openFile" => open_file(shell, args).await,
        "getOpenEditors" => Ok(open_editors(shell)),
        "saveDocument" => save_document(shell, args),
        "checkDocumentDirty" => check_document_dirty(shell, args),
        "openDiff" => open_diff(shell, args).await,
        "close_tab" => close_tab(shell, args),
        "closeAllDiffTabs" => Ok(close_all_diff_tabs(shell)),
        _ => return None,
    })
}

/// A string argument that is there and not empty (Bolt tests `!arg`).
fn text_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// `path` as the workspace the tools act on sees it: a relative one is the
/// current directory's.
fn resolve(shell: Shell, path: &str) -> String {
    if Path::new(path).is_absolute() {
        path.to_owned()
    } else {
        Path::new(&shell.directory()).join(path).to_string_lossy().into_owned()
    }
}

/// `openFile` (`handleOpenFile`): opens the file and, given `startText`,
/// selects from its first occurrence to the first `endText` after it (the
/// end of `startText` without one), to the end of that line with
/// `selectToEndOfLine`. Bolt only moves the caret to `startText`.
/// `preview` and `makeFrontmost` (the window's front) have nothing to do
/// here, as in Bolt.
async fn open_file(shell: Shell, args: &Value) -> ToolResult {
    let Some(file_path) = text_arg(args, "filePath") else {
        return Err("filePath is required".into());
    };
    let path = resolve(shell, file_path);
    let start = text_arg(args, "startText").map(str::to_owned);
    let end = text_arg(args, "endText").map(str::to_owned);
    let to_eol = args.get("selectToEndOfLine").and_then(Value::as_bool).unwrap_or(false);
    let read = path.clone();
    let (select, note) = off_thread(move || inspect(&read, start.as_deref(), end.as_deref(), to_eol)).await?;
    shell.open_editable_at(shell.directory(), path, select);
    Ok(match note {
        Some(note) => format!("Opened {file_path} ({note})"),
        None => format!("Opened {file_path}"),
    })
}

/// Checks `path` is a file the viewer can open and, when markers were
/// given, finds the range they name; the note says why there is none.
fn inspect(path: &str, start: Option<&str>, end: Option<&str>, to_eol: bool) -> Result<(Option<Select>, Option<String>), String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("Cannot read {path}: {e}"))?;
    if !meta.is_file() {
        return Err(format!("Cannot open {path}: not a file"));
    }
    if start.is_none() && end.is_none() {
        return Ok((None, None));
    }
    if meta.len() > MAX_BYTES {
        return Ok((None, Some("nothing selected: the file is too large".into())));
    }
    let Ok(text) = std::fs::read(path).map(String::from_utf8) else {
        return Ok((None, Some("nothing selected: the file could not be read".into())));
    };
    let Ok(text) = text else {
        return Ok((None, Some("nothing selected: not a text file".into())));
    };
    Ok(find_range(&text, start, end, to_eol))
}

/// The range `start`, `end` and `to_eol` name in `text`, and the note on
/// what was not found.
fn find_range(text: &str, start: Option<&str>, end: Option<&str>, to_eol: bool) -> (Option<Select>, Option<String>) {
    let (from, mut to) = match start {
        Some(s) => match text.find(s) {
            Some(i) => (i, i + s.len()),
            None => return (None, Some("nothing selected: startText not found".into())),
        },
        None => (0, 0),
    };
    let mut note = None;
    if let Some(e) = end {
        match text[from..].find(e) {
            Some(i) => to = from + i + e.len(),
            None => note = Some("endText not found".to_owned()),
        }
    }
    if to_eol {
        let line_end = text[to..].find('\n').map_or(text.len(), |i| to + i);
        to = if text[..line_end].ends_with('\r') { line_end - 1 } else { line_end };
        to = to.max(from);
    }
    (Some(Select { anchor: position(text, from), head: position(text, to) }), note)
}

/// `offset`'s line and column from 0, the column in UTF-16 code units.
fn position(text: &str, offset: usize) -> (u32, u32) {
    let before = &text[..offset];
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    (before.matches('\n').count() as u32, before[line_start..].encode_utf16().count() as u32)
}

/// What the tools know of a tab, by its id.
struct Tab {
    /// Bolt's `mode`.
    mode: &'static str,
    /// Bolt's `filePath || id`.
    file_path: String,
    /// What `close_tab` answers to: Bolt's id, path and file name.
    names: Vec<String>,
    dirty: bool,
}

/// Tab `id` of any workspace, as a Bolt tab: a terminal's `filePath` is its
/// directory; a Changes or Settings tab has no path, so its id's name.
fn describe(shell: Shell, id: u64) -> Option<Tab> {
    if let Some(tab) = shell.terms.with_untracked(|t| {
        t.iter().find(|t| t.id == id).map(|t| Tab {
            mode: "terminal",
            file_path: t.cwd.clone(),
            // Not the directory: its name would close a shell, and a path
            // already sits in the id of every other kind of tab.
            names: vec![t.session_id.clone()],
            dirty: false,
        })
    }) {
        return Some(tab);
    }
    if let Some(tab) = shell.docs.with_untracked(|d| {
        d.iter().find(|d| d.id == id).map(|d| {
            let base = Path::new(&d.path).file_name().map_or_else(|| d.path.clone(), |n| n.to_string_lossy().into_owned());
            match &d.proposal {
                Some(p) => Tab {
                    mode: "diff",
                    file_path: d.path.clone(),
                    names: [Some(format!("diff:{}", d.path)), Some(d.path.clone()), Some(base), p.name.clone()].into_iter().flatten().collect(),
                    dirty: false,
                },
                None => Tab { mode: "file", file_path: d.path.clone(), names: vec![d.path.clone(), base], dirty: viewer::is_dirty(id) },
            }
        })
    }) {
        return Some(tab);
    }
    if let Some(cwd) = shell.changes.with_untracked(|c| c.iter().find(|c| c.id == id).map(|c| c.cwd.clone())) {
        return Some(Tab { mode: "changes", file_path: cwd, names: Vec::new(), dirty: false });
    }
    shell
        .settings_tab
        .with_untracked(|s| s.as_ref().is_some_and(|(s, _)| *s == id))
        .then(|| Tab { mode: "settings", file_path: "settings".into(), names: Vec::new(), dirty: false })
}

/// `getOpenEditors` (`handleGetOpenEditors`): every tab of the current
/// workspace's columns, left to right, as `{ filePath, isActive, isDirty,
/// mode }`; a tab is dirty here for the 400 ms between typing and its save.
fn open_editors(shell: Shell) -> String {
    let cwd = shell.directory();
    let cols = shell.columns.with_untracked(|c| c.get(&cwd).cloned()).unwrap_or_default();
    let mut editors = Vec::new();
    for group in &cols.groups {
        for (i, id) in group.tabs.iter().enumerate() {
            let Some(tab) = describe(shell, *id) else { continue };
            editors.push(json!({
                "filePath": tab.file_path,
                "isActive": group.id == cols.active_group && i == group.active,
                "isDirty": tab.dirty,
                "mode": tab.mode,
            }));
        }
    }
    Value::Array(editors).to_string()
}

/// The file tab open on `path`, in any workspace (Bolt matches the tab by
/// its path). A diff tab has the path of its file too, so `any` lets it
/// count.
fn doc_at(shell: Shell, path: &str, any: bool) -> Option<u64> {
    shell.docs.with_untracked(|d| {
        d.iter().find(|d| (any || d.proposal.is_none()) && Path::new(&d.path) == Path::new(path)).map(|d| d.id)
    })
}

/// `saveDocument` (`handleSaveDocument`): writes what was typed and not yet
/// saved; `File not open` when no file tab shows the path.
fn save_document(shell: Shell, args: &Value) -> ToolResult {
    let Some(file_path) = text_arg(args, "filePath") else {
        return Err("filePath is required".into());
    };
    match doc_at(shell, &resolve(shell, file_path), false) {
        Some(id) => {
            viewer::flush(id);
            Ok(format!("Saved {file_path}"))
        }
        None => Err(format!("File not open: {file_path}")),
    }
}

/// `checkDocumentDirty` (`handleCheckDocumentDirty`): `{ isDirty }`, or
/// `{ isDirty: false, notOpen: true }` for a path no tab shows.
fn check_document_dirty(shell: Shell, args: &Value) -> ToolResult {
    let Some(file_path) = text_arg(args, "filePath") else {
        return Err("filePath is required".into());
    };
    Ok(match doc_at(shell, &resolve(shell, file_path), true) {
        Some(id) => json!({ "isDirty": describe(shell, id).is_some_and(|t| t.dirty) }),
        None => json!({ "isDirty": false, "notOpen": true }),
    }
    .to_string())
}

/// `openDiff` (`handleOpenDiff`): a diff tab of `old_file_path` as it is now
/// (empty when it does not exist yet) against `new_file_contents`. Like
/// Bolt's, it answers at once: nothing waits for the diff to be accepted
/// or rejected, and `new_file_path` is not read. `tab_name` is not shown
/// but `close_tab` answers to it. Opening the same file's diff again shows
/// the tab with the new text.
async fn open_diff(shell: Shell, args: &Value) -> ToolResult {
    let Some(old_path) = text_arg(args, "old_file_path") else {
        return Err("old_file_path is required".into());
    };
    let path = resolve(shell, old_path);
    let modified = args.get("new_file_contents").and_then(Value::as_str).unwrap_or_default().to_owned();
    let name = text_arg(args, "tab_name").map(str::to_owned);
    let read = path.clone();
    let original = off_thread(move || std::fs::read_to_string(read).unwrap_or_default()).await;
    shell.open_proposal(shell.directory(), path, Proposal { name, original, modified });
    Ok(format!("Opened diff for {old_path}"))
}

/// `close_tab` (`handleCloseTab`): the first tab whose id, path or file
/// name is `tab_name`, the current workspace's first, then the others'. A
/// terminal answers to its session id only: Bolt's match on a directory's
/// name would close a shell for it.
fn close_tab(shell: Shell, args: &Value) -> ToolResult {
    let Some(tab_name) = text_arg(args, "tab_name") else {
        return Err("tab_name is required".into());
    };
    let resolved = resolve(shell, tab_name);
    let cwd = shell.directory();
    let mut others: Vec<String> = shell.columns.with_untracked(|c| c.keys().filter(|k| **k != cwd).cloned().collect());
    others.sort();
    let ids = std::iter::once(cwd).chain(others).flat_map(|cwd| shell.tabs_of(&cwd));
    for id in ids {
        if describe(shell, id).is_some_and(|t| t.names.iter().any(|n| n == tab_name || *n == resolved)) {
            shell.close_tab(id);
            return Ok(format!("Closed tab: {tab_name}"));
        }
    }
    Err(format!("Tab not found: {tab_name}"))
}

/// `closeAllDiffTabs` (`handleCloseAllDiffTabs`): every diff tab, in every
/// workspace (they are the ones `openDiff` made, and the workspace showing
/// when it ran need not be the one showing now).
fn close_all_diff_tabs(shell: Shell) -> String {
    let ids: Vec<u64> = shell.docs.with_untracked(|d| d.iter().filter(|d| d.proposal.is_some()).map(|d| d.id).collect());
    for id in &ids {
        shell.close_tab(*id);
    }
    format!("Closed {} diff tab(s)", ids.len())
}
