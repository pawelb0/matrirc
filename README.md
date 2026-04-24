# matrirc

Local IRC server backed by Matrix. Point irssi/weechat/hexchat at
`127.0.0.1:6667` and talk to your Matrix rooms + DMs from there. E2EE
included.

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

Once connected, `/msg matrirc help` prints the full command reference
from inside irssi. `/msg matrirc search <term>` queries the public-room
directory; `/join #alias:server.org` joins any public Matrix room.
`/msg @alice:server.org hi` opens or creates a DM.

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

Inbound events route through the bridge and get emitted as `PRIVMSG`
to every connection that joined the target channel. Outbound `PRIVMSG`
matches channel name → room id (or nick → DM room id), then
`room.send()`. DMs don't `JOIN` — each peer becomes an IRC query when
the first message arrives (or when the client sends `/query`).

Matrirc tracks a "we just sent this" event-id set so sync echoes of
your own messages don't double-print in irssi.

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

Historic messages need the server-side key backup. If Element set one
up, the backup key rides along with SAS. If not, old messages stay
encrypted. `matrirc verify` prints the current state.

Recovery-key path: `matrirc bootstrap-e2ee` reads
`MATRIRC_RECOVERY_KEY` (or stdin), opens SSS, pulls cross-signing +
backup directly. Not stored anywhere.

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
- `RUST_LOG` — `tracing-subscriber` filter

## Building

```
cargo build --release
cargo test
```

Rust 1.95+. First build pulls matrix-sdk, takes a few minutes.

## Caveats

- No SSO / OIDC / QR sign-in. On homeservers that only advertise those
  (matrix.org, anything behind MAS), grab an access token from Element
  and run `matrirc login ... --token`.
- Single account per daemon.
- IRC listener binds `127.0.0.1` without TLS. Don't expose it.

## License

GPL-3.0-or-later.
