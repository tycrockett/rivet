//! Bolt's MCP server (`mcpServer.js`, its streamable HTTP transport): the
//! `bolt` server every agent reaches at `http://127.0.0.1:41777/mcp`
//! (`~/.omp/agent/mcp.json`, `~/.claude.json`, `~/.codex/config.toml`). JSON-RPC over `POST /mcp`,
//! batches allowed, notifications answered 202; `GET` 405, `DELETE` 204.
//!
//! A listener thread takes connections, one thread each. `initialize` and
//! `tools/list` are answered there; a `tools/call` crosses to the engine
//! thread, which owns every signal and the PTY table, over a channel drained
//! by a task of the app ([`spawn_root`]), and its answer comes back on a
//! channel of its own, waited on as long as Bolt waits (10 s, 120 s a
//! command for the palette's slow verbs).
//!
//! The tool schemas are Bolt's (`tools.json`, dumped from
//! `_handleToolsList`), with Rivet's names where they differ: `startAgents`
//! starts whatever agent the user's `agentCommand` is, and `saveNote`
//! saves a rivet note. The tools Rivet has nothing behind (selection,
//! diagnostics, the model canvas, graphs, boards, terminal transcripts,
//! group sessions) stay listed and answer that they are not supported
//! here; those it backs in part (the web tab tools) say what is missing.
//!
//! Unlike Bolt's, a request carrying an `Origin` that is not this machine is
//! refused: a web page could otherwise post to it and type into terminals.

mod editor;
mod notes;
mod terminals;
mod web;
mod workspace;

use std::{
    io::{BufRead, BufReader, Write},
    net::{TcpListener, TcpStream},
    sync::mpsc,
    thread,
    time::Duration,
};

use serde_json::{Value, json};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::{hub::engine::spawn_root, state::Shell};

/// Fixed, because `claude mcp add`, `codex mcp add` and omp's `mcp.json` store a literal URL.
const PORT: u16 = 41777;
/// Bolt's `HTTP_RETRY_MS`: while another app holds the port.
const RETRY: Duration = Duration::from_secs(5);
/// Bolt's `REQUEST_TIMEOUT`.
const TIMEOUT: Duration = Duration::from_secs(10);
/// Bolt's `RUN_COMMAND_TIMEOUT`, per command of a batch, at most ten.
const SLOW_TIMEOUT: Duration = Duration::from_secs(120);
/// A request body past this is refused rather than read.
const MAX_BODY: usize = 8 << 20;

const TOOLS_JSON: &str = include_str!("tools.json");

/// The tools listed but not backed by anything in Rivet.
const UNSUPPORTED: &[&str] = &[
    "getCurrentSelection",
    "getLatestSelection",
    "getDiagnostics",
    "startGroupSession",
    "readTerminals",
    "readBoard",
    "listBoards",
    "postBoard",
    "graphUpsert",
    "graphRead",
    "graphRemove",
    "upsertContainers",
    "upsertComponents",
    "setRelationships",
    "removeContainers",
    "removeComponents",
    "removeRelationships",
    "getOntology",
    "getSelection",
    "showInBolt",
];

/// A tool's answer: its text, or an error the caller sees as `isError`.
pub type ToolResult = Result<String, String>;

struct Call {
    name: String,
    args: Value,
    reply: mpsc::SyncSender<ToolResult>,
}

/// Starts the server: the listener thread, and the engine-side task that
/// runs each tool call against `shell`.
pub fn start(shell: Shell) {
    let (tx, rx) = unbounded_channel();
    spawn_root(serve_calls(shell, rx));
    let spawned = thread::Builder::new().name("mcp-listen".into()).spawn(move || listen(tx));
    if let Err(e) = spawned {
        eprintln!("rivet: mcp: could not start the listener: {e}");
    }
}

/// Each call as a task of its own, so a slow `runCommand` holds up nothing.
async fn serve_calls(shell: Shell, mut rx: UnboundedReceiver<Call>) {
    while let Some(call) = rx.recv().await {
        spawn_root(async move {
            let result = dispatch(shell, &call.name, &call.args).await;
            let _ = call.reply.send(result);
        });
    }
}

