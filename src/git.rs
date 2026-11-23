//! Processes the commands run: git and the shell, blocking, so callers run
//! them on `nsh_core`'s blocking threads (`git::*` from a task through
//! [`off_thread`]). Failures carry Bolt's message: stderr, else
//! `Exited with code N`.

use std::{path::Path, process::Command, sync::Mutex};

use nsh_core::prelude::blocking;
use serde::{Deserialize, Serialize};

pub type Result<T, E = String> = std::result::Result<T, E>;

/// Runs `f` on a blocking thread and awaits it.
pub async fn off_thread<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
    blocking(f).await.expect("blocking task panicked")
}

fn output(mut cmd: Command, cwd: &str) -> Result<String> {
    if !Path::new(cwd).is_dir() {
        return Err(format!("Directory does not exist: {cwd}"));
    }
    let out = cmd.current_dir(cwd).output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        Err(if stderr.is_empty() {
            format!("Exited with code {}", out.status.code().unwrap_or(-1))
        } else {
            stderr
        })
    }
}

pub fn git<S: AsRef<str>>(args: &[S], cwd: &str) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.args(args.iter().map(AsRef::as_ref));
    output(cmd, cwd)
}

/// `cmd args…` through the shell, as Bolt's `cli:run` (`shell: true`) runs
/// `ls`, `mkdir` and `/custom` commands.
pub fn shell(cmd: &str, args: &[String], cwd: &str) -> Result<String> {
    let line = std::iter::once(cmd.to_owned()).chain(args.iter().cloned()).collect::<Vec<_>>().join(" ");
    let mut c = Command::new("/bin/sh");
    c.arg("-c").arg(line);
    output(c, cwd)
}

/// A tab's git state, as `BranchStatusIndicator` draws it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BranchStatus {
    pub branch: String,
    pub repo: Option<String>,
    pub has_remote: bool,
    pub dirty: bool,
    pub conflict: bool,
    /// A linked worktree, not the repo's main checkout.
    pub worktree: bool,
}

pub fn branch_status(cwd: &str) -> Option<BranchStatus> {
    let out = git(&["status", "--porcelain=v2", "--branch"], cwd).ok()?;
    let worktree = worktree_context(cwd).is_ok_and(|c| c.is_worktree);
    let mut status = BranchStatus { repo: repo_name(cwd), worktree, ..Default::default() };
    for line in out.lines() {
        if let Some(head) = line.strip_prefix("# branch.head ") {
            status.branch = head.to_owned();
        } else if line.starts_with("# branch.upstream ") {
            status.has_remote = true;
        } else if line.starts_with("u ") {
            status.conflict = true;
            status.dirty = true;
        } else if !line.starts_with('#') && !line.is_empty() {
            status.dirty = true;
        }
    }
    Some(status)
}

/// The repo a directory belongs to, a worktree's included: the folder
/// holding `--git-common-dir` (`getRepoName`).
pub fn repo_name(cwd: &str) -> Option<String> {
    let common = git(&["rev-parse", "--path-format=absolute", "--git-common-dir"], cwd).ok()?;
    let common = Path::new(common.trim());
    common.parent()?.file_name().map(|n| n.to_string_lossy().into_owned())
}

pub fn current_branch(cwd: &str) -> Result<String> {
    git(&["rev-parse", "--abbrev-ref", "HEAD"], cwd).map(|s| s.trim().to_owned())
}

pub fn branches(cwd: &str) -> Result<Vec<String>> {
    Ok(git(&["branch", "--format=%(refname:short)"], cwd)?
        .lines()
        .map(str::to_owned)
        .filter(|l| !l.is_empty())
        .collect())
}

/// `origin/HEAD`'s branch, else `main` or `master` if one exists
/// (`resolveDefaultBranch`'s local strategies).
pub fn default_branch(cwd: &str) -> String {
    if let Ok(s) = git(&["symbolic-ref", "--quiet", "--short", "refs/remotes/origin/HEAD"], cwd) {
        if let Some(b) = s.trim().strip_prefix("origin/") {
            return b.to_owned();
        }
    }
    for b in ["main", "master"] {
        if git(&["branch", "--list", b], cwd).is_ok_and(|s| !s.trim().is_empty()) {
            return b.to_owned();
        }
    }
    "main".into()
}

pub struct WorktreeContext {
    pub is_worktree: bool,
    pub main_repo_path: String,
}

