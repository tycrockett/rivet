//! Bolt's pty daemon (`src/ptyd/daemon.js`), so terminals outlive the app:
//! this executable run as `--ptyd` ([`ENTRY`]) owns every PTY, with the scrollback ring
//! and last-output time the app asks for when it comes back. Unlike Bolt's
//! client, a launch does not restart the daemon: surviving a restart of the
//! app is the point (`pty.rs` only replaces a daemon that speaks another
//! [`PROTOCOL_VERSION`]).
//!
//! Wire: `[4-byte BE length][JSON]` both ways (`protocol.js`). A request
//! has `type` and, when it wants an answer, an `id` that the reply
//! (`{ id, ok, ... }` or `{ id, ok: false, error }`) repeats. `hello
//! { version }` opens a connection; `spawn { cwd, cols, rows }`, `list`,
//! `attach { session_id }` and `kill { session_id }` answer; `write
//! { session_id, data }`, `resize { session_id, cols, rows }` and `detach
//! { session_id }` do not. A client that spawned or attached to a session
//! then gets its `{ event: "data", session_id, data }` and `{ event:
//! "exit", session_id }`. A client going away just detaches. A `kill` ends
//! the shell and what it runs in front of it (an agent that ignores the
//! hangup too): hangup, then terminate, then kill ([`end_session`]).
//!
//! What the daemon does is logged to `ptyd.log` beside its socket.

use std::{
    collections::{BTreeSet, HashMap, HashSet, VecDeque},
    fs::{File, TryLockError},
    io::{self, Read, Write},
    os::unix::{fs::PermissionsExt, net::{UnixListener, UnixStream}},
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard, mpsc},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde_json::{Value, json};

use crate::settings::data_dir;

/// Bumped when the wire changes: a client that finds another version
/// replaces the daemon, which loses its sessions.
pub const PROTOCOL_VERSION: u32 = 1;

/// A session's ring (`ringBuffer.js` `DEFAULT_MAX`).
const RING_MAX: usize = 5 * 1024 * 1024;

/// DEC private modes that change what the keyboard and mouse send:
/// application cursor keys, mouse reports, focus events, bracketed paste.
/// A program sets them once, when it starts, and a scrollback wipe (omp's
/// redraw on a resize) or the ring's size soon drops that from the
/// replay; a view attached later would type and paste as if they were
/// off (omp takes an empty bracketed paste as an image paste, and an
/// unbracketed empty paste sends nothing).
const INPUT_MODES: [u16; 7] = [1, 1000, 1002, 1003, 1004, 1006, 2004];

/// The [`INPUT_MODES`] a session's output has turned on.
#[derive(Default)]
pub(crate) struct Modes {
    on: BTreeSet<u16>,
    /// A sequence the last chunk ended inside.
    tail: String,
}

impl Modes {
    /// Follows `CSI ? Pm h` / `CSI ? Pm l` and a reset (`ESC c`) in `data`.
    pub(crate) fn scan(&mut self, data: &str) {
        let mut text = std::mem::take(&mut self.tail);
        text.push_str(data);
        let b = text.as_bytes();
        let mut i = 0;
        while let Some(at) = b[i..].iter().position(|&c| c == 0x1b).map(|p| p + i) {
            match (b.get(at + 1), b.get(at + 2)) {
                (Some(b'c'), _) => {
                    self.on.clear();
                    i = at + 2;
                }
                (Some(b'['), Some(b'?')) => {
                    let params = at + 3;
                    let Some(end) = b[params..].iter().position(|c| !(c.is_ascii_digit() || *c == b';')).map(|p| p + params) else {
                        // Cut off mid-sequence; a long run of digits is no mode.
                        if b.len() - at < 32 {
                            self.tail = text[at..].to_owned();
                        }
                        return;
                    };
                    let modes = text[params..end].split(';').filter_map(|p| p.parse::<u16>().ok());
                    match b[end] {
                        b'h' => self.on.extend(modes.filter(|n| INPUT_MODES.contains(n))),
                        b'l' => modes.for_each(|n| {
                            self.on.remove(&n);
                        }),
                        _ => {}
                    }
                    i = end + 1;
                }
                (None, _) | (Some(b'['), None) => {
                    self.tail = text[at..].to_owned();
                    return;
                }
                _ => i = at + 1,
            }
        }
    }

