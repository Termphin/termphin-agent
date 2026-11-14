//! Notifications that reach the phone while no phone is attached.
//!
//! The master watches the session's output for the notification sequences a
//! program sends - `OSC 9` (iTerm2's, what Claude Code sends), `OSC 777 ;
//! notify` (what `termphin notify` writes) and kitty's `OSC 99` - and when
//! no client is attached to show one, hands it to Termphin's relay with the
//! key the app wrote to `~/.config/termphin/relay`. Without that file
//! nothing is sent anywhere.
//!
//! A program that sends none of them is still noticed when it stops: see
//! [`QuietWatch`].
//!
//! The request goes out through `curl` - part of Windows since Windows 10
//! 1803 too - so the agent keeps its few dependencies; a server without curl
//! simply sends nothing.

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

/// How long a program has to keep writing before its going quiet is news.
const BUSY_FOR: Duration = Duration::from_secs(20);

/// A pause in the output longer than this ends a stretch of work: a spinner
/// redraws several times a second, so even a slow one never pauses this long
/// while its program works.
const STILL_BUSY_GAP: Duration = Duration::from_secs(3);

/// How long the output has to stay still before the program counts as
/// stopped.
const QUIET_AFTER: Duration = Duration::from_secs(8);

/// Notices a program that worked for a while and then went quiet, without
/// any help from it.
///
/// An AI agent, a build or an installer asking `Continue? [Y/n]` all show
/// the same thing from outside: output that kept coming - a spinner, a
/// timer, a log - and then stops while the program still holds the
/// terminal. That is when someone is likely needed, whichever program it
/// is. Typing ends a stretch of work (whoever types is watching), and so
/// does a notification the program sent itself, which says more than a
/// guess.
#[derive(Default)]
pub(crate) struct QuietWatch {
    busy: Option<Busy>,
}

struct Busy {
    since: Instant,
    last_output: Instant,
}

/// A program that went quiet: its name, if the system says, and how long it
/// had been working.
#[derive(Debug, PartialEq)]
pub(crate) struct WentQuiet {
    /// The process group that went quiet.
    pub(crate) group: i32,
    pub(crate) command: Option<String>,
    pub(crate) worked: Duration,
    /// What the screen shows, as [`screen_line`] picks it.
    pub(crate) line: Option<String>,
}

impl QuietWatch {
    /// The program wrote to the terminal at [now].
    pub(crate) fn output(&mut self, now: Instant) {
        match &mut self.busy {
            Some(busy) if now.saturating_duration_since(busy.last_output) <= STILL_BUSY_GAP => {
                busy.last_output = now;
            }
            _ => {
                self.busy = Some(Busy {
                    since: now,
                    last_output: now,
                })
            }
        }
    }

    /// Someone typed into the session.
    pub(crate) fn typed(&mut self) {
        self.busy = None;
    }

    /// The program sent a notification of its own.
    pub(crate) fn notified(&mut self) {
        self.busy = None;
    }

    /// Checks at [now] whether the program holding the terminal stopped
    /// after a long stretch of work. [foreground] and [shell] are as for
    /// [`CommandWatch::observe`]: at the shell's own prompt nothing is
    /// waiting, and a command that ended is that watch's news.
    pub(crate) fn check(
        &mut self,
        foreground: Option<i32>,
        shell: i32,
        now: Instant,
        name_of: impl Fn(i32) -> Option<String>,
    ) -> Option<WentQuiet> {
        let busy = self.busy.as_ref()?;
        if now.saturating_duration_since(busy.last_output) < QUIET_AFTER {
            return None;
        }
        let worked = busy.last_output.saturating_duration_since(busy.since);
        self.busy = None;
        let command = foreground.filter(|&group| group > 0 && group != shell)?;
        (worked >= BUSY_FOR).then(|| WentQuiet {
            group: command,
            command: name_of(command),
            worked,
            line: None,
        })
    }
}

impl WentQuiet {
    /// The notification, announced like [`Finished::sequence`].
    pub(crate) fn sequence(&self) -> Vec<u8> {
        let title = match &self.command {
            Some(name) => format!("{name} went quiet"),
            None => "Session went quiet".to_owned(),
        };
        let plain = format!(
            "after {} of work - it may need you",
            format_duration(self.worked)
        );
        let body = match &self.line {
            Some(line) => format!("{line} · after {}", format_duration(self.worked)),
            None => plain.clone(),
        };
        notice_sequence("quiet", &title, &body, &plain)
    }
}

