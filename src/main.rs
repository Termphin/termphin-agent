use std::collections::{BTreeSet, HashMap, VecDeque};
use std::env;
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const PROTOCOL_VERSION: u32 = 1;
const MAX_FRAME_SIZE: usize = 1024 * 1024;
/// Sized against the client's 2000-row transcript, which fits inside this even
/// with heavy SGR markup. Anything beyond it is evicted on reattach anyway, so
/// a larger ring only buys a slower replay.
const HISTORY_SIZE: usize = 256 * 1024;

/// History is replayed in pieces because one frame may not exceed
/// [`MAX_FRAME_SIZE`]. Small enough that the client starts painting early.
const REPLAY_CHUNK_SIZE: usize = 128 * 1024;

/// Queued-but-unwritten bytes a single client may accumulate before the master
/// gives up on it. Must comfortably exceed a full history replay.
const MAX_CLIENT_BACKLOG: usize = 8 * 1024 * 1024;

/// Cap on how long a single frame write may block before the client counts as
/// wedged. Only bounds one write; the backlog limit bounds the total.
const CLIENT_WRITE_TIMEOUT: Duration = Duration::from_secs(60);

const FRAME_ATTACH: u8 = 1;
const FRAME_INPUT: u8 = 2;
const FRAME_RESIZE: u8 = 3;
const FRAME_STATUS: u8 = 4;
const FRAME_RENAME: u8 = 5;
const FRAME_KILL: u8 = 6;
const FRAME_HISTORY: u8 = 7;

const FRAME_OUTPUT: u8 = 101;
const FRAME_STATUS_RESPONSE: u8 = 102;
const FRAME_OK: u8 = 103;
const FRAME_ERROR: u8 = 104;
const FRAME_EXIT: u8 = 105;
/// Sent after the last replay chunk, so the client can tell a complete
/// scrollback from one still arriving. Older clients ignore unknown frames.
const FRAME_REPLAY_DONE: u8 = 106;

/// Written to the terminal once the replay is complete, so the app can show the
/// restored screen in one piece instead of line by line. Terminal emulators
/// drop unknown OSCs, so it stays invisible if it reaches a real terminal.
const REPLAY_END_MARKER: &[u8] = b"\x1b]5380;termphin-replay-end\x07";

static RESIZE_PENDING: AtomicBool = AtomicBool::new(false);

fn main() {
    if let Err(error) = run() {
        eprintln!("termphin-agent: {error}");
        process::exit(1);
    }
}

