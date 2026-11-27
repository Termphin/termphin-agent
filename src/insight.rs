//! What a finished turn of an AI agent can say beyond "done", read without
//! changing anything on the machine: the agent's last reply, from the
//! transcript it keeps itself (Claude Code, Codex), and the commands it
//! started that are still running, from the process tree.
//!
//! Every step gives up quietly - a file that is not there, a format that
//! changed, a process that ended mid-read - and the notification goes out
//! as it would have without it.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::Value;

/// How much of the end of a transcript is read for the last reply.
const TAIL_BYTES: u64 = 512 * 1024;

/// How recently a transcript must have been written to be the one the turn
/// that just ended wrote.
const FRESH: Duration = Duration::from_secs(120);

/// The most commands "still running" names.
const MAX_STILL_RUNNING: usize = 3;

/// Programs that only wrap the command an agent runs: what they run is
/// named instead.
const WRAPPERS: [&str; 10] = [
    "bash",
    "sh",
    "zsh",
    "dash",
    "fish",
    "ksh",
    "pwsh",
    "powershell",
    "cmd",
    "env",
];

/// The agent's last reply as it wrote it in its own transcript, for the
/// agent [app] running in [cwd] when that is known. `None` for an agent
/// that keeps none this reads, or whenever anything does not add up.
pub(crate) fn last_reply(app: &str, cwd: Option<&Path>) -> Option<String> {
    let home = home()?;
    match app {
        "claude-code" | "claude" | "claudecode" => claude_reply(&home, cwd),
        "codex" | "codex-cli" => codex_reply(&home, cwd),
        _ => None,
    }
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// The folder Claude Code keeps the transcripts of sessions started in
/// [cwd] in: the path with every character but letters and digits a dash.
fn claude_project_dir(root: &Path, cwd: &Path) -> PathBuf {
    let escaped: String = cwd
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    root.join(escaped)
}

fn claude_reply(home: &Path, cwd: Option<&Path>) -> Option<String> {
    let root = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".claude"))
        .join("projects");
    let candidates = match cwd {
        Some(cwd) => fresh_files(&claude_project_dir(&root, cwd), 1),
        None => fresh_files(&root, 2),
    };
    let path = only_or_newest(candidates, cwd.is_some())?;
    let tail = read_tail(&path)?;
    tail.lines().rev().find_map(claude_text)
}

/// The text of an assistant entry in a Claude Code transcript line.
fn claude_text(line: &str) -> Option<String> {
    let entry: Value = serde_json::from_str(line).ok()?;
    if entry.get("type")?.as_str()? != "assistant" {
        return None;
    }
    let content = entry.get("message")?.get("content")?.as_array()?;
    let text = content
        .iter()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    (!text.trim().is_empty()).then_some(text)
}

fn codex_reply(home: &Path, cwd: Option<&Path>) -> Option<String> {
    let root = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".codex"))
        .join("sessions");
    let mut candidates = fresh_files(&root, 4);
    if let Some(cwd) = cwd {
        candidates.retain(|(path, _)| codex_cwd(path).as_deref() == Some(cwd));
    }
    let path = only_or_newest(candidates, cwd.is_some())?;
    let tail = read_tail(&path)?;
    tail.lines().rev().find_map(codex_text)
}

/// The folder a Codex session ran in, from the `session_meta` line it
/// opens with.
fn codex_cwd(path: &Path) -> Option<PathBuf> {
    let mut first = String::new();
    fs::File::open(path)
        .ok()?
        .take(64 * 1024)
        .read_to_string(&mut first)
        .ok()?;
    let entry: Value = serde_json::from_str(first.lines().next()?).ok()?;
    let payload = entry.get("payload")?;
    Some(PathBuf::from(payload.get("cwd")?.as_str()?))
}

/// The text of a reply in a Codex session line, in either of the shapes
/// Codex has written them in.
fn codex_text(line: &str) -> Option<String> {
    let entry: Value = serde_json::from_str(line).ok()?;
    let payload = entry.get("payload")?;
    if payload.get("type").and_then(Value::as_str) == Some("agent_message") {
        return payload
            .get("message")
            .and_then(Value::as_str)
            .filter(|text| !text.trim().is_empty())
            .map(str::to_owned);
    }
    if payload.get("type").and_then(Value::as_str) != Some("message")
        || payload.get("role").and_then(Value::as_str) != Some("assistant")
    {
        return None;
    }
    let text = payload
        .get("content")?
        .as_array()?
        .iter()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    (!text.trim().is_empty()).then_some(text)
}

/// The `.jsonl` files under [dir], down to [depth] levels, written in the
/// last [`FRESH`], with when.
fn fresh_files(dir: &Path, depth: usize) -> Vec<(PathBuf, SystemTime)> {
    let mut found = Vec::new();
    let now = SystemTime::now();
    let mut stack = vec![(dir.to_path_buf(), depth)];
    while let Some((current, left)) = stack.pop() {
        let Ok(entries) = fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                if left > 1 {
                    stack.push((path, left - 1));
                }
                continue;
            }
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            if let Ok(modified) = meta.modified()
                && now.duration_since(modified).is_ok_and(|age| age <= FRESH)
            {
                found.push((path, modified));
            }
        }
    }
    found
}

