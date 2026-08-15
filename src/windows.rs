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

use std::collections::{HashMap, VecDeque};
use std::env;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_BROKEN_PIPE, ERROR_IO_PENDING,
    ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE,
    LocalFree, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_NONE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
    ReadFile, WriteFile,
};
use windows_sys::Win32::System::Console::{
    COORD, CONSOLE_SCREEN_BUFFER_INFO, ClosePseudoConsole, CreatePseudoConsole,
    ENABLE_VIRTUAL_TERMINAL_INPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING,
    GetConsoleMode, GetConsoleScreenBufferInfo, GetStdHandle, HPCON, ResizePseudoConsole,
    SetConsoleMode, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
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
    EXTENDED_STARTUPINFO_PRESENT, INFINITE, InitializeProcThreadAttributeList,
    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION, ReleaseMutex, STARTUPINFOEXW,
    STARTUPINFOW, TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
};

use crate::{
    CLIENT_WRITE_TIMEOUT, ClientQueue, FRAME_ATTACH, FRAME_ERROR, FRAME_EXIT, FRAME_HISTORY,
    FRAME_INPUT, FRAME_KILL, FRAME_OK, FRAME_OUTPUT, FRAME_REPLAY_DONE, FRAME_RESIZE,
    FRAME_RENAME, FRAME_STATUS, FRAME_STATUS_RESPONSE, HANDSHAKE_TIMEOUT, History, MAX_CLIENTS,
    REPLAY_CHUNK_SIZE, REPLAY_END_MARKER, RestoreState, SCROLLBACK_FLUSH_EVERY_TICKS, TermSize,
    append_scrollback, decode_size, encode_size, invalid_input, read_frame, send_frame,
    validate_name,
};

