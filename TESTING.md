# Testing matrirc

Five layers, each catching a different bug class. All tests live alongside the
code under `#[cfg(test)]`, run under `cargo test`, and must pass `cargo clippy
--all-targets -- -D warnings`.

## 1. Pure helpers — `src/matrix.rs::tests`

Free functions: `body_from_event`, `mxc_to_https`, `strip_reply_fallback`,
`sanitize_nick`, `server_name_from_mxid`. No async, no fixtures.

**Catches:** rendering regressions (e.g. CTCP ACTION wrapping for `/me`, MXC
→ HTTPS resolution for images), nick sanitization edges.

**Adding a test:** build a `RoomMessageEventContent` via the ruma
`plain`-style constructors, call the helper, assert on the returned string.

## 2. Bridge mapping + broadcasts — `src/bridge.rs::tests`

`Mapping` round-trips and `Bridge` broadcast invariants. No async work
needed beyond constructing `Bridge::new(Mapping::default())`.

**Catches:** the DM identity-split class of bugs — every alias form (nick,
MXID, localpart, case variants, with/without leading `@`) must resolve to the
same DM room. Also `RECENT_SENT_CAP` FIFO eviction and `RoomAdded` /
`TopicChanged` broadcast fan-out.

**Adding a test:** `let (b, rx) = Bridge::new(Mapping::default());` then drive
`insert_dm` / `add_mapping` / `update_topic` and either inspect the mapping
under the read lock or assert on `rx.recv().await`.

## 3. IRC event-routing — `src/irc/conn.rs::tests` (Matrix → IRC)

Exercises `handle_matrix_event` end-to-end with an in-memory writer
(`Vec<u8>`). The helpers were refactored to accept `&mut (impl AsyncWrite +
Unpin)` so no OS socket is needed.

**Catches:** own-message mis-routing (DM vs self-query), missing DM rooms
dropped silently, JOIN-on-RoomAdded ordering, TOPIC propagation.

**Adding a test:** pre-populate the bridge mapping, drive a `FromMatrix::*`
event through `handle_matrix_event`, then parse the bytes written to the
sink.

## 4. IRC command-dispatch — `src/irc/conn.rs::tests` (IRC → Matrix)

Drives `handle_command` with parsed `Message` values and asserts on the
wire-format reply bytes. Covers PING, JOIN (ack/403), PRIVMSG (echo/401),
WHOIS (311/318/401), and the matrirc bot commands (including silent
drop of CTCP ACTION).

**Catches:** unknown-command responses, handshake sequencing, the
matrirc-bot "help" path regression.

**Adding a test:** `let mut s = registered_state("nick");` then call the
`dispatch` helper with a wire-format command string; assert on the drained
output.

## 5. Proptest wire round-trip — `src/irc/proto.rs::tests`

Generator-driven parse/serialize equivalence. 1000 cases per run.

**Catches:** tag escaping/unescaping regressions, trailing-parameter rules,
case normalization, prefix handling edges.

**Adding a test:** extend `arb_message` with new generators (e.g. wider
prefix charset) — the existing `round_trip_any_valid_message` proptest
picks them up.

## CI

`.github/workflows/ci.yml` runs `cargo clippy --all-targets -- -D warnings`
and `cargo test` on ubuntu-latest + macos-latest. Triggered on push to
`main` and all pull requests.
