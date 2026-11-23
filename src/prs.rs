//! The open pull request of a workspace's branch (Bolt's `useTabPrs` +
//! `getPrForBranch` + `prStatus.ts`): [`fetch`] for the PR chip under each
//! row, [`fetch_detail`] for the current workspace's PR bar (`pr_bar.rs`),
//! which adds comments, reviews, review threads and the branch's last push.
//! `gh` is a network round trip, so the strip asks only for a branch with
//! an upstream, again when the branch moves, every 15 s while checks run,
//! and every 5 minutes otherwise (Bolt asks again on window focus instead).

use std::process::{Command, Output};

use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pr {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub state: String,
    #[serde(default)]
    pub is_draft: bool,
    #[serde(default)]
    pub review_decision: Option<String>,
    #[serde(default)]
    pub status_check_rollup: Option<Vec<Check>>,
    // The rest come with `fetch_detail` only.
    #[serde(default)]
    pub mergeable: String,
    #[serde(default)]
    pub merge_state_status: String,
    #[serde(default)]
    pub head_ref_name: String,
    #[serde(default)]
    pub comments: Vec<IssueComment>,
    #[serde(default)]
    pub reviews: Vec<Review>,
    /// Who opened the PR: their own comments are never new.
    #[serde(default)]
    pub author: Option<Author>,
}

/// A CheckRun (status + conclusion, name, detailsUrl) or a StatusContext
/// (state, context, targetUrl).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Check {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub conclusion: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub context: Option<String>,
    #[serde(default)]
    pub details_url: Option<String>,
    #[serde(default)]
    pub target_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Author {
    pub login: String,
}

/// gh's `comments`: the PR's conversation, issue comments only.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IssueComment {
    #[serde(default)]
    pub author: Option<Author>,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Review {
    #[serde(default)]
    pub author: Option<Author>,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub submitted_at: String,
}

/// A review thread flattened to its opening comment, with who said each of
/// its comments when (a reply after a push makes an old thread new again).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Thread {
    pub resolved: bool,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub line: Option<u64>,
    pub author: String,
    pub body: String,
    pub at: String,
    #[serde(default)]
    pub comments: Vec<ThreadComment>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ThreadComment {
    pub author: String,
    pub at: String,
}

/// The chip's color (`prBadge`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Badge {
    Failed,
    Running,
    Approved,
    Idle,
}

/// One check's state (`checkState`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckState {
    Running,
    Failed,
    Passed,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CheckCounts {
    pub running: usize,
    pub failed: usize,
    pub passed: usize,
}

const RUNNING: [&str; 5] = ["QUEUED", "IN_PROGRESS", "PENDING", "WAITING", "REQUESTED"];
const FAILED: [&str; 6] = ["FAILURE", "TIMED_OUT", "CANCELLED", "ACTION_REQUIRED", "STARTUP_FAILURE", "ERROR"];

impl Check {
    pub fn state(&self) -> CheckState {
        let status = self.status.as_deref().unwrap_or("").to_uppercase();
        let verdict = self.conclusion.as_deref().filter(|c| !c.is_empty()).or(self.state.as_deref()).unwrap_or("").to_uppercase();
        if RUNNING.contains(&status.as_str()) || RUNNING.contains(&verdict.as_str()) {
            CheckState::Running
        } else if FAILED.contains(&verdict.as_str()) {
            CheckState::Failed
        } else {
            CheckState::Passed
        }
    }

    pub fn name(&self) -> &str {
        self.name.as_deref().or(self.context.as_deref()).filter(|n| !n.is_empty()).unwrap_or("check")
    }

    pub fn url(&self) -> Option<&str> {
        self.details_url.as_deref().or(self.target_url.as_deref()).filter(|u| !u.is_empty())
    }
}

/// Only these reviews decide: a later COMMENTED one leaves an approval.
const DECIDING: [&str; 3] = ["APPROVED", "CHANGES_REQUESTED", "DISMISSED"];

impl Pr {
    pub fn checks(&self) -> &[Check] {
        self.status_check_rollup.as_deref().unwrap_or(&[])
    }

    pub fn check_counts(&self) -> CheckCounts {
        let mut counts = CheckCounts::default();
        for c in self.checks() {
            match c.state() {
                CheckState::Running => counts.running += 1,
                CheckState::Failed => counts.failed += 1,
                CheckState::Passed => counts.passed += 1,
            }
        }
        counts
    }

