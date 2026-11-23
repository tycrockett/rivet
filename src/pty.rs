//! Pseudo-terminals for the shell's terminals, held by the pty daemon
//! (`ptyd.rs`, Bolt's `src/ptyDaemonClient.js`) so they outlive the app.
//! One connection per process, started on first use (the daemon too, if
//! none runs); a reader thread hands each session's output to the engine
//! through a channel of its own. The daemon reporting that the shell exited
//! is an [`Output::Exit`] on it; a channel that merely closes is a lost
//! connection, never an exit, and nothing may end a shell on its account.
//! Dropping a [`Pty`] leaves the shell running: only [`Pty::kill`] ends it,
//! and quitting just disconnects.

use std::{
    collections::HashMap,
    io::{self, Write},
    os::unix::{net::UnixStream, process::CommandExt},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use serde_json::{Value, json};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::ptyd::{PROTOCOL_VERSION, encode, pid_path, read_frame, socket_path};

/// How long a request waits for the daemon.
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// What a session's channel carries.
#[derive(Debug, PartialEq, Eq)]
pub enum Output {
    /// Text the shell printed.
    Data(String),
    /// The daemon says the shell exited: the last thing the channel gets.
    Exit,
}

/// A request in flight: where its reply goes, and the output channel to
/// install the moment it arrives, so the first bytes after a spawn or an
/// attach find their way (the daemon sends them right behind the reply).
struct Waiting {
    reply: mpsc::Sender<Value>,
    output: Option<UnboundedSender<Output>>,
}

struct Conn {
    writer: Mutex<UnixStream>,
    next_id: AtomicU64,
    waiting: Mutex<HashMap<u64, Waiting>>,
    /// Each attached session's views: a session taken up twice by this
    /// process feeds both, and an earlier view is never cut off by a later
    /// one.
    sessions: Mutex<HashMap<String, Vec<UnboundedSender<Output>>>>,
    /// The daemon went away; the next [`conn`] connects again.
    dead: AtomicBool,
}

fn locked<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Conn {
    fn open(stream: UnixStream) -> io::Result<Arc<Conn>> {
        let mut reader = stream.try_clone()?;
        let conn = Arc::new(Conn {
            writer: Mutex::new(stream),
            next_id: AtomicU64::new(0),
            waiting: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            dead: AtomicBool::new(false),
        });
        let c = conn.clone();
        thread::Builder::new().name("pty-client".into()).spawn(move || {
            while let Ok(msg) = read_frame(&mut reader) {
                c.dispatch(msg);
            }
            // The output channels stay open: the shells still run in the
            // daemon, and a terminal that read a closed channel as its shell
            // exiting would close its tab and kill it. They go quiet until
            // the next launch attaches again.
            eprintln!("rivet: lost the pty daemon; terminals stop updating until the next launch");
            c.dead.store(true, Ordering::SeqCst);
            locked(&c.waiting).clear();
        })?;
        Ok(conn)
    }

    fn dispatch(&self, msg: Value) {
        if let Some(id) = msg["id"].as_u64() {
            let Some(w) = locked(&self.waiting).remove(&id) else { return };
            if let (Some(out), Some(session)) = (w.output, msg["session_id"].as_str()) {
                if msg["ok"] == true {
                    locked(&self.sessions).entry(session.to_owned()).or_default().push(out);
                }
            }
            let _ = w.reply.send(msg);
            return;
        }
        let Some(session) = msg["session_id"].as_str() else { return };
        let mut sessions = locked(&self.sessions);
        match msg["event"].as_str() {
            Some("data") => {
                let Some(views) = sessions.get_mut(session) else { return };
                let data = msg["data"].as_str().unwrap_or_default().to_owned();
                send_to_views(views, data);
                if views.is_empty() {
                    sessions.remove(session);
                }
            }
            Some("exit") => {
                for tx in sessions.remove(session).into_iter().flatten() {
                    let _ = tx.send(Output::Exit);
                }
            }
            _ => {}
        }
    }

    /// For requests the daemon does not answer (`write`, `resize`).
    fn send(&self, msg: &Value) {
        if self.dead.load(Ordering::SeqCst) {
            return;
        }
        let _ = locked(&self.writer).write_all(&encode(msg));
    }

    fn request(&self, mut msg: Value, output: Option<UnboundedSender<Output>>) -> io::Result<Value> {
        if self.dead.load(Ordering::SeqCst) {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "the pty daemon is gone"));
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        msg["id"] = id.into();
        let (reply, answer) = mpsc::channel();
        locked(&self.waiting).insert(id, Waiting { reply, output });
        if let Err(e) = locked(&self.writer).write_all(&encode(&msg)) {
            locked(&self.waiting).remove(&id);
            return Err(e);
        }
        let reply = answer.recv_timeout(REPLY_TIMEOUT).map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "the pty daemon did not answer"))?;
        if reply["ok"] == true {
            Ok(reply)
        } else {
            Err(io::Error::other(reply["error"].as_str().unwrap_or("the pty daemon refused").to_owned()))
        }
    }
}

