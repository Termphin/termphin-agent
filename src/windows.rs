//! Windows session master and attach client. Same wire protocol as
//! [`crate::unix`]: ConPTY instead of a PTY, a named pipe instead of a Unix
//! socket, a detached re-exec instead of `fork`+`setsid` for persistence.
//!
//! cwd is reported via a PowerShell prompt hook emitting an OSC marker (no
//! `/proc/<pid>/cwd` equivalent, and the child's PEB doesn't stay in sync
//! with `cd`). Not implemented: live-resize-during-attach is polled every
//! 300ms off `GetConsoleScreenBufferInfo` rather than event-driven -
//! `ENABLE_VIRTUAL_TERMINAL_INPUT` consumes the same input-record queue that
//! would otherwise deliver `WINDOW_BUFFER_SIZE_EVENT`.

use std::collections::{HashMap, HashSet};
use std::env;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows_sys::Wdk::System::Threading::{
    NtQueryInformationProcess, ProcessCommandLineInformation,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED,
    GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE, LocalFree, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Foundation::{INVALID_HANDLE_VALUE, UNICODE_STRING};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_NONE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
    ReadFile, WriteFile,
};
use windows_sys::Win32::System::Console::{
    CONSOLE_SCREEN_BUFFER_INFO, COORD, ClosePseudoConsole, CreatePseudoConsole,
    ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleCP,
    GetConsoleMode, GetConsoleOutputCP, GetConsoleScreenBufferInfo, GetStdHandle, HPCON,
    ResizePseudoConsole, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetConsoleCP, SetConsoleCtrlHandler,
    SetConsoleMode, SetConsoleOutputCP,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE,
    PIPE_UNLIMITED_INSTANCES, PIPE_WAIT, WaitNamedPipeW,
};
use windows_sys::Win32::System::SystemInformation::GetTickCount64;
use windows_sys::Win32::System::Threading::{
    CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, CreateEventW,
    CreateMutexW, CreateProcessW, DETACHED_PROCESS, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetProcessId, INFINITE, InitializeProcThreadAttributeList,
    OpenProcess, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION,
    PROCESS_QUERY_LIMITED_INFORMATION, ReleaseMutex, STARTUPINFOEXW, STARTUPINFOW,
    TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
};

use crate::win32_codec::{CWD_PROMPT_HOOK, Win32InputDecoder, base64_utf16le};
use crate::{
    CLIENT_WRITE_TIMEOUT, CONTROL_JOIN, CONTROL_TAKE, ClientQueue, DetachKey, Ending, FRAME_ATTACH,
    FRAME_CONTROL, FRAME_CONTROL_STATE, FRAME_DEVICE, FRAME_DEVICE_CONTROL, FRAME_ERROR,
    FRAME_EXIT, FRAME_HISTORY, FRAME_INPUT, FRAME_KILL, FRAME_OK, FRAME_OUTPUT, FRAME_PRESENCE,
    FRAME_RENAME, FRAME_REPLAY_DONE, FRAME_RESIZE, FRAME_STATUS, FRAME_STATUS_RESPONSE,
    HANDSHAKE_TIMEOUT, History, MAX_CLIENTS, PRESENCE_BACK_MARKER, PresenceEvent, PresenceKeys,
    REPLAY_CHUNK_SIZE, REPLAY_END_MARKER, RestoreState, SCROLLBACK_FLUSH_EVERY_TICKS, SESSION_ENV,
    TermSize, Typed, Viewers, control_marker, decode_size, devices_field, encode_control_state,
    encode_size, invalid_input, read_frame, send_frame, strip_resize_reports, validate_name,
};

/// Ten cheap console reads a second, against a visible lag on every rotation
/// and keyboard toggle if it were longer.
const RESIZE_POLL_INTERVAL: Duration = Duration::from_millis(100);

fn to_wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn last_error() -> io::Error {
    io::Error::last_os_error()
}

fn win_bool(value: i32) -> bool {
    value != 0
}

fn base_dir() -> io::Result<PathBuf> {
    let local =
        env::var_os("LOCALAPPDATA").ok_or_else(|| invalid_input("LOCALAPPDATA is not set"))?;
    Ok(PathBuf::from(local).join("termphin").join("sessions"))
}

fn session_dir(name: &str) -> io::Result<PathBuf> {
    validate_name(name)?;
    Ok(base_dir()?.join(name))
}

fn prepare_base_dir() -> io::Result<PathBuf> {
    let base = base_dir()?;
    std::fs::create_dir_all(&base)?;
    Ok(base)
}

fn username() -> String {
    env::var("USERNAME").unwrap_or_else(|_| "termphin".to_owned())
}

/// Derived from uptime, not a true boot id, so it can drift on a long uptime
/// if the clock is adjusted - rounded to reduce false positives from
/// scheduler jitter between two reads of the same boot. Worst case of
/// getting it wrong is an extra "restored after a restart" banner.
fn current_boot_id() -> Option<String> {
    let uptime_ms = unsafe { GetTickCount64() };
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?;
    let boot = now.checked_sub(Duration::from_millis(uptime_ms))?;
    Some(((boot.as_secs() / 10) * 10).to_string())
}

fn pipe_path(name: &str) -> String {
    format!(r"\\.\pipe\termphin-agent-{}-{name}", username())
}

/// `D:P(A;;GA;;;OW)` grants generic-all to the object's owner only - the
/// closest Windows equivalent of the Unix build's `0600` socket / `0700`
/// directory.
struct OwnerOnlySecurity {
    attributes: SECURITY_ATTRIBUTES,
    descriptor: *mut core::ffi::c_void,
}

unsafe impl Send for OwnerOnlySecurity {}

impl OwnerOnlySecurity {
    fn new() -> io::Result<Self> {
        let sddl = to_wide("D:P(A;;GA;;;OW)");
        let mut descriptor: *mut core::ffi::c_void = std::ptr::null_mut();
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION,
                &mut descriptor as *mut _ as *mut _,
                std::ptr::null_mut(),
            )
        };
        if !win_bool(ok) || descriptor.is_null() {
            return Err(last_error());
        }
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };
        Ok(Self {
            attributes,
            descriptor,
        })
    }

    fn as_ptr(&self) -> *const SECURITY_ATTRIBUTES {
        &self.attributes
    }
}

impl Drop for OwnerOnlySecurity {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.descriptor);
        }
    }
}

pub(crate) struct AsyncPipe {
    handle: HANDLE,
    /// False for [`AsyncPipe::borrowed`], so `Drop` doesn't close a handle
    /// this instance doesn't own.
    owned: bool,
}

unsafe impl Send for AsyncPipe {}
unsafe impl Sync for AsyncPipe {}

struct EventHandle(HANDLE);

impl EventHandle {
    fn new() -> io::Result<Self> {
        let handle = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if handle.is_null() {
            return Err(last_error());
        }
        Ok(Self(handle))
    }
}

impl Drop for EventHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

impl AsyncPipe {
    fn from_raw(handle: HANDLE) -> Self {
        Self {
            handle,
            owned: true,
        }
    }

    /// Non-owning view of a handle owned elsewhere.
    fn borrowed(handle: HANDLE) -> Self {
        Self {
            handle,
            owned: false,
        }
    }

    fn raw_handle(&self) -> HANDLE {
        self.handle
    }

    fn read_once(&self, buf: &mut [u8], timeout_ms: Option<u32>) -> io::Result<usize> {
        let event = EventHandle::new()?;
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        overlapped.hEvent = event.0;
        let mut read = 0u32;
        let ok = unsafe {
            ReadFile(
                self.handle,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut read,
                &mut overlapped,
            )
        };
        if win_bool(ok) {
            return Ok(read as usize);
        }
        let error = unsafe { GetLastError() };
        if error != ERROR_IO_PENDING {
            return Err(io::Error::from_raw_os_error(error as i32));
        }
        self.wait_and_collect(&mut overlapped, event.0, timeout_ms)
    }

    fn write_once(&self, buf: &[u8], timeout_ms: Option<u32>) -> io::Result<usize> {
        let event = EventHandle::new()?;
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        overlapped.hEvent = event.0;
        let mut written = 0u32;
        let ok = unsafe {
            WriteFile(
                self.handle,
                buf.as_ptr(),
                buf.len() as u32,
                &mut written,
                &mut overlapped,
            )
        };
        if win_bool(ok) {
            return Ok(written as usize);
        }
        let error = unsafe { GetLastError() };
        if error != ERROR_IO_PENDING {
            return Err(io::Error::from_raw_os_error(error as i32));
        }
        self.wait_and_collect(&mut overlapped, event.0, timeout_ms)
    }

    fn wait_and_collect(
        &self,
        overlapped: &mut OVERLAPPED,
        event: HANDLE,
        timeout_ms: Option<u32>,
    ) -> io::Result<usize> {
        let wait = unsafe { WaitForSingleObject(event, timeout_ms.unwrap_or(INFINITE)) };
        if wait == WAIT_TIMEOUT {
            unsafe {
                CancelIoEx(self.handle, overlapped);
            }
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "pipe I/O timed out",
            ));
        }
        if wait != WAIT_OBJECT_0 {
            return Err(last_error());
        }
        let mut transferred = 0u32;
        let ok = unsafe { GetOverlappedResult(self.handle, overlapped, &mut transferred, 0) };
        if !win_bool(ok) {
            return Err(last_error());
        }
        Ok(transferred as usize)
    }

    pub(crate) fn write_all_timeout(
        &self,
        mut buf: &[u8],
        timeout_ms: Option<u32>,
    ) -> io::Result<()> {
        while !buf.is_empty() {
            let written = self.write_once(buf, timeout_ms)?;
            if written == 0 {
                return Err(io::Error::new(io::ErrorKind::WriteZero, "wrote zero bytes"));
            }
            buf = &buf[written..];
        }
        Ok(())
    }

    pub(crate) fn read_exact_timeout(
        &self,
        mut buf: &mut [u8],
        timeout_ms: Option<u32>,
    ) -> io::Result<()> {
        while !buf.is_empty() {
            let read = self.read_once(buf, timeout_ms)?;
            if read == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "pipe closed mid-frame",
                ));
            }
            buf = &mut buf[read..];
        }
        Ok(())
    }
}