    /// Turns them on again.
    pub(crate) fn replay(&self) -> String {
        self.on.iter().map(|n| format!("\x1b[?{n}h")).collect()
    }
}

/// A frame past this is a broken peer, not a message.
const FRAME_MAX: usize = 64 * 1024 * 1024;

/// Idle shutdown: no live session and no client for this long (Bolt's
/// `IDLE_TIMEOUT`). `RIVET_PTYD_IDLE_MS` shortens it, for tests.
const IDLE_MS: u64 = 5 * 60 * 1000;
const IDLE_ENV: &str = "RIVET_PTYD_IDLE_MS";

/// How long a hangup has to end a session's processes, then a terminate,
/// before they are killed.
const HANGUP_GRACE: Duration = Duration::from_millis(600);
const TERMINATE_GRACE: Duration = Duration::from_millis(1500);

/// The log grows to this before the next daemon starts it over.
const LOG_MAX: u64 = 1024 * 1024;

pub fn socket_path() -> PathBuf {
    data_dir().join("ptyd.sock")
}

pub fn pid_path() -> PathBuf {
    data_dir().join("ptyd.pid")
}

fn lock_path() -> PathBuf {
    data_dir().join("ptyd.lock")
}

fn log_path() -> PathBuf {
    data_dir().join("ptyd.log")
}

/// Appends a line to the daemon's log: who asked for what, and how each
/// shell ended, so a terminal that vanished can be accounted for. One write
/// per line, so clients logging at once do not interleave.
fn log(line: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(log_path()) {
        let _ = f.write_all(format!("{:.3} {line}\n", unix_ms() / 1000.0).as_bytes());
    }
}

pub fn unix_ms() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_millis() as f64)
}

/// `msg` as a frame.
pub fn encode(msg: &Value) -> Vec<u8> {
    let payload = msg.to_string().into_bytes();
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(&payload);
    frame
}