fn run() -> io::Result<()> {
    let mut args = env::args().skip(1);
    let command = args.next().unwrap_or_else(|| "help".to_owned());
    match command.as_str() {
        "--version" => {
            println!(
                "termphin-agent {} protocol {}",
                env!("CARGO_PKG_VERSION"),
                PROTOCOL_VERSION
            );
        }
        "version" => {
            if args.next().as_deref() == Some("--machine") {
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
        "attach" => {
            let first = args.next();
            let (replay, name) = if first.as_deref() == Some("--replay") {
                (true, required_name(args.next())?)
            } else {
                (false, required_name(first)?)
            };
            attach_command(&name, replay)?;
        }
        "list" => list_command()?,
        "rename" => {
            let old_name = required_name(args.next())?;
            let new_name = required_name(args.next())?;
            rename_command(&old_name, &new_name)?;
        }
        "kill" => kill_command(&required_name(args.next())?)?,
        _ => {
            eprintln!("usage: termphin-agent <attach|list|rename|kill|version>");
            process::exit(2);
        }
    }
    Ok(())
}

fn required_name(value: Option<String>) -> io::Result<String> {
    let name = value.ok_or_else(|| invalid_input("missing session name"))?;
    validate_name(&name)?;
    Ok(name)
}

fn validate_name(name: &str) -> io::Result<()> {
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

fn invalid_input(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn base_dir() -> io::Result<PathBuf> {
    let home = env::var_os("HOME").ok_or_else(|| invalid_input("HOME is not set"))?;
    Ok(PathBuf::from(home)
        .join(".cache")
        .join("termphin")
        .join("sessions"))
}

fn session_dir(name: &str) -> io::Result<PathBuf> {
    validate_name(name)?;
    Ok(base_dir()?.join(name))
}

fn socket_path(name: &str) -> io::Result<PathBuf> {
    Ok(session_dir(name)?.join("control.sock"))
}

fn prepare_base_dir() -> io::Result<PathBuf> {
    let base = base_dir()?;
    fs::create_dir_all(&base)?;
    let metadata = fs::symlink_metadata(&base)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::other("session base path is not a directory"));
    }
    fs::set_permissions(&base, fs::Permissions::from_mode(0o700))?;
    Ok(base)
}

struct CreationLock {
    file: File,
}

impl CreationLock {
    fn acquire() -> io::Result<Self> {
        let base = prepare_base_dir()?;
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(base.join(".lock"))?;
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { file })
    }
}

impl Drop for CreationLock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

fn attach_command(name: &str, replay: bool) -> io::Result<()> {
    install_attach_signal_handlers()?;
    let _raw_mode = RawModeGuard::enable(libc::STDIN_FILENO)?;

    if !replay {
        try_attach(name, false)?;
        return Ok(());
    }
    if try_attach(name, true)? == Attachment::ReplayRejected {
        // Masters started by an older build refuse a history larger than one
        // frame. Reaching a session without its scrollback beats refusing to
        // reach it at all, so drop the replay and attach again.
        try_attach(name, false)?;
    }
    Ok(())
}

fn try_attach(name: &str, replay: bool) -> io::Result<Attachment> {
    let size = terminal_size(libc::STDIN_FILENO);
    let mut stream = connect_or_create(name, size)?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    if replay {
        // Protocol-v1 masters older than 0.2 return an error for this optional
        // frame, then still accept FRAME_ATTACH. The client ignores that one
        // compatibility error below.
        send_frame(&mut stream, FRAME_HISTORY, &[])?;
    }
    send_frame(&mut stream, FRAME_ATTACH, &encode_size(size))?;
    bridge_terminal(stream, replay)
}

#[derive(PartialEq, Eq)]
enum Attachment {
    /// The session ended, or the user detached.
    Finished,
    /// The master refused the attach while replaying, before any output
    /// reached the terminal, so retrying without a replay is still safe.
    ReplayRejected,
}

fn connect_or_create(name: &str, size: libc::winsize) -> io::Result<UnixStream> {
    if let Ok(stream) = UnixStream::connect(socket_path(name)?) {
        return Ok(stream);
    }

    let lock = CreationLock::acquire()?;
    if let Ok(stream) = UnixStream::connect(socket_path(name)?) {
        return Ok(stream);
    }

    let directory = session_dir(name)?;
    if directory.exists() {
        fs::remove_dir_all(&directory)?;
    }
    fs::create_dir(&directory)?;
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;

    if let Err(error) = spawn_master(name, size) {
        let _ = fs::remove_dir_all(&directory);
        return Err(error);
    }
    drop(lock);

    for _ in 0..40 {
        match UnixStream::connect(socket_path(name)?) {
            Ok(stream) => return Ok(stream),
            Err(_) => thread::sleep(Duration::from_millis(25)),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "session master did not start",
    ))
}

fn spawn_master(name: &str, size: libc::winsize) -> io::Result<()> {
    let mut pipe_fds = [0; 2];
    if unsafe { libc::pipe2(pipe_fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        close_fd(pipe_fds[0]);
        close_fd(pipe_fds[1]);
        return Err(io::Error::last_os_error());
    }
    if pid == 0 {
        close_fd(pipe_fds[0]);
        master_process(name.to_owned(), size, pipe_fds[1]);
    }

    close_fd(pipe_fds[1]);
    let mut ready = [0_u8; 1];
    let read = unsafe { libc::read(pipe_fds[0], ready.as_mut_ptr().cast(), 1) };
    close_fd(pipe_fds[0]);
    if read == 1 && ready[0] == 1 {
        Ok(())
    } else {
        Err(io::Error::other("session master failed to initialize"))
    }
}

fn master_process(name: String, size: libc::winsize, ready_fd: RawFd) -> ! {
    let setup = (|| -> io::Result<(UnixListener, File, Arc<MasterState>)> {
        if unsafe { libc::setsid() } < 0 {
            return Err(io::Error::last_os_error());
        }
        unsafe {
            libc::signal(libc::SIGHUP, libc::SIG_IGN);
            libc::signal(libc::SIGPIPE, libc::SIG_IGN);
        }
        redirect_stdio()?;

        let directory = session_dir(&name)?;
        let path = directory.join("control.sock");
        let listener = UnixListener::bind(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;

        let mut master_fd = -1;
        let mut slave_fd = -1;
        if unsafe {
            libc::openpty(
                &mut master_fd,
                &mut slave_fd,
                std::ptr::null_mut(),
                std::ptr::null(),
                &size,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }

        let shell_pid = spawn_shell(master_fd, slave_fd, listener.as_raw_fd(), ready_fd)?;
        close_fd(slave_fd);

        let created_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        fs::write(directory.join("created_at"), created_at.to_string())?;
        fs::set_permissions(
            directory.join("created_at"),
            fs::Permissions::from_mode(0o600),
        )?;

        let reader = unsafe { File::from_raw_fd(master_fd) };
        let writer = reader.try_clone()?;
        let state = Arc::new(MasterState {
            name: Mutex::new(name),
            directory: Mutex::new(directory),
            created_at,
            shell_pid,
            pty: Mutex::new(writer),
            clients: Mutex::new(HashMap::new()),
            history: Mutex::new(History::default()),
            terminating: AtomicBool::new(false),
        });
        Ok((listener, reader, state))
    })();

    let (listener, reader, state) = match setup {
        Ok(value) => value,
        Err(_) => {
            write_ready(ready_fd, false);
            unsafe { libc::_exit(1) }
        }
    };

    write_ready(ready_fd, true);
    run_master(listener, reader, state);
}

fn redirect_stdio() -> io::Result<()> {
    let path = CString::new("/dev/null").expect("static string");
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    for target in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
        if unsafe { libc::dup2(fd, target) } < 0 {
            close_fd(fd);
            return Err(io::Error::last_os_error());
        }
    }
    if fd > libc::STDERR_FILENO {
        close_fd(fd);
    }
    Ok(())
}

fn spawn_shell(
    master_fd: RawFd,
    slave_fd: RawFd,
    listener_fd: RawFd,
    ready_fd: RawFd,
) -> io::Result<libc::pid_t> {
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        close_fd(master_fd);
        close_fd(slave_fd);
        return Err(io::Error::last_os_error());
    }
    if pid != 0 {
        return Ok(pid);
    }

    close_fd(master_fd);
    close_fd(listener_fd);
    close_fd(ready_fd);
    if unsafe { libc::setsid() } < 0 {
        unsafe { libc::_exit(126) }
    }
    unsafe {
        libc::ioctl(slave_fd, libc::TIOCSCTTY as _, 0);
        libc::tcsetpgrp(slave_fd, libc::getpid());
    }
    for target in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
        if unsafe { libc::dup2(slave_fd, target) } < 0 {
            unsafe { libc::_exit(126) }
        }
    }
    if slave_fd > libc::STDERR_FILENO {
        close_fd(slave_fd);
    }
    unsafe {
        libc::signal(libc::SIGHUP, libc::SIG_DFL);
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        libc::signal(libc::SIGINT, libc::SIG_DFL);
        libc::signal(libc::SIGTERM, libc::SIG_DFL);
    }

    let shell = env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_owned());
    let shell_c = match CString::new(shell.as_bytes()) {
        Ok(value) => value,
        Err(_) => unsafe { libc::_exit(126) },
    };
    let login = CString::new("-l").expect("static string");
    let argv = [shell_c.as_ptr(), login.as_ptr(), std::ptr::null()];
    unsafe {
        libc::execv(shell_c.as_ptr(), argv.as_ptr());
        libc::_exit(127)
    }
}

