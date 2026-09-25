//! The local control port: an operator's commands into the running loop.
//!
//! Everything worth controlling lives in this process's memory — the kill
//! switch, the orders the trader believes resting, the reconciliation it
//! last ran — so a separate process could only read the journal after the
//! fact. The port lets something on the same host ask for that state and
//! give the few commands that are safe to give from outside.
//!
//! # Commands are events
//!
//! The loop calls [`Control::poll`] once an iteration and acts on what
//! arrived there, inside the loop, exactly as it acts on a fill. Nothing
//! runs on another thread. So a simulation can script "the operator halts
//! at minute twelve" through [`Environment`](crate::env::Environment) and
//! assert on what follows, and production and simulation run the same
//! code for it.
//!
//! # Protocol
//!
//! One request per connection, one line, tab-separated:
//!
//! ```text
//! <command>\t<origin>\t<reason>
//! ```
//!
//! `command` is `status`, `orders`, `metrics`, `halt`, `shutdown` or
//! `resume`. `origin` is who is asking as the caller authenticated them
//! (for the agent: the deck user and step-up credential); the port adds
//! the peer's uid, which it read from the kernel, and records both.
//! `reason` is required for the three that change anything. The answer is
//! one line of JSON, and the connection closes.
//!
//! No JSON parser on the way in and no dependency for the way out: the
//! request is three fields, and [`Json`] writes the answer.
//!
//! # Who may connect
//!
//! Two locks, neither trusted alone. The socket lives in the service's
//! runtime directory (systemd's `RuntimeDirectory=`), group `oq-ctl`,
//! mode 0660; and every connection's peer uid is read with `SO_PEERCRED`
//! and must be this process's own or one named in `OQ_CONTROL_PEERS`.
//! Without a runtime directory there is no port — never a fallback into
//! `/tmp`, where anyone can create the name first.
//!
//! # Failure never stops trading
//!
//! Any error on a connection closes that connection and is printed. A port
//! that cannot be created leaves the process trading without one.

use core::fmt::Write as _;

/// What an operator asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Identity, halt state, positions, orders, feed and reconciliation.
    Status,
    /// The orders believed resting.
    Orders,
    /// The Prometheus text the heartbeat's numbers come from.
    Metrics,
    /// Stop opening, withdraw opening orders, keep the closing ones.
    Halt(String),
    /// Withdraw everything and exit, as on SIGTERM, and do not come back.
    Shutdown(String),
    /// Clear the kill switch, if allowed and if the books agree.
    Resume(String),
}

impl Command {
    /// Whether this command changes anything.
    #[must_use]
    pub const fn mutates(&self) -> bool {
        matches!(self, Self::Halt(_) | Self::Shutdown(_) | Self::Resume(_))
    }

    /// The command's name, as it is written in the journal.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Orders => "orders",
            Self::Metrics => "metrics",
            Self::Halt(_) => "halt",
            Self::Shutdown(_) => "shutdown",
            Self::Resume(_) => "resume",
        }
    }

    /// The operator's reason, for the commands that carry one.
    #[must_use]
    pub fn reason(&self) -> &str {
        match self {
            Self::Halt(r) | Self::Shutdown(r) | Self::Resume(r) => r,
            _ => "",
        }
    }
}

/// One request, as the loop receives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// Answer with this.
    pub id: u64,
    pub command: Command,
    /// Who asked: the caller's authenticated identity and the peer uid.
    pub origin: String,
}

/// Where requests come from and answers go.
pub trait Control {
    /// Requests that arrived since the last call. Never blocks.
    fn poll(&mut self) -> Vec<Request>;
    /// Answer one request with a line of JSON.
    fn answer(&mut self, id: u64, reply: &str);
}

/// The longest request line accepted.
pub const MAX_REQUEST: usize = 4096;

