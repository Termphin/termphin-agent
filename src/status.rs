//! The OSC 7501 Program Status Protocol: a program in the session - an AI
//! coding agent, mostly - says whether it is working, blocked on the user,
//! done or failed, instead of the master guessing from its output going
//! quiet.
//!
//! The master stands in for the terminal the protocol talks to: it answers
//! the support query (`OSC 7501 ; ? ST`), keeps the records the reports
//! describe, and turns the moments worth a notification - blocked, done,
//! error - into [`StatusEvent`]s. Hooks an agent runs (`termphin-agent hook`)
//! reach the same records through the session's socket.
//!
//! Spec: <https://www.superlogical.com/rex/docs/build/program-status>.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// The longest whole sequence the protocol allows.
pub(crate) const MAX_SEQUENCE: usize = 4096;

const MAX_MSG_ENCODED: usize = 2732;
const MAX_MSG: usize = 2048;
const MAX_TITLE_ENCODED: usize = 256;
const MAX_TITLE: usize = 192;
const MAX_APP: usize = 32;
const MAX_ID: usize = 128;
const MAX_ID_SEGMENT: usize = 32;
const MAX_ID_DEPTH: usize = 8;
const MAX_KEY: usize = 16;

/// The most records one session keeps; past it the least recently updated
/// goes, as the spec allows.
const MAX_RECORDS: usize = 64;

/// How long after one notification another of the same kind from the
/// session is taken for the same moment told twice - by the program and by
/// its hook, or by a report and the repeat that follows it.
const SAME_MOMENT: Duration = Duration::from_secs(20);

/// How long a finished turn with no words of its own waits for the same
/// moment told with them: Claude Code reports `done` bare, and its `Stop`
/// hook, a moment later, brings the last thing it said.
const WORDS_GRACE: Duration = Duration::from_millis(1500);

/// The most a notification quotes of what a program last said.
pub(crate) const SAID_CHARS: usize = 180;

/// The reply to the support query: every state and kind is understood.
pub(crate) fn query_reply() -> Vec<u8> {
    b"\x1b]7501;?:states=idle,working,done,blocked,error:kinds=permission,question,auth\x1b\\"
        .to_vec()
}

/// What a program says it is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProgramState {
    Idle,
    Working,
    Done,
    Blocked,
    Error,
}

impl ProgramState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Done => "done",
            Self::Blocked => "blocked",
            Self::Error => "error",
        }
    }

    /// Reads [`Self::as_str`] back.
    pub(crate) fn parse_name(text: &str) -> Option<Self> {
        match text {
            "idle" => Some(Self::Idle),
            "working" => Some(Self::Working),
            "done" => Some(Self::Done),
            "blocked" => Some(Self::Blocked),
            "error" => Some(Self::Error),
            _ => None,
        }
    }

    /// How much a record in this state wants the user, for picking the one
    /// a session shows.
    fn urgency(self) -> u8 {
        match self {
            Self::Blocked => 5,
            Self::Error => 4,
            Self::Done => 3,
            Self::Working => 2,
            Self::Idle => 1,
        }
    }
}

/// What a blocked program needs from the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BlockedKind {
    Permission,
    Question,
    Auth,
}

impl BlockedKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Permission => "permission",
            Self::Question => "question",
            Self::Auth => "auth",
        }
    }

    pub(crate) fn parse_name(text: &str) -> Option<Self> {
        match text {
            "permission" => Some(Self::Permission),
            "question" => Some(Self::Question),
            "auth" => Some(Self::Auth),
            _ => None,
        }
    }
}

/// One report: a state for the record at [id], or `None` to clear it and
/// everything under it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Report {
    pub(crate) id: String,
    pub(crate) state: Option<ProgramState>,
    pub(crate) kind: Option<BlockedKind>,
    pub(crate) app: Option<String>,
    pub(crate) msg: Option<String>,
}

/// What an OSC body after `7501;` is: the support query, a report, or
/// neither.
#[derive(Debug, PartialEq)]
pub(crate) enum Sequence {
    Query,
    Report(Report),
}

