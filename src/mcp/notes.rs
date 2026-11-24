//! The `saveNote` tool (`handleSaveNote`, `noteSignature`, `noteFileName`,
//! `signNote`, `writeNote`): writes a rivet note, a `.md` in the
//! `bolt-notes` folder shared with the Electron Bolt or, with `scope`
//! "group", in the caller's group's notes folder; signs it with the
//! caller's handle and repo, and nudges the notes strip to re-read it.

use std::path::PathBuf;

use serde_json::Value;

use super::ToolResult;
use crate::{
    git,
    hub::{self, TargetKind, engine},
    notes,
    state::Shell,
};

/// The file name for a note name (`noteFileName`): one path segment, always
/// `.md`. A name that already exists is replaced, which is how a note is
/// edited.
fn note_file_name(name: &str) -> Option<String> {
    let base = name.len().checked_sub(3).and_then(|i| name.get(i..)).filter(|ext| ext.eq_ignore_ascii_case(".md")).map_or(name, |_| &name[..name.len() - 3]);
    let spaced: String = base.chars().map(|c| if matches!(c, '/' | '\\' | ':') || c < ' ' { ' ' } else { c }).collect();
    let collapsed = spaced.split_whitespace().collect::<Vec<_>>().join(" ");
    let base: String = collapsed.trim_start_matches('.').chars().take(80).collect();
    let base = base.trim();
    (!base.is_empty()).then(|| format!("{base}.md"))
}

/// Re-signing an edited note replaces its old signature rather than
/// stacking a second one (`signNote`).
fn sign_note(text: &str, from: Option<&str>) -> String {
    let (body, _) = notes::split_signature(text.trim_end());
    match from {
        Some(from) => format!("{}\n\n— from {from}\n", body.trim_end()),
        None => format!("{}\n", body.trim_end()),
    }
}

fn trim_slash(s: &str) -> &str {
    let t = s.trim().trim_end_matches('/');
    if t.is_empty() && s.trim().starts_with('/') { "/" } else { t }
}

/// The terminal the caller runs in (`callerTerminal`): its agent session id
/// when it sent one, else the one agent terminal at or above its cwd, the
/// streaming one when several are.
fn caller_terminal(shell: Shell, args: &Value) -> Option<String> {
    let terms = shell.terms.get_untracked();
    let claimed = args.get("sessionId").and_then(Value::as_str).map(str::trim).unwrap_or("");
    if !claimed.is_empty() {
        if let Some(t) = terms.iter().find(|t| engine::terminal_status(t.id).agent_session_id.as_deref() == Some(claimed)) {
            return Some(t.session_id.clone());
        }
    }
    let cwd = trim_slash(args.get("cwd").and_then(Value::as_str).unwrap_or(""));
    let agents: Vec<(&crate::state::Term, engine::TermStatus)> = terms
        .iter()
        .map(|t| (t, engine::terminal_status(t.id)))
        .filter(|(_, s)| s.agent_pid.is_some())
        .collect();
    let here: Vec<&(&crate::state::Term, engine::TermStatus)> = agents
        .iter()
        .filter(|(t, _)| {
            let tcwd = trim_slash(&t.cwd);
            !cwd.is_empty() && !tcwd.is_empty() && (cwd == tcwd || cwd.starts_with(&format!("{tcwd}/")))
        })
        .collect();
    let streaming = |v: &[&(&crate::state::Term, engine::TermStatus)]| -> Vec<String> {
        v.iter().filter(|(_, s)| s.status == "agent").map(|(t, _)| t.session_id.clone()).collect()
    };
    if here.len() == 1 {
        return Some(here[0].0.session_id.clone());
    }
    let s = streaming(&here);
    if s.len() == 1 {
        return s.into_iter().next();
    }
    if here.is_empty() {
        let all: Vec<&(&crate::state::Term, engine::TermStatus)> = agents.iter().collect();
        let s = streaming(&all);
        if s.len() == 1 {
            return s.into_iter().next();
        }
    }
    None
}

