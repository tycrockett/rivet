//! Bolt's command line (`src/services/commands/`): the same parser, the
//! same registry in the same order, and the executor's order — `git …`
//! passes through, then `/custom` commands from settings, then the
//! registry; anything else is `command not found`.
//!
//! Commands whose effect lives outside this shell (notes' snippets,
//! boards, settings pages) are listed but not runnable here:
//! [`Command::ported`] is false and running one says so.

mod branch;
mod builtin;
mod git_verbs;
pub mod rich;

use std::cell::RefCell;

use nsh_core::prelude::*;

use crate::{git, settings::Settings, state::Shell};

/// One thing a command printed (Bolt's `CommandResult`): what the
/// palette's result card shows for the run. `text` is all of it; `rich`,
/// when set, is how the card draws it.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Output {
    pub key: String,
    pub error: bool,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rich: Option<rich::Rich>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Git,
    BuiltIn,
    Terminal,
}

pub struct Command {
    pub name: &'static str,
    pub kind: Kind,
    pub ported: bool,
}

const fn cmd(name: &'static str, kind: Kind, ported: bool) -> Command {
    Command { name, kind, ported }
}

/// `createCommandRegistry`'s entries in declaration order, which is the
/// completion tiebreak: the git verbs lead each prefix.
pub const REGISTRY: &[Command] = &[
    cmd("commit", Kind::Git, true),
    cmd("new", Kind::Git, true),
    cmd("go", Kind::Git, true),
    cmd("list", Kind::Git, true),
    cmd("delete", Kind::Git, true),
    cmd("push", Kind::Git, true),
    cmd("pop", Kind::Git, true),
    cmd("prune", Kind::Git, true),
    cmd("merge", Kind::Git, true),
    cmd("fetch", Kind::Git, true),
    cmd("apply", Kind::Git, true),
    cmd("amend", Kind::Git, true),
    cmd("rename", Kind::Git, true),
    cmd("reset", Kind::Git, true),
    cmd("open", Kind::Git, true),
    cmd("status", Kind::Git, true),
    cmd("show", Kind::Git, true),
    cmd("stash", Kind::Git, true),
    cmd("sync", Kind::Git, true),
    cmd("diff", Kind::BuiltIn, true),
    cmd("settings", Kind::BuiltIn, true),
    cmd("repo", Kind::BuiltIn, false),
    cmd("clear", Kind::BuiltIn, true),
    cmd("close", Kind::BuiltIn, true),
    cmd("exit", Kind::BuiltIn, true),
    cmd("bm", Kind::BuiltIn, true),
    cmd("tab", Kind::BuiltIn, true),
    cmd("ports", Kind::BuiltIn, false),
    cmd("help", Kind::BuiltIn, true),
    cmd("todo", Kind::BuiltIn, false),
    cmd("terminal", Kind::BuiltIn, true),
    cmd("browser", Kind::BuiltIn, true),
    cmd("trace", Kind::Git, true),
    cmd("edit", Kind::BuiltIn, false),
    cmd("notes", Kind::BuiltIn, true),
    cmd("snippets", Kind::BuiltIn, false),
    cmd("bolt", Kind::BuiltIn, false),
    cmd("board", Kind::BuiltIn, false),
    cmd("pwd", Kind::Terminal, true),
    cmd("mkdir", Kind::Terminal, true),
    cmd("cd", Kind::Terminal, true),
    cmd("..", Kind::Terminal, true),
    cmd("ls", Kind::Terminal, true),
];

/// A command that navigates: the palette keeps open for it, runs it only
/// in the finder, and neither `/` nor `/help` lists it (`NAV`).
pub fn is_nav(cmd: &str) -> bool {
    matches!(cmd.split_whitespace().next().unwrap_or("").to_lowercase().as_str(), "cd" | ".." | "mkdir")
}

/// Commands that show nothing in the palette, so it closes once they ran
/// (`NON_RESULT_COMMANDS`).
pub fn is_non_result(input: &str) -> bool {
    const NON_RESULT: &[&str] = &[
        "cd", "..", "terminal", "close", "edit", "notes", "snippets", "ai", "open", "browser", "diff",
        "bolt", "repo", "mkdir", "clear",
    ];
    let trimmed = input.trim();
    let first = trimmed.split_whitespace().next().unwrap_or("").to_lowercase();
    // `notes --list` shows its card (the palette would close on it in Bolt
    // too, but there the result shows outside the palette); `settings`
    // alone opens its tab.
    if first == "notes" && trimmed.contains("--list") {
        return false;
    }
    NON_RESULT.contains(&first.as_str())
        || first.starts_with(':')
        || trimmed == "bm"
        || trimmed == "settings"
        || trimmed.starts_with("bm go ")
        || trimmed.starts_with("board ")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub command: String,
    pub args: Vec<String>,
}