/// Reads an OSC 7501 body, the part after `7501;`. A report that breaks any
/// rule of the spec is dropped whole.
pub(crate) fn parse(body: &[u8]) -> Option<Sequence> {
    if body.len() > MAX_SEQUENCE {
        return None;
    }
    let text = std::str::from_utf8(body).ok()?;
    if text.trim().starts_with('?') {
        return Some(Sequence::Query);
    }
    let mut pairs = BTreeMap::new();
    for pair in text.split(':') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        if key.is_empty()
            || key.len() > MAX_KEY
            || !key.bytes().all(|byte| byte.is_ascii_lowercase())
            || !value.bytes().all(value_byte)
        {
            continue;
        }
        pairs.insert(key, value);
    }
    let state = match *pairs.get("state")? {
        "clear" => None,
        name => Some(ProgramState::parse_name(name)?),
    };
    let id = pairs.get("id").copied().unwrap_or("");
    if !id.is_empty() && !valid_id(id) {
        return None;
    }
    let msg = match pairs.get("msg") {
        Some(encoded) => Some(decode_text(encoded, MAX_MSG_ENCODED, MAX_MSG)?),
        None => None,
    };
    if let Some(encoded) = pairs.get("title") {
        decode_text(encoded, MAX_TITLE_ENCODED, MAX_TITLE)?;
    }
    let app = pairs
        .get("app")
        .filter(|app| valid_app(app))
        .map(|app| (*app).to_owned());
    let kind = if state == Some(ProgramState::Blocked) {
        pairs
            .get("kind")
            .and_then(|kind| BlockedKind::parse_name(kind))
    } else {
        None
    };
    Some(Sequence::Report(Report {
        id: id.to_owned(),
        state,
        kind,
        app,
        msg: msg.filter(|msg| !msg.is_empty()),
    }))
}

fn value_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b',' | b'+' | b'/' | b'=' | b'-')
}

fn valid_id(id: &str) -> bool {
    id.len() <= MAX_ID
        && id.split('/').count() <= MAX_ID_DEPTH
        && id.split('/').all(|segment| {
            !segment.is_empty()
                && segment.len() <= MAX_ID_SEGMENT
                && segment.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'+' | b'-')
                })
        })
}

fn valid_app(app: &str) -> bool {
    !app.is_empty()
        && app.len() <= MAX_APP
        && app
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'+' | b'-'))
}

/// Base64 text of at most [max_encoded] characters that decodes to at most
/// [max] bytes of one line of UTF-8 with no control characters - or `None`,
/// which drops the whole report.
fn decode_text(encoded: &str, max_encoded: usize, max: usize) -> Option<String> {
    if encoded.len() > max_encoded {
        return None;
    }
    let url_safe = encoded.replace('+', "-").replace('/', "_");
    let bytes = crate::presence::decode_base64url(&url_safe)?;
    if bytes.len() > max {
        return None;
    }
    let text = String::from_utf8(bytes).ok()?;
    if text.chars().any(char::is_control) {
        return None;
    }
    Some(text)
}

/// One record a report wrote.
#[derive(Clone, Debug, PartialEq)]
struct Record {
    state: ProgramState,
    kind: Option<BlockedKind>,
    app: Option<String>,
    msg: Option<String>,
    updated: Instant,
}

/// A moment worth telling the user about, from the record at the root or
/// any of its tasks for `Blocked`, from a top-level record for the rest.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum StatusEvent {
    Blocked {
        app: Option<String>,
        kind: Option<BlockedKind>,
        msg: Option<String>,
    },
    Done {
        app: Option<String>,
        msg: Option<String>,
    },
    Error {
        app: Option<String>,
        msg: Option<String>,
    },
}

impl StatusEvent {
    fn moment(&self) -> ProgramState {
        match self {
            Self::Blocked { .. } => ProgramState::Blocked,
            Self::Done { .. } => ProgramState::Done,
            Self::Error { .. } => ProgramState::Error,
        }
    }

    /// The program's identifier, `claude` or `codex`, when it gave one.
    pub(crate) fn app(&self) -> Option<&str> {
        match self {
            Self::Blocked { app, .. } | Self::Done { app, .. } | Self::Error { app, .. } => {
                app.as_deref()
            }
        }
    }
}

/// What the session shows for its program: the record that wants the user
/// most, with the program it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Summary {
    pub(crate) state: ProgramState,
    pub(crate) kind: Option<BlockedKind>,
    pub(crate) app: Option<String>,
    pub(crate) msg: Option<String>,
}

