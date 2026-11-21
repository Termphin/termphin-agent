use std::collections::VecDeque;
use std::env;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::{Arc, Mutex};

mod presence;
mod relay;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

// Only the Windows build calls into this, but it is pure byte-shuffling with
// no syscalls, so it is compiled everywhere and its tests run on any host.
#[cfg_attr(not(windows), allow(dead_code))]
mod win32_codec;

pub(crate) const PROTOCOL_VERSION: u32 = 1;
pub(crate) const MAX_FRAME_SIZE: usize = 1024 * 1024;

pub(crate) const REPLAY_CHUNK_SIZE: usize = 128 * 1024;

/// Queued-but-unwritten bytes a single client may accumulate before the master
/// gives up on it. Must comfortably exceed a full history replay.
pub(crate) const MAX_CLIENT_BACKLOG: usize = 8 * 1024 * 1024;

/// Cap on how long a single frame write may block before the client counts as
/// wedged. Only bounds one write; the backlog limit bounds the total.
pub(crate) const CLIENT_WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

pub(crate) const FRAME_ATTACH: u8 = 1;
pub(crate) const FRAME_INPUT: u8 = 2;
pub(crate) const FRAME_RESIZE: u8 = 3;
pub(crate) const FRAME_STATUS: u8 = 4;
pub(crate) const FRAME_RENAME: u8 = 5;
pub(crate) const FRAME_KILL: u8 = 6;
pub(crate) const FRAME_HISTORY: u8 = 7;
/// Asks to continue from a byte offset of the session's output instead of a
/// replay, for a client whose terminal already holds everything before it.
/// Carries an [`OutputPosition`]. Sent before FRAME_ATTACH. Older masters answer with an error and go on to
/// replay as asked, which is also what a newer one does when the offset has
/// left [`OUTPUT_LOG_BYTES`].
#[cfg(unix)]
pub(crate) const FRAME_RESUME: u8 = 8;
/// Client to master: the phone behind this client has gone to the
/// background (payload `[1]`) or come back (`[0]`). iOS freezes a
/// backgrounded app without closing its connection, so to the master it
/// would still look attached; while every client is away, notifications go
/// to the relay as they would with none attached.
pub(crate) const FRAME_PRESENCE: u8 = 9;
/// Which device a client is: what the app said in [`PRESENCE_DEVICE`],
/// checked by [`valid_device`]. Only sent to a master from 0.20.0 on, which
/// the app knows from the master's version marker.
pub(crate) const FRAME_DEVICE: u8 = 10;
/// Who types into the session. Payload [`CONTROL_JOIN`] before
/// FRAME_ATTACH says the client follows control: it attaches as a viewer
/// while another client holds the session, and what it types is dropped
/// until it takes control. [`CONTROL_TAKE`] after FRAME_ATTACH takes it.
/// Older masters answer both with an error the client tolerates.
pub(crate) const FRAME_CONTROL: u8 = 11;
pub(crate) const CONTROL_JOIN: u8 = 0;
pub(crate) const CONTROL_TAKE: u8 = 1;
/// Sent on a control connection, never an attached one, by `take`, `away`
/// and `back`: one of [`DEVICE_TAKE`], [`DEVICE_AWAY`], [`DEVICE_BACK`]
/// followed by an install id, applied to the clients of the device with
/// that id. The way in for an app whose messages typed into the terminal
/// never arrive - a Windows console swallows every OSC in its input.
pub(crate) const FRAME_DEVICE_CONTROL: u8 = 12;
pub(crate) const DEVICE_TAKE: u8 = 0;
pub(crate) const DEVICE_AWAY: u8 = 1;
pub(crate) const DEVICE_BACK: u8 = 2;

pub(crate) const FRAME_OUTPUT: u8 = 101;
pub(crate) const FRAME_STATUS_RESPONSE: u8 = 102;
pub(crate) const FRAME_OK: u8 = 103;
pub(crate) const FRAME_ERROR: u8 = 104;
pub(crate) const FRAME_EXIT: u8 = 105;
/// Sent after the last replay chunk, so the client can tell a complete
/// scrollback from one still arriving. Older clients ignore unknown frames.
pub(crate) const FRAME_REPLAY_DONE: u8 = 106;
/// Sent after the output missed since a FRAME_RESUME position. Like
/// FRAME_REPLAY_DONE, carries the [`OutputPosition`] live output continues
/// from.
#[cfg(unix)]
pub(crate) const FRAME_RESUME_DONE: u8 = 107;
/// Master to a client that joined control: whether it holds the session,
/// the size the session runs at, and the controlling client's device
/// description when it gave one. See [`ControlState`].
pub(crate) const FRAME_CONTROL_STATE: u8 = 108;

pub(crate) const REPLAY_END_MARKER: &[u8] = b"\x1b]5380;termphin-replay-end\x07";
pub(crate) const REBOOT_RESTORED_MARKER: &[u8] = b"\x1b]5381;termphin-reboot-restored\x07";
/// Written instead of [`REPLAY_END_MARKER`] when the attach resumed rather
/// than replayed: what came before it continues the client's own terminal.
#[cfg(unix)]
pub(crate) const RESUME_END_MARKER: &[u8] = b"\x1b]5380;termphin-resume-end\x07";

/// Raw output kept for resuming. Past this a client that was away gets the
/// replay instead.
#[cfg(unix)]
pub(crate) const OUTPUT_LOG_BYTES: usize = 1024 * 1024;

/// Tells the client where the bytes after it start, so it can ask to resume
/// from wherever it stops receiving.
#[cfg(unix)]
pub(crate) fn offset_marker(position: OutputPosition) -> Vec<u8> {
    format!(
        "\x1b]5383;termphin-offset;{}:{}\x07",
        position.epoch, position.offset
    )
    .into_bytes()
}

/// A place in one master's output. The epoch keeps an offset from an earlier
/// master of the same name - one that crashed, or was restored after a
/// reboot - from being read against this one's numbering.
#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OutputPosition {
    pub(crate) epoch: u64,
    pub(crate) offset: u64,
}

#[cfg(unix)]
impl OutputPosition {
    pub(crate) fn encode(self) -> [u8; 16] {
        let mut bytes = [0; 16];
        bytes[..8].copy_from_slice(&self.epoch.to_be_bytes());
        bytes[8..].copy_from_slice(&self.offset.to_be_bytes());
        bytes
    }

    pub(crate) fn decode(payload: &[u8]) -> io::Result<Self> {
        let (epoch, offset) = payload
            .split_first_chunk::<8>()
            .and_then(|(epoch, rest)| Some((epoch, <&[u8; 8]>::try_from(rest).ok()?)))
            .ok_or_else(|| invalid_input("invalid output position"))?;
        Ok(Self {
            epoch: u64::from_be_bytes(*epoch),
            offset: u64::from_be_bytes(*offset),
        })
    }

    /// `<epoch>:<offset>`, as the marker writes it and `--resume` takes it.
    pub(crate) fn parse(value: &str) -> Option<Self> {
        let (epoch, offset) = value.split_once(':')?;
        Some(Self {
            epoch: epoch.parse().ok()?,
            offset: offset.parse().ok()?,
        })
    }
}

/// The session's raw output, the last [`OUTPUT_LOG_BYTES`] of it, numbered
/// from the start of the session.
#[cfg(unix)]
pub(crate) struct OutputLog {
    epoch: u64,
    bytes: VecDeque<u8>,
    total: u64,
    capacity: usize,
}

/// Different for every master. Random, not the clock and pid: a pid comes
/// round again after a reboot, and macOS reads the clock in microseconds.
#[cfg(unix)]
fn new_epoch() -> u64 {
    let mut bytes = [0_u8; 8];
    let random =
        std::fs::File::open("/dev/urandom").and_then(|mut file| file.read_exact(&mut bytes));
    if random.is_err() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos() as u64)
            .unwrap_or(0);
        return nanos ^ (u64::from(process::id()) << 40);
    }
    u64::from_ne_bytes(bytes)
}

#[cfg(unix)]
impl OutputLog {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            epoch: new_epoch(),
            bytes: VecDeque::new(),
            total: 0,
            capacity,
        }
    }

    pub(crate) fn push(&mut self, data: &[u8]) {
        self.total += data.len() as u64;
        let data = &data[data.len().saturating_sub(self.capacity)..];
        let overflow = (self.bytes.len() + data.len()).saturating_sub(self.capacity);
        self.bytes.drain(..overflow);
        self.bytes.extend(data);
    }

    /// The epoch, the count of bytes ever written and the bytes still kept:
    /// what a master hands over so offsets keep their meaning after it.
    pub(crate) fn parts(&self) -> (u64, u64, Vec<u8>) {
        (self.epoch, self.total, self.bytes.iter().copied().collect())
    }

    /// The log [`Self::parts`] described.
    pub(crate) fn from_parts(epoch: u64, total: u64, bytes: &[u8], capacity: usize) -> Self {
        let bytes = &bytes[bytes.len().saturating_sub(capacity)..];
        Self {
            epoch,
            total: total.max(bytes.len() as u64),
            bytes: bytes.iter().copied().collect(),
            capacity,
        }
    }

    /// Where the next byte will be written.
    pub(crate) fn position(&self) -> OutputPosition {
        OutputPosition {
            epoch: self.epoch,
            offset: self.total,
        }
    }

    /// Everything from `position` on, or `None` once part of it is gone - or
    /// for a position in some other master's output, or past this one's end.
    pub(crate) fn since(&self, position: OutputPosition) -> Option<Vec<u8>> {
        if position.epoch != self.epoch {
            return None;
        }
        let offset = position.offset;
        let oldest = self.total - self.bytes.len() as u64;
        if offset < oldest || offset > self.total {
            return None;
        }
        Some(
            self.bytes
                .range((offset - oldest) as usize..)
                .copied()
                .collect(),
        )
    }
}

/// The handover a master writes before it execs a newer binary. A binary
/// that does not print this from `__resume-capable` cannot take a session
/// over and is never exec'd into one.
#[cfg(unix)]
pub(crate) const RESUME_FORMAT: u32 = 1;

/// Whether `candidate` is a later `major.minor.patch` than `current`.
/// Anything that does not parse is not later.
#[cfg(unix)]
pub(crate) fn newer_version(candidate: &str, current: &str) -> bool {
    fn parse(version: &str) -> Option<(u64, u64, u64)> {
        let mut parts = version.trim().split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts
            .next()?
            .split(|c: char| !c.is_ascii_digit())
            .next()?
            .parse()
            .ok()?;
        Some((major, minor, patch))
    }
    match (parse(candidate), parse(current)) {
        (Some(candidate), Some(current)) => candidate > current,
        _ => false,
    }
}

pub(crate) const CWD_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);
pub(crate) const SCROLLBACK_FLUSH_EVERY_TICKS: u32 = 15;

/// How long the leftovers of a session whose master died - a crash, an OOM
/// kill, the machine losing power - are kept so the next attach can restore
/// its directory and scrollback. Past this nobody is coming back for it, and
/// what is left is a directory of stale terminal output sitting on the server
/// forever.
pub(crate) const ABANDONED_SESSION_RETENTION: std::time::Duration =
    std::time::Duration::from_secs(7 * 24 * 60 * 60);

/// Whether a session directory last written [`ABANDONED_SESSION_RETENTION`]
/// ago or more should be swept. Split out so it can be tested without
/// touching a filesystem.
pub(crate) fn is_abandoned(modified: std::time::SystemTime, now: std::time::SystemTime) -> bool {
    now.duration_since(modified)
        .map(|age| age >= ABANDONED_SESSION_RETENTION)
        .unwrap_or(false)
}

/// Concurrent connections a master will serve.
pub(crate) const MAX_CLIENTS: usize = 16;

/// How long a connection may take to identify itself. Cleared once the client
/// attaches, because an attached client is expected to sit idle for hours.
pub(crate) const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

fn main() {
    if let Err(error) = run() {
        eprintln!("termphin-agent: {error}");
        process::exit(1);
    }
}

