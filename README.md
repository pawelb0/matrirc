# matrirc

matrirc runs a local IRC server for your Matrix account. Connect with irssi,
WeeChat, or HexChat to read and send messages in rooms and DMs, including
end-to-end encrypted rooms.

![matrirc setup and irssi demo](demo/demo.gif)

## Install

With Homebrew:

```sh
brew tap pawelb0/tap
brew install matrirc
```

For nightly builds from `main`:

```sh
brew install matrirc-nightly
brew link --overwrite matrirc-nightly
```

The nightly formula is keg-only; the second command puts it on `PATH`.

From a source checkout:

```sh
cargo install --path .
```

## Connect

```sh
matrirc login @you:homeserver.org
matrirc run
```

Login prompts for your password, saves the session, and starts emoji
verification. Compare the emoji with another device, such as Element, and
confirm on both devices.

In your IRC client:

```text
/connect 127.0.0.1 6667
```

Joined Matrix rooms open as IRC channels once the initial sync finishes.
Channel names use the room's display name and a six-character room-ID suffix,
such as `#project-AbCdEf`. Names persist across restarts and room renames.
DMs use query windows named after the peer's display name, converted to an IRC
nick.

matrirc requests up to 1,000 recent events per room when you connect. To show
original timestamps in irssi:

```text
/set show_server_time on
/save
```

### irssi helper

```sh
matrirc install-irssi
```

This installs `~/.irssi/scripts/autorun/matrirc.pl`. When loaded, the script
starts matrirc if needed, creates a `matrirc` network, and connects to it.
It checks the daemon every five seconds and restarts it if it stops; irssi
handles reconnection. On quit or script unload, it stops the daemon if the
script started it.

After upgrading matrirc, update the installed script with:

```sh
matrirc install-irssi --force
```

The helper also displays reply IDs. For manual connections, run `matrirc run`
yourself and use `/connect 127.0.0.1 6667`.

## Rooms and messages

Run these commands in your IRC client:

| Command | Effect |
| --- | --- |
| `/msg matrirc help` | Show the full command reference. |
| `/msg matrirc search <term>` | Search the public-room directory. |
| `/msg matrirc join <#alias:server or !room:server>` | Join a room by alias or ID. |
| `/msg matrirc knock <target> [reason]` | Request admission to a room that allows knocking. |
| `/join #alias:server.org` | Join a Matrix room by alias. |
| `/msg @alice:server.org hi` | Find or create a DM and send a message. |
| `/me <action>` | Send a Matrix emote. |

**Parting a bridged channel leaves the Matrix room.** To reload history,
disconnect and reconnect to matrirc.

Text, edits, and replies appear as IRC messages. Reactions appear as actions,
attachments as local URLs, and topic changes as IRC topics. Messages that
cannot be decrypted appear as placeholders.

### Replies

The irssi helper displays a three-letter ID beside incoming messages:

```text
14:33 alice | [abc] could you take a look at this?
```

Reply in the same channel or query window:

```text
!r abc on it
```

matrirc sends a Matrix reply referencing the original event. Incoming replies
include a quote above the body.

Each connection remembers the last 64 reply targets per window. An ID is
derived from the Matrix event ID, so it stays the same when history is loaded
again. If two stored targets share an ID, the newer one wins. An unknown ID
produces a notice in the `matrirc` window; the message is not sent.

To inspect stored targets or hide IDs:

```text
/msg matrirc dump <#channel or peer>
/msg matrirc ids off
```

The `ids` command also accepts `on`, `toggle`, and `status`. Set
`show_reply_ids = false` in `config.toml` to hide IDs by default.

## Media

matrirc serves Matrix attachments through a local HTTP proxy at
`127.0.0.1:6680`. It downloads attachments using your Matrix session and
decrypts encrypted files before serving them. Messages contain URLs such as
`http://127.0.0.1:6680/attach/<event_id>`.

Install the optional irssi media script:

```sh
matrirc install-irssi --media --force
```

Restart irssi or run `/script load matrirc-media`. The script uses `curl` to
transfer files.

| Command | Effect |
| --- | --- |
| `/mediashow [N\|name\|nick]` | Download and open an attachment. |
| `/mediasave [N\|name\|nick] [dir]` | Save an attachment, by default to `~/Downloads`. |
| `/medialist [all]` | List attachments in this window, or across windows with `all`. |
| `/mediasend <path> [caption]` | Upload a file to the active room or DM. |