async fn dispatch(shell: Shell, name: &str, args: &Value) -> ToolResult {
    if UNSUPPORTED.contains(&name) {
        return Err(format!("{name} is not supported in Rivet"));
    }
    if let Some(r) = workspace::call(shell, name, args).await {
        return r;
    }
    if let Some(r) = terminals::call(shell, name, args).await {
        return r;
    }
    if let Some(r) = editor::call(shell, name, args).await {
        return r;
    }
    if let Some(r) = notes::call(shell, name, args).await {
        return r;
    }
    if let Some(r) = web::call(shell, name, args).await {
        return r;
    }
    Err(format!("Unknown tool: {name}"))
}

fn listen(tx: UnboundedSender<Call>) {
    let listener = loop {
        match TcpListener::bind(("127.0.0.1", PORT)) {
            Ok(l) => break l,
            Err(e) => {
                eprintln!("rivet: mcp: port {PORT} unavailable ({e}); retrying");
                thread::sleep(RETRY);
            }
        }
    };
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let tx = tx.clone();
        let _ = thread::Builder::new().name("mcp-conn".into()).spawn(move || {
            if let Err(e) = connection(stream, &tx) {
                eprintln!("rivet: mcp: {e}");
            }
        });
    }
}

struct Request {
    method: String,
    path: String,
    origin: Option<String>,
    body: Vec<u8>,
}

/// One request per connection, answered with `Connection: close`.
fn connection(stream: TcpStream, tx: &UnboundedSender<Call>) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut out = stream;
    let Some(req) = read_request(&mut reader)? else {
        return respond(&mut out, 400, "Bad Request", None);
    };
    if req.origin.as_deref().is_some_and(|o| !is_local_origin(o)) {
        return respond(&mut out, 403, "Forbidden", None);
    }
    let path = req.path.split('?').next().unwrap_or("");
    match (req.method.as_str(), path) {
        ("POST", "/mcp") => {}
        ("GET", "/mcp") => return respond(&mut out, 405, "Method Not Allowed", None),
        ("DELETE", "/mcp") => return respond(&mut out, 204, "No Content", None),
        _ => return respond(&mut out, 404, "Not Found", None),
    }
    let Ok(msg) = serde_json::from_slice::<Value>(&req.body) else {
        let err = json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": "Parse error" } });
        return respond(&mut out, 400, "Bad Request", Some(&err));
    };
    let reply = match msg {
        Value::Array(batch) => {
            let replies: Vec<Value> = batch.iter().filter_map(|m| handle(m, tx)).collect();
            (!replies.is_empty()).then_some(Value::Array(replies))
        }
        m => handle(&m, tx),
    };
    match reply {
        Some(body) => respond(&mut out, 200, "OK", Some(&body)),
        None => respond(&mut out, 202, "Accepted", None),
    }
}

fn read_request(reader: &mut impl BufRead) -> std::io::Result<Option<Request>> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    let mut parts = line.split_whitespace();
    let (Some(method), Some(path)) = (parts.next(), parts.next()) else { return Ok(None) };
    let (method, path) = (method.to_owned(), path.to_owned());
    let (mut length, mut origin) = (0usize, None);
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let header = line.trim_end();
        if header.is_empty() {
            break;
        }
        let Some((key, value)) = header.split_once(':') else { continue };
        let value = value.trim();
        if key.eq_ignore_ascii_case("content-length") {
            length = value.parse().unwrap_or(0);
        } else if key.eq_ignore_ascii_case("origin") {
            origin = Some(value.to_owned());
        }
    }
    if length > MAX_BODY {
        return Ok(None);
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    Ok(Some(Request { method, path, origin, body }))
}

