//! Group memories (`groupSessionStore`'s prompt half, wired in `App`): the
//! memories live as files in the group's folder (`notes::memories_dir`),
//! and an agent session saves them because Rivet keeps the group's prompt
//! where each harness reads instructions from its worktree ([`Slot`]) in
//! every group home (the group session's instructions) and in every other
//! tab of a group (the member prompt), excluded from git. A group needs no
//! home for this: Rivet cannot start group sessions, but groups and
//! membership change here, so the prompts are rewritten when they do and
//! stale member prompts removed.

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
    hub::engine::agents::codex_home,
    notes::{md_files, memories_dir},
    state::Shell,
    workspace::{group_home, group_key},
};

/// Where a harness reads an addition to its instructions from the worktree
/// it starts in.
#[derive(Clone, Copy)]
enum Slot {
    /// omp appends `.omp/APPEND_SYSTEM.md` to its system prompt on every
    /// start, resume included.
    Omp,
    /// Claude Code loads `.claude/rules/*.md` at launch beside `CLAUDE.md`,
    /// and unlike a `CLAUDE.local.md` they leave an `AGENTS.md` read.
    Claude,
    /// Codex takes `developer_instructions` from a trusted project's
    /// `.codex/config.toml`.
    Codex,
}

impl Slot {
    const ALL: [Slot; 3] = [Slot::Omp, Slot::Claude, Slot::Codex];

    /// The file, from the worktree's root.
    fn file(self) -> &'static str {
        match self {
            Slot::Omp => ".omp/APPEND_SYSTEM.md",
            Slot::Claude => ".claude/rules/rivet-group.md",
            Slot::Codex => ".codex/config.toml",
        }
    }

    /// The file's content for `prompt`. Codex's opens with the prompt's
    /// first line as a comment, so [`Slot::head`] can tell whose it is, and
    /// keeps the user's own `developer_instructions` ahead of the prompt,
    /// since the project's value replaces theirs; an edit of theirs lands
    /// at the next rewrite.
    fn render(self, prompt: &str) -> String {
        match self {
            Slot::Omp | Slot::Claude => prompt.to_owned(),
            Slot::Codex => {
                let value = match codex_user_instructions() {
                    Some(own) => format!("{own}\n\n{prompt}"),
                    None => prompt.to_owned(),
                };
                // A JSON string is a TOML basic string.
                format!("# {}\ndeveloper_instructions = {}\n", prompt.lines().next().unwrap_or_default(), serde_json::Value::from(value))
            }
        }
    }

    /// How the prompt in a file of this slot starts.
    fn head(self, content: &str) -> &str {
        match self {
            Slot::Omp | Slot::Claude => content,
            Slot::Codex => content.strip_prefix("# ").unwrap_or_default(),
        }
    }
}

/// `developer_instructions` from the user's codex config.
fn codex_user_instructions() -> Option<String> {
    let text = std::fs::read_to_string(codex_home().join("config.toml")).ok()?;
    let config: toml::Table = text.parse().ok()?;
    config.get("developer_instructions")?.as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned)
}

/// A member's prompt opens with this line. Only a member prompt that carries
/// it is rewritten or deleted, so a worktree's own instructions are never
/// touched.
const MEMBER_MARK: &str = "<!-- Bolt group member prompt: rewritten by Bolt, deleted when this tab leaves the group -->";

/// How the group session's prompt opens, Rivet's and the Electron Bolt's:
/// what a home's prompt is known by.
const SESSION_LEAD: &str = "You are a group session for the Bolt workspace group";
const BOLT_SESSION_LEAD: &str = "You are the group session for the Bolt workspace group";

fn is_member_prompt(head: &str) -> bool {
    head.starts_with(MEMBER_MARK)
}

