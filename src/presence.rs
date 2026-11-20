//! Live presence: which sessions this server holds, for the user's apps.
//!
//! When the app has written `~/.config/termphin/presence` - the presence
//! backend's URL and a key only the user's devices know - every session
//! process reports the server's sessions when it starts, every few minutes
//! while it runs, and when it ends; `termphin-agent presence`, which the app
//! puts in the user's crontab, reports every five minutes and at boot, so a
//! machine with no session still shows as online. The list is sealed with that key
//! (AES-256-GCM, bound to this server's relay key id), so the backend only
//! routes it. The relay key authenticates the report and travels in the
//! request body, never on a command line. Without either file nothing is
//! sent.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use aes_gcm::Aes256Gcm;
use aes_gcm::aead::rand_core::RngCore;
use aes_gcm::aead::{Aead, KeyInit, OsRng, Payload};

use crate::SessionInfo;
use crate::relay::json_string;

/// How often a running session reports, which keeps the server online in the
/// apps; the backend lets a report lapse after twelve minutes.
const HEARTBEAT: Duration = Duration::from_secs(5 * 60);

/// Longest one report may take, so a session that ends never waits long on
/// the network before exiting.
const TIMEOUT_SECONDS: &str = "5";

/// Where to report and with what: the backend URL, the sealing key, and the
/// relay key with its id.
#[derive(Debug, PartialEq)]
pub(crate) struct Config {
    url: String,
    key: [u8; 32],
    relay_key: String,
    key_id: String,
}

/// The presence configuration, if the app set this server up for it.
fn config() -> Option<Config> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    let path = PathBuf::from(home)
        .join(".config")
        .join("termphin")
        .join("presence");
    let text = std::fs::read_to_string(path).ok()?;
    parse_config(&text, &crate::relay::relay_key()?)
}

/// Reads `<https url> <base64url key>` and pairs it with the relay key. Any
/// other shape, a key that is not 32 bytes, or a URL that is not https is
/// refused.
pub(crate) fn parse_config(text: &str, relay_key: &str) -> Option<Config> {
    let mut parts = text.split_whitespace();
    let url = parts.next()?.trim_end_matches('/').to_owned();
    let key: [u8; 32] = decode_base64url(parts.next()?)?.try_into().ok()?;
    if parts.next().is_some() || !url.starts_with("https://") {
        return None;
    }
    let (key_id, _) = relay_key.split_once('.')?;
    Some(Config {
        url,
        key,
        relay_key: relay_key.to_owned(),
        key_id: key_id.to_owned(),
    })
}

/// Reports the server's sessions. [exclude] leaves out the session that is
/// ending. With [wait] the report is sent here and the caller is held until
/// it goes out or times out - what a session about to exit wants.
///
/// Otherwise it is handed to one worker thread and the caller goes on at
/// once: listing the sessions asks every other session on the server, which
/// can take seconds, and a client attaching or a program changing what it
/// does must not wait for that. Reports asked for while one is being sent
/// are folded into a single one after it, at most one every
/// [`REPORT_SPACING`], so a program flipping between working and quiet does
/// not start a request per flip.
pub(crate) fn report(exclude: Option<&str>, wait: bool) {
    if wait {
        send(exclude, &[], true, TIMEOUT_SECONDS);
        return;
    }
    let mut worker = WORKER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    worker.pending = true;
    if worker.running {
        return;
    }
    worker.running = true;
    std::thread::spawn(|| {
        loop {
            {
                let mut worker = WORKER
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if !worker.pending {
                    worker.running = false;
                    return;
                }
                worker.pending = false;
            }
            send(None, &[], true, TIMEOUT_SECONDS);
            std::thread::sleep(REPORT_SPACING);
        }
    });
}

/// Whether the report worker is running and whether another report was
/// asked for since it last started one.
struct Worker {
    running: bool,
    pending: bool,
}

static WORKER: std::sync::Mutex<Worker> = std::sync::Mutex::new(Worker {
    running: false,
    pending: false,
});

/// The least time between two reports from the worker.
const REPORT_SPACING: Duration = Duration::from_secs(2);