impl Drop for AsyncPipe {
    fn drop(&mut self) {
        if self.owned {
            unsafe {
                CloseHandle(self.handle);
            }
        }
    }
}

impl Read for AsyncPipe {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.read_once(buf, None)
    }
}

impl Write for AsyncPipe {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.write_all_timeout(buf, None)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Plain blocking pipe handle, no `OVERLAPPED` - `CreatePipe`'s anonymous
/// pipes (used for ConPTY) have no `FILE_FLAG_OVERLAPPED` equivalent, and an
/// overlapped `ReadFile` against one just blocks forever instead of
/// completing or erroring. Named pipes (the control connections) are opened
/// with `FILE_FLAG_OVERLAPPED` and use [`AsyncPipe`] instead.
struct SyncPipe(HANDLE);

unsafe impl Send for SyncPipe {}

impl SyncPipe {
    fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        let mut read = 0u32;
        let ok = unsafe {
            ReadFile(
                self.0,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut read,
                std::ptr::null_mut(),
            )
        };
        if !win_bool(ok) {
            return Err(last_error());
        }
        Ok(read as usize)
    }

    fn write_all(&self, mut buf: &[u8]) -> io::Result<()> {
        while !buf.is_empty() {
            let mut written = 0u32;
            let ok = unsafe {
                WriteFile(
                    self.0,
                    buf.as_ptr(),
                    buf.len() as u32,
                    &mut written,
                    std::ptr::null_mut(),
                )
            };
            if !win_bool(ok) {
                return Err(last_error());
            }
            if written == 0 {
                return Err(io::Error::new(io::ErrorKind::WriteZero, "wrote zero bytes"));
            }
            buf = &buf[written as usize..];
        }
        Ok(())
    }
}

impl Drop for SyncPipe {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

fn read_frame_timeout(pipe: &AsyncPipe, timeout_ms: Option<u32>) -> io::Result<(u8, Vec<u8>)> {
    let mut header = [0_u8; 5];
    pipe.read_exact_timeout(&mut header, timeout_ms)?;
    let length = u32::from_be_bytes(header[1..].try_into().expect("four bytes")) as usize;
    if length > crate::MAX_FRAME_SIZE {
        return Err(invalid_input("protocol frame is too large"));
    }
    let mut payload = vec![0_u8; length];
    pipe.read_exact_timeout(&mut payload, timeout_ms)?;
    Ok((header[0], payload))
}

fn create_pipe_instance(path: &str, security: &OwnerOnlySecurity) -> io::Result<AsyncPipe> {
    let wide = to_wide(path);
    let handle = unsafe {
        CreateNamedPipeW(
            wide.as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            PIPE_UNLIMITED_INSTANCES,
            65536,
            65536,
            0,
            security.as_ptr(),
        )
    };
    if handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
        return Err(last_error());
    }
    Ok(AsyncPipe::from_raw(handle))
}

fn accept(pipe: &AsyncPipe) -> io::Result<()> {
    let event = EventHandle::new()?;
    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    overlapped.hEvent = event.0;
    let ok = unsafe { ConnectNamedPipe(pipe.handle, &mut overlapped) };
    if win_bool(ok) {
        return Ok(());
    }
    let error = unsafe { GetLastError() };
    if error == ERROR_PIPE_CONNECTED {
        return Ok(());
    }
    if error != ERROR_IO_PENDING {
        return Err(io::Error::from_raw_os_error(error as i32));
    }
    let wait = unsafe { WaitForSingleObject(event.0, INFINITE) };
    if wait != WAIT_OBJECT_0 {
        return Err(last_error());
    }
    Ok(())
}

fn try_connect(name: &str) -> io::Result<AsyncPipe> {
    let path = to_wide(&pipe_path(name));
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_NONE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED,
            std::ptr::null_mut(),
        )
    };
    if handle != windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
        return Ok(AsyncPipe::from_raw(handle));
    }
    let error = unsafe { GetLastError() };
    if error == ERROR_PIPE_BUSY {
        let wide = to_wide(&pipe_path(name));
        unsafe { WaitNamedPipeW(wide.as_ptr(), 2000) };
        return try_connect_once(name);
    }
    Err(io::Error::from_raw_os_error(error as i32))
}

fn try_connect_once(name: &str) -> io::Result<AsyncPipe> {
    let path = to_wide(&pipe_path(name));
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_NONE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED,
            std::ptr::null_mut(),
        )
    };
    if handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
        return Err(last_error());
    }
    Ok(AsyncPipe::from_raw(handle))
}

struct CreationLock {
    handle: HANDLE,
}

impl CreationLock {
    fn acquire() -> io::Result<Self> {
        let name = to_wide(&format!(
            r"Local\termphin-agent-{}-creation-lock",
            username()
        ));
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        if handle.is_null() {
            return Err(last_error());
        }
        let wait = unsafe { WaitForSingleObject(handle, INFINITE) };
        if wait != WAIT_OBJECT_0 && wait != 0x80
        /* WAIT_ABANDONED */
        {
            unsafe { CloseHandle(handle) };
            return Err(io::Error::other("failed to acquire creation lock"));
        }
        Ok(Self { handle })
    }
}

impl Drop for CreationLock {
    fn drop(&mut self) {
        unsafe {
            ReleaseMutex(self.handle);
            CloseHandle(self.handle);
        }
    }
}

struct PseudoConsole {
    handle: HPCON,
}

unsafe impl Send for PseudoConsole {}

impl Drop for PseudoConsole {
    fn drop(&mut self) {
        unsafe {
            ClosePseudoConsole(self.handle);
        }
    }
}

struct ChildProcess {
    process: HANDLE,
    #[allow(dead_code)]
    thread: HANDLE,
}

unsafe impl Send for ChildProcess {}
unsafe impl Sync for ChildProcess {}

impl Drop for ChildProcess {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.process);
            CloseHandle(self.thread);
        }
    }
}

fn create_pipe_pair() -> io::Result<(HANDLE, HANDLE)> {
    let mut read: HANDLE = std::ptr::null_mut();
    let mut write: HANDLE = std::ptr::null_mut();
    let ok = unsafe {
        windows_sys::Win32::System::Pipes::CreatePipe(&mut read, &mut write, std::ptr::null(), 0)
    };
    if !win_bool(ok) {
        return Err(last_error());
    }
    Ok((read, write))
}

/// The hook rides in on the command line, not the shell's input: typed, the
/// shell echoes it, and hiding that took a `Clear-Host` that wiped scrollback.
fn shell_command_line() -> String {
    if let Ok(custom) = env::var("TERMPHIN_SHELL") {
        return custom;
    }
    format!(
        "powershell.exe -NoLogo -NoExit -EncodedCommand {}",
        base64_utf16le(CWD_PROMPT_HOOK)
    )
}

/// Processes that belong to the console itself, not to anything the user
/// ran: ConPTY starts its host as a child of the shell.
const CONSOLE_HOSTS: [&str; 2] = ["conhost.exe", "openconsole.exe"];

/// The command the shell [shell] is running: its newest child process other
/// than the console host. Windows has no foreground process group to ask,
/// and a command the shell runs is its child - a pipeline's processes are
/// all children, the newest standing for it.
fn running_command(shell: u32) -> Option<u32> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return None;
    }
    let mut entry = unsafe { std::mem::zeroed::<PROCESSENTRY32W>() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    let mut newest = None;
    let mut more = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
    while more {
        if entry.th32ParentProcessID == shell {
            let length = entry
                .szExeFile
                .iter()
                .position(|&unit| unit == 0)
                .unwrap_or(entry.szExeFile.len());
            let name = String::from_utf16_lossy(&entry.szExeFile[..length]).to_lowercase();
            if !CONSOLE_HOSTS.contains(&name.as_str()) {
                newest = Some(entry.th32ProcessID);
            }
        }
        more = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
    }
    unsafe { CloseHandle(snapshot) };
    newest
}

/// How a notification names the command running as process [pid]: its
/// command line, split as Windows programs split theirs, through
/// [`crate::relay::command_label`].
fn command_name(pid: i32) -> Option<String> {
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid as u32) };
    if process.is_null() {
        return None;
    }
    let mut buffer = vec![0_u64; 512];
    let mut needed = 0_u32;
    let mut status = unsafe {
        NtQueryInformationProcess(
            process,
            ProcessCommandLineInformation,
            buffer.as_mut_ptr().cast(),
            (buffer.len() * 8) as u32,
            &mut needed,
        )
    };
    if status < 0 && needed as usize > buffer.len() * 8 {
        buffer = vec![0_u64; (needed as usize).div_ceil(8)];
        status = unsafe {
            NtQueryInformationProcess(
                process,
                ProcessCommandLineInformation,
                buffer.as_mut_ptr().cast(),
                (buffer.len() * 8) as u32,
                &mut needed,
            )
        };
    }
    unsafe { CloseHandle(process) };
    if status < 0 {
        return None;
    }
    let text = unsafe { &*buffer.as_ptr().cast::<UNICODE_STRING>() };
    if text.Buffer.is_null() || text.Length == 0 {
        return None;
    }
    let units = unsafe { std::slice::from_raw_parts(text.Buffer, usize::from(text.Length) / 2) };
    let line = String::from_utf16_lossy(units);
    crate::relay::command_label(&crate::relay::split_command_line(&line))
}

/// Returns the pseudo console, the write end of its input pipe, the read end
/// of its output pipe, and the shell process attached to it.
fn spawn_conpty_shell(
    size: TermSize,
) -> io::Result<(PseudoConsole, SyncPipe, SyncPipe, ChildProcess)> {
    let (pty_in_read, pty_in_write) = create_pipe_pair()?;
    let (pty_out_read, pty_out_write) = create_pipe_pair()?;

    let coord = COORD {
        X: size.cols as i16,
        Y: size.rows as i16,
    };
    let mut hpc: HPCON = 0;
    let result = unsafe { CreatePseudoConsole(coord, pty_in_read, pty_out_write, 0, &mut hpc) };
    // The pseudo console owns these two ends now; only the other two (which
    // we keep using) are still ours to close.
    unsafe {
        CloseHandle(pty_in_read);
        CloseHandle(pty_out_write);
    }
    if result != 0 {
        unsafe {
            CloseHandle(pty_in_write);
            CloseHandle(pty_out_read);
        }
        return Err(io::Error::from_raw_os_error(result));
    }
    let pseudo_console = PseudoConsole { handle: hpc };

    let child = spawn_attached_process(&shell_command_line(), hpc)?;

    Ok((
        pseudo_console,
        SyncPipe(pty_in_write),
        SyncPipe(pty_out_read),
        child,
    ))
}

