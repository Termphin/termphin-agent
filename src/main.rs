use std::collections::VecDeque;
use std::env;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::{Arc, Mutex};

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
pub(crate) const HISTORY_SIZE: usize = 256 * 1024;

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

pub(crate) const FRAME_OUTPUT: u8 = 101;
pub(crate) const FRAME_STATUS_RESPONSE: u8 = 102;
pub(crate) const FRAME_OK: u8 = 103;
pub(crate) const FRAME_ERROR: u8 = 104;
pub(crate) const FRAME_EXIT: u8 = 105;
/// Sent after the last replay chunk, so the client can tell a complete
/// scrollback from one still arriving. Older clients ignore unknown frames.
pub(crate) const FRAME_REPLAY_DONE: u8 = 106;

pub(crate) const REPLAY_END_MARKER: &[u8] = b"\x1b]5380;termphin-replay-end\x07";
pub(crate) const REBOOT_RESTORED_MARKER: &[u8] = b"\x1b]5381;termphin-reboot-restored\x07";

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

fn run() -> io::Result<()> {
    let mut args = env::args().skip(1);
    let command = args.next().unwrap_or_else(|| "help".to_owned());
    match command.as_str() {
        "--version" => print_version(false),
        "version" => print_version(args.next().as_deref() == Some("--machine")),
        "attach" => {
            let first = args.next();
            let (replay, name) = if first.as_deref() == Some("--replay") {
                (true, required_name(args.next())?)
            } else {
                (false, required_name(first)?)
            };
            platform::attach_command(&name, replay)?;
        }
        "list" => platform::list_command()?,
        "rename" => {
            let old_name = required_name(args.next())?;
            let new_name = required_name(args.next())?;
            platform::rename_command(&old_name, &new_name)?;
        }
        "kill" => platform::kill_command(&required_name(args.next())?)?,
        // Internal re-exec target used to detach the session master from the
        // process that spawned it. Not part of the public CLI.
        #[cfg(windows)]
        "__master" => windows::run_as_master(args)?,
        _ => {
            eprintln!("usage: termphin-agent <attach|list|rename|kill|version>");
            process::exit(2);
        }
    }
    Ok(())
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

pub(crate) const SCROLLBACK_ROWS: usize = 2000;

#[derive(Default, Clone)]
struct TitleTracker {
    title: Arc<Mutex<Option<Vec<u8>>>>,
}

impl vt100::Callbacks for TitleTracker {
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        *self.title.lock().expect("title mutex poisoned") = Some(title.to_vec());
    }
}

pub(crate) struct History {
    parser: vt100::Parser<TitleTracker>,
    title: Arc<Mutex<Option<Vec<u8>>>>,
    restored: bool,
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
        }
    }

    pub(crate) fn push(&mut self, data: &[u8]) {
        self.parser.process(data);
    }

    pub(crate) fn resize(&mut self, rows: u16, cols: u16) {
        self.parser.screen_mut().set_size(rows.max(1), cols.max(1));
    }

    pub(crate) fn snapshot(&mut self) -> Vec<u8> {
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
        output.extend(self.scrollback_lines());
        if self.parser.screen().alternate_screen() {
            output.extend(b"\x1b[?1049h");
        }
        output.extend(self.parser.screen().state_formatted());
        output
    }

    fn scrollback_lines(&mut self) -> Vec<u8> {
        let screen = self.parser.screen_mut();
        if screen.alternate_screen() {
            return Vec::new();
        }
        let (rows, cols) = screen.size();
        screen.set_scrollback(usize::MAX);
        let depth = screen.scrollback();
        let mut output = Vec::new();
        for n in (1..=depth).rev() {
            screen.set_scrollback(n);
            if let Some(line) = screen.rows(0, cols).next() {
                output.extend(line.trim_end().as_bytes());
                output.extend(b"\r\n");
            }
        }
        output.extend(std::iter::repeat_n(
            b'\n',
            depth.min(rows.saturating_sub(1).into()),
        ));
        screen.set_scrollback(0);
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

/// Queued-but-unwritten frames for one attached client. Platform-neutral: the
/// platform's client-channel wraps this with its own stream type and wakeup
/// mechanism.
#[derive(Default)]
pub(crate) struct ClientQueue {
    pub(crate) frames: VecDeque<(u8, Vec<u8>)>,
    pub(crate) bytes: usize,
    pub(crate) closed: bool,
}

pub(crate) fn append_scrollback(scrollback: &std::sync::Mutex<VecDeque<u8>>, data: &[u8]) {
    let mut buf = scrollback.lock().expect("scrollback mutex poisoned");
    buf.extend(data);
    while buf.len() > HISTORY_SIZE {
        buf.pop_front();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