const USAGE: &str = "\
usage: termphin <command>

  attach [name]         attach to a session, choosing one if there are several
  new [name]            start a session and attach to it
  ls                    list sessions
  rename <old> <new>    rename a session
  kill <name>           end a session
  notify [title] [body] show a notification in Termphin (or any terminal that
                        reads OSC 777), e.g. at the end of a long command
  presence              tell the user's apps this machine is online, with its
                        sessions (run from crontab by the app)
  take <name> <device>  give the session to the app on that device
  away <name> <device>  that device's app went to the background
  back <name> <device>  that device's app came back
  version               print the version

Detach with Ctrl-\\ then d. Ctrl-\\ twice sends it to the program.
";

fn run() -> io::Result<()> {
    let mut args = env::args().skip(1);
    let command = args.next().unwrap_or_else(|| "help".to_owned());
    match command.as_str() {
        "--version" => print_version(false),
        "version" => print_version(args.next().as_deref() == Some("--machine")),
        "attach" | "a" => {
            let mut replay = false;
            let mut resume = None;
            let mut control = false;
            let mut device = None;
            let mut stdio = None;
            let name = loop {
                match args.next().as_deref() {
                    Some("--replay") => replay = true,
                    Some("--stdio") => {
                        let value = args
                            .next()
                            .and_then(|value| parse_size(&value, 'x'))
                            .ok_or_else(|| invalid_input("expected --stdio <cols>x<rows>"))?;
                        stdio = Some(value);
                    }
                    Some("--control") => control = true,
                    Some("--device") => {
                        let value = args
                            .next()
                            .filter(|value| valid_device(value))
                            .ok_or_else(|| invalid_input("invalid device description"))?;
                        device = Some(value);
                    }
                    // Only read as a string here: positions are a Unix notion,
                    // and Windows takes the flag without acting on it.
                    Some("--resume") => {
                        let value = args
                            .next()
                            .ok_or_else(|| invalid_input("missing resume position"))?;
                        resume = Some(value);
                    }
                    other => break other.map(str::to_owned),
                }
            };
            // The app always passes a flag; a bare attach is a person at a
            // terminal, who gets the replay, a detach key and a choice.
            if replay || resume.is_some() || control || device.is_some() || stdio.is_some() {
                platform::attach_command(
                    &required_name(name)?,
                    replay,
                    resume,
                    control,
                    device.as_deref(),
                    stdio,
                )?;
            } else {
                attach_person(name)?;
            }
        }
        "new" => new_session(args.next())?,
        "ls" => print_sessions()?,
        "list" => platform::list_command()?,
        "rename" => {
            let old_name = required_name(args.next())?;
            let new_name = required_name(args.next())?;
            platform::rename_command(&old_name, &new_name)?;
        }
        "kill" => platform::kill_command(&required_name(args.next())?)?,
        "notify" => notify_command(args.next(), args.collect::<Vec<_>>().join(" "))?,
        "presence" => presence::report(None, true),
        "take" | "away" | "back" => {
            let operation = match command.as_str() {
                "take" => DEVICE_TAKE,
                "away" => DEVICE_AWAY,
                _ => DEVICE_BACK,
            };
            let name = required_name(args.next())?;
            let id = args
                .next()
                .filter(|id| valid_device_id(id))
                .ok_or_else(|| invalid_input("invalid device id"))?;
            platform::device_command(&name, &device_control(operation, &id))?;
        }
        // Internal re-exec target used to detach the session master from the
        // process that spawned it. Not part of the public CLI.
        #[cfg(windows)]
        "__master" => windows::run_as_master(args)?,
        // A running session master asks a new binary whether it can take the
        // session over, then execs it as `__master-resume`. Not public CLI.
        #[cfg(unix)]
        "__resume-capable" => println!("{RESUME_FORMAT}"),
        #[cfg(unix)]
        "__master-resume" => unix::resume_master(args.collect())?,
        "help" | "--help" | "-h" => print!("{USAGE}"),
        _ => {
            eprint!("{USAGE}");
            process::exit(2);
        }
    }
    Ok(())
}

/// What `termphin notify` writes: rxvt's `OSC 777 ; notify ; title ; body`,
/// which Termphin, Ghostty and urxvt show as a notification. Semicolons and
/// control characters would end the sequence or the field early, so they
/// are taken out of the text.
pub(crate) fn notify_sequence(title: &str, body: &str) -> Vec<u8> {
    fn clean(text: &str) -> String {
        text.chars()
            .map(|c| if c == ';' || c.is_control() { ' ' } else { c })
            .collect::<String>()
            .trim()
            .to_owned()
    }
    format!("\x1b]777;notify;{};{}\x07", clean(title), clean(body)).into_bytes()
}

/// Writes the notification to the terminal itself, not to stdout: a hook
/// runner - Claude Code's among them - captures a hook's stdout, and the
/// sequence has to reach the screen to be seen. Without a terminal to open
/// (a cron job) it falls back to stdout.
fn notify_command(title: Option<String>, body: String) -> io::Result<()> {
    let title = title.unwrap_or_else(|| "Done".to_owned());
    let sequence = notify_sequence(&title, &body);
    #[cfg(unix)]
    if let Ok(mut tty) = std::fs::OpenOptions::new().write(true).open("/dev/tty") {
        tty.write_all(&sequence)?;
        return tty.flush();
    }
    let mut stdout = io::stdout().lock();
    stdout.write_all(&sequence)?;
    stdout.flush()
}

/// Set in every session's shell, so an attach from inside one can tell.
pub(crate) const SESSION_ENV: &str = "TERMPHIN_SESSION";

fn refuse_nesting() -> io::Result<()> {
    match env::var(SESSION_ENV) {
        Ok(name) => Err(io::Error::other(format!(
            "already inside session {name} - detach first, or unset {SESSION_ENV} to nest"
        ))),
        Err(_) => Ok(()),
    }
}

fn attach_person(name: Option<String>) -> io::Result<()> {
    refuse_nesting()?;
    let name = match name {
        Some(name) => {
            validate_name(&name)?;
            name
        }
        None => choose_session(&sessions()?)?,
    };
    finish_attach(&name, platform::attach_person(&name)?);
    Ok(())
}

fn new_session(name: Option<String>) -> io::Result<()> {
    refuse_nesting()?;
    let existing = sessions()?;
    let name = match name {
        Some(name) => {
            validate_name(&name)?;
            if existing.iter().any(|session| session.name == name) {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{name} already exists - attach to it with: termphin attach {name}"),
                ));
            }
            name
        }
        None => free_name(&existing),
    };
    finish_attach(&name, platform::attach_person(&name)?);
    Ok(())
}

fn finish_attach(name: &str, ending: Ending) {
    match ending {
        Ending::Detached => eprintln!("[detached from {name}]"),
        Ending::Ended => eprintln!("[{name} ended]"),
    }
}

/// How an attach a person started came to an end.
pub(crate) enum Ending {
    Detached,
    Ended,
}

/// Sessions whose process is gone but whose directory is kept - stopped by
/// a reboot or a signal - with their creation time: attaching to one
/// restores its screen and working directory.
pub(crate) fn restorable_sessions() -> Vec<(String, u64)> {
    platform::restorable_sessions()
}

pub(crate) fn sessions() -> io::Result<Vec<SessionInfo>> {
    Ok(platform::session_statuses()?
        .iter()
        .filter_map(|status| SessionInfo::parse(status))
        .collect())
}

fn print_sessions() -> io::Result<()> {
    let sessions = sessions()?;
    if sessions.is_empty() {
        println!("no sessions - start one with: termphin new");
        return Ok(());
    }
    print!("{}", format_sessions(&sessions, unix_now()));
    Ok(())
}

fn choose_session(sessions: &[SessionInfo]) -> io::Result<String> {
    match sessions {
        [] => Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no sessions - start one with: termphin new",
        )),
        [only] => Ok(only.name.clone()),
        _ => {
            let now = unix_now();
            for (index, session) in sessions.iter().enumerate() {
                eprintln!("  {}) {}", index + 1, describe(session, now));
            }
            eprint!("attach to [1-{}]: ", sessions.len());
            io::stderr().flush()?;
            let mut answer = String::new();
            io::stdin().read_line(&mut answer)?;
            answer
                .trim()
                .parse::<usize>()
                .ok()
                .and_then(|choice| choice.checked_sub(1))
                .and_then(|index| sessions.get(index))
                .map(|session| session.name.clone())
                .ok_or_else(|| invalid_input("no such session"))
        }
    }
}

fn free_name(existing: &[SessionInfo]) -> String {
    let taken = |name: &str| existing.iter().any(|session| session.name == name);
    if !taken("shell") {
        return "shell".to_owned();
    }
    (2..)
        .map(|n| format!("shell-{n}"))
        .find(|name| !taken(name))
        .expect("an unused name")
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// One line of `list`: name, protocol, creation time in Unix seconds, the
/// number of clients attached and - from 0.20.0, as a fifth field - the
/// devices of those clients that said which they are, comma-separated.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SessionInfo {
    pub(crate) name: String,
    pub(crate) created_at: u64,
    pub(crate) clients: usize,
    pub(crate) devices: Vec<String>,
    pub(crate) activity: Option<Activity>,
}

/// What a session is doing, as its master sees it every few seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ActivityState {
    /// The shell has the terminal: nothing is running.
    Idle,
    /// A program has the terminal and wrote something in the last seconds.
    Working,
    /// A program worked, went still and is waiting for keys or asking
    /// something - what the "quiet" notification announces.
    Waiting,
    /// A program has the terminal and is none of the above.
    Running,
}

impl ActivityState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Waiting => "waiting",
            Self::Running => "running",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "idle" => Self::Idle,
            "working" => Self::Working,
            "waiting" => Self::Waiting,
            "running" => Self::Running,
            _ => return None,
        })
    }
}

/// A session's activity: its state and the program in front, named the
/// way notifications name it - the program and the words before its first
/// option, so nothing typed after an option, a password say, ever shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Activity {
    pub(crate) state: ActivityState,
    pub(crate) command: Option<String>,
}

impl Activity {
    /// The sixth field of a status line: the state, then `;` and the
    /// command in URL-safe base64 when there is one.
    pub(crate) fn field(&self) -> String {
        match &self.command {
            Some(command) => format!(
                "{};{}",
                self.state.as_str(),
                presence::encode_base64url(command.as_bytes())
            ),
            None => self.state.as_str().to_owned(),
        }
    }

    pub(crate) fn parse(field: &str) -> Option<Self> {
        let (state, command) = match field.split_once(';') {
            Some((state, encoded)) => (
                state,
                Some(String::from_utf8(presence::decode_base64url(encoded)?).ok()?),
            ),
            None => (field, None),
        };
        Some(Self {
            state: ActivityState::parse(state)?,
            command,
        })
    }
}