/// Splits on spaces outside quotes, `\"` `\'` `\\` escaping, and expands
/// combined short flags (`-dr` → `-d -r`, but not `-l=15`) (`parseCommand`).
pub fn parse(input: &str) -> Parsed {
    let mut parts: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if matches!(chars.peek(), Some('"' | '\'' | '\\')) => current.push(chars.next().unwrap()),
            '"' | '\'' => match quote {
                None => quote = Some(c),
                Some(q) if q == c => quote = None,
                Some(_) => current.push(c),
            },
            ' ' if quote.is_none() => {
                if !current.trim().is_empty() {
                    parts.push(current.trim().to_owned());
                }
                current.clear();
            }
            _ => current.push(c),
        }
    }
    if !current.trim().is_empty() {
        parts.push(current.trim().to_owned());
    }
    let mut parts = parts.into_iter();
    let command = parts.next().unwrap_or_default();
    let args = parts
        .flat_map(|arg| {
            if arg.starts_with('-') && !arg.starts_with("--") && arg.chars().count() > 2 && !arg.contains('=') {
                arg.chars().skip(1).map(|c| format!("-{c}")).collect()
            } else {
                vec![arg]
            }
        })
        .collect();
    Parsed { command, args }
}

/// What a handler runs with: the submit-time directory and settings, and
/// the shell to change (`FullCommandContext`).
pub struct Ctx {
    pub shell: Shell,
    pub command: String,
    pub args: Vec<String>,
    pub directory: String,
    pub settings: Settings,
    run: u64,
    /// Run in another directory for a script (`runPaletteIn`): nothing
    /// moves the window.
    offscreen: bool,
    out: RefCell<Vec<Output>>,
}

impl Ctx {
    /// Moves the shell to `path`, unless a newer command started meanwhile
    /// or the run is off-screen.
    pub fn set_directory(&self, path: String) {
        if !self.offscreen && self.shell.run_id.get_untracked() == self.run {
            self.shell.set_directory(path);
        }
    }

    /// Adds a result, replacing the run's earlier one of the same key.
    pub fn add(&self, key: &str, text: impl Into<String>) {
        self.push(Output { key: key.into(), error: false, text: text.into(), rich: None });
    }

    pub fn add_error(&self, key: &str, text: impl Into<String>) {
        self.push(Output { key: key.into(), error: true, text: text.into(), rich: None });
    }

    /// A result its card draws as `rich` (`text` still says all of it).
    pub fn add_rich(&self, key: &str, text: impl Into<String>, rich: rich::Rich) {
        self.push(Output { key: key.into(), error: false, text: text.into(), rich: Some(rich) });
    }

    /// Bolt's `git-message`: `text` after the icon `icon`
    /// ([`rich::Rich::Git`]).
    pub fn add_git(&self, key: &str, icon: &str, text: impl Into<String>) {
        self.add_rich(key, text, rich::Rich::Git { icon: icon.into(), detail: None });
    }

    fn push(&self, out: Output) {
        let mut all = self.out.borrow_mut();
        all.retain(|o| o.key != out.key);
        all.push(out);
    }

    pub fn flag(&self, names: &[&str]) -> bool {
        self.args.iter().any(|a| names.contains(&a.as_str()))
    }

    /// The value after `name` (`getFlag`).
    pub fn flag_value(&self, name: &str) -> Option<&str> {
        let at = self.args.iter().position(|a| a == name)?;
        self.args.get(at + 1).map(String::as_str)
    }

    /// The first argument that is not a flag (`branchArg`).
    pub fn positional(&self) -> Option<&str> {
        self.args.iter().map(String::as_str).find(|a| !a.starts_with('-'))
    }

    /// Runs git in the submit-time directory, off the engine thread.
    pub async fn git(&self, args: Vec<String>) -> git::Result<String> {
        let dir = self.directory.clone();
        git::off_thread(move || git::git(&args, &dir)).await
    }
}

