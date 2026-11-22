//! The remaining git verbs, ported from Bolt's `commandHandlers/{commit,
//! branch,del,update,stash,status}Command.ts` and the `src/services/git/`
//! functions behind them. Each runs the same git invocations in the same
//! order and reports the same text; a result Bolt draws as a card (a git
//! message, a conflict list, the status lists) is plain text here.
//!
//! Where Bolt opens a view this app has no counterpart of (a diff tab, the
//! in-app browser, the close-tab modal, the AI client) the nearest thing is
//! done instead and the file says so at that spot.

use std::{fmt::Display, path::Path};

use super::{Ctx, rich};
use crate::{
    git,
    settings::{Tab, tab_name_for},
};

type Result = std::result::Result<(), String>;

// ───────────────────────── helpers ─────────────────────────

/// `git args…` in `dir`, off the engine thread.
async fn run(dir: &str, args: &[&str]) -> git::Result<String> {
    let dir = dir.to_owned();
    let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
    git::off_thread(move || git::git(&args, &dir)).await
}

async fn current_branch(dir: &str) -> git::Result<String> {
    let dir = dir.to_owned();
    git::off_thread(move || git::show_current_branch(&dir)).await
}

async fn has_remote(dir: &str, branch: &str) -> bool {
    let (dir, branch) = (dir.to_owned(), branch.to_owned());
    git::off_thread(move || git::has_remote_branch(&branch, &dir)).await
}

async fn default_branch(dir: &str) -> String {
    let dir = dir.to_owned();
    git::off_thread(move || git::default_branch(&dir)).await
}

async fn worktree_context(dir: &str) -> git::Result<git::WorktreeContext> {
    let dir = dir.to_owned();
    git::off_thread(move || git::worktree_context(&dir)).await
}

async fn worktree_for_branch(dir: &str, branch: &str) -> git::Result<Option<String>> {
    let (dir, branch) = (dir.to_owned(), branch.to_owned());
    git::off_thread(move || git::worktree_for_branch(&dir, &branch)).await
}

/// A `git-message` result: `icon` (a `gitMessageIconMap` key), the message,
/// and a detail Bolt draws faint at the right edge. The text carries the
/// detail too, for anything that reads results as text.
fn git_message(ctx: &Ctx, key: &str, icon: &str, message: impl Display, detail: &str) {
    if detail.is_empty() {
        return ctx.add_git(key, icon, message.to_string());
    }
    let rich = rich::Rich::Git { icon: icon.into(), detail: Some(detail.to_owned()) };
    ctx.add_rich(key, format!("{message} — {detail}"), rich);
}

/// A `conflict` result: the message, the files (relative to `directory`),
/// an optional hint.
fn conflict(ctx: &Ctx, key: &str, message: &str, directory: &str, files: &[String], hint: Option<&str>) {
    let mut lines = vec![message.to_owned()];
    lines.extend(files.iter().map(|f| format!("  {f}")));
    lines.extend(hint.map(|h| format!("→ {h}")));
    let rich = rich::Rich::Conflict {
        files: files.to_vec(),
        directory: directory.to_owned(),
        hint: hint.map(str::to_owned),
    };
    ctx.add_rich(key, lines.join("\n"), rich);
}

/// `getFlag`: `--flag [value]` (the value must not look like a flag) or
/// `--flag=value`. An empty value counts as none.
struct Flag {
    present: bool,
    value: Option<String>,
}

fn get_flag(ctx: &Ctx, names: &[&str]) -> Flag {
    for (i, arg) in ctx.args.iter().enumerate() {
        for name in names {
            if arg.as_str() == *name {
                let value = ctx.args.get(i + 1).filter(|n| !n.is_empty() && !n.starts_with('-')).cloned();
                return Flag { present: true, value };
            }
            if let Some(v) = arg.strip_prefix(&format!("{name}=")) {
                return Flag { present: true, value: Some(v.to_owned()).filter(|v| !v.is_empty()) };
            }
        }
    }
    Flag { present: false, value: None }
}

/// The first all-digit argument, else `0` (`stashIndex`).
fn stash_index(ctx: &Ctx) -> &str {
    ctx.args
        .iter()
        .find(|a| !a.is_empty() && a.bytes().all(|b| b.is_ascii_digit()))
        .map_or("0", String::as_str)
}

/// The first seven characters of a hash.
fn short(hash: &str) -> &str {
    &hash[..hash.len().min(7)]
}

fn contains_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| text.contains(n))
}

/// Whether this command is still the newest one (a stale command must not
/// move the directory).
fn is_current(ctx: &Ctx) -> bool {
    ctx.shell.run_id.get_untracked() == ctx.run
}

/// `checkoutBranch`, honouring `--here`.
async fn checkout_branch(ctx: &Ctx, branch: &str) -> git::Result<Option<String>> {
    let (dir, branch, here) = (ctx.directory.clone(), branch.to_owned(), get_flag(ctx, &["--here"]).present);
    git::off_thread(move || git::checkout_branch(&dir, &branch, here)).await
}

// ───────────────────────── commit / amend ─────────────────────────