impl SessionInfo {
    pub(crate) fn parse(status: &str) -> Option<Self> {
        let mut fields = status.split('\t');
        let name = fields.next()?.to_owned();
        let _protocol = fields.next()?;
        let created_at = fields.next()?.parse().ok()?;
        let clients = fields.next()?.parse().ok()?;
        let devices = fields
            .next()
            .map(|field| {
                field
                    .split(',')
                    .filter(|device| valid_device(device))
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let activity = fields.next().and_then(Activity::parse);
        Some(Self {
            name,
            created_at,
            clients,
            devices,
            activity,
        })
    }
}

fn describe(session: &SessionInfo, now: u64) -> String {
    let clients = match session.clients {
        0 => "detached".to_owned(),
        1 => "1 client".to_owned(),
        n => format!("{n} clients"),
    };
    format!(
        "{}  {clients}, started {}",
        session.name,
        ago(now.saturating_sub(session.created_at))
    )
}

pub(crate) fn format_sessions(sessions: &[SessionInfo], now: u64) -> String {
    let width = sessions
        .iter()
        .map(|session| session.name.len())
        .max()
        .unwrap_or(0);
    sessions
        .iter()
        .map(|session| {
            let described = describe(session, now);
            let rest = &described[session.name.len()..];
            format!("{:width$}{rest}\n", session.name)
        })
        .collect()
}

fn ago(seconds: u64) -> String {
    match seconds {
        0..60 => "just now".to_owned(),
        60..3600 => format!("{}m ago", seconds / 60),
        3600..86400 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86400),
    }
}

/// Ctrl-\ then d, in an attach a person started. Ctrl-\ is otherwise only
/// SIGQUIT, which a raw terminal does not generate; twice sends it on.
pub(crate) const DETACH_PREFIX: u8 = 0x1c;

#[derive(Default)]
pub(crate) struct DetachKey {
    armed: bool,
}

impl DetachKey {
    /// The input to send on, and whether the detach key was pressed - in
    /// which case whatever followed it is dropped with it.
    pub(crate) fn feed(&mut self, input: &[u8]) -> (Vec<u8>, bool) {
        let mut forward = Vec::with_capacity(input.len());
        for &byte in input {
            if self.armed {
                self.armed = false;
                match byte {
                    b'd' => return (forward, true),
                    DETACH_PREFIX => forward.push(DETACH_PREFIX),
                    other => forward.extend([DETACH_PREFIX, other]),
                }
            } else if byte == DETACH_PREFIX {
                self.armed = true;
            } else {
                forward.push(byte);
            }
        }
        (forward, false)
    }
}

/// What the app types to say its phone went away, never reaching the shell:
/// a private OSC that no terminal sends as input of its own.
pub(crate) const PRESENCE_AWAY: &[u8] = b"\x1b]5386;away\x07";

/// What the app types to say its phone is back.
pub(crate) const PRESENCE_BACK: &[u8] = b"\x1b]5386;back\x07";

/// What the master sends a client that came back, after everything queued
/// for it while it was away: the app shows nothing for notifications that
/// arrive before it, since the relay already delivered them.
pub(crate) const PRESENCE_BACK_MARKER: &[u8] = b"\x1b]5386;termphin-back\x07";

/// Whether a phone is looking at the session: some client is attached and
/// has not said it went away. With nobody away - every phone but an iPhone,
/// which is the only one that says so - this is just whether anyone is
/// attached, as it always was.
pub(crate) fn anyone_present<'a>(
    clients: impl IntoIterator<Item = &'a u64>,
    away: &std::collections::HashSet<u64>,
) -> bool {
    clients.into_iter().any(|id| !away.contains(id))
}

/// The fifth field of a status line: the attached devices that said which
/// they are, comma-separated.
pub(crate) fn devices_field<'a>(devices: impl IntoIterator<Item = &'a String>) -> String {
    devices
        .into_iter()
        .filter(|device| valid_device(device))
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(",")
}

/// What the app types to say which device it is, followed by the device's
/// description and `BEL`: `<install id>;<kind>;<base64url name>`. The agent
/// keeps it opaque and passes it on in its presence reports, so the user's
/// other devices can show who is attached to a session.
pub(crate) const PRESENCE_DEVICE: &[u8] = b"\x1b]5386;device;";

/// Longest device description kept.
const MAX_DEVICE: usize = 200;

/// Whether [description] is something the app could have sent: a short run
/// of URL-safe base64 characters and the `;` between its fields - never a
/// tab, comma or quote that would break the status line or a report.
pub(crate) fn valid_device(description: &str) -> bool {
    !description.is_empty()
        && description.len() <= MAX_DEVICE
        && description
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b';'))
}

/// What the app types to take control of the session; see
/// [`FRAME_CONTROL`].
pub(crate) const CONTROL_TAKE_KEYS: &[u8] = b"\x1b]5386;control;take\x07";

/// What an app attached with `--stdio` types when its terminal changes size,
/// followed by `<cols>;<rows>` and `BEL`: with no terminal of its own, the
/// client cannot ask one.
pub(crate) const PRESENCE_SIZE: &[u8] = b"\x1b]5386;size;";

/// Longest size message body: two five-digit numbers and the `;`.
const MAX_SIZE_MESSAGE: usize = 11;

/// The size in a `<cols>;<rows>` message, or `<cols>x<rows>` given to
/// `attach --stdio`; none for anything else or a zero.
pub(crate) fn parse_size(text: &str, separator: char) -> Option<TermSize> {
    let (cols, rows) = text.split_once(separator)?;
    let number = |value: &str| {
        (!value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| value.parse::<u16>().ok())
            .flatten()
            .filter(|&value| value > 0)
    };
    Some(TermSize {
        cols: number(cols)?,
        rows: number(rows)?,
    })
}

/// Whether [id] could be an install id: the first field of a device
/// description.
pub(crate) fn valid_device_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

/// The payload of [`FRAME_DEVICE_CONTROL`].
pub(crate) fn device_control(operation: u8, id: &str) -> Vec<u8> {
    let mut payload = vec![operation];
    payload.extend_from_slice(id.as_bytes());
    payload
}

/// Reads [`FRAME_DEVICE_CONTROL`]: the operation and the install id.
pub(crate) fn parse_device_control(payload: &[u8]) -> io::Result<(u8, &str)> {
    let (&operation, id) = payload
        .split_first()
        .ok_or_else(|| invalid_input("empty device control frame"))?;
    let id = std::str::from_utf8(id).map_err(|_| invalid_input("invalid device id"))?;
    if operation > DEVICE_BACK || !valid_device_id(id) {
        return Err(invalid_input("invalid device control frame"));
    }
    Ok((operation, id))
}

/// The clients whose device has install id [id], newest first.
pub(crate) fn clients_of_device<'a>(
    devices: impl IntoIterator<Item = (&'a u64, &'a String)>,
    id: &str,
) -> Vec<u64> {
    let mut clients = devices
        .into_iter()
        .filter(|(_, description)| description.split(';').next() == Some(id))
        .map(|(client, _)| *client)
        .collect::<Vec<_>>();
    clients.sort_unstable_by(|a, b| b.cmp(a));
    clients
}

/// Whether two device descriptions name the same device: the same install
/// id, the first field, whatever its name now says.
pub(crate) fn same_device(a: &str, b: &str) -> bool {
    match (a.split(';').next(), b.split(';').next()) {
        (Some(first), Some(second)) => !first.is_empty() && first == second,
        _ => false,
    }
}

/// A message the app typed that is not for the shell.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PresenceEvent {
    Away,
    Back,
    Device(String),
    TakeControl,
    Resize(TermSize),
}

/// Takes the app's presence messages out of what is typed, the way
/// [`DetachKey`] takes the detach key out.
#[derive(Default)]
pub(crate) struct PresenceKeys {
    pending: Vec<u8>,
}

impl PresenceKeys {
    /// The input to send on, and the presence messages found in it in order.
    ///
    /// A message cut off at the end of a read is held for the next one. Only
    /// from `ESC ]` on, so an Escape pressed on its own is never held back,
    /// and a device message only up to [`MAX_DEVICE`]: anything longer was
    /// not the app's and goes to the shell as typed.
    pub(crate) fn feed(&mut self, input: &[u8]) -> (Vec<u8>, Vec<PresenceEvent>) {
        let mut data = std::mem::take(&mut self.pending);
        data.extend_from_slice(input);
        let mut forward = Vec::with_capacity(data.len());
        let mut events = Vec::new();
        let mut index = 0;
        while index < data.len() {
            let rest = &data[index..];
            if rest.starts_with(PRESENCE_AWAY) {
                events.push(PresenceEvent::Away);
                index += PRESENCE_AWAY.len();
                continue;
            }
            if rest.starts_with(PRESENCE_BACK) {
                events.push(PresenceEvent::Back);
                index += PRESENCE_BACK.len();
                continue;
            }
            if rest.starts_with(CONTROL_TAKE_KEYS) {
                events.push(PresenceEvent::TakeControl);
                index += CONTROL_TAKE_KEYS.len();
                continue;
            }
            if rest.starts_with(PRESENCE_DEVICE) {
                let body = &rest[PRESENCE_DEVICE.len()..];
                let limit = body.len().min(MAX_DEVICE + 1);
                if let Some(end) = body[..limit].iter().position(|&byte| byte == 0x07) {
                    if let Ok(description) = std::str::from_utf8(&body[..end])
                        && valid_device(description)
                    {
                        events.push(PresenceEvent::Device(description.to_owned()));
                    }
                    index += PRESENCE_DEVICE.len() + end + 1;
                    continue;
                }
                if body.len() <= MAX_DEVICE {
                    self.pending = rest.to_vec();
                    break;
                }
            }
            if rest.starts_with(PRESENCE_SIZE) {
                let body = &rest[PRESENCE_SIZE.len()..];
                let limit = body.len().min(MAX_SIZE_MESSAGE + 1);
                if let Some(end) = body[..limit].iter().position(|&byte| byte == 0x07) {
                    if let Some(size) = std::str::from_utf8(&body[..end])
                        .ok()
                        .and_then(|text| parse_size(text, ';'))
                    {
                        events.push(PresenceEvent::Resize(size));
                    }
                    index += PRESENCE_SIZE.len() + end + 1;
                    continue;
                }
                if body.len() <= MAX_SIZE_MESSAGE {
                    self.pending = rest.to_vec();
                    break;
                }
            }
            if rest.len() >= 2
                && rest[0] == 0x1b
                && (PRESENCE_AWAY.starts_with(rest)
                    || PRESENCE_BACK.starts_with(rest)
                    || PRESENCE_DEVICE.starts_with(rest)
                    || PRESENCE_SIZE.starts_with(rest)
                    || CONTROL_TAKE_KEYS.starts_with(rest))
            {
                self.pending = rest.to_vec();
                break;
            }
            forward.push(data[index]);
            index += 1;
        }
        (forward, events)
    }
}

/// Drops ConPTY's `CSI 8 ; rows ; cols t` from its output. It sends one after
/// every resize to tell the terminal the new size, and a client whose own
/// console obeys it shrinks to match whoever resized last - after which that
/// client reports the wrong size itself. Only whole sequences inside `data`
/// are dropped; ConPTY writes each in one piece.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn strip_resize_reports(data: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    let mut kept: Option<Vec<u8>> = None;
    let mut index = 0;
    while index < data.len() {
        if let Some(length) = resize_report_at(&data[index..]) {
            kept.get_or_insert_with(|| data[..index].to_vec());
            index += length;
            continue;
        }
        if let Some(kept) = kept.as_mut() {
            kept.push(data[index]);
        }
        index += 1;
    }
    match kept {
        Some(kept) => std::borrow::Cow::Owned(kept),
        None => std::borrow::Cow::Borrowed(data),
    }
}

fn resize_report_at(data: &[u8]) -> Option<usize> {
    let rest = data.strip_prefix(b"\x1b[8;")?;
    let digits = |from: &[u8]| from.iter().take_while(|byte| byte.is_ascii_digit()).count();
    let rows = digits(rest);
    let rest = rest.get(rows..)?.strip_prefix(b";").filter(|_| rows > 0)?;
    let cols = digits(rest);
    rest.get(cols..)?.strip_prefix(b"t").filter(|_| cols > 0)?;
    Some(b"\x1b[8;".len() + rows + 1 + cols + 1)
}

/// Who holds the session and whose size its terminal takes.
///
/// A client that joined control ([`FRAME_CONTROL`]) attaches as a viewer
/// while another client holds the session and is not away, and only takes
/// it with [`CONTROL_TAKE`]; what it types meanwhile is dropped. A client
/// that never joined is an older app, and gets what every client had
/// before: attaching or typing makes it the one whose size is used.
/// When the holder leaves, the client used most recently before it takes
/// over.
#[derive(Default)]
pub(crate) struct Viewers {
    recent: Vec<Viewer>,
    holder: Option<u64>,
}

#[derive(Clone, Copy)]
struct Viewer {
    id: u64,
    size: TermSize,
    joined: bool,
}