const RESIZE_POLL_INTERVAL: Duration = Duration::from_millis(300);

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
    let local = env::var_os("LOCALAPPDATA")
        .ok_or_else(|| invalid_input("LOCALAPPDATA is not set"))?;
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
            return Err(io::Error::new(io::ErrorKind::TimedOut, "pipe I/O timed out"));
        }
        if wait != WAIT_OBJECT_0 {
            return Err(last_error());
        }
        let mut transferred = 0u32;
        let ok = unsafe {
            GetOverlappedResult(self.handle, overlapped, &mut transferred, 0)
        };
        if !win_bool(ok) {
            return Err(last_error());
        }
        Ok(transferred as usize)
    }

    pub(crate) fn write_all_timeout(&self, mut buf: &[u8], timeout_ms: Option<u32>) -> io::Result<()> {
        while !buf.is_empty() {
            let written = self.write_once(buf, timeout_ms)?;
            if written == 0 {
                return Err(io::Error::new(io::ErrorKind::WriteZero, "wrote zero bytes"));
            }
            buf = &buf[written..];
        }
        Ok(())
    }

    pub(crate) fn read_exact_timeout(&self, mut buf: &mut [u8], timeout_ms: Option<u32>) -> io::Result<()> {
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
    if error == ERROR_PIPE_BUSY as u32 {
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
        let name = to_wide(&format!(r"Local\termphin-agent-{}-creation-lock", username()));
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        if handle.is_null() {
            return Err(last_error());
        }
        let wait = unsafe { WaitForSingleObject(handle, INFINITE) };
        if wait != WAIT_OBJECT_0 && wait != 0x80 /* WAIT_ABANDONED */ {
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
        windows_sys::Win32::System::Pipes::CreatePipe(
            &mut read,
            &mut write,
            std::ptr::null(),
            0,
        )
    };
    if !win_bool(ok) {
        return Err(last_error());
    }
    Ok((read, write))
}

/// Decodes Win32-Input-Mode (`CSI Vk;Sc;Uc;Kd;Cs;Rc _`) back into plain
/// bytes - sshd's outer ConPTY wraps every keystroke this way once a pty is
/// allocated, and fails to recognize arrow keys as one unit under it
/// (`ESC[A` arrives as three separate `Vk=0` char events), which our inner
/// ConPTY then mis-delivers to the shell the same way. Decoding back to
/// plain bytes lets the inner ConPTY parse `ESC[A` itself.
#[derive(Default)]
struct Win32InputDecoder {
    pending: Vec<u8>,
    high_surrogate: Option<u16>,
}

impl Win32InputDecoder {
    fn feed(&mut self, data: &[u8]) -> Vec<u8> {
        self.pending.extend_from_slice(data);
        const MAX_BODY_LEN: usize = 40;

        let mut output = Vec::new();
        let mut i = 0;
        while i < self.pending.len() {
            if self.pending[i] == 0x1b && i + 1 < self.pending.len() && self.pending[i + 1] == b'[' {
                let search_from = i + 2;
                let search_to = (search_from + MAX_BODY_LEN).min(self.pending.len());
                match self.pending[search_from..search_to].iter().position(|&b| b == b'_') {
                    Some(rel_end) => {
                        let end = search_from + rel_end;
                        let body = self.pending[search_from..end].to_vec();
                        if let Some(decoded) = self.decode_body(&body) {
                            output.extend(decoded);
                            i = end + 1;
                            continue;
                        }
                    }
                    None if search_to - search_from < MAX_BODY_LEN => break,
                    None => {}
                }
            }
            output.push(self.pending[i]);
            i += 1;
        }
        self.pending.drain(..i);
        output
    }

    fn decode_body(&mut self, body: &[u8]) -> Option<Vec<u8>> {
        let text = std::str::from_utf8(body).ok()?;
        let mut parts = text.split(';');
        let _vk: u32 = parts.next()?.parse().ok()?;
        let _sc: u32 = parts.next()?.parse().ok()?;
        let uc: u32 = parts.next()?.parse().ok()?;
        let kd: u32 = parts.next()?.parse().ok()?;
        let _cs: u32 = parts.next()?.parse().ok()?;
        let _rc: u32 = parts.next()?.parse().ok()?;
        if kd != 1 || uc == 0 {
            return Some(Vec::new());
        }
        let unit = uc as u16;
        if (0xD800..=0xDBFF).contains(&unit) {
            self.high_surrogate = Some(unit);
            return Some(Vec::new());
        }
        let scalar = if (0xDC00..=0xDFFF).contains(&unit) {
            let high = self.high_surrogate.take()?;
            0x10000 + (((high as u32 - 0xD800) << 10) | (unit as u32 - 0xDC00))
        } else {
            self.high_surrogate = None;
            unit as u32
        };
        let ch = char::from_u32(scalar)?;
        let mut buf = [0u8; 4];
        Some(ch.encode_utf8(&mut buf).as_bytes().to_vec())
    }
}

fn shell_command_line() -> String {
    env::var("TERMPHIN_SHELL").unwrap_or_else(|_| "powershell.exe -NoLogo".to_owned())
}

/// PowerShell's `cd`/`Set-Location` only ever updates its own `$PWD`, never
/// the process's actual OS-level current directory - so unlike Unix, where
/// the agent reads `/proc/<pid>/cwd` from outside, there is nothing external
/// to read here. This wraps whatever `prompt` function the shell already
/// has (the user's own, from `$profile`, if any - never replaced, only
/// chained) so it reports `$PWD` itself, over an OSC marker real terminals
/// silently drop, the same trick `REPLAY_END_MARKER` already relies on.
/// Written directly into the shell's input at spawn time, before any client
/// can attach, so there is nothing for a human to see: by the time anyone
/// looks, `Clear-Host` has already wiped the setup line back off-screen.
fn install_cwd_reporting(pty_write: &SyncPipe) {
    if !shell_command_line()
        .trim_start()
        .to_ascii_lowercase()
        .starts_with("powershell")
    {
        return;
    }
    const SCRIPT: &str = "if (-not $function:__termphinOriginalPrompt) { \
        $function:__termphinOriginalPrompt = $function:prompt }; \
        function prompt { \
        [Console]::Out.Write([char]27 + ']5382;termphin-cwd;' + $PWD.Path + [char]7); \
        & $function:__termphinOriginalPrompt }; Clear-Host\r";
    let _ = pty_write.write_all(SCRIPT.as_bytes());
}

/// Returns the pseudo console, the write end of its input pipe, the read end
/// of its output pipe, and the shell process attached to it.
fn spawn_conpty_shell(size: TermSize) -> io::Result<(PseudoConsole, SyncPipe, SyncPipe, ChildProcess)> {
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
        let flags = EXTENDED_STARTUPINFO_PRESENT | CREATE_NEW_PROCESS_GROUP;

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
    history: Mutex<History>,
    size: Mutex<TermSize>,
    pending_listener: Mutex<Option<RawHandle>>,
    terminating: AtomicBool,
    pty_closed: AtomicBool,
}

impl MasterState {
    fn add_client(&self, id: u64, channel: Arc<ClientChannel>, replay: bool) -> io::Result<()> {
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
            ResizePseudoConsole(self.pseudo_console.lock().expect("pty mutex poisoned").handle, coord)
        };
        if result != 0 {
            return Err(io::Error::from_raw_os_error(result));
        }
        *current = size;
        Ok(true)
    }

    /// Briefly shrinks the pseudo console by one row to force a full-screen
    /// app to repaint on a same-size reattach - the alternate buffer has no
    /// scrollback to replay otherwise.
    fn request_redraw(&self) {
        if !self
            .history
            .lock()
            .expect("history mutex poisoned")
            .alternate_screen_active()
        {
            return;
        }
        let size = *self.size.lock().expect("size mutex poisoned");
        if size.rows < 2 {
            return;
        }
        let shrunk = TermSize {
            cols: size.cols,
            rows: size.rows - 1,
        };
        let handle = self.pseudo_console.lock().expect("pty mutex poisoned").handle;
        let shrink = unsafe {
            ResizePseudoConsole(
                handle,
                COORD {
                    X: shrunk.cols as i16,
                    Y: shrunk.rows as i16,
                },
            )
        };
        if shrink != 0 {
            return;
        }
        thread::sleep(Duration::from_millis(40));
        unsafe {
            ResizePseudoConsole(
                handle,
                COORD {
                    X: size.cols as i16,
                    Y: size.rows as i16,
                },
            );
        }
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
            ClosePseudoConsole(self.pseudo_console.lock().expect("pty mutex poisoned").handle);
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
        let _ = std::fs::remove_dir_all(directory);
        std::process::exit(0)
    }
}

