//! Hooks: what an AI coding agent tells the programs it runs on its own
//! events, turned into the same program status reports OSC 7501 carries.
//!
//! They reach what OSC 7501 cannot - an agent inside tmux, which drops the
//! sequence, or a version from before it reported - and say a little more,
//! such as the tool a permission prompt is about. `termphin-agent hook
//! <agent>` reads the event, finds the session it runs in through
//! `TERMPHIN_SESSION_ID` - the name in `TERMPHIN_SESSION` goes stale once
//! the session is renamed - and hands the master the report over the
//! session's socket. Outside a Termphin session it does nothing. It never prints a
//! decision, so the agent goes on exactly as it would without it.
//!
//! `termphin-agent hooks install` writes the hooks into Claude Code's
//! `~/.claude/settings.json` and Codex's `~/.codex/config.toml`, leaving
//! everything else in them as it was; `hooks remove` takes them out again.

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::status::{BlockedKind, ProgramState, report_body};

/// The record hook reports write to, apart from the one the program reports
/// itself, so neither overwrites the other.
const HOOK_ID: &str = "hook";

/// The Claude Code events the hooks listen to, with whether the event is
/// about a tool - those take a matcher.
const CLAUDE_EVENTS: [(&str, bool); 6] = [
    ("Notification", false),
    ("PermissionRequest", true),
    ("PostToolUse", true),
    ("UserPromptSubmit", false),
    ("Stop", false),
    ("StopFailure", false),
];

/// A report a hook event stands for: the state, what a block is about, the
/// program and what it said.
#[derive(Debug, PartialEq)]
pub(crate) struct HookReport {
    pub(crate) state: ProgramState,
    pub(crate) kind: Option<BlockedKind>,
    pub(crate) app: &'static str,
    pub(crate) msg: Option<String>,
}

/// What a Claude Code hook event says, from the JSON it gets on stdin, or
/// `None` for an event that changes nothing worth showing. A subagent
/// finishing is no news; one waiting for permission is.
pub(crate) fn claude_event(event: &Value) -> Option<HookReport> {
    let name = event.get("hook_event_name")?.as_str()?;
    let subagent = event.get("agent_id").is_some_and(|id| !id.is_null());
    let text = |key: &str| {
        event
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    };
    let report = |state, kind, msg| HookReport {
        state,
        kind,
        app: "claude-code",
        msg,
    };
    match name {
        "Notification" => match event.get("notification_type")?.as_str()? {
            "permission_prompt" => Some(report(
                ProgramState::Blocked,
                Some(BlockedKind::Permission),
                text("message"),
            )),
            "elicitation_dialog" | "agent_needs_input" => Some(report(
                ProgramState::Blocked,
                Some(BlockedKind::Question),
                text("message"),
            )),
            _ => None,
        },
        "PermissionRequest" => Some(report(
            ProgramState::Blocked,
            Some(BlockedKind::Permission),
            text("tool_name").map(|tool| format!("approve {tool}")),
        )),
        "PostToolUse" | "UserPromptSubmit" => Some(report(ProgramState::Working, None, None)),
        "Stop" if !subagent => Some(report(
            ProgramState::Done,
            None,
            text("last_assistant_message").and_then(|said| crate::status::last_sentences(&said)),
        )),
        "StopFailure" if !subagent => Some(report(ProgramState::Error, None, text("error"))),
        _ => None,
    }
}

/// What Codex's `notify` program is told, from the JSON it gets as its last
/// argument: only the end of a turn.
pub(crate) fn codex_event(event: &Value) -> Option<HookReport> {
    (event.get("type")?.as_str()? == "agent-turn-complete").then(|| HookReport {
        state: ProgramState::Done,
        kind: None,
        app: "codex",
        msg: event
            .get("last-assistant-message")
            .and_then(Value::as_str)
            .and_then(crate::status::last_sentences),
    })
}