/// What to do with input a client typed.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Typed {
    /// Write it, after resizing to this size if there is one.
    Write(Option<TermSize>),
    /// Drop it: the client follows control and does not hold the session.
    Drop,
}

impl Viewers {
    /// Client [id] attached at [size]. [held] says whether another client
    /// holds the session and is not away. The size to apply, if the client
    /// now holds the session.
    pub(crate) fn attached(
        &mut self,
        id: u64,
        size: TermSize,
        joined: bool,
        held: bool,
    ) -> Option<TermSize> {
        self.recent.retain(|viewer| viewer.id != id);
        self.recent.push(Viewer { id, size, joined });
        if joined && held && self.holder.is_some_and(|holder| holder != id) {
            return None;
        }
        self.holder = Some(id);
        Some(size)
    }

    /// The size to apply, if the client that resized holds the session.
    pub(crate) fn resized(&mut self, id: u64, size: TermSize) -> Option<TermSize> {
        let viewer = self.recent.iter_mut().find(|viewer| viewer.id == id)?;
        viewer.size = size;
        (self.holder == Some(id)).then_some(size)
    }

    pub(crate) fn typed(&mut self, id: u64) -> Typed {
        if self.holder == Some(id) {
            return Typed::Write(None);
        }
        match self.recent.iter().find(|viewer| viewer.id == id) {
            Some(viewer) if viewer.joined => Typed::Drop,
            Some(_) => Typed::Write(self.take(id)),
            None => Typed::Write(None),
        }
    }

    /// Client [id] takes the session. Its size, if that changed hands.
    pub(crate) fn take(&mut self, id: u64) -> Option<TermSize> {
        if self.holder == Some(id) {
            return None;
        }
        let index = self.recent.iter().position(|viewer| viewer.id == id)?;
        let viewer = self.recent.remove(index);
        self.recent.push(viewer);
        self.holder = Some(id);
        Some(viewer.size)
    }

    /// The size to go back to, if the client that left held the session.
    pub(crate) fn left(&mut self, id: u64) -> Option<TermSize> {
        self.recent.retain(|viewer| viewer.id != id);
        if self.holder != Some(id) {
            return None;
        }
        let next = self.recent.last().copied();
        self.holder = next.map(|viewer| viewer.id);
        next.map(|viewer| viewer.size)
    }

    pub(crate) fn holder(&self) -> Option<u64> {
        self.holder
    }

    /// The size the session runs at: the holder's.
    pub(crate) fn size(&self) -> Option<TermSize> {
        let holder = self.holder?;
        self.recent
            .iter()
            .find(|viewer| viewer.id == holder)
            .map(|viewer| viewer.size)
    }

    /// The clients that joined control, which are told who holds it.
    pub(crate) fn joined(&self) -> impl Iterator<Item = u64> + '_ {
        self.recent
            .iter()
            .filter(|viewer| viewer.joined)
            .map(|viewer| viewer.id)
    }
}

/// What [`FRAME_CONTROL_STATE`] carries: one byte, 1 when the client it is
/// sent to holds the session; the session's size as in [`encode_size`];
/// then the holder's device description, empty when it gave none.
pub(crate) fn encode_control_state(holds: bool, size: TermSize, device: &str) -> Vec<u8> {
    let mut payload = Vec::with_capacity(9 + device.len());
    payload.push(u8::from(holds));
    payload.extend_from_slice(&encode_size(size));
    payload.extend_from_slice(device.as_bytes());
    payload
}

/// The marker a client writes for the app on [`FRAME_CONTROL_STATE`]:
/// `ESC ] 5384 ; termphin-control ; <you|other> ; <cols> ; <rows> ; <device>
/// BEL`, the device last since it holds `;` of its own. Nothing for a
/// payload that does not decode.
pub(crate) fn control_marker(payload: &[u8]) -> Option<Vec<u8>> {
    let (&holds, rest) = payload.split_first()?;
    let size = decode_size(rest.get(..8)?).ok()?;
    let device = std::str::from_utf8(&rest[8..]).ok()?;
    if !device.is_empty() && !valid_device(device) {
        return None;
    }
    let role = if holds == 1 { "you" } else { "other" };
    Some(
        format!(
            "\x1b]5384;termphin-control;{role};{};{};{device}\x07",
            size.cols, size.rows
        )
        .into_bytes(),
    )
}

fn print_version(machine: bool) {
    if machine {
        println!("version={}", env!("CARGO_PKG_VERSION"));
        println!("protocol={PROTOCOL_VERSION}");
    } else {
        println!(
            "termphin-agent {} protocol {}",
            env!("CARGO_PKG_VERSION"),
            PROTOCOL_VERSION
        );
    }
}

#[cfg(unix)]
use unix as platform;
#[cfg(windows)]
use windows as platform;

fn required_name(value: Option<String>) -> io::Result<String> {
    let name = value.ok_or_else(|| invalid_input("missing session name"))?;
    validate_name(&name)?;
    Ok(name)
}

pub(crate) fn validate_name(name: &str) -> io::Result<()> {
    if name.is_empty() || name.len() > 64 {
        return Err(invalid_input(
            "session name must contain 1 to 64 characters",
        ));
    }
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return Err(invalid_input(
            "session name may contain only letters, numbers, '_' and '-'",
        ));
    }
    Ok(())
}

pub(crate) fn invalid_input(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

/// Terminal size in cells. Platform-neutral stand-in for `libc::winsize` /
/// Windows' `COORD`, which the two platform modules convert to and from at
/// their own boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TermSize {
    pub(crate) cols: u16,
    pub(crate) rows: u16,
}

pub(crate) fn encode_size(size: TermSize) -> [u8; 8] {
    let mut payload = [0_u8; 8];
    payload[..4].copy_from_slice(&(u32::from(size.cols)).to_be_bytes());
    payload[4..].copy_from_slice(&(u32::from(size.rows)).to_be_bytes());
    payload
}

pub(crate) fn decode_size(payload: &[u8]) -> io::Result<TermSize> {
    if payload.len() != 8 {
        return Err(invalid_input("invalid terminal size frame"));
    }
    let columns = u32::from_be_bytes(payload[..4].try_into().expect("four bytes"));
    let rows = u32::from_be_bytes(payload[4..].try_into().expect("four bytes"));
    if columns == 0 || rows == 0 || columns > u16::MAX.into() || rows > u16::MAX.into() {
        return Err(invalid_input("invalid terminal size"));
    }
    Ok(TermSize {
        cols: columns as u16,
        rows: rows as u16,
    })
}

pub(crate) fn send_frame<W: Write>(stream: &mut W, kind: u8, payload: &[u8]) -> io::Result<()> {
    if payload.len() > MAX_FRAME_SIZE {
        return Err(invalid_input("protocol frame is too large"));
    }
    let mut header = [0_u8; 5];
    header[0] = kind;
    header[1..].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    stream.write_all(&header)?;
    stream.write_all(payload)
}

pub(crate) fn read_frame<R: Read>(stream: &mut R) -> io::Result<(u8, Vec<u8>)> {
    let mut header = [0_u8; 5];
    stream.read_exact(&mut header)?;
    let length = u32::from_be_bytes(header[1..].try_into().expect("four bytes")) as usize;
    if length > MAX_FRAME_SIZE {
        return Err(invalid_input("protocol frame is too large"));
    }
    let mut payload = vec![0_u8; length];
    stream.read_exact(&mut payload)?;
    Ok((header[0], payload))
}

/// What a fresh session master should seed itself with, salvaged from a
/// directory a previous master left behind (a clean shutdown removes the
/// directory entirely, so finding one here means the host rebooted or the
/// master was killed).
#[derive(Default)]
pub(crate) struct RestoreState {
    // Read on Unix to restore the shell's working directory; the Windows
    // master does not track cwd (see the module doc in `windows.rs`), so
    // this field is write-only there - expected, not dead code.
    #[cfg_attr(windows, allow(dead_code))]
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) scrollback: Vec<u8>,
    /// The old master is gone and this session is being rebuilt from what it
    /// left on disk - after a reboot, but equally after an OOM kill or a
    /// crash. Either way the shell the client reattaches to is a new one.
    pub(crate) restored: bool,
    /// The machine itself came back up, which is worth saying differently
    /// from a master that died on a host that stayed up.
    pub(crate) reboot_restored: bool,
}

pub(crate) fn boot_id_differs(persisted: Option<&str>, current: Option<&str>) -> bool {
    match (persisted, current) {
        (Some(old), Some(new)) => old.trim() != new.trim(),
        _ => false,
    }
}

impl RestoreState {
    pub(crate) fn load(directory: &Path, current_boot_id: Option<&str>) -> Self {
        let cwd = std::fs::read_to_string(directory.join("cwd"))
            .ok()
            .map(PathBuf::from)
            .filter(|path| path.is_dir());
        let scrollback = std::fs::read(directory.join("scrollback")).unwrap_or_default();
        let persisted_boot_id = std::fs::read_to_string(directory.join("boot_id")).ok();
        let reboot_restored = boot_id_differs(persisted_boot_id.as_deref(), current_boot_id);
        Self {
            cwd,
            scrollback,
            // `load` is only reached when a session directory is already
            // there, which is the leftovers of a master that is no longer
            // running.
            restored: true,
            reboot_restored,
        }
    }
}

/// As many rows as the app's own terminal keeps, so a replay gives back
/// everything the app could have been showing.
pub(crate) const SCROLLBACK_ROWS: usize = 10_000;

#[derive(Default, Clone)]
struct TitleTracker {
    title: Arc<Mutex<Option<Vec<u8>>>>,
}

impl vt100::Callbacks for TitleTracker {
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        *self.title.lock().expect("title mutex poisoned") = Some(title.to_vec());
    }
}

/// Says which master serves the session, first thing on an attach: the
/// app reads it to tell which features the session can have. Sent to the
/// one client and never kept in the history - so it goes ahead of the
/// replay and of the offset marker that ends it, where a client counting
/// output positions has not started counting yet.
pub(crate) fn master_version_marker() -> Vec<u8> {
    format!(
        "\x1b]5384;termphin-master;{}\x07",
        env!("CARGO_PKG_VERSION")
    )
    .into_bytes()
}

/// Rows above the cursor [`History::screen_line`] looks through for the last
/// line a command wrote.
const WRITTEN_ROWS: u16 = 3;

pub(crate) struct History {
    parser: vt100::Parser<TitleTracker>,
    title: Arc<Mutex<Option<Vec<u8>>>>,
    restored: bool,
    #[cfg(unix)]
    unsaved: bool,
    #[cfg(unix)]
    output: OutputLog,
}

impl History {
    pub(crate) fn new(rows: u16, cols: u16) -> Self {
        let tracker = TitleTracker::default();
        Self {
            parser: vt100::Parser::new_with_callbacks(
                rows.max(1),
                cols.max(1),
                SCROLLBACK_ROWS,
                tracker.clone(),
            ),
            title: tracker.title,
            restored: false,
            #[cfg(unix)]
            unsaved: true,
            #[cfg(unix)]
            output: OutputLog::new(OUTPUT_LOG_BYTES),
        }
    }

    /// A history taken over from a master that exec'd a newer binary: the
    /// screen as [`Self::snapshot`] drew it, and the raw output log as it
    /// was, so a client resuming from an offset carries on as if nothing
    /// had happened.
    #[cfg(unix)]
    pub(crate) fn resumed(
        rows: u16,
        cols: u16,
        screen: &[u8],
        output: OutputLog,
        restored: bool,
    ) -> Self {
        let mut history = Self::new(rows, cols);
        history.parser.process(screen);
        history.output = output;
        history.restored = restored;
        history
    }

    #[cfg(unix)]
    pub(crate) fn restored(&self) -> bool {
        self.restored
    }

    pub(crate) fn push(&mut self, data: &[u8]) {
        self.parser.process(data);
        #[cfg(unix)]
        {
            self.unsaved = true;
            self.output.push(data);
        }
    }