/// Reports as the session called [name] is stopped by a signal, keeping its
/// directory so it is restored when next opened: it is listed as
/// restorable, and the machine goes offline at once when the system is
/// shutting down or no other session is left - a session killed on its own
/// leaves a machine that is still up. Waits two seconds at most: the network
/// may already be gone, and the lease then takes the machine offline.
#[cfg_attr(windows, allow(dead_code))]
pub(crate) fn report_stopping(name: &str, created_at: u64) {
    let others_alive = crate::sessions()
        .unwrap_or_default()
        .iter()
        .any(|session| session.name != name);
    let shutting_down = std::path::Path::new("/run/nologin").exists();
    send(
        Some(name),
        &[(name.to_owned(), created_at)],
        others_alive && !shutting_down,
        "2",
    );
}

/// Lists the sessions and posts the sealed report, waiting for it. The time
/// is taken before the listing, so a report never carries a newer time than
/// the state it describes. Nothing goes out when no random nonce can be had:
/// one report missed beats a session brought down.
fn send(exclude: Option<&str>, stopping: &[(String, u64)], online: bool, timeout: &'static str) {
    let Some(config) = config() else {
        return;
    };
    let now = unix_millis();
    let mut nonce = [0_u8; 12];
    if OsRng.try_fill_bytes(&mut nonce).is_err() {
        return;
    }
    let sessions = crate::sessions()
        .unwrap_or_default()
        .into_iter()
        .filter(|session| Some(session.name.as_str()) != exclude)
        .collect::<Vec<_>>();
    let mut restorable = crate::restorable_sessions();
    for entry in stopping {
        if !restorable.iter().any(|(name, _)| *name == entry.0) {
            restorable.push(entry.clone());
        }
    }
    let body = request_body(&config, &sessions, &restorable, online, now, &nonce);
    post(&format!("{}/v1/agent", config.url), &body, timeout);
}

/// Reports once the session has settled, then every [HEARTBEAT] for as long
/// as the process lives. The first report waits a moment so this session's
/// own control socket answers the listing; later ones are offset by the
/// process id, so many sessions on one server do not all report in the same
/// second.
pub(crate) fn start_heartbeat() {
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(2));
        report(None, true);
        std::thread::sleep(Duration::from_secs(u64::from(std::process::id()) % 60));
        loop {
            std::thread::sleep(HEARTBEAT);
            report(None, true);
        }
    });
}

/// The JSON posted to the backend: the relay key, the sealed session list
/// and whether the machine is online. Every report but the last one of a
/// machine shutting down says online - the machine is up to send it - and
/// the backend lets that lapse when the reports stop.
pub(crate) fn request_body(
    config: &Config,
    sessions: &[SessionInfo],
    restorable: &[(String, u64)],
    online: bool,
    now_millis: u64,
    nonce: &[u8; 12],
) -> String {
    let sealed = seal(
        &config.key,
        plaintext(sessions, restorable, now_millis).as_bytes(),
        &additional_data(&config.key_id),
        nonce,
    );
    format!(
        "{{\"key\":{},\"payload\":{},\"online\":{online}}}",
        json_string(&config.relay_key),
        json_string(&encode_base64(&sealed)),
    )
}

/// The session list as the app reads it once unsealed. `t` lets the app
/// drop a state older than one it already has. Sessions whose process is
/// gone but whose directory is kept - stopped by a reboot - follow, marked
/// restorable: opening one restores its screen and working directory.
pub(crate) fn plaintext(
    sessions: &[SessionInfo],
    restorable: &[(String, u64)],
    now_millis: u64,
) -> String {
    let live = sessions
        .iter()
        .map(|session| {
            format!(
                "{{\"name\":{},\"created\":{},\"clients\":{},\"devices\":[{}]{}}}",
                json_string(&session.name),
                session.created_at,
                session.clients,
                session
                    .devices
                    .iter()
                    .map(|device| json_string(device))
                    .collect::<Vec<_>>()
                    .join(","),
                activity_json(session.activity.as_ref())
            )
        })
        .collect::<Vec<String>>();
    let stopped = restorable
        .iter()
        .filter(|(name, _)| !sessions.iter().any(|session| session.name == *name))
        .map(|(name, created_at)| {
            format!(
                "{{\"name\":{},\"created\":{created_at},\"clients\":0,\"devices\":[],\"restorable\":true}}",
                json_string(name)
            )
        });
    let entries = live
        .into_iter()
        .chain(stopped)
        .collect::<Vec<_>>()
        .join(",");
    format!("{{\"v\":1,\"t\":{now_millis},\"sessions\":[{entries}]}}")
}