/// `termphin-agent hook <agent> [json]`: hands the session the event. Quiet
/// and successful whatever happens, so an agent never sees it fail.
pub(crate) fn hook_command(agent: Option<String>, args: Vec<String>) {
    let by_id = std::env::var(crate::SESSION_ID_ENV).ok().and_then(|id| {
        crate::platform::sessions_dir()
            .ok()
            .and_then(|base| crate::session_named_by_id(&base, &id))
    });
    let by_ancestry = || {
        crate::platform::sessions_dir()
            .ok()
            .and_then(|base| crate::session_named_by_ancestry(&base, &crate::platform::ancestors()))
    };
    let Some(session) = by_id.or_else(by_ancestry).or_else(|| {
        std::env::var(crate::SESSION_ENV)
            .ok()
            .filter(|name| !name.is_empty())
    }) else {
        return;
    };
    let report = match agent.as_deref() {
        Some("claude") => {
            let mut input = String::new();
            if io::stdin()
                .take(1 << 20)
                .read_to_string(&mut input)
                .is_err()
            {
                return;
            }
            serde_json::from_str(&input)
                .ok()
                .and_then(|event| claude_event(&event))
        }
        Some("codex") => args
            .last()
            .and_then(|json| serde_json::from_str(json).ok())
            .and_then(|event| codex_event(&event)),
        _ => None,
    };
    let Some(report) = report else {
        return;
    };
    let body = report_body(
        HOOK_ID,
        report.state,
        report.kind,
        report.app,
        report.msg.as_deref(),
    );
    let _ = crate::platform::program_status_command(&session, body.as_bytes());
}

/// How the hooks invoke this binary: its own path, quoted for the shell
/// the agent runs hooks in - double quotes on Windows, which cmd, PowerShell
/// and Git Bash all read the same way for a path.
fn hook_command_line(binary: &Path, agent: &str) -> String {
    let path = binary.to_string_lossy();
    if cfg!(windows) {
        format!("\"{path}\" hook {agent}")
    } else {
        format!("{} hook {agent}", shell_quote(&path))
    }
}

fn shell_quote(text: &str) -> String {
    if text
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'_' | b'-'))
    {
        text.to_owned()
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
}

/// Whether [command] is one of the hooks this agent writes, whichever path
/// the binary had when it wrote it.
fn ours(command: &str, agent: &str) -> bool {
    command.contains("termphin-agent") && command.trim_end().ends_with(&format!(" hook {agent}"))
}

/// Claude Code's settings with the hooks for [binary] in them: every hook
/// this agent wrote before taken out, then one entry per event added, so
/// installing twice leaves one. Everything else stays as it was.
pub(crate) fn claude_install(settings: &mut Value, binary: &Path) {
    claude_remove(settings);
    if !settings.is_object() {
        *settings = Value::Object(Map::new());
    }
    let command = hook_command_line(binary, "claude");
    let root = settings.as_object_mut().expect("settings are an object");
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()));
    if !hooks.is_object() {
        *hooks = Value::Object(Map::new());
    }
    let hooks = hooks.as_object_mut().expect("hooks are an object");
    for (event, tool) in CLAUDE_EVENTS {
        let entries = hooks
            .entry(event)
            .or_insert_with(|| Value::Array(Vec::new()));
        if !entries.is_array() {
            *entries = Value::Array(Vec::new());
        }
        let mut entry = Map::new();
        if tool {
            entry.insert("matcher".to_owned(), json!("*"));
        }
        entry.insert(
            "hooks".to_owned(),
            json!([{ "type": "command", "command": command, "timeout": 5 }]),
        );
        entries
            .as_array_mut()
            .expect("entries are an array")
            .push(Value::Object(entry));
    }
}

/// Takes every hook this agent wrote out of Claude Code's settings, and an
/// event or `hooks` left empty by that with it.
pub(crate) fn claude_remove(settings: &mut Value) {
    let Some(root) = settings.as_object_mut() else {
        return;
    };
    let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) else {
        return;
    };
    for entries in hooks.values_mut() {
        let Some(list) = entries.as_array_mut() else {
            continue;
        };
        for entry in list.iter_mut() {
            if let Some(inner) = entry.get_mut("hooks").and_then(Value::as_array_mut) {
                inner.retain(|hook| {
                    !hook
                        .get("command")
                        .and_then(Value::as_str)
                        .is_some_and(|command| ours(command, "claude"))
                });
            }
        }
        list.retain(|entry| {
            entry
                .get("hooks")
                .and_then(Value::as_array)
                .is_none_or(|inner| !inner.is_empty())
        });
    }
    hooks.retain(|_, entries| entries.as_array().is_none_or(|list| !list.is_empty()));
    if hooks.is_empty() {
        root.remove("hooks");
    }
}