/// `commit` and `amend` (`gitCommitOperations.commit`).
///
/// `commit --ai` needs Bolt's AI client, which this app does not have.
pub async fn commit(ctx: &Ctx) -> Result {
    let dir = ctx.directory.as_str();
    let amend = ctx.command == "amend";

    if !amend && ctx.args.first().map(String::as_str) == Some("pop") {
        return commit_pop(ctx).await;
    }

    if get_flag(ctx, &["--ai"]).present {
        return Err(format!("{} --ai runs only in the Electron Bolt for now", ctx.command));
    }

    if amend {
        let hash = get_flag(ctx, &["--hash"]);
        let message = ctx
            .args
            .iter()
            .find(|a| !a.starts_with('-') && Some(a.as_str()) != hash.value.as_deref())
            .cloned()
            .ok_or("amend requires a message")?;

        if let Some(target) = hash.value.as_deref().map(str::trim) {
            // Reword that commit: the sequence editor turns its `pick` into
            // `reword`, the commit editor writes the message.
            let target = target.to_owned();
            let sequence = format!("sed -i '' 's/^pick {target}/reword {target}/'");
            let editor = format!("printf '%s' {} >", git::shell_quote(&message));
            let (cwd, base) = (dir.to_owned(), format!("{target}~1"));
            git::off_thread(move || {
                git::git_env(
                    &["rebase", "-i", &base],
                    &cwd,
                    &[("GIT_SEQUENCE_EDITOR", &sequence), ("GIT_EDITOR", &editor)],
                )
            })
            .await?;
            git_message(ctx, "commitAmend", "commit", format!("Rewrote message for {}: {message}", short(&target)), "");
        } else {
            let (cwd, text) = (dir.to_owned(), message.clone());
            git::off_thread(move || git::commit_with_message(&cwd, &text, &["--amend"])).await?;
            git_message(ctx, "commitAmend", "commit", format!("Amended last commit: {message}"), "");
        }
        // Bolt then refreshes a hidden status result; nothing is drawn.
        return Ok(());
    }

    let push_flag = ctx.flag(&["--push", "-p"]);
    let force = ctx.flag(&["--force", "-f"]);
    const KNOWN: [&str; 7] = ["--push", "-p", "--ai", "--amend", "--hash", "--force", "-f"];
    if ctx.args.iter().any(|a| a.starts_with('-') && !KNOWN.contains(&a.as_str())) {
        return Err("Invalid flags provided".into());
    }

    let rest: Vec<&String> = ctx.args.iter().filter(|a| !a.starts_with('-')).collect();
    let branch = current_branch(dir).await?;

    // `commit "message"` and `commit . "message"`.
    let (add_path, message) = match rest.as_slice() {
        [] => (".", None),
        [message] => (".", Some(message.as_str())),
        [path, message, ..] => (path.as_str(), Some(message.as_str())),
    };

    if let Some(message) = message.filter(|m| !m.is_empty()) {
        run(dir, &["add", add_path]).await?;
        git_message(ctx, "gitMessage", "add", format!("Added {add_path}"), "");
        let (cwd, text) = (dir.to_owned(), message.to_owned());
        git::off_thread(move || git::commit_with_message(&cwd, &text, &[])).await?;
        git_message(ctx, "commitMessage", "commit", format!("Message: {message}"), "");
    }
    if push_flag || has_remote(dir, &branch).await {
        push_branch(ctx, force).await?;
    }
    Ok(())
}

/// `commit pop`: reset `HEAD~`, or with a target drop that commit from
/// history, or restore (else remove) that file as the default branch has it.
async fn commit_pop(ctx: &Ctx) -> Result {
    let dir = ctx.directory.as_str();
    let Some(target) = ctx.args[1..].iter().find(|a| !a.starts_with('-')) else {
        let before = run(dir, &["rev-parse", "HEAD"]).await?.trim().to_owned();
        run(dir, &["reset", "HEAD~"]).await?;
        let after = run(dir, &["rev-parse", "HEAD"]).await?.trim().to_owned();
        git_message(ctx, "commitPop", "undo", format!("Reset HEAD~ ({} -> {})", short(&before), short(&after)), "");
        return Ok(());
    };

    let value = target.trim();
    if value.is_empty() {
        return Err("commit pop requires a commit hash or file path".into());
    }

    // A commit hash: drop that commit from history.
    if let Ok(out) = run(dir, &["rev-parse", "--verify", &format!("{value}^{{commit}}")]).await {
        let hash = out.trim().to_owned();
        if run(dir, &["rebase", "--onto", &format!("{hash}~"), &hash]).await.is_err() {
            git_message(
                ctx,
                "commitPop",
                "warning",
                format!("Conflict while dropping {} — resolve conflicts then run 'git rebase --continue'", short(&hash)),
                "",
            );
            return Ok(());
        }
        git_message(ctx, "commitPop", "undo", format!("Dropped commit {} from history", short(&hash)), "");
        return Ok(());
    }

    // A file path: take it from the default branch.
    let default = default_branch(dir).await;
    match run(dir, &["checkout", &default, "--", value]).await {
        Ok(_) => git_message(ctx, "commitPop", "undo", format!("Checked out {value} from {default}"), ""),
        Err(error) => {
            if !contains_any(&error, &["did not match", "pathspec", "not found"]) {
                return Err(error);
            }
            // The default branch has no such file: remove it instead.
            if let Err(rm_error) = run(dir, &["rm", "-f", value]).await {
                if contains_any(&rm_error, &["staged", "index", "not staged"]) {
                    run(dir, &["reset", "HEAD", "--", value]).await?;
                    run(dir, &["rm", "-f", value]).await?;
                } else if rm_error.contains("not in the index") {
                    let (cwd, file) = (dir.to_owned(), value.to_owned());
                    git::off_thread(move || git::shell("rm", &[file], &cwd)).await?;
                } else {
                    return Err(rm_error);
                }
            }
            git_message(ctx, "commitPop", "undo", format!("Removed {value} (file didn't exist on {default})"), "");
        }
    }
    Ok(())
}

// ───────────────────────── push / sync / merge ─────────────────────────

/// `push [--force|-f]`.
pub async fn push(ctx: &Ctx) -> Result {
    push_branch(ctx, ctx.flag(&["--force", "-f"])).await
}

/// `push` (`gitRemoteOperations.push`): sets the upstream first when the
/// branch has no remote.
async fn push_branch(ctx: &Ctx, force: bool) -> Result {
    let dir = ctx.directory.as_str();
    let branch = current_branch(dir).await?;
    let has_remote = has_remote(dir, &branch).await;
    let (verb, past) = if force { ("Force pushing", "Force pushed") } else { ("Pushing", "Pushed") };
    let count = run(dir, &["rev-list", "--count", "HEAD", "--not", "--remotes"])
        .await?
        .trim()
        .parse::<u32>()
        .unwrap_or(0);
    let summary = match count {
        0 => "nothing new".to_owned(),
        1 => "1 commit".to_owned(),
        n => format!("{n} commits"),
    };
    let mut push = vec!["push"];
    push.extend(force.then_some("--force"));

    if !has_remote {
        git_message(ctx, "settingUpstream", "upstream", format!("Setting Upstream for {branch}"), "");
        let mut set_upstream = push.clone();
        set_upstream.extend(["--set-upstream", "origin", &branch]);
        run(dir, &set_upstream).await?;
        git_message(ctx, "gitSetUpstream", "upstream", format!("Set Upstream {branch}"), "");
    }
    git_message(ctx, "gitPushing", "pushing", format!("{verb} {branch}"), "");
    run(dir, &push).await?;
    git_message(ctx, "gitPush", "push", format!("{past} {branch}"), &summary);
    Ok(())
}