/// Whether [line], what a program that stopped shows, asks for something:
/// a question, a `[y/n]`, a `Password:`. With [`QuietWatch`] this tells a
/// program waiting for an answer from a log that just went still - `tail -f`
/// or `docker compose up` after the burst - where the terminal mode cannot.
pub(crate) fn asks_something(line: &str) -> bool {
    let lower = line.trim_end().to_lowercase();
    lower.ends_with('?')
        || lower.ends_with(':')
        || ["[y/n]", "(y/n)", "[yes/no]", "(yes/no)", "[y/n]?"]
            .iter()
            .any(|choice| lower.contains(choice))
}

/// The program that went quiet and how much it had written by then, so a
/// "finished" right after - with nothing written in between - is not
/// announced as a second notification about the same stop.
#[derive(Default)]
pub(crate) struct QuietAnnounced {
    last: Option<(i32, u64)>,
}

impl QuietAnnounced {
    /// The program running as [group] was announced as quiet, having
    /// written [writes] pieces of output.
    pub(crate) fn announced(&mut self, group: i32, writes: u64) {
        self.last = Some((group, writes));
    }

    /// Whether the end of [group], having written [writes] pieces, is the
    /// stop already announced; forgets it either way.
    pub(crate) fn covers(&mut self, group: i32, writes: u64) -> bool {
        self.last.take() == Some((group, writes))
    }
}

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
    /// The process group the command ran as, which tells whose last line
    /// [`Finished::line`] has to be.
    pub(crate) group: i32,
    /// The last line the command wrote, as [`screen_line`] picks it.
    pub(crate) line: Option<String>,
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
                let ended = self.end(now);
                self.running = Some((group, now, name_of(group)));
                ended
            }
            (None, None) => None,
            (None, Some(_)) => self.end(now),
        }
    }

    /// Ends the running command at [now] - back at the prompt, or straight
    /// into the next one, as in `dnf update -y; reboot` - and returns it if
    /// its end is news.
    fn end(&mut self, now: Instant) -> Option<Finished> {
        let (group, started, name) = self.running.take()?;
        let duration = now.saturating_duration_since(started);
        let watched = self
            .last_input
            .is_some_and(|typed| now.saturating_duration_since(typed) < TYPED_RECENTLY);
        (duration >= LONG_COMMAND && !watched).then_some(Finished {
            command: name,
            duration,
            group,
            line: None,
        })
    }
}