pub fn worktree_context(cwd: &str) -> Result<WorktreeContext> {
    let dir = git(&["rev-parse", "--path-format=absolute", "--git-dir"], cwd)?;
    let common = git(&["rev-parse", "--path-format=absolute", "--git-common-dir"], cwd)?;
    let (dir, common) = (dir.trim().trim_end_matches('/'), common.trim().trim_end_matches('/'));
    Ok(WorktreeContext {
        is_worktree: dir != common,
        main_repo_path: common.trim_end_matches("/.git").to_owned(),
    })
}

/// Every worktree's `(path, branch)`, from `git worktree list --porcelain`.
pub fn worktrees(cwd: &str) -> Result<Vec<(String, Option<String>)>> {
    let mut out = Vec::new();
    let mut path: Option<String> = None;
    for line in git(&["worktree", "list", "--porcelain"], cwd)?.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            if let Some(prev) = path.replace(p.to_owned()) {
                out.push((prev, None));
            }
        } else if let Some(b) = line.strip_prefix("branch ") {
            if let Some(p) = path.take() {
                out.push((p, Some(b.trim_start_matches("refs/heads/").to_owned())));
            }
        }
    }
    out.extend(path.map(|p| (p, None)));
    Ok(out)
}

/// Where `go <branch>` lands (`checkoutBranch`): the main repo for the
/// default branch from a worktree, the branch's own worktree, else a
/// checkout here (`None`). `here` checks it out here regardless, detaching
/// the worktree that holds it.
pub fn checkout_branch(cwd: &str, branch: &str, here: bool) -> Result<Option<String>> {
    if here {
        let holder = worktrees(cwd)?.into_iter().find(|(path, b)| path != cwd && b.as_deref() == Some(branch));
        if let Some((path, _)) = holder {
            git(&["-C", &path, "switch", "--detach"], cwd)?;
        }
        git(&["checkout", branch], cwd)?;
        return Ok(None);
    }
    let ctx = worktree_context(cwd)?;
    if ctx.is_worktree && branch == default_branch(cwd) {
        return Ok(Some(ctx.main_repo_path));
    }
    let held = worktrees(cwd)?
        .into_iter()
        .find(|(path, b)| *path != ctx.main_repo_path && b.as_deref() == Some(branch));
    if let Some((path, _)) = held {
        return Ok(Some(path));
    }
    git(&["checkout", branch], cwd)?;
    Ok(None)
}

/// `git` with extra environment variables (the `GIT_EDITOR=…` of
/// `amend --hash`).
pub fn git_env<S: AsRef<str>>(args: &[S], cwd: &str, env: &[(&str, &str)]) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.args(args.iter().map(AsRef::as_ref)).envs(env.iter().copied());
    output(cmd, cwd)
}

/// The checked-out branch, empty when detached (`getCurrentBranch`:
/// `git branch --show-current`).
pub fn show_current_branch(cwd: &str) -> Result<String> {
    git(&["branch", "--show-current"], cwd).map(|s| s.trim().to_owned())
}

/// Whether `origin` has the branch (`hasRemoteBranch`).
pub fn has_remote_branch(branch: &str, cwd: &str) -> bool {
    git(&["show-branch", &format!("remotes/origin/{branch}")], cwd).is_ok()
}

/// The path of the branch's own worktree, the main repo excluded
/// (`getWorktreeForBranch`).
pub fn worktree_for_branch(cwd: &str, branch: &str) -> Result<Option<String>> {
    let main = worktree_context(cwd)?.main_repo_path;
    Ok(worktrees(cwd)?
        .into_iter()
        .find(|(path, b)| *path != main && b.as_deref() == Some(branch))
        .map(|(path, _)| path))
}

/// Where a branch is checked out, the main repo included
/// (`findBranchLocation`).
pub fn branch_location(cwd: &str, branch: &str) -> Result<Option<String>> {
    Ok(worktrees(cwd)?.into_iter().find(|(_, b)| b.as_deref() == Some(branch)).map(|(path, _)| path))
}

/// Removes a worktree (pruning when the remove fails) and deletes its
/// branch, from the main repo (`deleteWorktreeAndBranch`).
pub fn delete_worktree_and_branch(main_repo: &str, worktree: &str, branch: &str) -> Result<()> {
    if git(&["worktree", "remove", "--force", worktree], main_repo).is_err() {
        git(&["worktree", "prune"], main_repo)?;
    }
    git(&["branch", "-D", branch], main_repo).map(|_| ())
}