fn client_loop(id: u64, pipe: AsyncPipe, state: Arc<MasterState>) {
    let writer = ClientChannel::spawn(&pipe);
    let handshake_ms = Some(HANDSHAKE_TIMEOUT.as_millis() as u32);
    let mut attached = false;
    let mut replay_requested = false;

    loop {
        let timeout = if attached { None } else { handshake_ms };
        let Ok((kind, payload)) = read_frame_timeout(&pipe, timeout) else {
            break;
        };
        let outcome = match kind {
            FRAME_HISTORY if !attached => {
                replay_requested = true;
                Ok(())
            }
            FRAME_ATTACH => {
                let attach_result = if !attached {
                    attached = true;
                    state.add_client(id, Arc::clone(&writer), replay_requested)
                } else {
                    Ok(())
                };
                attach_result
                    .and_then(|_| decode_size(&payload))
                    .and_then(|size| state.resize(size))
                    .map(|resized| {
                        if !resized {
                            state.request_redraw();
                        }
                    })
            }
            FRAME_INPUT if attached => state.write_input(&payload),
            FRAME_RESIZE if attached => {
                decode_size(&payload).and_then(|size| state.resize(size)).map(|_| ())
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
    let _ = std::fs::write(directory.join("master.log"), format!("startup failed: {error}\n"));
    error
}

/// Entry point for `termphin-agent __master <name> <cols> <rows>`.
pub(crate) fn run_as_master(mut args: impl Iterator<Item = String>) -> io::Result<()> {
    let name = args.next().ok_or_else(|| invalid_input("missing session name"))?;
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
    std::fs::create_dir_all(&directory)?;
    install_master_panic_log(directory.clone());
    let directory_log = directory.clone();
    let restore = RestoreState::load(&directory, current_boot_id().as_deref());

    let (pseudo_console, pty_write, pty_read, child) = spawn_conpty_shell(size)
        .map_err(|error| log_master_error(&directory_log, error))?;
    install_cwd_reporting(&pty_write);

    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let _ = std::fs::write(directory.join("created_at"), created_at.to_string());
    if let Some(id) = current_boot_id() {
        let _ = std::fs::write(directory.join("boot_id"), &id);
    }

    let mut history = History::default();
    if restore.reboot_restored && !restore.scrollback.is_empty() {
        history.seed_restored(&restore.scrollback);
    }

    let state = Arc::new(MasterState {
        name: Mutex::new(name.clone()),
        directory: Mutex::new(directory),
        created_at,
        child,
        pseudo_console: Mutex::new(pseudo_console),
        pty_write: Mutex::new(pty_write),
        clients: Mutex::new(HashMap::new()),
        history: Mutex::new(history),
        size: Mutex::new(size),
        pending_listener: Mutex::new(None),
        pty_closed: AtomicBool::new(false),
        terminating: AtomicBool::new(false),
    });

    let reader_state = Arc::clone(&state);
    thread::spawn(move || {
        let mut buffer = [0_u8; 8192];
        loop {
            match pty_read.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => reader_state.broadcast_output(&buffer[..count]),
                Err(error) if error.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) => break,
                Err(_) => break,
            }
        }
        reader_state.finish();
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
            if tick.is_multiple_of(SCROLLBACK_FLUSH_EVERY_TICKS) {
                persistence_state.flush_scrollback();
            }
        }
    });

    let security = OwnerOnlySecurity::new().map_err(|error| log_master_error(&directory_log, error))?;
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

fn spawn_detached(command_line: &str) -> io::Result<()> {
    unsafe {
        let mut startup_info: STARTUPINFOW = std::mem::zeroed();
        startup_info.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        let mut process_info: PROCESS_INFORMATION = std::mem::zeroed();
        let mut wide_cmd = to_wide(command_line);
        let flags = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB | CREATE_NO_WINDOW;
        let ok = CreateProcessW(
            std::ptr::null(),
            wide_cmd.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            flags,
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

pub(crate) fn list_command() -> io::Result<()> {
    let base = prepare_base_dir()?;
    let mut directories = std::fs::read_dir(base)?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false))
        .collect::<Vec<_>>();
    directories.sort_by_key(|entry| entry.file_name());

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

pub(crate) fn attach_command(name: &str, replay: bool) -> io::Result<()> {
    let _raw_mode = ConsoleRawMode::enable()?;
    let scrollback = Arc::new(Mutex::new(VecDeque::new()));

    if !replay {
        try_attach(name, false, &scrollback)?;
        return Ok(());
    }
    if try_attach(name, true, &scrollback)? == Attachment::ReplayRejected {
        try_attach(name, false, &scrollback)?;
    }
    Ok(())
}

#[derive(PartialEq, Eq)]
enum Attachment {
    Finished,
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
    scrollback: &Arc<Mutex<VecDeque<u8>>>,
) -> io::Result<Attachment> {
    let size = console_size();
    let pipe = connect_or_create(name, size)?;
    if replay {
        send_frame(&mut &pipe, FRAME_HISTORY, &[])?;
    }
    send_frame(&mut &pipe, FRAME_ATTACH, &encode_size(size))?;
    bridge_terminal(pipe, size, replay, scrollback)
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

fn bridge_terminal(
    pipe: AsyncPipe,
    initial_size: TermSize,
    replay_requested: bool,
    scrollback: &Arc<Mutex<VecDeque<u8>>>,
) -> io::Result<Attachment> {
    let pipe = Arc::new(pipe);
    let input_pipe = Arc::clone(&pipe);

    // Own thread because Windows has no single call that waits on both a
    // console input handle and a named pipe the way `poll()` does on Unix.
    // Never joined: the process exits once the output loop below finishes,
    // taking a still-blocked `ReadFile` on stdin down with it.
    thread::spawn(move || {
        let stdin = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        let mut buffer = [0_u8; 8192];
        let mut win32_input = Win32InputDecoder::default();
        loop {
            let mut read = 0u32;
            let ok = unsafe {
                ReadFile(
                    stdin,
                    buffer.as_mut_ptr(),
                    buffer.len() as u32,
                    &mut read,
                    std::ptr::null_mut(),
                )
            };
            if !win_bool(ok) || read == 0 {
                return;
            }
            let decoded = win32_input.feed(&buffer[..read as usize]);
            if decoded.is_empty() {
                continue;
            }
            if send_frame(&mut &*input_pipe, FRAME_INPUT, &decoded).is_err() {
                return;
            }
        }
    });

    let resize_pipe = Arc::clone(&pipe);
    let last_size = Arc::new(Mutex::new(initial_size));
    let resize_last_size = Arc::clone(&last_size);
    thread::spawn(move || {
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
    let mut tolerate_legacy_replay_error = replay_requested;
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
                append_scrollback(scrollback, &data);
                output.write_all(&data)?;
                output.flush()?;
            }
            (FRAME_REPLAY_DONE, _) => {
                painted = true;
                output.write_all(REPLAY_END_MARKER)?;
                output.flush()?;
            }
            (FRAME_EXIT, _) => return Ok(Attachment::Finished),
            (FRAME_ERROR, message) => {
                if tolerate_legacy_replay_error && message.as_slice() == b"invalid protocol frame" {
                    tolerate_legacy_replay_error = false;
                    continue;
                }
                if replay_requested && !painted {
                    return Ok(Attachment::ReplayRejected);
                }
                return Err(io::Error::other(String::from_utf8_lossy(&message).into_owned()));
            }
            _ => {}
        }
    }
}

struct ConsoleRawMode {
    stdin: HANDLE,
    original_input_mode: u32,
    stdout: HANDLE,
    original_output_mode: u32,
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
        if has_console {
            unsafe {
                SetConsoleMode(stdin, ENABLE_VIRTUAL_TERMINAL_INPUT);
                SetConsoleMode(stdout, original_output_mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
            }
        }
        Ok(Self {
            stdin,
            original_input_mode,
            stdout,
            original_output_mode,
        })
    }
}

impl Drop for ConsoleRawMode {
    fn drop(&mut self) {
        unsafe {
            SetConsoleMode(self.stdin, self.original_input_mode);
            SetConsoleMode(self.stdout, self.original_output_mode);
        }
    }
}

#[cfg(test)]
mod win32_input_decoder_tests {
    use super::Win32InputDecoder;

    #[test]
    fn decodes_a_real_captured_up_arrow_into_plain_esc_bracket_a() {
        // Captured verbatim from a real Win32-OpenSSH session's stdin: three
        // separate "raw character" events (Vk=0) for ESC, '[', 'A' - not one
        // recognised up-arrow key event. This is the exact input that used
        // to print "[A" instead of recalling history.
        let raw: &[u8] = &[
            0x1b, 0x5b, 0x30, 0x3b, 0x30, 0x3b, 0x32, 0x37, 0x3b, 0x31, 0x3b, 0x30, 0x3b, 0x31,
            0x5f, 0x1b, 0x5b, 0x30, 0x3b, 0x30, 0x3b, 0x39, 0x31, 0x3b, 0x31, 0x3b, 0x30, 0x3b,
            0x31, 0x5f, 0x1b, 0x5b, 0x30, 0x3b, 0x30, 0x3b, 0x36, 0x35, 0x3b, 0x31, 0x3b, 0x30,
            0x3b, 0x31, 0x5f,
        ];
        let mut decoder = Win32InputDecoder::default();
        assert_eq!(decoder.feed(raw), vec![0x1b, b'[', b'A']);
    }

    #[test]
    fn key_up_events_are_dropped_so_a_letter_is_not_doubled() {
        // Real captured key-down/key-up pair for the letter 'e'.
        let raw: &[u8] = &[
            0x1b, 0x5b, 0x36, 0x39, 0x3b, 0x31, 0x38, 0x3b, 0x31, 0x30, 0x31, 0x3b, 0x31, 0x3b,
            0x30, 0x3b, 0x31, 0x5f, 0x1b, 0x5b, 0x36, 0x39, 0x3b, 0x31, 0x38, 0x3b, 0x31, 0x30,
            0x31, 0x3b, 0x30, 0x3b, 0x30, 0x3b, 0x31, 0x5f,
        ];
        let mut decoder = Win32InputDecoder::default();
        assert_eq!(decoder.feed(raw), b"e".to_vec());
    }

    #[test]
    fn a_sequence_split_across_two_reads_still_decodes() {
        let mut decoder = Win32InputDecoder::default();
        let first = decoder.feed(b"\x1b[0;0;65;1;0");
        assert!(first.is_empty(), "incomplete sequence should not emit yet");
        let second = decoder.feed(b";1_");
        assert_eq!(second, vec![b'A']);
    }

    #[test]
    fn plain_text_with_no_escape_sequences_passes_through_unchanged() {
        let mut decoder = Win32InputDecoder::default();
        assert_eq!(decoder.feed(b"hello"), b"hello".to_vec());
    }

    #[test]
    fn an_escape_byte_not_followed_by_a_bracket_is_forwarded_literally() {
        let mut decoder = Win32InputDecoder::default();
        assert_eq!(decoder.feed(b"\x1bX"), vec![0x1b, b'X']);
    }

    #[test]
    fn a_csi_sequence_with_no_underscore_terminator_is_forwarded_literally() {
        // Not Win32-Input-Mode at all - e.g. a plain xterm sequence that
        // happens to arrive on stdin unwrapped.
        let mut decoder = Win32InputDecoder::default();
        let input = b"\x1b[Hrest";
        assert_eq!(decoder.feed(input), input.to_vec());
    }

    #[test]
    fn surrogate_pair_reassembles_into_one_astral_character() {
        // U+1F600 (grinning face) as UTF-16 surrogates: 0xD83D 0xDE00.
        let high = 0xD83Du32;
        let low = 0xDE00u32;
        let seq = format!("\x1b[0;0;{high};1;0;1_\x1b[0;0;{low};1;0;1_");
        let mut decoder = Win32InputDecoder::default();
        let decoded = decoder.feed(seq.as_bytes());
        assert_eq!(decoded, "😀".as_bytes().to_vec());
    }
}