/// Read one request line.
///
/// # Errors
/// An unknown command, a missing field, or a state-changing command with
/// no reason — a halt with nobody saying why is the log line an operator
/// cannot act on later.
pub fn parse(line: &str) -> Result<(Command, String), String> {
    let line = line.trim_end_matches(['\r', '\n']);
    let mut fields = line.splitn(3, '\t');
    let name = fields.next().unwrap_or_default().trim();
    let origin = fields.next().unwrap_or_default().trim().to_string();
    let reason = fields.next().unwrap_or_default().trim().to_string();
    let needs_reason = |make: fn(String) -> Command| {
        if reason.is_empty() {
            Err(format!("{name} needs a reason"))
        } else {
            Ok(make(reason.clone()))
        }
    };
    let command = match name {
        "status" => Command::Status,
        "orders" => Command::Orders,
        "metrics" => Command::Metrics,
        "halt" => needs_reason(Command::Halt)?,
        "shutdown" => needs_reason(Command::Shutdown)?,
        "resume" => needs_reason(Command::Resume)?,
        other => return Err(format!("unknown command {other:?}")),
    };
    Ok((command, origin))
}

/// A minimal JSON writer: objects, arrays, strings, numbers, booleans.
///
/// Enough for the answers here, and a string is escaped the one way JSON
/// requires, so nothing a venue or a strategy says can break the line.
#[derive(Debug, Default)]
pub struct Json {
    out: String,
    /// Whether the container being written already has a member.
    comma: Vec<bool>,
}

impl Json {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn separate(&mut self) {
        if let Some(has) = self.comma.last_mut() {
            if *has {
                self.out.push(',');
            }
            *has = true;
        }
    }

    fn key(&mut self, key: &str) {
        self.separate();
        escape(&mut self.out, key);
        self.out.push(':');
        // The value that follows is this member, not a new one.
        if let Some(has) = self.comma.last_mut() {
            *has = false;
        }
    }

    fn after_value(&mut self) {
        if let Some(has) = self.comma.last_mut() {
            *has = true;
        }
    }

    pub fn begin_object(&mut self) -> &mut Self {
        self.separate();
        self.out.push('{');
        self.comma.push(false);
        self
    }

    pub fn end_object(&mut self) -> &mut Self {
        self.comma.pop();
        self.out.push('}');
        self.after_value();
        self
    }

    pub fn begin_array(&mut self) -> &mut Self {
        self.separate();
        self.out.push('[');
        self.comma.push(false);
        self
    }

    pub fn end_array(&mut self) -> &mut Self {
        self.comma.pop();
        self.out.push(']');
        self.after_value();
        self
    }

    /// Start a member whose value is an object or array.
    pub fn field(&mut self, key: &str) -> &mut Self {
        self.key(key);
        self
    }

    pub fn str(&mut self, key: &str, value: &str) -> &mut Self {
        self.key(key);
        escape(&mut self.out, value);
        self.after_value();
        self
    }

    pub fn opt_str(&mut self, key: &str, value: Option<&str>) -> &mut Self {
        match value {
            Some(v) => self.str(key, v),
            None => self.null(key),
        }
    }

    pub fn int(&mut self, key: &str, value: i64) -> &mut Self {
        self.key(key);
        let _ = write!(self.out, "{value}");
        self.after_value();
        self
    }

    pub fn uint(&mut self, key: &str, value: u64) -> &mut Self {
        self.key(key);
        let _ = write!(self.out, "{value}");
        self.after_value();
        self
    }

    pub fn bool(&mut self, key: &str, value: bool) -> &mut Self {
        self.key(key);
        self.out.push_str(if value { "true" } else { "false" });
        self.after_value();
        self
    }

    pub fn null(&mut self, key: &str) -> &mut Self {
        self.key(key);
        self.out.push_str("null");
        self.after_value();
        self
    }

    /// A string as an array element.
    pub fn item_str(&mut self, value: &str) -> &mut Self {
        self.separate();
        escape(&mut self.out, value);
        self
    }

    #[must_use]
    pub fn finish(self) -> String {
        self.out
    }
}

fn escape(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// An error answer.
#[must_use]
pub fn refusal(why: &str) -> String {
    let mut j = Json::new();
    j.begin_object()
        .bool("ok", false)
        .str("error", why)
        .end_object();
    j.finish()
}

/// Requests delivered at chosen moments, for a simulation.
///
/// Each line is given to the loop the first time it polls at or after the
/// line's time, on whatever clock the simulation keeps. Answers are kept,
/// in order, for the test to read.
#[derive(Default)]
pub struct Scripted {
    pending: std::collections::VecDeque<(std::time::Duration, String)>,
    next_id: u64,
    answers: std::rc::Rc<std::cell::RefCell<Vec<(String, String)>>>,
    asked: std::collections::BTreeMap<u64, String>,
    now: Option<Box<dyn Fn() -> std::time::Duration>>,
}

impl core::fmt::Debug for Scripted {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Scripted")
            .field("pending", &self.pending.len())
            .finish_non_exhaustive()
    }
}

