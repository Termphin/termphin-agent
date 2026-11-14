//! Notifications that reach the phone while no phone is attached.
//!
//! The master watches the session's output for the notification sequences a
//! program sends - `OSC 9` (iTerm2's, what Claude Code sends), `OSC 777 ;
//! notify` (what `termphin notify` writes) and kitty's `OSC 99` - and when
//! no client is attached to show one, hands it to Termphin's relay with the
//! key the app wrote to `~/.config/termphin/relay`. Without that file
//! nothing is sent anywhere.
//!
//! The request goes out through `curl`, so the agent keeps its two
//! dependencies; a server without curl simply sends nothing.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Longest OSC body kept while scanning; anything longer is not a
/// notification worth carrying and is dropped.
const MAX_OSC: usize = 1024;

/// Shortest gap between two relayed notifications from one session.
const QUIET: Duration = Duration::from_secs(3);

/// How long a command has to run before its end is worth a notification.
const LONG_COMMAND: Duration = Duration::from_secs(30);

/// A command typed into this recently was being watched, not waited for:
/// `vim` or a REPL ending is no news.
const TYPED_RECENTLY: Duration = Duration::from_secs(15);

/// Notices a long command coming to an end, without any help from the
/// shell: the master polls which process group has the terminal, and a
/// command is whatever holds it other than the shell.
#[derive(Default)]
pub(crate) struct CommandWatch {
    running: Option<(i32, Instant, Option<String>)>,
    last_input: Option<Instant>,
}

/// A long command that ended: its name, if the system says, and how long it
/// ran.
#[derive(Debug, PartialEq)]
pub(crate) struct Finished {
    pub(crate) command: Option<String>,
    pub(crate) duration: Duration,
}

impl CommandWatch {
    /// Someone typed into the session.
    pub(crate) fn typed(&mut self, now: Instant) {
        self.last_input = Some(now);
    }

    /// Takes the group holding the terminal at [now]; [name_of] names a
    /// group when it starts. Returns the command that just ended, if it ran
    /// long and unattended.
    pub(crate) fn observe(
        &mut self,
        foreground: Option<i32>,
        shell: i32,
        now: Instant,
        name_of: impl Fn(i32) -> Option<String>,
    ) -> Option<Finished> {
        let command = foreground.filter(|&group| group > 0 && group != shell);
        match (command, &self.running) {
            (Some(group), Some((running, _, _))) if group == *running => None,
            (Some(group), _) => {
                self.running = Some((group, now, name_of(group)));
                None
            }
            (None, None) => None,
            (None, Some(_)) => {
                let (_, started, name) = self.running.take()?;
                let duration = now.saturating_duration_since(started);
                let watched = self
                    .last_input
                    .is_some_and(|typed| now.saturating_duration_since(typed) < TYPED_RECENTLY);
                (duration >= LONG_COMMAND && !watched).then_some(Finished {
                    command: name,
                    duration,
                })
            }
        }
    }
}

impl Finished {
    /// The notification as `termphin notify` would write it, so the app and
    /// the relay take it like any other: `OSC 777 ; notify ; title ; body`.
    pub(crate) fn sequence(&self) -> Vec<u8> {
        let title = match &self.command {
            Some(name) => format!("{} finished", field(name)),
            None => "Command finished".to_owned(),
        };
        format!(
            "\x1b]777;notify;{};after {}\x07",
            title,
            format_duration(self.duration)
        )
        .into_bytes()
    }
}

/// [text] made safe to stand as one field of an OSC sequence.
fn field(text: &str) -> String {
    clean(text).replace(';', ",")
}

fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    match (seconds / 3600, seconds % 3600 / 60, seconds % 60) {
        (0, 0, s) => format!("{s}s"),
        (0, m, s) => format!("{m}m {s}s"),
        (h, m, _) => format!("{h}h {m}m"),
    }
}