impl Finished {
    /// The notification, announced with [`notice_sequence`]: the app shows
    /// it, and with nobody attached the scanner hands it to the relay.
    pub(crate) fn sequence(&self) -> Vec<u8> {
        let title = match &self.command {
            Some(name) => format!("{name} finished"),
            None => "Command finished".to_owned(),
        };
        let plain = format!("after {}", format_duration(self.duration));
        let body = match &self.line {
            Some(line) => format!("{line} · {}", format_duration(self.duration)),
            None => plain.clone(),
        };
        notice_sequence("finished", &title, &body, &plain)
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

/// Programs that run another one, and the options of theirs that take a
/// value, which the command a notification names is past.
const WRAPPERS: [(&str, &[&str]); 9] = [
    (
        "sudo",
        &[
            "-u", "-g", "-C", "-D", "-h", "-p", "-R", "-r", "-t", "-U", "-T",
        ],
    ),
    ("doas", &["-u", "-C"]),
    ("env", &["-u", "-C", "-S"]),
    ("time", &["-f", "-o"]),
    ("nice", &["-n"]),
    ("nohup", &[]),
    ("timeout", &["-s", "-k"]),
    ("ionice", &["-c", "-n", "-p"]),
    ("chrt", &[]),
];

/// Interpreters whose program is the script they run, named in its place -
/// shells among them, since a script started as `./deploy.sh` runs as
/// `bash ./deploy.sh`.
const INTERPRETERS: [&str; 14] = [
    "node",
    "bun",
    "deno",
    "python",
    "python3",
    "ruby",
    "perl",
    "bash",
    "sh",
    "zsh",
    "dash",
    "fish",
    "powershell",
    "pwsh",
];

/// Interpreter options whose value comes as the next argument, which is
/// then not the script: PowerShell's, mostly.
const INTERPRETER_VALUES: [&str; 6] = [
    "-executionpolicy",
    "-windowstyle",
    "-outputformat",
    "-inputformat",
    "-version",
    "-configurationname",
];

/// Most words a command is named by.
const LABEL_WORDS: usize = 3;

/// How a command is named in a notification, from its arguments [argv]:
/// the program and the words after it up to the first option, at most
/// [`LABEL_WORDS`] of them - `dnf update` for `sudo dnf update -y`,
/// `docker compose up` for `docker compose up -d --build`, `train.py` for
/// `python3 train.py --epochs 50`.
///
/// Wrappers such as `sudo` are skipped, and so is everything from the first
/// option, assignment or path on: a password typed on the command line
/// (`mysql -pSecret`, a bearer token) goes after an option, and a
/// notification shows on a lock screen and passes through the push service.
pub(crate) fn command_label(argv: &[String]) -> Option<String> {
    let split_up;
    let argv = match argv {
        [shell, switches @ .., line]
            if program_name(shell).eq_ignore_ascii_case("cmd")
                && switches
                    .iter()
                    .any(|switch| switch.eq_ignore_ascii_case("/c"))
                && line.contains(' ') =>
        {
            split_up = split_command_line(line);
            &split_up[..]
        }
        _ => argv,
    };
    let mut words = argv.iter().map(String::as_str).peekable();
    let mut program = loop {
        let word = words.next()?;
        let name = program_name(word);
        if name.contains('=') {
            continue;
        }
        if name.eq_ignore_ascii_case("cmd") {
            while words.peek().is_some_and(|next| next.starts_with('/')) {
                words.next();
            }
            continue;
        }
        let Some((_, takes_value)) = WRAPPERS.iter().find(|(wrapper, _)| *wrapper == name) else {
            break name;
        };
        let mut positional = name == "timeout";
        while let Some(&next) = words.peek() {
            if next.starts_with('-') {
                words.next();
                if takes_value.contains(&next) {
                    words.next();
                }
            } else if next.contains('=') || positional {
                words.next();
                positional = false;
            } else {
                break;
            }
        }
    };
    let interpreter = INTERPRETERS.contains(&program.to_ascii_lowercase().as_str())
        || program.starts_with("python3.");
    if interpreter {
        let script = loop {
            match words.peek().copied() {
                None => break None,
                Some(word)
                    if ["-c", "-e", "-command", "-encodedcommand"]
                        .contains(&word.to_ascii_lowercase().as_str()) =>
                {
                    break None;
                }
                Some(word) if INTERPRETER_VALUES.contains(&word.to_ascii_lowercase().as_str()) => {
                    words.next();
                    words.next();
                }
                Some("-m") => {
                    words.next();
                    break words.next();
                }
                Some(word) if word.starts_with('-') => {
                    words.next();
                }
                Some(word) => {
                    words.next();
                    break Some(word);
                }
            }
        };
        let Some(script) = script else {
            return Some(program.to_owned());
        };
        program = program_name(script);
        program = [".js", ".mjs", ".cjs"]
            .iter()
            .find_map(|suffix| program.strip_suffix(suffix))
            .unwrap_or(program);
    }
    if program.is_empty() {
        return None;
    }
    let mut label = vec![program];
    for word in words {
        if label.len() == LABEL_WORDS
            || word.is_empty()
            || word.starts_with('-')
            || word.contains(['=', '/', '\\', '@', ':'])
            || word.len() > 24
        {
            break;
        }
        label.push(word);
    }
    Some(label.join(" "))
}

/// The last line the latest two commands wrote, by process group: when one
/// hands the terminal straight to the next, the first one's end is noticed
/// only after the second has started writing.
#[derive(Default)]
pub(crate) struct LastLines {
    latest: Option<(i32, String)>,
    before: Option<(i32, String)>,
}

impl LastLines {
    /// The command running as [group] wrote [line].
    pub(crate) fn wrote(&mut self, group: i32, line: String) {
        if self.latest.as_ref().is_some_and(|(by, _)| *by != group) {
            self.before = self.latest.take();
        }
        self.latest = Some((group, line));
    }

    /// The last line the command that ran as [group] wrote, forgotten once
    /// taken.
    pub(crate) fn take(&mut self, group: i32) -> Option<String> {
        [&mut self.latest, &mut self.before]
            .into_iter()
            .find(|slot| slot.as_ref().is_some_and(|(by, _)| *by == group))?
            .take()
            .map(|(_, line)| line)
    }
}

/// The program [word] names: its last path segment, `/` or `\\`, without a
/// Windows `.exe`.
fn program_name(word: &str) -> &str {
    let name = word.rsplit(['/', '\\']).next().unwrap_or(word);
    match name.len().checked_sub(4) {
        Some(stem) if name.is_char_boundary(stem) && name[stem..].eq_ignore_ascii_case(".exe") => {
            &name[..stem]
        }
        _ => name,
    }
}

/// [line] split into arguments the way Windows programs split their command
/// line: on spaces outside double quotes, `\\"` standing for a quote. Close
/// enough to name a command; the full rules about runs of backslashes only
/// matter to arguments a label never shows.
pub(crate) fn split_command_line(line: &str) -> Vec<String> {
    let mut arguments = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut started = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'"') => {
                current.push('"');
                chars.next();
                started = true;
            }
            '"' => {
                quoted = !quoted;
                started = true;
            }
            c if c.is_whitespace() && !quoted => {
                if started {
                    arguments.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            c => {
                current.push(c);
                started = true;
            }
        }
    }
    if started {
        arguments.push(current);
    }
    arguments
}

/// Which line of the screen a notification quotes.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Pick {
    /// The last line written: the one the cursor is on, or the closest
    /// above it with something to say. For a command still running.
    Written,
    /// What a program that stopped is showing: a question if there is one
    /// near the bottom, otherwise the lowest line with something to say.
    Showing,
}