/// `merge <branch>`.
pub async fn merge(ctx: &Ctx) -> Result {
    let branch = ctx.positional().ok_or("merge requires a branch name")?.to_owned();
    merge_branch(ctx, &branch).await.map(|_| ())
}

/// `merge` (`gitBranchOperations.merge`): reports conflicts instead of
/// failing; whether it merged cleanly.
async fn merge_branch(ctx: &Ctx, branch: &str) -> git::Result<bool> {
    let dir = ctx.directory.as_str();
    let current = current_branch(dir).await?;
    git_message(ctx, "gitMerging", "merging", format!("Merging {branch} into {current}"), "");
    if run(dir, &["merge", branch]).await.is_ok() {
        git_message(ctx, "gitMerge", "merge", format!("Merged {branch} into {current}"), "");
        return Ok(true);
    }
    let files: Vec<String> = run(dir, &["diff", "--name-only", "--diff-filter=U"])
        .await?
        .lines()
        .map(|f| f.trim().to_owned())
        .filter(|f| !f.is_empty())
        .collect();
    if files.is_empty() {
        ctx.add_error("mergeFailed", format!("Failed to merge {branch} into {current}"));
    } else {
        let s = if files.len() > 1 { "s" } else { "" };
        let message = format!("Merge conflict: {} file{s} need resolution", files.len());
        conflict(ctx, "mergeConflict", &message, dir, &files, None);
    }
    Ok(false)
}

/// `sync [branch]` (`updateCommand` → `update`): pull the branch (the default
/// one unless named) wherever it is checked out, then merge it in.
pub async fn sync(ctx: &Ctx) -> Result {
    let dir = ctx.directory.as_str();
    let current = current_branch(dir).await?;
    let from = match ctx.positional() {
        Some(branch) => branch.to_owned(),
        None => default_branch(dir).await,
    };

    if from == current {
        git_message(ctx, "gitPulling", "pulling", format!("Pulling {from}"), "");
        let Some(stats) = pull_with_stats(ctx, dir, &from).await? else { return Ok(()) };
        git_message(ctx, "gitPull", "pull", format!("Pulled {from}"), &stats);
        return Ok(());
    }

    let location = {
        let (cwd, branch) = (dir.to_owned(), from.clone());
        git::off_thread(move || git::branch_location(&cwd, &branch)).await?
    };
    if let Some(path) = location {
        // Pull it in its own directory; no checkout needed.
        git_message(ctx, "gitPulling", "pulling", format!("Pulling {from}"), "");
        let Some(stats) = pull_with_stats(ctx, &path, &from).await? else { return Ok(()) };
        git_message(ctx, "gitPull", "pull", format!("Pulled {from}"), &stats);
    } else {
        run(dir, &["checkout", &from]).await?;
        git_message(ctx, "gitCheckout-1", "checkout", format!("Checked out {from}"), "");
        git_message(ctx, "gitPulling", "pulling", format!("Pulling {from}"), "");
        let Some(stats) = pull_with_stats(ctx, dir, &from).await? else {
            run(dir, &["checkout", &current]).await?;
            return Ok(());
        };
        git_message(ctx, "gitPull", "pull", format!("Pulled {from}"), &stats);
        run(dir, &["checkout", &current]).await?;
        git_message(ctx, "gitCheckout-2", "checkout", format!("Checked out {current}"), "");
    }

    merge_branch(ctx, &from).await.map(|_| ())
}

/// `git pull origin <branch>` in `dir`, and `+N −M` of what it brought. A
/// pull that would overwrite uncommitted files reports them and gives `None`.
async fn pull_with_stats(ctx: &Ctx, dir: &str, from: &str) -> git::Result<Option<String>> {
    let before = run(dir, &["rev-parse", "HEAD"]).await?.trim().to_owned();
    if let Err(error) = run(dir, &["pull", "origin", from]).await {
        if !error.contains("would be overwritten by merge") {
            return Err(error);
        }
        let files: Vec<String> = error
            .lines()
            .filter(|l| l.starts_with(char::is_whitespace) && !l.trim().is_empty() && !l.contains("->"))
            .map(|l| l.trim().to_owned())
            .collect();
        let folder = dir.rsplit('/').next().unwrap_or(dir);
        let hint = format!("Commit or stash these in {dir}, then run update again.");
        conflict(
            ctx,
            "pullBlocked",
            &format!("Can't pull {from} — uncommitted changes in {folder} would be overwritten"),
            dir,
            &files,
            Some(&hint),
        );
        return Ok(None);
    }
    let stat = run(dir, &["diff", "--shortstat", &before, "HEAD"]).await?.trim().to_owned();
    Ok(Some(shortstat_summary(&stat)))
}

/// `(insertions, deletions)` of a `--shortstat` line; zero for what it
/// doesn't name (`parseShortstat`).
fn shortstat_counts(stat: &str) -> (u32, u32) {
    let count = |word: &str| {
        stat.split(',')
            .filter_map(|part| {
                let mut tokens = part.split_whitespace();
                let n = tokens.next()?.parse().ok()?;
                tokens.next()?.starts_with(word).then_some(n)
            })
            .next()
            .unwrap_or(0)
    };
    (count("insertion"), count("deletion"))
}

/// `+adds −dels` of a `--shortstat` line; empty when there is none.
fn shortstat_summary(stat: &str) -> String {
    if stat.is_empty() {
        return String::new();
    }
    let (adds, dels) = shortstat_counts(stat);
    format!("+{adds} −{dels}")
}

// ───────────────────────── branches ─────────────────────────