/// Sends `data` to every view of a session, dropping the views whose
/// receiver is gone. The last one gets `data` itself, so the usual single
/// view costs no copy.
fn send_to_views(views: &mut Vec<UnboundedSender<Output>>, data: String) {
    let mut data = Some(data);
    let mut left = views.len();
    views.retain(|tx| {
        left -= 1;
        let chunk = if left == 0 { data.take() } else { data.clone() };
        tx.send(Output::Data(chunk.unwrap_or_default())).is_ok()
    });
}

static CONN: Mutex<Option<Arc<Conn>>> = Mutex::new(None);

/// The process's connection, made (and the daemon started) on first use,
/// and again after the daemon died.
fn conn() -> io::Result<Arc<Conn>> {
    let mut slot = locked(&CONN);
    if let Some(c) = slot.as_ref().filter(|c| !c.dead.load(Ordering::SeqCst)) {
        return Ok(c.clone());
    }
    let c = connect()?;
    *slot = Some(c.clone());
    Ok(c)
}

/// A connection to a daemon of this [`PROTOCOL_VERSION`]. A daemon of
/// another one is stopped and replaced, which ends its sessions: the only
/// time an app launch does.
fn connect() -> io::Result<Arc<Conn>> {
    for _ in 0..3 {
        let conn = Conn::open(dial()?)?;
        match conn.request(json!({ "type": "hello", "version": PROTOCOL_VERSION }), None) {
            Ok(hello) if hello["version"] == PROTOCOL_VERSION => return Ok(conn),
            Ok(hello) => {
                if let Some(pid) = hello["pid"].as_u64() {
                    stop_daemon(pid as i32);
                }
            }
            // It went away between accepting and answering (an idle exit).
            Err(_) => {}
        }
    }
    Err(io::Error::other("could not reach a pty daemon"))
}

