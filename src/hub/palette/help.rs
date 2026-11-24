//! `helpGroups`' entries (`services/commands/helpMetadata.ts`), flattened in order:
//! the `/` menu's hints and `#run`'s verbs and flags.

/// A command's name, its description and its flags.
pub const HELP: &[(&str, &str, &[&str])] = &[
    ("new", "Create a branch, a worktree, or both", &["-b", "-w", "-bw", "--parent"]),
    ("go", "Switch to a branch", &["--default", "--here"]),
    ("list", "List branches, commits, or stashes", &["-b", "-c", "-s"]),
    ("delete", "Delete a branch or a stash", &["-b", "-s", "-r", "-v"]),
    ("commit", "Stage, commit, and push", &["--ai", "-p", "-f"]),
    ("amend", "Rewrite a commit message", &["--hash"]),
    ("commit pop", "Undo the last commit, a commit, or a file", &[]),
    ("push", "Push the current branch", &["-f"]),
    ("fetch", "Fetch and report remote status", &[]),
    ("merge", "Merge a branch into the current one", &[]),
    ("sync", "Pull the default branch and merge it in", &[]),
    ("stash", "Stash working tree changes", &[]),
    ("apply", "Apply the latest stash", &[]),
    ("pop", "Pop a stash by index", &[]),
    ("show", "Open a stash as a diff", &[]),
    ("reset", "Discard all uncommitted work", &["--hard"]),
    ("rename", "Rename the current branch and its worktree", &[]),
    ("prune", "Delete local branches with no remote", &[]),
    ("trace", "Show ancestor branches and their distance", &[]),
    ("open", "Open the branch's PR or compare page", &["--browser"]),
    ("status", "Show working tree status", &[]),
    ("cd", "Change directory", &[]),
    ("..", "Go to parent directory", &[]),
    ("ls", "List directory contents", &[]),
    ("pwd", "Print working directory", &[]),
    ("mkdir", "Create directory", &[]),
    ("bm", "Manage bookmarks", &[]),
    ("tab", "Manage workspace tabs", &[]),
    ("@", "Open file in editor", &[]),
    ("terminal", "Open interactive terminal", &[]),
    ("browser", "Open a browser tab, optionally at a URL", &["-o"]),
    ("board", "Open a shared notes board in the focused terminal's panel; bare lists boards", &[]),
    ("todo", "Manage todo items", &[]),
    ("notes", "Open project notes", &[]),
    ("ports", "View and kill ports", &[]),
    ("settings", "View and edit settings", &[]),
    ("repo", "Repo config: files symlinked into new worktrees, post-create commands", &[]),
    ("diff", "Review file changes", &["--commit <hash>", "--since <hash>"]),
    ("bolt", "Ask a new agent to drive Bolt: bolt [@handle] <request>", &[]),
    ("clear", "Clear results, keep the palette open", &[]),
    ("close", "Close the command palette", &[]),
    ("help", "Browse commands by category", &[]),
];

/// A command's description (`HELP`).
pub fn description(name: &str) -> &'static str {
    HELP.iter().find(|(n, ..)| *n == name).map_or("", |(_, d, _)| d)
}