/// The newest of [candidates] when they are tied to the agent's folder;
/// otherwise only a single one, since with several fresh transcripts from
/// several folders nothing says which is this agent's.
fn only_or_newest(mut candidates: Vec<(PathBuf, SystemTime)>, tied: bool) -> Option<PathBuf> {
    if !tied && candidates.len() != 1 {
        return None;
    }
    candidates.sort_by_key(|(_, modified)| *modified);
    candidates.pop().map(|(path, _)| path)
}

fn read_tail(path: &Path) -> Option<String> {
    let mut file = fs::File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    if length > TAIL_BYTES {
        file.seek(SeekFrom::Start(length - TAIL_BYTES)).ok()?;
    }
    let mut bytes = Vec::new();
    file.take(TAIL_BYTES).read_to_end(&mut bytes).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// Every process under [root] in [parents] (child to parent).
pub(crate) fn descendants(parents: &HashMap<u32, u32>, root: u32) -> HashSet<u32> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (&pid, &parent) in parents {
        children.entry(parent).or_default().push(pid);
    }
    let mut found = HashSet::new();
    let mut stack = vec![root];
    while let Some(pid) = stack.pop() {
        for &child in children.get(&pid).map(Vec::as_slice).unwrap_or(&[]) {
            if child != root && found.insert(child) {
                stack.push(child);
            }
        }
    }
    found
}

/// The commands under [root] that are not in [baseline] - what the agent
/// had running at rest - and that it ran through a shell, as agents run
/// commands, while MCP servers and its own helpers are started directly and
/// some only after the agent is at rest. Each is named as notifications
/// name commands ([label]), the wrapping shell giving way to what it runs;
/// at most [`MAX_STILL_RUNNING`], and how many more.
pub(crate) fn still_running(
    parents: &HashMap<u32, u32>,
    root: u32,
    baseline: &HashSet<u32>,
    label: impl Fn(u32) -> Option<String>,
    program: impl Fn(u32) -> Option<String>,
) -> (Vec<String>, usize) {
    let under = descendants(parents, root);
    let fresh: HashSet<u32> = under.difference(baseline).copied().collect();
    let mut tops: Vec<u32> = fresh
        .iter()
        .copied()
        .filter(|pid| {
            parents
                .get(pid)
                .is_none_or(|parent| !fresh.contains(parent))
        })
        .filter(|pid| program(*pid).is_some_and(|name| is_wrapper(&name)))
        .collect();
    tops.sort_unstable();
    let mut names: Vec<String> = Vec::new();
    for top in tops {
        let Some(name) = innermost(parents, top, &label, &program) else {
            continue;
        };
        if !names.contains(&name) {
            names.push(name);
        }
    }
    let more = names.len().saturating_sub(MAX_STILL_RUNNING);
    names.truncate(MAX_STILL_RUNNING);
    (names, more)
}

/// [pid]'s name, or while its [program] is only a shell or `env` with
/// children, the name of what it runs.
fn innermost(
    parents: &HashMap<u32, u32>,
    pid: u32,
    label: &impl Fn(u32) -> Option<String>,
    program: &impl Fn(u32) -> Option<String>,
) -> Option<String> {
    let mut current = pid;
    for _ in 0..8 {
        let name = label(current)?;
        if !program(current).is_some_and(|program| is_wrapper(&program)) {
            return Some(name);
        }
        let child = parents
            .iter()
            .filter(|(_, parent)| **parent == current)
            .map(|(child, _)| *child)
            .max();
        match child {
            Some(child) => current = child,
            None => return Some(name),
        }
    }
    None
}

/// Whether [name] - a program file, or a command whose first word is one -
/// is a shell or `env`, which only runs another command.
fn is_wrapper(name: &str) -> bool {
    let program = name.split_whitespace().next().unwrap_or("");
    let program = program
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(program)
        .to_ascii_lowercase();
    WRAPPERS.contains(&program.trim_end_matches(".exe"))
}