/// Local branches `origin` has no branch of (`getBranches`' `hasRemote`
/// false, which `prune` deletes).
pub fn branches_without_remote(cwd: &str) -> Result<Vec<String>> {
    let remotes: std::collections::HashSet<String> = git(&["branch", "-r", "--no-abbrev"], cwd)
        .unwrap_or_default()
        .lines()
        .map(|l| l.trim().to_owned())
        .filter(|l| !l.is_empty() && !l.contains("->"))
        .collect();
    Ok(branches(cwd)?
        .into_iter()
        .filter(|b| !b.starts_with('(') && !remotes.contains(&format!("origin/{b}")))
        .collect())
}

/// Commits with `message` through a file in the git dir, so no shell ever
/// sees it (`commitWithMessage`).
pub fn commit_with_message(cwd: &str, message: &str, extra: &[&str]) -> Result<String> {
    let git_dir = git(&["rev-parse", "--absolute-git-dir"], cwd)?.trim().to_owned();
    let path = format!("{git_dir}/BOLT_COMMITMSG");
    std::fs::write(&path, message).map_err(|e| e.to_string())?;
    let mut args = vec!["commit"];
    args.extend_from_slice(extra);
    args.extend(["-F", &path]);
    let out = git(&args, cwd);
    let _ = std::fs::remove_file(&path);
    out
}

/// `'…'` for a shell (`shellQuote`).
pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// `locStats`: `(added, deleted)` since the branch's base, and not yet
/// committed.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Loc {
    pub committed: (u32, u32),
    pub uncommitted: (u32, u32),
    /// The branch `committed` counts from; none on the default branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
}

impl Loc {
    pub fn is_empty(&self) -> bool {
        self.committed == (0, 0) && self.uncommitted == (0, 0)
    }

    /// What a count leads with: the committed lines when there are any,
    /// else the uncommitted; and whether the uncommitted follow it in
    /// parentheses (both have lines).
    pub fn lead(&self) -> ((u32, u32), bool) {
        match self.committed == (0, 0) {
            true => (self.uncommitted, false),
            false => (self.committed, self.uncommitted != (0, 0)),
        }
    }
}

/// `useGitStatus`' `locStats`: lines added and removed since the branch's
/// base (nothing on the default branch itself) and in the working tree,
/// where untracked files count each of their lines as added. Whatever git
/// can't say counts as zero.
pub fn loc_stats(dir: &str) -> Loc {
    let counts = |args: &[&str]| git(args, dir).map_or((0, 0), |stat| shortstat_counts(&stat));
    let default = default_branch(dir);
    let base = match current_branch(dir) {
        Ok(current) if current != default => Some(base_branch(dir, &current, &default)),
        _ => None,
    };
    let committed = base.as_ref().map_or((0, 0), |base| counts(&["diff", "--shortstat", &format!("{base}...HEAD")]));
    let (tracked_adds, dels) = counts(&["diff", "--shortstat", "HEAD"]);
    Loc { committed, uncommitted: (tracked_adds + untracked_lines(dir), dels), base }
}

/// [`find_base_branch`], kept while the directory, branch and HEAD stay
/// (`baseCacheRef`): it asks git twice per local branch.
fn base_branch(dir: &str, current: &str, default: &str) -> String {
    static LAST: Mutex<Option<(String, String)>> = Mutex::new(None);
    let head = git(&["rev-parse", "HEAD"], dir).unwrap_or_default();
    let key = format!("{dir}\0{current}\0{}", head.trim());
    if let Some((_, base)) = LAST.lock().unwrap().as_ref().filter(|(k, _)| *k == key) {
        return base.clone();
    }
    let base = find_base_branch(dir, default);
    *LAST.lock().unwrap() = Some((key, base.clone()));
    base
}