fn activity_json(activity: Option<&crate::Activity>) -> String {
    let Some(activity) = activity else {
        return String::new();
    };
    let command = activity
        .command
        .as_deref()
        .map(json_string)
        .unwrap_or_else(|| "null".to_owned());
    format!(
        ",\"activity\":{{\"state\":\"{}\",\"command\":{}}}",
        activity.state.as_str(),
        command
    )
}

/// What the seal is bound to, so the backend cannot pass one server's state
/// off as another's.
pub(crate) fn additional_data(key_id: &str) -> String {
    format!("termphin-presence:v1:{key_id}")
}

/// AES-256-GCM with the nonce in front: the layout the app's `openBytes`
/// reads once `enc:v1:` is put back in front of the base64.
pub(crate) fn seal(key: &[u8; 32], plaintext: &[u8], aad: &str, nonce: &[u8; 12]) -> Vec<u8> {
    let cipher = Aes256Gcm::new(key.into());
    let sealed = cipher
        .encrypt(
            nonce.into(),
            Payload {
                msg: plaintext,
                aad: aad.as_bytes(),
            },
        )
        .expect("AES-GCM sealing a short message cannot fail");
    let mut out = nonce.to_vec();
    out.extend(sealed);
    out
}

fn post(url: &str, body: &str, timeout: &str) {
    let mut curl = Command::new("curl");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        curl.creation_flags(CREATE_NO_WINDOW);
    }
    let child = curl
        .args([
            "-q", "--proto", "=https", "-fsS", "-m", timeout, "-X", "POST",
        ])
        .args(["-H", "Content-Type: application/json"])
        .args(["--data-binary", "@-", url])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Ok(mut child) = child {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(body.as_bytes());
        }
        let _ = child.wait();
    }
}

fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding, which the backend accepts as a payload.
pub(crate) fn encode_base64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let triple = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for index in 0..4 {
            if index <= chunk.len() {
                out.push(BASE64[((triple >> (18 - 6 * index)) & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// URL-safe base64 without padding, for status line fields.
#[cfg_attr(windows, allow(dead_code))]
pub(crate) fn encode_base64url(bytes: &[u8]) -> String {
    encode_base64(bytes)
        .trim_end_matches('=')
        .replace('+', "-")
        .replace('/', "_")
}

/// URL-safe base64, padding optional - how the app writes the key.
pub(crate) fn decode_base64url(text: &str) -> Option<Vec<u8>> {
    let text = text.trim_end_matches('=');
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let mut buffer = 0_u32;
    let mut bits = 0;
    for byte in text.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return None,
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    (bits < 6).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RELAY_KEY: &str = "AbCdEfGhIjKlMnOpQrStUv.s3cr3t-s3cr3t-s3cr3t-s3cr3t-s3cr3t";

    fn key() -> [u8; 32] {
        let mut key = [0_u8; 32];
        for (index, byte) in key.iter_mut().enumerate() {
            *byte = index as u8;
        }
        key
    }

    fn session(name: &str) -> SessionInfo {
        SessionInfo {
            name: name.to_owned(),
            created_at: 1_763_600_000,
            clients: 1,
            devices: vec!["id1;phone;UGl4ZWw".to_owned()],
            activity: None,
        }
    }

    #[test]
    fn config_needs_https_a_32_byte_key_and_a_relay_key() {
        let key = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8";
        let config = parse_config(&format!("https://rt.termphin.dev/ {key}\n"), RELAY_KEY).unwrap();
        assert_eq!(config.url, "https://rt.termphin.dev");
        assert_eq!(config.key, self::key());
        assert_eq!(config.key_id, "AbCdEfGhIjKlMnOpQrStUv");
        assert!(parse_config(&format!("http://rt.termphin.dev {key}"), RELAY_KEY).is_none());
        assert!(parse_config("https://rt.termphin.dev AAEC", RELAY_KEY).is_none());
        assert!(parse_config(&format!("https://rt.termphin.dev {key} extra"), RELAY_KEY).is_none());
        assert!(parse_config(&format!("https://rt.termphin.dev {key}"), "no-dot").is_none());
        assert!(parse_config("", RELAY_KEY).is_none());
    }

    #[test]
    fn plaintext_carries_the_activity() {
        let mut working = session("build");
        working.activity = Some(crate::Activity {
            state: crate::ActivityState::Working,
            command: Some("npm \"run\"".to_owned()),
        });
        let mut idle = session("idle");
        idle.activity = Some(crate::Activity {
            state: crate::ActivityState::Idle,
            command: None,
        });
        let text = plaintext(&[working, idle], &[], 1);
        assert!(
            text.contains(",\"activity\":{\"state\":\"working\",\"command\":\"npm \\\"run\\\"\"}")
        );
        assert!(text.contains(",\"activity\":{\"state\":\"idle\",\"command\":null}"));
    }

    #[test]
    fn stopped_sessions_follow_as_restorable_unless_running() {
        let text = plaintext(
            &[session("live")],
            &[("live".to_owned(), 1), ("rebooted".to_owned(), 5)],
            1,
        );
        assert!(text.ends_with(
            ",{\"name\":\"rebooted\",\"created\":5,\"clients\":0,\"devices\":[],\"restorable\":true}]}"
        ));
        assert_eq!(text.matches("\"name\":\"live\"").count(), 1);
    }

    #[test]
    fn plaintext_escapes_session_names() {
        let text = plaintext(&[session("a\"b"), session("c")], &[], 42);
        assert_eq!(
            text,
            "{\"v\":1,\"t\":42,\"sessions\":[{\"name\":\"a\\\"b\",\"created\":1763600000,\"clients\":1,\"devices\":[\"id1;phone;UGl4ZWw\"]},{\"name\":\"c\",\"created\":1763600000,\"clients\":1,\"devices\":[\"id1;phone;UGl4ZWw\"]}]}"
        );
        assert_eq!(plaintext(&[], &[], 7), "{\"v\":1,\"t\":7,\"sessions\":[]}");
    }

    #[test]
    fn sealed_state_opens_with_the_same_key_and_aad_only() {
        let nonce = [9_u8; 12];
        let sealed = seal(&key(), b"hello", "termphin-presence:v1:abc", &nonce);
        assert_eq!(&sealed[..12], &nonce);
        let cipher = Aes256Gcm::new((&key()).into());
        let open = |aad: &str| {
            cipher.decrypt(
                (&nonce).into(),
                Payload {
                    msg: &sealed[12..],
                    aad: aad.as_bytes(),
                },
            )
        };
        assert_eq!(open("termphin-presence:v1:abc").unwrap(), b"hello");
        assert!(open("termphin-presence:v1:xyz").is_err());
    }

    #[test]
    fn known_vector_matches_the_app() {
        let sealed = seal(
            &key(),
            b"{\"v\":1,\"t\":1,\"sessions\":[]}",
            "termphin-presence:v1:AbCdEfGhIjKlMnOpQrStUv",
            &[7_u8; 12],
        );
        assert_eq!(
            encode_base64(&sealed),
            include_str!("../tests/presence_vector.txt").trim()
        );
    }

    #[test]
    fn request_body_carries_the_key_and_always_says_online() {
        let config = parse_config(
            "https://rt.termphin.dev AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8",
            RELAY_KEY,
        )
        .unwrap();
        let online = request_body(&config, &[session("main")], &[], true, 1, &[1; 12]);
        assert!(online.starts_with(&format!("{{\"key\":\"{RELAY_KEY}\",\"payload\":\"")));
        assert!(online.ends_with("\",\"online\":true}"));
        assert!(!online.contains("main"));
        let empty = request_body(&config, &[], &[], true, 1, &[1; 12]);
        assert!(empty.ends_with("\",\"online\":true}"));
        let going_down = request_body(&config, &[], &[], false, 1, &[1; 12]);
        assert!(going_down.ends_with("\",\"online\":false}"));
    }

    #[test]
    fn base64_round_trips() {
        assert_eq!(encode_base64(b""), "");
        assert_eq!(encode_base64(b"f"), "Zg==");
        assert_eq!(encode_base64(b"fo"), "Zm8=");
        assert_eq!(encode_base64(b"foo"), "Zm9v");
        assert_eq!(encode_base64(&[0xfb, 0xff]), "+/8=");
        assert_eq!(decode_base64url("-_8").unwrap(), vec![0xfb, 0xff]);
        assert_eq!(decode_base64url("Zm9v").unwrap(), b"foo");
        assert_eq!(decode_base64url("Zg==").unwrap(), b"f");
        assert!(decode_base64url("Zm9v!").is_none());
        assert!(decode_base64url("Z").is_none());
    }
}
