//! Built-in and terminal-kind handlers, ported from Bolt's
//! `commandHandlers/{tabCommand,bookmarkCommand,directoryCommand,terminalCommand}.ts`
//! and `useCommandExecution`'s `directCommand`.

use std::{path::Path, time::Duration};

use nsh_core::prelude::*;

use super::{Ctx, Kind, REGISTRY};
use crate::{
    git,
    settings::{Tab, tab_name_for},
    state,
    workspace::{group_key, regroup},
};

type Result = std::result::Result<(), String>;

/// `tab`, `tab --name <n> [--path <p>]`, `tab rm`, `tab group <name>`,
/// `tab group --name <new>`, `tab add <group>`, `tab ungroup`.
pub fn tab(ctx: &Ctx) -> Result {
    let shell = ctx.shell;
    let s = &ctx.settings;
    let dir = &ctx.directory;
    let rest = |from: usize| ctx.args.get(from..).unwrap_or_default().join(" ").trim().to_owned();
    match ctx.args.first().map(String::as_str) {
        Some("group") => {
            if let Some(at) = ctx.args.iter().position(|a| a == "--name") {
                let new_name = rest(at + 1);
                let old = s.tabs.iter().find(|t| &t.path == dir).and_then(|t| t.group.clone());
                if new_name.is_empty() {
                    ctx.add_error("tabGroup", "Usage: tab group --name <new-name>");
                } else if let Some(old) = old {
                    if s.groups.contains(&new_name) {
                        ctx.add_error("tabGroup", format!("Group {new_name} already exists"));
                        return Ok(());
                    }
                    shell.update_settings(|s| s.rename_group(&old, &new_name));
                    ctx.add("tabGroup", format!("Renamed group {old} to {new_name}"));
                } else {
                    ctx.add_error("tabGroup", "Current tab has no group");
                }
                return Ok(());
            }
            let name = rest(1);
            if name.is_empty() {
                ctx.add_error("tabGroup", "Usage: tab group <name>");
            } else if s.groups.contains(&name) {
                ctx.add("tabGroup", format!("Group {name} already exists"));
            } else {
                shell.update_settings(|s| s.groups.push(name.clone()));
                ctx.add("tabGroup", format!("Created group {name}"));
            }
        }
        Some(sub @ ("add" | "ungroup")) => {
            let name = (sub == "add").then(|| rest(1));
            if name.as_ref().is_some_and(String::is_empty) {
                ctx.add_error("tabGroup", "Usage: tab add <group-name> | tab ungroup");
            } else if let Some(n) = name.as_ref().filter(|n| !s.groups.contains(n)) {
                ctx.add_error("tabGroup", format!("No group named {n} — tab group {n} creates it"));
            } else if !s.tabs.iter().any(|t| &t.path == dir) {
                ctx.add_error("tabGroup", format!("{dir} is not a regular tab"));
            } else {
                let moved = s
                    .tabs
                    .iter()
                    .cloned()
                    .map(|mut t| {
                        if &t.path == dir {
                            t.group = name.clone();
                        }
                        t
                    })
                    .collect();
                let tabs = regroup(moved, &s.groups);
                shell.update_settings(|s| s.tabs = tabs);
                ctx.add("tabGroup", match &name {
                    Some(n) => format!("Grouped under {n}"),
                    None => "Removed from its group".to_owned(),
                });
            }
        }
        Some("session") => {
            let here = s.tabs.iter().find(|t| &t.path == dir);
            let name = Some(rest(1)).filter(|n| !n.is_empty()).or_else(|| here.and_then(|t| group_key(t, &s.groups).map(str::to_owned)));
            match name {
                None => ctx.add_error("tabSession", "Usage: tab session <group-name>"),
                Some(n) => ctx.add_error("tabSession", format!("{n}'s group session runs only in the Electron Bolt for now")),
            }
        }
        Some("rm") => {
            shell.update_settings(|s| s.tabs.retain(|t| &t.path != dir));
            ctx.add("tabRm", format!("Removed {dir} from tabs"));
        }
        _ => {
            let new_name = ctx.flag_value("--name").map(str::to_owned);
            let path_flag = ctx.flag_value("--path").map(str::to_owned);
            let target = path_flag.clone().unwrap_or_else(|| dir.clone());
            let known = s.tabs.iter().chain(&s.pinned).any(|t| t.path == target);
            if path_flag.is_some() && (!known || new_name.is_none()) {
                let message = if known {
                    "Usage: tab --name <name> --path <tab path>".to_owned()
                } else {
                    format!("No tab at {target}")
                };
                ctx.add_error("tabRename", message);
            } else if let (Some(name), true) = (&new_name, known) {
                shell.update_settings(|s| {
                    for t in s.tabs.iter_mut().chain(s.pinned.iter_mut()) {
                        if t.path == target {
                            t.name.clone_from(name);
                        }
                    }
                });
                ctx.add("tabRename", format!("Renamed tab to {name}"));
            } else if s.tabs.iter().any(|t| &t.path == dir) {
                ctx.add("tabExists", format!("{dir} is already in tabs"));
            } else {
                let name = new_name.unwrap_or_else(|| tab_name_for(dir));
                shell.update_settings(|s| s.tabs.push(Tab::new(name.clone(), dir.clone())));
                ctx.add("tabAdd", format!("Added {name} to tabs"));
            }
        }
    }
    Ok(())
}

