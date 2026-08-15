use std::collections::VecDeque;
use std::env;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process;

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
/// Sized against the client's 2000-row transcript, which fits inside this even
/// with heavy SGR markup. Anything beyond it is evicted on reattach anyway, so
/// a larger ring only buys a slower replay.
pub(crate) const HISTORY_SIZE: usize = 256 * 1024;

/// History is replayed in pieces because one frame may not exceed
/// [`MAX_FRAME_SIZE`]. Small enough that the client starts painting early.
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
            reboot_restored,
        }
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
pub(crate) struct History {
    bytes: VecDeque<u8>,
    /// Bytes ever written by the shell, including those already evicted. Used
    /// to tell whether a mode-setting sequence is still inside the ring.
    total: u64,
    modes: ModeTracker,
}

impl History {
    pub(crate) fn push(&mut self, data: &[u8]) {
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

    pub(crate) fn snapshot(&self) -> Vec<u8> {
        let mut output = Vec::with_capacity(self.bytes.len() + 64);
        output.extend(self.modes.replay_prefix(self.retained_from()));
        output.extend(self.bytes.iter().copied());
        output.extend(self.modes.replay_suffix());
        output
    }

    pub(crate) fn alternate_screen_active(&self) -> bool {
        self.modes.alternate.is_some()
    }

    pub(crate) fn seed_restored(&mut self, scrollback: &[u8]) {
        self.push(scrollback);
        self.push(REBOOT_RESTORED_MARKER);
        self.push(
            b"\r\n\x1b[33mrestored after a server restart - new shell, same directory\x1b[0m\r\n\r\n",
        );
    }
}

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
    enabled_private_modes: std::collections::BTreeSet<u16>,
    reset_default_on_modes: std::collections::BTreeSet<u16>,
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
    fn restore_state_with_no_persisted_boot_id_is_not_a_reboot() {
        let dir = temp_dir("no-boot-id");
        let restore = RestoreState::load(&dir, Some("current"));
        assert!(!restore.reboot_restored);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn seed_restored_carries_scrollback_and_marks_it() {
        let mut history = History::default();
        history.seed_restored(b"$ echo hi\nhi\n");
        let snapshot = history.snapshot();
        assert!(
            snapshot
                .windows(b"$ echo hi\nhi\n".len())
                .any(|window| window == b"$ echo hi\nhi\n")
        );
        assert!(
            snapshot
                .windows(REBOOT_RESTORED_MARKER.len())
                .any(|window| window == REBOOT_RESTORED_MARKER)
        );
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