/// `new`: a branch, a worktree for one, or a worktree for the current branch.
pub async fn new(ctx: &Ctx) -> Result {
    let dir = ctx.directory.as_str();
    let name = ctx.positional().map(str::to_owned);
    let bare = name.is_some() && !ctx.flag(&["-b", "--branch", "-w", "--worktree"]);
    let wants_worktree = bare || ctx.flag(&["--worktree", "-w"]);

    if !wants_worktree {
        let name = name.ok_or("new requires a branch name")?;
        let parent = match get_flag(ctx, &["--parent"]).value {
            Some(parent) => parent,
            None => default_branch(dir).await,
        };
        run(dir, &["branch", &name, &parent]).await?;
        run(dir, &["checkout", &name]).await?;
        return Ok(());
    }

    let wants_branch = bare || ctx.flag(&["--branch", "-b"]);
    let (path, setup_failed) = if let Some(name) = name {
        if wants_branch {
            let parent = get_flag(ctx, &["--parent"]).value;
            create_worktree(ctx, &name, true, parent.as_deref()).await?
        } else {
            create_worktree(ctx, &name, false, None).await?
        }
    } else {
        // No name: a worktree for the current branch.
        let target = current_branch(dir).await?;
        if worktree_context(dir).await?.is_worktree {
            return Err(format!("Already in a worktree for '{target}'"));
        }
        let default = default_branch(dir).await;
        if target == default {
            return Err(format!("Cannot create worktree for default branch '{target}'"));
        }
        // git refuses to check a branch out twice: move this one to default first.
        run(dir, &["checkout", &default]).await?;
        create_worktree(ctx, &target, false, None).await?
    };

    // The tab and the directory change in one settings update.
    let current = is_current(ctx);
    ctx.shell.update_settings(|s| {
        s.tabs.push(Tab::new(tab_name_for(&path), path.clone()));
        if current {
            s.directory.clone_from(&path);
        }
    });
    if current {
        ctx.shell.refresh_status(path.clone());
    }

    let failed = if setup_failed.is_empty() { String::new() } else { format!(" — setup failed: {}", setup_failed.join(", ")) };
    ctx.add("worktree", format!("Created worktree at {path}{failed}"));
    Ok(())
}

/// A worktree at `~/.worktrees/<repo>/<branch>` for `branch` (with `-b`: the
/// branch is created from `parent` unless it exists) and its setup
/// (`createWorktree` / `createBranchWithWorktree`): the path and what failed.
async fn create_worktree(
    ctx: &Ctx,
    branch: &str,
    create_branch: bool,
    parent: Option<&str>,
) -> git::Result<(String, Vec<String>)> {
    let dir = ctx.directory.as_str();
    let wt = worktree_context(dir).await?;
    let home = std::env::var("HOME").unwrap_or_else(|_| ctx.settings.home.clone());
    let repo = wt.main_repo_path.rsplit('/').next().filter(|r| !r.is_empty()).unwrap_or("repo");
    let root = format!("{home}/.worktrees/{repo}");
    let path = format!("{root}/{branch}");

    if let Some(existing) = worktree_for_branch(dir, branch).await? {
        return Err(format!("Worktree for '{branch}' already exists at {existing}"));
    }
    let made = root.clone();
    git::off_thread(move || std::fs::create_dir_all(made).map_err(|e| e.to_string())).await?;

    let exists = create_branch && run(dir, &["rev-parse", "--verify", branch]).await.is_ok();
    if create_branch && !exists {
        let mut args = vec!["worktree", "add", "-b", branch, &path];
        args.extend(parent);
        run(dir, &args).await?;
    } else {
        run(dir, &["worktree", "add", &path, branch]).await?;
    }

    let (main, target) = (wt.main_repo_path, path.clone());
    let failed = git::off_thread(move || git::setup_worktree(&main, &target)).await?;
    Ok((path, failed))
}

/// `delete`: a stash with `--stash`/`-s`, else a branch.
pub async fn delete(ctx: &Ctx) -> Result {
    if ctx.flag(&["--stash", "-s"]) {
        let index = stash_index(ctx);
        run(&ctx.directory, &["stash", "drop", &format!("stash@{{{index}}}")]).await?;
        ctx.add("stash-drop", format!("Dropped stash@{{{index}}}"));
        return Ok(());
    }
    delete_branch(ctx).await
}

/// `deleteBranch`. Bolt asks through a close-tab modal before removing a
/// branch's worktree; without it this takes the handler's own fallback for
/// "no UI bridge": drop the tab, then remove the worktree and branch.
async fn delete_branch(ctx: &Ctx) -> Result {
    let dir = ctx.directory.as_str();
    let delete_remote = ctx.flag(&["-r", "--remote"]);

    if !Path::new(dir).is_dir() {
        return delete_stale_worktree(ctx).await;
    }

    // `delete <name> -b` deletes <name>, not whatever is checked out.
    let branch = match ctx.positional() {
        Some(branch) => branch.to_owned(),
        None => current_branch(dir).await?,
    };
    let default = default_branch(dir).await;

    if ctx.flag(&["-v", "--verify"]) {
        git_message(ctx, "gitFetching", "fetching", "Fetching", "");
        run(dir, &["fetch", "-p"]).await?;
        git_message(ctx, "gitFetch", "fetch", "Fetched", "");
        if has_remote(dir, &branch).await {
            ctx.add_error("verifyRemote",
            format!("'{branch}' still has a remote origin — not deleting. Use 'delete {branch} -b' to delete anyway."),);
            return Ok(());
        }
    }

    if delete_remote {
        run(dir, &["push", "origin", "-d", &branch]).await?;
        return Ok(());
    }

    if let Some(worktree) = worktree_for_branch(dir, &branch).await? {
        let wt = worktree_context(dir).await?;
        let main = wt.main_repo_path;
        // Close the tab first, then remove (and leave it when standing in it).
        ctx.shell.update_settings(|s| s.tabs.retain(|t| t.path != worktree));
        if wt.is_worktree {
            ctx.set_directory(main.clone());
        }
        let removed = async {
            if run(&main, &["worktree", "remove", "--force", &worktree]).await.is_err() {
                run(&main, &["worktree", "prune"]).await?;
            }
            run(&main, &["branch", "-D", &branch]).await
        }
        .await;
        match removed {
            Ok(_) => ctx.add("removeWorktree", format!("Removed worktree at {worktree} and deleted branch {branch}")),
            Err(error) => ctx.add_error("removeWorktree", error),
        }
        return Ok(());
    }

    // No worktree: leave the branch for the default one, then delete it.
    match checkout_branch(ctx, &default).await? {
        Some(path) => {
            ctx.set_directory(path.clone());
            run(&path, &["branch", "-D", &branch]).await?;
        }
        None => {
            run(dir, &["branch", "-D", &branch]).await?;
        }
    }
    Ok(())
}