/// `bm add <name>`, `bm list`, `bm rm <name>`. `bm` and `bm go` open Bolt's
/// bookmark view, which this shell does not have.
pub fn bookmark(ctx: &Ctx) -> Result {
    let shell = ctx.shell;
    let name = ctx.args.get(1).cloned();
    match ctx.args.first().map(String::as_str) {
        None | Some("go") => Err("The bookmark view runs only in the Electron Bolt for now".into()),
        Some("add") => {
            let name = name.ok_or("No bookmark name provided")?;
            let dir = ctx.directory.clone();
            shell.update_settings(|s| {
                s.bookmarks.insert(name.clone(), dir.clone());
            });
            ctx.add("bookmarkAdd", format!("Bookmark {name} set to {dir}"));
            Ok(())
        }
        Some("list") => {
            let items: Vec<(String, String)> = ctx.settings.bookmarks.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            let list = items.iter().map(|(k, v)| format!("{k}  {v}")).collect::<Vec<_>>();
            ctx.add_rich("bookmarkList", list.join("\n"), super::rich::Rich::Bookmarks { items });
            Ok(())
        }
        Some("rm") => {
            let name = name.ok_or("No bookmark name provided")?;
            shell.update_settings(|s| {
                s.bookmarks.remove(&name);
            });
            ctx.add("bookmarkRemove", format!("Removed bookmark {name}"));
            Ok(())
        }
        Some(_) => Err("Invalid command".into()),
    }
}

pub fn help(ctx: &Ctx) -> Result {
    let section = |kind: Kind, title: &str| {
        let names: Vec<String> = REGISTRY
            .iter()
            .filter(|c| c.kind == kind && !super::is_nav(c.name))
            .map(|c| if c.ported { c.name.to_owned() } else { format!("({})", c.name) })
            .collect();
        format!("{title}: {}", names.join(" "))
    };
    let custom: Vec<&str> = ctx.settings.commands.keys().map(String::as_str).collect();
    ctx.add("help",
    [
        section(Kind::Git, "git"),
        section(Kind::BuiltIn, "built-in"),
        section(Kind::Terminal, "terminal"),
        format!("custom: {}", custom.join(" ")),
        "git <args> passes through; :cmd [*N] runs cmd in N new terminals; (name) runs only in the Electron Bolt".into(),
    ]
    .join("\n"),);
    Ok(())
}