fn write_ready(fd: RawFd, ready: bool) {
    let byte = [u8::from(ready)];
    unsafe {
        libc::write(fd, byte.as_ptr().cast(), 1);
    }
    close_fd(fd);
}

fn run_master(listener: UnixListener, mut pty_reader: File, state: Arc<MasterState>) -> ! {
    let reader_state = Arc::clone(&state);
    thread::spawn(move || {
        let mut buffer = [0_u8; 8192];
        loop {
            match pty_reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => reader_state.broadcast_output(&buffer[..count]),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
                Err(_) => break,
            }
        }
        reader_state.finish();
    });

    static NEXT_CLIENT_ID: AtomicU64 = AtomicU64::new(1);
    for connection in listener.incoming() {
        match connection {
            Ok(stream) => {
                // A connection costs two threads and a frame of buffer, so a
                // peer opening them in a loop must not exhaust the master.
                let Some(slot) = ClientSlot::acquire() else {
                    continue;
                };
                let id = NEXT_CLIENT_ID.fetch_add(1, Ordering::Relaxed);
                let client_state = Arc::clone(&state);
                thread::spawn(move || {
                    let _slot = slot;
                    client_loop(id, stream, client_state)
                });
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    state.kill_shell();
    state.finish();
}

#[derive(Default)]
struct History {
    bytes: VecDeque<u8>,
    /// Bytes ever written by the shell, including those already evicted. Used
    /// to tell whether a mode-setting sequence is still inside the ring.
    total: u64,
    modes: ModeTracker,
}

impl History {
    fn push(&mut self, data: &[u8]) {
        self.modes.consume(data, self.total);
        self.total += data.len() as u64;
        self.bytes.extend(data);
        while self.bytes.len() > HISTORY_SIZE {
            self.bytes.pop_front();
        }
    }

    /// Stream offset of the oldest byte still held in the ring.
    fn retained_from(&self) -> u64 {
        self.total - self.bytes.len() as u64
    }

    fn snapshot(&self) -> Vec<u8> {
        let mut output = Vec::with_capacity(self.bytes.len() + 64);
        output.extend(self.modes.replay_prefix(self.retained_from()));
        output.extend(self.bytes.iter().copied());
        output.extend(self.modes.replay_suffix());
        output
    }

    fn alternate_screen_active(&self) -> bool {
        self.modes.alternate.is_some()
    }
}

/// Switching to the alternate screen also clears it, so these can never be
/// replayed after the content they were meant to introduce.
const ALTERNATE_SCREEN_MODES: [u16; 3] = [1049, 1047, 47];

/// Private modes that are on by default, so only an explicit reset is worth
/// replaying.
const DEFAULT_ON_MODES: [u16; 2] = [7, 25];

/// Private modes that change how the client must encode keyboard, paste and
/// scroll input. Anything outside this list is left alone: re-asserting
/// arbitrary modes (`?3h` resizes and clears, `?5h` inverts the screen) does
/// more damage than the state it would restore.
const REPLAYABLE_MODES: [u16; 13] = [
    1, 9, 66, 1000, 1001, 1002, 1003, 1004, 1005, 1006, 1015, 1016, 2004,
];

#[derive(Default)]
struct ModeTracker {
    state: u8,
    sequence: Vec<u8>,
    /// Escape byte offset of the sequence currently being parsed.
    sequence_offset: u64,
    /// Active alternate-screen mode plus the offset of the sequence that
    /// turned it on, so [`replay_prefix`] can tell whether the ring still
    /// carries it.
    alternate: Option<(u16, u64)>,
    enabled_private_modes: BTreeSet<u16>,
    reset_default_on_modes: BTreeSet<u16>,
    application_keypad: bool,
    /// Body of an OSC sequence (`ESC ] ... BEL`/`ESC ] ... ST`) currently
    /// being parsed, e.g. `0;my title`.
    osc_buffer: Vec<u8>,
    /// Most recent window title (OSC 0 or 2), kept for the life of the
    /// session regardless of the ring - unlike the bytes that set it, this
    /// never gets evicted, so a reattach can restore it even long after the
    /// original sequence has scrolled out of history.
    last_title: Option<String>,
}

impl ModeTracker {
    fn consume(&mut self, data: &[u8], base: u64) {
        for (index, &byte) in data.iter().enumerate() {
            let offset = base + index as u64;
            match self.state {
                0 if byte == 0x1b => {
                    self.state = 1;
                    self.sequence_offset = offset;
                }
                1 if byte == b'[' => {
                    self.state = 2;
                    self.sequence.clear();
                }
                1 if byte == b']' => {
                    self.state = 3;
                    self.osc_buffer.clear();
                }
                1 if byte == b'=' => {
                    self.application_keypad = true;
                    self.state = 0;
                }
                1 if byte == b'>' => {
                    self.application_keypad = false;
                    self.state = 0;
                }
                1 if byte == 0x1b => self.sequence_offset = offset,
                1 => self.state = 0,
                2 if (0x40..=0x7e).contains(&byte) => {
                    self.finish_csi(byte);
                    self.state = 0;
                    self.sequence.clear();
                }
                2 if self.sequence.len() < 64 => self.sequence.push(byte),
                2 => {
                    self.state = 0;
                    self.sequence.clear();
                }
                // OSC body, terminated by BEL or ST (`ESC \`).
                3 if byte == 0x07 => {
                    self.finish_osc();
                    self.state = 0;
                }
                3 if byte == 0x1b => self.state = 4,
                3 if self.osc_buffer.len() < 512 => self.osc_buffer.push(byte),
                3 => {
                    self.state = 0;
                    self.osc_buffer.clear();
                }
                4 if byte == b'\\' => {
                    self.finish_osc();
                    self.state = 0;
                }
                // Not a valid ST after all - the ESC starts whatever comes next.
                4 => {
                    self.state = 1;
                    self.sequence_offset = offset;
                    self.osc_buffer.clear();
                }
                _ => self.state = 0,
            }
        }
    }

    /// Keeps the title (OSC 0 or 2) as long as this state lives, which is
    /// what lets [`replay_prefix`] restore it after the setting sequence
    /// itself has fallen out of the ring.
    fn finish_osc(&mut self) {
        if let Ok(text) = std::str::from_utf8(&self.osc_buffer)
            && let Some(title) = text.strip_prefix("0;").or_else(|| text.strip_prefix("2;"))
        {
            self.last_title = Some(title.to_string());
        }
        self.osc_buffer.clear();
    }

    fn finish_csi(&mut self, final_byte: u8) {
        if !matches!(final_byte, b'h' | b'l') || self.sequence.first() != Some(&b'?') {
            return;
        }
        let Ok(parameters) = std::str::from_utf8(&self.sequence[1..]) else {
            return;
        };
        let set = final_byte == b'h';
        for mode in parameters
            .split(';')
            .filter_map(|value| value.parse::<u16>().ok())
        {
            if ALTERNATE_SCREEN_MODES.contains(&mode) {
                self.alternate = set.then_some((mode, self.sequence_offset));
            } else if DEFAULT_ON_MODES.contains(&mode) {
                if set {
                    self.reset_default_on_modes.remove(&mode);
                } else {
                    self.reset_default_on_modes.insert(mode);
                }
            } else if REPLAYABLE_MODES.contains(&mode) {
                if set {
                    self.enabled_private_modes.insert(mode);
                } else {
                    self.enabled_private_modes.remove(&mode);
                }
            }
        }
    }

    /// Re-enters the alternate screen only when the sequence that did so has
    /// already been evicted from the ring - otherwise the replayed bytes take
    /// care of it, and repeating it here would clear what they just drew.
    fn replay_prefix(&self, retained_from: u64) -> Vec<u8> {
        let mut prefix = match self.alternate {
            Some((mode, offset)) if offset < retained_from => format!("\x1b[?{mode}h").into_bytes(),
            _ => Vec::new(),
        };
        // Always resent, not just when evicted: a client that just missed the
        // original sequence and one that still has it in its own scrollback
        // both end up with the same, currently-correct title either way.
        if let Some(title) = &self.last_title {
            prefix.extend(format!("\x1b]0;{title}\x07").into_bytes());
        }
        prefix
    }

    fn replay_suffix(&self) -> Vec<u8> {
        let mut suffix = Vec::new();
        for mode in &self.enabled_private_modes {
            suffix.extend(format!("\x1b[?{mode}h").as_bytes());
        }
        for mode in &self.reset_default_on_modes {
            suffix.extend(format!("\x1b[?{mode}l").as_bytes());
        }
        if self.application_keypad {
            suffix.extend(b"\x1b=");
        }
        suffix
    }
}

/// Concurrent connections the master will serve.
const MAX_CLIENTS: usize = 16;

/// How long a connection may take to identify itself. Cleared once the client
/// attaches, because an attached client is expected to sit idle for hours.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

static ACTIVE_CLIENTS: AtomicUsize = AtomicUsize::new(0);

/// Holds one of the [`MAX_CLIENTS`] slots for as long as it is alive.
struct ClientSlot;

impl ClientSlot {
    fn acquire() -> Option<Self> {
        let taken = ACTIVE_CLIENTS.fetch_add(1, Ordering::SeqCst);
        if taken >= MAX_CLIENTS {
            ACTIVE_CLIENTS.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(Self)
    }
}

impl Drop for ClientSlot {
    fn drop(&mut self) {
        ACTIVE_CLIENTS.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Outbound half of one attached client.
///
/// A dedicated thread writes queued frames, so neither the history lock nor the
/// PTY reader waits on a slow socket. A client that falls too far behind is
/// dropped rather than allowed to stall the session.
struct ClientChannel {
    queue: Mutex<ClientQueue>,
    ready: Condvar,
    /// Second handle on the same socket, used to unblock the reader thread in
    /// [`client_loop`] once the writer gives up.
    shutdown: UnixStream,
}

#[derive(Default)]
struct ClientQueue {
    frames: VecDeque<(u8, Vec<u8>)>,
    bytes: usize,
    closed: bool,
}

impl ClientChannel {
    fn spawn(stream: UnixStream) -> io::Result<Arc<Self>> {
        let writer_stream = stream.try_clone()?;
        writer_stream.set_write_timeout(Some(CLIENT_WRITE_TIMEOUT))?;
        let channel = Arc::new(Self {
            queue: Mutex::new(ClientQueue::default()),
            ready: Condvar::new(),
            shutdown: stream,
        });
        let writer = Arc::clone(&channel);
        thread::spawn(move || writer.run(writer_stream));
        Ok(channel)
    }

    fn send(&self, kind: u8, payload: &[u8]) -> io::Result<()> {
        let mut queue = self.queue.lock().expect("client queue mutex poisoned");
        if queue.closed {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "client is disconnected",
            ));
        }
        if queue.bytes + payload.len() > MAX_CLIENT_BACKLOG {
            drop(queue);
            self.close();
            return Err(io::Error::other("client is not keeping up"));
        }
        queue.bytes += payload.len();
        queue.frames.push_back((kind, payload.to_vec()));
        self.ready.notify_all();
        Ok(())
    }

    fn close(&self) {
        let mut queue = self.queue.lock().expect("client queue mutex poisoned");
        queue.closed = true;
        queue.frames.clear();
        queue.bytes = 0;
        drop(queue);
        self.ready.notify_all();
        let _ = self.shutdown.shutdown(std::net::Shutdown::Both);
    }

    /// Blocks until every queued frame has been written or [`deadline`] passes.
    fn drain(&self, deadline: Duration) {
        let end = Instant::now() + deadline;
        let mut queue = self.queue.lock().expect("client queue mutex poisoned");
        // `bytes` drops to zero only once a frame has left the socket; an
        // empty frame list alone would race the final write.
        while !queue.closed && queue.bytes > 0 {
            let Some(remaining) = end.checked_duration_since(Instant::now()) else {
                return;
            };
            let (next, timeout) = self
                .ready
                .wait_timeout(queue, remaining)
                .expect("client queue mutex poisoned");
            if timeout.timed_out() {
                return;
            }
            queue = next;
        }
    }

    fn run(&self, mut stream: UnixStream) {
        loop {
            let frame = {
                let mut queue = self.queue.lock().expect("client queue mutex poisoned");
                loop {
                    if let Some(frame) = queue.frames.pop_front() {
                        break frame;
                    }
                    if queue.closed {
                        return;
                    }
                    queue = self.ready.wait(queue).expect("client queue mutex poisoned");
                }
            };
            let written = send_frame(&mut stream, frame.0, &frame.1);
            // Counted only once the frame is out, so drain() and the backlog
            // limit cover bytes in flight. close() may have zeroed the count
            // meanwhile, so this has to saturate.
            let mut queue = self.queue.lock().expect("client queue mutex poisoned");
            queue.bytes = queue.bytes.saturating_sub(frame.1.len());
            drop(queue);
            self.ready.notify_all();
            if written.is_err() {
                self.close();
                return;
            }
        }
    }
}

struct MasterState {
    name: Mutex<String>,
    directory: Mutex<PathBuf>,
    created_at: u64,
    shell_pid: libc::pid_t,
    pty: Mutex<File>,
    clients: Mutex<HashMap<u64, Arc<ClientChannel>>>,
    history: Mutex<History>,
    terminating: AtomicBool,
}

impl MasterState {
    fn add_client(&self, id: u64, channel: Arc<ClientChannel>, replay: bool) -> io::Result<()> {
        // Held across both steps so output arriving mid-replay queues behind the
        // snapshot. Queueing never blocks, so a slow client cannot hold the PTY
        // reader hostage.
        let history = self.history.lock().expect("history mutex poisoned");
        if replay {
            for chunk in history.snapshot().chunks(REPLAY_CHUNK_SIZE) {
                channel.send(FRAME_OUTPUT, chunk)?;
            }
            channel.send(FRAME_REPLAY_DONE, &[])?;
        }
        self.clients
            .lock()
            .expect("clients mutex poisoned")
            .insert(id, channel);
        Ok(())
    }

    fn remove_client(&self, id: u64) {
        self.clients
            .lock()
            .expect("clients mutex poisoned")
            .remove(&id);
    }

    fn broadcast_output(&self, data: &[u8]) {
        let mut history = self.history.lock().expect("history mutex poisoned");
        history.push(data);
        let clients = self.clients.lock().expect("clients mutex poisoned");
        if clients.is_empty() {
            return;
        }
        let failed = clients
            .iter()
            .filter_map(|(id, channel)| channel.send(FRAME_OUTPUT, data).err().map(|_| *id))
            .collect::<Vec<_>>();
        drop(clients);
        drop(history);
        if !failed.is_empty() {
            let mut clients = self.clients.lock().expect("clients mutex poisoned");
            for id in failed {
                clients.remove(&id);
            }
        }
    }

    fn write_input(&self, data: &[u8]) -> io::Result<()> {
        self.pty.lock().expect("pty mutex poisoned").write_all(data)
    }

    fn window_size(&self) -> Option<libc::winsize> {
        let fd = self.pty.lock().expect("pty mutex poisoned").as_raw_fd();
        let mut size = unsafe { std::mem::zeroed::<libc::winsize>() };
        (unsafe { libc::ioctl(fd, libc::TIOCGWINSZ as _, &mut size) } == 0).then_some(size)
    }

    fn set_window_size(&self, size: libc::winsize) -> io::Result<()> {
        let fd = self.pty.lock().expect("pty mutex poisoned").as_raw_fd();
        if unsafe { libc::ioctl(fd, libc::TIOCSWINSZ as _, &size) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Applies [`size`] to the PTY. Returns whether it actually changed -
    /// the kernel only raises SIGWINCH on a real change, which is what tells
    /// a full-screen app to repaint.
    fn resize(&self, size: libc::winsize) -> io::Result<bool> {
        let previous = self.window_size();
        let changed = previous
            .map(|current| current.ws_row != size.ws_row || current.ws_col != size.ws_col)
            .unwrap_or(true);
        self.set_window_size(size)?;
        unsafe {
            libc::kill(-self.shell_pid, libc::SIGWINCH);
        }
        Ok(changed)
    }

    /// Nudges a full-screen application into repainting by briefly shrinking
    /// the PTY by one row.
    ///
    /// The alternate buffer has no scrollback to replay, so making the
    /// application draw again is the only portable way to recover the frame.
    /// Skipped elsewhere, where the replay is already exact and a spurious
    /// resize would reflow the prompt for nothing.
    fn request_redraw(&self) {
        if !self
            .history
            .lock()
            .expect("history mutex poisoned")
            .alternate_screen_active()
        {
            return;
        }
        let Some(size) = self.window_size() else {
            return;
        };
        if size.ws_row < 2 {
            return;
        }
        let mut shrunk = size;
        shrunk.ws_row -= 1;
        if self.set_window_size(shrunk).is_err() {
            return;
        }
        thread::sleep(Duration::from_millis(40));
        let _ = self.set_window_size(size);
    }

    fn status(&self) -> String {
        let name = self.name.lock().expect("name mutex poisoned");
        let attached = self.clients.lock().expect("clients mutex poisoned").len();
        format!("{name}\t1\t{}\t{attached}", self.created_at)
    }

    fn rename(&self, new_name: &str) -> io::Result<()> {
        validate_name(new_name)?;
        let _lock = CreationLock::acquire()?;
        let mut directory = self.directory.lock().expect("directory mutex poisoned");
        let destination = base_dir()?.join(new_name);
        if destination.exists() {
            if UnixStream::connect(destination.join("control.sock")).is_ok() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "session name is already in use",
                ));
            }
            // A directory left behind by a session that never shut down
            // cleanly (SIGKILL, power loss, ...) - nothing is listening on
            // its socket, so the name is free to reclaim rather than stuck
            // forever. Mirrors the same self-heal `connect_or_create` does
            // on attach.
            fs::remove_dir_all(&destination)?;
        }
        fs::rename(&*directory, &destination)?;
        *directory = destination;
        *self.name.lock().expect("name mutex poisoned") = new_name.to_owned();
        Ok(())
    }

    fn kill_shell(&self) {
        if self.terminating.swap(true, Ordering::SeqCst) {
            return;
        }
        unsafe {
            libc::kill(-self.shell_pid, libc::SIGHUP);
        }
        let pid = self.shell_pid;
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(2));
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        });
    }

    fn finish(&self) -> ! {
        if !self.terminating.swap(true, Ordering::SeqCst) {
            let writers = self
                .clients
                .lock()
                .expect("clients mutex poisoned")
                .values()
                .cloned()
                .collect::<Vec<_>>();
            for writer in &writers {
                let _ = writer.send(FRAME_EXIT, &[]);
            }
            // Background threads do the writing, so give them a moment before
            // the process exits and takes the sockets down.
            for writer in &writers {
                writer.drain(Duration::from_secs(2));
            }
        }
        unsafe {
            libc::waitpid(self.shell_pid, std::ptr::null_mut(), 0);
        }
        let directory = self
            .directory
            .lock()
            .expect("directory mutex poisoned")
            .clone();
        let _ = fs::remove_dir_all(directory);
        process::exit(0)
    }
}

