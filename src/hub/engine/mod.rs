//! What a palette line does once sent (`fleetStore`'s hops and posts,
//! `boltCommandStore`, `terminalSendStore`, `services/boltCommands/types`).
//!
//! - [`types`]: Bolt command scripts: nodes, parser, pure helpers.
//! - [`script`]: the node runner and the ways a line becomes a run.
//! - [`fleet`]: posts to terminals, board notes, jobs, what comes back.
//! - [`hop`]: headless agent hops, on any [`agents::Harness`].
//! - [`send`]: typing into terminals, held sends.
//! - [`agents`]: a terminal's live status (`pty:status`) and its agent's
//!   session reads.

pub mod agents;
pub mod fleet;
pub mod hop;
pub mod script;
pub mod send;
pub mod types;

use std::{cell::Cell, cell::RefCell, future::Future, path::PathBuf, time::Duration};

use nsh_core::prelude::*;
use serde::Serialize;
use serde_json::Value;

use super::{BoltCommand, ComposeSelf, Hub};
use crate::settings;

pub use types::match_command;

/// A terminal's live state (`pty:status`'s `{ status, agent, agentPid,
/// label }`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TermStatus {
    /// Bolt's names: `idle` at the shell's prompt, `working` while
    /// something runs in front of it (or an agent rests at its prompt),
    /// `agent` while an agent streams output.
    pub status: String,
    /// The agent running in it: a [`agents::Harness`] id, or the name of
    /// another agent CLI.
    pub agent: Option<String>,
    pub agent_pid: Option<u32>,
    /// What the chip shows: the agent's title and last prompt, else empty.
    pub label: String,
    /// The agent is blocked on a question for the user.
    pub needs_input: bool,
    /// The agent's session says a turn is open (codex), which a dialog
    /// waiting for an answer may be part of.
    pub mid_turn: bool,
    /// The last prompt the user typed to the agent.
    pub prompt: Option<String>,
    /// The agent's own session id (what a fork resumes).
    pub agent_session_id: Option<String>,
    /// The agent's own `--resume` argument.
    pub agent_resume: Option<String>,
    /// The session file the agent writes, which its brief is read from.
    pub agent_transcript: Option<PathBuf>,
    /// When the terminal last printed (Unix ms).
    pub last_output_at: f64,
}

impl TermStatus {
    /// The hub target's detail (`label || status || "idle"`).
    pub fn label(&self) -> String {
        [self.label.trim(), self.status.as_str()]
            .into_iter()
            .find(|s| !s.is_empty())
            .unwrap_or("idle")
            .to_owned()
    }

    /// The harness running in it, when its session can be read.
    pub fn harness(&self) -> Option<agents::Harness> {
        agents::Harness::from_id(self.agent.as_deref()?)
    }
}

thread_local! {
    /// The owner of the app, so a run outlives the palette that started it.
    static ROOT: Cell<Option<Owner>> = const { Cell::new(None) };
}

/// Runs `future` as a task of the app itself, not of whatever component
/// was handling the click: closing the palette must not stop a job.
pub(crate) fn spawn_root(future: impl Future<Output = ()> + 'static) {
    match ROOT.get() {
        Some(owner) => {
            owner.run(|| spawn(future));
        }
        None => {
            spawn(future);
        }
    }
}

/// Terminal `id`'s live state.
pub fn terminal_status(id: u64) -> TermStatus {
    agents::status_of(id)
}

const COMMANDS_FILE: &str = "bolt-commands.json";

fn commands_path() -> PathBuf {
    settings::data_dir().join(COMMANDS_FILE)
}

/// Bolt's own commands file, which seeds ours on first run.
fn bolt_commands_path() -> PathBuf {
    settings::bolt_data_dir().join(COMMANDS_FILE)
}

fn str_of(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or_default().to_owned()
}

fn read_commands(path: &PathBuf) -> Option<Vec<BoltCommand>> {
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    Some(
        saved
            .get("commands")?
            .as_array()?
            .iter()
            .map(|c| BoltCommand {
                id: Some(str_of(c, "id")).filter(|i| !i.is_empty()).unwrap_or_else(|| types::new_id("cmd")),
                name: str_of(c, "name"),
                keyword: str_of(c, "keyword"),
                targets: c.get("targets").and_then(Value::as_array).map(|t| t.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default(),
                script: str_of(c, "script"),
            })
            .collect(),
    )
}

/// The Bolt commands, from `bolt-commands.json` (`initBoltCommands`): this
/// app's own file, else Bolt's.
pub fn load_commands() -> Vec<BoltCommand> {
    if ROOT.get().is_none() {
        ROOT.set(Some(Owner::current()));
    }
    read_commands(&commands_path()).or_else(|| read_commands(&bolt_commands_path())).unwrap_or_default()
}

#[derive(Serialize)]
struct SavedCommands<'a> {
    commands: &'a [BoltCommand],
}