impl Scripted {
    /// `script` in time order; `now` reads the simulation's clock.
    pub fn new(
        script: Vec<(std::time::Duration, String)>,
        now: Box<dyn Fn() -> std::time::Duration>,
        answers: std::rc::Rc<std::cell::RefCell<Vec<(String, String)>>>,
    ) -> Self {
        Self {
            pending: script.into(),
            next_id: 0,
            answers,
            asked: std::collections::BTreeMap::new(),
            now: Some(now),
        }
    }
}

impl Control for Scripted {
    fn poll(&mut self) -> Vec<Request> {
        let now = self.now.as_ref().map_or(std::time::Duration::MAX, |f| f());
        let mut out = Vec::new();
        while self.pending.front().is_some_and(|(at, _)| *at <= now) {
            let (_, line) = self.pending.pop_front().unwrap_or_default();
            self.next_id += 1;
            match parse(&line) {
                Ok((command, origin)) => {
                    self.asked.insert(self.next_id, line);
                    out.push(Request {
                        id: self.next_id,
                        command,
                        origin: format!("{origin} (scripted)"),
                    });
                }
                Err(why) => self.answers.borrow_mut().push((line, refusal(&why))),
            }
        }
        out
    }

    fn answer(&mut self, id: u64, reply: &str) {
        let line = self.asked.remove(&id).unwrap_or_default();
        self.answers.borrow_mut().push((line, reply.to_string()));
    }
}

#[cfg(unix)]
pub use socket::Socket;

#[cfg(unix)]
mod socket {
    use super::{Control, MAX_REQUEST, Request, parse, refusal};
    use std::io::{ErrorKind, Read, Write};
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    /// Requests handled per poll, so a flood cannot starve the loop.
    const PER_POLL: usize = 8;
    /// A connection that has not sent its line in this long is closed.
    const IDLE: Duration = Duration::from_secs(10);

    struct Pending {
        stream: UnixStream,
        buf: Vec<u8>,
        since: Instant,
        uid: u32,
    }

    /// The production port: a Unix socket in the service's runtime
    /// directory.
    pub struct Socket {
        path: PathBuf,
        listener: UnixListener,
        allowed: Vec<u32>,
        reading: Vec<Pending>,
        waiting: std::collections::BTreeMap<u64, UnixStream>,
        next_id: u64,
    }