    /// Whether anything changed since this was last asked, which is what
    /// the snapshot on disk is behind by. A session nobody is using has
    /// nothing new to save, and its snapshot - thousands of rows - is not
    /// written out again every interval just the same.
    #[cfg(unix)]
    pub(crate) fn take_unsaved(&mut self) -> bool {
        std::mem::replace(&mut self.unsaved, false)
    }

    #[cfg(unix)]
    pub(crate) fn output(&self) -> &OutputLog {
        &self.output
    }

    /// The line of the screen a notification quotes, as
    /// [`relay::screen_line`] picks it. [`relay::Pick::Written`] looks at the
    /// cursor's row and the few above it only, because it runs on every
    /// piece of output a command writes.
    pub(crate) fn screen_line(&self, pick: relay::Pick) -> Option<String> {
        let screen = self.parser.screen();
        let (_, cols) = screen.size();
        let (cursor, _) = screen.cursor_position();
        let rows = match pick {
            relay::Pick::Written => {
                let top = cursor.saturating_sub(WRITTEN_ROWS);
                (top..=cursor)
                    .map(|row| screen.contents_between(row, 0, row, cols))
                    .collect::<Vec<_>>()
            }
            relay::Pick::Showing => screen.rows(0, cols).collect(),
        };
        let cursor = match pick {
            relay::Pick::Written => rows.len() - 1,
            relay::Pick::Showing => usize::from(cursor),
        };
        relay::screen_line(&rows, cursor, pick)
    }

    pub(crate) fn resize(&mut self, rows: u16, cols: u16) {
        self.parser.screen_mut().set_size(rows.max(1), cols.max(1));
        #[cfg(unix)]
        {
            self.unsaved = true;
        }
    }

    /// The session as a terminal of its own size is shown it: the snapshot
    /// saved to disk and handed to a new version, replayed into a parser
    /// just like this one.
    pub(crate) fn snapshot(&mut self) -> Vec<u8> {
        let (rows, _) = self.parser.screen().size();
        self.snapshot_for(rows)
    }

    /// The session as a client `client_rows` tall is shown it on attach.
    ///
    /// That client is not always the size this screen still has - the one
    /// that resized it last may have been taller, or the phone's keyboard
    /// may be open now - and the size it asks for is only applied after the
    /// replay. So the primary screen is not cleared and redrawn at absolute
    /// rows, which loses or blanks rows whenever the two heights differ, but
    /// written on after the scrollback as more of the same text, the cursor
    /// put back relative to the last row written.
    pub(crate) fn snapshot_for(&mut self, client_rows: u16) -> Vec<u8> {
        let mut output = Vec::new();
        if self.restored {
            output.extend(REBOOT_RESTORED_MARKER);
        }
        if let Some(title) = self.title.lock().expect("title mutex poisoned").clone() {
            output.extend(b"\x1b]0;");
            output.extend(title);
            output.push(0x07);
        }
        output.extend(b"\x1b[H\x1b[2J\x1b[3J");
        if self.parser.screen().alternate_screen() {
            output.extend(b"\x1b[?1049h");
            output.extend(self.parser.screen().state_formatted());
            return output;
        }
        output.extend(self.primary_lines(client_rows));
        let screen = self.parser.screen();
        output.extend(screen.attributes_formatted());
        output.extend(if screen.hide_cursor() {
            b"\x1b[?25l"
        } else {
            b"\x1b[?25h"
        });
        output.extend(screen.input_mode_formatted());
        output
    }

    /// The scrollback and then the screen, row after row, a row the
    /// terminal wrapped running on into the next one - across the top of
    /// the screen too, or a line that wrapped there is torn in two.
    ///
    /// The blank rows below the cursor are written only to a client at least
    /// as tall as this screen, where they keep the screen's own layout - a
    /// prompt at the top after `clear`. To a shorter one they would push
    /// the cursor's row up past where it is put back.
    fn primary_lines(&mut self, client_rows: u16) -> Vec<u8> {
        let screen = self.parser.screen_mut();
        let (rows, cols) = screen.size();
        screen.set_scrollback(usize::MAX);
        let depth = screen.scrollback();
        let mut output = Vec::new();
        for n in (1..=depth).rev() {
            screen.set_scrollback(n);
            if let Some(line) = screen.rows_formatted(0, cols).next() {
                output.extend(line);
                output.extend(b"\x1b[m");
                if !runs_on(screen, 0, cols) {
                    output.extend(b"\r\n");
                }
            }
        }
        screen.set_scrollback(0);

        let (cursor_row, cursor_col) = screen.cursor_position();
        let lines: Vec<Vec<u8>> = screen.rows_formatted(0, cols).collect();
        let last = if client_rows >= rows {
            rows.saturating_sub(1)
        } else {
            lines
                .iter()
                .rposition(|line| !line.is_empty())
                .and_then(|row| u16::try_from(row).ok())
                .unwrap_or(0)
                .max(cursor_row)
        };
        for (row, line) in (0..=last).zip(&lines) {
            output.extend(line);
            output.extend(b"\x1b[m");
            if row < last && !runs_on(screen, row, cols) {
                output.extend(b"\r\n");
            }
        }
        output.push(b'\r');
        let up = last.saturating_sub(cursor_row);
        if up > 0 {
            output.extend(format!("\x1b[{up}A").as_bytes());
        }
        if cursor_col > 0 {
            output.extend(format!("\x1b[{cursor_col}C").as_bytes());
        }
        output
    }

    pub(crate) fn seed_restored(&mut self, scrollback: &[u8], after_reboot: bool) {
        if !scrollback.is_empty() {
            self.push(scrollback);
        }
        self.restored = true;
        self.push(if after_reboot {
            b"\r\n\x1b[33mrestored after a server restart - new shell, same directory\x1b[0m\r\n\r\n"
                .as_slice()
        } else {
            b"\r\n\x1b[33mthe previous shell is gone - new shell, same directory\x1b[0m\r\n\r\n"
                .as_slice()
        });
    }
}

/// Whether `row` of the visible rows runs on into the next one: the
/// terminal wrapped it, and its last column is written, so the client's own
/// wrap carries the text on where this one's did.
fn runs_on(screen: &vt100::Screen, row: u16, cols: u16) -> bool {
    screen.row_wrapped(row)
        && screen
            .cell(row, cols - 1)
            .is_some_and(|cell| cell.has_contents() || cell.is_wide_continuation())
}

/// Queued-but-unwritten frames for one attached client. Platform-neutral: the
/// platform's client-channel wraps this with its own stream type and wakeup
/// mechanism.
#[derive(Default)]
pub(crate) struct ClientQueue {
    pub(crate) frames: VecDeque<(u8, Vec<u8>)>,
    pub(crate) bytes: usize,
    pub(crate) closed: bool,
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_master_names_its_version_in_a_private_sequence() {
        let marker = String::from_utf8(super::master_version_marker()).unwrap();
        assert!(marker.starts_with("\x1b]5384;termphin-master;"));
        assert!(marker.ends_with('\x07'));
        assert!(marker.contains(env!("CARGO_PKG_VERSION")));
    }

    use super::*;

    #[test]
    fn resize_reports_are_dropped_and_nothing_else() {
        assert_eq!(
            &*strip_resize_reports(b"a\x1b[8;12;40tb\x1b[8;30;100t"),
            b"ab"
        );
        assert_eq!(
            &*strip_resize_reports(b"\x1b[31mred\x1b[0m"),
            b"\x1b[31mred\x1b[0m"
        );
        assert_eq!(&*strip_resize_reports(b"\x1b[8;12t"), b"\x1b[8;12t");
        assert_eq!(&*strip_resize_reports(b"\x1b[8;;40t"), b"\x1b[8;;40t");
        assert_eq!(&*strip_resize_reports(b"\x1b[8;12;40"), b"\x1b[8;12;40");
    }

    #[test]
    fn output_without_a_resize_report_is_not_copied() {
        assert!(matches!(
            strip_resize_reports(b"plain"),
            std::borrow::Cow::Borrowed(_)
        ));
    }

    fn size(cols: u16, rows: u16) -> TermSize {
        TermSize { cols, rows }
    }

    #[test]
    fn with_nobody_away_presence_is_just_being_attached() {
        let away = std::collections::HashSet::new();
        assert!(!anyone_present(&[], &away));
        assert!(anyone_present(&[1, 2], &away));
    }

    #[test]
    fn a_session_whose_every_phone_is_away_counts_as_left() {
        let away: std::collections::HashSet<u64> = [1, 2].into();
        assert!(!anyone_present(&[1, 2], &away));
        assert!(anyone_present(&[1, 2, 3], &away));
    }

    #[test]
    fn presence_messages_are_taken_out_of_the_input() {
        let mut keys = PresenceKeys::default();
        let (forward, events) = keys.feed(b"ls\x1b]5386;away\x07\x1b]5386;back\x07\r");
        assert_eq!(forward, b"ls\r".to_vec());
        assert_eq!(events, vec![PresenceEvent::Away, PresenceEvent::Back]);
    }

    #[test]
    fn a_presence_message_split_across_reads_is_still_found() {
        let mut keys = PresenceKeys::default();
        let (first, none) = keys.feed(b"a\x1b]5386;aw");
        assert_eq!(first, b"a".to_vec());
        assert!(none.is_empty());
        let (second, events) = keys.feed(b"ay\x07b");
        assert_eq!(second, b"b".to_vec());
        assert_eq!(events, vec![PresenceEvent::Away]);
    }

    #[test]
    fn a_device_message_is_taken_out_whole_or_split() {
        let mut keys = PresenceKeys::default();
        let (forward, events) = keys.feed(b"x\x1b]5386;device;id1;phone;UGl4ZWw\x07y");
        assert_eq!(forward, b"xy".to_vec());
        assert_eq!(
            events,
            vec![PresenceEvent::Device("id1;phone;UGl4ZWw".to_owned())]
        );

        let (first, none) = keys.feed(b"a\x1b]5386;dev");
        assert_eq!(first, b"a".to_vec());
        assert!(none.is_empty());
        let (second, none) = keys.feed(b"ice;id2;desk");
        assert!(second.is_empty());
        assert!(none.is_empty());
        let (third, events) = keys.feed(b"top;bWJwYw\x07b");
        assert_eq!(third, b"b".to_vec());
        assert_eq!(
            events,
            vec![PresenceEvent::Device("id2;desktop;bWJwYw".to_owned())]
        );
    }

    #[test]
    fn a_device_message_with_anything_odd_is_dropped_not_typed() {
        let mut keys = PresenceKeys::default();
        let (forward, events) = keys.feed(b"\x1b]5386;device;a\tb,c\x07z");
        assert_eq!(forward, b"z".to_vec());
        assert!(events.is_empty());
    }

    #[test]
    fn an_endless_device_message_goes_to_the_shell() {
        let mut keys = PresenceKeys::default();
        let mut input = PRESENCE_DEVICE.to_vec();
        input.extend(std::iter::repeat_n(b'a', MAX_DEVICE + 5));
        let (forward, events) = keys.feed(&input);
        assert_eq!(forward, input);
        assert!(events.is_empty());
    }

    #[test]
    fn a_size_message_is_taken_out_whole_or_split() {
        let mut keys = PresenceKeys::default();
        let (forward, events) = keys.feed(b"ls\x1b]5386;size;120;40\x07\r");
        assert_eq!(forward, b"ls\r".to_vec());
        assert_eq!(
            events,
            vec![PresenceEvent::Resize(TermSize {
                cols: 120,
                rows: 40
            })]
        );

        let (first, none) = keys.feed(b"\x1b]5386;size;8");
        assert_eq!((first, none), (Vec::new(), vec![]));
        let (rest, events) = keys.feed(b"0;24\x07");
        assert!(rest.is_empty());
        assert_eq!(
            events,
            vec![PresenceEvent::Resize(TermSize { cols: 80, rows: 24 })]
        );
    }

