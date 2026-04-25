# matrirc

Local IRC server backed by Matrix. Point irssi/weechat/hexchat at
`127.0.0.1:6667` and talk to your Matrix rooms + DMs from there. E2EE
included.

![matrirc setup + irssi demo](demo/demo.gif)

## Install

Homebrew:

```
brew tap pawelb0/tap
brew install matrirc
```

From source:

```
cargo install --path .
```

## Quick start

```
matrirc login @you:homeserver.org
matrirc run
```

In your IRC client: `/connect 127.0.0.1` (or `/connect matrirc` if you
ran `matrirc install-irssi`, which registers a `matrirc` chatnet).

`login` prompts for your password and walks you through emoji
verification from another Element device. Config lands in
`~/.config/matrirc/config.toml`.

## irssi helper

```
matrirc install-irssi
```

Drops a Perl script into `~/.irssi/scripts/autorun/`. On irssi load it
starts the daemon (if not already running), creates a persistent
`matrirc` chatnet, runs `/connect matrirc`, reconnects on crash, and
SIGTERMs the daemon on `/quit`.

If you prefer not to autorun, skip `install-irssi` and just connect
manually: `/connect 127.0.0.1 6667` from your IRC client of choice.

For backfilled messages to render with their original send time,
flip on irssi's server-time display (off by default in 1.4):

```
/set show_server_time on
/save
```

## Media

Matrix attachments are gated behind authenticated media or E2EE
keys — IRC clients can't fetch them directly. Matrirc binds a
second listener on `127.0.0.1:6680` and rewrites image/file/audio/
video msgtypes to `<http://127.0.0.1:6680/attach/<event_id>>`. The
proxy looks up the indexed `MediaSource`, downloads via the
authenticated client, decrypts E2EE blobs, returns plaintext.

Setup:

```
matrirc install-irssi --media
# restart irssi (or /script load matrirc-media)
```

Commands (run from a channel or query window — that's the scope):

```
/mediashow [N|name|nick]            fetch + open via $MATRIRC_IMG_OPEN
/mediasave [N|name|nick] [dir]      fetch + save (default ~/Downloads)
/medialist [all]                    history; `all` for cross-channel
/mediasend <path> [caption]         upload a local file to the active room
```

`N` is an index into the current scope's history. A bare nick
(`/mediashow alice`) returns that user's most recent attachment;
a substring (`/mediashow screenshot`) matches the filename. Prefix
with `#channel` to override scope (`/mediashow #room 3`).

`/mediasend` always targets the active window; the path argument
tab-completes. Daemon caps uploads at 100 MiB; bigger files come
back as HTTP 413. Honors `MATRIRC_ATTACH_BIND` if set.

## Commands

```
matrirc login @you:server          password + SAS
matrirc login ... --token          use an access token (stdin / env)
matrirc login ... --skip-verify    skip SAS, do it later
matrirc verify                     redo SAS
matrirc bootstrap-e2ee             import via recovery key
matrirc run                        start the daemon
matrirc status                     check
matrirc stop                       SIGTERM
matrirc reset --force              wipe local state
matrirc install-irssi              drop irssi script
```

Once connected, from inside irssi:

- `/msg matrirc help` — full command reference
- `/msg matrirc search <term>` — public-room directory search
- `/join #alias:server.org` — join any public Matrix room
- `/msg @alice:server.org hi` — open or create a DM

## How it works

Matrirc is one long-running daemon with two tasks talking through a
small bridge module:

```
  IRC client ─TCP─▶ irc::serve ─────▶ bridge ◀───── matrix::run_sync ◀─HTTPS/sync─ homeserver
                       │                 ▲                │
                       └──────PRIVMSG────┘                └─sqlite crypto/state store
```

- `matrix::run_sync` restores the session (from `config.toml`), runs
  the Matrix sync loop, and keeps the sqlite crypto store under
  `~/.local/share/matrirc/store/` in sync.
- `irc::serve` accepts IRC clients on `127.0.0.1:6667`. Each client has
  its own task + CAP/NICK/USER state.
- `bridge` is a shared struct holding room↔channel + MXID↔nick maps
  plus a `broadcast` channel for Matrix→IRC events and an `mpsc` for
  IRC→Matrix commands.

At startup the sync task walks every joined room, classifies each as
channel or DM (`is_direct`), picks a stable channel name (slug of the
display name + 6-char suffix from the room id, persisted in
`names.json`), and pushes it into the mapping. When an IRC client
registers, matrirc auto-joins every known channel on its behalf,
backfills the last 200 messages, and attaches IRCv3 `@time=` tags if
the client negotiated the cap.

## Paths

```
~/.config/matrirc/config.toml       access token       0600
~/.local/share/matrirc/store/       crypto store       0700
~/.local/share/matrirc/names.json   channel naming
~/.local/state/matrirc/daemon.pid   pidfile
~/.local/state/matrirc/log          daemon log (irssi-spawned)
```

Respects `XDG_CONFIG_HOME` / `XDG_DATA_HOME` / `XDG_STATE_HOME`.

## E2EE

Password login gives you a fresh, unverified device. SAS emoji
verification cross-signs it. After that other clients share megolm
room keys with you.

Historic messages need the server-side key backup. SAS pulls the
backup key with it when Element has one set up; otherwise old messages
stay encrypted. `matrirc verify` prints the current state.

Recovery-key path: `matrirc bootstrap-e2ee` reads
`MATRIRC_RECOVERY_KEY` (or stdin), opens SSS, pulls cross-signing +
backup directly. The key is not stored anywhere.

## Naming

Rooms show up as `#slug-<6char>` where `<6char>` comes from the room
id localpart. Stable across renames. DMs open as IRC queries to the
peer's display-name (ASCII-sanitized).

Events:
- text / edit / reply / emote → `PRIVMSG`
- reaction → `* nick reacted X` (CTCP ACTION)
- image/file/audio/video → `[image] caption <https-url>`
- topic change → `TOPIC`
- undecryptable → placeholder line

## Env

- `MATRIRC_PASSWORD` — skip prompt
- `MATRIRC_TOKEN` — for `--token`
- `MATRIRC_RECOVERY_KEY` — for `bootstrap-e2ee`
- `MATRIRC_ROOM` — bridge only this room id as `#matrix` (dev)
- `MATRIRC_BIND` — override IRC listen addr (default `127.0.0.1:6667`)
- `MATRIRC_ATTACH_BIND` — override media-proxy listen addr (default `127.0.0.1:6680`)
- `RUST_LOG` — `tracing-subscriber` filter

## Building

```
cargo build --release
cargo test
```

Rust 1.95+. First build takes a few minutes.

## Caveats

- No SSO / OIDC / QR sign-in. On homeservers that only advertise those
  (matrix.org, anything behind MAS), grab an access token from Element
  and run `matrirc login ... --token`.
- Single account per daemon.
- IRC listener binds `127.0.0.1` without TLS. Don't expose it.

## License

GPL-3.0-or-later.