    pub fn running(&self) -> bool {
        self.check_counts().running > 0
    }

    /// A draft is idle whatever its checks say: it asks for nothing yet.
    pub fn badge(&self) -> Badge {
        let counts = self.check_counts();
        if self.is_draft {
            Badge::Idle
        } else if counts.failed > 0 {
            Badge::Failed
        } else if counts.running > 0 {
            Badge::Running
        } else if self.review_decision.as_deref() == Some("APPROVED") {
            Badge::Approved
        } else {
            Badge::Idle
        }
    }

    /// `reviewLabel`, capitalized for the bar; an approval names who gave
    /// it: `Approved - a, b`.
    pub fn review_label(&self) -> String {
        match self.review_decision.as_deref() {
            Some("APPROVED") => {
                let approvers: Vec<String> = self.reviewers().into_iter().filter(|(_, s)| s == "APPROVED").map(|(l, _)| l).collect();
                if approvers.is_empty() { "Approved".to_owned() } else { format!("Approved - {}", approvers.join(", ")) }
            }
            Some("CHANGES_REQUESTED") => "Changes requested".to_owned(),
            Some("REVIEW_REQUIRED") => "Review required".to_owned(),
            _ => "No reviews".to_owned(),
        }
    }

    /// Each reviewer's latest deciding review, oldest reviewer first.
    pub fn reviewers(&self) -> Vec<(String, String)> {
        let mut reviews: Vec<&Review> = self.reviews.iter().filter(|r| DECIDING.contains(&r.state.as_str())).collect();
        reviews.sort_by(|a, b| a.submitted_at.cmp(&b.submitted_at));
        let mut latest: Vec<(String, String)> = Vec::new();
        for r in reviews {
            let login = login_of(&r.author);
            match latest.iter_mut().find(|(l, _)| *l == login) {
                Some(entry) => entry.1 = r.state.clone(),
                None => latest.push((login, r.state.clone())),
            }
        }
        latest
    }

    pub fn can_merge(&self) -> bool {
        !self.is_draft && self.mergeable == "MERGEABLE"
    }

    /// Why the merge button is off (`mergeBlockedReason`), or empty.
    pub fn blocked_reason(&self) -> &'static str {
        if self.is_draft {
            "Pull request is a draft"
        } else if self.mergeable == "CONFLICTING" {
            "Branch has conflicts"
        } else if self.mergeable != "MERGEABLE" {
            "GitHub hasn't reported mergeability yet"
        } else {
            ""
        }
    }

    /// `mergeable` stays MERGEABLE when branch protection blocks the merge:
    /// only `mergeStateStatus` says so, and gh then refuses without
    /// `--admin`.
    pub fn needs_admin(&self) -> bool {
        self.merge_state_status == "BLOCKED"
    }
}

fn login_of(author: &Option<Author>) -> String {
    author.as_ref().map_or_else(|| "unknown".to_owned(), |a| a.login.clone())
}

/// The current workspace's PR with everything its bar and dialog show.
#[derive(Debug, Clone, PartialEq)]
pub struct Detail {
    pub pr: Pr,
    pub threads: Vec<Thread>,
    /// When the PR's branch was last pushed (GitHub's repository activity),
    /// none when GitHub would not say.
    pub pushed_at: Option<String>,
}

