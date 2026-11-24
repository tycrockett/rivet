//! The workspace tools of Bolt's MCP server, ported from
//! `src/hooks/useMcpBridge.ts` (`handleGetWorkspaceTabs`, `handleRunCommand`,
//! `handleCreateWorktrees`, `handleSetTabGroup`, `handleCloseTabs`) and
//! `mcpServer.js` (`getWorkspaceFolders`, which Bolt answers in main).

use serde_json::{Value, json};

use super::ToolResult;
use crate::{
    commands, git,
    hub::engine::{script::{add_workspace_tabs, cut_branches}, send::status_by_session},
    settings::Tab,
    state::Shell,
    workspace::{group_key, regroup},
};

/// `MCP_COMMANDS`: the verbs `runCommand` may run — the rest of the registry
/// deletes, commits or merges, and wants a human at the keyboard.
const MCP_COMMANDS: &[&str] = &["new", "go", "tab", "edit", "delete", "browser"];

pub async fn call(shell: Shell, name: &str, args: &Value) -> Option<ToolResult> {
    Some(match name {
        "getWorkspaceFolders" => Ok(workspace_folders(shell)),
        "getWorkspaceTabs" => Ok(workspace_tabs(shell).await),
        "runCommand" => run_command(shell, args).await,
        "createWorktrees" => create_worktrees(shell, args).await,
        "setTabGroup" => set_tab_group(shell, args),
        "closeTabs" => close_tabs(shell, args),
        _ => return None,
    })
}

/// `trimSlash`: a path as the tools compare it.
fn trim_slash(v: Option<&Value>) -> String {
    let s = v.and_then(Value::as_str).unwrap_or("").trim();
    if s.len() > 1 { s.trim_end_matches('/').to_owned() } else { s.to_owned() }
}

fn str_list(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect())
        .unwrap_or_default()
}

