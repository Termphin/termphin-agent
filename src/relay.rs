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
    pub(crate) fn send(&mut self, title: &str, body: &str) {
        if self.last.is_some_and(|last| last.elapsed() < QUIET) {
            return;
        }
        let Some((url, key)) = config() else {
            return;
        };
        self.last = Some(Instant::now());
        let payload = format!(
            "{{\"key\":{},\"title\":{},\"body\":{}}}",
            json_string(&key),
            json_string(title),
            json_string(body)
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
    fn json_strings_are_escaped() {
        assert_eq!(json_string("a\"b\\c\n"), "\"a\\\"b\\\\c\\u000a\"");
    }
}
