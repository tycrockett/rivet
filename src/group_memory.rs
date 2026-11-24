//! Group memories (`groupSessionStore`'s prompt half, wired in `App`): the
//! memories live as files in the group's folder (`notes::memories_dir`),
//! and an omp session saves them because Rivet keeps `.omp/APPEND_SYSTEM.md`
//! in every group home (the group session's instructions) and in every
//! other tab of a group (the member prompt), excluded from git. A group
//! needs no home for this: Rivet cannot start group sessions, but groups
//! and membership change here, so the prompts are rewritten when they do
//! and stale member prompts removed.

use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use nsh_core::prelude::{effect, spawn};

use crate::{
    git,
    notes::{md_files, memories_dir},
    state::Shell,
    workspace::{group_home, group_key},
};

/// omp appends this file from its cwd to its system prompt on every start,
/// resume included.
const OMP_PROMPT: &str = ".omp/APPEND_SYSTEM.md";

/// A member's prompt opens with this line. Only a member prompt that carries
/// it is rewritten or deleted, so a worktree's own `APPEND_SYSTEM.md` is
/// never touched.
const MEMBER_MARK: &str = "<!-- Bolt group member prompt: rewritten by Bolt, deleted when this tab leaves the group -->";

struct Member {
    group: String,
    /// The group's home, when it has one.
    home: Option<String>,
    path: String,
    dir: PathBuf,
}

/// One writer at a time, in the order the changes came (`memberSync`'s
/// chain): the generation of the last write that ran. A write older than it
/// leaves the members alone, so a tab that leaves a group and rejoins can't
/// have its delete land after the rewrite.
static WRITES: Mutex<u64> = Mutex::new(0);
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Each memory's name and body, newest edit first (`readMemories`).
fn read_memories(dir: &Path) -> (String, Vec<(String, String)>) {
    let memories = md_files(dir)
        .into_iter()
        .map(|(name, path, _)| (name, std::fs::read_to_string(path).unwrap_or_default()))
        .collect();
    (dir.to_string_lossy().into_owned(), memories)
}

/// Shared by the home's prompt and every member's (`groupMemoriesSection`).
/// The memories are a snapshot from the last time the prompt was written;
/// several sessions share the folder, so it is the authority.
fn memories_section(dir: &str, memories: &[(String, String)]) -> String {
    let snapshot = if memories.is_empty() {
        "It was empty when this prompt was written.".to_owned()
    } else {
        let list: Vec<String> = memories.iter().map(|(n, b)| format!("## {n}\n{}", b.trim())).collect();
        format!("A snapshot of it, which may be behind the folder:\n\n{}", list.join("\n\n"))
    };
    [
        "# Group memories".to_owned(),
        format!("{dir} is this group's memory. It outlives your session, every session of this group is given it, and it is deleted when the group is. Facts about this group go there, not in any global or per-repo memory."),
        "Save a memory when you learn something a later session of this group would need: a decision and why, a convention, which member owns what, the state of the sync, anything the user asks you to remember. Each memory is its own file in that folder: the file name is a short title the user sees on a tile in Bolt (a few words, ending in .md, no slashes), and the body is the memory, a few lines at most. Create the folder if it does not exist. Update a memory by editing its file, forget one by deleting it. Other sessions write there too: list the folder and read a file before relying on it, and never overwrite a file another session wrote without reading it first.".to_owned(),
        snapshot,
    ]
    .join("\n\n")
}