/// The shell gets neither a process group of its own nor the master's
/// "ignore Ctrl+C", which the master has from being started in one: either
/// leaves the shell ignoring Ctrl+C, so a Ctrl+C typed into the session would
/// stop nothing.
fn spawn_attached_process(command_line: &str, hpc: HPCON) -> io::Result<ChildProcess> {
    unsafe {
        let mut size: usize = 0;
        InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut size);
        let mut buffer = vec![0_u8; size];
        let attribute_list = buffer.as_mut_ptr() as *mut core::ffi::c_void;
        if InitializeProcThreadAttributeList(attribute_list, 1, 0, &mut size) == 0 {
            return Err(last_error());
        }
        let update_ok = UpdateProcThreadAttribute(
            attribute_list,
            0,
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
            hpc as *const core::ffi::c_void,
            std::mem::size_of::<HPCON>(),
            std::ptr::null_mut(),
            std::ptr::null(),
        );
        if !win_bool(update_ok) {
            DeleteProcThreadAttributeList(attribute_list);
            return Err(last_error());
        }

        let mut startup_info: STARTUPINFOEXW = std::mem::zeroed();
        startup_info.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        startup_info.lpAttributeList = attribute_list;

        let mut process_info: PROCESS_INFORMATION = std::mem::zeroed();
        let mut wide_cmd = to_wide(command_line);
        // No CREATE_NO_WINDOW here: it is not part of Microsoft's own ConPTY
        // sample and appears to interfere with the pseudo console attachment
        // itself rather than being a harmless no-op.
        let flags = EXTENDED_STARTUPINFO_PRESENT;
        SetConsoleCtrlHandler(None, 0);

        let ok = CreateProcessW(
            std::ptr::null(),
            wide_cmd.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            flags,
            std::ptr::null(),
            std::ptr::null(),
            &startup_info.StartupInfo,
            &mut process_info,
        );

        DeleteProcThreadAttributeList(attribute_list);

        if !win_bool(ok) {
            return Err(last_error());
        }
        Ok(ChildProcess {
            process: process_info.hProcess,
            thread: process_info.hThread,
        })
    }
}

struct ClientChannel {
    queue: Mutex<ClientQueue>,
    ready: Condvar,
    pipe: AsyncPipe,
}

impl ClientChannel {
    /// `pipe` is borrowed: [`client_loop`] owns the real `AsyncPipe` and
    /// closes it once, on return. This only needs the raw handle, to write
    /// frames and to [`ClientChannel::close`] the reader out of a stuck read.
    fn spawn(pipe: &AsyncPipe) -> Arc<Self> {
        let channel = Arc::new(Self {
            queue: Mutex::new(ClientQueue::default()),
            ready: Condvar::new(),
            pipe: AsyncPipe::borrowed(pipe.raw_handle()),
        });
        let writer = Arc::clone(&channel);
        thread::spawn(move || writer.run());
        channel
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
        // Unblocks client_loop's pending read - same job `shutdown()` on a
        // duplicated socket does on the Unix side.
        unsafe {
            CancelIoEx(self.pipe.raw_handle(), std::ptr::null());
        }
    }