/// `diff` (`changesCommand`): the Changes tab on the working tree, or the
/// branch when it is clean; `--since <hash>` (`-s`) pins `<hash>..HEAD`,
/// `--commit <hash>` (`-c`) one commit.
pub fn diff(ctx: &Ctx) -> Result {
    let value = |names: [&str; 2]| names.iter().find_map(|n| ctx.flag_value(n)).filter(|v| !v.starts_with('-'));
    let spec = if let Some(since) = value(["--since", "-s"]) {
        Some(format!("{since}..HEAD"))
    } else if ctx.flag(&["--commit", "-c"]) {
        match value(["--commit", "-c"]) {
            Some(hash) => Some(hash.to_owned()),
            None => {
                ctx.add_error("changesError", "Usage: diff --commit <hash>");
                return Ok(());
            }
        }
    } else {
        None
    };
    ctx.shell.open_changes(ctx.directory.clone(), spec);
    Ok(())
}

/// `browser [url]` (`webCommand`): a web tab here, on `url` or blank;
/// `browser <url> -o` hands it to the system browser instead (Bolt asks
/// which browser window).
pub fn browser(ctx: &Ctx) -> Result {
    let url = ctx.positional().map(crate::web::to_url);
    if ctx.flag(&["--open", "-o"]) {
        if let Some(url) = url {
            let _ = open_url(url);
        }
        return Ok(());
    }
    ctx.shell.open_web(ctx.directory.clone(), url.unwrap_or_default());
    Ok(())
}

/// `terminal` opens one in the current directory; `terminal -o` lists the
/// shells whose directory is gone or has no tab.
pub fn terminal(ctx: &Ctx) -> Result {
    if ctx.flag(&["--orphan", "-o"]) {
        let s = &ctx.settings;
        let lines: Vec<String> = ctx
            .shell
            .terms
            .get_untracked()
            .into_iter()
            .filter_map(|t| {
                let mut reasons = Vec::new();
                if !Path::new(&t.cwd).is_dir() {
                    reasons.push("cwd-missing");
                }
                if !s.tabs.iter().chain(&s.pinned).any(|tab| tab.path == t.cwd) {
                    reasons.push("no-tab");
                }
                (!reasons.is_empty()).then(|| {
                    let pid = state::pty_pid(t.id).map_or("?".into(), |p| p.to_string());
                    format!("pid {pid}  {}  ({})", t.cwd, reasons.join(", "))
                })
            })
            .collect();
        ctx.add("orphanTerminals", if lines.is_empty() { "No orphaned terminals".into() } else { lines.join("\n") });
        return Ok(());
    }
    ctx.shell.open_terminal(ctx.directory.clone()).map(|_| ())
}

/// `:cmd args [*N]`: N new terminals here, each running `cmd args`, 400 ms
/// apart so they don't race for ports.
pub async fn direct(ctx: &Ctx, command: &str) -> Result {
    let mut args = ctx.args.clone();
    let count = args
        .last()
        .and_then(|a| a.strip_prefix('*'))
        .and_then(|n| n.parse::<usize>().ok())
        .map(|n| n.max(1));
    if count.is_some() {
        args.pop();
    }
    let line = std::iter::once(command.to_owned()).chain(args).collect::<Vec<_>>().join(" ");
    for i in 0..count.unwrap_or(1) {
        if i > 0 {
            timer(Duration::from_millis(400)).await;
        }
        ctx.shell.run_in_terminal(ctx.directory.clone(), &line)?;
    }
    Ok(())
}

async fn listing(ctx: &Ctx, dir: String) -> Result {
    let out = git::off_thread(move || git::shell("ls", &[], &dir)).await?;
    ctx.add("directoryList", out.trim_end());
    Ok(())
}

pub async fn ls(ctx: &Ctx) -> Result {
    listing(ctx, ctx.directory.clone()).await
}

