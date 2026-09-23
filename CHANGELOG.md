# Changelog

## 0.12.0 - 2026-09-22

- `attach --resume <epoch>:<offset>` continues a session from a position in its output instead of replaying it, when the master still holds everything since then (the last 1 MiB). A client that reconnects after a short drop gets only what it missed, so its own terminal and scrollback carry on unchanged. Otherwise it falls back to the replay, as does a master started by an older build.
- The end of a replay or resume is followed by a position marker, `OSC 5383;termphin-offset;<epoch>:<offset>`, telling the client where live output starts. The epoch is new for every master, so a position from one that crashed or was restored after a reboot is never read against the next one's numbering.
- Unix only. On Windows ConPTY re-renders the output, so the bytes a client receives are not the ones counted, and `--resume` is ignored.
- A command line for people as well as the app. `install.sh` and `install.ps1` put the agent on PATH as `termphin`, installing it first if the app has not. `termphin attach` without a name picks the only session or offers a choice, `termphin new` starts one, `termphin ls` lists them. `Ctrl-\` then `d` detaches, and resets the terminal's modes on the way out.
- With several clients attached, the session takes the size of whichever last attached or typed, and goes back to the previous one's when it leaves, rather than each resize fighting the last.
- A session's shell has `TERMPHIN_SESSION` set, and `termphin attach` refuses to run inside one.

## 0.11.0 - 2026-08-28

- Fixed restores coming back with the wrong scrollback. A second writer on the client overwrote the master's snapshot with a stale raw-byte copy, and whichever wrote last won.
- Fixed an attach that could hang forever while starting a session, and gave the wait for the new master a timeout.
- A control socket that cannot be reached no longer counts as a missing master. It used to delete a live session's directory and start a second master on its name.
- `list` no longer stops at the first session whose master fails mid-exchange.
- `kill` now delivers the exit frame and the shell's last output instead of dropping both.
- Windows is built and linted on every push, not only from a release tag.

## 0.10.1 - 2026-08-19

- Added macOS support, x86_64 and aarch64. The aarch64 binary is ad-hoc codesigned in CI, since Apple Silicon refuses to run an unsigned one.

## 0.10.0 - 2026-08-16

- Fixed corrupted scrollback when reattaching to a long-running session. A server-side terminal emulator (vt100) now tracks the real screen state and replays it exactly, instead of dumping raw output bytes - in-place redraws like spinners and status lines no longer scatter text across the screen.
- Scrollback history is replayed as plain text so it flows into the client's own native scrollback, followed by the current screen with colors, cursor position and input modes restored.
- Alternate screen apps (vim, htop, less) now replay their exact current frame on reattach. Removed the one-row resize hack that used to force them to repaint.
- Window title is restored on reattach and survives reboots.
- The reboot-restored marker is re-emitted on every replay, so the client reliably learns the previous shell is gone after a crash or restart.
- Persisted scrollback is now a restorable snapshot, which makes restore-after-crash exact as well.
- Terminal resizes keep the emulator in sync with the PTY.
