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
  inside a directory created with mode `0700`. Nothing binds a port.
- **No privileges.** Runs as your user. Nothing is installed system-wide, no
  setuid, no service unit, no cron entry.
- **Everything lives under `~/.cache/termphin/sessions`.** One directory per
  session holding the socket and a `created_at` stamp. Killing a session
  removes its directory; nothing else on disk is touched.
- **It starts your `$SHELL`** (or `/bin/sh`) as a login shell on a PTY, and
  passes bytes between that PTY and the socket unchanged.
- **Scrollback is held in memory only**, a 256 KiB ring per session. It is
  never written to disk.
- **One dependency**, `libc`, in about 1400 lines of one file, which is small
  enough to read in an afternoon.

Authorization is the filesystem. Anyone who can reach the socket is already
running as your user, and could read the PTY anyway.

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

That writes stripped static x86_64 and aarch64 binaries to `dist/` along with
their SHA-256 manifest, using musl cross-compilation images pinned by digest.
The same source and the same script produce the same checksums on any host, so
you can verify that what Termphin uploads matches this repository. The app
checks those checksums before uploading, and again against whatever is already
on the server.

## License

MIT. See [LICENSE](LICENSE).