/// The directory is gone (a worktree deleted by hand): find the repo through
/// another tab, prune the worktree, drop its tab and delete its branch.
async fn delete_stale_worktree(ctx: &Ctx) -> Result {
    let dir = ctx.directory.as_str();
    let fallback = ctx
        .settings
        .tabs
        .iter()
        .find(|t| t.path != dir && Path::new(&t.path).is_dir())
        .map_or_else(|| ctx.settings.home.clone(), |t| t.path.clone());

    let listing = {
        let fallback = fallback.clone();
        git::off_thread(move || git::worktrees(&fallback)).await?
    };
    let branch = listing
        .into_iter()
        .find(|(path, _)| path == dir)
        .and_then(|(_, branch)| branch)
        .ok_or_else(|| format!("Could not determine branch for missing directory {dir}"))?;
    let main = worktree_context(&fallback).await?.main_repo_path;

    run(&main, &["worktree", "prune"]).await?;
    ctx.shell.update_settings(|s| s.tabs.retain(|t| t.path != dir));
    ctx.set_directory(main.clone());
    run(&main, &["branch", "-D", &branch]).await?;
    ctx.add("removeWorktree", format!("Pruned stale worktree and deleted branch {branch}"));
    Ok(())
}

/// `prune`: delete the local branches `origin` no longer has.
pub async fn prune(ctx: &Ctx) -> Result {
    let dir = ctx.directory.as_str();
    let locals = {
        let dir = dir.to_owned();
        git::off_thread(move || git::branches_without_remote(&dir)).await?
    };
    let default = default_branch(dir).await;
    checkout_branch(ctx, &default).await?;
    for branch in locals.iter().filter(|b| **b != default) {
        run(dir, &["branch", "-d", branch]).await?;
    }
    Ok(())
}

/// `rename <new-name>`: the branch, its remote branch, and its worktree.
pub async fn rename(ctx: &Ctx) -> Result {
    let dir = ctx.directory.as_str();
    let new_name = ctx.positional().ok_or("rename requires a new branch name")?.to_owned();

    let old = current_branch(dir).await?;
    let worktree = worktree_for_branch(dir, &old).await?;

    let remote = has_remote(dir, &old).await;
    run(dir, &["branch", "-m", &new_name]).await?;
    ctx.add("renameBranch", format!("Renamed branch {old} to {new_name}"));
    if remote {
        run(dir, &["push", "origin", "--delete", &old]).await?;
        ctx.add("deleteRemoteBranch", format!("Deleted remote branch {old}"));
        run(dir, &["push", "origin", "-u", &new_name]).await?;
        ctx.add("renameRemoteBranch", format!("Renamed remote branch {old} to {new_name}"));
    }

    let Some(worktree) = worktree else { return Ok(()) };
    let moved = match worktree.rsplit_once('/') {
        Some((parent, _)) => format!("{parent}/{new_name}"),
        None => worktree.clone(),
    };
    run(dir, &["worktree", "move", &worktree, &moved]).await?;

    if ctx.shell.settings.with_untracked(|s| s.tabs.iter().any(|t| t.path == worktree)) {
        ctx.shell.update_settings(|s| {
            if let Some(tab) = s.tabs.iter_mut().find(|t| t.path == worktree) {
                tab.name = tab_name_for(&moved);
                tab.path.clone_from(&moved);
            }
        });
    }
    if dir == worktree {
        ctx.set_directory(moved.clone());
    }
    ctx.add("renameWorktree", format!("Moved worktree to {moved}"));
    Ok(())
}

/// `open [--browser]`: the branch's PR (GitHub, through `gh`) or its compare
/// / tree page. Bolt shows it in its in-app browser unless `--browser`; here
/// both go to the system browser. `open <path>[:line[:column]]` opens the
/// file in Helix (`hx`) in a new terminal instead.
pub async fn open(ctx: &Ctx) -> Result {
    if let Some(arg) = ctx.positional() {
        return open_file(ctx, arg);
    }
    let dir = ctx.directory.as_str();
    let branch = current_branch(dir).await?;
    if !has_remote(dir, &branch).await {
        ctx.add_error("remote", format!("No remote branch found for '{branch}'"));
        return Ok(());
    }

    let remote = run(dir, &["config", "--get", "remote.origin.url"]).await?.trim().to_owned();
    let web = web_url(&remote);
    let host = host_of(&web).ok_or("Invalid URL")?;
    let target = if host == "github.com" || host.ends_with(".github.com") {
        // `gh` knows the PR; none (or no `gh`) falls through to the
        // create-PR page. Any PR state will do.
        let cwd = dir.to_owned();
        let pr = git::off_thread(move || pr_url(&cwd)).await;
        pr.unwrap_or_else(|| format!("{web}/compare/{branch}?expand=1"))
    } else {
        format!("{web}/-/tree/{branch}?ref_type=heads")
    };

    let cwd = dir.to_owned();
    git::off_thread(move || git::shell("/usr/bin/open", &[git::shell_quote(&target)], &cwd)).await?;
    Ok(())
}

/// `open <path>[:line[:column]]`: the path relative to the directory (or
/// `~/…`, or absolute), line and column from 1 as editors print them.
fn open_file(ctx: &Ctx, arg: &str) -> Result {
    let (path, at) = split_position(arg);
    let expanded = match path.strip_prefix("~/") {
        Some(rest) => dirs::home_dir().unwrap_or_default().join(rest),
        None => std::path::PathBuf::from(path),
    };
    let full = Path::new(&ctx.directory).join(expanded).canonicalize().map_err(|_| format!("No such file: {path}"))?;
    if !full.is_file() {
        return Err(format!("Not a file: {path}"));
    }
    ctx.shell.open_in_helix(ctx.directory.clone(), &full, at)
}

/// `src/main.rs:12:5` → (`src/main.rs`, line 11, column 4), `a.rs:3` →
/// line 2; a path with no number after its last colon stays whole.
fn split_position(arg: &str) -> (&str, Option<(u32, u32)>) {
    let num = |s: &str| s.parse::<u32>().ok().filter(|n| *n > 0);
    let Some((rest, last)) = arg.rsplit_once(':').and_then(|(r, l)| Some((r, num(l)?))) else {
        return (arg, None);
    };
    match rest.rsplit_once(':').and_then(|(p, l)| Some((p, num(l)?))) {
        Some((path, line)) => (path, Some((line - 1, last - 1))),
        None => (rest, Some((last - 1, 0))),
    }
}

