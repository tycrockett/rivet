//! What a result card draws beyond its text (Bolt's `CommandResult` types
//! other than a plain `message`, as `SwitchResults` renders them). The
//! text stays the whole answer for anything that reads results as text (a
//! hop, MCP); this only shapes the card.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Rich {
    /// `type: "git-message"`: the text after an icon (`gitMessageIconMap`'s
    /// key: `fetch`, `fetching`, `pull`, `pulling`, `push`, `pushing`,
    /// `checkout`, `merge`, `merging`, `undo`, `add`, `commit`,
    /// `upstream`, `terminal`, `stash`, `warning`), `detail` faint at the
    /// right.
    Git {
        icon: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    /// `type: "direct"`: the text as preformatted monospace.
    Direct,
    /// `type: "conflict"`: the text as its title, the conflicted files
    /// (relative to `directory`) to open, and what to do next.
    Conflict {
        files: Vec<String>,
        directory: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hint: Option<String>,
    },
    /// `key: "gitStatus"` (`GitStatus`).
    Status(GitStatus),
    /// `key: "branches"` (`Branches`).
    Branches(BranchList),
    /// `key: "commitLogs"` (`CommitLogs`): newest first.
    CommitLogs { commits: Vec<Commit> },
    /// `key: "bookmarkList"` (`BookmarkList`): `(key, path)`.
    Bookmarks { items: Vec<(String, String)> },
    /// `key: "notesList"` (`NotesList`): `(name, file)`; a click opens one,
    /// its × deletes it.
    Notes { items: Vec<(String, String)> },
}

/// The working tree's changes, paths relative to the repo's directory.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitStatus {
    pub modified: Vec<String>,
    pub added: Vec<String>,
    pub deleted: Vec<String>,
    pub renamed: Vec<String>,
    pub copied: Vec<String>,
    pub untracked: Vec<String>,
    /// Ahead/behind its upstream, as Bolt words it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_stats: Option<String>,
    /// Commits not on the upstream.
    #[serde(default)]
    pub unpushed: Vec<Commit>,
    /// Lines added and removed (`locStats`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loc: Option<crate::git::Loc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Commit {
    pub hash: String,
    pub subject: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchList {
    pub current: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
    pub list: Vec<Branch>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Branch {
    pub name: String,
    pub has_remote: bool,
    pub is_worktree: bool,
    /// Its last commit's subject (and age, as Bolt shows it).
    #[serde(default)]
    pub last_commit: String,
}