    fn drain(&self, deadline: Duration) {
        let end = Instant::now() + deadline;
        let mut queue = self.queue.lock().expect("client queue mutex poisoned");
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

    fn run(&self) {
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
            let mut header = [0_u8; 5];
            header[0] = frame.0;
            header[1..].copy_from_slice(&(frame.1.len() as u32).to_be_bytes());
            let timeout_ms = Some(CLIENT_WRITE_TIMEOUT.as_millis() as u32);
            let written = self
                .pipe
                .write_all_timeout(&header, timeout_ms)
                .and_then(|_| self.pipe.write_all_timeout(&frame.1, timeout_ms));
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

/// A named pipe lives in a flat, global `\\.\pipe\` namespace, not at a path
/// inside the session directory - renaming that directory does nothing to
/// the pipe. [`MasterState::rename`] compensates by cancelling whatever
/// `ConnectNamedPipe` the accept loop is blocked on, so it loops back and
/// opens the next instance under the new name.
struct RawHandle(HANDLE);
unsafe impl Send for RawHandle {}
unsafe impl Sync for RawHandle {}

struct MasterState {
    name: Mutex<String>,
    directory: Mutex<PathBuf>,
    created_at: u64,
    child: ChildProcess,
    pseudo_console: Mutex<PseudoConsole>,
    pty_write: Mutex<SyncPipe>,
    clients: Mutex<HashMap<u64, Arc<ClientChannel>>>,
    /// Clients whose phone said it went to the background; see
    /// [`crate::FRAME_PRESENCE`].
    away: Mutex<HashSet<u64>>,
    /// What each client said about its device, by client id.
    devices: Mutex<HashMap<u64, String>>,
    history: Mutex<History>,
    viewers: Mutex<Viewers>,
    size: Mutex<TermSize>,
    pending_listener: Mutex<Option<RawHandle>>,
    terminating: AtomicBool,
    pty_closed: AtomicBool,
    /// The shell's process id, whose children are the commands it runs.
    shell_pid: u32,
    /// The command the shell is running, as of the last look - see
    /// [`running_command`]. Read on every piece of output, so it is not
    /// looked up there.
    foreground: Mutex<Option<u32>>,
    notifications: Mutex<crate::relay::NotificationScanner>,
    relay: Mutex<crate::relay::Relay>,
    commands: Mutex<crate::relay::CommandWatch>,
    quiet: Mutex<crate::relay::QuietWatch>,
    /// What the program in front says it is doing over OSC 7501, or through
    /// the hooks of an agent that runs `termphin-agent hook`.
    status: Mutex<crate::status::ProgramStatus>,
    /// The program in front and what it had running at rest - its MCP
    /// servers, say - so a finished turn names only what it started since.
    agent_baseline: Mutex<Option<(u32, std::collections::HashSet<u32>)>>,
    last_line: Mutex<crate::relay::LastLines>,
    /// Pieces of output written while a program held the terminal, which
    /// tells whether it wrote anything after it was announced as quiet.
    program_writes: AtomicU64,
    quiet_announced: Mutex<crate::relay::QuietAnnounced>,
    /// When the last phone left, so a stop it was there to see is not
    /// announced once it has gone.
    watchers: Mutex<crate::relay::Watchers>,
    /// What the session is doing, as last worked out by `track_activity`.
    activity: Mutex<Option<crate::Activity>>,
    /// The command and the count of its writes when it last went quiet
    /// asking something - waiting for the user until it writes again.
    went_quiet: Mutex<Option<(i32, u64)>>,
}

impl MasterState {
    fn add_client(
        &self,
        id: u64,
        channel: Arc<ClientChannel>,
        replay: bool,
        client_rows: u16,
    ) -> io::Result<()> {
        let mut history = self.history.lock().expect("history mutex poisoned");
        channel.send(FRAME_OUTPUT, &crate::master_version_marker())?;
        if replay {
            for chunk in history.snapshot_for(client_rows).chunks(REPLAY_CHUNK_SIZE) {
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

    /// Whether a phone is looking: a client attached and not away.
    fn present(&self, clients: &HashMap<u64, Arc<ClientChannel>>) -> bool {
        crate::anyone_present(
            clients.keys(),
            &self.away.lock().expect("away mutex poisoned"),
        )
    }

    /// A client's phone went away or came back. Once every client is away,
    /// the session is left as far as notifications go. One that comes back is
    /// sent [`PRESENCE_BACK_MARKER`] after whatever was queued for it.
    fn presence(&self, id: u64, away: bool) -> io::Result<()> {
        let clients = self.clients.lock().expect("clients mutex poisoned");
        let was_present = self.present(&clients);
        {
            let mut set = self.away.lock().expect("away mutex poisoned");
            if away {
                set.insert(id);
            } else {
                set.remove(&id);
            }
        }
        if was_present && !self.present(&clients) {
            self.watchers
                .lock()
                .expect("watchers mutex poisoned")
                .left(std::time::Instant::now());
        }
        if !away && let Some(channel) = clients.get(&id) {
            channel.send(FRAME_OUTPUT, PRESENCE_BACK_MARKER)?;
        }
        Ok(())
    }

    /// Records which device client [id] is and reports it, so the user's
    /// other devices see who is attached. Anything [`valid_device`] refuses
    /// is ignored rather than failing the client.
    fn device(&self, id: u64, payload: &[u8]) -> io::Result<()> {
        if let Ok(description) = std::str::from_utf8(payload)
            && crate::valid_device(description)
        {
            self.devices
                .lock()
                .expect("devices mutex poisoned")
                .insert(id, description.to_owned());
            crate::presence::report(None, false);
            if self.holder_is_same_device(id, description) {
                return self.take_control(id);
            }
            self.announce_control();
        }
        Ok(())
    }

    /// Whether the session is held by another connection of the device
    /// that client [id] says it is: one that dropped off the network and
    /// came back while its old connection still looks attached. That device
    /// takes the session back rather than watching its own ghost.
    fn holder_is_same_device(&self, id: u64, description: &str) -> bool {
        let holder = self
            .viewers
            .lock()
            .expect("viewers mutex poisoned")
            .holder();
        let Some(holder) = holder.filter(|&holder| holder != id) else {
            return false;
        };
        self.devices
            .lock()
            .expect("devices mutex poisoned")
            .get(&holder)
            .is_some_and(|held| crate::same_device(held, description))
    }

    /// Forgets client [id] and reports the session without it - after it is
    /// gone from the clients, or the report would still count it attached.
    fn remove_client(&self, id: u64) {
        self.away.lock().expect("away mutex poisoned").remove(&id);
        self.devices
            .lock()
            .expect("devices mutex poisoned")
            .remove(&id);
        let nobody_left = {
            let mut clients = self.clients.lock().expect("clients mutex poisoned");
            clients.remove(&id).is_some() && clients.is_empty()
        };
        if nobody_left {
            self.watchers
                .lock()
                .expect("watchers mutex poisoned")
                .left(std::time::Instant::now());
        }
        let back_to = self
            .viewers
            .lock()
            .expect("viewers mutex poisoned")
            .left(id);
        if let Some(size) = back_to {
            let _ = self.resize(size);
            self.announce_control();
        }
        crate::presence::report(None, false);
    }

    /// As on Unix: a client that [joined] control attaches as a viewer
    /// while another holds the session and is looking at it.
    fn attached_at(&self, id: u64, size: TermSize, joined: bool) -> io::Result<()> {
        let apply = {
            let away = self.away.lock().expect("away mutex poisoned");
            let mut viewers = self.viewers.lock().expect("viewers mutex poisoned");
            let held = viewers
                .holder()
                .is_some_and(|holder| holder != id && !away.contains(&holder));
            viewers.attached(id, size, joined, held)
        };
        if let Some(size) = apply {
            self.resize(size)?;
        }
        self.announce_control();
        crate::presence::report(None, false);
        Ok(())
    }

    fn resized_to(&self, id: u64, size: TermSize) -> io::Result<()> {
        let apply = self
            .viewers
            .lock()
            .expect("viewers mutex poisoned")
            .resized(id, size);
        match apply {
            Some(size) => {
                self.resize(size)?;
                self.announce_control();
                Ok(())
            }
            None => Ok(()),
        }
    }

    fn take_control(&self, id: u64) -> io::Result<()> {
        let apply = self
            .viewers
            .lock()
            .expect("viewers mutex poisoned")
            .take(id);
        if let Some(size) = apply {
            self.resize(size)?;
            self.announce_control();
        }
        Ok(())
    }

    /// What `take`, `away` or `back` asked for a device: applied to its
    /// newest client for a take, to all of them for away and back.
    fn device_control(&self, payload: &[u8]) -> io::Result<()> {
        let (operation, device) = crate::parse_device_control(payload)?;
        let clients = crate::clients_of_device(
            self.devices.lock().expect("devices mutex poisoned").iter(),
            device,
        );
        let Some(&newest) = clients.first() else {
            return Err(invalid_input("no client of that device is attached"));
        };
        match operation {
            crate::DEVICE_TAKE => self.take_control(newest),
            operation => {
                for client in clients {
                    self.presence(client, operation == crate::DEVICE_AWAY)?;
                }
                Ok(())
            }
        }
    }

    /// Tells each client that joined control whether it holds the session,
    /// the size the session runs at and which device holds it.
    fn announce_control(&self) {
        let (holder, size, joined) = {
            let viewers = self.viewers.lock().expect("viewers mutex poisoned");
            let (Some(holder), Some(size)) = (viewers.holder(), viewers.size()) else {
                return;
            };
            (holder, size, viewers.joined().collect::<Vec<_>>())
        };
        if joined.is_empty() {
            return;
        }
        let device = self
            .devices
            .lock()
            .expect("devices mutex poisoned")
            .get(&holder)
            .cloned()
            .unwrap_or_default();
        let clients = self.clients.lock().expect("clients mutex poisoned");
        for id in joined {
            if let Some(channel) = clients.get(&id) {
                let _ = channel.send(
                    FRAME_CONTROL_STATE,
                    &encode_control_state(id == holder, size, &device),
                );
            }
        }
    }

    fn input_from(&self, id: u64, data: &[u8]) -> io::Result<()> {
        let typed = self
            .viewers
            .lock()
            .expect("viewers mutex poisoned")
            .typed(id);
        match typed {
            Typed::Drop => return Ok(()),
            Typed::Write(Some(size)) => {
                self.resize(size)?;
                self.announce_control();
            }
            Typed::Write(None) => {}
        }
        self.commands
            .lock()
            .expect("commands mutex poisoned")
            .typed(std::time::Instant::now());
        self.quiet.lock().expect("quiet mutex poisoned").typed();
        self.status_seen();
        self.write_input(data)
    }

    /// Takes in what [sequences] - OSC 7501 the program wrote, or a hook
    /// sent - say, as the Unix master does: the support query is answered,
    /// and each report goes to the session's records, announcing what it is
    /// due.
    fn program_status(&self, sequences: Vec<crate::status::Sequence>) {
        let mut changed = false;
        for sequence in sequences {
            match sequence {
                crate::status::Sequence::Query => {
                    let reply = String::from_utf8_lossy(&crate::status::query_reply()).into_owned();
                    let _ = self.write_input(&crate::win32_codec::typed_as_keys(&reply));
                }
                crate::status::Sequence::Report(report) => {
                    changed = true;
                    if report.state == Some(crate::status::ProgramState::Idle) {
                        self.note_agent_at_rest();
                    }
                    let event = self
                        .status
                        .lock()
                        .expect("status mutex poisoned")
                        .apply(report, std::time::Instant::now());
                    if let Some(event) = event {
                        self.announce_program(&event);
                    }
                }
            }
        }
        if changed {
            self.quiet.lock().expect("quiet mutex poisoned").notified();
            self.refresh_activity();
        }
    }

    /// Tells what a program reported - blocked, done, failed - as the
    /// master's own notification, naming the program in front when it gave
    /// no name of its own.
    fn announce_program(&self, event: &crate::status::StatusEvent) {
        let fallback = (*self.foreground.lock().expect("foreground mutex poisoned"))
            .and_then(|pid| command_name(pid as i32));
        let root = self.agent_root();
        let mut event = event.clone();
        let mut still = None;
        if let (crate::status::StatusEvent::Done { app, msg }, Some(root)) = (&mut event, root) {
            if msg.is_none()
                && let Some(app) = app.as_deref()
            {
                *msg =
                    crate::insight::last_reply(app, crate::platform::process_cwd(root).as_deref())
                        .and_then(|said| crate::status::last_sentences(&said));
            }
            let baseline = self
                .agent_baseline
                .lock()
                .expect("agent baseline mutex poisoned")
                .clone()
                .filter(|(at, _)| *at == root);
            if let Some((_, baseline)) = baseline {
                let parents = crate::platform::process_parents();
                let (names, more) = crate::insight::still_running(
                    &parents,
                    root,
                    &baseline,
                    crate::platform::process_label,
                    crate::platform::process_program,
                );
                still = crate::insight::still_running_line(&names, more);
            }
        }
        self.broadcast_output(&crate::relay::status_notice(
            &event,
            fallback.as_deref(),
            still.as_deref(),
        ));
    }

    /// The program in front said it is at rest: what it has running now is
    /// its own - MCP servers and the like - and not news when a turn ends.
    fn note_agent_at_rest(&self) {
        let Some(root) = self.agent_root() else {
            return;
        };
        let parents = crate::platform::process_parents();
        let now = crate::insight::descendants(&parents, root);
        let mut baseline = self
            .agent_baseline
            .lock()
            .expect("agent baseline mutex poisoned");
        match baseline.as_mut() {
            Some((at, known)) if *at == root => known.extend(now),
            _ => *baseline = Some((root, now)),
        }
    }

    /// The program the shell is running, which stands for the program in
    /// front on Windows - looked up now, since a report can come before the
    /// next poll has seen the program start.
    fn agent_root(&self) -> Option<u32> {
        running_command(self.shell_pid)
    }

    /// The user is at the session: what a program finished or failed has
    /// been seen, and no longer shows.
    fn status_seen(&self) {
        let mut status = self.status.lock().expect("status mutex poisoned");
        if !status.reporting() {
            return;
        }
        status.seen();
        drop(status);
        self.refresh_activity();
    }

    /// Works the session's activity out again now, after the program said
    /// something about itself.
    fn refresh_activity(&self) {
        let foreground = (*self.foreground.lock().expect("foreground mutex poisoned"))
            .map(|pid| pid as i32)
            .unwrap_or(0);
        let writes = self.program_writes.load(Ordering::SeqCst);
        self.track_activity(foreground, writes, std::time::Instant::now());
    }

    /// Looks at which command the shell is running and, as the Unix master
    /// does with process groups, announces a long one that ended or a
    /// program that went quiet, by writing the notification into the
    /// session's output.
    fn watch_command(&self) {
        let held = self
            .status
            .lock()
            .expect("status mutex poisoned")
            .due(std::time::Instant::now());
        if let Some(event) = held {
            self.announce_program(&event);
        }
        let running = running_command(self.shell_pid);
        *self.foreground.lock().expect("foreground mutex poisoned") = running;
        let shell = self.shell_pid as i32;
        let foreground = running.map(|pid| pid as i32);
        let now = std::time::Instant::now();
        let finished = self
            .commands
            .lock()
            .expect("commands mutex poisoned")
            .observe(foreground, shell, now, command_name);
        let writes = self.program_writes.load(Ordering::SeqCst);
        let attached = self.present(&self.clients.lock().expect("clients mutex poisoned"));
        let ended_at = now.checked_sub(crate::CWD_POLL_INTERVAL).unwrap_or(now);
        let finished = finished.filter(|finished| {
            !self
                .quiet_announced
                .lock()
                .expect("quiet announced mutex poisoned")
                .covers(finished.group, writes)
                && !self
                    .watchers
                    .lock()
                    .expect("watchers mutex poisoned")
                    .saw(ended_at, attached)
        });
        if let Some(mut finished) = finished {
            finished.line = self
                .last_line
                .lock()
                .expect("last line mutex poisoned")
                .take(finished.group);
            self.broadcast_output(&finished.sequence());
        }
        let quiet = self.quiet.lock().expect("quiet mutex poisoned").check(
            foreground,
            shell,
            now,
            command_name,
        );
        let quiet = quiet.filter(|_| {
            !self
                .status
                .lock()
                .expect("status mutex poisoned")
                .reporting()
        });
        if let Some(mut quiet) = quiet {
            quiet.line = self
                .history
                .lock()
                .expect("history mutex poisoned")
                .screen_line(crate::relay::Pick::Showing);
            let asks = quiet
                .line
                .as_deref()
                .is_some_and(crate::relay::asks_something);
            if asks {
                *self.went_quiet.lock().expect("went quiet mutex poisoned") =
                    Some((quiet.group, writes));
            }
            let seen = self
                .watchers
                .lock()
                .expect("watchers mutex poisoned")
                .saw(quiet.stopped_at, attached);
            if asks && !seen {
                self.quiet_announced
                    .lock()
                    .expect("quiet announced mutex poisoned")
                    .announced(quiet.group, writes);
                self.broadcast_output(&quiet.sequence());
            }
        }
        self.track_activity(foreground.unwrap_or(0), writes, now);
    }

    /// Works out what the session is doing and reports it when that changed,
    /// so the user's apps show it on the session's row - as the Unix master
    /// does, with the shell's newest child standing in for the foreground
    /// process group.
    fn track_activity(&self, command: i32, writes: u64, now: std::time::Instant) {
        let program = {
            let mut status = self.status.lock().expect("status mutex poisoned");
            if command <= 0 || command == self.shell_pid as i32 {
                status.program_ended();
            }
            status.summary()
        };
        let activity = if command <= 0 || command == self.shell_pid as i32 {
            crate::Activity {
                state: crate::ActivityState::Idle,
                command: None,
                program,
            }
        } else {
            let working = self
                .quiet
                .lock()
                .expect("quiet mutex poisoned")
                .working(now);
            let waiting = !working
                && *self.went_quiet.lock().expect("went quiet mutex poisoned")
                    == Some((command, writes));
            let reported = program.as_ref().map(|program| program.state);
            crate::Activity {
                state: match reported {
                    Some(crate::status::ProgramState::Blocked) => crate::ActivityState::Waiting,
                    Some(crate::status::ProgramState::Working) => crate::ActivityState::Working,
                    Some(_) => crate::ActivityState::Running,
                    None if working => crate::ActivityState::Working,
                    None if waiting => crate::ActivityState::Waiting,
                    None => crate::ActivityState::Running,
                },
                command: command_name(command),
                program,
            }
        };
        let changed = {
            let mut current = self.activity.lock().expect("activity mutex poisoned");
            let changed = current.as_ref() != Some(&activity);
            *current = Some(activity);
            changed
        };
        if changed {
            crate::presence::report(None, false);
        }
    }

    /// What the session's program wrote, as opposed to what the master adds
    /// to the output itself.
    fn program_output(&self, data: &[u8]) {
        self.quiet
            .lock()
            .expect("quiet mutex poisoned")
            .output(std::time::Instant::now());
        let sequences = self.broadcast_output(data);
        if !sequences.is_empty() {
            self.program_status(sequences);
        }
        let Some(pid) = *self.foreground.lock().expect("foreground mutex poisoned") else {
            return;
        };
        self.program_writes.fetch_add(1, Ordering::SeqCst);
        let line = self
            .history
            .lock()
            .expect("history mutex poisoned")
            .screen_line(crate::relay::Pick::Written);
        if let Some(line) = line {
            self.last_line
                .lock()
                .expect("last line mutex poisoned")
                .wrote(pid as i32, line);
        }
    }

    /// Sends [data] to every attached client and into the history, and
    /// relays the notifications in it while nobody is there to see them.
    /// Returns the program status sequences in it, for
    /// [`Self::program_status`] once nothing here is held.
    fn broadcast_output(&self, data: &[u8]) -> Vec<crate::status::Sequence> {
        let mut history = self.history.lock().expect("history mutex poisoned");
        history.push(data);
        let (mut found, sequences) = {
            let mut scanner = self
                .notifications
                .lock()
                .expect("notifications mutex poisoned");
            let found = scanner.feed(data);
            (found, scanner.take_status())
        };
        if !found.is_empty() {
            self.quiet.lock().expect("quiet mutex poisoned").notified();
        }
        if self
            .status
            .lock()
            .expect("status mutex poisoned")
            .reporting()
        {
            found.retain(|notice| notice.kind.is_some());
        }
        let clients = self.clients.lock().expect("clients mutex poisoned");
        if !self.present(&clients) {
            let name = self.name.lock().expect("name mutex poisoned").clone();
            let mut relay = self.relay.lock().expect("relay mutex poisoned");
            for notice in &found {
                relay.send(notice, &name);
            }
        }
        if clients.is_empty() {
            return sequences;
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
        sequences
    }

    fn write_input(&self, data: &[u8]) -> io::Result<()> {
        self.pty_write
            .lock()
            .expect("pty mutex poisoned")
            .write_all(data)
    }

    /// Returns whether the size actually changed, matching the Unix build's
    /// SIGWINCH-only-on-change behaviour.
    fn resize(&self, size: TermSize) -> io::Result<bool> {
        let mut current = self.size.lock().expect("size mutex poisoned");
        if *current == size {
            return Ok(false);
        }
        let coord = COORD {
            X: size.cols as i16,
            Y: size.rows as i16,
        };
        let result = unsafe {
            ResizePseudoConsole(
                self.pseudo_console
                    .lock()
                    .expect("pty mutex poisoned")
                    .handle,
                coord,
            )
        };
        if result != 0 {
            return Err(io::Error::from_raw_os_error(result));
        }
        self.history
            .lock()
            .expect("history mutex poisoned")
            .resize(size.rows, size.cols);
        *current = size;
        Ok(true)
    }

    fn status(&self) -> String {
        let name = self.name.lock().expect("name mutex poisoned");
        let attached = self.clients.lock().expect("clients mutex poisoned").len();
        let devices = devices_field(
            self.devices
                .lock()
                .expect("devices mutex poisoned")
                .values(),
        );
        let (activity, program) = self
            .activity
            .lock()
            .expect("activity mutex poisoned")
            .as_ref()
            .map(|activity| (activity.field(), activity.program_field()))
            .unwrap_or_default();
        format!(
            "{name}\t1\t{}\t{attached}\t{devices}\t{activity}\t{program}",
            self.created_at
        )
    }

    fn rename(&self, new_name: &str) -> io::Result<()> {
        validate_name(new_name)?;
        let _lock = CreationLock::acquire()?;
        let mut directory = self.directory.lock().expect("directory mutex poisoned");
        let destination = base_dir()?.join(new_name);
        if destination.exists() {
            if try_connect(new_name).is_ok() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "session name is already in use",
                ));
            }
            std::fs::remove_dir_all(&destination)?;
        }
        std::fs::rename(&*directory, &destination)?;
        *directory = destination;
        *self.name.lock().expect("name mutex poisoned") = new_name.to_owned();
        if let Some(handle) = self
            .pending_listener
            .lock()
            .expect("pending listener mutex poisoned")
            .as_ref()
        {
            unsafe {
                CancelIoEx(handle.0, std::ptr::null());
            }
        }
        crate::presence::report(None, false);
        Ok(())
    }

    fn kill_shell(&self) {
        if self.terminating.swap(true, Ordering::SeqCst) {
            return;
        }
        unsafe {
            TerminateProcess(self.child.process, 1);
        }
        self.close_pseudo_console();
    }

    /// Terminating the child does not tear down ConPTY by itself - its
    /// actual writer is conhost, a separate process `CreatePseudoConsole`
    /// spawns internally, which keeps the output pipe open until this call.
    /// Guarded so [`Self::kill_shell`] and [`Self::finish`] can both call it.
    fn close_pseudo_console(&self) {
        if self.pty_closed.swap(true, Ordering::SeqCst) {
            return;
        }
        unsafe {
            ClosePseudoConsole(
                self.pseudo_console
                    .lock()
                    .expect("pty mutex poisoned")
                    .handle,
            );
        }
    }

    fn flush_scrollback(&self) {
        let directory = self
            .directory
            .lock()
            .expect("directory mutex poisoned")
            .clone();
        let snapshot = self
            .history
            .lock()
            .expect("history mutex poisoned")
            .snapshot();
        let _ = std::fs::write(directory.join("scrollback"), snapshot);
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
            for writer in &writers {
                writer.drain(Duration::from_secs(2));
            }
        }
        self.flush_scrollback();
        unsafe {
            TerminateProcess(self.child.process, 1);
        }
        self.close_pseudo_console();
        let directory = self
            .directory
            .lock()
            .expect("directory mutex poisoned")
            .clone();
        let name = self.name.lock().expect("name mutex poisoned").clone();
        let _ = std::fs::remove_dir_all(directory);
        crate::presence::report(Some(&name), true);
        std::process::exit(0)
    }
}

fn client_loop(id: u64, pipe: AsyncPipe, state: Arc<MasterState>) {
    let writer = ClientChannel::spawn(&pipe);
    let handshake_ms = Some(HANDSHAKE_TIMEOUT.as_millis() as u32);
    let mut attached = false;
    let mut replay_requested = false;
    let mut joined = false;

    loop {
        let timeout = if attached { None } else { handshake_ms };
        let Ok((kind, payload)) = read_frame_timeout(&pipe, timeout) else {
            break;
        };
        let outcome = match kind {
            FRAME_CONTROL if !attached && payload == [CONTROL_JOIN] => {
                joined = true;
                Ok(())
            }
            FRAME_CONTROL if attached && payload == [CONTROL_TAKE] => state.take_control(id),
            FRAME_DEVICE_CONTROL if !attached => state
                .device_control(&payload)
                .and_then(|_| writer.send(FRAME_OK, &[])),
            crate::FRAME_PROGRAM_STATUS if !attached => {
                if let Some(report @ crate::status::Sequence::Report(_)) =
                    crate::status::parse(&payload)
                {
                    state.program_status(vec![report]);
                }
                writer.send(FRAME_OK, &[])
            }
            FRAME_HISTORY if !attached => {
                replay_requested = true;
                Ok(())
            }
            FRAME_ATTACH => decode_size(&payload).and_then(|size| {
                if !attached {
                    attached = true;
                    state.add_client(id, Arc::clone(&writer), replay_requested, size.rows)?;
                    state.status_seen();
                }
                state.attached_at(id, size, joined)
            }),
            FRAME_INPUT if attached => state.input_from(id, &payload),
            FRAME_PRESENCE if attached => state.presence(id, payload.first() == Some(&1)),
            FRAME_DEVICE if attached => state.device(id, &payload),
            FRAME_RESIZE if attached => {
                decode_size(&payload).and_then(|size| state.resized_to(id, size))
            }
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

/// The master runs `DETACHED_PROCESS`, so `eprintln!`/the default panic
/// handler write to a handle nothing reads. Without this, a startup failure
/// or panic here is silent - the attach client just times out waiting for a
/// pipe that will never exist.
fn install_master_panic_log(directory: PathBuf) {
    std::panic::set_hook(Box::new(move |info| {
        let _ = std::fs::write(directory.join("master.log"), format!("panic: {info}\n"));
    }));
}

fn log_master_error(directory: &std::path::Path, error: io::Error) -> io::Error {
    let _ = std::fs::write(
        directory.join("master.log"),
        format!("startup failed: {error}\n"),
    );
    error
}

/// Entry point for `termphin-agent __master <name> <cols> <rows>`.
pub(crate) fn run_as_master(mut args: impl Iterator<Item = String>) -> io::Result<()> {
    let name = args
        .next()
        .ok_or_else(|| invalid_input("missing session name"))?;
    let cols: u16 = args
        .next()
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| invalid_input("missing column count"))?;
    let rows: u16 = args
        .next()
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| invalid_input("missing row count"))?;
    let size = TermSize { cols, rows };

    let directory = session_dir(&name)?;
    // `connect_or_create` already created the directory, so only `created_at`
    // tells a rebuilt session from a new one.
    let restore = if directory.join("created_at").exists() {
        RestoreState::load(&directory, current_boot_id().as_deref())
    } else {
        RestoreState::default()
    };
    std::fs::create_dir_all(&directory)?;
    unsafe { env::set_var(crate::SESSION_ID_ENV, crate::session_id(&directory)) };
    install_master_panic_log(directory.clone());
    let directory_log = directory.clone();
    // Before the shell starts, which is what inherits it; nothing else runs yet.
    unsafe { env::set_var(SESSION_ENV, &name) };

    let (pseudo_console, pty_write, pty_read, child) =
        spawn_conpty_shell(size).map_err(|error| log_master_error(&directory_log, error))?;

    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let _ = std::fs::write(directory.join("created_at"), created_at.to_string());
    if let Some(id) = current_boot_id() {
        let _ = std::fs::write(directory.join("boot_id"), &id);
    }

    let mut history = History::new(size.rows, size.cols);
    if restore.restored {
        history.seed_restored(&restore.scrollback, restore.reboot_restored);
    }

    let shell_pid = unsafe { GetProcessId(child.process) };
    let state = Arc::new(MasterState {
        name: Mutex::new(name.clone()),
        directory: Mutex::new(directory),
        created_at,
        child,
        shell_pid,
        foreground: Mutex::new(None),
        notifications: Mutex::new(crate::relay::NotificationScanner::default()),
        relay: Mutex::new(crate::relay::Relay::default()),
        commands: Mutex::new(crate::relay::CommandWatch::default()),
        quiet: Mutex::new(crate::relay::QuietWatch::default()),
        status: Mutex::new(crate::status::ProgramStatus::default()),
        agent_baseline: Mutex::new(None),
        last_line: Mutex::new(crate::relay::LastLines::default()),
        program_writes: AtomicU64::new(0),
        quiet_announced: Mutex::new(crate::relay::QuietAnnounced::default()),
        watchers: Mutex::new(crate::relay::Watchers::default()),
        activity: Mutex::new(None),
        went_quiet: Mutex::new(None),
        pseudo_console: Mutex::new(pseudo_console),
        pty_write: Mutex::new(pty_write),
        clients: Mutex::new(HashMap::new()),
        away: Mutex::new(HashSet::new()),
        devices: Mutex::new(HashMap::new()),
        history: Mutex::new(history),
        viewers: Mutex::new(Viewers::default()),
        size: Mutex::new(size),
        pending_listener: Mutex::new(None),
        pty_closed: AtomicBool::new(false),
        terminating: AtomicBool::new(false),
    });
    crate::remember_shell(
        &state.directory.lock().expect("directory mutex poisoned"),
        shell_pid,
    );

    let reader_state = Arc::clone(&state);
    thread::spawn(move || {
        let mut buffer = [0_u8; 8192];
        loop {
            match pty_read.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    let output = strip_resize_reports(&buffer[..count]);
                    if !output.is_empty() {
                        reader_state.program_output(&output);
                    }
                }
                Err(error) if error.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) => break,
                Err(_) => break,
            }
        }
        reader_state.finish();
    });
    crate::presence::start_heartbeat();

    // ConPTY holds its output pipe open after the shell exits, until it is
    // closed - so without this an `exit` would leave the session listed, and
    // its clients attached to nothing, for good.
    let exit_state = Arc::clone(&state);
    let shell = RawHandle(state.child.process);
    thread::spawn(move || {
        let shell = shell;
        unsafe { WaitForSingleObject(shell.0, INFINITE) };
        exit_state.close_pseudo_console();
    });

    let persistence_state = Arc::clone(&state);
    thread::spawn(move || {
        let mut tick: u32 = 0;
        loop {
            thread::sleep(crate::CWD_POLL_INTERVAL);
            if persistence_state.terminating.load(Ordering::SeqCst) {
                return;
            }
            tick += 1;
            persistence_state.watch_command();
            // First tick included: see the same loop in `unix.rs`.
            if tick == 1 || tick.is_multiple_of(SCROLLBACK_FLUSH_EVERY_TICKS) {
                persistence_state.flush_scrollback();
            }
        }
    });

    let security =
        OwnerOnlySecurity::new().map_err(|error| log_master_error(&directory_log, error))?;
    static NEXT_CLIENT_ID: AtomicU64 = AtomicU64::new(1);
    let active_clients = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    loop {
        let current_name = state.name.lock().expect("name mutex poisoned").clone();
        let pipe = match create_pipe_instance(&pipe_path(&current_name), &security) {
            Ok(pipe) => pipe,
            Err(_) => break,
        };
        *state
            .pending_listener
            .lock()
            .expect("pending listener mutex poisoned") = Some(RawHandle(pipe.raw_handle()));
        let accepted = accept(&pipe);
        *state
            .pending_listener
            .lock()
            .expect("pending listener mutex poisoned") = None;
        if accepted.is_err() {
            continue;
        }
        let taken = active_clients.fetch_add(1, Ordering::SeqCst);
        if taken >= MAX_CLIENTS {
            active_clients.fetch_sub(1, Ordering::SeqCst);
            continue;
        }
        let id = NEXT_CLIENT_ID.fetch_add(1, Ordering::Relaxed);
        let client_state = Arc::clone(&state);
        let counter = Arc::clone(&active_clients);
        thread::spawn(move || {
            client_loop(id, pipe, client_state);
            counter.fetch_sub(1, Ordering::SeqCst);
        });
    }
    state.kill_shell();
    state.finish();
}

/// Spawns a detached copy of this executable as `__master <name> <cols>
/// <rows>`, breaking away from whatever job object owns the current process
/// (sshd's, typically) so it outlives the SSH session that asked for it. If
/// the job does not permit breakaway, the child dies with the job - the next
/// attach just finds no listener and starts a fresh one, same as any session
/// that did not shut down cleanly.
fn spawn_master(name: &str, size: TermSize) -> io::Result<()> {
    let exe = env::current_exe()?;
    let command_line = format!(
        "{} __master {} {} {}",
        quote_arg(&exe.to_string_lossy()),
        name,
        size.cols,
        size.rows
    );
    spawn_detached(&command_line)?;
    Ok(())
}

/// Starts the master detached from the app that asked for it. It first asks
/// to leave the caller's job object, so the master outlives an app that runs
/// inside one and kills its children on exit. A job that forbids leaving it -
/// some launchers, terminals and editors start the app in one - refuses that
/// with `ERROR_ACCESS_DENIED`, and the master then starts inside the job
/// rather than not at all.
fn spawn_detached(command_line: &str) -> io::Result<()> {
    match create_detached(command_line, true) {
        Err(error) if error.raw_os_error() == Some(ERROR_ACCESS_DENIED_CODE) => {
            create_detached(command_line, false)
        }
        result => result,
    }
}

/// `ERROR_ACCESS_DENIED`, what `CreateProcessW` answers a breakaway the job
/// does not allow.
const ERROR_ACCESS_DENIED_CODE: i32 = 5;

/// The creation flags of a detached master, leaving the caller's job when
/// [breakaway] is set.
fn detached_flags(breakaway: bool) -> u32 {
    let flags = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW;
    if breakaway {
        flags | CREATE_BREAKAWAY_FROM_JOB
    } else {
        flags
    }
}

fn create_detached(command_line: &str, breakaway: bool) -> io::Result<()> {
    unsafe {
        let mut startup_info: STARTUPINFOW = std::mem::zeroed();
        startup_info.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        let mut process_info: PROCESS_INFORMATION = std::mem::zeroed();
        let mut wide_cmd = to_wide(command_line);
        let ok = CreateProcessW(
            std::ptr::null(),
            wide_cmd.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            detached_flags(breakaway),
            std::ptr::null(),
            std::ptr::null(),
            &startup_info,
            &mut process_info,
        );
        if !win_bool(ok) {
            return Err(last_error());
        }
        CloseHandle(process_info.hProcess);
        CloseHandle(process_info.hThread);
    }
    Ok(())
}

/// MSDN "Parsing C++ Command-Line Arguments" quoting. Only `exe`'s path
/// realistically needs it - the name and size are already restricted to a
/// safe character set - but it's cheaper to do this once than maintain a
/// second bespoke escaping rule.
fn quote_arg(value: &str) -> String {
    if !value.is_empty() && !value.contains([' ', '\t', '"']) {
        return value.to_owned();
    }
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            let mut backslashes = 1;
            while chars.peek() == Some(&'\\') {
                backslashes += 1;
                chars.next();
            }
            if chars.peek() == Some(&'"') || chars.peek().is_none() {
                quoted.extend(std::iter::repeat_n('\\', backslashes * 2));
            } else {
                quoted.extend(std::iter::repeat_n('\\', backslashes));
            }
        } else if ch == '"' {
            quoted.push('\\');
            quoted.push('"');
        } else {
            quoted.push(ch);
        }
    }
    quoted.push('"');
    quoted
}