    impl core::fmt::Debug for Socket {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.debug_struct("Socket").field("path", &self.path).finish()
        }
    }

    impl Socket {
        /// Open `<runtime dir>/<name>.sock`.
        ///
        /// The runtime directory is `$RUNTIME_DIRECTORY`, which systemd
        /// sets for `RuntimeDirectory=`; nothing else. Peers allowed are
        /// this process's uid and the users named, comma-separated, in
        /// `$OQ_CONTROL_PEERS`.
        ///
        /// # Errors
        /// No runtime directory, a peer name that does not resolve, or the
        /// socket could not be created.
        pub fn open(name: &str) -> Result<Self, String> {
            let dir = std::env::var_os("RUNTIME_DIRECTORY")
                .map(PathBuf::from)
                .ok_or("no RUNTIME_DIRECTORY; the port needs a private runtime directory")?;
            // systemd may list several, colon-separated; the first is ours.
            let dir = PathBuf::from(
                dir.to_string_lossy()
                    .split(':')
                    .next()
                    .unwrap_or_default()
                    .to_string(),
            );
            let mut peers = Vec::new();
            if let Ok(names) = std::env::var("OQ_CONTROL_PEERS") {
                for user in names.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                    peers.push(uid_of(user)?);
                }
            }
            let mut port = Self::bind(&dir.join(format!("{name}.sock")), peers)?;
            // This process's own uid, as the owner of the socket it just
            // made: the one uid that is always allowed.
            let own = std::fs::metadata(&port.path)
                .map_err(|e| format!("{}: {e}", port.path.display()))?
                .uid();
            port.allowed.push(own);
            Ok(port)
        }

        /// Bind at `path`, accepting the uids given.
        ///
        /// # Errors
        /// The socket could not be created.
        pub fn bind(path: &Path, allowed: Vec<u32>) -> Result<Self, String> {
            // A socket left by this service's previous run. The directory
            // is private to the service and the interlock is held, so the
            // name can only be ours; anything that is not a socket is left
            // alone and refused.
            if let Ok(meta) = std::fs::symlink_metadata(path) {
                use std::os::unix::fs::FileTypeExt;
                if meta.file_type().is_socket() {
                    let _ = std::fs::remove_file(path);
                } else {
                    return Err(format!("{} exists and is not a socket", path.display()));
                }
            }
            let listener =
                UnixListener::bind(path).map_err(|e| format!("{}: {e}", path.display()))?;
            listener
                .set_nonblocking(true)
                .map_err(|e| format!("{}: {e}", path.display()))?;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))
                .map_err(|e| format!("{}: {e}", path.display()))?;
            Ok(Self {
                path: path.to_path_buf(),
                listener,
                allowed,
                reading: Vec::new(),
                waiting: std::collections::BTreeMap::new(),
                next_id: 0,
            })
        }

        /// Where it listens, for the startup banner.
        #[must_use]
        pub fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for Socket {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    /// A local user's uid, from `/etc/passwd`.
    ///
    /// The peers named here are system users created on this host, which
    /// live in that file; reading it needs no libc and no directory
    /// service.
    fn uid_of(user: &str) -> Result<u32, String> {
        let passwd =
            std::fs::read_to_string("/etc/passwd").map_err(|e| format!("/etc/passwd: {e}"))?;
        passwd
            .lines()
            .map(|l| l.split(':').collect::<Vec<_>>())
            .find(|f| f.first() == Some(&user))
            .and_then(|f| f.get(2).and_then(|u| u.parse().ok()))
            .ok_or_else(|| format!("OQ_CONTROL_PEERS names {user:?}, which is not a user here"))
    }

    impl Control for Socket {
        fn poll(&mut self) -> Vec<Request> {
            // New connections, checked before a byte is read from them.
            for _ in 0..PER_POLL {
                match self.listener.accept() {
                    Ok((stream, _)) => {
                        let Some(uid) = oq_l2feed::peer::uid(&stream) else {
                            eprintln!(
                                "control          refused a peer whose uid could not be read"
                            );
                            continue;
                        };
                        if !self.allowed.contains(&uid) {
                            eprintln!("control          refused uid {uid}");
                            let mut s = stream;
                            let _ = writeln!(s, "{}", refusal("this uid may not use the port"));
                            continue;
                        }
                        if stream.set_nonblocking(true).is_ok() {
                            self.reading.push(Pending {
                                stream,
                                buf: Vec::new(),
                                since: Instant::now(),
                                uid,
                            });
                        }
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(e) => {
                        eprintln!("control          accept failed: {e}");
                        break;
                    }
                }
            }

            let mut out = Vec::new();
            let mut keep = Vec::new();
            for mut p in self.reading.drain(..) {
                let mut chunk = [0_u8; 512];
                let mut closed = false;
                loop {
                    match p.stream.read(&mut chunk) {
                        Ok(0) => {
                            closed = true;
                            break;
                        }
                        Ok(n) => {
                            p.buf.extend_from_slice(&chunk[..n]);
                            if p.buf.len() > MAX_REQUEST || p.buf.contains(&b'\n') {
                                break;
                            }
                        }
                        Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                        Err(_) => {
                            closed = true;
                            break;
                        }
                    }
                }
                if p.buf.len() > MAX_REQUEST {
                    let _ = writeln!(p.stream, "{}", refusal("request too long"));
                    continue;
                }
                let complete = p.buf.contains(&b'\n') || (closed && !p.buf.is_empty());
                if !complete {
                    if !closed && p.since.elapsed() < IDLE {
                        keep.push(p);
                    }
                    continue;
                }
                let line = String::from_utf8_lossy(&p.buf).to_string();
                let line = line.split('\n').next().unwrap_or_default().to_string();
                match parse(&line) {
                    Ok((command, origin)) => {
                        self.next_id += 1;
                        let id = self.next_id;
                        out.push(Request {
                            id,
                            command,
                            origin: format!("uid {} {origin}", p.uid).trim_end().to_string(),
                        });
                        self.waiting.insert(id, p.stream);
                    }
                    Err(why) => {
                        let _ = writeln!(p.stream, "{}", refusal(&why));
                    }
                }
            }
            self.reading = keep;
            out
        }

        fn answer(&mut self, id: u64, reply: &str) {
            if let Some(mut stream) = self.waiting.remove(&id) {
                // The answer is small; a peer that is not reading gets
                // what fits and is closed, rather than stalling the loop.
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_write_timeout(Some(Duration::from_millis(200)));
                let _ = writeln!(stream, "{reply}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_line_reads_as_command_origin_and_reason() {
        assert_eq!(parse("status\t\t\n"), Ok((Command::Status, String::new())));
        assert_eq!(
            parse("halt\tdeck alice totp\tfeed looks wrong"),
            Ok((
                Command::Halt("feed looks wrong".into()),
                "deck alice totp".into()
            ))
        );
        assert_eq!(parse("orders"), Ok((Command::Orders, String::new())));
    }

    #[test]
    fn a_change_without_a_reason_is_refused() {
        for name in ["halt", "shutdown", "resume"] {
            assert!(parse(&format!("{name}\torigin\t  ")).is_err(), "{name}");
        }
        assert!(parse("liquidate\t\tnow").is_err());
    }

    #[test]
    fn json_escapes_whatever_it_is_given() {
        let mut j = Json::new();
        j.begin_object()
            .str("a", "quote \" slash \\ line\nend\u{1}")
            .int("n", -3)
            .field("xs")
            .begin_array()
            .begin_object()
            .bool("t", true)
            .end_object()
            .begin_object()
            .null("z")
            .end_object()
            .end_array()
            .uint("u", 7)
            .end_object();
        assert_eq!(
            j.finish(),
            r#"{"a":"quote \" slash \\ line\nend\u0001","n":-3,"xs":[{"t":true},{"z":null}],"u":7}"#
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_socket_answers_an_allowed_peer_and_refuses_another() {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixStream;
        let dir = std::env::temp_dir().join(format!("oq-control-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("t.sock");

        let port = Socket::bind(&path, Vec::new()).expect("binds");
        let me = {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(&path).expect("socket").uid()
        };
        drop(port);
        let mut port = Socket::bind(&path, vec![me]).expect("binds");
        let mut client = UnixStream::connect(&path).expect("connects");
        client
            .write_all(b"halt\ttester\tchecking\n")
            .expect("writes");
        let mut got = Vec::new();
        for _ in 0..50 {
            got = port.poll();
            if !got.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].command, Command::Halt("checking".into()));
        assert!(
            got[0].origin.starts_with(&format!("uid {me} tester")),
            "{}",
            got[0].origin
        );
        port.answer(got[0].id, r#"{"ok":true}"#);
        let mut line = String::new();
        BufReader::new(&client).read_line(&mut line).expect("reads");
        assert_eq!(line.trim(), r#"{"ok":true}"#);

        // The same socket, allowing nobody who is running this test.
        drop(port);
        let mut port = Socket::bind(&path, vec![me.wrapping_add(1)]).expect("binds");
        let client = UnixStream::connect(&path).expect("connects");
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(port.poll().is_empty(), "a peer not allowed reaches nothing");
        let mut line = String::new();
        BufReader::new(&client).read_line(&mut line).expect("reads");
        assert!(line.contains("may not use the port"), "{line}");

        drop(port);
        assert!(!path.exists(), "the socket goes with the port");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn something_that_is_not_a_socket_is_never_removed() {
        let dir = std::env::temp_dir().join(format!("oq-control-f-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("t.sock");
        std::fs::write(&path, "a file").expect("file");
        assert!(Socket::bind(&path, vec![]).is_err());
        assert_eq!(
            std::fs::read_to_string(&path).expect("still there"),
            "a file"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