/// Runs one line as Bolt's `submit` does and returns what it printed, a
/// failure as an error result; then refreshes the git state. `at` runs it
/// off-screen in that directory instead of the current one, for scripts
/// (`runPaletteIn`): the window stays where it is.
pub async fn run(shell: Shell, input: String, at: Option<String>) -> Vec<Output> {
    let parsed = parse(&input);
    if parsed.command.is_empty() {
        return Vec::new();
    }
    let run = if at.is_none() {
        let run = shell.run_id.get_untracked() + 1;
        shell.run_id.set(run);
        run
    } else {
        shell.run_id.get_untracked()
    };
    let offscreen = at.is_some();
    let ctx = Ctx {
        shell,
        command: parsed.command,
        args: parsed.args,
        directory: at.unwrap_or_else(|| shell.directory()),
        settings: shell.settings.get_untracked(),
        run,
        offscreen,
        out: RefCell::new(Vec::new()),
    };
    if let Err(message) = execute(&ctx).await {
        ctx.add_error(&format!("error-{}", ctx.run), message);
    }
    shell.refresh_status(if offscreen { ctx.directory.clone() } else { shell.directory() });
    ctx.out.into_inner()
}

/// Runs a line for its effect alone (a tab click, ⌘N): what it printed is
/// dropped, as Bolt's results stay closed for those.
pub fn submit(shell: Shell, input: String) {
    spawn(async move {
        run(shell, input, None).await;
    });
}

async fn execute(ctx: &Ctx) -> Result<(), String> {
    let name = ctx.command.as_str();
    if let Some(command) = name.strip_prefix(':') {
        return builtin::direct(ctx, command).await;
    }
    if name == "git" {
        let out = ctx.git(ctx.args.clone()).await?;
        ctx.add("git", out);
        return Ok(());
    }
    if name.starts_with('/') {
        if let Some(line) = ctx.settings.commands.get(name).cloned() {
            let (args, dir) = (ctx.args.clone(), ctx.directory.clone());
            git::off_thread(move || git::shell(&line, &args, &dir)).await?;
            return Ok(());
        }
    }
    let Some(entry) = REGISTRY.iter().find(|c| c.name == name) else {
        return Err(format!("command not found: {name}"));
    };
    if !entry.ported {
        return Err(format!("'{name}' runs only in the Electron Bolt for now"));
    }
    match name {
        "go" => branch::go(ctx).await,
        "list" => branch::list(ctx).await,
        "fetch" => branch::fetch(ctx).await,
        "commit" | "amend" => git_verbs::commit(ctx).await,
        "new" => git_verbs::new(ctx).await,
        "delete" => git_verbs::delete(ctx).await,
        "push" => git_verbs::push(ctx).await,
        "pop" => git_verbs::pop(ctx).await,
        "prune" => git_verbs::prune(ctx).await,
        "merge" => git_verbs::merge(ctx).await,
        "apply" => git_verbs::apply(ctx).await,
        "rename" => git_verbs::rename(ctx).await,
        "reset" => git_verbs::reset(ctx).await,
        "open" => git_verbs::open(ctx).await,
        "status" => git_verbs::status(ctx).await,
        "show" => git_verbs::show(ctx).await,
        "stash" => git_verbs::stash(ctx).await,
        "sync" => git_verbs::sync(ctx).await,
        "trace" => git_verbs::trace(ctx).await,
        // The palette clears its own thread; there is nothing else to clear.
        "clear" => Ok(()),
        "close" => Ok(()),
        "exit" => {
            std::process::exit(0);
        }
        "bm" => builtin::bookmark(ctx),
        "tab" => builtin::tab(ctx),
        "help" => builtin::help(ctx),
        "terminal" => builtin::terminal(ctx),
        "diff" => builtin::diff(ctx),
        "browser" => builtin::browser(ctx),
        "pwd" => {
            ctx.add("pwd", ctx.directory.clone());
            Ok(())
        }
        "mkdir" => builtin::mkdir(ctx).await,
        "cd" => builtin::cd(ctx).await,
        ".." => builtin::up(ctx).await,
        "ls" => builtin::ls(ctx).await,
        "notes" => builtin::notes(ctx).await,
        "settings" => builtin::settings(ctx),
        _ => unreachable!("every ported command has a handler"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_quotes_escapes_and_combined_flags() {
        let p = parse(r#"commit -am "fix \"it\" now" --name 'a b' -l=15"#);
        assert_eq!(p.command, "commit");
        assert_eq!(p.args, ["-a", "-m", r#"fix "it" now"#, "--name", "a b", "-l=15"]);
    }

    #[test]
    fn non_result_commands_match_bolt() {
        assert!(is_non_result("cd src"));
        assert!(is_non_result("bm"));
        assert!(is_non_result(":ls"));
        assert!(!is_non_result("bm list"));
        assert!(!is_non_result("pwd"));
    }
}
