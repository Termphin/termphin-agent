# Changelog

## Unreleased

- A program that worked for 20 seconds or more and then stops while it still holds the terminal - an AI agent waiting for an answer, an installer asking to continue - announces it, with no setup and whichever program it is, unless you were typing into it.
- Notifications name the command, not its wrapper: `dnf update` for `sudo dnf update -y`, `deploy.sh prod` for a script, `gemini` for a tool run by node - never anything from the first option on, where passwords and tokens go.
- A notification quotes the last line the command wrote (`Complete!`), or the question a stopped program is asking.
- A long command that hands the terminal straight to the next one, as in `dnf update -y; reboot`, announces its end too.
- macOS sessions name the command as well, from its arguments.
- Windows sessions tell the app which version runs them, so the app no longer reports every Windows session as started with an older helper.
- Windows sessions announce long commands and programs that went quiet, and pass a program's own notifications to the relay while no phone is attached, like the other systems.

## 0.16.0 - 2026-11-14

- A running session moves to a newly installed version on its own, the next time nobody is attached: the shell and everything in it keep running, and the scrollback and resume position carry over.
- A session tells the app which version runs it, so the app can say what an older session is missing.

## 0.15.0 - 2026-11-14

- `termphin notify [title] [body]` shows a notification in Termphin, for the end of a long command or as a Claude Code hook.
- A notification a program asks for while no phone is attached goes out through Termphin's relay, once the app has set the server up for push. It names the session, so tapping it opens that session.
- A command that runs for 30 seconds or more announces its end, in any shell and with no setup, unless you were typing into it.

## 0.14.0 - 2026-11-13

- Scrollback kept and replayed is 10 000 rows, up from 2000.
- The scrollback snapshot is only written when something changed.

## 0.13.0 - 2026-10-08

- Fixed history losing its colours on reattach.

## 0.12.1 - 2026-10-05

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