fn is_home_prompt(head: &str) -> bool {
    head.starts_with(SESSION_LEAD) || head.starts_with(BOLT_SESSION_LEAD)
}

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
fn session_prompt(group: &str, path: &str, branch: &str, repo: &str, dir: &str, memories: &[(String, String)], files: &str) -> String {
    let lead = [
        format!("{SESSION_LEAD} \"{group}\". Your worktree is {path}, on the group branch {branch} of the repo {repo}. Other sessions of this group may be running in the same worktree."),
        format!("The group's other workspace tabs are worktrees for slices of the same work. Membership changes, so read it fresh with the Bolt MCP tool getWorkspaceTabs: your group is the one whose tab with groupHome true has the path {path}, and every other tab in that group is a member. Each member lists its mainRepoPath, branch, ahead/behind, dirty state and terminals."),
        "A group can span repos, and its members are of two kinds, which you tell apart by mainRepoPath:".to_owned(),
        format!("Branch members have mainRepoPath {repo}, the same repo as you. You write the shared code on the group branch and keep their branches in sync with it: merge the group branch into branch members that fall behind, and bring a branch member's work into the group branch when the user asks, resolving conflicts yourself."),
        "Outside members have any other mainRepoPath. There is no shared branch with them: never merge, rebase or cherry-pick between repos. Coordinate them instead: follow their status through getWorkspaceTabs, edit files in an outside member's worktree path when the user asks, and talk to their terminals with sendToTerminals or a board. The contracts shared between the repos (API types, request and response shapes, error codes) are yours to keep consistent: a change on one side of one is not done until the other side matches.".to_owned(),
        "Shape the group with Bolt's tools: createWorktrees with that group adds tabs, setTabGroup moves tabs in or out, closeTabs closes tabs (their worktrees stay on disk).".to_owned(),
        format!("A member whose terminal is working or agent has another session in it: ask before committing, merging or checking out there, and before sending its terminal anything. Never push unless the user asks. Never commit {files}."),
    ]
    .join(" ");
    [lead, memories_section(dir, memories)].join("\n\n")
}

/// A member is not the group session: it gets the memories, and is told the
/// group branch and the syncing belong to the home, when the group has one
/// (`groupMemberPrompt`).
fn member_prompt(group: &str, path: &str, home: Option<&str>, dir: &str, memories: &[(String, String)], files: &str) -> String {
    let role = match home {
        Some(home) => format!("The group's session runs in its home worktree {home}; the group branch, merging between members and keeping them in sync are its job, not yours, unless the user asks."),
        None => "Its other workspace tabs are the other slices.".to_owned(),
    };
    [
        MEMBER_MARK.to_owned(),
        format!("You are working in a member of the Bolt workspace group \"{group}\": your worktree {path} is one slice of the group's work. {role} Never commit {files}."),
        memories_section(dir, memories),
    ]
    .join("\n\n")
}

/// Whether `slot` takes the group's prompt in worktree `path`: not when the
/// worktree tracks that file, or has one `ours` does not know for Rivet's.
fn check_slot(path: &str, slot: Slot, ours: fn(&str) -> bool) -> Result<(), String> {
    let file = slot.file();
    if !git::git(&["ls-files", "--", file], path)?.trim().is_empty() {
        return Err(format!("{path} tracks its own {file}; the group's would overwrite it"));
    }
    if std::fs::read_to_string(format!("{path}/{file}")).is_ok_and(|c| !ours(slot.head(&c))) {
        return Err(format!("{path} has its own {file}; not replacing it with the group's"));
    }
    Ok(())
}

/// The repo's `info/exclude`, which every worktree of it shares.
fn exclude_file(path: &str) -> Result<String, String> {
    let raw = git::git(&["rev-parse", "--git-path", "info/exclude"], path)?;
    let raw = raw.trim();
    Ok(if raw.starts_with('/') { raw.to_owned() } else { format!("{path}/{raw}") })
}

/// Writes the prompt into `slot`, excluded from git (`writeOmpPrompt`).
/// The pattern hides the file in every worktree of the repo, so it is
/// dropped again once no worktree holds a prompt of Rivet's there
/// ([`release_exclude`]).
fn write_prompt(path: &str, slot: Slot, prompt: &str) -> Result<(), String> {
    let exclude = exclude_file(path)?;
    let current = std::fs::read_to_string(&exclude).unwrap_or_default();
    let pattern = format!("/{}", slot.file());
    if !current.lines().any(|l| l == pattern) {
        if let Some(dir) = Path::new(&exclude).parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let sep = if !current.is_empty() && !current.ends_with('\n') { "\n" } else { "" };
        std::fs::write(&exclude, format!("{current}{sep}{pattern}\n")).map_err(|e| e.to_string())?;
    }
    let target = Path::new(path).join(slot.file());
    if let Some(dir) = target.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(target, slot.render(prompt)).map_err(|e| e.to_string())
}