/// One entry of the dialog's comment list (`PrThread`): an issue comment,
/// a review with a body, or a review thread, which alone has a resolved
/// state.
#[derive(Debug, Clone, PartialEq)]
pub struct Comment {
    pub author: String,
    pub body: String,
    pub at: String,
    /// A review's state (`APPROVED`, `CHANGES_REQUESTED`, …), else empty.
    pub state: String,
    pub resolved: Option<bool>,
    pub path: Option<String>,
    pub line: Option<u64>,
    /// Said (or, for a thread, last replied to) after the last push by
    /// someone other than the PR's author.
    pub fresh: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommentCounts {
    /// None when the last push is unknown.
    pub new: Option<usize>,
    pub unresolved: usize,
    pub total: usize,
}

impl Detail {
    /// Newest first (`prComments`).
    pub fn comments(&self) -> Vec<Comment> {
        let pushed = self.pushed_at.as_deref().and_then(unix_ms_of);
        let pr_author = self.pr.author.as_ref().map(|a| a.login.as_str());
        let fresh = |who: &str, at: &str| pr_author != Some(who) && pushed.is_some_and(|p| unix_ms_of(at).is_some_and(|a| a > p));
        let pr = &self.pr;
        let mut all: Vec<Comment> = pr
            .comments
            .iter()
            .map(|c| Comment {
                author: login_of(&c.author),
                body: c.body.clone(),
                at: c.created_at.clone(),
                state: String::new(),
                resolved: None,
                path: None,
                line: None,
                fresh: fresh(&login_of(&c.author), &c.created_at),
            })
            .chain(pr.reviews.iter().filter(|r| !r.body.trim().is_empty()).map(|r| Comment {
                author: login_of(&r.author),
                body: r.body.clone(),
                at: r.submitted_at.clone(),
                state: r.state.clone(),
                resolved: None,
                path: None,
                line: None,
                fresh: fresh(&login_of(&r.author), &r.submitted_at),
            }))
            .chain(self.threads.iter().map(|t| Comment {
                author: t.author.clone(),
                body: t.body.clone(),
                at: t.at.clone(),
                state: String::new(),
                resolved: Some(t.resolved),
                path: Some(t.path.clone()).filter(|p| !p.is_empty()),
                line: t.line,
                fresh: t.comments.iter().any(|c| fresh(&c.author, &c.at)),
            }))
            .collect();
        all.sort_by(|a, b| b.at.cmp(&a.at));
        all
    }