fn connect_or_create(name: &str, size: TermSize) -> io::Result<AsyncPipe> {
    if let Ok(pipe) = try_connect(name) {
        return Ok(pipe);
    }

    let lock = CreationLock::acquire()?;
    if let Ok(pipe) = try_connect(name) {
        return Ok(pipe);
    }

    let directory = session_dir(name)?;
    std::fs::create_dir_all(&directory)?;

    spawn_master(name, size)?;
    drop(lock);

    for _ in 0..80 {
        match try_connect(name) {
            Ok(pipe) => return Ok(pipe),
            Err(_) => thread::sleep(Duration::from_millis(25)),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "session master did not start",
    ))
}

/// The Windows half of the sweep in `unix.rs`, with a named pipe standing in
/// for the control socket as the proof that a master is still alive.
fn sweep_abandoned_sessions(base: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        // A live master answers on its pipe, whatever the timestamps say.
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| try_connect(name).is_ok())
            .unwrap_or(false)
        {
            continue;
        }
        let modified = std::fs::metadata(path.join("scrollback"))
            .or_else(|_| std::fs::metadata(path.join("created_at")))
            .or_else(|_| std::fs::metadata(&path))
            .and_then(|metadata| metadata.modified());
        if let Ok(modified) = modified
            && crate::is_abandoned(modified, now)
        {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

/// Restoring after a reboot is not reported on Windows yet.
pub(crate) fn restorable_sessions() -> Vec<(String, u64)> {
    Vec::new()
}

pub(crate) fn list_command() -> io::Result<()> {
    for status in session_statuses()? {
        println!("{status}");
    }
    Ok(())
}

pub(crate) fn session_statuses() -> io::Result<Vec<String>> {
    let base = prepare_base_dir()?;
    sweep_abandoned_sessions(&base);
    let mut directories = std::fs::read_dir(base)?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false))
        .collect::<Vec<_>>();
    directories.sort_by_key(|entry| entry.file_name());

    let mut statuses = Vec::new();
    for directory in directories {
        let name = directory.file_name();
        let Some(name) = name.to_str() else { continue };
        let Ok(mut pipe) = try_connect(name) else {
            continue;
        };
        send_frame(&mut pipe, FRAME_STATUS, &[])?;
        if let Ok((FRAME_STATUS_RESPONSE, payload)) = read_frame(&mut pipe)
            && let Ok(status) = String::from_utf8(payload)
        {
            statuses.push(status);
        }
    }
    Ok(statuses)
}