/// The next frame; an error at the end of the stream.
pub fn read_frame(r: &mut impl Read) -> io::Result<Value> {
    let mut header = [0u8; 4];
    r.read_exact(&mut header)?;
    let len = u32::from_be_bytes(header) as usize;
    if len > FRAME_MAX {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    serde_json::from_slice(&payload).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// The last [`RING_MAX`] bytes of a session's output, as whole chunks
/// (`ringBuffer.js`): the oldest go first, so the text stays valid UTF-8.
/// Replayed after the [`INPUT_MODES`] the output turned on.
struct Ring {
    chunks: VecDeque<String>,
    bytes: usize,
    max: usize,
    modes: Modes,
}

impl Ring {
    fn new(max: usize) -> Ring {
        Ring { chunks: VecDeque::new(), bytes: 0, max, modes: Modes::default() }
    }

    fn push(&mut self, data: &str) {
        self.modes.scan(data);
        // A real terminal already threw away what came before a scrollback
        // wipe, so replaying it only re-runs every redraw.
        let wipe = ["\x1b[3J", "\x1bc"].iter().filter_map(|w| data.rfind(w)).max();
        let data = match wipe {
            Some(at) => {
                self.chunks.clear();
                self.bytes = 0;
                &data[at..]
            }
            None => data,
        };
        self.bytes += data.len();
        self.chunks.push_back(data.to_owned());
        while self.bytes > self.max && self.chunks.len() > 1 {
            let gone = self.chunks.pop_front().expect("more than one chunk");
            self.bytes -= gone.len();
        }
    }

    fn all(&self) -> String {
        let modes = self.modes.replay();
        let mut out = String::with_capacity(modes.len() + self.bytes);
        out.push_str(&modes);
        out.extend(self.chunks.iter().map(String::as_str));
        out
    }
}

struct Session {
    pid: u32,
    cwd: String,
    cols: u16,
    rows: u16,
    ring: Ring,
    last_output_at: f64,
    /// `Some` once the shell exited.
    exit_code: Option<u32>,
    master: Box<dyn MasterPty + Send>,
    /// Input for the session's writer thread ([`spawn_input`]): a program
    /// not reading its input stalls that thread, not a client's requests
    /// (a kill among them) nor the daemon.
    input: mpsc::Sender<Vec<u8>>,
    /// The clients that get its events.
    subscribers: HashSet<u64>,
}

#[derive(Default)]
struct State {
    sessions: HashMap<String, Session>,
    /// Each client's outgoing frames; one thread per client writes them.
    clients: HashMap<u64, mpsc::Sender<Arc<Vec<u8>>>>,
    next_session: u64,
    next_client: u64,
}

type Shared = Arc<Mutex<State>>;

fn lock(shared: &Shared) -> MutexGuard<'_, State> {
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

impl State {
    fn send(&self, client: u64, frame: &Arc<Vec<u8>>) {
        if let Some(tx) = self.clients.get(&client) {
            let _ = tx.send(frame.clone());
        }
    }

    fn reply(&self, client: u64, id: Option<&Value>, body: Result<Value, String>) {
        let Some(id) = id else { return };
        let msg = match body {
            Ok(mut v) => {
                v["id"] = id.clone();
                v["ok"] = Value::Bool(true);
                v
            }
            Err(error) => json!({ "id": id, "ok": false, "error": error }),
        };
        self.send(client, &Arc::new(encode(&msg)));
    }

    fn broadcast(&self, session_id: &str, msg: &Value) {
        let Some(s) = self.sessions.get(session_id) else { return };
        let frame = Arc::new(encode(msg));
        for c in &s.subscribers {
            self.send(*c, &frame);
        }
    }

    fn info(id: &str, s: &Session) -> Value {
        json!({
            "session_id": id, "pid": s.pid, "cwd": s.cwd, "cols": s.cols, "rows": s.rows,
            "alive": s.exit_code.is_none(), "exit_code": s.exit_code, "last_output_at": s.last_output_at,
        })
    }

    fn alive(&self) -> usize {
        self.sessions.values().filter(|s| s.exit_code.is_none()).count()
    }
}

/// The thread that writes a session's input to its pty, in order.
fn spawn_input(mut writer: Box<dyn Write + Send>) -> io::Result<mpsc::Sender<Vec<u8>>> {
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    thread::Builder::new().name("ptyd-input".into()).spawn(move || {
        while let Ok(data) = rx.recv() {
            if writer.write_all(&data).and_then(|()| writer.flush()).is_err() {
                break;
            }
        }
    })?;
    Ok(tx)
}

/// The process groups of a session worth signalling: the shell's own (it
/// leads a session of its own, so its group is its pid) and the one in
/// front of the tty, which is what the shell waits on (an agent) when it
/// is another. Only groups that belong to the session are named: a pid
/// that leads some other session is not ours to signal.
fn session_groups(pid: u32, foreground: Option<i32>) -> Vec<i32> {
    let shell = pid as i32;
    let mut groups = vec![shell];
    // SAFETY: `getsid` takes a pid and reads nothing of ours.
    if let Some(front) = foreground.filter(|g| *g != shell && unsafe { libc::getsid(*g) } == shell) {
        groups.push(front);
    }
    groups
}

fn signal_groups(groups: &[i32], signal: i32) {
    for group in groups {
        // SAFETY: `killpg` takes a group id; the groups are the session's
        // ([`session_groups`]).
        unsafe { libc::killpg(*group, signal) };
    }
}

fn groups_gone(groups: &[i32]) -> bool {
    // SAFETY: signal 0 only checks that the group exists.
    groups.iter().all(|group| unsafe { libc::killpg(*group, 0) } != 0)
}

/// Ends a session's processes, without waiting for them: a hangup now, as
/// `node-pty`'s kill sends, then a terminate and a kill for whatever is
/// still there after each grace. A shell running an agent that ignores the
/// hangup (omp does) would otherwise end up hung half way out, with the
/// agent running on with no terminal.
fn end_session(groups: Vec<i32>) {
    signal_groups(&groups, libc::SIGHUP);
    let escalate = thread::Builder::new().name("ptyd-end".into()).spawn({
        let groups = groups.clone();
        move || {
            for (grace, signal) in [(HANGUP_GRACE, libc::SIGTERM), (TERMINATE_GRACE, libc::SIGKILL)] {
                let until = Instant::now() + grace;
                while Instant::now() < until {
                    if groups_gone(&groups) {
                        return;
                    }
                    thread::sleep(Duration::from_millis(20));
                }
                signal_groups(&groups, signal);
            }
        }
    });
    // No thread to wait with: kill at once rather than leave them running.
    if escalate.is_err() {
        signal_groups(&groups, libc::SIGKILL);
    }
}

/// Where `--ptyd` becomes the daemon: among the executable's initializers,
/// before any `main`. The executable is whatever links this crate: Rivet's
/// own `main.rs`, or a `main` nsh generates for `nsh run`, `nsh test` and
/// its dev host, which never look for `--ptyd` and would boot the whole
/// app instead (windowless, in the Dock, never idling out). `pty.rs`
/// refers to it so the linker keeps it.
#[used]
#[cfg_attr(target_os = "macos", unsafe(link_section = "__DATA,__mod_init_func"))]
#[cfg_attr(target_os = "linux", unsafe(link_section = ".init_array"))]
pub static ENTRY: extern "C" fn(libc::c_int, *const *const libc::c_char) = enter;

/// dyld and glibc pass an initializer the process's `argc` and `argv`.
extern "C" fn enter(argc: libc::c_int, argv: *const *const libc::c_char) {
    // SAFETY: `argv` holds `argc` NUL-terminated strings.
    if argc < 2 || unsafe { std::ffi::CStr::from_ptr(*argv.add(1)) } != c"--ptyd" {
        return;
    }
    // What std's runtime does before `main`: a write to a client that went
    // away fails rather than ending the daemon.
    // SAFETY: setting a signal's disposition touches no memory.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };
    std::process::exit(match run() {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("rivet: pty daemon: {e}");
            1
        }
    });
}

/// Runs the daemon until it idles out. Returns at once when another daemon
/// of this data directory holds the lock (two apps raced to start one).
pub fn run() -> io::Result<()> {
    std::fs::create_dir_all(data_dir())?;
    let lock_file = File::options().create(true).truncate(false).write(true).open(lock_path())?;
    match lock_file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(()),
        Err(TryLockError::Error(e)) => return Err(e),
    }
    let socket = socket_path();
    // Ours to take: the lock says no daemon is serving it.
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket)?;
    // Whoever connects gets a shell.
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
    std::fs::write(pid_path(), std::process::id().to_string())?;
    if std::fs::metadata(log_path()).is_ok_and(|m| m.len() > LOG_MAX) {
        let _ = std::fs::remove_file(log_path());
    }
    log(&format!("daemon {} listening, protocol {PROTOCOL_VERSION}", std::process::id()));

    let shared: Shared = Arc::new(Mutex::new(State::default()));
    let idle = Duration::from_millis(std::env::var(IDLE_ENV).ok().and_then(|v| v.parse().ok()).unwrap_or(IDLE_MS));
    {
        let shared = shared.clone();
        thread::Builder::new().name("ptyd-idle".into()).spawn(move || idle_watch(&shared, idle))?;
    }
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let shared = shared.clone();
        let _ = thread::Builder::new().name("ptyd-client".into()).spawn(move || serve(stream, &shared));
    }
    Ok(())
}