/// The local branch nearest above HEAD (fewest commits since their
/// merge-base), the default one winning ties; a branch at or ahead of HEAD
/// can't be its parent. The default branch when it isn't among them, since
/// it then already contains HEAD (`findBaseBranch`).
fn find_base_branch(dir: &str, default: &str) -> String {
    let Ok(head) = git(&["rev-parse", "HEAD"], dir) else { return default.to_owned() };
    let head = head.trim();
    let Ok(refs) = git(&["for-each-ref", "--format=%(refname:short)", "refs/heads"], dir) else {
        return default.to_owned();
    };
    let (mut best, mut best_count, mut saw_default) = (default, u32::MAX, false);
    for name in refs.lines().map(str::trim).filter(|n| !n.is_empty()) {
        let Ok(base) = git(&["merge-base", name, "HEAD"], dir) else { continue };
        let base = base.trim();
        if base == head {
            continue;
        }
        let Ok(count) = git(&["rev-list", "--count", &format!("{base}..HEAD")], dir) else { continue };
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
    let Ok(files) = git(&["ls-files", "-z", "--others", "--exclude-standard"], dir) else { return 0 };
    files
        .split('\0')
        .filter(|f| !f.is_empty())
        .filter_map(|f| std::fs::read(Path::new(dir).join(f)).ok())
        .map(|bytes| bytes.iter().filter(|b| **b == b'\n').count() as u32)
        .sum()
}

/// `(insertions, deletions)` of a `--shortstat` line; zero for what it
/// doesn't name (`parseShortstat`).
pub fn shortstat_counts(stat: &str) -> (u32, u32) {
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

/// What a repo's `repo-config.json` entry asks of a new worktree.
#[derive(Default)]
struct RepoConfig {
    worktree_include: String,
    post_setup: Vec<String>,
}

/// The repo's entry in `repo-config.json`: this app's data directory first,
/// then Bolt's own (`readRepoConfig`).
fn repo_config(main_repo: &str) -> RepoConfig {
    for dir in [crate::settings::data_dir(), crate::settings::bolt_data_dir()] {
        let Ok(text) = std::fs::read_to_string(dir.join("repo-config.json")) else { continue };
        let Ok(all) = serde_json::from_str::<serde_json::Value>(&text) else { continue };
        let Some(entry) = all.get(main_repo).filter(|e| e.is_object()) else { continue };
        return RepoConfig {
            worktree_include: entry.get("worktreeInclude").and_then(|v| v.as_str()).unwrap_or_default().to_owned(),
            post_setup: entry
                .get("postSetup")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|c| c.as_str().map(str::to_owned)).collect())
                .unwrap_or_default(),
        };
    }
    RepoConfig::default()
}

/// What `ls-files --directory` output is worth a symlink: a directory only
/// when nothing below it was listed, `node_modules` and friends never
/// (`linkTargets`).
pub fn link_targets(ls_output: &str) -> Vec<String> {
    const SKIP_TOP: [&str; 4] = ["node_modules", ".next", ".dart_tool", "build"];
    let entries: Vec<&str> = ls_output.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    entries
        .iter()
        .filter(|e| !e.ends_with('/') || !entries.iter().any(|o| o != *e && o.starts_with(**e)))
        .map(|e| e.strip_suffix('/').unwrap_or(e))
        .filter(|e| !SKIP_TOP.contains(&e.split('/').next().unwrap_or("")))
        .map(str::to_owned)
        .collect()
}

/// Symlinks the repo's `worktreeInclude` matches from the main checkout into
/// a new worktree, then runs its `postSetup` commands; returns what failed
/// (`setupWorktree`).
pub fn setup_worktree(main_repo: &str, worktree: &str) -> Result<Vec<String>> {
    let config = repo_config(main_repo);
    let patterns: Vec<&str> = config
        .worktree_include
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    let mut failed = Vec::new();

    if !patterns.is_empty() {
        let mut args = vec!["ls-files".to_owned(), "--others".into(), "--ignored".into(), "--directory".into()];
        args.extend(patterns.iter().map(|p| format!("--exclude={p}")));
        for rel in link_targets(&git(&args, main_repo)?) {
            let (src, dst) = (format!("{main_repo}/{rel}"), format!("{worktree}/{rel}"));
            if Path::new(&dst).exists() || std::fs::symlink_metadata(&dst).is_ok() {
                continue;
            }
            let linked = Path::new(&dst)
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| std::os::unix::fs::symlink(&src, &dst));
            if linked.is_err() {
                failed.push(format!("link {rel}"));
            }
        }
    }

    for cmd in &config.post_setup {
        let line = format!("export MAIN={} HERE={}; {cmd}", shell_quote(main_repo), shell_quote(worktree));
        if shell(&line, &[], worktree).is_err() {
            failed.push(cmd.clone());
        }
    }
    Ok(failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_leaves_not_their_parents_and_skips_build_dirs() {
        let ls = ".env\napps/\napps/web/.env\nnode_modules/\nbuild/out\nconfig/\n";
        assert_eq!(link_targets(ls), [".env", "apps/web/.env", "config"]);
    }

    #[test]
    fn quotes_single_quotes() {
        assert_eq!(shell_quote("a'b"), r"'a'\''b'");
    }
}