Selection uses the current window's attachment history. Use an index, a
filename substring, or a nick to select that user's latest attachment.
Prefix a selection with a channel to use its history, for example
`/mediashow #project-AbCdEf 3`.

`/mediasend` supports path completion. Quote paths containing spaces:

```text
/mediasend "~/My Pics/x.png" screenshot
/statusbar window add matrirc_upload
```

The statusbar item shows upload progress. Uploads over 100 MiB are rejected
with HTTP 413.

Set `MATRIRC_IMG_OPEN` before starting irssi to choose the opener (default:
`open`), and `MATRIRC_SAVE_DIR` to change the save directory. If you change
`MATRIRC_ATTACH_BIND`, use the same value for the daemon and irssi.

## Login and encryption

Password login creates a new Matrix device. If verification fails or you use
`--skip-verify`, retry it with:

```sh
matrirc verify
```

This also reports the device's encryption and backup state. Decrypting old
messages requires the corresponding room keys. If they are in your Matrix
key backup, recover access with:

```sh
matrirc bootstrap-e2ee
```

Supply the recovery key through `MATRIRC_RECOVERY_KEY` or standard input.
The command imports cross-signing and backup secrets from Matrix secret
storage. It does not save the supplied recovery key. History remains
unreadable if the required room keys are unavailable. Restart the daemon after
recovery.

matrirc does not support SSO, OIDC, or QR sign-in. For an account that requires
one of these, supply an access token through `MATRIRC_TOKEN` or standard input:

```sh
matrirc login @you:homeserver.org --token
```

Use `--homeserver https://matrix.example.org` to override homeserver discovery.

## Daemon commands

| Command | Effect |
| --- | --- |
| `matrirc run` | Run the daemon in the foreground. This is also the default command. |
| `matrirc status` | Check whether the daemon is running. |
| `matrirc stop` | Send SIGTERM to the daemon. |
| `matrirc reset` | Delete the local session, crypto store, and saved channel names after confirmation. |

`reset --force` skips confirmation. Resetting does not sign out the device on
the homeserver; remove the old session in another Matrix client.

## Configuration and files

Login writes `~/.config/matrirc/config.toml` with your Matrix user ID,
homeserver URL, access token, and device ID.

| Default path | Contents |
| --- | --- |
| `~/.config/matrirc/config.toml` | Session and settings; created with mode `0600`. |
| `~/.local/share/matrirc/store/` | Matrix state and crypto store; directory mode `0700`. |
| `~/.local/share/matrirc/names.json` | Saved channel names. |
| `~/.local/state/matrirc/daemon.pid` | Daemon PID file. |
| `~/.local/state/matrirc/log` | Output from a daemon started by the irssi helper. |

The daemon respects `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, and `XDG_STATE_HOME`.
The irssi helper uses `~/.local/state/matrirc` for its log and PID lookup.

| Environment variable | Purpose |
| --- | --- |
| `MATRIRC_PASSWORD` | Supply the login password without a prompt. |
| `MATRIRC_TOKEN` | Supply the access token for `login --token`. |
| `MATRIRC_RECOVERY_KEY` | Supply the key for `bootstrap-e2ee`. |
| `MATRIRC_BIND` | IRC listen address; default `127.0.0.1:6667`. |
| `MATRIRC_ATTACH_BIND` | Media listen address; default `127.0.0.1:6680`. |
| `MATRIRC_ROOM` | Limit initial room discovery to this room ID, for development. |
| `RUST_LOG` | Set the tracing filter; default `matrirc=info`. |

One daemon serves one Matrix account. The IRC listener has no TLS, and the
media proxy serves decrypted files. Keep both listeners on loopback.

## Development

Build with Rust 1.95 or newer:

```sh
cargo build --release
cargo test
cargo clippy --all-targets -- -D warnings
```

CI runs tests and Clippy on Linux and macOS with stable Rust.

The daemon uses Tokio. `src/matrix.rs` restores the session, runs Matrix sync,
and sends Matrix requests. `src/irc/conn.rs` handles each IRC connection.
`src/bridge.rs` holds room and nick mappings, broadcasts Matrix events to IRC
clients, and queues IRC commands for the Matrix task. `src/proxy.rs` serves
attachments and accepts uploads; `src/names.rs` persists channel names.

For debug logs:

```sh
RUST_LOG=matrirc=debug,matrix_sdk=info matrirc run
```

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
