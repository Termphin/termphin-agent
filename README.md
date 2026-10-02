[![Termphin: SSH that survives the dropped connection](https://termphin.dev/og-image.png)](https://termphin.dev)

# termphin-agent

The server-side half of [Termphin](https://termphin.dev), an SSH client whose
sessions survive a dropped connection. This is the part that runs on your
machine, built and released separately.

Keeps one remote shell alive while Termphin is disconnected, so a session
survives losing the network, backgrounding the app or closing it. Termphin
uploads this binary to the server over SSH and runs it there.

It is deliberately not a terminal multiplexer. No windows, panes, status bars,
copy modes or mouse handling, and no terminal emulation of its own: it holds a
PTY open and replays what the shell wrote.

## What it does on your server

The part worth reading before letting anything run on a machine you care about.

- **No network listener.** Every connection goes through a Unix socket at
  `~/.cache/termphin/sessions/<name>/control.sock`, created with mode `0600`
  inside a directory created with mode `0700`. Nothing binds a port. On
  Windows the equivalent is a named pipe under `%LOCALAPPDATA%\termphin\sessions`,
  restricted to its owner via an ACL rather than a mode bit.
- **No privileges.** Runs as your user. Nothing is installed system-wide, no
  setuid, no service unit, no cron entry.
- **Everything lives under `~/.cache/termphin/sessions`** (`%LOCALAPPDATA%\termphin\sessions`
  on Windows). One directory per session holds the socket, a `created_at`
  stamp and a `scrollback` snapshot. Killing a session removes its directory.
  Nothing else on disk is touched.
- **It starts your `$SHELL`** (or `/bin/sh`) as a login shell on a PTY, and
  passes bytes between that PTY and the socket unchanged. On Windows that's
  `powershell.exe` on a ConPTY pseudo console instead.
- **Scrollback is kept in memory**, the last 2000 rows of the session's
  terminal state. A snapshot is written to the session directory every 30
  seconds so a reboot can restore it.
- **Two dependencies on Unix**, `libc` and `vt100`, and about 3000 lines of
  code in `src/main.rs` and `src/unix.rs`. Small enough to read in an
  afternoon. Windows uses `windows-sys` for ConPTY and named pipes and lives in
  its own module.

Authorization is the filesystem (or, on Windows, the pipe's ACL). Anyone who
can reach it is already running as your user, and could read the PTY anyway.

### Windows

A few things work differently on Windows:

- **Resize** during an attach is polled every 100ms instead of delivered
  instantly. The module doc in `src/windows.rs` explains why.
- **The working directory** cannot be read from outside the process, because
  PowerShell's `cd` does not change the process's real directory. The shell
  is started with a `prompt` function that reports `$PWD` over an OSC marker.
  It is chained onto the user's own prompt, not replacing it, and is passed
  with `-EncodedCommand` so the shell never echoes it.
- **Persistence** needs the master process to break away from the job object
  that owns the SSH session (usually Win32-OpenSSH's). If the job does not
  allow that, the session ends with the connection, like any session that did
  not shut down cleanly. There is no separate "unsupported" mode.

## Using it from a terminal

The app installs the agent on its own. To reach the same sessions from a
terminal on the server, for example to continue on a laptop what you started
on your phone, put it on your PATH as `termphin`:

```sh
curl -fsSL https://termphin.dev/install.sh | sh
```

```powershell
irm https://termphin.dev/install.ps1 | iex
```

If the app has already installed the agent, the script only links it:
`~/.local/bin/termphin` on Linux and macOS (adding that directory to your shell
profile if needed), or a `termphin.cmd` next to the binary on Windows, with
that directory added to your user PATH. Otherwise it downloads the release
binary into the same place the app uses, verifies its checksum and writes the
metadata the app reads, so both always agree on which copy is current.

```text
termphin attach [name]    attach, choosing one if there are several
termphin new [name]       start a session and attach to it
termphin ls               list sessions
```

Detach with `Ctrl-\` then `d`. Press `Ctrl-\` twice to send it to the
program. Detaching resets any modes a program left your terminal in, such as
the alternate screen, mouse reporting or a hidden cursor.

Several clients can be attached at once. A phone and a laptop cannot both have
their own width, so the session takes the size of whichever client last
attached or typed, and goes back to the previous size when that client
leaves.

`attach` from inside a session is refused: every session's shell has
`TERMPHIN_SESSION` set to its name. Unset it to nest on purpose.

## Commands

```text
termphin-agent attach [--replay] [--resume <epoch>:<offset>] <name>
termphin-agent attach [name]
termphin-agent new [name]
termphin-agent ls
termphin-agent list
termphin-agent rename <old-name> <new-name>
termphin-agent kill <name>
termphin-agent version --machine
```

`attach` with `--replay` or `--resume` is what the app uses: no detach key, no
session picker, and the name is required. `list` prints one tab-separated line
per session for the app to parse. `ls` shows the same for people.

Each session has a master process and a Unix socket below
`~/.cache/termphin/sessions`. The attach client forwards terminal resize events
to the child PTY. The master keeps the session's terminal state: 2000 rows of
scrollback, the screen, and the active DEC private terminal modes.

`--replay` reconstructs a fresh local terminal's scrollback and modes. The
buffer is sent as a series of frames, since it can exceed the 1 MiB protocol
frame limit. Modes that only affect input encoding are re-asserted after the
replayed bytes. The alternate screen is re-entered ahead of them, and only when
the sequence that switched to it has already been evicted, because entering it
again would clear what the replay just drew.

`--resume <epoch>:<offset>` continues from a position in the session's output
instead, sending only what the client missed, as long as the master still
holds it: the last 1 MiB of raw output. Every replay and resume ends with
`ESC ] 5383;termphin-offset;<epoch>:<offset> BEL`, the position live output
starts at, which the client counts on from to know where to resume next time.
The epoch is new for every master, so a position from an earlier one of the
same name is refused rather than misread. When the position is not there, the
master replays instead; `--replay` alongside is what makes that fallback a
full scrollback. Resume is Unix only. ConPTY re-renders output on Windows, so
the offsets would not match, and the flag is ignored there.

On attach the master also briefly changes the PTY height while the alternate
screen is active. That buffer has no scrollback to reconstruct, so the
resulting SIGWINCH is what makes a full-screen application repaint its current
frame for the newly attached client.

## Limits

The master serves at most 16 concurrent connections, gives a connection 30
seconds to identify itself before dropping it, and disconnects a client that
falls more than 8 MiB behind rather than let it stall the session.

## Build

For the host:

```bash
cargo build --release
cargo test
```

For the binaries Termphin actually ships, which need docker:

```bash
./scripts/build.sh
```

That writes stripped static x86_64 and aarch64 Linux binaries plus an
x86_64 Windows one to `dist/`, along with their SHA-256 manifest, using
cross-compilation images pinned by digest (musl for Linux, mingw-w64 for
Windows). The same source and the same script produce the same checksums on
any host, so you can verify that what Termphin uploads matches this
repository. The app checks those checksums before uploading, and again
against whatever is already on the server.

## License

MIT. See [LICENSE](LICENSE).