/// Finds notification sequences in output that arrives in arbitrary pieces.
#[derive(Default)]
pub(crate) struct NotificationScanner {
    state: ScanState,
    body: Vec<u8>,
}

#[derive(Default, PartialEq)]
enum ScanState {
    #[default]
    Text,
    Escape,
    Osc,
    OscEscape,
}

impl NotificationScanner {
    /// Feeds [data] and returns the notifications completed in it, as
    /// `(title, body)`.
    pub(crate) fn feed(&mut self, data: &[u8]) -> Vec<(String, String)> {
        let mut found = Vec::new();
        for &byte in data {
            match self.state {
                ScanState::Text => {
                    if byte == 0x1b {
                        self.state = ScanState::Escape;
                    }
                }
                ScanState::Escape => {
                    if byte == b']' {
                        self.state = ScanState::Osc;
                        self.body.clear();
                    } else {
                        self.state = if byte == 0x1b {
                            ScanState::Escape
                        } else {
                            ScanState::Text
                        };
                    }
                }
                ScanState::Osc => match byte {
                    0x07 => {
                        found.extend(parse(&self.body));
                        self.state = ScanState::Text;
                    }
                    0x1b => self.state = ScanState::OscEscape,
                    _ => {
                        if self.body.len() < MAX_OSC {
                            self.body.push(byte);
                        } else {
                            self.state = ScanState::Text;
                        }
                    }
                },
                ScanState::OscEscape => {
                    if byte == b'\\' {
                        found.extend(parse(&self.body));
                        self.state = ScanState::Text;
                    } else if byte == b']' {
                        self.state = ScanState::Osc;
                        self.body.clear();
                    } else {
                        self.state = ScanState::Text;
                    }
                }
            }
        }
        found
    }
}

/// The notification an OSC body asks for, if it is one.
fn parse(body: &[u8]) -> Option<(String, String)> {
    let text = String::from_utf8_lossy(body);
    let mut fields = text.splitn(2, ';');
    let code = fields.next()?;
    let rest = fields.next().unwrap_or("");
    let (title, body) = match code {
        "9" => {
            if rest.is_empty() || rest.split(';').next()?.chars().all(|c| c.is_ascii_digit()) {
                return None;
            }
            (rest.to_owned(), String::new())
        }
        "777" => {
            let mut parts = rest.splitn(3, ';');
            if parts.next()? != "notify" {
                return None;
            }
            (
                parts.next().unwrap_or("").to_owned(),
                parts.next().unwrap_or("").to_owned(),
            )
        }
        "99" => {
            let mut parts = rest.splitn(2, ';');
            let metadata = parts.next()?;
            let payload = parts.next().unwrap_or("").to_owned();
            if metadata.split(':').any(|pair| pair == "p=body") {
                (String::new(), payload)
            } else if metadata
                .split(':')
                .any(|pair| pair.starts_with("p=") && pair != "p=title")
                || metadata.split(':').any(|pair| pair == "e=1")
            {
                return None;
            } else {
                (payload, String::new())
            }
        }
        _ => return None,
    };
    let title = clean(&title);
    let body = clean(&body);
    (!title.is_empty() || !body.is_empty()).then_some((title, body))
}

fn clean(text: &str) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let flat = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    flat.chars().take(200).collect()
}

/// Sends notifications through the relay, at most one per [QUIET].
#[derive(Default)]
pub(crate) struct Relay {
    last: Option<Instant>,
}

impl Relay {
    /// Sends [title] and [body] from the session called [session], which
    /// the app opens when the notification is tapped.
    pub(crate) fn send(&mut self, title: &str, body: &str, session: &str) {
        if self.last.is_some_and(|last| last.elapsed() < QUIET) {
            return;
        }
        let Some((url, key)) = config() else {
            return;
        };
        self.last = Some(Instant::now());
        let payload = format!(
            "{{\"key\":{},\"title\":{},\"body\":{},\"session\":{}}}",
            json_string(&key),
            json_string(title),
            json_string(body),
            json_string(session)
        );
        std::thread::spawn(move || {
            let child = Command::new("curl")
                .args(["-fsS", "-m", "10", "-X", "POST"])
                .args(["-H", "Content-Type: application/json"])
                .args(["--data-binary", "@-", &url])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
            if let Ok(mut child) = child {
                if let Some(mut stdin) = child.stdin.take() {
                    let _ = stdin.write_all(payload.as_bytes());
                }
                let _ = child.wait();
            }
        });
    }
}