/// `git@host:org/repo.git` → `https://host/org/repo`.
fn web_url(remote: &str) -> String {
    let mut url = remote.to_owned();
    if let Some((host, path)) = remote.strip_prefix("git@").and_then(|r| r.split_once(':')).filter(|(h, _)| !h.is_empty()) {
        url = format!("https://{host}/{path}");
    }
    url.strip_suffix(".git").map_or(url.clone(), str::to_owned)
}

/// A URL's lowercase host (`new URL(url).hostname`).
fn host_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    if scheme.is_empty() {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit('@').next()?.split(':').next()?;
    (!host.is_empty()).then(|| host.to_lowercase())
}

/// The PR `gh` finds for the checked-out branch; a `GITHUB_TOKEN` from the
/// launching shell can override `gh`'s keyring and fail, so retry without.
fn pr_url(dir: &str) -> Option<String> {
    let view = "gh pr view --json url";
    let out = git::shell(&format!("{view} 2>/dev/null || GITHUB_TOKEN= GH_TOKEN= {view}"), &[], dir).ok()?;
    let value: serde_json::Value = serde_json::from_str(&out).ok()?;
    value.get("url")?.as_str().filter(|u| !u.is_empty()).map(str::to_owned)
}

/// `trace`: the branches in HEAD's ancestry, nearest first.
pub async fn trace(ctx: &Ctx) -> Result {
    let dir = ctx.directory.clone();
    let current = current_branch(&dir).await?;
    let found = git::off_thread(move || trace_branches(&dir)).await?;
    if found.is_empty() {
        ctx.add("trace", trace_text(&current, &found));
    } else {
        ctx.add_rich("trace", trace_text(&current, &found), rich::Rich::Direct);
    }
    Ok(())
}

