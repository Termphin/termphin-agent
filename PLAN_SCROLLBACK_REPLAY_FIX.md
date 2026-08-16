# Plan: fix corrupted scrollback replay on reattach

## Bug, confirmed

Symptom: reattaching to a long-lived session (`termphin-agent attach --replay <name>`)
sometimes paints garbage - scattered characters, overlapping text - instead of the
session's actual content. Reported as happening "very often" on a session that had
been running Claude Code for a while (heavy use of the CLI's own animated spinner).

Root cause, confirmed by reading `~/.cache/termphin/sessions/<name>/scrollback` for a
live repro (`termphin_Agent` session on this machine) and reading
`termphin-agent/src/main.rs`:

- `History::snapshot()` (`src/main.rs`) replays the **entire raw byte ring buffer**
  (up to `HISTORY_SIZE` = 256 KiB) verbatim to a client on attach.
- `ModeTracker` (same file) only tracks alternate-screen state, the window title, and
  a handful of input modes - **not cursor position or screen contents**.
- Any program that redraws in place using *relative* cursor moves (`CUU`/`CUD`/`CUF`/
  `CUB` - status lines, spinners, progress bars; Claude Code's own CLI does this
  constantly) accumulates thousands of relative moves in the ring over a long
  session. Replaying that raw stream into a **fresh** client terminal - which does not
  start at the same cursor position/state the original stream assumed - scatters
  output across the screen. The longer the session lives, the worse it gets, which
  matches "bugs out very often."
- This is used from both `src/unix.rs` (`MasterState::add_client`,
  `MasterState::flush_restore_state`) and the equivalent in `src/windows.rs` - `History`
  is platform-neutral (`src/main.rs`), so one fix in that file covers both platforms.

## Chosen fix

Replace raw-byte replay with a real terminal emulator kept server-side, and replay
its **reconstructed current state** (absolute-positioned, corruption-proof) instead of
the raw byte history.

Crate: [`vt100`](https://docs.rs/vt100) `0.16.2`. Small, pure Rust, no unsafe in its
public surface, pulls in `vte` (what Alacritty uses to parse), `arrayvec`, `itoa`,
`memchr`, `unicode-width`. Already added:

```
cargo add vt100
```

This already ran in this checkout - `Cargo.toml` and `Cargo.lock` are updated and
committed-worthy as-is. **No source files have been edited yet** - `src/main.rs`,
`src/unix.rs`, `src/windows.rs` are still on the old raw-byte-ring design. (Those three
files currently show as modified in `git status` from a *different*, unrelated,
already-in-progress change - abandoned-session sweeping / reboot-restore wording. Do
not discard that diff; this task's changes land on top of it.)

## vt100 API notes (from reading the vendored source, not guessed)

Source: `~/.cargo/registry/src/index.crates.io-*/vt100-0.16.2/src/`

- `vt100::Parser::new(rows: u16, cols: u16, scrollback_len: usize) -> Parser` -
  construct once per session master, sized like the PTY.
- `parser.process(&[u8])` - feed raw output bytes. This is a real VT100/xterm emulator:
  cursor position, SGR attributes, alternate screen, all correctly tracked regardless
  of how many relative moves are in the stream. Safe to feed the *entire* historical
  byte stream through this - the corruption problem is specific to replaying raw bytes
  **at a fresh client terminal**, not to parsing them into an emulator.
- `parser.screen() -> &Screen` / `parser.screen_mut() -> &mut Screen`.
- `Screen::set_size(rows, cols)` - call on every PTY resize (hook into
  `MasterState::resize` in both `unix.rs` and `windows.rs`).
- `Screen::state_formatted() -> Vec<u8>` - **the key method**. Returns escape codes
  that reproduce the *entire current visible screen* (contents + colors/attrs + cursor
  position + input modes: keypad/app-cursor/bracketed-paste/mouse) from a blank
  terminal, using absolute positioning throughout. This is what replaces the old
  `History::snapshot()` raw-byte dump for "what does the screen look like right now."
  Safe to send to any terminal starting from a cleared state, live or another `vt100`
  parser.
- `Screen::alternate_screen() -> bool` - whether the alternate screen is currently
  active. `state_formatted()` does **not** itself emit `\x1b[?1049h`/`l` (that is a
  mode switch, not screen content) - the caller must prepend `\x1b[?1049h` when this is
  true, mirroring what the *old* `ModeTracker.alternate` field was for, but now always
  correct (driven by real state, not an eviction-offset heuristic). No more need for
  `MasterState::request_redraw()`'s "shrink the PTY by one row to force a SIGWINCH
  repaint" hack for alt-screen apps - `state_formatted()` reproduces alt-screen content
  directly. That hack (and the SIGWINCH-based full-screen-app-repaint trick generally)
  can be removed once this lands; verify nothing else depends on it first.
- `Screen::set_scrollback(rows: usize)` / `Screen::scrollback() -> usize` - scrolls the
  *view* used by `rows()`/`cell()`/etc. `set_scrollback(usize::MAX)` then reading back
  `scrollback()` returns the actual clamped scrollback depth (documented: "value given
  will be clamped to the actual size of the scrollback") - use this to discover how
  much history exists without a separate row-count API.
- `Screen::rows(start_col: u16, width: u16) -> impl Iterator<Item = String>` - **plain
  text**, no formatting, one entry per currently-visible row (respects the current
  `set_scrollback` offset). `.next()` is lazy - taking only the first item does not
  materialize the rest, so peeling one historical row at a time by looping
  `set_scrollback(n)` + `rows(0, cols).next()` for `n` from `max` down to `1` is cheap
  (no need to worry about the O(rows) cost of the iterator itself).

### Confirmed API constraint: no scrollback while alternate screen is active

Read `Screen::grid()`/`grid_mut()` (`screen.rs`): they switch between `self.grid`
(primary) and `self.alternate_grid` based on the alternate-screen mode bit.
`Screen::new` constructs `alternate_grid` with `scrollback_len` hardcoded to `0`
(`crate::grid::Grid::new(size, 0)`), and `enter_alternate_grid()` explicitly zeroes the
*primary* grid's scrollback position before switching. Net effect: **while
`alternate_screen()` is true, `scrollback()`/`set_scrollback()`/`rows()` all operate on
the alt grid, which has zero scrollback capacity.** There is no public API to reach the
primary grid's history while alt-screen is active.

Decision: only attempt to prepend primary-screen scrollback text when
`!screen.alternate_screen()`. If the session is currently inside a full-screen app
(vim, htop, top, less, `claude` in some modes, etc.) at the moment of reattach, the
client gets that app's current screen via `state_formatted()` (correct) without the
shell history from before entering it (same as today - you can't see that either
without leaving the app). Not a regression; the primary scrollback becomes available
again once the alt-screen app exits and its `1049l` reaches this same code path.

## New `History` design (`src/main.rs`)

Replace the whole `bytes: VecDeque<u8>` + `ModeTracker` machinery with:

```rust
/// Rows of scrollback the emulator keeps beyond the visible screen. Sized
/// against the client's own ~2000-row transcript - no point keeping more
/// server-side than the client will ever show.
pub(crate) const SCROLLBACK_ROWS: usize = 2000;

pub(crate) struct History {
    parser: vt100::Parser,
}

impl History {
    pub(crate) fn new(rows: u16, cols: u16) -> Self {
        Self { parser: vt100::Parser::new(rows.max(1), cols.max(1), SCROLLBACK_ROWS) }
    }

    pub(crate) fn push(&mut self, data: &[u8]) {
        self.parser.process(data);
    }

    pub(crate) fn resize(&mut self, rows: u16, cols: u16) {
        self.parser.screen_mut().set_size(rows.max(1), cols.max(1));
    }

    pub(crate) fn alternate_screen_active(&self) -> bool {
        self.parser.screen().alternate_screen()
    }

    /// Escape sequences that reconstruct the current session state from a
    /// blank terminal: historical scrollback as plain text (so it flows into
    /// the client's own native scrollback), then the current screen exactly
    /// (colors, cursor, modes). Safe to send to any fresh terminal - a
    /// reattaching client, or another `vt100::Parser` being seeded after a
    /// restart (see `seed_restored`) - regardless of what relative-cursor
    /// gymnastics produced the original output.
    pub(crate) fn snapshot(&mut self) -> Vec<u8> {
        let mut output = Vec::new();
        output.extend(b"\x1b[H\x1b[2J\x1b[3J");
        output.extend(self.scrollback_lines());
        let screen = self.parser.screen();
        if screen.alternate_screen() {
            output.extend(b"\x1b[?1049h");
        }
        output.extend(screen.state_formatted());
        output
    }

    fn scrollback_lines(&mut self) -> Vec<u8> {
        let screen = self.parser.screen_mut();
        if screen.alternate_screen() {
            return Vec::new();
        }
        let (_, cols) = screen.size();
        screen.set_scrollback(usize::MAX);
        let max = screen.scrollback();
        let mut output = Vec::new();
        for n in (1..=max).rev() {
            screen.set_scrollback(n);
            if let Some(line) = screen.rows(0, cols).next() {
                output.extend(line.trim_end().as_bytes());
                output.extend(b"\r\n");
            }
        }
        screen.set_scrollback(0);
        output
    }

    pub(crate) fn seed_restored(&mut self, scrollback: &[u8], after_reboot: bool) {
        if !scrollback.is_empty() {
            self.push(scrollback);
        }
        self.push(REBOOT_RESTORED_MARKER);
        self.push(if after_reboot {
            b"\r\n\x1b[33mrestored after a server restart - new shell, same directory\x1b[0m\r\n\r\n"
                .as_slice()
        } else {
            b"\r\n\x1b[33mthe previous shell is gone - new shell, same directory\x1b[0m\r\n\r\n"
                .as_slice()
        });
    }
}
```

Notes / things to get right, not just translate 1:1:

- `snapshot()` needs `&mut self` now (scrollback peeling mutates view state), whereas
  the old one was `&self`. Check every call site compiles with a `&mut` borrow -
  `MasterState::add_client` and `MasterState::flush_restore_state` both already hold
  `history.lock()` as a mutable-capable `MutexGuard`, so this should be a non-issue,
  but confirm.
- `REBOOT_RESTORED_MARKER` bytes still go through `push()` (i.e. through
  `parser.process()`), same as today - it's just another OSC sequence, vt100 will
  parse it like any other and it'll show up in `state_formatted()`'s output stream
  faithfully (it's inert to a real terminal - purely a marker OSC the app's client
  code matches on by string). Verify the client-side matching in
  `termphin/lib/services/ssh_session_manager.dart` (the `5380;termphin-replay-end` /
  reboot-restored marker handling) still receives it intact through the new
  `state_formatted()`-based path - it should, since these are just bytes flowing
  through same as any other terminal content, but this is exactly the kind of thing
  that's cheap to verify and expensive to get wrong silently.
- Disk persistence (`MasterState::flush_restore_state` in `unix.rs`,
  equivalent in `windows.rs`) currently writes `history.snapshot()` - unchanged, still
  correct: the new `snapshot()` output is self-contained/absolute, so feeding it back
  through `seed_restored()` → `push()` → `parser.process()` on a freshly-constructed
  `Parser` after a crash/reboot reconstructs the exact prior state. No corruption risk
  here either, because feeding escape sequences *into* an emulator (rather than at a
  live client terminal) is always safe - that's the whole premise of the fix.
- Drop the `ModeTracker` struct, its tests, `ALTERNATE_SCREEN_MODES`,
  `DEFAULT_ON_MODES`, `REPLAYABLE_MODES`, `REPLAY_CHUNK_SIZE`-adjacent logic stays (that
  chunking is about frame size, unrelated - keep it, `add_client` still chunks
  `snapshot()` output the same way).
- Existing `History` tests in `src/main.rs` (`history_is_bounded_and_ordered`,
  `input_modes_are_replayed_after_history`, `alternate_screen_is_never_re_entered_...`,
  `alternate_screen_is_restored_once_its_sequence_is_evicted`,
  `title_survives_eviction_...`, `title_terminated_with_st_is_recognised`,
  `later_title_replaces_earlier_one`, `destructive_and_unknown_modes_are_not_replayed`,
  `replay_is_split_into_sendable_frames`) are all written against the old
  ring/ModeTracker behavior (eviction, byte-offset tracking) and need to be **replaced**,
  not just left broken. New tests should assert the property that actually matters:

  - A stream with heavy *relative* cursor movement (simulate a spinner: repeat
    `\x1b[2C\x1b[3A...\x1b[6A<char>\x1b[39m\r` a few hundred times, similar to what was
    found in the real repro file) followed by `push`, then `snapshot()`, reconstructs
    the *correct final visible state* when fed into a **second, independent**
    `vt100::Parser` - i.e. round-trip: `push` bytes into parser A, take `A.snapshot()`,
    feed that into fresh parser B via `B.process()`, assert `A.parser.screen().cell(r,c)`
    == `B.screen().cell(r,c)` for all visible cells. This is the regression test for
    the actual bug.
  - Alternate screen: entering alt screen, drawing, snapshot - assert output does not
    re-run destructive setup, and reconstructing via a second parser lands in
    `alternate_screen() == true` with matching content.
  - Scrollback text: push more lines than fit on screen, assert `snapshot()`'s
    plain-text scrollback portion contains the earlier lines in order, and that
    entering alt-screen makes `scrollback_lines()` return empty.
  - `seed_restored` still marks the session (existing two tests
    `seed_restored_carries_scrollback_and_marks_it` /
    `seed_restored_still_marks_a_session_whose_scrollback_was_lost`) should keep
    passing conceptually - port them to the new struct.

## Call-site changes needed

`src/unix.rs`:
- `let mut history = History::default();` (in `master_process`) → `History::new(size.ws_row, size.ws_col)` (the `size: libc::winsize` used to open the PTY is right there).
- `MasterState::resize()` - after `self.set_window_size(size)?` succeeds and is applied, also call into `History` to resize the emulator: needs a way to reach `history` from `resize()` (it's a sibling field on `MasterState`) - add `self.history.lock().expect(...).resize(size.ws_row, size.ws_col)`.
- `MasterState::request_redraw()` - re-evaluate whether this is still needed at all now that alt-screen apps get a correct `state_formatted()` replay on attach. It exists today specifically because "the alternate buffer has no scrollback to replay" (old comment) - with vt100 that's no longer strictly true (the current alt-screen content *does* replay correctly now). Likely safe to delete this method and its call site in `client_loop`'s `FRAME_ATTACH` handling (`if !resized { state.request_redraw(); }`), but double-check: this was *also* possibly compensating for other things (e.g. an app that doesn't repaint until it sees its own SIGWINCH for unrelated reasons like reflowing text) - read the git blame / original commit message for `request_redraw` before deleting, don't remove blind.

`src/windows.rs`: mirror both of the above - find the equivalent of `master_process`'s `History::default()` construction and the resize path (`grep -n "History::default\|history.lock" src/windows.rs` to locate).

`Cargo.toml`: already done (`vt100 = "0.16.2"` or whatever `cargo add` pinned - check the exact line and whether it should be a caret or exact pin; this project doesn't currently pin dependencies tightly (`libc = "0.2"`), so match that convention - `cargo add` already did this correctly, just verify).

## Verification checklist

1. `cargo build` and `cargo build --target x86_64-pc-windows-gnu` (the three release
   targets are in `termphin/scripts/build_remote_agent.sh`: `x86_64-unknown-linux-musl`,
   `aarch64-unknown-linux-musl`, `x86_64-pc-windows-gnu` - vt100/vte/arrayvec are pure
   Rust with no OS-specific code paths, should cross-compile cleanly to all three, but
   actually build all three before declaring done, don't assume).
2. `cargo test` - full suite green, including the new vt100-round-trip tests above.
3. `cargo clippy --all-targets` - clean.
4. Manual repro: run the built binary locally, `termphin-agent attach <name>`, generate
   a long spinner-heavy session (e.g. run Claude Code or any status-line-heavy TUI for
   a few minutes - or synthetically write a spinner loop to the PTY), detach, then
   `termphin-agent attach --replay <name>` and confirm the screen redraws cleanly with
   no scattered characters. The existing corrupted session on this machine
   (`~/.cache/termphin/sessions/termphin_Agent`) is a ready-made repro - `termphin-agent
   attach --replay Agent` against it (after rebuilding and restarting that session, since
   the *live* master process needs to be one built from the new binary to have a
   `vt100`-backed `History` - killing and reattaching to recreate it, or `termphin-agent
   kill Agent` then reattach, will do that).
5. Once satisfied: `scripts/build_remote_agent.sh` from the `termphin` repo (needs
   `TERMPHIN_AGENT_REPO` pointed at this checkout, or run from a sibling directory per
   its own logic - see that script) to refresh `assets/termphin-agent/*` and
   `manifest.properties` in the `termphin` app repo. This is required for the fix to
   ever reach the app - the app ships prebuilt binaries, it does not build this crate.
6. Bump `termphin-agent`'s own `Cargo.toml` `version` and add a changelog entry if this
   project keeps one (check for `CHANGELOG.md` in this repo - unlike `terminal_view` it
   may not have one; check before assuming).

## Explicitly out of scope / not touched by this plan

- The unrelated in-progress diff already sitting in `src/main.rs`/`src/unix.rs`
  (abandoned-session sweeping, reboot-restore wording) - leave it, build on top of it,
  do not revert it.
- `terminal_view` (separate repo, separate bug, already fixed and released as
  `0.1.4` this session - unrelated to this one beyond both being terminal-rendering
  correctness work).
- Windows-specific PTY/ConPTY quirks beyond wiring `History::new`/`resize` call sites -
  no reason to expect ConPTY output differs from Unix PTY output in a way that affects
  vt100 parsing, but this wasn't verified against a live Windows session in this
  session of work.
