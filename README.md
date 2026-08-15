# termphin-agent

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
  on Windows). One directory per session holding the socket and a `created_at`
  stamp. Killing a session removes its directory; nothing else on disk is
  touched.
- **It starts your `$SHELL`** (or `/bin/sh`) as a login shell on a PTY, and
  passes bytes between that PTY and the socket unchanged. On Windows that's
  `powershell.exe` on a ConPTY pseudo console instead.
- **Scrollback is held in memory only**, a 256 KiB ring per session. It is
  never written to disk, except a periodic snapshot so a reboot can restore it.
- **One dependency on Unix**, `libc`, in about 1400 lines of one file, small
  enough to read in an afternoon. Windows needs `windows-sys` for ConPTY and
  named pipes and is a separate module.

Authorization is the filesystem (or, on Windows, the pipe's ACL). Anyone who
can reach it is already running as your user, and could read the PTY anyway.

Windows has one gap against the Unix build: a live terminal resize during an
attach is polled every 100ms rather than delivered instantly - see the module
doc in `src/windows.rs` for why. The shell's working directory *is* tracked,
but by a different route: PowerShell's `cd` never touches the process's real
OS-level directory, so there is no `/proc/<pid>/cwd` equivalent to read from
outside. Instead the shell is started with a `prompt` function - chained onto
whatever the user's own profile defines, never replacing it - that reports
`$PWD` over an OSC marker. It rides in on `-EncodedCommand` so the shell
never echoes it. Persistence itself - a session surviving the
SSH connection that started it - relies on the new master process breaking
away from whatever job object owns the SSH session (Win32-OpenSSH's, usually).
If that job doesn't permit breakaway, the session just doesn't outlive the
connection, the same as a session that didn't shut down cleanly for any other
reason - there is no separate "unsupported" mode.

## Commands

```text
termphin-agent attach [--replay] <name>
termphin-agent list
termphin-agent rename <old-name> <new-name>
termphin-agent kill <name>
termphin-agent version --machine
```

Each session has a master process and a Unix socket below
`~/.cache/termphin/sessions`. The attach client forwards terminal resize events
to the child PTY. The master retains 256 KiB of raw output and tracks the
active DEC private terminal modes.

`--replay` reconstructs a fresh local terminal's scrollback and modes. The
buffer is sent as a series of frames, since it can exceed the 1 MiB protocol
frame limit. Modes that only affect input encoding are re-asserted after the
replayed bytes. The alternate screen is re-entered ahead of them, and only when
the sequence that switched to it has already been evicted, because entering it
again would clear what the replay just drew.

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