/// The group session's instructions (`groupSessionPrompt`). Names the home
/// path rather than leaning on the group name alone: a rename moves the
/// tab's group, and the path is how it finds itself. Membership is
/// described by rule, not listed, since it changes without this file being
/// rewritten.
fn session_prompt(group: &str, path: &str, branch: &str, repo: &str, dir: &str, memories: &[(String, String)]) -> String {
    let lead = [
        format!("You are a group session for the Bolt workspace group \"{group}\". Your worktree is {path}, on the group branch {branch} of the repo {repo}. Other sessions of this group may be running in the same worktree."),
        format!("The group's other workspace tabs are worktrees for slices of the same work. Membership changes, so read it fresh with the Bolt MCP tool getWorkspaceTabs: your group is the one whose tab with groupHome true has the path {path}, and every other tab in that group is a member. Each member lists its mainRepoPath, branch, ahead/behind, dirty state and terminals."),
        "A group can span repos, and its members are of two kinds, which you tell apart by mainRepoPath:".to_owned(),
        format!("Branch members have mainRepoPath {repo}, the same repo as you. You write the shared code on the group branch and keep their branches in sync with it: merge the group branch into branch members that fall behind, and bring a branch member's work into the group branch when the user asks, resolving conflicts yourself."),
        "Outside members have any other mainRepoPath. There is no shared branch with them: never merge, rebase or cherry-pick between repos. Coordinate them instead: follow their status through getWorkspaceTabs, edit files in an outside member's worktree path when the user asks, and talk to their terminals with sendToTerminals or a board. The contracts shared between the repos (API types, request and response shapes, error codes) are yours to keep consistent: a change on one side of one is not done until the other side matches.".to_owned(),
        "Shape the group with Bolt's tools: createWorktrees with that group adds tabs, setTabGroup moves tabs in or out, closeTabs closes tabs (their worktrees stay on disk).".to_owned(),
        format!("A member whose terminal is working or agent has another session in it: ask before committing, merging or checking out there, and before sending its terminal anything. Never push unless the user asks. Never commit {OMP_PROMPT}."),
    ]
    .join(" ");
    [lead, memories_section(dir, memories)].join("\n\n")
}

/// A member is not the group session: it gets the memories, and is told the
/// group branch and the syncing belong to the home, when the group has one
/// (`groupMemberPrompt`).
fn member_prompt(group: &str, path: &str, home: Option<&str>, dir: &str, memories: &[(String, String)]) -> String {
    let role = match home {
        Some(home) => format!("The group's session runs in its home worktree {home}; the group branch, merging between members and keeping them in sync are its job, not yours, unless the user asks."),
        None => "Its other workspace tabs are the other slices.".to_owned(),
    };
    [
        MEMBER_MARK.to_owned(),
        format!("You are working in a member of the Bolt workspace group \"{group}\": your worktree {path} is one slice of the group's work. {role} Never commit {OMP_PROMPT}."),
        memories_section(dir, memories),
    ]
    .join("\n\n")
}

/// Writes the prompt, excluded from git (`writeOmpPrompt`). Worktrees share
/// the common dir's exclude, which is fine: the pattern only hides the file
/// where Bolt wrote one.
fn write_omp_prompt(path: &str, prompt: &str) -> Result<(), String> {
    if !git::git(&["ls-files", "--", OMP_PROMPT], path)?.trim().is_empty() {
        return Err(format!("{path} tracks its own {OMP_PROMPT}; the group's would overwrite it"));
    }
    let raw = git::git(&["rev-parse", "--git-path", "info/exclude"], path)?;
    let raw = raw.trim();
    let exclude = if raw.starts_with('/') { raw.to_owned() } else { format!("{path}/{raw}") };
    let current = std::fs::read_to_string(&exclude).unwrap_or_default();
    let pattern = format!("/{OMP_PROMPT}");
    if !current.lines().any(|l| l == pattern) {
        if let Some(dir) = Path::new(&exclude).parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let sep = if !current.is_empty() && !current.ends_with('\n') { "\n" } else { "" };
        std::fs::write(&exclude, format!("{current}{sep}{pattern}\n")).map_err(|e| e.to_string())?;
    }
    std::fs::create_dir_all(format!("{path}/.omp")).map_err(|e| e.to_string())?;
    std::fs::write(format!("{path}/{OMP_PROMPT}"), prompt).map_err(|e| e.to_string())
}

/// Deletes the prompt, and `.omp` with it when that leaves it empty
/// (`removeOmpPrompt`).
fn remove_omp_prompt(path: &str) {
    let _ = std::fs::remove_file(format!("{path}/{OMP_PROMPT}"));
    let _ = std::fs::remove_dir(format!("{path}/.omp"));
}