/// Exits once no shell lives and no client is connected for `idle`.
fn idle_watch(shared: &Shared, idle: Duration) {
    let tick = (idle / 10).clamp(Duration::from_millis(50), Duration::from_secs(5));
    let mut since: Option<Instant> = None;
    loop {
        thread::sleep(tick);
        let busy = {
            let st = lock(shared);
            st.alive() > 0 || !st.clients.is_empty()
        };
        if busy {
            since = None;
        } else if since.get_or_insert_with(Instant::now).elapsed() >= idle {
            let _ = std::fs::remove_file(socket_path());
            let _ = std::fs::remove_file(pid_path());
            std::process::exit(0);
        }
    }
}

/// One client's connection, until it closes.
fn serve(stream: UnixStream, shared: &Shared) {
    let Ok(mut reader) = stream.try_clone() else { return };
    let (tx, rx) = mpsc::channel::<Arc<Vec<u8>>>();
    let mut writer = stream;
    if thread::Builder::new()
        .name("ptyd-write".into())
        .spawn(move || {
            while let Ok(frame) = rx.recv() {
                if writer.write_all(&frame).is_err() {
                    break;
                }
            }
        })
        .is_err()
    {
        return;
    }
    let client = {
        let mut st = lock(shared);
        st.next_client += 1;
        let id = st.next_client;
        st.clients.insert(id, tx);
        id
    };
    log(&format!("client {client} connected"));
    while let Ok(msg) = read_frame(&mut reader) {
        handle(shared, client, &msg);
    }
    let mut st = lock(shared);
    st.clients.remove(&client);
    for s in st.sessions.values_mut() {
        s.subscribers.remove(&client);
    }
    drop(st);
    log(&format!("client {client} disconnected"));
}