/// The records one session's programs reported, and the notifications they
/// are due.
#[derive(Default)]
pub(crate) struct ProgramStatus {
    records: BTreeMap<String, Record>,
    last_event: Option<(ProgramState, Instant)>,
    /// A finished turn with no words, held for [`WORDS_GRACE`] in case the
    /// same moment comes again with them.
    bare_done: Option<(StatusEvent, Instant)>,
}

impl ProgramStatus {
    /// Takes [report] in and returns the notification it is due, if any: a
    /// record that just became blocked (or blocked on something else), or
    /// a top-level one that just finished or failed. The same moment told
    /// again within [`SAME_MOMENT`] - by a hook after the program itself -
    /// is not due twice.
    pub(crate) fn apply(&mut self, report: Report, now: Instant) -> Option<StatusEvent> {
        let Some(state) = report.state else {
            self.clear(&report.id);
            return None;
        };
        let app = report
            .app
            .clone()
            .or_else(|| self.inherited_app(&report.id));
        let before = self.records.get(&report.id).cloned();
        let record = Record {
            state,
            kind: report.kind,
            app: report.app,
            msg: report.msg,
            updated: now,
        };
        let event = match state {
            ProgramState::Blocked
                if before.as_ref().is_none_or(|before| {
                    before.state != ProgramState::Blocked
                        || before.kind != record.kind
                        || before.msg != record.msg
                }) =>
            {
                Some(StatusEvent::Blocked {
                    app,
                    kind: record.kind,
                    msg: record.msg.clone(),
                })
            }
            ProgramState::Done | ProgramState::Error
                if !report.id.contains('/')
                    && (before.as_ref().is_none_or(|before| before.state != state)
                        || (state == ProgramState::Done
                            && record.msg.is_some()
                            && self.bare_done.is_some())) =>
            {
                Some(if state == ProgramState::Done {
                    StatusEvent::Done {
                        app,
                        msg: record.msg.clone(),
                    }
                } else {
                    StatusEvent::Error {
                        app,
                        msg: record.msg.clone(),
                    }
                })
            }
            _ => None,
        };
        self.records.insert(report.id, record);
        self.trim();
        let event = event?;
        if let StatusEvent::Done { msg: None, .. } = event {
            if self.bare_done.is_none() && !self.told_lately(ProgramState::Done, now) {
                self.bare_done = Some((event, now));
            }
            return None;
        }
        if matches!(event, StatusEvent::Done { .. }) && self.bare_done.take().is_some() {
            self.last_event = Some((ProgramState::Done, now));
            return Some(event);
        }
        self.announce(event, now)
    }

    /// A finished turn held for its words, once [`WORDS_GRACE`] is over
    /// without them: told bare after all.
    pub(crate) fn due(&mut self, now: Instant) -> Option<StatusEvent> {
        let (_, since) = self.bare_done.as_ref()?;
        if now.saturating_duration_since(*since) < WORDS_GRACE {
            return None;
        }
        let (event, _) = self.bare_done.take()?;
        self.announce(event, now)
    }

    fn told_lately(&self, moment: ProgramState, now: Instant) -> bool {
        self.last_event.is_some_and(|(last, at)| {
            last == moment && now.saturating_duration_since(at) < SAME_MOMENT
        })
    }

    fn announce(&mut self, event: StatusEvent, now: Instant) -> Option<StatusEvent> {
        let moment = event.moment();
        if self.told_lately(moment, now) {
            return None;
        }
        self.last_event = Some((moment, now));
        Some(event)
    }

    fn clear(&mut self, id: &str) {
        if id.is_empty() {
            self.records.clear();
            return;
        }
        let below = format!("{id}/");
        self.records
            .retain(|key, _| key != id && !key.starts_with(&below));
    }

    /// The `app` the nearest record above [id] carries, which a record
    /// without its own inherits.
    fn inherited_app(&self, id: &str) -> Option<String> {
        let mut path = id;
        while let Some((parent, _)) = path.rsplit_once('/') {
            if let Some(app) = self
                .records
                .get(parent)
                .and_then(|record| record.app.clone())
            {
                return Some(app);
            }
            path = parent;
        }
        if id.is_empty() {
            return None;
        }
        self.records.get("").and_then(|record| record.app.clone())
    }