/// Whether Claude Code's settings carry this agent's hooks.
pub(crate) fn claude_installed(settings: &Value) -> bool {
    settings
        .get("hooks")
        .and_then(Value::as_object)
        .is_some_and(|hooks| {
            hooks
                .values()
                .filter_map(Value::as_array)
                .flatten()
                .any(|entry| {
                    entry
                        .get("hooks")
                        .and_then(Value::as_array)
                        .is_some_and(|inner| {
                            inner.iter().any(|hook| {
                                hook.get("command")
                                    .and_then(Value::as_str)
                                    .is_some_and(|command| ours(command, "claude"))
                            })
                        })
                })
        })
}

/// What installing into Codex's config did.
#[derive(Debug, PartialEq)]
pub(crate) enum CodexInstall {
    Installed(String),
    /// `notify` already runs something else, which Codex allows only one of.
    Taken,
}

/// The top-level `notify` line of a Codex config, if there is one: the keys
/// before the first table.
fn codex_notify_line(config: &str) -> Option<usize> {
    for (index, line) in config.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') {
            return None;
        }
        if let Some(rest) = trimmed.strip_prefix("notify")
            && rest.trim_start().starts_with('=')
        {
            return Some(index);
        }
    }
    None
}

/// Codex's config with `notify` running this agent's hook, put first so it
/// stays a top-level key; a `notify` of its own is left alone.
pub(crate) fn codex_install(config: &str, binary: &Path) -> CodexInstall {
    let path = binary.to_string_lossy();
    let path = if path.contains('\'') {
        format!("\"{}\"", path.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        format!("'{path}'")
    };
    let line = format!("notify = [{path}, \"hook\", \"codex\"]");
    match codex_notify_line(config) {
        Some(index) => {
            let existing = config.lines().nth(index).unwrap_or("");
            if existing.contains("termphin-agent") {
                let mut lines: Vec<&str> = config.lines().collect();
                lines[index] = &line;
                CodexInstall::Installed(lines.join("\n") + "\n")
            } else {
                CodexInstall::Taken
            }
        }
        None => CodexInstall::Installed(format!("{line}\n{config}")),
    }
}

/// Codex's config without this agent's `notify` line.
pub(crate) fn codex_remove(config: &str) -> String {
    match codex_notify_line(config) {
        Some(index)
            if config
                .lines()
                .nth(index)
                .is_some_and(|line| line.contains("termphin-agent")) =>
        {
            let kept: Vec<&str> = config
                .lines()
                .enumerate()
                .filter(|(at, _)| *at != index)
                .map(|(_, line)| line)
                .collect();
            if kept.is_empty() {
                String::new()
            } else {
                kept.join("\n") + "\n"
            }
        }
        _ => config.to_owned(),
    }
}

/// The user's home: `HOME`, or on Windows `USERPROFILE`, where Claude Code
/// and Codex keep their folders there.
fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// Writes [contents] to [path] whole, through a file beside it, keeping a
/// copy of what was there the first time this agent changed it.
fn replace(path: &Path, contents: &str) -> io::Result<()> {
    let backup = path.with_extension(format!(
        "{}.termphin-backup",
        path.extension().and_then(|ext| ext.to_str()).unwrap_or("")
    ));
    if path.exists() && !backup.exists() {
        fs::copy(path, &backup)?;
    }
    let temp = path.with_extension("termphin-tmp");
    fs::write(&temp, contents)?;
    fs::rename(&temp, path)
}

fn claude_settings_path(home: &Path) -> PathBuf {
    home.join(".claude").join("settings.json")
}

fn codex_config_path(home: &Path) -> PathBuf {
    home.join(".codex").join("config.toml")
}

fn read_claude(path: &Path) -> io::Result<Value> {
    match fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => Ok(Value::Object(Map::new())),
        Ok(text) => serde_json::from_str(&text).map_err(|_| {
            io::Error::other("~/.claude/settings.json is not valid JSON; left as it is")
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Value::Object(Map::new())),
        Err(error) => Err(error),
    }
}

/// `termphin-agent hooks install|remove|status`: one line per agent, `claude
/// installed`, `codex taken` and so on, for the app to read. An agent whose
/// folder is not there is `absent` and left alone.
pub(crate) fn hooks_command(action: Option<String>) -> io::Result<()> {
    let home = home().ok_or_else(|| io::Error::other("HOME is not set"))?;
    let binary = std::env::current_exe()?;
    let action = action.unwrap_or_else(|| "status".to_owned());
    if !matches!(action.as_str(), "install" | "remove" | "status") {
        return Err(io::Error::other(
            "usage: termphin-agent hooks install|remove|status",
        ));
    }
    let claude_dir = home.join(".claude");
    let claude = if !claude_dir.is_dir() {
        "absent".to_owned()
    } else {
        let path = claude_settings_path(&home);
        match read_claude(&path) {
            Err(_) => "unreadable".to_owned(),
            Ok(mut settings) => match action.as_str() {
                "install" => {
                    claude_install(&mut settings, &binary);
                    write_json(&path, &settings)?;
                    "installed".to_owned()
                }
                "remove" => {
                    if claude_installed(&settings) {
                        claude_remove(&mut settings);
                        write_json(&path, &settings)?;
                    }
                    "removed".to_owned()
                }
                _ => if claude_installed(&settings) {
                    "installed"
                } else {
                    "not-installed"
                }
                .to_owned(),
            },
        }
    };
    let codex_dir = home.join(".codex");
    let codex = if !codex_dir.is_dir() {
        "absent".to_owned()
    } else {
        let path = codex_config_path(&home);
        let config = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(error),
        };
        let ours_now = codex_notify_line(&config)
            .and_then(|index| config.lines().nth(index))
            .is_some_and(|line| line.contains("termphin-agent"));
        match action.as_str() {
            "install" => match codex_install(&config, &binary) {
                CodexInstall::Installed(text) => {
                    replace(&path, &text)?;
                    "installed".to_owned()
                }
                CodexInstall::Taken => "taken".to_owned(),
            },
            "remove" => {
                if ours_now {
                    replace(&path, &codex_remove(&config))?;
                }
                "removed".to_owned()
            }
            _ => if ours_now {
                "installed"
            } else {
                "not-installed"
            }
            .to_owned(),
        }
    };
    println!("claude {claude}");
    println!("codex {codex}");
    Ok(())
}