fn handle(shared: &Shared, client: u64, msg: &Value) {
    let id = msg.get("id");
    let text = |k: &str| msg.get(k).and_then(Value::as_str);
    let size = |k: &str, default: u16| msg.get(k).and_then(Value::as_u64).map_or(default, |n| n.clamp(1, u64::from(u16::MAX)) as u16);
    match text("type").unwrap_or_default() {
        "hello" => lock(shared).reply(client, id, Ok(json!({ "version": PROTOCOL_VERSION, "pid": std::process::id() }))),
        "spawn" => {
            let cwd = text("cwd").unwrap_or_default();
            let (cols, rows) = (size("cols", 80), size("rows", 24));
            match spawn(shared, client, cwd, cols, rows) {
                // Under the lock the shell's first bytes wait for the answer.
                Ok((session_id, pid, st)) => st.reply(client, id, Ok(json!({ "session_id": session_id, "pid": pid }))),
                Err(e) => lock(shared).reply(client, id, Err(e)),
            }
        }
        "list" => {
            let st = lock(shared);
            let mut sessions: Vec<_> = st.sessions.iter().map(|(i, s)| State::info(i, s)).collect();
            sessions.sort_by(|a, b| a["session_id"].as_str().cmp(&b["session_id"].as_str()));
            st.reply(client, id, Ok(json!({ "sessions": sessions })));
        }
        "attach" => {
            let mut st = lock(shared);
            let session_id = text("session_id").unwrap_or_default();
            let body = match st.sessions.get_mut(session_id) {
                Some(s) => {
                    s.subscribers.insert(client);
                    let mut info = State::info(session_id, s);
                    info["replay"] = Value::String(s.ring.all());
                    log(&format!("client {client} attached {session_id}"));
                    Ok(info)
                }
                None => Err("no such session".into()),
            };
            st.reply(client, id, body);
        }
        "write" => {
            if let Some(s) = lock(shared).sessions.get(text("session_id").unwrap_or_default()) {
                let _ = s.input.send(text("data").unwrap_or_default().as_bytes().to_vec());
            }
        }
        "resize" => {
            let mut st = lock(shared);
            if let Some(s) = st.sessions.get_mut(text("session_id").unwrap_or_default()) {
                let (cols, rows) = (size("cols", s.cols), size("rows", s.rows));
                if s.master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 }).is_ok() {
                    (s.cols, s.rows) = (cols, rows);
                }
            }
        }
        "detach" => {
            if let Some(s) = lock(shared).sessions.get_mut(text("session_id").unwrap_or_default()) {
                s.subscribers.remove(&client);
            }
        }
        "kill" => {
            let session_id = text("session_id").unwrap_or_default();
            let mut st = lock(shared);
            if let Some(s) = st.sessions.remove(session_id) {
                log(&format!("client {client} killed {session_id} (shell {}, {})", s.pid, s.exit_code.map_or("running".into(), |c| format!("exited {c}"))));
                if s.exit_code.is_none() {
                    // The shell is reaped by its reader thread, once what
                    // holds its tty is gone.
                    end_session(session_groups(s.pid, s.master.process_group_leader()));
                    let frame = Arc::new(encode(&json!({ "event": "exit", "session_id": session_id })));
                    for c in &s.subscribers {
                        st.send(*c, &frame);
                    }
                }
            } else {
                log(&format!("client {client} killed {session_id}: no such session"));
            }
            st.reply(client, id, Ok(json!({})));
        }
        other => lock(shared).reply(client, id, Err(format!("unknown request: {other}"))),
    }
}