    pub fn counts(&self) -> CommentCounts {
        let comments = self.comments();
        CommentCounts {
            new: self.pushed_at.as_ref().map(|_| comments.iter().filter(|c| c.fresh).count()),
            unresolved: self.threads.iter().filter(|t| !t.resolved).count(),
            total: comments.len(),
        }
    }
}

/// `2026-11-20T18:15:41Z` → Unix ms. GitHub's timestamps are UTC (`Z`);
/// anything after the seconds is ignored.
pub fn unix_ms_of(iso: &str) -> Option<u64> {
    let n = |r: std::ops::Range<usize>| iso.get(r)?.parse::<i64>().ok();
    let (y, m, d) = (n(0..4)?, n(5..7)?, n(8..10)?);
    let (hh, mm, ss) = (n(11..13)?, n(14..16)?, n(17..19)?);
    // Days from the civil date (Howard Hinnant's `days_from_civil`).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    u64::try_from((days * 86_400 + hh * 3_600 + mm * 60 + ss) * 1_000).ok()
}

/// `gh args…` in `dir`, its stdout when it succeeds; none without `gh`. A
/// `GITHUB_TOKEN` inherited from the launching shell overrides gh's keyring
/// login and fails, so it retries without one. Never for a merge: a retry
/// would merge twice.
fn gh(dir: &str, args: &[&str]) -> Option<Vec<u8>> {
    let run = |clear_token: bool| {
        let mut cmd = Command::new("gh");
        cmd.args(args).current_dir(dir);
        if clear_token {
            cmd.env_remove("GITHUB_TOKEN").env_remove("GH_TOKEN");
        }
        cmd.output().ok().filter(|o| o.status.success())
    };
    run(false).or_else(|| run(true)).map(|o| o.stdout)
}

const FIELDS: &str = "state,url,title,number,isDraft,reviewDecision,statusCheckRollup";
const DETAIL_FIELDS: &str = "state,url,title,number,isDraft,reviewDecision,statusCheckRollup,mergeable,mergeStateStatus,headRefName,comments,reviews,author";

/// The open PR of `dir`'s branch; none for a merged or closed one, or
/// without `gh`.
pub fn fetch(dir: &str) -> Option<Pr> {
    let out = gh(dir, &["pr", "view", "--json", FIELDS])?;
    serde_json::from_slice::<Pr>(&out).ok().filter(|pr| pr.state == "OPEN")
}

// Resolved lives on review threads, which `gh pr view --json` has no field
// for: only GraphQL has `isResolved`. gh fills `{owner}` and `{repo}` from
// the directory's remote.
const THREADS_Q: &str = "query($owner:String!,$repo:String!,$number:Int!){repository(owner:$owner,name:$repo){pullRequest(number:$number){reviewThreads(first:100){nodes{isResolved path line comments(first:1){nodes{author{login} body createdAt}} all:comments(last:100){nodes{author{login} createdAt}}}}}}}";
const THREADS_JQ: &str = r#"[.data.repository.pullRequest.reviewThreads.nodes[] | {resolved:.isResolved, path:(.path // ""), line:.line, author:(.comments.nodes[0].author.login // "unknown"), body:(.comments.nodes[0].body // ""), at:(.comments.nodes[0].createdAt // ""), comments:[.all.nodes[] | {author:(.author.login // "unknown"), at:.createdAt}]}]"#;
// A branch's pushes, force pushes and its creation are all pushes of it.
const PUSH_JQ: &str = r#"[.[] | select(.activity_type == "push" or .activity_type == "force_push" or .activity_type == "branch_creation")][0].timestamp // """#;

/// [`fetch`] plus the review threads and the branch's last push: three
/// round trips, so only for the current workspace.
pub fn fetch_detail(dir: &str) -> Option<Detail> {
    let out = gh(dir, &["pr", "view", "--json", DETAIL_FIELDS])?;
    let pr = serde_json::from_slice::<Pr>(&out).ok().filter(|pr| pr.state == "OPEN")?;
    let number = format!("number={}", pr.number);
    let query = format!("query={THREADS_Q}");
    let threads = gh(dir, &["api", "graphql", "-F", "owner={owner}", "-F", "repo={repo}", "-F", &number, "-f", &query, "-q", THREADS_JQ])
        .and_then(|o| serde_json::from_slice::<Vec<Thread>>(&o).ok())
        .unwrap_or_default();
    let reference = format!("ref=refs/heads/{}", pr.head_ref_name);
    let pushed_at = gh(dir, &["api", "-X", "GET", "repos/{owner}/{repo}/activity", "-f", &reference, "-f", "per_page=20", "-q", PUSH_JQ])
        .map(|o| String::from_utf8_lossy(&o).trim().to_owned())
        .filter(|s| !s.is_empty());
    Some(Detail { pr, threads, pushed_at })
}

/// `gh pr merge --squash --delete-branch` (`mergePr`). `--delete-branch`
/// checks out the base branch, fatal in a worktree, so a merge that landed
/// can still fail: a PR that reads MERGED afterwards counts as merged.
/// gh's stderr otherwise. Only a token gh could not use (the PR's repo not
/// found) is retried without it: nothing was merged then.
pub fn merge(dir: &str, admin: bool) -> Result<(), String> {
    let mut args = vec!["pr", "merge", "--squash", "--delete-branch"];
    if admin {
        args.push("--admin");
    }
    let run = |clear_token: bool| -> Result<Output, String> {
        let mut cmd = Command::new("gh");
        cmd.args(&args).current_dir(dir);
        if clear_token {
            cmd.env_remove("GITHUB_TOKEN").env_remove("GH_TOKEN");
        }
        cmd.output().map_err(|e| e.to_string())
    };
    let stderr = |o: &Output| String::from_utf8_lossy(&o.stderr).trim().to_owned();
    let mut out = run(false)?;
    if !out.status.success() && stderr(&out).contains("Could not resolve to a") {
        out = run(true)?;
    }
    if out.status.success() {
        return Ok(());
    }
    let state = gh(dir, &["pr", "view", "--json", "state", "-q", ".state"]).map(|o| String::from_utf8_lossy(&o).trim().to_owned());
    if state.as_deref() == Some("MERGED") {
        return Ok(());
    }
    Err(Some(stderr(&out)).filter(|s| !s.is_empty()).unwrap_or_else(|| "merge failed".to_owned()))
}

/// A failed merge's stderr (`mergeError`): whether gh asked for `--admin`,
/// and the reason to show, the `--admin` hint left to the button.
pub fn merge_error(stderr: &str) -> (bool, String) {
    let lines: Vec<&str> = stderr.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let admin = lines.iter().any(|l| l.contains("--admin"));
    let message = lines.iter().rev().find(|l| !l.contains("--admin")).map_or_else(String::new, |l| (*l).to_owned());
    (admin, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(json: &str) -> Pr {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn the_badge_reads_drafts_failures_runs_and_approvals() {
        let base = r#""number":1,"title":"t","url":"u","state":"OPEN""#;
        assert_eq!(pr(&format!("{{{base},\"isDraft\":true,\"statusCheckRollup\":[{{\"conclusion\":\"FAILURE\"}}]}}")).badge(), Badge::Idle);
        assert_eq!(pr(&format!("{{{base},\"statusCheckRollup\":[{{\"conclusion\":\"FAILURE\"}},{{\"status\":\"IN_PROGRESS\"}}]}}")).badge(), Badge::Failed);
        assert_eq!(pr(&format!("{{{base},\"statusCheckRollup\":[{{\"state\":\"PENDING\"}}]}}")).badge(), Badge::Running);
        assert_eq!(pr(&format!("{{{base},\"reviewDecision\":\"APPROVED\",\"statusCheckRollup\":null}}")).badge(), Badge::Approved);
    }

    #[test]
    fn new_counts_what_others_said_after_the_last_push_and_their_replies_revive_old_threads() {
        let pr = pr(r#"{"number":1,"title":"t","url":"u","state":"OPEN","author":{"login":"me"},
            "comments":[{"author":{"login":"a"},"body":"before","createdAt":"2026-11-20T10:00:00Z"},
                        {"author":{"login":"me"},"body":"mine","createdAt":"2026-11-20T11:45:00Z"},
                        {"author":{"login":"b"},"body":"after","createdAt":"2026-11-20T12:00:00Z"}],
            "reviews":[{"author":{"login":"c"},"body":"","state":"APPROVED","submittedAt":"2026-11-20T13:00:00Z"},
                       {"author":{"login":"c"},"body":"lgtm","state":"COMMENTED","submittedAt":"2026-11-20T09:00:00Z"}]}"#);
        let thread = |resolved: bool, said: &[(&str, &str)]| Thread {
            resolved,
            path: "a.rs".into(),
            line: Some(3),
            author: "d".into(),
            body: "fix".into(),
            at: "2026-11-20T08:00:00Z".into(),
            comments: said.iter().map(|(author, at)| ThreadComment { author: (*author).into(), at: (*at).into() }).collect(),
        };
        let detail = Detail {
            pr,
            threads: vec![
                thread(false, &[("d", "2026-11-20T08:00:00Z"), ("d", "2026-11-20T11:30:00Z")]),
                thread(true, &[("d", "2026-11-20T08:00:00Z")]),
                // The PR author answering a thread after the push leaves it old.
                thread(false, &[("d", "2026-11-20T08:00:00Z"), ("me", "2026-11-20T11:30:00Z")]),
            ],
            pushed_at: Some("2026-11-20T11:00:00Z".into()),
        };
        // The empty-bodied approval is no comment.
        assert_eq!(detail.counts(), CommentCounts { new: Some(2), unresolved: 2, total: 7 });
        assert_eq!(detail.comments()[0].body, "after");
        assert_eq!(Detail { pushed_at: None, ..detail }.counts().new, None);
    }

    #[test]
    fn a_later_comment_review_keeps_an_approval() {
        let pr = pr(r#"{"number":1,"title":"t","url":"u","state":"OPEN","reviews":[
            {"author":{"login":"a"},"state":"CHANGES_REQUESTED","submittedAt":"2026-11-20T09:00:00Z"},
            {"author":{"login":"a"},"state":"APPROVED","submittedAt":"2026-11-20T10:00:00Z"},
            {"author":{"login":"a"},"state":"COMMENTED","submittedAt":"2026-11-20T11:00:00Z"}]}"#);
        assert_eq!(pr.reviewers(), vec![("a".to_owned(), "APPROVED".to_owned())]);
    }

    #[test]
    fn an_approval_names_each_reviewer_still_approving() {
        let pr = pr(r#"{"number":1,"title":"t","url":"u","state":"OPEN","reviewDecision":"APPROVED","reviews":[
            {"author":{"login":"a"},"state":"APPROVED","submittedAt":"2026-11-20T09:00:00Z"},
            {"author":{"login":"b"},"state":"APPROVED","submittedAt":"2026-11-20T10:00:00Z"},
            {"author":{"login":"c"},"state":"APPROVED","submittedAt":"2026-11-20T10:30:00Z"},
            {"author":{"login":"c"},"state":"DISMISSED","submittedAt":"2026-11-20T11:00:00Z"}]}"#);
        assert_eq!(pr.review_label(), "Approved - a, b");
    }

    #[test]
    fn timestamps_and_merge_errors_parse() {
        assert_eq!(unix_ms_of("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(unix_ms_of("2026-11-20T18:15:41Z"), Some(1_795_198_541_000));
        assert_eq!(unix_ms_of("garbage"), None);
        let (admin, message) =
            merge_error("X Pull request is not mergeable: the base branch policy prohibits the merge.\nTo have the pull request merged after all the requirements have been met, add the `--auto` flag.\nTo use administrator privileges to immediately merge the pull request, add the `--admin` flag.\n");
        assert!(admin);
        assert!(message.starts_with("To have the pull request merged"));
    }
}