/// How far up from the bottom a question still counts as what the program
/// is asking.
const QUESTION_ROWS: usize = 15;

/// Longest line a notification quotes.
const LINE_CHARS: usize = 100;

/// Words that mark a line as a hint about keys, not something the program
/// says: `Esc to cancel`, `? for shortcuts`, `shift+tab to cycle`.
const KEY_HINTS: [&str; 9] = [
    "esc to",
    "enter to",
    "ctrl+",
    "shift+",
    "for shortcuts",
    "to cycle",
    "to navigate",
    "to select",
    "to interrupt",
];

/// The line of [rows] - the screen, top to bottom, the cursor on row
/// [cursor] - a notification quotes, cleaned of the frames and markers
/// around it, or None when nothing there says anything.
pub(crate) fn screen_line(rows: &[String], cursor: usize, pick: Pick) -> Option<String> {
    let lines = rows.iter().map(|row| tidy(row)).collect::<Vec<_>>();
    match pick {
        Pick::Written => lines[..=cursor.min(lines.len().checked_sub(1)?)]
            .iter()
            .rev()
            .find(|line| says_something(line))
            .cloned(),
        Pick::Showing => {
            let bottom = lines.iter().rposition(|line| says_something(line))?;
            let question = lines[bottom.saturating_sub(QUESTION_ROWS)..=bottom]
                .iter()
                .rev()
                .find(|line| says_something(line) && line.contains('?'));
            question.or(Some(&lines[bottom])).cloned()
        }
    }
    .map(|line| shorten(&line))
}