/// `@handle · repo (branch)`, whatever of it can be found (`noteSignature`).
async fn note_signature(shell: Shell, term: Option<&str>, cwd: Option<String>) -> Option<String> {
    let handle = term.and_then(|id| {
        hub::targets(shell)
            .into_iter()
            .find(|t| t.kind == TargetKind::Terminal && t.members[0].session_id.as_deref() == Some(id))
            .map(|t| t.handle)
    });
    let (repo, branch) = match cwd {
        Some(cwd) => git::off_thread(move || (git::repo_name(&cwd), git::current_branch(&cwd).unwrap_or_default())).await,
        None => (None, String::new()),
    };
    let place = repo.map(|r| if branch.is_empty() { r } else { format!("{r} ({branch})") });
    let parts: Vec<String> = [handle.map(|h| format!("@{h}")), place].into_iter().flatten().collect();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// The folder a note goes in, and its group when that is a group's:
/// `scope` "group" is the caller's group, "global" the shared folder. Left
/// out, a note the caller's group already has by that name is edited in
/// place; anything else is global.
fn note_folder(scope: Option<&str>, group: Option<(String, PathBuf)>, global: PathBuf, file_name: &str) -> Result<(PathBuf, Option<String>), String> {
    match (scope.map(str::trim).filter(|s| !s.is_empty()), group) {
        (Some("group"), Some((g, dir))) => Ok((dir, Some(g))),
        (Some("group"), None) => Err("scope \"group\" needs your terminal to be in a tab of a Rivet group, and it is in none. Save it without scope.".into()),
        (Some("global"), _) => Ok((global, None)),
        (Some(other), _) => Err(format!("scope is \"global\" or \"group\", not \"{other}\".")),
        (None, Some((g, dir))) if dir.join(file_name).is_file() => Ok((dir, Some(g))),
        (None, _) => Ok((global, None)),
    }
}

pub async fn call(shell: Shell, name: &str, args: &Value) -> Option<ToolResult> {
    if name != "saveNote" {
        return None;
    }
    Some(save_note(shell, args).await)
}

async fn save_note(shell: Shell, args: &Value) -> ToolResult {
    let Some(file_name) = args.get("name").and_then(Value::as_str).and_then(note_file_name) else {
        return Err("name is required.".into());
    };
    let text = match args.get("text").and_then(Value::as_str) {
        Some(t) if !t.trim().is_empty() => t.to_owned(),
        _ => return Err("text is required.".into()),
    };
    let term = caller_terminal(shell, args);
    let given = trim_slash(args.get("cwd").and_then(Value::as_str).unwrap_or("")).to_owned();
    let term_cwd = term.as_ref().and_then(|id| shell.terms.get_untracked().into_iter().find(|t| &t.session_id == id).map(|t| t.cwd));
    // The strip picks a terminal's group from the terminal's cwd, so that one decides here too.
    let group = shell.settings.with_untracked(|s| {
        let g = notes::group_at(s, term_cwd.as_deref().unwrap_or(&given))?;
        let dir = notes::group_notes_dir(s, &g)?;
        Some((g, dir))
    });
    let group_name = group.as_ref().map(|(g, _)| g.clone());
    let (dir, saved_in) = note_folder(args.get("scope").and_then(Value::as_str), group, notes::notes_dir(), &file_name)?;
    let from = note_signature(shell, term.as_deref(), if given.is_empty() { term_cwd } else { Some(given) }).await;
    let signed = sign_note(&text, from.as_deref());
    let path = dir.join(&file_name);
    let written = {
        let path = path.clone();
        git::off_thread(move || {
            std::fs::create_dir_all(path.parent().expect("a note has a folder")).map_err(|e| e.to_string())?;
            std::fs::write(&path, signed).map_err(|e| e.to_string())
        })
        .await
    };
    if let Err(e) = written {
        return Err(format!("Could not save {file_name}: {e}"));
    }
    notes::refresh();
    // Mentions resolve when a strip renders, so one that matches nothing open today is kept, just reported.
    let live: Vec<String> = hub::targets(shell).into_iter().filter(|t| t.kind != TargetKind::Group).map(|t| t.handle).collect();
    let mentions = notes::mentions(notes::split_signature(&text).0);
    let known: Vec<String> = mentions.iter().filter(|m| live.contains(m)).map(|m| format!("@{m}")).collect();
    let unknown: Vec<String> = mentions.iter().filter(|m| !live.contains(m)).map(|m| format!("@{m}")).collect();
    let scope = match (&saved_in, &group_name) {
        (Some(g), _) => format!("for the terminals of group {g}"),
        (None, Some(g)) => format!("for every terminal (scope \"group\" keeps it to group {g})"),
        (None, None) => "for every terminal".to_owned(),
    };
    let mut out = format!(
        "Saved rivet note {file_name} {scope} ({}), {}.",
        path.display(),
        match &from {
            Some(f) => format!("signed \"— from {f}\""),
            None => "unsigned: pass your cwd so Rivet can tell where it came from".into(),
        }
    );
    if !known.is_empty() {
        out.push_str(&format!(" Highlighted for {}.", known.join(", ")));
    }
    if !unknown.is_empty() {
        out.push_str(&format!(" No open terminal or workspace answers to {}; getWorkspaceTabs lists the handles.", unknown.join(", ")));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_note_is_global_unless_asked_for_the_group_or_already_there() {
        let dir = std::env::temp_dir().join(format!("rivet-note-folder-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Kept.md"), "in the group").unwrap();
        let group = || Some(("web".to_owned(), dir.clone()));
        let global = PathBuf::from("/global");
        let at = |scope, group, name| note_folder(scope, group, global.clone(), name);

        assert_eq!(at(None, group(), "New.md").unwrap(), (global.clone(), None));
        assert_eq!(at(None, group(), "Kept.md").unwrap(), (dir.clone(), Some("web".to_owned())));
        assert_eq!(at(Some("group"), group(), "New.md").unwrap(), (dir.clone(), Some("web".to_owned())));
        assert_eq!(at(Some("global"), group(), "Kept.md").unwrap(), (global.clone(), None));
        assert!(at(Some("group"), None, "New.md").is_err());
        assert!(at(Some("team"), group(), "New.md").is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