pub(crate) fn rename_command(old_name: &str, new_name: &str) -> io::Result<()> {
    validate_name(new_name)?;
    control_command(old_name, FRAME_RENAME, new_name.as_bytes())
}

/// `take`, `away` and `back`: a [`crate::FRAME_DEVICE_CONTROL`] sent to the
/// session's master.
pub(crate) fn device_command(name: &str, payload: &[u8]) -> io::Result<()> {
    control_command(name, FRAME_DEVICE_CONTROL, payload)
}

/// Where the session directories are, for finding one by its id.
pub(crate) fn sessions_dir() -> io::Result<PathBuf> {
    base_dir()
}

/// Every process's parent, child to parent, from one snapshot of the
/// process list. Empty when the snapshot fails.
pub(crate) fn process_parents() -> HashMap<u32, u32> {
    let mut parents = HashMap::new();
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return parents;
    }
    let mut entry = unsafe { std::mem::zeroed::<PROCESSENTRY32W>() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    let mut more = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
    while more {
        parents.insert(entry.th32ProcessID, entry.th32ParentProcessID);
        more = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
    }
    unsafe { CloseHandle(snapshot) };
    parents
}

/// Windows has no cheap way to read another process's working directory;
/// a transcript is then picked only when it is the only fresh one.
pub(crate) fn process_cwd(_pid: u32) -> Option<PathBuf> {
    None
}