/// [row] without the frame, bullets and selection markers around its text.
fn tidy(row: &str) -> String {
    let keep = |c: char| c.is_alphanumeric() || "[(\"'./~$#<".contains(c);
    let start = row.find(keep).unwrap_or(row.len());
    let text = &row[start..];
    text.trim_end_matches(|c: char| c.is_whitespace() || ('\u{2500}'..='\u{259f}').contains(&c))
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn says_something(line: &str) -> bool {
    let lower = line.to_lowercase();
    line.chars().filter(|c| c.is_alphabetic()).count() >= 3
        && !KEY_HINTS.iter().any(|hint| lower.contains(hint))
        && !windows_prompt(line)
}

/// Whether [line] is a Windows shell's prompt - `PS C:\\work>` or
/// `C:\\work>` - which a Windows session shows as soon as a command ends,
/// before the master notices it ended.
fn windows_prompt(line: &str) -> bool {
    let line = line.strip_prefix("PS ").unwrap_or(line);
    let mut chars = line.chars();
    matches!(
        (chars.next(), chars.next(), chars.next()),
        (Some(drive), Some(':'), Some('\\')) if drive.is_ascii_alphabetic()
    ) && line.trim_end().ends_with('>')
}

fn shorten(line: &str) -> String {
    if line.chars().count() <= LINE_CHARS {
        return line.to_owned();
    }
    let cut = line.chars().take(LINE_CHARS - 1).collect::<String>();
    format!("{}…", cut.trim_end())
}

/// A notification found in the output: one a program asked for, or one the
/// master announced itself, which carries its [kind] and a [plain] body
/// without the quoted line, for the relay to use when the user turned
/// quoted lines off.
#[derive(Debug, PartialEq)]
pub(crate) struct Notice {
    pub(crate) kind: Option<String>,
    pub(crate) title: String,
    pub(crate) body: String,
    pub(crate) plain: Option<String>,
}

/// The private sequence the master announces its own notifications with,
/// `OSC 5385 ; termphin-notice ; kind ; title ; body ; plain`: a long command
/// that `finished`, or a program that went `quiet`. Private so the app can
/// tell them from what a program asked for, and honour a setting per kind.
pub(crate) fn notice_sequence(kind: &str, title: &str, body: &str, plain: &str) -> Vec<u8> {
    format!(
        "\x1b]5385;termphin-notice;{kind};{};{};{}\x07",
        field(title),
        field(body),
        field(plain)
    )
    .into_bytes()
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
    pub(crate) fn feed(&mut self, data: &[u8]) -> Vec<Notice> {
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
fn parse(body: &[u8]) -> Option<Notice> {
    let text = String::from_utf8_lossy(body);
    let mut fields = text.splitn(2, ';');
    let code = fields.next()?;
    let rest = fields.next().unwrap_or("");
    if code == "5385" {
        let mut parts = rest.splitn(5, ';');
        if parts.next()? != "termphin-notice" {
            return None;
        }
        let kind = clean(parts.next()?);
        let title = clean(parts.next().unwrap_or(""));
        let body = clean(parts.next().unwrap_or(""));
        let plain = clean(parts.next().unwrap_or(""));
        return (!kind.is_empty() && !title.is_empty()).then_some(Notice {
            kind: Some(kind),
            title,
            body,
            plain: Some(plain),
        });
    }
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
    (!title.is_empty() || !body.is_empty()).then_some(Notice {
        kind: None,
        title,
        body,
        plain: None,
    })
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
    /// Sends [notice] from the session called [session], which the app
    /// opens when the notification is tapped. Its kind and plain body go
    /// along, so the relay can honour the user's settings for the master's
    /// own notifications.
    pub(crate) fn send(&mut self, notice: &Notice, session: &str) {
        if self.last.is_some_and(|last| last.elapsed() < QUIET) {
            return;
        }
        let Some((url, key)) = config() else {
            return;
        };
        self.last = Some(Instant::now());
        let mut payload = format!(
            "{{\"key\":{},\"title\":{},\"body\":{},\"session\":{}",
            json_string(&key),
            json_string(&notice.title),
            json_string(&notice.body),
            json_string(session)
        );
        if let Some(kind) = &notice.kind {
            payload.push_str(&format!(",\"kind\":{}", json_string(kind)));
        }
        if let Some(plain) = &notice.plain {
            payload.push_str(&format!(",\"plain\":{}", json_string(plain)));
        }
        payload.push('}');
        std::thread::spawn(move || {
            let mut curl = Command::new("curl");
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                const CREATE_NO_WINDOW: u32 = 0x0800_0000;
                curl.creation_flags(CREATE_NO_WINDOW);
            }
            let child = curl
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

/// `<url> <key>` from `~/.config/termphin/relay` - under `%USERPROFILE%` on
/// Windows - if the app wrote one.
fn config() -> Option<(String, String)> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    let path = PathBuf::from(home)
        .join(".config")
        .join("termphin")
        .join("relay");
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

    fn pairs(found: Vec<Notice>) -> Vec<(String, String)> {
        found
            .into_iter()
            .map(|notice| (notice.title, notice.body))
            .collect()
    }

    #[test]
    fn finds_each_dialect_across_chunks() {
        let mut scanner = NotificationScanner::default();
        let mut found = scanner.feed(b"work\x1b]9;Claude is wai");
        found.extend(scanner.feed(b"ting\x07 more \x1b]777;notify;Build;done\x1b\\"));
        found.extend(scanner.feed(b"\x1b]99;i=1;Tests passed\x07"));
        assert_eq!(
            pairs(found),
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
            "\x1b]5385;termphin-notice;finished;cargo finished;after 1m 35s;after 1m 35s\x07"
        );
        assert_eq!(watch.observe(Some(10), 10, later, name), None);
    }

    #[test]
    fn a_command_followed_straight_by_another_still_announces_its_end() {
        let start = Instant::now();
        let name = |group| Some(if group == 42 { "dnf update" } else { "reboot" }.to_owned());
        let mut watch = CommandWatch::default();
        watch.observe(Some(42), 10, start, name);
        let next = start + Duration::from_secs(40);
        let done = watch.observe(Some(43), 10, next, name).unwrap();
        assert_eq!(
            (done.command.as_deref(), done.group),
            (Some("dnf update"), 42)
        );
        assert_eq!(
            watch.observe(Some(43), 10, next + Duration::from_secs(2), name),
            None
        );
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
            group: 42,
            line: None,
        };
        let found = NotificationScanner::default().feed(&done.sequence());
        assert_eq!(
            pairs(found),
            vec![("make, rm finished".to_owned(), "after 1h 1m".to_owned())]
        );
    }

    fn spinner(watch: &mut QuietWatch, from: Instant, seconds: u64) -> Instant {
        let mut at = from;
        watch.output(at);
        for _ in 0..seconds * 4 {
            at += Duration::from_millis(250);
            watch.output(at);
        }
        at
    }

    #[test]
    fn a_program_that_worked_and_stopped_is_announced_once() {
        let start = Instant::now();
        let name = |_| Some("claude".to_owned());
        let mut watch = QuietWatch::default();
        let last = spinner(&mut watch, start, 95);
        assert_eq!(
            watch.check(Some(42), 10, last + Duration::from_secs(4), name),
            None
        );
        let quiet = watch
            .check(Some(42), 10, last + Duration::from_secs(9), name)
            .unwrap();
        assert_eq!(
            String::from_utf8(quiet.sequence()).unwrap(),
            "\x1b]5385;termphin-notice;quiet;claude went quiet;after 1m 35s of work - it may need you;after 1m 35s of work - it may need you\x07"
        );
        assert_eq!(
            watch.check(Some(42), 10, last + Duration::from_secs(30), name),
            None
        );
    }

    #[test]
    fn short_work_typing_and_own_notifications_are_not_announced() {
        let start = Instant::now();
        let name = |_| None;

        let mut watch = QuietWatch::default();
        let last = spinner(&mut watch, start, 10);
        assert_eq!(
            watch.check(Some(42), 10, last + Duration::from_secs(10), name),
            None
        );

        let mut watch = QuietWatch::default();
        let last = spinner(&mut watch, start, 60);
        watch.typed();
        assert_eq!(
            watch.check(Some(42), 10, last + Duration::from_secs(10), name),
            None
        );

        let mut watch = QuietWatch::default();
        let last = spinner(&mut watch, start, 60);
        watch.notified();
        assert_eq!(
            watch.check(Some(42), 10, last + Duration::from_secs(10), name),
            None
        );
    }

    #[test]
    fn back_at_the_prompt_is_not_waiting() {
        let start = Instant::now();
        let mut watch = QuietWatch::default();
        let last = spinner(&mut watch, start, 60);
        assert_eq!(
            watch.check(Some(10), 10, last + Duration::from_secs(10), |_| None),
            None
        );
        assert_eq!(
            watch.check(None, 10, last + Duration::from_secs(10), |_| None),
            None
        );
    }

    #[test]
    fn a_long_pause_starts_new_work() {
        let start = Instant::now();
        let mut watch = QuietWatch::default();
        let first = spinner(&mut watch, start, 15);
        let resumed = first + Duration::from_secs(5);
        watch.output(resumed);
        let last = spinner(&mut watch, resumed, 15);
        assert_eq!(
            watch.check(Some(42), 10, last + Duration::from_secs(9), |_| None),
            None
        );
    }

    #[test]
    fn the_scanner_reads_back_what_a_quiet_program_writes() {
        let quiet = WentQuiet {
            group: 42,
            command: None,
            worked: Duration::from_secs(42),
            line: None,
        };
        let found = NotificationScanner::default().feed(&quiet.sequence());
        assert_eq!(
            pairs(found),
            vec![(
                "Session went quiet".to_owned(),
                "after 42s of work - it may need you".to_owned()
            )]
        );
    }

    fn args(line: &str) -> Vec<String> {
        line.split(' ').map(str::to_owned).collect()
    }

    #[test]
    fn commands_go_by_program_and_subcommand() {
        let label = |line| command_label(&args(line));
        assert_eq!(label("sudo dnf update -y").as_deref(), Some("dnf update"));
        assert_eq!(
            label("docker compose up -d --build").as_deref(),
            Some("docker compose up")
        );
        assert_eq!(label("npm run build").as_deref(), Some("npm run build"));
        assert_eq!(
            label("/usr/bin/python3 train.py --epochs 50").as_deref(),
            Some("train.py")
        );
        assert_eq!(label("./deploy.sh prod").as_deref(), Some("deploy.sh prod"));
        assert_eq!(
            label("/bin/bash ./deploy.sh prod").as_deref(),
            Some("deploy.sh prod")
        );
        assert_eq!(
            label("/usr/bin/env bash ./deploy.sh prod").as_deref(),
            Some("deploy.sh prod")
        );
        assert_eq!(label("bash -c sleep 40").as_deref(), Some("bash"));
        assert_eq!(
            label("python3 -m pytest -x tests").as_deref(),
            Some("pytest")
        );
        assert_eq!(
            label("/usr/bin/python3 /home/a/.local/bin/aider --model x").as_deref(),
            Some("aider")
        );
        assert_eq!(
            label("node --no-warnings /usr/lib/node_modules/gemini-cli/dist/gemini.js").as_deref(),
            Some("gemini")
        );
        assert_eq!(label("sudo -u postgres psql").as_deref(), Some("psql"));
        assert_eq!(
            label("timeout 10m env FOO=1 make all").as_deref(),
            Some("make all")
        );
        assert_eq!(
            label("git push origin main --force").as_deref(),
            Some("git push origin")
        );
    }

    #[test]
    fn windows_commands_go_by_program_and_subcommand() {
        let label = |line: &str| command_label(&split_command_line(line));
        assert_eq!(
            label(r#""C:\Program Files\Git\cmd\git.exe" pull origin main"#).as_deref(),
            Some("git pull origin")
        );
        assert_eq!(
            label(r#"cargo.exe build --release"#).as_deref(),
            Some("cargo build")
        );
        assert_eq!(
            label(r#"C:\WINDOWS\system32\cmd.exe /d /s /c "npm run build""#).as_deref(),
            Some("npm run build")
        );
        assert_eq!(
            label(r#""C:\Python312\python.exe" C:\work\train.py --epochs 5"#).as_deref(),
            Some("train.py")
        );
        assert_eq!(
            label(r#"winget upgrade --all"#).as_deref(),
            Some("winget upgrade")
        );
    }

    #[test]
    fn windows_command_lines_split_on_spaces_outside_quotes() {
        assert_eq!(
            split_command_line(r#"  "C:\Program Files\x.exe"  a "b c" d\"e "#),
            vec!["C:\\Program Files\\x.exe", "a", "b c", "d\"e"]
        );
        assert!(split_command_line("   ").is_empty());
    }

    #[test]
    fn secrets_after_an_option_never_reach_the_label() {
        let label = |line| command_label(&args(line));
        assert_eq!(label("mysql -pSecret db").as_deref(), Some("mysql"));
        assert_eq!(
            label("curl -H Authorization:Bearer-x https://a").as_deref(),
            Some("curl")
        );
        assert_eq!(
            label("psql postgres://u:pw@host/db").as_deref(),
            Some("psql")
        );
        assert_eq!(label("TOKEN=abc deploy").as_deref(), Some("deploy"));
    }

    fn screen(rows: &[&str]) -> Vec<String> {
        rows.iter().map(|row| (*row).to_owned()).collect()
    }

    #[test]
    fn a_finished_command_quotes_its_last_line() {
        let rows = screen(&[
            "$ sudo dnf update",
            "Upgraded: 4 packages",
            "",
            "Complete!",
            "",
        ]);
        assert_eq!(
            screen_line(&rows, 3, Pick::Written).as_deref(),
            Some("Complete!")
        );
        let rows = screen(&["Building", "\r| Working (24s)"]);
        assert_eq!(
            screen_line(&rows, 1, Pick::Written).as_deref(),
            Some("Working (24s)")
        );
    }

    #[test]
    fn a_stopped_program_quotes_its_question_not_its_key_hints() {
        let rows = screen(&[
            "\u{25cf} Update(src/checkout/cart.ts)",
            "\u{256d}\u{2500}\u{2500}\u{2500}\u{256e}",
            "\u{2502} Do you want to make this edit to cart.ts? \u{2502}",
            "\u{2502} \u{276f} 1. Yes                              \u{2502}",
            "\u{2502}   2. No, and tell Claude what to do (esc)   \u{2502}",
            "\u{2570}\u{2500}\u{2500}\u{2500}\u{256f}",
            "  Esc to cancel \u{00b7} Enter to select",
            "",
        ]);
        assert_eq!(
            screen_line(&rows, 7, Pick::Showing).as_deref(),
            Some("Do you want to make this edit to cart.ts?")
        );
        let rows = screen(&[
            "Need to get 52.3 MB of archives.",
            "Do you want to continue? [Y/n] ",
        ]);
        assert_eq!(
            screen_line(&rows, 1, Pick::Showing).as_deref(),
            Some("Do you want to continue? [Y/n]")
        );
        let rows = screen(&["Tests passed", "\u{276f} ", "  ? for shortcuts"]);
        assert_eq!(
            screen_line(&rows, 1, Pick::Showing).as_deref(),
            Some("Tests passed")
        );
        assert_eq!(screen_line(&screen(&["", "  "]), 1, Pick::Showing), None);
    }

    #[test]
    fn the_line_of_a_command_survives_the_next_one_starting() {
        let mut lines = LastLines::default();
        lines.wrote(42, "Complete!".to_owned());
        lines.wrote(43, "Working (1s)".to_owned());
        lines.wrote(43, "Working (2s)".to_owned());
        assert_eq!(lines.take(42).as_deref(), Some("Complete!"));
        assert_eq!(lines.take(42), None);
        assert_eq!(lines.take(43).as_deref(), Some("Working (2s)"));
    }

    #[test]
    fn a_windows_prompt_is_not_what_a_command_said() {
        let rows = screen(&["Complete!", "", "PS C:\\Users\\a> "]);
        assert_eq!(
            screen_line(&rows, 2, Pick::Written).as_deref(),
            Some("Complete!")
        );
        let rows = screen(&["Done", "C:\\work>"]);
        assert_eq!(
            screen_line(&rows, 1, Pick::Written).as_deref(),
            Some("Done")
        );
    }

    #[test]
    fn a_long_line_is_cut() {
        let rows = screen(&[&"x".repeat(300)]);
        let line = screen_line(&rows, 0, Pick::Written).unwrap();
        assert_eq!(line.chars().count(), 100);
        assert!(line.ends_with('…'));
    }

    #[test]
    fn the_quoted_line_travels_in_the_body() {
        let done = Finished {
            command: Some("dnf update".to_owned()),
            duration: Duration::from_secs(252),
            group: 42,
            line: Some("Complete!".to_owned()),
        };
        assert_eq!(
            pairs(NotificationScanner::default().feed(&done.sequence())),
            vec![(
                "dnf update finished".to_owned(),
                "Complete! · 4m 12s".to_owned()
            )]
        );
        let quiet = WentQuiet {
            group: 42,
            command: Some("claude".to_owned()),
            worked: Duration::from_secs(185),
            line: Some("Allow edit; of cart.ts?".to_owned()),
        };
        assert_eq!(
            pairs(NotificationScanner::default().feed(&quiet.sequence())),
            vec![(
                "claude went quiet".to_owned(),
                "Allow edit, of cart.ts? · after 3m 5s".to_owned()
            )]
        );
    }

    #[test]
    fn the_masters_own_notices_carry_their_kind_and_plain_body() {
        let done = Finished {
            command: Some("dnf update".to_owned()),
            duration: Duration::from_secs(252),
            group: 42,
            line: Some("Complete!".to_owned()),
        };
        assert_eq!(
            NotificationScanner::default().feed(&done.sequence()),
            vec![Notice {
                kind: Some("finished".to_owned()),
                title: "dnf update finished".to_owned(),
                body: "Complete! · 4m 12s".to_owned(),
                plain: Some("after 4m 12s".to_owned()),
            }]
        );
        let found = NotificationScanner::default().feed(b"\x1b]9;Claude is waiting\x07");
        assert_eq!(found[0].kind, None);
        assert_eq!(found[0].plain, None);
        assert!(
            NotificationScanner::default()
                .feed(b"\x1b]5385;something-else;finished;a;b;c\x07")
                .is_empty()
        );
    }

    #[test]
    fn a_question_or_prompt_asks_something_and_a_log_line_does_not() {
        for line in [
            "Do you want to make this edit to cart.ts?",
            "Do you want to continue? [Y/n]",
            "Overwrite file (y/n)",
            "[sudo] password for alex:",
            "Password:",
        ] {
            assert!(asks_something(line), "{line}");
        }
        for line in [
            "api-1  | GET /health 200 3ms",
            "Complete!",
            "Downloading package 34/34",
            "Tests passed",
        ] {
            assert!(!asks_something(line), "{line}");
        }
    }

    #[test]
    fn a_finish_right_after_a_quiet_stop_is_the_same_news() {
        let mut announced = QuietAnnounced::default();
        announced.announced(42, 10);
        assert!(announced.covers(42, 10));
        assert!(!announced.covers(42, 10), "forgotten once asked");
        announced.announced(42, 10);
        assert!(!announced.covers(42, 11), "it wrote more since");
        announced.announced(42, 10);
        assert!(!announced.covers(43, 10), "another program");
    }

    #[test]
    fn json_strings_are_escaped() {
        assert_eq!(json_string("a\"b\\c\n"), "\"a\\\"b\\\\c\\u000a\"");
    }
}