fn client_loop(id: u64, mut stream: UnixStream, state: Arc<MasterState>) {
    let writer = match stream.try_clone().and_then(ClientChannel::spawn) {
        Ok(writer) => writer,
        Err(_) => return,
    };
    // Bounded until attach, so a peer that stalls mid-frame cannot hold a
    // thread and its buffer forever.
    let _ = stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT));
    let mut attached = false;
    let mut replay_requested = false;

    while let Ok((kind, payload)) = read_frame(&mut stream) {
        let outcome = match kind {
            FRAME_HISTORY if !attached => {
                replay_requested = true;
                Ok(())
            }
            FRAME_ATTACH => {
                let attach_result = if !attached {
                    attached = true;
                    // An attached client may idle for hours between keystrokes.
                    let _ = stream.set_read_timeout(None);
                    state.add_client(id, Arc::clone(&writer), replay_requested)
                } else {
                    Ok(())
                };
                attach_result
                    .and_then(|_| decode_size(&payload))
                    .and_then(|size| state.resize(size))
                    .map(|resized| {
                        // A changed size already makes the application repaint,
                        // so only ask for a redraw when it did not.
                        if !resized {
                            state.request_redraw();
                        }
                    })
            }
            FRAME_INPUT if attached => state.write_input(&payload),
            FRAME_RESIZE if attached => decode_size(&payload)
                .and_then(|size| state.resize(size))
                .map(|_| ()),
            FRAME_STATUS => writer.send(FRAME_STATUS_RESPONSE, state.status().as_bytes()),
            FRAME_RENAME => match String::from_utf8(payload) {
                Ok(name) => state
                    .rename(&name)
                    .and_then(|_| writer.send(FRAME_OK, state.status().as_bytes())),
                Err(_) => Err(invalid_input("session name is not valid UTF-8")),
            },
            FRAME_KILL => {
                let response = writer.send(FRAME_OK, &[]);
                state.kill_shell();
                response
            }
            _ => Err(invalid_input("invalid protocol frame")),
        };
        if let Err(error) = outcome {
            let _ = writer.send(FRAME_ERROR, error.to_string().as_bytes());
            if kind == FRAME_ATTACH || kind == FRAME_INPUT || kind == FRAME_RESIZE {
                writer.drain(Duration::from_secs(2));
                break;
            }
        }
    }
    if attached {
        state.remove_client(id);
    }
    writer.close();
}