/// How notifications name the command process [pid] runs.
pub(crate) fn process_label(pid: u32) -> Option<String> {
    command_name(pid as i32)
}

/// The program file process [pid] runs, by its name alone, from a snapshot
/// of the process list.
pub(crate) fn process_program(pid: u32) -> Option<String> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return None;
    }
    let mut entry = unsafe { std::mem::zeroed::<PROCESSENTRY32W>() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    let mut found = None;
    let mut more = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
    while more {
        if entry.th32ProcessID == pid {
            let length = entry
                .szExeFile
                .iter()
                .position(|&unit| unit == 0)
                .unwrap_or(entry.szExeFile.len());
            found = Some(String::from_utf16_lossy(&entry.szExeFile[..length]));
            break;
        }
        more = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
    }
    unsafe { CloseHandle(snapshot) };
    found
}

/// This process and the ones above it, nearest first, for finding the
/// session a hook runs in by its shell, from one snapshot of the process
/// list.
pub(crate) fn ancestors() -> Vec<u32> {
    let mut parents = HashMap::new();
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot != INVALID_HANDLE_VALUE {
        let mut entry = unsafe { std::mem::zeroed::<PROCESSENTRY32W>() };
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut more = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
        while more {
            parents.insert(entry.th32ProcessID, entry.th32ParentProcessID);
            more = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
        }
        unsafe { CloseHandle(snapshot) };
    }
    let mut chain = vec![std::process::id()];
    while chain.len() < 64 {
        let current = *chain.last().expect("never empty");
        match parents.get(&current) {
            Some(&parent) if parent > 0 && !chain.contains(&parent) => chain.push(parent),
            _ => break,
        }
    }
    chain
}

/// `hook`: a program status report from an agent's hook, sent to the
/// session's master as if the program had written it.
pub(crate) fn program_status_command(name: &str, body: &[u8]) -> io::Result<()> {
    control_command(name, crate::FRAME_PROGRAM_STATUS, body)
}

pub(crate) fn kill_command(name: &str) -> io::Result<()> {
    control_command(name, FRAME_KILL, &[])
}

fn control_command(name: &str, kind: u8, payload: &[u8]) -> io::Result<()> {
    let mut pipe = try_connect(name)?;
    send_frame(&mut pipe, kind, payload)?;
    match read_frame(&mut pipe)? {
        (FRAME_OK, _) => Ok(()),
        (FRAME_ERROR, message) => Err(io::Error::other(
            String::from_utf8_lossy(&message).into_owned(),
        )),
        _ => Err(io::Error::other("invalid response from session master")),
    }
}

/// `_resume` is not honoured here: ConPTY re-renders the output, so the
/// bytes a client receives are not the bytes an offset counts. A resume asked
/// for gets the replay, which is what the app falls back to anyway.
pub(crate) fn attach_command(
    name: &str,
    replay: bool,
    _resume: Option<String>,
    control: bool,
    device: Option<&str>,
    stdio: Option<TermSize>,
) -> io::Result<()> {
    // With `--stdio` the app's pipes stand in for a console: there is none
    // to switch modes on, and its size comes from the app.
    let _raw_mode = match stdio {
        None => Some(ConsoleRawMode::enable()?),
        Some(_) => None,
    };

    if !replay {
        try_attach(name, false, false, control, device, stdio)?;
        return Ok(());
    }
    if try_attach(name, true, false, control, device, stdio)? == Attachment::ReplayRejected {
        try_attach(name, false, false, control, device, stdio)?;
    }
    Ok(())
}

/// Modes a program in the session may have left the person's own terminal
/// in: the alternate screen, a hidden cursor, mouse reporting, bracketed
/// paste, the application keypad, colours.
const RESET_ON_DETACH: &[u8] =
    b"\x1b[?1049l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?2004l\x1b>\x1b[0m\r\n";

pub(crate) fn attach_person(name: &str) -> io::Result<Ending> {
    let _raw_mode = ConsoleRawMode::enable()?;
    let attachment = match try_attach(name, true, true, false, None, None)? {
        Attachment::ReplayRejected => try_attach(name, false, true, false, None, None)?,
        other => other,
    };
    // Still in VT mode: restored, the console would print these as text.
    let mut output = io::stdout().lock();
    output.write_all(RESET_ON_DETACH)?;
    output.flush()?;
    Ok(match attachment {
        Attachment::Detached => Ending::Detached,
        _ => Ending::Ended,
    })
}

#[derive(PartialEq, Eq)]
enum Attachment {
    Finished,
    Detached,
    ReplayRejected,
}

fn console_size() -> TermSize {
    let handle = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
    let mut info: CONSOLE_SCREEN_BUFFER_INFO = unsafe { std::mem::zeroed() };
    if win_bool(unsafe { GetConsoleScreenBufferInfo(handle, &mut info) }) {
        let cols = (info.srWindow.Right - info.srWindow.Left + 1).max(1) as u16;
        let rows = (info.srWindow.Bottom - info.srWindow.Top + 1).max(1) as u16;
        return TermSize { cols, rows };
    }
    TermSize { cols: 80, rows: 24 }
}

fn try_attach(
    name: &str,
    replay: bool,
    detach_key: bool,
    control: bool,
    device: Option<&str>,
    stdio: Option<TermSize>,
) -> io::Result<Attachment> {
    let size = stdio.unwrap_or_else(console_size);
    let pipe = connect_or_create(name, size)?;
    if replay {
        send_frame(&mut &pipe, FRAME_HISTORY, &[])?;
    }
    if control {
        send_frame(&mut &pipe, FRAME_CONTROL, &[CONTROL_JOIN])?;
    }
    send_frame(&mut &pipe, FRAME_ATTACH, &encode_size(size))?;
    if let Some(device) = device {
        send_frame(&mut &pipe, FRAME_DEVICE, device.as_bytes())?;
    }
    bridge_terminal(
        pipe,
        size,
        replay,
        detach_key,
        usize::from(control) + usize::from(device.is_some()),
        stdio.is_some(),
    )
}

impl Read for &AsyncPipe {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.read_once(buf, None)
    }
}