    fn trim(&mut self) {
        while self.records.len() > MAX_RECORDS {
            let Some(oldest) = self
                .records
                .iter()
                .min_by_key(|(_, record)| record.updated)
                .map(|(id, _)| id.clone())
            else {
                return;
            };
            self.records.remove(&oldest);
        }
    }

    /// The program in front ended, or the shell is back: what it was doing
    /// or waiting for is over. What it finished or failed stays until the
    /// user has seen it.
    pub(crate) fn program_ended(&mut self) {
        self.records
            .retain(|_, record| matches!(record.state, ProgramState::Done | ProgramState::Error));
    }

    /// The user is at the session - typing, or just attached - so what was
    /// finished or failed has been seen.
    pub(crate) fn seen(&mut self) {
        self.records
            .retain(|_, record| !matches!(record.state, ProgramState::Done | ProgramState::Error));
    }

    /// Whether a program in the session says itself what it is doing, so
    /// the master does not guess from silence, and the notifications it
    /// also sends the classic way are the same moments told again.
    pub(crate) fn reporting(&self) -> bool {
        !self.records.is_empty()
    }

    /// The record that wants the user most - the newest among equals - or
    /// `None` when nothing is reported.
    pub(crate) fn summary(&self) -> Option<Summary> {
        let (id, record) = self.records.iter().max_by(|a, b| {
            a.1.state
                .urgency()
                .cmp(&b.1.state.urgency())
                .then(a.1.updated.cmp(&b.1.updated))
        })?;
        Some(Summary {
            state: record.state,
            kind: record.kind,
            app: record.app.clone().or_else(|| self.inherited_app(id)),
            msg: record.msg.clone(),
        })
    }
}

/// A program's identifier as people know it, for a notification's title.
pub(crate) fn app_name(app: &str) -> String {
    match app.to_ascii_lowercase().as_str() {
        "claude" | "claude-code" | "claudecode" => "Claude Code".to_owned(),
        "codex" | "codex-cli" => "Codex".to_owned(),
        "amp" => "Amp".to_owned(),
        "goose" => "goose".to_owned(),
        "qwen" | "qwen-code" => "Qwen Code".to_owned(),
        "opencode" => "OpenCode".to_owned(),
        "gemini" | "gemini-cli" => "Gemini CLI".to_owned(),
        "copilot" | "copilot-cli" => "Copilot".to_owned(),
        "crush" => "Crush".to_owned(),
        "droid" | "factory-droid" => "Droid".to_owned(),
        "kiro" | "kiro-cli" => "Kiro".to_owned(),
        "vibe" | "mistral-vibe" => "Mistral Vibe".to_owned(),
        "antigravity" | "antigravity-cli" | "agy" => "Antigravity".to_owned(),
        _ => app.to_owned(),
    }
}

/// The body of an OSC 7501 report for [state], for a hook to hand the
/// master in the same words a program would use.
pub(crate) fn report_body(
    id: &str,
    state: ProgramState,
    kind: Option<BlockedKind>,
    app: &str,
    msg: Option<&str>,
) -> String {
    let mut body = format!("state={}:app={app}", state.as_str());
    if !id.is_empty() {
        body.push_str(&format!(":id={id}"));
    }
    if let Some(kind) = kind {
        body.push_str(&format!(":kind={}", kind.as_str()));
    }
    if let Some(msg) = msg.map(one_line).filter(|msg| !msg.is_empty()) {
        let clipped: String = msg.chars().take(500).collect();
        body.push_str(&format!(
            ":msg={}",
            crate::presence::encode_base64(clipped.as_bytes())
        ));
    }
    body
}