fn list_command() -> io::Result<()> {
    let base = prepare_base_dir()?;
    let mut directories = fs::read_dir(base)?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false))
        .collect::<Vec<_>>();
    directories.sort_by_key(|entry| entry.file_name());

    for directory in directories {
        let path = directory.path().join("control.sock");
        let Ok(mut stream) = UnixStream::connect(path) else {
            continue;
        };
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        send_frame(&mut stream, FRAME_STATUS, &[])?;
        if let Ok((FRAME_STATUS_RESPONSE, payload)) = read_frame(&mut stream)
            && let Ok(status) = String::from_utf8(payload)
        {
            println!("{status}");
        }
    }
    Ok(())
}

fn rename_command(old_name: &str, new_name: &str) -> io::Result<()> {
    validate_name(new_name)?;
    control_command(old_name, FRAME_RENAME, new_name.as_bytes())
}

fn kill_command(name: &str) -> io::Result<()> {
    control_command(name, FRAME_KILL, &[])
}

fn control_command(name: &str, kind: u8, payload: &[u8]) -> io::Result<()> {
    let mut stream = UnixStream::connect(socket_path(name)?)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    send_frame(&mut stream, kind, payload)?;
    match read_frame(&mut stream)? {
        (FRAME_OK, _) => Ok(()),
        (FRAME_ERROR, message) => Err(io::Error::other(
            String::from_utf8_lossy(&message).into_owned(),
        )),
        _ => Err(io::Error::other("invalid response from session master")),
    }
}

