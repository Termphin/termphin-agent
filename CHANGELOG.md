# Changelog

## 0.10.0 - 2026-08-16

- Fixed corrupted scrollback when reattaching to a long-running session. A server-side terminal emulator (vt100) now tracks the real screen state and replays it exactly, instead of dumping raw output bytes - in-place redraws like spinners and status lines no longer scatter text across the screen.
- Scrollback history is replayed as plain text so it flows into the client's own native scrollback, followed by the current screen with colors, cursor position and input modes restored.
- Alternate screen apps (vim, htop, less) now replay their exact current frame on reattach. Removed the one-row resize hack that used to force them to repaint.
- Window title is restored on reattach and survives reboots.
- The reboot-restored marker is re-emitted on every replay, so the client reliably learns the previous shell is gone after a crash or restart.
- Persisted scrollback is now a restorable snapshot, which makes restore-after-crash exact as well.
- Terminal resizes keep the emulator in sync with the PTY.