    #[test]
    fn a_size_message_with_a_bad_size_is_dropped_and_a_long_one_typed() {
        let mut keys = PresenceKeys::default();
        let (forward, events) = keys.feed(b"\x1b]5386;size;0;24\x07");
        assert!(forward.is_empty() && events.is_empty());

        let mut input = PRESENCE_SIZE.to_vec();
        input.extend(std::iter::repeat_n(b'1', MAX_SIZE_MESSAGE + 3));
        let (forward, events) = keys.feed(&input);
        assert_eq!(forward, input);
        assert!(events.is_empty());
    }

    #[test]
    fn stdio_sizes_are_columns_by_rows() {
        assert_eq!(
            parse_size("132x43", 'x'),
            Some(TermSize {
                cols: 132,
                rows: 43
            })
        );
        for bad in [
            "", "80", "80x", "x24", "0x24", "80x0", "-1x24", "80x24x1", "99999x24",
        ] {
            assert_eq!(parse_size(bad, 'x'), None, "{bad}");
        }
    }

    #[test]
    fn device_descriptions_are_url_safe_and_short() {
        assert!(valid_device("AbC-_9;phone;UGl4ZWw"));
        assert!(!valid_device(""));
        assert!(!valid_device("a b"));
        assert!(!valid_device("a,b"));
        assert!(!valid_device("a\"b"));
        assert!(!valid_device(&"a".repeat(MAX_DEVICE + 1)));
    }

    #[test]
    fn escape_and_other_sequences_pass_straight_through() {
        let mut keys = PresenceKeys::default();
        assert_eq!(keys.feed(b"\x1b").0, b"\x1b".to_vec());
        assert_eq!(keys.feed(b"\x1b[A").0, b"\x1b[A".to_vec());
        assert_eq!(keys.feed(b"\x1b]52;c;x\x07").0, b"\x1b]52;c;x\x07".to_vec());
    }

    #[test]
    fn detach_key_detaches_and_keeps_what_came_before() {
        let mut key = DetachKey::default();

        assert_eq!(key.feed(b"ls\x1cd"), (b"ls".to_vec(), true));
    }

    #[test]
    fn detach_key_split_across_reads_still_detaches() {
        let mut key = DetachKey::default();

        assert_eq!(key.feed(b"x\x1c"), (b"x".to_vec(), false));
        assert_eq!(key.feed(b"d"), (Vec::new(), true));
    }

    #[test]
    fn detach_prefix_twice_sends_one() {
        let mut key = DetachKey::default();

        assert_eq!(key.feed(b"\x1c\x1c"), (vec![DETACH_PREFIX], false));
    }

    #[test]
    fn detach_prefix_before_anything_else_is_sent_as_typed() {
        let mut key = DetachKey::default();

        assert_eq!(key.feed(b"\x1cq"), (vec![DETACH_PREFIX, b'q'], false));
        assert_eq!(key.feed(b"d"), (b"d".to_vec(), false));
    }

    #[test]
    fn the_last_client_to_attach_sets_the_size() {
        let mut viewers = Viewers::default();
        viewers.attached(1, size(40, 20), false, false);

        assert_eq!(
            viewers.attached(2, size(200, 50), false, true),
            Some(size(200, 50))
        );
        assert_eq!(viewers.resized(1, size(45, 20)), None);
        assert_eq!(viewers.resized(2, size(180, 50)), Some(size(180, 50)));
    }

    #[test]
    fn typing_hands_the_size_to_whoever_typed() {
        let mut viewers = Viewers::default();
        viewers.attached(1, size(40, 20), false, false);
        viewers.attached(2, size(200, 50), false, true);

        assert_eq!(viewers.typed(1), Typed::Write(Some(size(40, 20))));
        assert_eq!(viewers.typed(1), Typed::Write(None), "already in use");
        assert_eq!(viewers.resized(2, size(190, 50)), None);
        assert_eq!(viewers.typed(2), Typed::Write(Some(size(190, 50))));
    }

    #[test]
    fn the_size_goes_back_when_the_client_in_use_leaves() {
        let mut viewers = Viewers::default();
        viewers.attached(1, size(40, 20), false, false);
        viewers.attached(2, size(200, 50), false, true);

        assert_eq!(viewers.left(1), None, "was not the one in use");
        viewers.attached(3, size(100, 30), false, true);
        assert_eq!(viewers.left(3), Some(size(200, 50)));
        assert_eq!(viewers.left(2), None, "nobody left");
        assert_eq!(viewers.holder(), None);
    }

    #[test]
    fn a_client_that_joined_control_attaches_as_a_viewer_while_the_session_is_held() {
        let mut viewers = Viewers::default();
        viewers.attached(1, size(200, 50), true, false);

        assert_eq!(viewers.attached(2, size(40, 20), true, true), None);
        assert_eq!(viewers.holder(), Some(1));
        assert_eq!(viewers.size(), Some(size(200, 50)));
        assert_eq!(viewers.resized(2, size(45, 20)), None);
    }

    #[test]
    fn a_client_that_joined_control_takes_a_session_nobody_is_looking_at() {
        let mut viewers = Viewers::default();
        viewers.attached(1, size(200, 50), true, false);

        assert_eq!(
            viewers.attached(2, size(40, 20), true, false),
            Some(size(40, 20))
        );
        assert_eq!(viewers.holder(), Some(2));
    }

    #[test]
    fn a_viewer_cannot_type_until_it_takes_control() {
        let mut viewers = Viewers::default();
        viewers.attached(1, size(200, 50), true, false);
        viewers.attached(2, size(40, 20), true, true);

        assert_eq!(viewers.typed(2), Typed::Drop);
        assert_eq!(viewers.holder(), Some(1));
        assert_eq!(viewers.take(2), Some(size(40, 20)));
        assert_eq!(viewers.take(2), None, "already holds it");
        assert_eq!(viewers.typed(2), Typed::Write(None));
        assert_eq!(viewers.typed(1), Typed::Drop);
    }

    #[test]
    fn an_older_app_still_takes_the_session_by_typing() {
        let mut viewers = Viewers::default();
        viewers.attached(1, size(200, 50), true, false);
        viewers.attached(2, size(40, 20), false, true);

        assert_eq!(viewers.holder(), Some(2), "attaching takes it, as before");
        viewers.take(1);
        assert_eq!(viewers.typed(2), Typed::Write(Some(size(40, 20))));
        assert_eq!(viewers.joined().collect::<Vec<_>>(), vec![1]);
    }

    #[test]
    fn control_goes_to_the_last_used_client_when_the_holder_leaves() {
        let mut viewers = Viewers::default();
        viewers.attached(1, size(200, 50), true, false);
        viewers.attached(2, size(40, 20), true, true);
        viewers.attached(3, size(100, 30), true, true);

        assert_eq!(viewers.left(1), Some(size(100, 30)));
        assert_eq!(viewers.holder(), Some(3));
    }

    #[test]
    fn a_device_control_round_trips_and_refuses_junk() {
        let payload = device_control(DEVICE_TAKE, "abc-1");
        assert_eq!(
            parse_device_control(&payload).unwrap(),
            (DEVICE_TAKE, "abc-1")
        );
        assert!(parse_device_control(&device_control(7, "abc")).is_err());
        assert!(parse_device_control(&device_control(DEVICE_AWAY, "a;b")).is_err());
        assert!(parse_device_control(&[]).is_err());
        assert!(!valid_device_id(""));
        assert!(!valid_device_id(&"x".repeat(65)));
    }

    #[test]
    fn the_newest_client_of_a_device_comes_first() {
        let devices = std::collections::HashMap::from([
            (3_u64, "phone1;phone;UA".to_owned()),
            (7_u64, "phone1;phone;UA".to_owned()),
            (5_u64, "desk;desktop;RA".to_owned()),
        ]);
        assert_eq!(clients_of_device(&devices, "phone1"), vec![7, 3]);
        assert!(clients_of_device(&devices, "none").is_empty());
    }

    #[test]
    fn a_device_is_known_by_its_install_id() {
        assert!(same_device("abc;phone;UGl4ZWw", "abc;phone;TmV3"));
        assert!(!same_device("abc;phone;UGl4ZWw", "abd;phone;UGl4ZWw"));
        assert!(!same_device(";phone;UGl4ZWw", ";phone;UGl4ZWw"));
    }

    #[test]
    fn the_control_state_becomes_a_marker_for_the_app() {
        let payload = encode_control_state(false, size(200, 50), "abc;desktop;TWFj");

        assert_eq!(
            control_marker(&payload).unwrap(),
            b"\x1b]5384;termphin-control;other;200;50;abc;desktop;TWFj\x07".to_vec()
        );
        assert_eq!(
            control_marker(&encode_control_state(true, size(40, 20), "")).unwrap(),
            b"\x1b]5384;termphin-control;you;40;20;\x07".to_vec()
        );
        let mut forged = encode_control_state(false, size(1, 1), "");
        forged.extend_from_slice(b"\x07rm");
        assert_eq!(control_marker(&forged), None);
        assert_eq!(control_marker(&[1, 0, 0]), None);
    }

    #[test]
    fn taking_control_is_taken_out_of_what_is_typed() {
        let mut keys = PresenceKeys::default();

        let (forward, events) = keys.feed(b"a\x1b]5386;control;take\x07b");
        assert_eq!(forward, b"ab");
        assert_eq!(events, vec![PresenceEvent::TakeControl]);

        let (first, none) = keys.feed(b"a\x1b]5386;cont");
        assert_eq!((first, none), (b"a".to_vec(), vec![]));
        let (rest, events) = keys.feed(b"rol;take\x07");
        assert!(rest.is_empty());
        assert_eq!(events, vec![PresenceEvent::TakeControl]);
    }

    #[test]
    fn an_activity_field_round_trips() {
        for activity in [
            Activity {
                state: ActivityState::Waiting,
                command: Some("claude".to_owned()),
            },
            Activity {
                state: ActivityState::Idle,
                command: None,
            },
        ] {
            assert_eq!(Activity::parse(&activity.field()), Some(activity));
        }
    }

    #[test]
    fn a_status_line_is_read_back() {
        assert_eq!(
            SessionInfo::parse("work\t1\t1000\t2"),
            Some(SessionInfo {
                name: "work".to_owned(),
                created_at: 1000,
                clients: 2,
                devices: Vec::new(),
                activity: None,
            })
        );
        assert_eq!(SessionInfo::parse("work\t1"), None);
        assert_eq!(
            SessionInfo::parse("work\t1\t1000\t2\tid1;phone;QQ,bad device,id2;desktop;Qg")
                .unwrap()
                .devices,
            vec!["id1;phone;QQ".to_owned(), "id2;desktop;Qg".to_owned()]
        );
        assert_eq!(
            SessionInfo::parse("work\t1\t1000\t0\t\tworking;bnBtIHJ1biBidWlsZA")
                .unwrap()
                .activity,
            Some(Activity {
                state: ActivityState::Working,
                command: Some("npm run build".to_owned()),
            })
        );
        assert_eq!(
            SessionInfo::parse("work\t1\t1000\t0\t\tidle")
                .unwrap()
                .activity,
            Some(Activity {
                state: ActivityState::Idle,
                command: None,
            })
        );
        assert_eq!(
            SessionInfo::parse("work\t1\t1000\t0\t\tdancing")
                .unwrap()
                .activity,
            None
        );
    }

    #[test]
    fn sessions_are_listed_for_people() {
        let sessions = [
            SessionInfo {
                name: "work".to_owned(),
                created_at: 10_000 - 7200,
                clients: 0,
                devices: Vec::new(),
                activity: None,
            },
            SessionInfo {
                name: "deploy-prod".to_owned(),
                created_at: 10_000 - 30,
                clients: 2,
                devices: Vec::new(),
                activity: None,
            },
        ];

        assert_eq!(
            format_sessions(&sessions, 10_000),
            "work         detached, started 2h ago\n\
             deploy-prod  2 clients, started just now\n"
        );
    }