fn bridge_terminal(mut stream: UnixStream, replay_requested: bool) -> io::Result<Attachment> {
    let socket_fd = stream.as_raw_fd();
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    let mut buffer = [0_u8; 8192];
    let mut tolerate_legacy_replay_error = replay_requested;
    let mut painted = false;

    loop {
        if RESIZE_PENDING.swap(false, Ordering::SeqCst) {
            send_frame(
                &mut stream,
                FRAME_RESIZE,
                &encode_size(terminal_size(libc::STDIN_FILENO)),
            )?;
        }

        let mut poll_fds = [
            libc::pollfd {
                fd: libc::STDIN_FILENO,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: socket_fd,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let result = unsafe { libc::poll(poll_fds.as_mut_ptr(), poll_fds.len() as _, -1) };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }

        if poll_fds[0].revents & libc::POLLIN != 0 {
            let count = input.read(&mut buffer)?;
            if count == 0 {
                return Ok(Attachment::Finished);
            }
            send_frame(&mut stream, FRAME_INPUT, &buffer[..count])?;
        }
        if poll_fds[0].revents & (libc::POLLHUP | libc::POLLERR) != 0 {
            return Ok(Attachment::Finished);
        }

        if poll_fds[1].revents & libc::POLLIN != 0 {
            let frame = match read_frame(&mut stream) {
                Ok(frame) => frame,
                // An older master drops the connection outright instead of
                // reporting that the history did not fit in one frame. With
                // nothing painted yet, a plain attach is still worth trying.
                Err(_) if replay_requested && !painted => {
                    return Ok(Attachment::ReplayRejected);
                }
                Err(error) => return Err(error),
            };
            match frame {
                (FRAME_OUTPUT, data) => {
                    painted = true;
                    output.write_all(&data)?;
                    output.flush()?;
                }
                (FRAME_REPLAY_DONE, _) => {
                    // Writing the marker counts as painting: a retry without
                    // the replay would emit it a second time, and the app
                    // would take the first one for the whole history.
                    painted = true;
                    output.write_all(REPLAY_END_MARKER)?;
                    output.flush()?;
                }
                (FRAME_EXIT, _) => return Ok(Attachment::Finished),
                (FRAME_ERROR, message) => {
                    if tolerate_legacy_replay_error
                        && message.as_slice() == b"invalid protocol frame"
                    {
                        tolerate_legacy_replay_error = false;
                        continue;
                    }
                    if replay_requested && !painted {
                        // Nothing has been drawn yet, so the caller can still
                        // start over without a replay.
                        return Ok(Attachment::ReplayRejected);
                    }
                    return Err(io::Error::other(
                        String::from_utf8_lossy(&message).into_owned(),
                    ));
                }
                _ => {}
            }
        }
        if poll_fds[1].revents & (libc::POLLHUP | libc::POLLERR) != 0 {
            return Ok(Attachment::Finished);
        }
    }
}

fn send_frame(stream: &mut UnixStream, kind: u8, payload: &[u8]) -> io::Result<()> {
    if payload.len() > MAX_FRAME_SIZE {
        return Err(invalid_input("protocol frame is too large"));
    }
    let mut header = [0_u8; 5];
    header[0] = kind;
    header[1..].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    stream.write_all(&header)?;
    stream.write_all(payload)
}

fn read_frame(stream: &mut UnixStream) -> io::Result<(u8, Vec<u8>)> {
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

fn encode_size(size: libc::winsize) -> [u8; 8] {
    let mut payload = [0_u8; 8];
    payload[..4].copy_from_slice(&(u32::from(size.ws_col)).to_be_bytes());
    payload[4..].copy_from_slice(&(u32::from(size.ws_row)).to_be_bytes());
    payload
}

fn decode_size(payload: &[u8]) -> io::Result<libc::winsize> {
    if payload.len() != 8 {
        return Err(invalid_input("invalid terminal size frame"));
    }
    let columns = u32::from_be_bytes(payload[..4].try_into().expect("four bytes"));
    let rows = u32::from_be_bytes(payload[4..].try_into().expect("four bytes"));
    if columns == 0 || rows == 0 || columns > u16::MAX.into() || rows > u16::MAX.into() {
        return Err(invalid_input("invalid terminal size"));
    }
    Ok(libc::winsize {
        ws_row: rows as u16,
        ws_col: columns as u16,
        ws_xpixel: 0,
        ws_ypixel: 0,
    })
}

fn terminal_size(fd: RawFd) -> libc::winsize {
    let mut size = libc::winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    unsafe {
        libc::ioctl(fd, libc::TIOCGWINSZ as _, &mut size);
    }
    if size.ws_col == 0 {
        size.ws_col = 80;
    }
    if size.ws_row == 0 {
        size.ws_row = 24;
    }
    size
}

struct RawModeGuard {
    fd: RawFd,
    original: Option<libc::termios>,
}

impl RawModeGuard {
    fn enable(fd: RawFd) -> io::Result<Self> {
        if unsafe { libc::isatty(fd) } != 1 {
            return Ok(Self { fd, original: None });
        }
        let mut original = unsafe { std::mem::zeroed::<libc::termios>() };
        if unsafe { libc::tcgetattr(fd, &mut original) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut raw = original;
        unsafe {
            libc::cfmakeraw(&mut raw);
        }
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            fd,
            original: Some(original),
        })
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        if let Some(original) = &self.original {
            unsafe {
                libc::tcsetattr(self.fd, libc::TCSANOW, original);
            }
        }
    }
}

extern "C" fn handle_winch(_: libc::c_int) {
    RESIZE_PENDING.store(true, Ordering::SeqCst);
}

fn install_attach_signal_handlers() -> io::Result<()> {
    let mut action = unsafe { std::mem::zeroed::<libc::sigaction>() };
    action.sa_sigaction = handle_winch as *const () as usize;
    action.sa_flags = 0;
    unsafe {
        libc::sigemptyset(&mut action.sa_mask);
        if libc::sigaction(libc::SIGWINCH, &action, std::ptr::null_mut()) != 0 {
            return Err(io::Error::last_os_error());
        }
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
    Ok(())
}

fn close_fd(fd: RawFd) {
    if fd >= 0 {
        unsafe {
            libc::close(fd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let size = libc::winsize {
            ws_row: 47,
            ws_col: 132,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let decoded = decode_size(&encode_size(size)).unwrap();
        assert_eq!(decoded.ws_row, 47);
        assert_eq!(decoded.ws_col, 132);
    }

    #[test]
    fn history_is_bounded_and_ordered() {
        let mut history = History::default();
        history.push(&vec![b'a'; HISTORY_SIZE]);
        history.push(b"bc");
        let data = history.snapshot();
        assert_eq!(data.len(), HISTORY_SIZE);
        assert_eq!(&data[data.len() - 2..], b"bc");
    }

    #[test]
    fn input_modes_are_replayed_after_history() {
        let mut history = History::default();
        history.push(b"before\x1b[?1002;1006hafter");
        let snapshot = history.snapshot();
        assert!(snapshot.ends_with(b"\x1b[?1002h\x1b[?1006h"));

        history.push(b"\x1b[?1002l");
        let snapshot = history.snapshot();
        assert!(snapshot.ends_with(b"\x1b[?1006h"));
    }

    #[test]
    fn alternate_screen_is_never_re_entered_after_its_content() {
        let mut history = History::default();
        history.push(b"\x1b[?1049h\x1b[?1000hpainted");
        let snapshot = history.snapshot();
        // Re-entering the alternate screen clears it, so a trailing 1049h
        // would wipe exactly the frame the replay just reconstructed.
        assert!(!snapshot.ends_with(b"\x1b[?1049h"));
        assert!(snapshot.starts_with(b"\x1b[?1049h\x1b[?1000hpainted"));
        assert!(snapshot.ends_with(b"\x1b[?1000h"));
    }

    #[test]
    fn alternate_screen_is_restored_once_its_sequence_is_evicted() {
        let mut history = History::default();
        history.push(b"\x1b[?1049h");
        history.push(&vec![b'x'; HISTORY_SIZE]);
        let snapshot = history.snapshot();
        assert!(snapshot.starts_with(b"\x1b[?1049h"));
        assert_eq!(
            snapshot.iter().filter(|byte| **byte == b'x').count(),
            HISTORY_SIZE
        );
    }

    #[test]
    fn title_survives_eviction_of_the_sequence_that_set_it() {
        let mut history = History::default();
        history.push(b"\x1b]0;claude code\x07");
        history.push(&vec![b'x'; HISTORY_SIZE]);
        let snapshot = history.snapshot();
        // The setting sequence itself is long gone from the ring, but the
        // title it set is resent so a client attaching now still learns it.
        assert!(snapshot.starts_with(b"\x1b]0;claude code\x07"));
        assert_eq!(
            snapshot.iter().filter(|byte| **byte == b'x').count(),
            HISTORY_SIZE
        );
    }

    #[test]
    fn title_terminated_with_st_is_recognised() {
        let mut history = History::default();
        history.push(b"\x1b]2;opencode\x1b\\");
        let snapshot = history.snapshot();
        assert!(snapshot.starts_with(b"\x1b]0;opencode\x07"));
    }

    #[test]
    fn later_title_replaces_earlier_one() {
        let mut history = History::default();
        history.push(b"\x1b]0;first\x07\x1b]0;second\x07");
        let snapshot = history.snapshot();
        assert!(snapshot.starts_with(b"\x1b]0;second\x07"));
    }

    #[test]
    fn destructive_and_unknown_modes_are_not_replayed() {
        let mut history = History::default();
        history.push(b"\x1b[?3h\x1b[?5h\x1b[?47h\x1b[?25l\x1b=");
        let snapshot = history.snapshot();
        let suffix = &snapshot[b"\x1b[?3h\x1b[?5h\x1b[?47h\x1b[?25l\x1b=".len()..];
        assert_eq!(suffix, b"\x1b[?25l\x1b=");
    }

    #[test]
    fn replay_is_split_into_sendable_frames() {
        let mut history = History::default();
        history.push(&vec![b'a'; HISTORY_SIZE * 2]);
        let snapshot = history.snapshot();
        assert_eq!(snapshot.len(), HISTORY_SIZE);
        const { assert!(REPLAY_CHUNK_SIZE <= MAX_FRAME_SIZE) };
        assert!(
            snapshot
                .chunks(REPLAY_CHUNK_SIZE)
                .all(|chunk| chunk.len() <= MAX_FRAME_SIZE)
        );
    }
}