pub async fn mkdir(ctx: &Ctx) -> Result {
    let (args, dir) = (ctx.args.clone(), ctx.directory.clone());
    git::off_thread(move || git::shell("mkdir", &args, &dir)).await.map(|_| ())
}

/// `cd` alone goes home; `cd <dir>` is relative unless `--absolute`.
pub async fn cd(ctx: &Ctx) -> Result {
    let Some(arg) = ctx.args.first().cloned() else {
        let home = ctx.settings.home.clone();
        ctx.set_directory(home.clone());
        return listing(ctx, home).await;
    };
    change_directory(ctx, &arg).await
}

pub async fn up(ctx: &Ctx) -> Result {
    change_directory(ctx, "..").await
}

/// `changeDirectory`: `..` drops the last segment; anything else must be
/// an existing directory.
async fn change_directory(ctx: &Ctx, arg: &str) -> Result {
    let path = if arg == ".." {
        ctx.directory.rsplit_once('/').map_or_else(String::new, |(p, _)| p.to_owned())
    } else if ctx.flag(&["--absolute"]) {
        arg.to_owned()
    } else {
        format!("{}/{arg}", ctx.directory)
    };
    if arg != ".." && !Path::new(&path).is_dir() {
        return Err("Directory does not exist".into());
    }
    ctx.set_directory(path.clone());
    listing(ctx, path).await
}

/// Where `notes` keeps its files: the Electron Bolt's `notes` folder, so
/// both apps show the same notes.
fn notes_dir() -> std::path::PathBuf {
    crate::settings::bolt_data_dir().join("notes")
}

/// `notesCommand`: `notes` opens this directory's notes (made with its
/// name as a heading the first time) for editing; `notes --list` lists
/// every note; `notes clear` deletes them all.
pub async fn notes(ctx: &Ctx) -> Result {
    let dir = notes_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let files = || -> Vec<std::path::PathBuf> {
        let Ok(entries) = std::fs::read_dir(&dir) else { return Vec::new() };
        let mut out: Vec<_> = entries.flatten().map(|e| e.path()).filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "md")).collect();
        out.sort();
        out
    };
    if ctx.args.first().map(String::as_str) == Some("clear") {
        let notes = files();
        for p in &notes {
            let _ = std::fs::remove_file(p);
        }
        ctx.add_git("notesClear", "terminal", format!("Cleared {} note{}", notes.len(), if notes.len() == 1 { "" } else { "s" }));
        return Ok(());
    }
    if ctx.flag(&["--list"]) {
        // A note's name is the directory it belongs to, `_` back to `/`.
        let items: Vec<(String, String)> = files()
            .into_iter()
            .map(|p| (p.file_stem().map(|s| s.to_string_lossy().replace('_', "/")).unwrap_or_default(), p.to_string_lossy().into_owned()))
            .collect();
        let text = if items.is_empty() { "No notes found".to_owned() } else { items.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join("\n") };
        ctx.add_rich("notesList", text, super::rich::Rich::Notes { items });
        return Ok(());
    }
    let file = dir.join(format!("{}.md", ctx.directory.replace(['/', '\\', ':'], "_")));
    if !file.exists() {
        let name = Path::new(&ctx.directory).file_name().map_or_else(|| ctx.directory.clone(), |n| n.to_string_lossy().into_owned());
        std::fs::write(&file, format!("# {name}\n\n")).map_err(|e| e.to_string())?;
    }
    ctx.shell.open_editable(ctx.directory.clone(), file.to_string_lossy().into_owned());
    ctx.add_git("notesOpen", "terminal", "Opened notes");
    Ok(())
}

/// `settings` opens the Settings tab (Bolt's footer gear); `settings set`
/// and `settings list` are not here.
pub fn settings(ctx: &Ctx) -> Result {
    if !ctx.args.is_empty() {
        return Err("'settings set' and 'settings list' run only in the Electron Bolt; 'settings' opens the Settings tab".into());
    }
    ctx.shell.open_settings();
    Ok(())
}