    #[test]
    fn a_new_session_gets_the_first_free_name() {
        let named = |names: &[&str]| {
            names
                .iter()
                .map(|name| SessionInfo {
                    name: (*name).to_owned(),
                    created_at: 0,
                    clients: 0,
                    devices: Vec::new(),
                    activity: None,
                })
                .collect::<Vec<_>>()
        };

        assert_eq!(free_name(&named(&[])), "shell");
        assert_eq!(free_name(&named(&["shell", "shell-2"])), "shell-3");
    }

    #[cfg(unix)]
    fn at(log: &OutputLog, offset: u64) -> OutputPosition {
        OutputPosition {
            epoch: log.position().epoch,
            offset,
        }
    }

    #[test]
    #[cfg(unix)]
    fn output_log_returns_what_was_missed() {
        let mut log = OutputLog::new(16);
        log.push(b"hello ");
        let position = log.position();
        log.push(b"world");

        assert_eq!(log.since(position).as_deref(), Some(&b"world"[..]));
        assert_eq!(log.since(log.position()).as_deref(), Some(&b""[..]));
    }

    #[test]
    #[cfg(unix)]
    fn output_log_refuses_an_offset_it_has_dropped() {
        let mut log = OutputLog::new(8);
        log.push(b"0123456789");

        assert_eq!(log.position().offset, 10);
        assert_eq!(log.since(at(&log, 1)), None);
        assert_eq!(log.since(at(&log, 2)).as_deref(), Some(&b"23456789"[..]));
    }

    #[test]
    #[cfg(unix)]
    fn output_log_refuses_an_offset_past_its_end() {
        let mut log = OutputLog::new(8);
        log.push(b"abc");

        assert_eq!(log.since(at(&log, 4)), None);
    }

    #[test]
    #[cfg(unix)]
    fn output_log_keeps_the_newest_bytes_across_pushes() {
        let mut log = OutputLog::new(4);
        log.push(b"ab");
        log.push(b"cde");

        assert_eq!(log.since(at(&log, 1)).as_deref(), Some(&b"bcde"[..]));
        assert_eq!(log.since(at(&log, 0)), None);
    }

    #[test]
    #[cfg(unix)]
    fn output_log_refuses_a_position_in_another_masters_output() {
        // A master restored after a crash numbers its output from zero again;
        // an offset the client kept from the old one must not land in it.
        let mut old = OutputLog::new(64);
        old.push(b"before the crash");
        let kept = old.position();
        let mut new = OutputLog::new(64);
        new.push(b"a restored session, already further along");

        assert_ne!(kept.epoch, new.position().epoch);
        assert_eq!(new.since(kept), None);
    }

    #[test]
    #[cfg(unix)]
    fn history_logs_what_it_is_given() {
        let mut history = History::new(5, 20);
        history.push(b"one");
        let position = history.output().position();
        history.push(b"two");

        assert_eq!(history.output().position().offset, 6);
        assert_eq!(
            history.output().since(position).as_deref(),
            Some(&b"two"[..])
        );
    }

    #[test]
    #[cfg(unix)]
    fn output_positions_survive_every_encoding() {
        let position = OutputPosition {
            epoch: 7,
            offset: 42,
        };

        assert_eq!(
            offset_marker(position),
            b"\x1b]5383;termphin-offset;7:42\x07"
        );
        assert_eq!(
            OutputPosition::decode(&position.encode()).unwrap(),
            position
        );
        assert_eq!(OutputPosition::parse("7:42"), Some(position));
        assert!(OutputPosition::decode(b"short").is_err());
        assert_eq!(OutputPosition::parse("42"), None);
    }

    #[test]
    fn boot_id_differs_only_when_both_present_and_different() {
        assert!(!boot_id_differs(Some("same-boot"), Some("same-boot")));
        assert!(!boot_id_differs(Some("same-boot\n"), Some("same-boot")));
        assert!(boot_id_differs(Some("old-boot"), Some("new-boot")));
        assert!(!boot_id_differs(None, Some("new-boot")));
        assert!(!boot_id_differs(Some("old-boot"), None));
    }