/// Starts a shell as `handleSpawn` does and subscribes `client` to it. The
/// state stays locked, for the reply to go out before any output.
fn spawn<'a>(shared: &'a Shared, client: u64, cwd: &str, cols: u16, rows: u16) -> Result<(String, u32, MutexGuard<'a, State>), String> {
    let pair = native_pty_system()
        .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
        .map_err(|e| e.to_string())?;
    // `resolveSpawnCwd`: a directory that is gone must not stop a shell.
    let cwd = if !cwd.is_empty() && std::path::Path::new(cwd).is_dir() {
        cwd.to_owned()
    } else {
        dirs::home_dir().map_or_else(|| "/".into(), |p| p.to_string_lossy().into_owned())
    };
    let mut cmd = CommandBuilder::new(default_shell());
    cmd.cwd(&cwd);
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    let mut child = pair.slave.spawn_command(cmd).map_err(|e| e.to_string())?;
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;
    let writer = pair.master.take_writer().map_err(|e| e.to_string())?;
    let input = spawn_input(writer).map_err(|e| e.to_string())?;
    let pid = child.process_id().unwrap_or(0);

    let mut st = lock(shared);
    st.next_session += 1;
    let session_id = format!("pty-{}-{}", unix_ms() as u64, st.next_session);
    st.sessions.insert(
        session_id.clone(),
        Session {
            pid,
            cwd,
            cols,
            rows,
            ring: Ring::new(RING_MAX),
            last_output_at: unix_ms(),
            exit_code: None,
            master: pair.master,
            input,
            subscribers: HashSet::from([client]),
        },
    );
    log(&format!("client {client} spawned {session_id} (shell {pid}, in {})", st.sessions[&session_id].cwd));

    let (shared, id) = (shared.clone(), session_id.clone());
    thread::Builder::new()
        .name("ptyd-reader".into())
        .spawn(move || {
            let mut buf = [0u8; 16 * 1024];
            // Bytes of a UTF-8 character split across two reads.
            let mut carry: Vec<u8> = Vec::new();
            loop {
                let n = match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    Err(_) => break,
                };
                carry.extend_from_slice(&buf[..n]);
                let valid = match std::str::from_utf8(&carry) {
                    Ok(_) => carry.len(),
                    Err(e) if e.error_len().is_none() => e.valid_up_to(),
                    // Not UTF-8 at all: show it lossily rather than stall.
                    Err(_) => carry.len(),
                };
                let text = String::from_utf8_lossy(&carry[..valid]).into_owned();
                carry.drain(..valid);
                if text.is_empty() {
                    continue;
                }
                let mut st = lock(&shared);
                let Some(s) = st.sessions.get_mut(&id) else { break };
                s.ring.push(&text);
                s.last_output_at = unix_ms();
                st.broadcast(&id, &json!({ "event": "data", "session_id": id, "data": text }));
            }
            let code = child.wait().map_or(1, |s| s.exit_code());
            log(&format!("{id}: shell {pid} exited with {code}"));
            let mut st = lock(&shared);
            if let Some(s) = st.sessions.get_mut(&id) {
                s.exit_code = Some(code);
                st.broadcast(&id, &json!({ "event": "exit", "session_id": id }));
            }
        })
        .map_err(|e| e.to_string())?;
    Ok((session_id, pid, st))
}