/// Branches whose tips are ancestors of HEAD, by commits behind (`traceBranches`).
fn trace_branches(dir: &str) -> git::Result<Vec<(String, u32)>> {
    let current = git::show_current_branch(dir)?;
    let default = git::default_branch(dir);
    let mut others: Vec<String> =
        git::branches(dir)?.into_iter().filter(|b| *b != current && !b.starts_with('(')).collect();
    others.sort_by_key(|b| *b != default); // the default branch leads, as Bolt lists it

    let per_thread = others.len().div_ceil(8).max(1);
    let mut found: Vec<(String, u32)> = std::thread::scope(|scope| {
        let handles: Vec<_> = others
            .chunks(per_thread)
            .map(|chunk| {
                scope.spawn(move || {
                    chunk
                        .iter()
                        .filter_map(|name| {
                            git::git(&["merge-base", "--is-ancestor", name, "HEAD"], dir).ok()?;
                            let out = git::git(&["rev-list", "--count", &format!("{name}..HEAD")], dir).ok()?;
                            let distance = out.trim().parse::<u32>().unwrap_or(0);
                            (distance != 0).then(|| (name.clone(), distance))
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
    });
    found.sort_by_key(|(_, distance)| *distance);
    Ok(found)
}

fn trace_text(current: &str, trace: &[(String, u32)]) -> String {
    if trace.is_empty() {
        return format!("No ancestor branches found for {current}");
    }
    let width = trace.iter().map(|(name, _)| name.chars().count()).max().unwrap_or(0);
    let lines = trace.iter().map(|(name, distance)| {
        let pad = " ".repeat(width - name.chars().count());
        let label = if *distance == 1 { "commit" } else { "commits" };
        format!("  {name}{pad}  {distance} {label} behind")
    });
    format!("{current}\n{}", lines.collect::<Vec<_>>().join("\n"))
}

// ───────────────────────── stash / reset ─────────────────────────

/// `stash`: everything, untracked files included.
pub async fn stash(ctx: &Ctx) -> Result {
    run(&ctx.directory, &["stash", "-u"]).await?;
    ctx.add("stash", "Stashed changes");
    Ok(())
}

/// `apply`: the latest stash (Bolt ignores an index here).
pub async fn apply(ctx: &Ctx) -> Result {
    run(&ctx.directory, &["stash", "apply"]).await?;
    ctx.add("stash", "Applied stash");
    Ok(())
}

/// `pop [n]`.
pub async fn pop(ctx: &Ctx) -> Result {
    let index = stash_index(ctx);
    run(&ctx.directory, &["stash", "pop", &format!("stash@{{{index}}}")]).await?;
    ctx.add("stash-pop", format!("Popped stash@{{{index}}}"));
    Ok(())
}

/// `show [n]`: the stash's patch. Bolt opens it in a diff tab; here it is
/// the result text.
pub async fn show(ctx: &Ctx) -> Result {
    let index = stash_index(ctx);
    let patch = run(&ctx.directory, &["stash", "show", "-p", &format!("stash@{{{index}}}")]).await?;
    let patch = patch.trim_end();
    ctx.add("stashShow",
    if patch.is_empty() { format!("stash@{{{index}}} has no changes") } else { patch.to_owned() },);
    Ok(())
}

/// `reset --hard [branch]`: discard everything, back to the branch's fork
/// point from `origin` when named.
pub async fn reset(ctx: &Ctx) -> Result {
    let dir = ctx.directory.as_str();
    // Destructive and unrecoverable: the flag is the confirmation.
    if !ctx.flag(&["--hard"]) {
        ctx.add_error("reset", "reset discards all uncommitted work — run 'reset --hard' to confirm");
        return Ok(());
    }

    let branch = ctx.positional().map(str::to_owned);
    let mut target = None;
    if let Some(branch) = &branch {
        run(dir, &["fetch", "origin"]).await?;
        let base = run(dir, &["merge-base", "HEAD", &format!("origin/{branch}")]).await?;
        target = Some(base.trim().to_owned());
    }
    let mut hard = vec!["reset", "--hard"];
    hard.extend(target.as_deref());
    run(dir, &hard).await?;
    run(dir, &["clean", "-fd"]).await?;
    ctx.add("reset",
    match branch {
        Some(branch) => format!("Reset to where this branch left origin/{branch} (hard reset + clean)"),
        None => "Reset complete (hard reset + clean)".to_owned(),
    },);
    Ok(())
}

// ───────────────────────── status ─────────────────────────

/// `status [--all] [--show[=<hash|path>]]`. Bolt opens the diffs `--show`
/// asks for in a diff or content tab; here they are the result text.
pub async fn status(ctx: &Ctx) -> Result {
    let dir = ctx.directory.as_str();
    let show = get_flag(ctx, &["--show", "-s"]);

    if show.present {
        let text = match show.value {
            Some(value) => show_target(dir, &value).await?,
            None => diff_sections(dir, None).await?.unwrap_or_else(|| "No changes to show".into()),
        };
        ctx.add("statusShow", text.trim().to_owned());
        return Ok(());
    }

    let mut info = parse_status(&run(dir, &["status", "--porcelain"]).await?);
    // Commits no remote has; a repo without commits has none.
    info.unpushed = run(dir, &["log", "HEAD", "--not", "--remotes", "--format=%h%x09%s"])
        .await
        .map(|log| {
            log.lines()
                .filter(|l| !l.is_empty())
                .map(|l| {
                    let (hash, subject) = l.split_once('\t').unwrap_or((l, ""));
                    (hash.to_owned(), subject.to_owned())
                })
                .collect()
        })
        .unwrap_or_default();
    if ctx.flag(&["--all"]) {
        info.branch_stats = branch_diff_stats(dir).await?;
    }
    let card_dir = dir.to_owned();
    let loc = git::off_thread(move || loc_stats(&card_dir)).await;
    ctx.add_rich("gitStatus", render_status(&info), rich::Rich::Status(info.card(loc)));
    Ok(())
}

/// `--show=<value>`: a commit's diff against its parent, else the staged and
/// unstaged diff of a path.
async fn show_target(dir: &str, value: &str) -> git::Result<String> {
    if let Ok(out) = run(dir, &["rev-parse", "--verify", &format!("{value}^{{commit}}")]).await {
        let hash = out.trim().to_owned();
        if let Ok(diff) = run(dir, &["diff", &format!("{hash}~1"), &hash]).await {
            let diff = diff.trim();
            return Ok(if diff.is_empty() { format!("No changes in commit {value}") } else { diff.to_owned() });
        }
    }
    Ok(diff_sections(dir, Some(value)).await?.unwrap_or_else(|| format!("No changes for {value}")))
}

/// The staged and unstaged diffs, headed; `None` when both are empty.
async fn diff_sections(dir: &str, path: Option<&str>) -> git::Result<Option<String>> {
    let mut staged = vec!["diff", "--cached"];
    let mut unstaged = vec!["diff"];
    if let Some(path) = path {
        staged.extend(["--", path]);
        unstaged.extend(["--", path]);
    }
    let (staged, unstaged) = (run(dir, &staged).await?, run(dir, &unstaged).await?);
    let mut out = String::new();
    if !staged.trim().is_empty() {
        out += &format!("━━━ Staged changes ━━━\n{staged}\n");
    }
    if !unstaged.trim().is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out += &format!("━━━ Unstaged changes ━━━\n{unstaged}");
    }
    Ok((!out.is_empty()).then_some(out))
}

/// `getBranchDiffStats`: "x files changed, …" from the merge-base with the
/// default branch through HEAD.
async fn branch_diff_stats(dir: &str) -> git::Result<String> {
    let default = default_branch(dir).await;
    let base = run(dir, &["merge-base", &default, "HEAD"]).await?.trim().to_owned();
    if base.is_empty() {
        return Ok(String::new());
    }
    let stat = run(dir, &["diff", "--shortstat", &format!("{base}..HEAD")]).await?;
    let stat = stat.trim();
    Ok(if stat.is_empty() { "0 files changed".into() } else { stat.to_owned() })
}

/// `useGitStatus`' `locStats`: lines added and removed since the branch's
/// base (nothing on the default branch itself) and in the working tree,
/// where untracked files count each of their lines as added. Whatever git
/// can't say counts as zero.
fn loc_stats(dir: &str) -> rich::Loc {
    let counts = |args: &[&str]| git::git(args, dir).map_or((0, 0), |stat| shortstat_counts(&stat));
    let default = git::default_branch(dir);
    let committed = match git::current_branch(dir) {
        Ok(current) if current != default => {
            let base = find_base_branch(dir, &default);
            counts(&["diff", "--shortstat", &format!("{base}...HEAD")])
        }
        _ => (0, 0),
    };
    let (tracked_adds, dels) = counts(&["diff", "--shortstat", "HEAD"]);
    rich::Loc { committed, uncommitted: (tracked_adds + untracked_lines(dir), dels) }
}

/// The local branch nearest above HEAD (fewest commits since their
/// merge-base), the default one winning ties; a branch at or ahead of HEAD
/// can't be its parent. The default branch when it isn't among them, since
/// it then already contains HEAD (`findBaseBranch`).
fn find_base_branch(dir: &str, default: &str) -> String {
    let Ok(head) = git::git(&["rev-parse", "HEAD"], dir) else { return default.to_owned() };
    let head = head.trim();
    let Ok(refs) = git::git(&["for-each-ref", "--format=%(refname:short)", "refs/heads"], dir) else {
        return default.to_owned();
    };
    let (mut best, mut best_count, mut saw_default) = (default, u32::MAX, false);
    for name in refs.lines().map(str::trim).filter(|n| !n.is_empty()) {
        let Ok(base) = git::git(&["merge-base", name, "HEAD"], dir) else { continue };
        let base = base.trim();
        if base == head {
            continue;
        }
        let Ok(count) = git::git(&["rev-list", "--count", &format!("{base}..HEAD")], dir) else { continue };
        let Ok(count) = count.trim().parse::<u32>() else { continue };
        saw_default |= name == default;
        if count < best_count || (count == best_count && name == default) {
            (best, best_count) = (name, count);
        }
    }
    (if saw_default { best } else { default }).to_owned()
}

/// Lines in the files git doesn't track or ignore (`wc -l`: newlines).
fn untracked_lines(dir: &str) -> u32 {
    let Ok(files) = git::git(&["ls-files", "-z", "--others", "--exclude-standard"], dir) else { return 0 };
    files
        .split('\0')
        .filter(|f| !f.is_empty())
        .filter_map(|f| std::fs::read(Path::new(dir).join(f)).ok())
        .map(|bytes| bytes.iter().filter(|b| **b == b'\n').count() as u32)
        .sum()
}

#[derive(Debug, Default, PartialEq)]
struct StatusInfo {
    modified: Vec<String>,
    added: Vec<String>,
    deleted: Vec<String>,
    renamed: Vec<String>,
    copied: Vec<String>,
    untracked: Vec<String>,
    unpushed: Vec<(String, String)>,
    branch_stats: String,
}

impl StatusInfo {
    /// The status card's data; `branch_stats` is only there when asked for
    /// (`status --all`).
    fn card(self, loc: rich::Loc) -> rich::GitStatus {
        rich::GitStatus {
            modified: self.modified,
            added: self.added,
            deleted: self.deleted,
            renamed: self.renamed,
            copied: self.copied,
            untracked: self.untracked,
            branch_stats: (!self.branch_stats.is_empty()).then_some(self.branch_stats),
            unpushed: self.unpushed.into_iter().map(|(hash, subject)| rich::Commit { hash, subject }).collect(),
            loc: Some(loc),
        }
    }
}

/// `gitStatus`' parse of `git status --porcelain`.
fn parse_status(porcelain: &str) -> StatusInfo {
    let mut info = StatusInfo::default();
    for line in porcelain.lines().map(|l| l.trim_end_matches('\r')).filter(|l| !l.is_empty()) {
        let code = line.get(..2).unwrap_or(line);
        let mut rest = line.get(3..).unwrap_or("");
        if let Some((_, to)) = rest.rsplit_once(" -> ") {
            rest = to.trim();
        }
        let file = rest.to_owned();
        if code == "??" {
            info.untracked.push(file);
            continue;
        }
        for (letter, list) in [
            ('M', &mut info.modified),
            ('A', &mut info.added),
            ('D', &mut info.deleted),
            ('R', &mut info.renamed),
            ('C', &mut info.copied),
        ] {
            if code.contains(letter) {
                list.push(file.clone());
            }
        }
    }
    info
}

/// The status card as text: the stats line, `unpushed`, then each non-empty
/// list; a clean tree says so.
fn render_status(info: &StatusInfo) -> String {
    let mut blocks = Vec::new();
    if !info.branch_stats.is_empty() {
        blocks.push(info.branch_stats.clone());
    }
    if !info.unpushed.is_empty() {
        let commits = info.unpushed.iter().map(|(hash, subject)| format!("  {hash} {subject}"));
        blocks.push(format!("unpushed\n{}", commits.collect::<Vec<_>>().join("\n")));
    }
    let lists = [
        ("modified", &info.modified),
        ("added", &info.added),
        ("deleted", &info.deleted),
        ("renamed", &info.renamed),
        ("copied", &info.copied),
        ("untracked", &info.untracked),
    ];
    let mut any = false;
    for (name, files) in lists.into_iter().filter(|(_, files)| !files.is_empty()) {
        any = true;
        let files = files.iter().map(|f| format!("  {f}"));
        blocks.push(format!("{name}\n{}", files.collect::<Vec<_>>().join("\n")));
    }
    if !any && info.branch_stats.is_empty() {
        blocks.push("Nothing to commit, working tree clean".into());
    }
    blocks.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_reads_line_and_column_after_the_path() {
        assert_eq!(split_position("src/main.rs:12:5"), ("src/main.rs", Some((11, 4))));
        assert_eq!(split_position("src/main.rs:12"), ("src/main.rs", Some((11, 0))));
        assert_eq!(split_position("src/main.rs"), ("src/main.rs", None));
        assert_eq!(split_position("notes:todo.md"), ("notes:todo.md", None));
    }

    #[test]
    fn parses_porcelain_like_git_status() {
        let info = parse_status("MM a.rs\n?? new.txt\nR  old.rs -> moved.rs\n D gone.rs\nA  fresh.rs\n");
        assert_eq!(info.modified, ["a.rs"]);
        assert_eq!(info.added, ["fresh.rs"]);
        assert_eq!(info.deleted, ["gone.rs"]);
        assert_eq!(info.renamed, ["moved.rs"]);
        assert_eq!(info.untracked, ["new.txt"]);
    }

    #[test]
    fn renders_status_sections_or_a_clean_tree() {
        assert_eq!(render_status(&StatusInfo::default()), "Nothing to commit, working tree clean");
        let info = StatusInfo {
            modified: vec!["a.rs".into()],
            unpushed: vec![("abc1234".into(), "fix".into())],
            branch_stats: "1 file changed".into(),
            ..Default::default()
        };
        assert_eq!(render_status(&info), "1 file changed\n\nunpushed\n  abc1234 fix\n\nmodified\n  a.rs");
    }

    #[test]
    fn sums_a_shortstat() {
        assert_eq!(shortstat_summary(" 2 files changed, 10 insertions(+), 3 deletions(-)"), "+10 −3");
        assert_eq!(shortstat_summary(" 1 file changed, 1 deletion(-)"), "+0 −1");
        assert_eq!(shortstat_summary(""), "");
        assert_eq!(shortstat_counts(" 2 files changed, 10 insertions(+), 3 deletions(-)"), (10, 3));
        assert_eq!(shortstat_counts(" 1 file changed, 1 insertion(+)"), (1, 0));
        assert_eq!(shortstat_counts(""), (0, 0));
    }

    #[test]
    fn turns_remotes_into_web_urls() {
        assert_eq!(web_url("git@github.com:org/repo.git"), "https://github.com/org/repo");
        assert_eq!(web_url("https://gitlab.com/org/repo.git"), "https://gitlab.com/org/repo");
        assert_eq!(host_of("https://GitHub.com/org/repo").as_deref(), Some("github.com"));
        assert_eq!(host_of("ssh://git@host.example:22/org/repo").as_deref(), Some("host.example"));
        assert_eq!(host_of("/local/path"), None);
    }

    #[test]
    fn pads_the_trace_to_the_longest_name() {
        let trace = [("main".to_owned(), 3), ("dev-long".to_owned(), 1)];
        assert_eq!(trace_text("feat", &trace), "feat\n  main      3 commits behind\n  dev-long  1 commit behind");
        assert_eq!(trace_text("feat", &[]), "No ancestor branches found for feat");
    }

    #[test]
    fn shortens_hashes() {
        assert_eq!(short("abcdef0123"), "abcdef0");
        assert_eq!(short("abc"), "abc");
    }
}