/// `pathsOf`: the `paths` argument, trimmed and de-duplicated.
fn paths_of(args: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for p in str_list(args.get("paths")) {
        let p = trim_slash(Some(&Value::String(p)));
        if !p.is_empty() && !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

fn quoted(cmds: &[String]) -> String {
    cmds.iter().map(|c| format!("'{c}'")).collect::<Vec<_>>().join(", ")
}

/// Bolt's workspace folders are `[settings.directory]`.
fn workspace_folders(shell: Shell) -> String {
    let dir = shell.directory();
    let name = dir.rsplit('/').find(|s| !s.is_empty()).unwrap_or("").to_owned();
    json!([{ "uri": format!("file://{dir}"), "name": name }]).to_string()
}

/// `getBranchStatus` plus the worktree facts of `getWorktreeContext` and
/// `getRepoName`, read fresh from one `git status --porcelain=v2 --branch`.
fn tab_git_state(path: &str) -> Value {
    let mut v = json!({
        "exists": true, "inGit": false, "branch": null, "detached": false, "hasCommits": false,
        "dirty": false, "staged": false, "untracked": false, "conflicted": false,
        "hasRemote": false, "ahead": 0, "behind": 0,
    });
    let (repo, wt) = (git::repo_name(path), git::worktree_context(path).ok());
    if !std::path::Path::new(path).is_dir() {
        v["exists"] = json!(false);
    } else if let Ok(out) = git::git(&["status", "--porcelain=v2", "--branch"], path) {
        v["inGit"] = json!(true);
        v["hasCommits"] = json!(true);
        for line in out.lines() {
            if let Some(oid) = line.strip_prefix("# branch.oid ") {
                if oid.trim() == "(initial)" {
                    v["hasCommits"] = json!(false);
                }
            } else if let Some(head) = line.strip_prefix("# branch.head ") {
                if head.trim() == "(detached)" {
                    v["detached"] = json!(true);
                } else {
                    v["branch"] = json!(head.trim());
                }
            } else if line.starts_with("# branch.upstream ") {
                v["hasRemote"] = json!(true);
            } else if let Some(ab) = line.strip_prefix("# branch.ab ") {
                let mut n = ab.split_whitespace().map(|x| x.trim_start_matches(['+', '-']).parse::<i64>().unwrap_or(0));
                v["ahead"] = json!(n.next().unwrap_or(0));
                v["behind"] = json!(n.next().unwrap_or(0));
            } else if line.starts_with("1 ") || line.starts_with("2 ") {
                v["dirty"] = json!(true);
                let xy = line.split(' ').nth(1).unwrap_or("..");
                if xy.chars().next().is_some_and(|c| c != '.') {
                    v["staged"] = json!(true);
                }
            } else if line.starts_with("u ") {
                v["dirty"] = json!(true);
                v["conflicted"] = json!(true);
            } else if line.starts_with("? ") {
                v["dirty"] = json!(true);
                v["untracked"] = json!(true);
            }
        }
    }
    v["repoName"] = json!(repo);
    v["isWorktree"] = json!(wt.as_ref().is_some_and(|w| w.is_worktree));
    v["mainRepoPath"] = json!(wt.map(|w| w.main_repo_path));
    v
}

/// `handleGetWorkspaceTabs`.
async fn workspace_tabs(shell: Shell) -> String {
    let s = shell.settings.get_untracked();
    let rows: Vec<(Tab, bool)> = s.pinned.iter().map(|t| (t.clone(), true)).chain(s.tabs.iter().map(|t| (t.clone(), false))).collect();
    let paths: Vec<String> = rows.iter().map(|(t, _)| t.path.clone()).collect();
    let states = git::off_thread(move || paths.iter().map(|p| tab_git_state(p)).collect::<Vec<_>>()).await;
    let targets = crate::hub::targets(shell);
    let terms = shell.terms.get_untracked();

    let tabs: Vec<Value> = rows
        .iter()
        .zip(states)
        .map(|((tab, pinned), state)| {
            let handle_of = |pred: &dyn Fn(&crate::hub::HubTarget) -> bool| targets.iter().find(|t| pred(t)).map(|t| t.handle.clone());
            let terminals: Vec<Value> = terms
                .iter()
                .filter(|t| t.cwd == tab.path)
                .map(|t| {
                    let info = status_by_session(shell, &t.session_id).unwrap_or_default();
                    json!({
                        "sessionId": t.session_id,
                        "handle": handle_of(&|h| h.members.first().is_some_and(|m| m.session_id.as_deref() == Some(t.session_id.as_str())) && h.kind == crate::hub::TargetKind::Terminal),
                        "status": if info.status.is_empty() { "idle".to_owned() } else { info.status },
                        "label": info.label,
                        "needsInput": info.needs_input,
                    })
                })
                .collect();
            let group = group_key(tab, &s.groups);
            let mut row = json!({
                "name": tab.name,
                "handle": handle_of(&|h| h.kind == crate::hub::TargetKind::Workspace && h.members.first().is_some_and(|m| m.cwd == tab.path)),
                "path": tab.path,
                "group": group,
                "groupHome": tab.home && group.is_some(),
                "pinned": pinned,
                "hidden": tab.hidden,
                "active": tab.path == s.directory,
                "terminals": terminals,
            });
            if let (Some(row), Some(state)) = (row.as_object_mut(), state.as_object()) {
                row.extend(state.clone());
            }
            row
        })
        .collect();

    json!({ "directory": s.directory, "groups": s.groups, "tabs": tabs }).to_string()
}

/// `handleRunCommand`: the batch runs in order in the open workspace and
/// stops at the first failure.
async fn run_command(shell: Shell, args: &Value) -> ToolResult {
    let directory = trim_slash(args.get("cwd"));
    if directory.is_empty() {
        return Err("cwd is required: it is the workspace the command runs in.".into());
    }
    let open = shell.directory();
    if directory != trim_slash(Some(&Value::String(open.clone()))) {
        let open = if open.is_empty() { "no repo" } else { open.as_str() };
        return Err(format!("Rivet has {open} open, not {directory}. A command runs in the open workspace — switch Rivet there first."));
    }

    let cmds: Vec<String> = match args.get("command") {
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).map(|c| c.trim().to_owned()).collect(),
        Some(Value::String(c)) => vec![c.trim().to_owned()],
        _ => Vec::new(),
    }
    .into_iter()
    .filter(|c| !c.is_empty())
    .collect();
    if cmds.is_empty() {
        return Err("command is required, e.g. [\"new my-branch -bw\"].".into());
    }

    for cmd in &cmds {
        let verb = commands::parse(cmd).command;
        if !MCP_COMMANDS.contains(&verb.as_str()) {
            return Err(format!("'{verb}' is not one an agent may run, so none of the batch ran. Allowed: {}.", MCP_COMMANDS.join(", ")));
        }
    }

    let mut ran: Vec<String> = Vec::new();
    let mut printed: Vec<String> = Vec::new();
    for (i, cmd) in cmds.iter().enumerate() {
        let out = commands::run(shell, cmd.clone(), None).await;
        let errors: Vec<&str> = out.iter().filter(|o| o.error).map(|o| o.text.trim()).collect();
        if !errors.is_empty() {
            let skipped = &cmds[i + 1..];
            let mut msg = format!("'{cmd}' failed: {}.", errors.join("; ").trim_end_matches('.'));
            if !ran.is_empty() {
                msg += &format!(" Already ran: {}.", quoted(&ran));
            }
            if !skipped.is_empty() {
                msg += &format!(" Not run: {}.", quoted(skipped));
            }
            return Err(msg);
        }
        printed.extend(out.iter().map(|o| o.text.trim().to_owned()).filter(|t| !t.is_empty()).map(|t| format!("$ {cmd}\n{t}")));
        ran.push(cmd.clone());
    }

    let mut text = format!("Ran {} in {directory}.", quoted(&ran));
    if !printed.is_empty() {
        text += &format!("\n\n{}", printed.join("\n\n"));
    }
    Ok(text)
}

/// `handleCreateWorktrees`: the headless sibling of `new <name> -bw` — the
/// user stays where they are.
async fn create_worktrees(shell: Shell, args: &Value) -> ToolResult {
    let directory = trim_slash(args.get("cwd"));
    if directory.is_empty() {
        return Err("cwd is required: it is the repo the branches are cut from.".into());
    }
    let branches: Vec<String> = str_list(args.get("branches")).into_iter().map(|b| b.trim().to_owned()).filter(|b| !b.is_empty()).collect();
    if branches.is_empty() {
        return Err("branches is required, e.g. [\"auth-intake\", \"auth-callback\"].".into());
    }
    let text = |k: &str| args.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned);
    let (parent, group) = (text("parent"), text("group"));
    let home = shell.settings.get_untracked().home;

    let (dir, b, p) = (directory.clone(), branches, parent);
    let (cuts, failure) = git::off_thread(move || cut_branches(&dir, &b, p.as_deref(), &home)).await;
    // One settings write for everything that landed, even when a later branch failed.
    if !cuts.is_empty() {
        add_workspace_tabs(shell, &cuts.iter().map(|c| c.path.clone()).collect::<Vec<_>>(), group.as_deref());
    }
    if let Some(failure) = failure {
        let made = if cuts.is_empty() { String::new() } else { format!(" Made: {}.", cuts.iter().map(|c| c.branch.as_str()).collect::<Vec<_>>().join(", ")) };
        return Err(format!("{failure}{made}"));
    }
    let created: Vec<Value> = cuts
        .iter()
        .map(|c| {
            let mut v = json!({ "branch": c.branch, "path": c.path, "existed": c.existed });
            if !c.setup_failed.is_empty() {
                v["setupFailed"] = json!(c.setup_failed);
            }
            v
        })
        .collect();
    Ok(json!({ "directory": directory, "group": group, "created": created }).to_string())
}