/// Connects to the daemon's socket, starting the daemon first if nothing
/// answers there.
fn dial() -> io::Result<UnixStream> {
    let socket = socket_path();
    if let Ok(stream) = UnixStream::connect(&socket) {
        return Ok(stream);
    }
    start_daemon()?;
    let mut last = io::Error::other("the pty daemon did not start");
    for _ in 0..60 {
        thread::sleep(Duration::from_millis(50));
        match UnixStream::connect(&socket) {
            Ok(stream) => return Ok(stream),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// The executable as the daemon runs it: a hard link to this one named
/// `rvt-ptyd` in the data directory (a copy across volumes), so it shows
/// and is matched as `rvt-ptyd`. Stopping the app by name (`pkill rivet`,
/// `killall`) must not stop the terminals with it, which is why the name
/// does not contain `rivet`. Made again when the app was rebuilt.
fn daemon_exe() -> io::Result<std::path::PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let exe = std::env::current_exe()?;
    let dir = crate::settings::data_dir();
    std::fs::create_dir_all(&dir)?;
    let link = dir.join("rvt-ptyd");
    let want = std::fs::metadata(&exe)?;
    // The same file, or a copy made after this build.
    let current = std::fs::metadata(&link).is_ok_and(|have| {
        (have.dev(), have.ino()) == (want.dev(), want.ino()) || (have.len() == want.len() && have.mtime() >= want.mtime())
    });
    if !current {
        let _ = std::fs::remove_file(&link);
        if std::fs::hard_link(&exe, &link).is_err() {
            std::fs::copy(&exe, &link)?;
        }
    }
    Ok(link)
}

/// Starts the daemon apart from the app: `sh`, in a session of its own,
/// puts `rvt-ptyd --ptyd` in the background and exits, so the daemon's
/// parent is launchd from the start. Nothing that ends the app, its
/// process group or its process tree reaches it.
fn start_daemon() -> io::Result<()> {
    // The daemon's way in; referring to it keeps it in the executable.
    std::hint::black_box(&crate::ptyd::ENTRY);
    let exe = daemon_exe()?;
    let mut sh = Command::new("/bin/sh");
    sh.arg("-c")
        .arg("\"$0\" --ptyd </dev/null >/dev/null 2>&1 &")
        .arg(&exe)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: `setsid` is async-signal-safe and touches no memory.
    unsafe {
        sh.pre_exec(|| if libc::setsid() == -1 { Err(io::Error::last_os_error()) } else { Ok(()) });
    }
    sh.spawn()?.wait()?;
    Ok(())
}

/// Terminates daemon `pid` and waits for its socket to go quiet.
fn stop_daemon(pid: i32) {
    // SAFETY: `kill` takes no pointers; the pid is the one the daemon reported.
    unsafe { libc::kill(pid, libc::SIGTERM) };
    for _ in 0..60 {
        if UnixStream::connect(socket_path()).is_err() {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let _ = std::fs::remove_file(pid_path());
}

/// A session of the daemon, as `list` reports it.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub session_id: String,
    /// Where it was started: the workspace it belongs to.
    pub cwd: String,
    pub alive: bool,
}

impl SessionInfo {
    fn of(v: &Value) -> Option<SessionInfo> {
        Some(SessionInfo {
            session_id: v["session_id"].as_str()?.to_owned(),
            cwd: v["cwd"].as_str().unwrap_or_default().to_owned(),
            alive: v["alive"].as_bool()?,
        })
    }
}

/// What an attach brings back besides the live output.
pub struct Attached {
    /// Its recent output (the daemon's ring), to draw before what follows.
    pub replay: String,
    pub last_output_at: f64,
}

pub struct Pty {
    conn: Arc<Conn>,
    session_id: String,
    pid: Option<u32>,
}

impl Pty {
    /// Starts the shell in `cwd`; the receiver yields its output.
    pub fn spawn(cwd: &str) -> io::Result<(Pty, UnboundedReceiver<Output>)> {
        let conn = conn()?;
        let (tx, rx) = unbounded_channel();
        let reply = conn.request(json!({ "type": "spawn", "cwd": cwd, "cols": 80, "rows": 24 }), Some(tx))?;
        Ok((Pty::of(conn, &reply), rx))
    }

    /// Takes up session `session_id` again: its replay, then its output as
    /// it comes.
    pub fn attach(session_id: &str) -> io::Result<(Pty, Attached, UnboundedReceiver<Output>)> {
        let conn = conn()?;
        let (tx, rx) = unbounded_channel();
        let reply = conn.request(json!({ "type": "attach", "session_id": session_id }), Some(tx))?;
        let attached = Attached { replay: reply["replay"].as_str().unwrap_or_default().to_owned(), last_output_at: reply["last_output_at"].as_f64().unwrap_or(0.0) };
        Ok((Pty::of(conn, &reply), attached, rx))
    }

    /// Every session the daemon holds, shells that exited included.
    pub fn list() -> io::Result<Vec<SessionInfo>> {
        let reply = conn()?.request(json!({ "type": "list" }), None)?;
        Ok(reply["sessions"].as_array().map(|a| a.iter().filter_map(SessionInfo::of).collect()).unwrap_or_default())
    }

    fn of(conn: Arc<Conn>, reply: &Value) -> Pty {
        Pty { conn, session_id: reply["session_id"].as_str().unwrap_or_default().to_owned(), pid: reply["pid"].as_u64().map(|p| p as u32) }
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn write(&mut self, data: &str) {
        self.conn.send(&json!({ "type": "write", "session_id": self.session_id, "data": data }));
    }

    pub fn resize(&self, cols: u32, rows: u32) {
        self.conn.send(&json!({ "type": "resize", "session_id": self.session_id, "cols": cols, "rows": rows }));
    }

    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// Ends the shell and everything it runs, and forgets the session. Not
    /// waited for: the daemon answers behind whatever it is busy with, and
    /// the tab closing must not wait on that.
    pub fn kill(&self) {
        self.conn.send(&json!({ "type": "kill", "session_id": self.session_id }));
    }
}

/// Drops session `session_id`, whose shell already exited, from the
/// daemon's list (a `kill` of an exited shell only forgets it).
pub fn forget_session(session_id: &str) {
    if let Ok(conn) = conn() {
        conn.send(&json!({ "type": "kill", "session_id": session_id }));
    }
}

#[cfg(test)]
mod tests {
    use std::{io::Write, os::unix::net::UnixStream, time::Instant};

    use tokio::sync::mpsc::error::TryRecvError;

    use super::*;

    /// A connection whose daemon is the test: the other end of the socket.
    fn connection() -> (Arc<Conn>, UnixStream) {
        let (client, daemon) = UnixStream::pair().unwrap();
        (Conn::open(client).unwrap(), daemon)
    }

    /// Answers the next request with `body`, as the daemon would.
    fn answer(daemon: &mut UnixStream, mut body: Value) {
        let request = read_frame(daemon).unwrap();
        body["id"] = request["id"].clone();
        body["ok"] = true.into();
        daemon.write_all(&encode(&body)).unwrap();
    }

    fn event(daemon: &mut UnixStream, kind: &str, session: &str, data: &str) {
        daemon.write_all(&encode(&json!({ "event": kind, "session_id": session, "data": data }))).unwrap();
    }

    /// A session taken up on `conn`, its request answered by `daemon`.
    fn take_up(conn: &Arc<Conn>, daemon: &mut UnixStream, request: Value) -> UnboundedReceiver<Output> {
        let (tx, rx) = unbounded_channel();
        let c = conn.clone();
        let asked = thread::spawn(move || c.request(request, Some(tx)));
        answer(daemon, json!({ "session_id": "pty-1", "pid": 7 }));
        asked.join().unwrap().unwrap();
        rx
    }

    fn next(rx: &mut UnboundedReceiver<Output>) -> Option<Output> {
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            match rx.try_recv() {
                Ok(out) => return Some(out),
                Err(TryRecvError::Disconnected) => return None,
                Err(TryRecvError::Empty) if Instant::now() > until => panic!("nothing came"),
                Err(TryRecvError::Empty) => thread::sleep(Duration::from_millis(5)),
            }
        }
    }

    #[test]
    fn the_daemon_reporting_an_exit_is_an_event_then_the_channel_ends() {
        let (conn, mut daemon) = connection();
        let mut rx = take_up(&conn, &mut daemon, json!({ "type": "spawn" }));
        event(&mut daemon, "data", "pty-1", "hi");
        event(&mut daemon, "exit", "pty-1", "");
        assert_eq!(next(&mut rx), Some(Output::Data("hi".into())));
        assert_eq!(next(&mut rx), Some(Output::Exit));
        assert_eq!(next(&mut rx), None);
    }

    #[test]
    fn a_session_taken_up_again_cuts_off_no_earlier_view() {
        let (conn, mut daemon) = connection();
        let mut first = take_up(&conn, &mut daemon, json!({ "type": "spawn" }));
        let mut second = take_up(&conn, &mut daemon, json!({ "type": "attach", "session_id": "pty-1" }));
        event(&mut daemon, "data", "pty-1", "both");
        event(&mut daemon, "exit", "pty-1", "");
        for rx in [&mut first, &mut second] {
            assert_eq!(next(rx), Some(Output::Data("both".into())));
            assert_eq!(next(rx), Some(Output::Exit));
        }
    }

    #[test]
    fn a_view_that_went_away_does_not_end_the_others() {
        let (conn, mut daemon) = connection();
        let first = take_up(&conn, &mut daemon, json!({ "type": "spawn" }));
        let mut second = take_up(&conn, &mut daemon, json!({ "type": "attach", "session_id": "pty-1" }));
        drop(first);
        event(&mut daemon, "data", "pty-1", "still here");
        assert_eq!(next(&mut second), Some(Output::Data("still here".into())));
    }

    #[test]
    fn a_lost_connection_closes_no_channel() {
        let (conn, daemon) = connection();
        let mut daemon = daemon;
        let mut rx = take_up(&conn, &mut daemon, json!({ "type": "spawn" }));
        drop(daemon);
        let until = Instant::now() + Duration::from_secs(5);
        while !conn.dead.load(Ordering::SeqCst) {
            assert!(Instant::now() < until, "the loss was never noticed");
            thread::sleep(Duration::from_millis(5));
        }
        // Closed would read as the shell having exited.
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
    }
}