/// The home's instructions with a fresh memories snapshot (`refreshGroupHome`).
fn refresh_home(group: &str, path: &str, dir: &Path) -> Result<(), String> {
    let (dir, memories) = read_memories(dir);
    let branch = git::current_branch(path)?;
    let repo = git::worktree_context(path)?.main_repo_path;
    write_omp_prompt(path, &session_prompt(group, path, &branch, &repo, &dir, &memories))
}

fn write_member(m: &Member) -> Result<(), String> {
    let file = format!("{}/{OMP_PROMPT}", m.path);
    if let Ok(current) = std::fs::read_to_string(&file) {
        if !current.starts_with(MEMBER_MARK) {
            return Err(format!("{} has its own {OMP_PROMPT}; not replacing it with the group's", m.path));
        }
    }
    let (dir, memories) = read_memories(&m.dir);
    write_omp_prompt(&m.path, &member_prompt(&m.group, &m.path, m.home.as_deref(), &dir, &memories))
}

fn clear_member(path: &str) {
    if std::fs::read_to_string(format!("{path}/{OMP_PROMPT}")).is_ok_and(|c| c.starts_with(MEMBER_MARK)) {
        remove_omp_prompt(path);
    }
}

/// Keeps every group home's and member's prompt current, as `App`'s two
/// effects do: keyed on group→home path, and on group/home/member path, so
/// it reruns on load, a new home, a rename and a membership change, not on
/// every settings write.
pub fn provide(shell: Shell) {
    let last = Rc::new(RefCell::new((None::<String>, None::<String>, Vec::<String>::new())));
    effect(move || {
        let s = shell.settings.get();
        let homes: Vec<(String, String, PathBuf)> = s
            .groups
            .iter()
            .filter_map(|g| group_home(&s, g).and_then(|t| Some((g.clone(), t.path.clone(), memories_dir(&s, g)?))))
            .collect();
        let members: Vec<Member> = s
            .tabs
            .iter()
            .filter(|t| !t.home)
            .filter_map(|t| {
                let group = group_key(t, &s.groups)?;
                let home = homes.iter().find(|(g, ..)| g == group).map(|h| h.1.clone());
                Some(Member { group: group.to_owned(), home, path: t.path.clone(), dir: memories_dir(&s, group)? })
            })
            .collect();
        let home_key = homes.iter().map(|(g, p, _)| format!("{g}\0{p}")).collect::<Vec<_>>().join("\n");
        let member_key = members.iter().map(|m| format!("{}\0{}\0{}", m.group, m.home.as_deref().unwrap_or_default(), m.path)).collect::<Vec<_>>().join("\n");
        let mut last = last.borrow_mut();
        let homes_changed = last.0.as_deref() != Some(&home_key);
        let members_changed = last.1.as_deref() != Some(&member_key);
        if !homes_changed && !members_changed {
            return;
        }
        // Paths that must not carry a member prompt: every other tab, and
        // every member that stopped being one.
        let member_paths: Vec<String> = members.iter().map(|m| m.path.clone()).collect();
        let mut others: Vec<String> = last.2.clone();
        others.extend(s.tabs.iter().chain(&s.pinned).map(|t| t.path.clone()));
        others.sort();
        others.dedup();
        others.retain(|p| !member_paths.contains(p));
        last.2 = member_paths;
        if homes_changed {
            last.0 = Some(home_key);
        }
        if members_changed {
            last.1 = Some(member_key);
        }
        let write_homes = homes_changed;
        let generation = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
        spawn(async move {
            git::off_thread(move || {
                let mut ran = WRITES.lock().unwrap_or_else(|e| e.into_inner());
                if write_homes {
                    for (group, path, dir) in &homes {
                        if let Err(e) = refresh_home(group, path, dir) {
                            eprintln!("rivet: group home {path}: instructions not written: {e}");
                        }
                    }
                }
                if generation < *ran {
                    return;
                }
                *ran = generation;
                for m in &members {
                    if let Err(e) = write_member(m) {
                        eprintln!("rivet: group member {}: memories prompt not written: {e}", m.path);
                    }
                }
                for p in &others {
                    clear_member(p);
                }
            })
            .await;
        });
    });
}