/// `handleSetTabGroup`: all-or-nothing; a path that is not a regular tab
/// refuses the call.
fn set_tab_group(shell: Shell, args: &Value) -> ToolResult {
    let paths = paths_of(args);
    if paths.is_empty() {
        return Err("paths is required: the workspace tab paths to move.".into());
    }
    let group = match args.get("group") {
        Some(Value::Null) => None,
        Some(Value::String(g)) => Some(g.trim().to_owned()).filter(|g| !g.is_empty()),
        _ => return Err("group is required: a group name, or null to ungroup.".into()),
    };

    let s = shell.settings.get_untracked();
    let unknown: Vec<&str> = paths.iter().filter(|p| !s.tabs.iter().any(|t| &t.path == *p)).map(String::as_str).collect();
    if !unknown.is_empty() {
        return Err(format!("Not regular workspace tabs: {}. Nothing moved.", unknown.join(", ")));
    }
    let homes: Vec<&str> = paths.iter().filter(|p| s.tabs.iter().any(|t| &t.path == *p && t.home)).map(String::as_str).collect();
    if !homes.is_empty() {
        return Err(format!("Group homes cannot be moved: {}. Nothing moved.", homes.join(", ")));
    }

    shell.update_settings(|s| {
        if let Some(g) = &group {
            if !s.groups.contains(g) {
                s.groups.push(g.clone());
            }
        }
        let mut tabs = std::mem::take(&mut s.tabs);
        for t in tabs.iter_mut().filter(|t| paths.contains(&t.path)) {
            t.group = group.clone();
        }
        s.tabs = regroup(tabs, &s.groups);
    });
    Ok(match group {
        Some(g) => format!("Moved {} tab(s) into {g}.", paths.len()),
        None => format!("Ungrouped {} tab(s).", paths.len()),
    })
}

/// `handleCloseTabs`: the tabs leave the strip; their worktrees, branches
/// and terminals stay as they are.
fn close_tabs(shell: Shell, args: &Value) -> ToolResult {
    let paths = paths_of(args);
    if paths.is_empty() {
        return Err("paths is required: the workspace tab paths to close.".into());
    }
    let s = shell.settings.get_untracked();
    let unknown: Vec<&str> = paths.iter().filter(|p| !s.tabs.iter().chain(&s.pinned).any(|t| &t.path == *p)).map(String::as_str).collect();
    if !unknown.is_empty() {
        return Err(format!("Not workspace tabs: {}. Nothing closed.", unknown.join(", ")));
    }
    shell.update_settings(|s| {
        s.tabs.retain(|t| !paths.contains(&t.path));
        s.pinned.retain(|t| !paths.contains(&t.path));
    });
    Ok(format!("Closed {}. Their worktrees and terminals are untouched.", paths.join(", ")))
}