/// The last two sentences of [text] - what an agent said last, from the
/// start of the one before last - cut at a word to [`SAID_CHARS`] when they
/// run longer, with the marks of Markdown taken out, or `None` when nothing
/// is left of it.
pub(crate) fn last_sentences(text: &str) -> Option<String> {
    let mut plain = String::new();
    let mut in_code = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if in_code || trimmed.is_empty() {
            continue;
        }
        let stripped = trimmed
            .trim_start_matches('#')
            .trim_start_matches(['-', '*', '>'])
            .trim_start();
        plain.push_str(stripped);
        plain.push(' ');
    }
    let plain = one_line(&plain.replace(['*', '`'], "").replace("__", ""));
    if plain.is_empty() {
        return None;
    }
    let mut ends = Vec::new();
    let chars: Vec<char> = plain.chars().collect();
    for (index, c) in chars.iter().enumerate() {
        if matches!(c, '.' | '!' | '?')
            && chars.get(index + 1).is_none_or(|next| next.is_whitespace())
        {
            ends.push(index + 1);
        }
    }
    if ends.last() != Some(&chars.len()) {
        ends.push(chars.len());
    }
    let start = if ends.len() >= 3 {
        ends[ends.len() - 3]
    } else {
        0
    };
    let tail: String = chars[start..].iter().collect();
    let tail = tail.trim();
    if tail.chars().count() <= SAID_CHARS {
        return Some(tail.to_owned());
    }
    let cut: String = tail.chars().take(SAID_CHARS - 3).collect();
    let cut = match cut.rfind(' ') {
        Some(space) if space > SAID_CHARS / 2 => &cut[..space],
        _ => cut.as_str(),
    };
    Some(format!("{}...", cut.trim_end_matches([',', ';', ':', ' '])))
}