/// Writes the commands as Bolt writes them (`flushBoltCommands`).
pub fn save_commands(commands: &[BoltCommand]) {
    let path = commands_path();
    let write = || -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
        std::fs::write(&tmp, serde_json::to_vec(&SavedCommands { commands }).map_err(std::io::Error::other)?)?;
        std::fs::rename(tmp, &path)
    };
    if let Err(e) = write() {
        eprintln!("rivet: could not save the Bolt commands: {e}");
    }
}

/// Starts what runs beside the hub for as long as the app does: the save of
/// edited Bolt commands (debounced 500 ms, as Bolt does) and the poll that
/// types held lines into terminals and brings back what they answer. Call
/// once, from the root, after the hub is provided.
pub fn start(hub: Hub) {
    ROOT.set(Some(Owner::current()));
    let pending: RefCell<Option<Spawned<()>>> = RefCell::new(None);
    let first = Cell::new(true);
    effect(move || {
        hub.commands.with(|_| ());
        if first.replace(false) {
            return;
        }
        if let Some(t) = pending.borrow_mut().take() {
            t.cancel();
        }
        *pending.borrow_mut() = Some(set_timeout(Duration::from_millis(500), move || {
            hub.commands.with_untracked(|c| save_commands(c));
        }));
    });
    fleet::start(hub);
}

pub fn check_script(script: &str) -> (usize, Vec<ScriptError>) {
    let (nodes, errors) = types::parse_script(script);
    (nodes.len(), errors)
}

pub use types::ScriptError;

pub fn is_script(text: &str) -> bool {
    text.trim_start().starts_with('#')
}

/// `#create` objects with their hints, then its `from` / `in` / `with`
/// words with the arg each sets, for the editor's completion
/// (`CREATE_OBJECTS`, `CREATE_WORDS`). `types::CREATE_OBJECTS` has each
/// object's keys.
pub fn create_words() -> Vec<(&'static str, &'static str)> {
    types::CREATE_OBJECTS.iter().map(|o| (o.name, o.hint)).chain(types::CREATE_WORDS.iter().copied()).collect()
}

/// A post or a Bolt command from the board (`submitBoltAtom`).
pub fn submit_bolt(hub: Hub, text: String) {
    script::submit_bolt(hub, text);
}

/// A line sent as `self_` (`composeSendAtom`).
pub fn compose_send(hub: Hub, text: String, self_: ComposeSelf) {
    script::compose_send(hub, text, self_);
}

/// `@bolt …` (`chatBoltAtom`).
pub fn chat_bolt(hub: Hub, text: String) {
    spawn_root(script::chat_bolt(hub, text));
}

/// A job card's Cancel (`cancelJobAtom`).
pub fn cancel_job(hub: Hub, job_id: &str) {
    script::cancel_job(hub, job_id);
}

/// An approval step's Continue (`continueApprovalAtom`).
pub fn continue_approval(hub: Hub, job_id: &str, at: usize) {
    let _ = hub;
    script::continue_approval(job_id, at);
}

/// A reply typed under a `@bolt` job (`replyBoltAtom`).
pub fn reply_bolt(hub: Hub, job_id: &str, text: String) {
    spawn_root(script::reply_bolt(hub, job_id.to_owned(), text));
}

/// A job's hop picked up in a terminal (`openHopAtom`).
pub fn open_hop(hub: Hub, job_id: &str) {
    fleet::open_hop(hub, job_id);
}

/// A job's reply typed into its addressee's terminal (`deliverJobAtom`).
#[allow(dead_code)]
pub fn deliver_job(hub: Hub, job_id: &str) {
    fleet::deliver_job(hub, job_id);
}

/// Leaves the palette for that terminal (`revealMemberAtom`).
pub fn reveal_member(hub: Hub, cwd: &str, session_id: Option<&str>) {
    fleet::reveal_member(hub, cwd, session_id);
}

/// `{:name}`s of a palette line run as a board job and spliced in
/// (`expandScripts`).
#[allow(dead_code)]
pub async fn expand_scripts(hub: Hub, line: &str) -> Result<String, String> {
    script::expand_scripts(hub, line).await
}


