# Changelog

## Unreleased

- Fixed Ctrl+C not stopping a running command in a Windows session.

## 0.12.0 - 2026-09-22

- `attach --resume <epoch>:<offset>` sends only the output missed since that position, falling back to a replay. Unix only.
- A replay or resume ends with `OSC 5383;termphin-offset;<epoch>:<offset>`, where live output starts.
- `termphin` command line: `attach`, `new`, `ls`, installed by `install.sh` and `install.ps1`. `Ctrl-\` then `d` detaches.
- With several clients, the session takes the size of whichever attached or typed last.
- The shell has `TERMPHIN_SESSION` set, and `attach` refuses to run inside a session.
- Fixed a Windows session staying listed after its shell exited.
- Fixed Windows clients shrinking each other on resize.

## 0.11.0 - 2026-08-28

- Fixed restores coming back with stale scrollback.
- Fixed an attach that could hang forever while starting a session.
- An unreachable control socket no longer counts as a dead session.
- `list` no longer stops at the first failing session.
- `kill` delivers the exit frame and the shell's last output.
- Windows is built and linted on every push.

## 0.10.1 - 2026-08-19

- Added macOS support, x86_64 and aarch64.

## 0.10.0 - 2026-08-16

- Reattach replays the real screen state instead of raw output, so redraws no longer scatter text.
- Scrollback is replayed as plain text into the client's own scrollback.
- Full-screen apps (vim, htop, less) come back exactly as they were.
- The window title is restored on reattach and survives reboots.
- The reboot-restored marker is sent on every replay.
- Saved scrollback restores exactly after a crash.
- Resizes keep the server-side screen in sync.