/// The prompt `build` makes, given the files it lands in, in every slot
/// that takes it; each slot's failure stands alone.
fn write_prompts(path: &str, ours: fn(&str) -> bool, build: impl Fn(&str) -> String) -> Result<(), String> {
    let mut errors = Vec::new();
    let slots: Vec<Slot> = Slot::ALL.into_iter().filter(|s| check_slot(path, *s, ours).map_err(|e| errors.push(e)).is_ok()).collect();
    if !slots.is_empty() {
        let prompt = build(&slots.iter().map(|s| s.file()).collect::<Vec<_>>().join(", "));
        errors.extend(slots.into_iter().filter_map(|slot| write_prompt(path, slot, &prompt).err()));
    }
    if errors.is_empty() { Ok(()) } else { Err(errors.join("; ")) }
}

/// Drops `slot`'s exclude pattern once no worktree of `path`'s repo holds a
/// prompt of Rivet's there, so a file of the user's own by that name shows
/// in git again.
fn release_exclude(path: &str, slot: Slot) {
    let Ok(worktrees) = git::git(&["worktree", "list", "--porcelain"], path) else { return };
    let held = worktrees.lines().filter_map(|l| l.strip_prefix("worktree ")).any(|w| {
        std::fs::read_to_string(Path::new(w).join(slot.file())).is_ok_and(|c| {
            let head = slot.head(&c);
            is_home_prompt(head) || is_member_prompt(head)
        })
    });
    let Ok(exclude) = exclude_file(path) else { return };
    let Ok(current) = std::fs::read_to_string(&exclude) else { return };
    let pattern = format!("/{}", slot.file());
    if held || !current.lines().any(|l| l == pattern) {
        return;
    }
    let kept: String = current.lines().filter(|l| *l != pattern).map(|l| format!("{l}\n")).collect();
    let _ = std::fs::write(&exclude, kept);
}

/// Deletes each slot's prompt that `ours` knows, the directories that
/// leaves empty, and the exclude no worktree needs anymore
/// (`removeOmpPrompt`).
fn remove_prompts(path: &str, ours: fn(&str) -> bool) {
    for slot in Slot::ALL {
        let target = Path::new(path).join(slot.file());
        if !std::fs::read_to_string(&target).is_ok_and(|c| ours(slot.head(&c))) {
            continue;
        }
        let _ = std::fs::remove_file(&target);
        let mut dir = target.parent();
        while let Some(d) = dir.filter(|d| *d != Path::new(path)) {
            if std::fs::remove_dir(d).is_err() {
                break;
            }
            dir = d.parent();
        }
        release_exclude(path, slot);
    }
}

/// The home's instructions with a fresh memories snapshot (`refreshGroupHome`).
/// A member prompt there is the tab's from before it became the home.
fn refresh_home(group: &str, path: &str, dir: &Path) -> Result<(), String> {
    let (dir, memories) = read_memories(dir);
    let branch = git::current_branch(path)?;
    let repo = git::worktree_context(path)?.main_repo_path;
    write_prompts(path, |h| is_home_prompt(h) || is_member_prompt(h), |files| session_prompt(group, path, &branch, &repo, &dir, &memories, files))
}

fn write_member(m: &Member) -> Result<(), String> {
    let (dir, memories) = read_memories(&m.dir);
    write_prompts(&m.path, is_member_prompt, |files| member_prompt(&m.group, &m.path, m.home.as_deref(), &dir, &memories, files))
}

fn clear_member(path: &str) {
    remove_prompts(path, is_member_prompt);
}

/// Deletes the group session's prompt from the home of a removed group, so
/// an agent there no longer takes itself for one.
pub fn clear_home(path: &str) {
    remove_prompts(path, is_home_prompt);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_slot_knows_its_own_prompts_back() {
        let member = member_prompt("g", "/w", None, "/m", &[("a.md".into(), "say \"hi\"\n".into())], ".omp/APPEND_SYSTEM.md");
        let home = session_prompt("g", "/w", "b", "/r", "/m", &[], ".omp/APPEND_SYSTEM.md");
        for slot in Slot::ALL {
            assert!(is_member_prompt(slot.head(&slot.render(&member))), "{}", slot.file());
            assert!(is_home_prompt(slot.head(&slot.render(&home))), "{}", slot.file());
            assert!(!is_member_prompt(slot.head("model = \"o3\"\n")), "{}", slot.file());
        }
    }
}