fn write_json(path: &Path, value: &Value) -> io::Result<()> {
    let mut text = serde_json::to_string_pretty(value).map_err(io::Error::other)?;
    text.push('\n');
    replace(path, &text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binary() -> PathBuf {
        PathBuf::from("/home/me/.cache/termphin/bin/termphin-agent")
    }

    #[test]
    fn a_permission_prompt_blocks_on_permission_with_claudes_words() {
        let report = claude_event(&json!({
            "hook_event_name": "Notification",
            "notification_type": "permission_prompt",
            "message": "Claude needs your permission to use Bash",
        }))
        .expect("a report");
        assert_eq!(report.state, ProgramState::Blocked);
        assert_eq!(report.kind, Some(BlockedKind::Permission));
        assert_eq!(
            report.msg.as_deref(),
            Some("Claude needs your permission to use Bash")
        );
    }

    #[test]
    fn a_permission_request_names_the_tool_and_never_what_it_runs() {
        let report = claude_event(&json!({
            "hook_event_name": "PermissionRequest",
            "tool_name": "Bash",
            "tool_input": {"command": "curl https://secret.example?token=abc"},
        }))
        .expect("a report");
        assert_eq!(report.msg.as_deref(), Some("approve Bash"));
    }

    #[test]
    fn the_end_of_a_turn_is_done_but_not_a_subagents() {
        assert_eq!(
            claude_event(&json!({"hook_event_name": "Stop"})).map(|r| r.state),
            Some(ProgramState::Done)
        );
        assert_eq!(
            claude_event(&json!({"hook_event_name": "Stop", "agent_id": "a1"})),
            None
        );
        assert_eq!(
            claude_event(&json!({"hook_event_name": "StopFailure", "error": "rate_limit"}))
                .and_then(|r| r.msg),
            Some("rate_limit".to_owned())
        );
    }

    #[test]
    fn going_back_to_work_and_late_idle_reminders_are_told_apart() {
        assert_eq!(
            claude_event(&json!({"hook_event_name": "PostToolUse"})).map(|r| r.state),
            Some(ProgramState::Working)
        );
        assert_eq!(
            claude_event(
                &json!({"hook_event_name": "Notification", "notification_type": "idle_prompt"})
            ),
            None
        );
        assert_eq!(
            claude_event(&json!({"hook_event_name": "SessionStart"})),
            None
        );
    }

    #[test]
    fn a_finished_turn_brings_what_the_agent_said_last() {
        assert_eq!(
            claude_event(&json!({
                "hook_event_name": "Stop",
                "last_assistant_message": "I checked it. Then I fixed the bug. All 42 tests pass now.",
            }))
            .and_then(|r| r.msg),
            Some("Then I fixed the bug. All 42 tests pass now.".to_owned())
        );
        assert_eq!(
            codex_event(&json!({
                "type": "agent-turn-complete",
                "last-assistant-message": "Updated the README.",
            }))
            .and_then(|r| r.msg),
            Some("Updated the README.".to_owned())
        );
    }

    #[test]
    fn codex_says_only_when_a_turn_is_complete() {
        assert_eq!(
            codex_event(&json!({"type": "agent-turn-complete", "turn-id": "1"})).map(|r| r.app),
            Some("codex")
        );
        assert_eq!(codex_event(&json!({"type": "something-else"})), None);
    }

    #[test]
    #[cfg(windows)]
    fn a_windows_binary_path_is_double_quoted() {
        assert_eq!(
            hook_command_line(Path::new(r"C:\Users\a b\termphin-agent.exe"), "claude"),
            r#""C:\Users\a b\termphin-agent.exe" hook claude"#
        );
        assert!(ours(
            r#""C:\Users\a b\termphin-agent.exe" hook claude"#,
            "claude"
        ));
    }

    #[test]
    #[cfg(unix)]
    fn installing_into_claude_keeps_everything_else_and_twice_is_once() {
        let mut settings = json!({
            "model": "opus",
            "hooks": {
                "Stop": [{"hooks": [{"type": "command", "command": "afplay done.aiff"}]}]
            }
        });
        claude_install(&mut settings, &binary());
        claude_install(&mut settings, &binary());
        assert_eq!(settings["model"], "opus");
        let stop = settings["hooks"]["Stop"].as_array().expect("Stop hooks");
        assert_eq!(stop.len(), 2);
        assert_eq!(stop[0]["hooks"][0]["command"], "afplay done.aiff");
        assert_eq!(
            stop[1]["hooks"][0]["command"],
            "/home/me/.cache/termphin/bin/termphin-agent hook claude"
        );
        assert_eq!(settings["hooks"]["PermissionRequest"][0]["matcher"], "*");
        assert!(claude_installed(&settings));
    }

    #[test]
    fn removing_from_claude_leaves_only_what_was_there() {
        let mut settings = json!({
            "hooks": {
                "Stop": [{"hooks": [{"type": "command", "command": "afplay done.aiff"}]}]
            }
        });
        let before = settings.clone();
        claude_install(&mut settings, &binary());
        claude_remove(&mut settings);
        assert_eq!(settings, before);
        let mut empty = json!({});
        claude_install(&mut empty, &binary());
        claude_remove(&mut empty);
        assert_eq!(empty, json!({}));
        assert!(!claude_installed(&empty));
    }

    #[test]
    #[cfg(unix)]
    fn a_binary_path_with_spaces_is_quoted_for_the_shell() {
        assert_eq!(
            hook_command_line(Path::new("/home/a b/termphin-agent"), "claude"),
            "'/home/a b/termphin-agent' hook claude"
        );
        assert!(ours("'/home/a b/termphin-agent' hook claude", "claude"));
        assert!(!ours("other-tool hook claude", "claude"));
    }

    #[test]
    fn codex_gets_notify_first_and_a_notify_of_its_own_is_left_alone() {
        let config = "model = \"o3\"\n\n[tui]\nnotifications = true\n";
        let CodexInstall::Installed(text) = codex_install(config, &binary()) else {
            panic!("not installed");
        };
        assert!(text.starts_with(
            "notify = ['/home/me/.cache/termphin/bin/termphin-agent', \"hook\", \"codex\"]\n"
        ));
        assert!(text.ends_with(config));
        assert_eq!(codex_remove(&text), config);
        let again = match codex_install(&text, &binary()) {
            CodexInstall::Installed(again) => again,
            CodexInstall::Taken => panic!("its own line counted as taken"),
        };
        assert_eq!(again.matches("termphin-agent").count(), 1);
        assert_eq!(
            codex_install("notify = [\"say\", \"done\"]\n", &binary()),
            CodexInstall::Taken
        );
        assert_eq!(codex_notify_line("[tui]\nnotify = 1\n"), None);
    }
}