impl Write for &AsyncPipe {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.write_all_timeout(buf, None)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Counts one down from [count] unless it is already zero; whether it did.
fn take_one(count: &AtomicUsize) -> bool {
    let mut left = count.load(Ordering::SeqCst);
    while left > 0 {
        match count.compare_exchange(left, left - 1, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return true,
            Err(now) => left = now,
        }
    }
    false
}

fn bridge_terminal(
    pipe: AsyncPipe,
    initial_size: TermSize,
    replay_requested: bool,
    detach_key: bool,
    optional_frames: usize,
    stdio: bool,
) -> io::Result<Attachment> {
    let pipe = Arc::new(pipe);
    let input_pipe = Arc::clone(&pipe);
    // Older masters answer each optional frame with an error and go on; one
    // such error is let through per frame sent, from either thread.
    let tolerated = Arc::new(AtomicUsize::new(
        optional_frames + usize::from(replay_requested),
    ));
    let input_tolerated = Arc::clone(&tolerated);
    let detached = Arc::new(AtomicBool::new(false));
    let input_detached = Arc::clone(&detached);
    let mut detach_key = detach_key.then(DetachKey::default);

    // Own thread because Windows has no single call that waits on both a
    // console input handle and a named pipe the way `poll()` does on Unix.
    // Never joined: the process exits once the output loop below finishes,
    // taking a still-blocked `ReadFile` on stdin down with it.
    thread::spawn(move || {
        let stdin = StdinHandle(unsafe { GetStdHandle(STD_INPUT_HANDLE) });
        relay_input(
            stdin,
            &mut &*input_pipe,
            detach_key.as_mut(),
            &input_tolerated,
            &input_detached,
        );
    });

    let resize_pipe = Arc::clone(&pipe);
    let last_size = Arc::new(Mutex::new(initial_size));
    let resize_last_size = Arc::clone(&last_size);
    // A console's size is polled; an app on `--stdio` says when it changes.
    thread::spawn(move || {
        if stdio {
            return;
        }
        loop {
            thread::sleep(RESIZE_POLL_INTERVAL);
            let size = console_size();
            let mut last = resize_last_size.lock().expect("size mutex poisoned");
            if *last != size {
                *last = size;
                if send_frame(&mut &*resize_pipe, FRAME_RESIZE, &encode_size(size)).is_err() {
                    return;
                }
            }
        }
    });

    let mut output = io::stdout().lock();
    let mut painted = false;

    loop {
        let frame = match read_frame(&mut &*pipe) {
            Ok(frame) => frame,
            Err(_) if replay_requested && !painted => return Ok(Attachment::ReplayRejected),
            Err(error) => return Err(error),
        };
        match frame {
            (FRAME_OUTPUT, data) => {
                painted = true;
                output.write_all(&data)?;
                output.flush()?;
            }
            (FRAME_REPLAY_DONE, _) => {
                painted = true;
                output.write_all(REPLAY_END_MARKER)?;
                output.flush()?;
            }
            (FRAME_CONTROL_STATE, payload) => {
                if let Some(marker) = control_marker(&payload) {
                    output.write_all(&marker)?;
                    output.flush()?;
                }
            }
            (FRAME_EXIT, _) => return Ok(Attachment::Finished),
            (FRAME_STATUS_RESPONSE, _) if detached.load(Ordering::SeqCst) => {
                return Ok(Attachment::Detached);
            }
            (FRAME_ERROR, message) => {
                if message.as_slice() == b"invalid protocol frame" && take_one(&tolerated) {
                    continue;
                }
                if replay_requested && !painted {
                    return Ok(Attachment::ReplayRejected);
                }
                return Err(io::Error::other(
                    String::from_utf8_lossy(&message).into_owned(),
                ));
            }
            _ => {}
        }
    }
}

/// The attach client's standard input, read with `ReadFile` so a pipe from
/// the app and a console both work.
struct StdinHandle(HANDLE);

impl Read for StdinHandle {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let mut read = 0u32;
        let ok = unsafe {
            ReadFile(
                self.0,
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                &mut read,
                std::ptr::null_mut(),
            )
        };
        if !win_bool(ok) {
            return Err(io::Error::last_os_error());
        }
        Ok(read as usize)
    }
}

/// Forwards what the user types to the master until the input ends. The end
/// of the input - the app closed its pipe or went away - or the detach key
/// both detach: [detached] is set and a status request wakes the output loop,
/// which is blocked reading the pipe, so the client exits instead of staying
/// attached to a session nobody can see.
fn relay_input<R: Read, W: Write>(
    mut input: R,
    pipe: &mut W,
    mut detach_key: Option<&mut DetachKey>,
    tolerated: &AtomicUsize,
    detached: &AtomicBool,
) {
    let mut buffer = [0_u8; 8192];
    let mut win32_input = Win32InputDecoder::default();
    let mut presence = PresenceKeys::default();
    loop {
        let read = match input.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        let (decoded, moves) = presence.feed(&win32_input.feed(&buffer[..read]));
        for event in moves {
            let sent = match event {
                PresenceEvent::Away => send_frame(pipe, FRAME_PRESENCE, &[1]),
                PresenceEvent::Back => send_frame(pipe, FRAME_PRESENCE, &[0]),
                PresenceEvent::Device(description) => {
                    send_frame(pipe, FRAME_DEVICE, description.as_bytes())
                }
                PresenceEvent::TakeControl => {
                    tolerated.fetch_add(1, Ordering::SeqCst);
                    send_frame(pipe, FRAME_CONTROL, &[CONTROL_TAKE])
                }
                PresenceEvent::Resize(size) => send_frame(pipe, FRAME_RESIZE, &encode_size(size)),
            };
            if sent.is_err() {
                return;
            }
        }
        let (forward, detach) = match detach_key.as_deref_mut() {
            Some(key) => key.feed(&decoded),
            None => (decoded, false),
        };
        if !forward.is_empty() && send_frame(pipe, FRAME_INPUT, &forward).is_err() {
            return;
        }
        if detach {
            break;
        }
    }
    detached.store(true, Ordering::SeqCst);
    let _ = send_frame(pipe, FRAME_STATUS, &[]);
}

/// UTF-8's code page number, `CP_UTF8`.
const UTF8_CODE_PAGE: u32 = 65001;

/// The attach client's console set up for relaying a session: raw VT input
/// and output, and UTF-8 both ways. Without the code pages the session's
/// UTF-8 output is read in the console's OEM code page (852, 437...), and
/// every letter outside ASCII - Polish ones among them - turns to garbage.
/// Everything is put back when it drops.
struct ConsoleRawMode {
    stdin: HANDLE,
    original_input_mode: u32,
    stdout: HANDLE,
    original_output_mode: u32,
    original_input_cp: u32,
    original_output_cp: u32,
}

impl ConsoleRawMode {
    fn enable() -> io::Result<Self> {
        let stdin = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        let stdout = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
        let mut original_input_mode = 0u32;
        let mut original_output_mode = 0u32;
        // A non-console stdin/stdout (piped, redirected) fails these calls,
        // same as `isatty` false on the Unix side - nothing to restore.
        let has_console = win_bool(unsafe { GetConsoleMode(stdin, &mut original_input_mode) })
            && win_bool(unsafe { GetConsoleMode(stdout, &mut original_output_mode) });
        let original_input_cp = unsafe { GetConsoleCP() };
        let original_output_cp = unsafe { GetConsoleOutputCP() };
        if has_console {
            unsafe {
                SetConsoleMode(stdin, ENABLE_VIRTUAL_TERMINAL_INPUT);
                SetConsoleMode(
                    stdout,
                    original_output_mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING,
                );
                SetConsoleCP(UTF8_CODE_PAGE);
                SetConsoleOutputCP(UTF8_CODE_PAGE);
            }
        }
        Ok(Self {
            stdin,
            original_input_mode,
            stdout,
            original_output_mode,
            original_input_cp,
            original_output_cp,
        })
    }
}

impl Drop for ConsoleRawMode {
    fn drop(&mut self) {
        unsafe {
            SetConsoleMode(self.stdin, self.original_input_mode);
            SetConsoleMode(self.stdout, self.original_output_mode);
            if self.original_input_cp != 0 {
                SetConsoleCP(self.original_input_cp);
            }
            if self.original_output_cp != 0 {
                SetConsoleOutputCP(self.original_output_cp);
            }
        }
    }
}

#[cfg(test)]
mod detached_spawn_tests {
    use super::*;

    #[test]
    fn the_first_try_leaves_the_job_and_the_fallback_stays_in_it() {
        assert_ne!(detached_flags(true) & CREATE_BREAKAWAY_FROM_JOB, 0);
        assert_eq!(detached_flags(false) & CREATE_BREAKAWAY_FROM_JOB, 0);
        for breakaway in [true, false] {
            let flags = detached_flags(breakaway);
            assert_ne!(flags & DETACHED_PROCESS, 0);
            assert_ne!(flags & CREATE_NO_WINDOW, 0);
        }
    }
}

#[cfg(test)]
mod relay_input_tests {
    use super::*;

    fn frames(mut written: &[u8]) -> Vec<(u8, Vec<u8>)> {
        let mut all = Vec::new();
        while !written.is_empty() {
            all.push(read_frame(&mut written).expect("frame"));
        }
        all
    }

    #[test]
    fn the_end_of_the_input_detaches() {
        let mut pipe = Vec::new();
        let detached = AtomicBool::new(false);
        relay_input(
            &b"ls\r"[..],
            &mut pipe,
            None,
            &AtomicUsize::new(0),
            &detached,
        );
        assert!(detached.load(Ordering::SeqCst));
        assert_eq!(
            frames(&pipe),
            vec![(FRAME_INPUT, b"ls\r".to_vec()), (FRAME_STATUS, Vec::new())]
        );
    }

    #[test]
    fn a_broken_input_detaches_too() {
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::from(io::ErrorKind::BrokenPipe))
            }
        }
        let mut pipe = Vec::new();
        let detached = AtomicBool::new(false);
        relay_input(Broken, &mut pipe, None, &AtomicUsize::new(0), &detached);
        assert!(detached.load(Ordering::SeqCst));
        assert_eq!(frames(&pipe), vec![(FRAME_STATUS, Vec::new())]);
    }
}