/// The line a finished turn adds for the commands still running, `Still
/// running: npm run dev, +2 more`, or `None` when there are none.
pub(crate) fn still_running_line(names: &[String], more: usize) -> Option<String> {
    if names.is_empty() {
        return None;
    }
    let mut line = format!("Still running: {}", names.join(", "));
    if more > 0 {
        line.push_str(&format!(", +{more} more"));
    }
    Some(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(pairs: &[(u32, u32)]) -> HashMap<u32, u32> {
        pairs.iter().copied().collect()
    }

    #[test]
    fn a_command_started_during_the_turn_is_still_running_and_mcp_is_not() {
        let parents = tree(&[
            (10, 1),
            (11, 10),
            (12, 10),
            (13, 12),
            (14, 10),
            (15, 14),
            (16, 10),
            (20, 1),
        ]);
        let baseline: HashSet<u32> = [11].into_iter().collect();
        let names = |pid: u32| match pid {
            11 => Some("node mcp-server".to_owned()),
            16 => Some("uv run late-mcp".to_owned()),
            12 => Some("bash".to_owned()),
            13 => Some("npm run dev".to_owned()),
            14 => Some("sh".to_owned()),
            15 => Some("cargo build".to_owned()),
            _ => None,
        };
        let (found, more) = still_running(&parents, 10, &baseline, names, |pid| {
            names(pid).map(|name| name.split_whitespace().next().unwrap_or("").to_owned())
        });
        assert_eq!(
            found,
            vec!["npm run dev".to_owned(), "cargo build".to_owned()]
        );
        assert_eq!(more, 0);
        assert_eq!(
            still_running_line(&found, 2).as_deref(),
            Some("Still running: npm run dev, cargo build, +2 more")
        );
    }

    #[test]
    fn a_shell_the_label_already_unwraps_still_counts_as_one() {
        let parents = tree(&[(10, 1), (12, 10), (13, 12)]);
        let (found, _) = still_running(
            &parents,
            10,
            &HashSet::new(),
            |pid| {
                Some(if pid == 12 {
                    "ping".to_owned()
                } else {
                    "PING.EXE -n 60".to_owned()
                })
            },
            |pid| {
                Some(if pid == 12 {
                    "cmd.exe".to_owned()
                } else {
                    "PING.EXE".to_owned()
                })
            },
        );
        assert_eq!(found, vec!["PING.EXE -n 60".to_owned()]);
    }

    #[test]
    fn nothing_new_means_nothing_still_running() {
        let parents = tree(&[(10, 1), (11, 10)]);
        let baseline: HashSet<u32> = [11].into_iter().collect();
        let (found, _) = still_running(
            &parents,
            10,
            &baseline,
            |_| Some("x".to_owned()),
            |_| Some("x".to_owned()),
        );
        assert!(found.is_empty());
        assert_eq!(still_running_line(&found, 0), None);
    }

    #[test]
    fn many_commands_are_capped() {
        let pairs: Vec<(u32, u32)> = (100..110)
            .map(|pid| (pid, 10))
            .chain((200..210).map(|pid| (pid, pid - 100)))
            .chain([(10, 1)])
            .collect();
        let parents = tree(&pairs);
        let (found, more) = still_running(
            &parents,
            10,
            &HashSet::new(),
            |pid| Some(format!("job{pid}")),
            |pid| {
                Some(if pid < 200 {
                    "bash".to_owned()
                } else {
                    "job".to_owned()
                })
            },
        );
        assert_eq!(found.len(), MAX_STILL_RUNNING);
        assert_eq!(more, 7);
    }

    #[test]
    fn a_claude_transcript_gives_its_last_reply() {
        let lines = [
            r#"{"type":"user","message":{"content":"hi"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"First answer."}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash"}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Tests pass. Ready."}]}}"#,
            r#"{"type":"system","subtype":"stop_hook_summary"}"#,
            "not json",
        ];
        assert_eq!(
            lines
                .iter()
                .rev()
                .find_map(|line| claude_text(line))
                .as_deref(),
            Some("Tests pass. Ready.")
        );
    }

    #[test]
    fn a_codex_session_gives_its_last_reply_in_either_shape() {
        assert_eq!(
            codex_text(
                r#"{"type":"event_msg","payload":{"type":"agent_message","message":"Updated it."}}"#
            )
            .as_deref(),
            Some("Updated it.")
        );
        assert_eq!(
            codex_text(
                r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Done here."}]}}"#
            )
            .as_deref(),
            Some("Done here.")
        );
        assert_eq!(
            codex_text(
                r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"x"}]}}"#
            ),
            None
        );
    }

    #[test]
    fn claude_keeps_a_folder_per_working_directory() {
        assert_eq!(
            claude_project_dir(Path::new("/r"), Path::new("/home/me/My Project.v2")),
            PathBuf::from("/r/-home-me-My-Project-v2")
        );
    }

    #[test]
    fn transcripts_are_found_fresh_and_picked_only_when_it_is_clear_which() {
        let base = std::env::temp_dir().join(format!("termphin-insight-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let project = claude_project_dir(&base.join("projects"), Path::new("/work/app"));
        fs::create_dir_all(&project).unwrap();
        fs::write(
            project.join("s1.jsonl"),
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"All good.\"}]}}\n",
        )
        .unwrap();
        fs::write(project.join("notes.txt"), "x").unwrap();
        let found = fresh_files(&project, 1);
        assert_eq!(found.len(), 1);
        let path = only_or_newest(found, true).unwrap();
        assert_eq!(
            read_tail(&path)
                .unwrap()
                .lines()
                .rev()
                .find_map(claude_text)
                .as_deref(),
            Some("All good.")
        );
        let other = claude_project_dir(&base.join("projects"), Path::new("/work/other"));
        fs::create_dir_all(&other).unwrap();
        fs::write(other.join("s2.jsonl"), "{}\n").unwrap();
        assert_eq!(
            only_or_newest(fresh_files(&base.join("projects"), 2), false),
            None
        );
        let _ = fs::remove_dir_all(&base);
    }
}