/// `http://localhost:…`, `http://127.0.0.1:…`, `http://[::1]:…`; a page
/// anywhere else gets nothing.
fn is_local_origin(origin: &str) -> bool {
    let host = origin.split_once("://").map_or(origin, |(_, rest)| rest);
    let host = host.split('/').next().unwrap_or("");
    let host = if host.starts_with('[') {
        host.split(']').next().map_or(host, |h| h.trim_start_matches('['))
    } else {
        host.split(':').next().unwrap_or(host)
    };
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

fn respond(out: &mut TcpStream, status: u16, reason: &str, body: Option<&Value>) -> std::io::Result<()> {
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    let kind = if body.is_empty() { "" } else { "Content-Type: application/json\r\n" };
    write!(out, "HTTP/1.1 {status} {reason}\r\n{kind}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())?;
    out.flush()
}

/// One JSON-RPC message (`_dispatchJsonRpc`): its response, or `None` for a
/// notification.
fn handle(msg: &Value, tx: &UnboundedSender<Call>) -> Option<Value> {
    let id = msg.get("id").filter(|id| !id.is_null())?.clone();
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let result = match msg.get("method").and_then(Value::as_str).unwrap_or("") {
        "initialize" => Ok(initialize(&params)),
        "tools/list" => Ok(tools_list()),
        "tools/call" => tool_call(&params, tx).map(|r| match r {
            Ok(text) => json!({ "content": [{ "type": "text", "text": text }] }),
            Err(text) => json!({ "content": [{ "type": "text", "text": text }], "isError": true }),
        }),
        method => {
            return Some(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": format!("Method not found: {method}") } }));
        }
    };
    Some(match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err(message) => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": message } }),
    })
}

/// Echoes the client's version: a mismatched echo makes strict clients bail.
fn initialize(params: &Value) -> Value {
    let version = params.get("protocolVersion").and_then(Value::as_str).unwrap_or("2024-11-05");
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "Rivet", "version": env!("CARGO_PKG_VERSION") },
    })
}

/// Bolt's schemas, the unsupported ones saying so first, the partly
/// supported ones what they lack.
fn tools_list() -> Value {
    let mut tools: Vec<Value> = serde_json::from_str(TOOLS_JSON).expect("tools.json is valid JSON");
    for tool in &mut tools {
        let name = tool["name"].as_str().unwrap_or_default();
        let description = tool["description"].as_str().unwrap_or_default();
        if UNSUPPORTED.contains(&name) {
            tool["description"] = Value::String(format!("Not supported in Rivet: returns an error. {description}"));
        } else if let Some((_, note)) = web::NOTES.iter().find(|(n, _)| *n == name) {
            tool["description"] = Value::String(format!("{note} {description}"));
        }
    }
    json!({ "tools": tools })
}

/// Hands a call to the engine thread and waits for its answer. The outer
/// `Err` is a JSON-RPC error (no engine, timed out); the inner one is the
/// tool's own.
fn tool_call(params: &Value, tx: &UnboundedSender<Call>) -> Result<ToolResult, String> {
    let name = params.get("name").and_then(Value::as_str).ok_or("tools/call needs a name")?.to_owned();
    let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
    let timeout = if matches!(name.as_str(), "runCommand" | "createWorktrees") {
        let batch = ["command", "branches"].iter().find_map(|k| args.get(k)?.as_array().map(Vec::len)).unwrap_or(1);
        SLOW_TIMEOUT * batch.clamp(1, 10) as u32
    } else {
        TIMEOUT
    };
    let (reply, answer) = mpsc::sync_channel(1);
    tx.send(Call { name: name.clone(), args, reply }).map_err(|_| "Rivet is shutting down".to_owned())?;
    answer.recv_timeout(timeout).map_err(|_| format!("{name} timed out after {}s", timeout.as_secs()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_origins() {
        assert!(is_local_origin("http://localhost:3000"));
        assert!(is_local_origin("http://127.0.0.1"));
        assert!(is_local_origin("http://[::1]:8080"));
        assert!(!is_local_origin("https://evil.example"));
        assert!(!is_local_origin("http://localhost.evil.example"));
        assert!(!is_local_origin("null"));
    }

    #[test]
    fn every_unsupported_tool_is_listed() {
        let tools: Vec<Value> = serde_json::from_str(TOOLS_JSON).unwrap();
        for name in UNSUPPORTED {
            assert!(tools.iter().any(|t| t["name"] == *name), "{name} is not in tools.json");
        }
    }
}
