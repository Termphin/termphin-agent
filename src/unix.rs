use std::collections::{HashMap, VecDeque};
use std::env;
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::{
    CLIENT_WRITE_TIMEOUT, CWD_POLL_INTERVAL, ClientQueue, FRAME_ATTACH, FRAME_ERROR, FRAME_EXIT,
    FRAME_HISTORY, FRAME_INPUT, FRAME_KILL, FRAME_OK, FRAME_OUTPUT, FRAME_RENAME,
    FRAME_REPLAY_DONE, FRAME_RESIZE, FRAME_STATUS, FRAME_STATUS_RESPONSE, HANDSHAKE_TIMEOUT,
    History, MAX_CLIENTS, REPLAY_CHUNK_SIZE, REPLAY_END_MARKER, RestoreState,
    SCROLLBACK_FLUSH_EVERY_TICKS, TermSize, append_scrollback, decode_size, encode_size,
    invalid_input, read_frame, send_frame, validate_name,
};

static RESIZE_PENDING: AtomicBool = AtomicBool::new(false);

fn to_winsize(size: TermSize) -> libc::winsize {
    libc::winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

fn from_winsize(size: libc::winsize) -> TermSize {
    TermSize {
        cols: size.ws_col,
        rows: size.ws_row,
    }
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

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn proc_listchildpids(
        pid: libc::pid_t,
        buffer: *mut libc::c_void,
        buffersize: libc::c_int,
    ) -> libc::c_int;
    fn termphin_agent_shell_cwd(
        pid: libc::pid_t,
        out: *mut libc::c_char,
        out_len: libc::c_int,
    ) -> libc::c_int;
}

#[cfg(not(target_os = "macos"))]
fn current_boot_id() -> Option<String> {
    fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()
        .map(|s| s.trim().to_owned())
}

#[cfg(target_os = "macos")]
fn current_boot_id() -> Option<String> {
    let mut mib = [libc::CTL_KERN, libc::KERN_BOOTTIME];
    let mut boottime: libc::timeval = unsafe { std::mem::zeroed() };
    let mut size = std::mem::size_of::<libc::timeval>();
    let result = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            (&raw mut boottime).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    (result == 0).then(|| boottime.tv_sec.to_string())
}

#[cfg(not(target_os = "macos"))]
fn shell_cwd(pid: libc::pid_t) -> Option<PathBuf> {
    fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

#[cfg(target_os = "macos")]
fn shell_cwd(pid: libc::pid_t) -> Option<PathBuf> {
    let mut buf = [0_u8; 1024];
    let n =
        unsafe { termphin_agent_shell_cwd(pid, buf.as_mut_ptr().cast(), buf.len() as libc::c_int) };
    (n > 0).then(|| PathBuf::from(std::ffi::OsStr::from_bytes(&buf[..n as usize])))
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

pub(crate) fn attach_command(name: &str, replay: bool) -> io::Result<()> {
    install_attach_signal_handlers()?;
    let _raw_mode = RawModeGuard::enable(libc::STDIN_FILENO)?;
    let scrollback = Arc::new(Mutex::new(VecDeque::new()));
    spawn_client_persistence_thread(name.to_owned(), Arc::clone(&scrollback));

    if !replay {
        try_attach(name, false, &scrollback)?;
        return Ok(());
    }
    if try_attach(name, true, &scrollback)? == Attachment::ReplayRejected {
        // Masters started by an older build refuse a history larger than one
        // frame. Reaching a session without its scrollback beats refusing to
        // reach it at all, so drop the replay and attach again.
        try_attach(name, false, &scrollback)?;
    }
    Ok(())
}

fn try_attach(
    name: &str,
    replay: bool,
    scrollback: &Arc<Mutex<VecDeque<u8>>>,
) -> io::Result<Attachment> {
    let size = terminal_size(libc::STDIN_FILENO);
    let mut stream = connect_or_create(name, size)?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    if replay {
        // Protocol-v1 masters older than 0.2 return an error for this optional
        // frame, then still accept FRAME_ATTACH. The client ignores that one
        // compatibility error below.
        send_frame(&mut stream, FRAME_HISTORY, &[])?;
    }
    send_frame(&mut stream, FRAME_ATTACH, &encode_size(from_winsize(size)))?;
    bridge_terminal(stream, replay, scrollback)
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
    let restore = if directory.exists() {
        let restore = RestoreState::load(&directory, current_boot_id().as_deref());
        fs::remove_dir_all(&directory)?;
        restore
    } else {
        RestoreState::default()
    };
    fs::create_dir(&directory)?;
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;

    if let Err(error) = spawn_master(name, size, restore) {
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

#[cfg(not(target_os = "macos"))]
fn cloexec_pipe() -> io::Result<[RawFd; 2]> {
    let mut pipe_fds = [0; 2];
    if unsafe { libc::pipe2(pipe_fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(pipe_fds)
}

#[cfg(target_os = "macos")]
fn cloexec_pipe() -> io::Result<[RawFd; 2]> {
    let mut pipe_fds = [0; 2];
    if unsafe { libc::pipe(pipe_fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    for fd in pipe_fds {
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(pipe_fds)
}

#[cfg(target_os = "macos")]
unsafe fn call_openpty(
    master_fd: &mut libc::c_int,
    slave_fd: &mut libc::c_int,
    size: &libc::winsize,
) -> libc::c_int {
    let mut size = *size;
    unsafe {
        libc::openpty(
            master_fd,
            slave_fd,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        )
    }
}

#[cfg(not(target_os = "macos"))]
unsafe fn call_openpty(
    master_fd: &mut libc::c_int,
    slave_fd: &mut libc::c_int,
    size: &libc::winsize,
) -> libc::c_int {
    unsafe {
        libc::openpty(
            master_fd,
            slave_fd,
            std::ptr::null_mut(),
            std::ptr::null(),
            size,
        )
    }
}

fn spawn_master(name: &str, size: libc::winsize, restore: RestoreState) -> io::Result<()> {
    let pipe_fds = cloexec_pipe()?;

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        close_fd(pipe_fds[0]);
        close_fd(pipe_fds[1]);
        return Err(io::Error::last_os_error());
    }
    if pid == 0 {
        close_fd(pipe_fds[0]);
        master_process(name.to_owned(), size, pipe_fds[1], restore);
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

fn master_process(name: String, size: libc::winsize, ready_fd: RawFd, restore: RestoreState) -> ! {
    let setup = (|| -> io::Result<(UnixListener, File, Arc<MasterState>)> {
        if unsafe { libc::setsid() } < 0 {
            return Err(io::Error::last_os_error());
        }
        unsafe {
            libc::signal(libc::SIGHUP, libc::SIG_IGN);
            libc::signal(libc::SIGPIPE, libc::SIG_IGN);
        }
        install_master_sigterm_handler()?;
        redirect_stdio()?;

        let directory = session_dir(&name)?;
        let path = directory.join("control.sock");
        let listener = UnixListener::bind(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;

        let mut master_fd = -1;
        let mut slave_fd = -1;
        if unsafe { call_openpty(&mut master_fd, &mut slave_fd, &size) } != 0 {
            return Err(io::Error::last_os_error());
        }

        let shell_pid = spawn_shell(
            master_fd,
            slave_fd,
            listener.as_raw_fd(),
            ready_fd,
            restore.cwd.as_deref(),
        )?;
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

        if let Some(id) = current_boot_id() {
            let _ = fs::write(directory.join("boot_id"), &id);
        }

        let mut history = History::new(size.ws_row, size.ws_col);
        if restore.restored {
            history.seed_restored(&restore.scrollback, restore.reboot_restored);
        }

        let reader = unsafe { File::from_raw_fd(master_fd) };
        let writer = reader.try_clone()?;
        let state = Arc::new(MasterState {
            name: Mutex::new(name),
            directory: Mutex::new(directory),
            created_at,
            shell_pid,
            pty: Mutex::new(writer),
            clients: Mutex::new(HashMap::new()),
            history: Mutex::new(history),
            terminating: AtomicBool::new(false),
        });
        spawn_persistence_thread(Arc::clone(&state));
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
    cwd: Option<&Path>,
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

    if let Some(dir) = cwd
        && let Ok(dir_c) = CString::new(dir.as_os_str().as_bytes())
    {
        unsafe {
            libc::chdir(dir_c.as_ptr());
        }
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

/// How long a connection may take to identify itself, and the concurrent
/// connection cap - see [`crate::HANDSHAKE_TIMEOUT`] / [`crate::MAX_CLIENTS`].
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
        if queue.bytes + payload.len() > crate::MAX_CLIENT_BACKLOG {
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
        let mut history = self.history.lock().expect("history mutex poisoned");
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
        if changed {
            self.history
                .lock()
                .expect("history mutex poisoned")
                .resize(size.ws_row, size.ws_col);
            unsafe {
                libc::kill(-self.shell_pid, libc::SIGWINCH);
            }
        }
        Ok(changed)
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

    fn flush_restore_state(&self, last_cwd: &mut Option<PathBuf>, force_scrollback: bool) {
        let directory = self
            .directory
            .lock()
            .expect("directory mutex poisoned")
            .clone();
        if let Some(cwd) = shell_cwd(self.shell_pid)
            && last_cwd.as_deref() != Some(cwd.as_path())
        {
            let _ = fs::write(directory.join("cwd"), cwd.as_os_str().as_bytes());
            *last_cwd = Some(cwd);
        }
        if force_scrollback {
            let snapshot = self
                .history
                .lock()
                .expect("history mutex poisoned")
                .snapshot();
            let _ = fs::write(directory.join("scrollback"), snapshot);
        }
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
                    .and_then(|size| state.resize(to_winsize(size)))
                    .map(|_| ())
            }
            FRAME_INPUT if attached => state.write_input(&payload),
            FRAME_RESIZE if attached => decode_size(&payload)
                .and_then(|size| state.resize(to_winsize(size)))
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

/// Drops the leftovers of sessions nobody came back for. A master that exits
/// cleanly removes its own directory; one that goes down with the machine
/// cannot, and what it leaves behind - cwd, boot id, scrollback - is only
/// kept so the next attach can restore from it. Swept from `list` because
/// that is the command the app runs regularly, and never on a hot path.
fn sweep_abandoned_sessions(base: &Path) {
    let Ok(entries) = fs::read_dir(base) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        // A live master answers on its socket, whatever the timestamps say.
        if UnixStream::connect(path.join("control.sock")).is_ok() {
            continue;
        }
        let modified = fs::metadata(path.join("scrollback"))
            .or_else(|_| fs::metadata(path.join("created_at")))
            .or_else(|_| fs::metadata(&path))
            .and_then(|metadata| metadata.modified());
        if let Ok(modified) = modified
            && crate::is_abandoned(modified, now)
        {
            let _ = fs::remove_dir_all(&path);
        }
    }
}

pub(crate) fn list_command() -> io::Result<()> {
    let base = prepare_base_dir()?;
    sweep_abandoned_sessions(&base);
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

pub(crate) fn rename_command(old_name: &str, new_name: &str) -> io::Result<()> {
    validate_name(new_name)?;
    control_command(old_name, FRAME_RENAME, new_name.as_bytes())
}

pub(crate) fn kill_command(name: &str) -> io::Result<()> {
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

fn bridge_terminal(
    mut stream: UnixStream,
    replay_requested: bool,
    scrollback: &Arc<Mutex<VecDeque<u8>>>,
) -> io::Result<Attachment> {
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
                &encode_size(from_winsize(terminal_size(libc::STDIN_FILENO))),
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
                    append_scrollback(scrollback, &data);
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

static MASTER_TERM_REQUESTED: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_master_sigterm(_: libc::c_int) {
    MASTER_TERM_REQUESTED.store(true, Ordering::SeqCst);
}

fn install_master_sigterm_handler() -> io::Result<()> {
    let mut action = unsafe { std::mem::zeroed::<libc::sigaction>() };
    action.sa_sigaction = handle_master_sigterm as *const () as usize;
    action.sa_flags = 0;
    unsafe {
        libc::sigemptyset(&mut action.sa_mask);
        if libc::sigaction(libc::SIGTERM, &action, std::ptr::null_mut()) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn spawn_persistence_thread(state: Arc<MasterState>) {
    thread::spawn(move || {
        let mut last_cwd: Option<PathBuf> = None;
        let mut tick: u32 = 0;
        loop {
            if MASTER_TERM_REQUESTED.load(Ordering::SeqCst) {
                state.flush_restore_state(&mut last_cwd, true);
                unsafe {
                    libc::_exit(0);
                }
            }
            if state.terminating.load(Ordering::SeqCst) {
                return;
            }
            thread::sleep(CWD_POLL_INTERVAL);
            tick += 1;
            // The first tick flushes too: a session that dies before the
            // first full interval - a power cut minutes in is rare, seconds
            // in is not - would otherwise have no scrollback on disk at all.
            state.flush_restore_state(
                &mut last_cwd,
                tick == 1 || tick.is_multiple_of(SCROLLBACK_FLUSH_EVERY_TICKS),
            );
        }
    });
}

#[cfg(not(target_os = "macos"))]
fn peer_pid(stream: &UnixStream) -> Option<libc::pid_t> {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast(),
            &mut len,
        )
    };
    (result == 0).then_some(cred.pid)
}

#[cfg(target_os = "macos")]
fn peer_pid(stream: &UnixStream) -> Option<libc::pid_t> {
    let mut pid: libc::pid_t = 0;
    let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            (&raw mut pid).cast(),
            &mut len,
        )
    };
    (result == 0).then_some(pid)
}

#[cfg(not(target_os = "macos"))]
fn child_pid_of(pid: libc::pid_t) -> Option<libc::pid_t> {
    let content = fs::read_to_string(format!("/proc/{pid}/task/{pid}/children")).ok()?;
    content.split_whitespace().next()?.parse().ok()
}

#[cfg(target_os = "macos")]
fn child_pid_of(pid: libc::pid_t) -> Option<libc::pid_t> {
    let mut children = [0 as libc::pid_t; 8];
    let bytes = unsafe {
        proc_listchildpids(
            pid,
            children.as_mut_ptr().cast(),
            (children.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int,
        )
    };
    if bytes <= 0 {
        return None;
    }
    let count = (bytes as usize / std::mem::size_of::<libc::pid_t>()).min(children.len());
    children[..count].first().copied()
}

fn client_side_shell_pid(name: &str) -> Option<libc::pid_t> {
    let path = socket_path(name).ok()?;
    let stream = UnixStream::connect(path).ok()?;
    child_pid_of(peer_pid(&stream)?)
}

fn spawn_client_persistence_thread(name: String, scrollback: Arc<Mutex<VecDeque<u8>>>) {
    thread::spawn(move || {
        let Ok(directory) = session_dir(&name) else {
            return;
        };
        let mut shell_pid = None;
        for _ in 0..5 {
            shell_pid = client_side_shell_pid(&name);
            if shell_pid.is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(200));
        }

        let mut last_cwd: Option<PathBuf> = None;
        let mut tick: u32 = 0;
        loop {
            thread::sleep(CWD_POLL_INTERVAL);
            tick += 1;
            if let Some(id) = current_boot_id() {
                let _ = fs::write(directory.join("boot_id"), &id);
            }
            if let Some(pid) = shell_pid
                && let Some(cwd) = shell_cwd(pid)
                && last_cwd.as_deref() != Some(cwd.as_path())
            {
                let _ = fs::write(directory.join("cwd"), cwd.as_os_str().as_bytes());
                last_cwd = Some(cwd);
            }
            if tick == 1 || tick.is_multiple_of(SCROLLBACK_FLUSH_EVERY_TICKS) {
                let snapshot: Vec<u8> = scrollback
                    .lock()
                    .expect("scrollback mutex poisoned")
                    .iter()
                    .copied()
                    .collect();
                if !snapshot.is_empty() {
                    let _ = fs::write(directory.join("scrollback"), snapshot);
                }
            }
        }
    });
}

fn close_fd(fd: RawFd) {
    if fd >= 0 {
        unsafe {
            libc::close(fd);
        }
    }
}
