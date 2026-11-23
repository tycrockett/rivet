//! The terminal tools of Bolt's MCP bridge: `sendToTerminals`
//! (`handleSendToTerminals`) and `startAgents` (`handleStartAgents` in
//! `useMcpBridge.ts`).

use std::collections::HashSet;

use serde_json::{Value, json};

use super::ToolResult;
use crate::{
    git::shell_quote,
    hub::engine::{
        fleet::{agent_command, open_workspace_terminal},
        send::{queue_submit, queue_write},
    },
    state::{self, Shell},
};

pub async fn call(shell: Shell, name: &str, args: &Value) -> Option<ToolResult> {
    match name {
        "sendToTerminals" => Some(send_to_terminals(shell, args)),
        "startAgents" => Some(start_agents(shell, args)),
        _ => None,
    }
}

fn strings(args: &Value, key: &str) -> Vec<String> {
    args.get(key).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default()
}

fn dedup(items: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    items.into_iter().filter(|i| seen.insert(i.clone())).collect()
}

/// Types into terminals that already exist; never spawns. An unknown id
/// refuses the whole call before anything is sent.
fn send_to_terminals(shell: Shell, args: &Value) -> ToolResult {
    let ids = dedup(strings(args, "sessionIds").into_iter().filter(|s| !s.trim().is_empty()).collect());
    if ids.is_empty() {
        return Err("sessionIds is required: the terminals to type into, from getWorkspaceTabs.".into());
    }
    let text = args.get("text").and_then(Value::as_str).unwrap_or("");
    if text.trim().is_empty() {
        return Err("text is required: what to type.".into());
    }
    let terms: Vec<_> = ids.iter().map(|id| (id, state::term_by_session(shell, id))).collect();
    let unknown: Vec<&str> = terms.iter().filter(|(_, t)| t.is_none()).map(|(id, _)| id.as_str()).collect();
    if !unknown.is_empty() {
        return Err(format!("No such terminal: {}. Nothing was sent.", unknown.join(", ")));
    }
    for (_, term) in &terms {
        if let Some(term) = term {
            queue_submit(term.id, text);
        }
    }
    Ok(json!({ "text": text, "sent": ids }).to_string())
}

/// A new terminal per workspace tab, typed with the agent command and the
/// quoted prompt. The user's view is left where it is.
fn start_agents(shell: Shell, args: &Value) -> ToolResult {
    let paths = dedup(
        strings(args, "paths")
            .into_iter()
            .map(|p| {
                let t = p.trim_end_matches('/');
                if t.is_empty() && p.starts_with('/') { p } else { t.to_owned() }
            })
            .filter(|p| !p.is_empty())
            .collect(),
    );
    if paths.is_empty() {
        return Err("paths is required: the workspace tab paths to start agents in.".into());
    }
    // Each terminal opens at the tab's own path, as stored: getWorkspaceTabs and the terminal chips find a tab's terminals by that.
    let tabs: Vec<String> = shell.settings.with_untracked(|s| s.tabs.iter().chain(&s.pinned).map(|t| t.path.clone()).collect());
    let resolved: Vec<(&String, Option<&String>)> = paths.iter().map(|p| (p, tabs.iter().find(|t| t.trim_end_matches('/') == p))).collect();
    let unknown: Vec<&str> = resolved.iter().filter(|(_, t)| t.is_none()).map(|(p, _)| p.as_str()).collect();
    if !unknown.is_empty() {
        return Err(format!("Not workspace tabs: {}. Nothing was started.", unknown.join(", ")));
    }

    let prompt = args.get("prompt").and_then(Value::as_str).map(str::trim).unwrap_or("");
    let command = if prompt.is_empty() { agent_command(shell) } else { format!("{} {}", agent_command(shell), shell_quote(prompt)) };

    let mut started: Vec<Value> = Vec::new();
    for (path, tab) in resolved.into_iter().filter_map(|(p, t)| Some((p, t?))) {
        let opened = open_workspace_terminal(shell, tab).and_then(|sid| state::term_by_session(shell, &sid).map(|t| (sid, t.id)).ok_or_else(|| "the terminal closed".to_owned()));
        match opened {
            Ok((session_id, id)) => {
                queue_write(id, format!("{command}\r"), 0);
                started.push(json!({ "path": path, "sessionId": session_id }));
            }
            Err(err) => {
                let done: Vec<&str> = started.iter().filter_map(|s| s["path"].as_str()).collect();
                let tail = if done.is_empty() { String::new() } else { format!(" Started: {}.", done.join(", ")) };
                return Err(format!("'{path}' failed: {err}.{tail}"));
            }
        }
    }
    Ok(json!({ "command": command, "started": started }).to_string())
}
