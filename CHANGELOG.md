# Changelog

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