pub fn default_shell() -> String {
    std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/zsh".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_evicts_whole_oldest_chunks() {
        let mut ring = Ring::new(10);
        for chunk in ["aaaa", "bbbb", "cccc"] {
            ring.push(chunk);
        }
        assert_eq!(ring.all(), "bbbbcccc");
        // The newest chunk stays even when it alone is over the cap.
        ring.push("dddddddddddd");
        assert_eq!(ring.all(), "dddddddddddd");
    }

    #[test]
    fn ring_drops_what_a_scrollback_wipe_threw_away() {
        let mut ring = Ring::new(100);
        ring.push("old");
        ring.push("x\x1b[3Jnew");
        assert_eq!(ring.all(), "\x1b[3Jnew");
    }

    #[test]
    fn replay_turns_on_the_input_modes_a_wipe_dropped() {
        let mut ring = Ring::new(100);
        // omp's start: bracketed paste and focus events, the second split
        // across chunks; the alternate screen is no input mode.
        ring.push("\x1b[?2004h\x1b[?10");
        ring.push("04h\x1b[?1049h");
        ring.push("\x1b[2J\x1b[3Jredrawn");
        assert_eq!(ring.all(), "\x1b[?1004h\x1b[?2004h\x1b[3Jredrawn");
        ring.push("\x1b[?1004;1l");
        assert_eq!(ring.all(), "\x1b[?2004h\x1b[3Jredrawn\x1b[?1004;1l");
        ring.push("\x1bcfresh");
        assert_eq!(ring.all(), "\x1bcfresh");
    }

    #[test]
    fn frames_round_trip_and_stop_at_the_end() {
        let msg = json!({ "type": "write", "data": "é\n" });
        let mut bytes = encode(&msg);
        bytes.extend(encode(&json!({ "n": 2 })));
        let mut r = bytes.as_slice();
        assert_eq!(read_frame(&mut r).unwrap(), msg);
        assert_eq!(read_frame(&mut r).unwrap()["n"], 2);
        assert!(read_frame(&mut r).is_err());
    }

    /// A shell of its own session, as a pty spawns one, that sets `traps`
    /// and then runs `command`; returned once it got that far.
    fn session_running(traps: &str, command: &str) -> std::process::Child {
        use std::os::unix::process::CommandExt;
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let ready = std::env::temp_dir().join(format!("ptyd-test-{}-{}", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)));
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.arg("-c").arg(format!("{traps} : > \"$0\"; {command}")).arg(&ready);
        // SAFETY: `setsid` is async-signal-safe, as `pre_exec` needs.
        unsafe { cmd.pre_exec(|| if libc::setsid() == -1 { Err(io::Error::last_os_error()) } else { Ok(()) }) };
        let child = cmd.spawn().unwrap();
        let start = Instant::now();
        while !ready.exists() {
            assert!(start.elapsed() < Duration::from_secs(5), "the shell never started");
            thread::sleep(Duration::from_millis(5));
        }
        let _ = std::fs::remove_file(&ready);
        child
    }

    /// How long `child` took to end after `end_session`, if it did.
    fn ended_after(child: &mut std::process::Child, within: Duration) -> Option<Duration> {
        let start = Instant::now();
        while start.elapsed() < within {
            if child.try_wait().unwrap().is_some() {
                return Some(start.elapsed());
            }
            thread::sleep(Duration::from_millis(10));
        }
        None
    }

    #[test]
    fn a_session_that_honors_the_hangup_ends_at_once() {
        let mut shell = session_running("", "sleep 60");
        end_session(session_groups(shell.id(), None));
        let took = ended_after(&mut shell, Duration::from_secs(5)).expect("the hangup ended nothing");
        assert!(took < HANGUP_GRACE, "{took:?}");
    }

    #[test]
    fn a_session_that_ignores_the_hangup_is_terminated() {
        let mut shell = session_running("trap '' HUP;", "sleep 60");
        end_session(session_groups(shell.id(), None));
        let took = ended_after(&mut shell, Duration::from_secs(5)).expect("the terminate ended nothing");
        assert!(took >= HANGUP_GRACE, "{took:?}");
    }

    #[test]
    fn a_session_that_ignores_hangup_and_terminate_is_killed() {
        let mut shell = session_running("trap '' HUP TERM;", "sleep 60");
        end_session(session_groups(shell.id(), None));
        let took = ended_after(&mut shell, Duration::from_secs(8)).expect("nothing ended it");
        assert!(took >= HANGUP_GRACE + TERMINATE_GRACE, "{took:?}");
    }

    #[test]
    fn only_groups_of_the_session_are_named() {
        let mut shell = session_running("", "sleep 60");
        // SAFETY: `getpgrp` takes nothing.
        let ours = unsafe { libc::getpgrp() };
        assert_eq!(session_groups(shell.id(), Some(ours)), [shell.id() as i32]);
        assert_eq!(session_groups(shell.id(), Some(shell.id() as i32)), [shell.id() as i32]);
        shell.kill().unwrap();
        shell.wait().unwrap();
    }

    /// The shell of a real pty, an agent running in front of it that ignores
    /// the hangup: the group in front is found through the master, and both end.
    #[test]
    fn the_agent_in_front_of_a_pty_ends_with_its_shell() {
        let pair = native_pty_system().openpty(PtySize { rows: 24, cols: 80, pixel_width: 0, pixel_height: 0 }).unwrap();
        let mut cmd = CommandBuilder::new("/bin/sh");
        cmd.arg("-i");
        let mut child = pair.slave.spawn_command(cmd).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        // Drained, or a full pty would stall the shell.
        thread::spawn(move || while reader.read(&mut [0u8; 4096]).map_or(false, |n| n > 0) {});
        let mut writer = pair.master.take_writer().unwrap();
        writer.write_all(b"trap '' HUP; sleep 61\n").unwrap();
        let shell = child.process_id().unwrap();
        let start = Instant::now();
        let front = loop {
            match pair.master.process_group_leader() {
                Some(g) if g != shell as i32 => break g,
                _ => assert!(start.elapsed() < Duration::from_secs(5), "no job ever took the tty"),
            }
            thread::sleep(Duration::from_millis(20));
        };
        let groups = session_groups(shell, Some(front));
        assert_eq!(groups, [shell as i32, front]);
        end_session(groups.clone());
        let start = Instant::now();
        while !groups_gone(&groups) {
            assert!(start.elapsed() < Duration::from_secs(5), "the agent outlived its session");
            // The shell is a zombie until waited for.
            let _ = child.try_wait();
            thread::sleep(Duration::from_millis(20));
        }
        drop(writer);
    }
}