/// `<url> <key>` from `~/.config/termphin/relay`, if the app wrote one.
fn config() -> Option<(String, String)> {
    let home = std::env::var_os("HOME")?;
    let path = PathBuf::from(home).join(".config/termphin/relay");
    let text = std::fs::read_to_string(path).ok()?;
    let mut parts = text.split_whitespace();
    let url = parts.next()?.to_owned();
    let key = parts.next()?.to_owned();
    url.starts_with("https://").then_some((url, key))
}

pub(crate) fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_each_dialect_across_chunks() {
        let mut scanner = NotificationScanner::default();
        let mut found = scanner.feed(b"work\x1b]9;Claude is wai");
        found.extend(scanner.feed(b"ting\x07 more \x1b]777;notify;Build;done\x1b\\"));
        found.extend(scanner.feed(b"\x1b]99;i=1;Tests passed\x07"));
        assert_eq!(
            found,
            vec![
                ("Claude is waiting".to_owned(), String::new()),
                ("Build".to_owned(), "done".to_owned()),
                ("Tests passed".to_owned(), String::new()),
            ]
        );
    }

    #[test]
    fn leaves_other_sequences_alone() {
        let mut scanner = NotificationScanner::default();
        let found = scanner.feed(b"\x1b]0;title\x07\x1b]9;4;1;50\x07\x1b]52;c;aGk=\x07\x1b[31mred");
        assert!(found.is_empty());
    }

    #[test]
    fn a_long_unattended_command_is_announced_once_it_ends() {
        let start = Instant::now();
        let mut watch = CommandWatch::default();
        let name = |_| Some("cargo".to_owned());
        assert_eq!(watch.observe(Some(10), 10, start, name), None);
        assert_eq!(watch.observe(Some(42), 10, start, name), None);
        let later = start + Duration::from_secs(95);
        assert_eq!(watch.observe(Some(42), 10, later, name), None);
        let done = watch.observe(Some(10), 10, later, name).unwrap();
        assert_eq!(done.command.as_deref(), Some("cargo"));
        assert_eq!(
            String::from_utf8(done.sequence()).unwrap(),
            "\x1b]777;notify;cargo finished;after 1m 35s\x07"
        );
        assert_eq!(watch.observe(Some(10), 10, later, name), None);
    }

    #[test]
    fn short_or_watched_commands_are_not() {
        let start = Instant::now();
        let name = |_| None;
        let mut watch = CommandWatch::default();
        watch.observe(Some(42), 10, start, name);
        assert_eq!(
            watch.observe(Some(10), 10, start + Duration::from_secs(5), name),
            None
        );

        let mut watch = CommandWatch::default();
        watch.observe(Some(42), 10, start, name);
        watch.typed(start + Duration::from_secs(55));
        assert_eq!(
            watch.observe(Some(10), 10, start + Duration::from_secs(60), name),
            None
        );
    }

    #[test]
    fn the_scanner_reads_back_what_a_finished_command_writes() {
        let done = Finished {
            command: Some("make; rm".to_owned()),
            duration: Duration::from_secs(3700),
        };
        let found = NotificationScanner::default().feed(&done.sequence());
        assert_eq!(
            found,
            vec![("make, rm finished".to_owned(), "after 1h 1m".to_owned())]
        );
    }

    #[test]
    fn json_strings_are_escaped() {
        assert_eq!(json_string("a\"b\\c\n"), "\"a\\\"b\\\\c\\u000a\"");
    }
}