/// [text] on one line, with every control character a space.
pub(crate) fn one_line(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(body: &str) -> Report {
        match parse(body.as_bytes()) {
            Some(Sequence::Report(report)) => report,
            other => panic!("not a report: {other:?}"),
        }
    }

    fn b64(text: &str) -> String {
        crate::presence::encode_base64(text.as_bytes())
    }

    #[test]
    fn the_support_query_is_told_from_a_report() {
        assert_eq!(parse(b"?"), Some(Sequence::Query));
        assert!(matches!(parse(b"state=idle"), Some(Sequence::Report(_))));
        assert!(query_reply().starts_with(b"\x1b]7501;?"));
        assert!(query_reply().ends_with(b"\x1b\\"));
    }

    #[test]
    fn a_report_carries_its_state_kind_app_and_message() {
        let found = report(&format!(
            "state=blocked:kind=permission:app=claude:msg={}",
            b64("Allow Bash?")
        ));
        assert_eq!(found.state, Some(ProgramState::Blocked));
        assert_eq!(found.kind, Some(BlockedKind::Permission));
        assert_eq!(found.app.as_deref(), Some("claude"));
        assert_eq!(found.msg.as_deref(), Some("Allow Bash?"));
        assert_eq!(found.id, "");
    }

    #[test]
    fn spaces_unknown_keys_and_broken_pairs_are_passed_over() {
        let found = report(" state = working : colour=blue : nonsense : id=task/1 ");
        assert_eq!(found.state, Some(ProgramState::Working));
        assert_eq!(found.id, "task/1");
    }

    #[test]
    fn a_report_breaking_a_rule_is_dropped_whole() {
        for body in [
            "state=sleeping".to_owned(),
            "kind=permission".to_owned(),
            format!("state=blocked:id={}", "x".repeat(MAX_ID_SEGMENT + 1)),
            "state=blocked:id=a//b".to_owned(),
            format!("state=blocked:id={}", "a/".repeat(8) + "a"),
            "state=blocked:msg=A".to_owned(),
            format!("state=blocked:msg={}", b64("line\nbreak")),
            format!("state=blocked:msg={}", b64("bell\u{7}")),
            format!("state=blocked:msg={}", b64(&"x".repeat(MAX_MSG + 1))),
            format!("state=blocked:title={}", b64(&"x".repeat(MAX_TITLE + 1))),
        ] {
            assert_eq!(parse(body.as_bytes()), None, "{body}");
        }
        assert_eq!(parse(&vec![b'a'; MAX_SEQUENCE + 1]), None);
    }

    #[test]
    fn a_kind_counts_only_for_a_blocked_program_and_a_bad_app_is_dropped() {
        assert_eq!(report("state=working:kind=permission").kind, None);
        assert_eq!(report("state=blocked:kind=coffee").kind, None);
        assert_eq!(report("state=idle:app=has/slash").app, None);
    }

    #[test]
    fn being_blocked_is_due_once_and_again_when_it_asks_something_else() {
        let start = Instant::now();
        let mut status = ProgramStatus::default();
        assert_eq!(
            status.apply(report("state=working:app=claude"), start),
            None
        );
        let first = status.apply(
            report(&format!(
                "state=blocked:kind=permission:msg={}",
                b64("Bash")
            )),
            start,
        );
        assert_eq!(
            first,
            Some(StatusEvent::Blocked {
                app: None,
                kind: Some(BlockedKind::Permission),
                msg: Some("Bash".to_owned()),
            })
        );
        let later = start + SAME_MOMENT + Duration::from_secs(1);
        assert_eq!(
            status.apply(
                report(&format!(
                    "state=blocked:kind=permission:msg={}",
                    b64("Bash")
                )),
                later
            ),
            None
        );
        assert!(
            status
                .apply(
                    report(&format!(
                        "state=blocked:kind=question:msg={}",
                        b64("Which one?")
                    )),
                    later
                )
                .is_some()
        );
    }

    #[test]
    fn a_task_finishing_is_no_news_but_the_program_finishing_is() {
        let start = Instant::now();
        let mut status = ProgramStatus::default();
        status.apply(report("state=working:app=codex"), start);
        assert_eq!(status.apply(report("state=done:id=agent/1"), start), None);
        assert_eq!(status.due(start + WORDS_GRACE), None);
        assert_eq!(status.apply(report("state=done:id=turn"), start), None);
        assert_eq!(
            status.due(start + WORDS_GRACE),
            Some(StatusEvent::Done {
                app: Some("codex".to_owned()),
                msg: None,
            })
        );
    }

    #[test]
    fn a_task_asking_for_permission_is_news_and_inherits_the_program() {
        let start = Instant::now();
        let mut status = ProgramStatus::default();
        status.apply(report("state=working:app=claude"), start);
        assert_eq!(
            status.apply(report("state=blocked:id=agent/2:kind=permission"), start),
            Some(StatusEvent::Blocked {
                app: Some("claude".to_owned()),
                kind: Some(BlockedKind::Permission),
                msg: None,
            })
        );
    }

    #[test]
    fn the_same_moment_told_twice_is_due_once() {
        let start = Instant::now();
        let mut status = ProgramStatus::default();
        assert_eq!(status.apply(report("state=done:app=claude"), start), None);
        assert!(status.due(start + WORDS_GRACE).is_some());
        assert_eq!(
            status.apply(
                report("state=done:id=hook:app=claude"),
                start + Duration::from_secs(2)
            ),
            None
        );
        assert!(
            status
                .apply(
                    report("state=error:id=hook"),
                    start + Duration::from_secs(3)
                )
                .is_some()
        );
    }

    #[test]
    fn ending_the_program_keeps_only_what_it_finished_and_seeing_it_clears_that() {
        let start = Instant::now();
        let mut status = ProgramStatus::default();
        status.apply(report("state=blocked:kind=question"), start);
        status.apply(report("state=done:id=other"), start);
        status.program_ended();
        assert_eq!(
            status.summary().map(|summary| summary.state),
            Some(ProgramState::Done)
        );
        status.seen();
        assert!(!status.reporting());
    }

    #[test]
    fn the_session_shows_what_wants_the_user_most() {
        let start = Instant::now();
        let mut status = ProgramStatus::default();
        status.apply(report("state=working:app=claude"), start);
        status.apply(
            report(&format!(
                "state=blocked:id=agent/1:kind=permission:msg={}",
                b64("Edit")
            )),
            start + Duration::from_secs(1),
        );
        assert_eq!(
            status.summary(),
            Some(Summary {
                state: ProgramState::Blocked,
                kind: Some(BlockedKind::Permission),
                app: Some("claude".to_owned()),
                msg: Some("Edit".to_owned()),
            })
        );
        status.apply(
            report("state=clear:id=agent"),
            start + Duration::from_secs(2),
        );
        assert_eq!(
            status.summary().map(|summary| summary.state),
            Some(ProgramState::Working)
        );
        status.apply(report("state=clear"), start + Duration::from_secs(3));
        assert_eq!(status.summary(), None);
    }

    #[test]
    fn records_are_bounded() {
        let start = Instant::now();
        let mut status = ProgramStatus::default();
        for index in 0..(MAX_RECORDS + 10) {
            status.apply(
                report(&format!("state=working:id=t{index}")),
                start + Duration::from_millis(index as u64),
            );
        }
        assert_eq!(status.records.len(), MAX_RECORDS);
        assert!(!status.records.contains_key("t0"));
    }

    #[test]
    fn a_hook_report_reads_back_as_the_same_report() {
        let body = report_body(
            "hook",
            ProgramState::Blocked,
            Some(BlockedKind::Permission),
            "claude",
            Some("Claude needs\nyour permission to use Bash"),
        );
        let found = report(&body);
        assert_eq!(found.id, "hook");
        assert_eq!(found.kind, Some(BlockedKind::Permission));
        assert_eq!(
            found.msg.as_deref(),
            Some("Claude needs your permission to use Bash")
        );
    }

    #[test]
    fn a_bare_done_waits_for_the_same_moment_with_words() {
        let start = Instant::now();
        let mut status = ProgramStatus::default();
        status.apply(report("state=working:app=claude"), start);
        assert_eq!(status.apply(report("state=done:app=claude"), start), None);
        assert_eq!(status.due(start + Duration::from_millis(500)), None);
        let worded = status.apply(
            report(&format!(
                "state=done:id=hook:app=claude:msg={}",
                b64("All tests pass.")
            )),
            start + Duration::from_millis(800),
        );
        assert_eq!(
            worded,
            Some(StatusEvent::Done {
                app: Some("claude".to_owned()),
                msg: Some("All tests pass.".to_owned()),
            })
        );
        assert_eq!(status.due(start + Duration::from_secs(5)), None);
    }

    #[test]
    fn words_for_a_done_already_held_bare_count_even_from_the_same_record() {
        let start = Instant::now();
        let mut status = ProgramStatus::default();
        status.apply(report("state=working:id=hook:app=claude"), start);
        assert_eq!(
            status.apply(report("state=done:id=hook:app=claude"), start),
            None
        );
        assert_eq!(
            status.apply(
                report(&format!(
                    "state=done:id=hook:app=claude:msg={}",
                    b64("Beta is ready.")
                )),
                start + Duration::from_millis(5)
            ),
            Some(StatusEvent::Done {
                app: Some("claude".to_owned()),
                msg: Some("Beta is ready.".to_owned()),
            })
        );
        assert_eq!(
            status.apply(
                report("state=done:app=claude"),
                start + Duration::from_millis(30)
            ),
            None
        );
        assert_eq!(status.due(start + Duration::from_secs(3)), None);
    }

    #[test]
    fn a_bare_done_is_told_bare_when_no_words_follow() {
        let start = Instant::now();
        let mut status = ProgramStatus::default();
        assert_eq!(status.apply(report("state=done:app=codex"), start), None);
        assert_eq!(status.due(start + Duration::from_millis(1000)), None);
        assert_eq!(
            status.due(start + WORDS_GRACE),
            Some(StatusEvent::Done {
                app: Some("codex".to_owned()),
                msg: None,
            })
        );
        assert_eq!(status.due(start + Duration::from_secs(3)), None);
        assert_eq!(
            status.apply(
                report("state=done:id=hook:app=codex"),
                start + Duration::from_secs(4)
            ),
            None
        );
    }

    #[test]
    fn what_an_agent_said_last_is_its_last_two_sentences() {
        assert_eq!(
            last_sentences("I looked at the logs. The port was taken. I restarted the server, so it is up again!")
                .as_deref(),
            Some("The port was taken. I restarted the server, so it is up again!")
        );
        assert_eq!(
            last_sentences("## Done\n\n- Fixed **the** `config`\n```\nnpm test\n```\nAll green")
                .as_deref(),
            Some("Done Fixed the config All green")
        );
        assert_eq!(last_sentences("Short.").as_deref(), Some("Short."));
        assert_eq!(last_sentences("   \n```\ncode\n```"), None);
        let long = format!("First. {} end.", "word ".repeat(60));
        let said = last_sentences(&long).expect("something said");
        assert!(said.starts_with("First. word"));
        assert!(said.ends_with("..."));
        assert!(said.chars().count() <= SAID_CHARS);
        assert_eq!(
            last_sentences("Version 1.2.3 is out. Try it.").as_deref(),
            Some("Version 1.2.3 is out. Try it.")
        );
    }

    #[test]
    fn known_programs_are_named_the_way_people_say_them() {
        assert_eq!(app_name("claude"), "Claude Code");
        assert_eq!(app_name("codex"), "Codex");
        assert_eq!(app_name("antigravity"), "Antigravity");
        assert_eq!(app_name("something-new"), "something-new");
    }
}
