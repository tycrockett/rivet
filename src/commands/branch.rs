//! Branch verbs, ported from Bolt's `commandHandlers/branchCommand.ts` and
//! `listCommand.ts`.

use super::{
    Ctx,
    rich::{self, Branch, BranchList},
};
use crate::git;

type Result = std::result::Result<(), String>;

/// `go <branch>`: to the branch's worktree (or the main repo for the
/// default branch), else checked out here; `go --default` names the
/// default branch.
pub async fn go(ctx: &Ctx) -> Result {
    let dir = ctx.directory.clone();
    if ctx.flag(&["--default"]) {
        let branch = git::off_thread(move || git::default_branch(&dir)).await;
        ctx.add("defaultBranch", format!("Default branch is: {branch}"));
        return Ok(());
    }
    let branch = ctx
        .positional()
        .ok_or("go requires a branch name — 'list' shows the branches")?
        .to_owned();
    let here = ctx.flag(&["--here"]);
    let target = git::off_thread(move || git::checkout_branch(&dir, &branch, here)).await?;
    let at = target.clone().unwrap_or_else(|| ctx.directory.clone());
    if let Some(path) = target {
        ctx.set_directory(path);
    }
    show_branches(ctx, at).await
}

/// `list`: the branches, the current one marked.
pub async fn list(ctx: &Ctx) -> Result {
    if ctx.flag(&["--commit", "-c", "--stash", "-s"]) {
        return Err("list --commit / --stash run only in the Electron Bolt for now".into());
    }
    show_branches(ctx, ctx.directory.clone()).await
}

async fn show_branches(ctx: &Ctx, dir: String) -> Result {
    let (branches, current, card) = git::off_thread(move || {
        (git::branches(&dir), git::current_branch(&dir), branch_list(&dir))
    })
    .await;
    let current = current.unwrap_or_default();
    let lines: Vec<String> = branches?
        .into_iter()
        .map(|b| if b == current { format!("* {b}") } else { format!("  {b}") })
        .collect();
    match card {
        Ok(card) => ctx.add_rich("branches", lines.join("\n"), rich::Rich::Branches(card)),
        Err(_) => ctx.add("branches", lines.join("\n")),
    }
    Ok(())
}

/// `getBranches`: each branch with whether `origin` has it, whether it
/// sits in a worktree other than the main repo, and its last commit; the
/// default branch first.
fn branch_list(dir: &str) -> git::Result<BranchList> {
    let verbose = git::git(&["branch", "-v", "--no-abbrev"], dir)?;
    let remotes = git::git(&["branch", "-r", "--no-abbrev"], dir).unwrap_or_default();
    let remotes: Vec<&str> = remotes.lines().map(str::trim).filter(|l| !l.is_empty() && !l.contains("->")).collect();
    let main = git::worktree_context(dir)?.main_repo_path;
    let worktrees: Vec<String> = git::worktrees(dir)?
        .into_iter()
        .filter(|(path, _)| *path != main)
        .filter_map(|(_, branch)| branch)
        .collect();
    let default = git::default_branch(dir);
    let mut list: Vec<Branch> = parse_branch_verbose(&verbose)
        .into_iter()
        .map(|(name, last_commit)| Branch {
            has_remote: remotes.contains(&format!("origin/{name}").as_str()),
            is_worktree: worktrees.contains(&name),
            name,
            last_commit,
        })
        .collect();
    list.sort_by_key(|b| b.name != default);
    Ok(BranchList { current: git::current_branch(dir)?, default_branch: Some(default), list })
}

/// `git branch -v --no-abbrev` as `(name, "<7-char hash> <subject>")`: the
/// `*` / `+` marks dropped, a detached HEAD's `(HEAD detached at …)` one name.
fn parse_branch_verbose(text: &str) -> Vec<(String, String)> {
    text.lines()
        .map(|l| l.trim_start_matches(['*', '+', ' ']).trim_end())
        .filter(|l| !l.is_empty())
        .map(|line| {
            let end = if line.starts_with('(') { line.find(')').map_or(line.len(), |i| i + 1) } else {
                line.find(char::is_whitespace).unwrap_or(line.len())
            };
            let (name, rest) = line.split_at(end);
            let rest = rest.trim_start();
            let (hash, subject) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            let is_hash = !hash.is_empty() && hash.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
            let last_commit = if is_hash { format!("{} {}", &hash[..hash.len().min(7)], subject.trim_start()) } else { String::new() };
            (name.to_owned(), last_commit)
        })
        .collect()
}

/// `fetch`: a pruning fetch, then whether the current branch still has a
/// remote.
pub async fn fetch(ctx: &Ctx) -> Result {
    let dir = ctx.directory.clone();
    let message = git::off_thread(move || -> git::Result<String> {
        let branch = git::current_branch(&dir)?;
        git::git(&["fetch", "-p"], &dir)?;
        let remote = git::git(&["branch", "-r", "--list", &format!("origin/{branch}")], &dir)?;
        Ok(if remote.trim().is_empty() {
            format!("'{branch}' has no remote origin")
        } else {
            format!("'{branch}' still has a remote origin")
        })
    })
    .await?;
    ctx.add("branchFetch", message);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_verbose_branches() {
        let text = "* main      0123456789abcdef0123456789abcdef01234567 first commit\n+ wt/x      fedcba9876543210fedcba9876543210fedcba98 on a worktree\n  (HEAD detached at abc) 0123456 detached\n";
        let parsed = parse_branch_verbose(text);
        assert_eq!(parsed[0], ("main".into(), "0123456 first commit".into()));
        assert_eq!(parsed[1], ("wt/x".into(), "fedcba9 on a worktree".into()));
        assert_eq!(parsed[2].0, "(HEAD detached at abc)");
    }
}