    fn temp_dir(label: &str) -> PathBuf {
        let dir = env::temp_dir().join(format!(
            "termphin-agent-test-{label}-{}-{}",
            process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn restore_state_loads_persisted_cwd_and_scrollback() {
        let dir = temp_dir("cwd-scrollback");
        std::fs::write(dir.join("cwd"), dir.to_string_lossy().as_bytes()).unwrap();
        std::fs::write(dir.join("scrollback"), b"previous output").unwrap();

        let restore = RestoreState::load(&dir, None);
        assert_eq!(restore.cwd.as_deref(), Some(dir.as_path()));
        assert_eq!(restore.scrollback, b"previous output");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn restore_state_drops_cwd_that_no_longer_exists() {
        let dir = temp_dir("missing-cwd");
        std::fs::write(dir.join("cwd"), b"/does/not/exist/anywhere").unwrap();

        let restore = RestoreState::load(&dir, None);
        assert_eq!(restore.cwd, None);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn leftovers_are_a_restore_even_when_the_machine_never_rebooted() {
        // A master killed by the OOM killer leaves the same directory behind
        // as one that went down with the machine, and the shell the client
        // gets back is new either way.
        let dir = temp_dir("same-boot");
        std::fs::write(dir.join("boot_id"), "same-boot").unwrap();
        let restore = RestoreState::load(&dir, Some("same-boot"));
        assert!(restore.restored);
        assert!(!restore.reboot_restored);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn restore_state_with_no_persisted_boot_id_is_not_a_reboot() {
        let dir = temp_dir("no-boot-id");
        let restore = RestoreState::load(&dir, Some("current"));
        assert!(!restore.reboot_restored);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn assert_same_screen(expected: &History, replayed: &vt100::Parser) {
        let (rows, cols) = expected.parser.screen().size();
        assert_eq!(replayed.screen().size(), (rows, cols));
        for row in 0..rows {
            for col in 0..cols {
                assert_eq!(
                    expected.parser.screen().cell(row, col),
                    replayed.screen().cell(row, col),
                    "cell mismatch at row {row} col {col}"
                );
            }
        }
        assert_eq!(
            expected.parser.screen().cursor_position(),
            replayed.screen().cursor_position()
        );
        assert_eq!(
            expected.parser.screen().alternate_screen(),
            replayed.screen().alternate_screen()
        );
    }

    fn assert_same_scrollback(expected: &mut History, replayed: &mut vt100::Parser) {
        expected.parser.screen_mut().set_scrollback(usize::MAX);
        replayed.screen_mut().set_scrollback(usize::MAX);

        let expected_depth = expected.parser.screen().scrollback();
        let replayed_depth = replayed.screen().scrollback();
        assert_eq!(expected_depth, replayed_depth, "scrollback depth mismatch");

        let (_, cols) = expected.parser.screen().size();

        for n in (1..=expected_depth).rev() {
            expected.parser.screen_mut().set_scrollback(n);
            replayed.screen_mut().set_scrollback(n);

            for col in 0..cols {
                let expected_cell = expected.parser.screen().cell(0, col);
                let replayed_cell = replayed.screen().cell(0, col);
                assert_eq!(
                    expected_cell, replayed_cell,
                    "scrollback cell mismatch at offset {n} col {col}"
                );
            }

            if n > 1 {
                let expected_wrapped = expected.parser.screen().row_wrapped(0);
                let replayed_wrapped = replayed.screen().row_wrapped(0);
                assert_eq!(
                    expected_wrapped, replayed_wrapped,
                    "row_wrapped mismatch at offset {n}"
                );
            }
        }

        expected.parser.screen_mut().set_scrollback(0);
        replayed.screen_mut().set_scrollback(0);
    }

    #[cfg(unix)]
    #[test]
    fn versions_compare_by_number_not_by_text() {
        assert!(newer_version("0.16.0", "0.15.0"));
        assert!(newer_version("0.15.10", "0.15.9"));
        assert!(newer_version("1.0.0", "0.99.99"));
        assert!(!newer_version("0.15.0", "0.15.0"));
        assert!(!newer_version("0.14.0", "0.15.0"));
        assert!(!newer_version("garbage", "0.15.0"));
    }

    #[cfg(unix)]
    #[test]
    fn an_output_log_taken_apart_and_rebuilt_resumes_the_same() {
        let mut log = OutputLog::new(8);
        log.push(b"hello, world");
        let position = OutputPosition {
            epoch: log.position().epoch,
            offset: 9,
        };
        let (epoch, total, bytes) = log.parts();
        let rebuilt = OutputLog::from_parts(epoch, total, &bytes, 8);
        assert_eq!(rebuilt.position(), log.position());
        assert_eq!(rebuilt.since(position), log.since(position));
        assert_eq!(rebuilt.since(position).as_deref(), Some(b"rld".as_slice()));
    }

    #[cfg(unix)]
    #[test]
    fn a_resumed_history_draws_the_same_screen_and_keeps_its_offsets() {
        let mut history = History::new(5, 20);
        history.push(b"line one\r\nline two\r\n\x1b[1mbold\x1b[m");
        let (epoch, total, bytes) = history.output().parts();
        let screen = history.snapshot();
        let mut resumed = History::resumed(
            5,
            20,
            &screen,
            OutputLog::from_parts(epoch, total, &bytes, OUTPUT_LOG_BYTES),
            false,
        );
        assert_eq!(resumed.output().position(), history.output().position());
        assert_eq!(resumed.snapshot(), history.snapshot());
    }

    #[test]
    fn seed_restored_carries_scrollback_and_marks_it() {
        let mut history = History::new(24, 80);
        history.seed_restored(b"$ echo hi\r\nhi\r\n", true);
        let snapshot = history.snapshot();
        let text = String::from_utf8(snapshot.clone()).unwrap();
        assert!(text.contains("$ echo hi"));
        assert!(text.contains("restored after a server restart"));
        assert!(snapshot.starts_with(REBOOT_RESTORED_MARKER));
    }

    #[test]
    fn seed_restored_still_marks_a_session_whose_scrollback_was_lost() {
        let mut history = History::new(24, 80);
        history.seed_restored(b"", false);
        let snapshot = history.snapshot();
        assert!(snapshot.starts_with(REBOOT_RESTORED_MARKER));
        let text = String::from_utf8(snapshot).unwrap();
        assert!(text.contains("the previous shell is gone"));
    }

    #[test]
    fn seed_restored_snapshot_replays_into_a_fresh_terminal() {
        let mut history = History::new(10, 40);
        history.seed_restored(b"$ old output\r\n", true);
        history.push(b"$ new prompt ");
        let snapshot = history.snapshot();
        let mut replayed = vt100::Parser::new(10, 40, 0);
        replayed.process(&snapshot);
        assert_same_screen(&history, &replayed);
    }

    #[test]
    fn only_long_untouched_session_leftovers_are_swept() {
        let now = std::time::SystemTime::now();
        assert!(!is_abandoned(now, now));
        assert!(!is_abandoned(
            now - ABANDONED_SESSION_RETENTION + std::time::Duration::from_secs(60),
            now
        ));
        assert!(is_abandoned(now - ABANDONED_SESSION_RETENTION, now));
        // A directory stamped in the future (a clock that jumped) is left be.
        assert!(!is_abandoned(
            now + std::time::Duration::from_secs(600),
            now
        ));
    }

    #[test]
    fn validates_session_names() {
        assert!(validate_name("termphin_ab12-CD").is_ok());
        assert!(validate_name("").is_err());
        assert!(validate_name("with/slash").is_err());
        assert!(validate_name("with space").is_err());
        assert!(validate_name(&"a".repeat(65)).is_err());
    }

    #[test]
    fn terminal_size_round_trips() {
        let size = TermSize {
            cols: 132,
            rows: 47,
        };
        let decoded = decode_size(&encode_size(size)).unwrap();
        assert_eq!(decoded, size);
    }

    #[test]
    fn snapshot_survives_heavy_relative_cursor_movement() {
        let mut history = History::new(10, 40);
        for step in 0..600 {
            let frame = format!("\r\x1b[2C\x1b[3A\x1b[6B\x1b[7Dstep {step:03}\x1b[39m");
            history.push(frame.as_bytes());
        }
        history.push(b"\x1b[10;1Hdone\x1b[1;31m!\x1b[0m");
        let snapshot = history.snapshot();
        let mut replayed = vt100::Parser::new(10, 40, 0);
        replayed.process(&snapshot);
        assert_same_screen(&history, &replayed);
    }

    #[test]
    fn alternate_screen_snapshot_round_trips() {
        let mut history = History::new(10, 40);
        history.push(b"shell output before the app\r\n");
        history.push(b"\x1b[?1049h\x1b[H\x1b[44m full-screen app \x1b[0m\x1b[?1000h");
        let snapshot = history.snapshot();
        assert!(snapshot.windows(8).any(|window| window == b"\x1b[?1049h"));
        let mut replayed = vt100::Parser::new(10, 40, 0);
        replayed.process(&snapshot);
        assert!(replayed.screen().alternate_screen());
        assert_same_screen(&history, &replayed);
    }

    #[test]
    fn alternate_screen_active_suppresses_scrollback_text() {
        let mut history = History::new(5, 20);
        for index in 0..30 {
            history.push(format!("line {index:02}\r\n").as_bytes());
        }
        history.push(b"\x1b[?1049h\x1b[Halt-screen");
        let snapshot = String::from_utf8(history.snapshot()).unwrap();
        assert!(!snapshot.contains("line 00"));
        assert!(!snapshot.contains("line 29"));
        assert!(snapshot.contains("alt-screen"));
    }

    #[test]
    fn input_modes_round_trip_through_a_fresh_terminal() {
        let mut history = History::new(10, 40);
        history.push(b"text\x1b[?1002;1006h\x1b[?25l\x1b=");
        let snapshot = history.snapshot();
        let mut replayed = vt100::Parser::new(10, 40, 0);
        replayed.process(&snapshot);
        assert_eq!(
            replayed.screen().mouse_protocol_mode(),
            history.parser.screen().mouse_protocol_mode()
        );
        assert_eq!(
            replayed.screen().mouse_protocol_encoding(),
            history.parser.screen().mouse_protocol_encoding()
        );
        assert_eq!(
            replayed.screen().application_keypad(),
            history.parser.screen().application_keypad()
        );
        assert_eq!(
            replayed.screen().hide_cursor(),
            history.parser.screen().hide_cursor()
        );
        let text = String::from_utf8(snapshot).unwrap();
        assert!(!text.contains("\x1b[?3h"));
        assert!(!text.contains("\x1b[?5h"));
    }

    #[test]
    fn title_is_replayed_with_the_snapshot() {
        let mut history = History::new(5, 20);
        history.push(b"\x1b]0;claude code\x07");
        assert!(history.snapshot().starts_with(b"\x1b]0;claude code\x07"));

        history.push(b"\x1b]2;opencode\x1b\\");
        assert!(history.snapshot().starts_with(b"\x1b]0;opencode\x07"));
    }

    #[test]
    fn scrollback_text_is_replayed_in_order_before_the_screen() {
        let mut history = History::new(5, 20);
        for index in 0..30 {
            history.push(format!("line {index:02}\r\n").as_bytes());
        }
        history.push(b"prompt $ ");
        let snapshot = String::from_utf8(history.snapshot()).unwrap();
        let needles = [
            "line 00",
            "line 12",
            "line 24",
            "line 25",
            "line 28",
            "prompt $ ",
        ];
        let positions = needles.map(|needle| {
            snapshot
                .find(needle)
                .unwrap_or_else(|| panic!("missing {needle}"))
        });
        assert!(positions.is_sorted());

        let mut replayed = vt100::Parser::new(5, 20, SCROLLBACK_ROWS);
        replayed.process(snapshot.as_bytes());
        replayed.screen_mut().set_scrollback(usize::MAX);
        assert!(replayed.screen().contents().starts_with("line 00"));
        replayed.screen_mut().set_scrollback(0);
        let screen = replayed.screen().contents();
        assert!(screen.contains("line 28"));
        assert!(screen.contains("prompt $ "));
    }

    #[test]
    fn notify_writes_an_rxvt_notification() {
        assert_eq!(
            notify_sequence("Build", "finished in 42s"),
            b"\x1b]777;notify;Build;finished in 42s\x07"
        );
    }

    #[test]
    fn notify_cannot_end_its_own_sequence_early() {
        assert_eq!(
            notify_sequence("a;b", "line\x07one\nline two"),
            b"\x1b]777;notify;a b;line one line two\x07"
        );
    }

    #[test]
    fn scrollback_keeps_ten_thousand_rows() {
        let mut history = History::new(5, 20);
        for index in 0..12_000 {
            history.push(format!("line {index:05}\r\n").as_bytes());
        }
        let snapshot = String::from_utf8(history.snapshot()).unwrap();
        assert!(snapshot.contains("line 02100"));
        assert!(snapshot.contains("line 11999"));
        assert!(!snapshot.contains("line 01900"));
    }

    #[cfg(unix)]
    #[test]
    fn only_a_changed_history_asks_to_be_saved() {
        let mut history = History::new(5, 20);
        assert!(history.take_unsaved());
        assert!(!history.take_unsaved());
        history.push(b"output");
        assert!(history.take_unsaved());
        assert!(!history.take_unsaved());
        history.resize(6, 20);
        assert!(history.take_unsaved());
    }

    #[test]
    fn replay_is_split_into_sendable_frames() {
        let mut history = History::new(50, 200);
        for index in 0..2000 {
            let line = format!("line {index:04} ");
            let padding = "x".repeat(150);
            history.push(format!("{line}{padding}\r\n").as_bytes());
        }
        let snapshot = history.snapshot();
        const { assert!(REPLAY_CHUNK_SIZE <= MAX_FRAME_SIZE) };
        assert!(snapshot.len() > REPLAY_CHUNK_SIZE);
        assert!(
            snapshot
                .chunks(REPLAY_CHUNK_SIZE)
                .all(|chunk| chunk.len() <= MAX_FRAME_SIZE)
        );
    }

    #[test]
    fn scrollback_keeps_colours_and_attributes() {
        let mut history = History::new(5, 20);
        history.push(b"\x1b[31mred\x1b[0m plain\r\n");
        history.push(b"\x1b[1;4mbold\x1b[0m\r\n");
        history.push(b"\x1b[38;5;208mx\x1b[48;2;1;2;3my\x1b[0m\r\n");
        for i in 0..10 {
            history.push(format!("plain line {i}\r\n").as_bytes());
        }

        let snapshot = history.snapshot();
        let mut replayed = vt100::Parser::new(5, 20, SCROLLBACK_ROWS);
        replayed.process(&snapshot);

        assert_same_scrollback(&mut history, &mut replayed);

        replayed.screen_mut().set_scrollback(usize::MAX);
        assert_eq!(
            replayed.screen().cell(0, 0).unwrap().fgcolor(),
            vt100::Color::Idx(1)
        );
    }

    #[test]
    fn scrollback_keeps_coloured_blank_tail() {
        let mut history = History::new(5, 20);
        history.push(b"\x1b[44mab\x1b[K\x1b[0m\r\n");
        for i in 0..10 {
            history.push(format!("plain line {i}\r\n").as_bytes());
        }

        let snapshot = history.snapshot();
        let mut replayed = vt100::Parser::new(5, 20, SCROLLBACK_ROWS);
        replayed.process(&snapshot);

        assert_same_scrollback(&mut history, &mut replayed);

        replayed.screen_mut().set_scrollback(usize::MAX);
        assert_eq!(
            replayed.screen().cell(0, 15).unwrap().bgcolor(),
            vt100::Color::Idx(4)
        );
    }

    #[test]
    fn wrapped_scrollback_lines_stay_joined() {
        let mut history = History::new(5, 20);
        let long_line = "x".repeat(45);
        history.push(format!("{long_line}\r\n").as_bytes());
        for i in 0..10 {
            history.push(format!("plain line {i}\r\n").as_bytes());
        }

        let snapshot = history.snapshot();
        let mut replayed = vt100::Parser::new(5, 20, SCROLLBACK_ROWS);
        replayed.process(&snapshot);

        assert_same_scrollback(&mut history, &mut replayed);

        replayed.screen_mut().set_scrollback(usize::MAX);
        let contents = replayed.screen().contents();
        assert!(contents.contains(&long_line));
    }

    fn ping(seq: usize) -> String {
        format!(
            "64 bytes from 1.1.1.1: icmp_seq={seq} ttl=55 time=7.8{} ms",
            seq % 10
        )
    }

    /// Every line a replayed terminal holds, scrollback first, with the rows
    /// a wrap carried on joined back into one and the blank rows below the
    /// last line dropped.
    fn logical_lines(parser: &mut vt100::Parser) -> Vec<String> {
        let (rows, cols) = parser.screen().size();
        let mut lines = Vec::new();
        let mut current = String::new();
        let mut take = |screen: &vt100::Screen, row: u16| {
            current.push_str(&screen.contents_between(row, 0, row, cols));
            if !screen.row_wrapped(row) {
                lines.push(std::mem::take(&mut current));
            }
        };
        parser.screen_mut().set_scrollback(usize::MAX);
        for n in (1..=parser.screen().scrollback()).rev() {
            parser.screen_mut().set_scrollback(n);
            take(parser.screen(), 0);
        }
        parser.screen_mut().set_scrollback(0);
        for row in 0..rows {
            take(parser.screen(), row);
        }
        while lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        lines
    }

    #[test]
    fn a_line_wrapped_across_the_top_of_the_screen_is_replayed_whole() {
        let mut history = History::new(20, 50);
        for seq in 1..=30 {
            history.push(format!("{}\r\n", ping(seq)).as_bytes());
        }

        let mut replayed = vt100::Parser::new(20, 50, SCROLLBACK_ROWS);
        replayed.process(&history.snapshot());

        let expected: Vec<String> = (1..=30).map(ping).collect();
        assert_eq!(logical_lines(&mut replayed), expected);
        assert_same_screen(&history, &replayed);
    }

    #[test]
    fn a_shorter_client_is_replayed_no_blank_rows() {
        let mut history = History::new(20, 50);
        for seq in 1..=30 {
            history.push(format!("{}\r\n", ping(seq)).as_bytes());
        }

        let mut replayed = vt100::Parser::new(12, 50, SCROLLBACK_ROWS);
        replayed.process(&history.snapshot_for(12));

        let expected: Vec<String> = (1..=30).map(ping).collect();
        assert_eq!(logical_lines(&mut replayed), expected);
        let (row, col) = replayed.screen().cursor_position();
        assert_eq!((row, col), (11, 0));
        assert_eq!(
            replayed.screen().contents_between(10, 0, 10, 50),
            &ping(30)[50..]
        );
    }

    #[test]
    fn a_taller_client_keeps_the_scrollback_the_screen_covered() {
        let mut history = History::new(12, 50);
        for seq in 1..=30 {
            history.push(format!("{}\r\n", ping(seq)).as_bytes());
        }

        let mut replayed = vt100::Parser::new(20, 50, SCROLLBACK_ROWS);
        replayed.process(&history.snapshot_for(20));

        let expected: Vec<String> = (1..=30).map(ping).collect();
        assert_eq!(logical_lines(&mut replayed), expected);
    }

    #[test]
    fn the_cursor_lands_where_it_was_on_a_shorter_client() {
        let mut history = History::new(20, 50);
        for seq in 1..=30 {
            history.push(format!("{}\r\n", ping(seq)).as_bytes());
        }
        history.push(b"$ echo hi\x1b[3D");

        let mut replayed = vt100::Parser::new(12, 50, SCROLLBACK_ROWS);
        replayed.process(&history.snapshot_for(12));

        assert_eq!(replayed.screen().cursor_position(), (11, 6));
        assert_eq!(
            replayed.screen().contents_between(11, 0, 11, 50),
            "$ echo hi"
        );
    }
}
