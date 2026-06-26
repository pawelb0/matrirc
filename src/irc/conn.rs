use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;

use anyhow::{Context, Result};
use time::format_description::FormatItem;
use time::macros::format_description;
use time::OffsetDateTime;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use super::proto::Message;
use crate::bridge::{Bridge, FromMatrix, ToMatrix};

const ISO_FMT: &[FormatItem<'static>] = format_description!(
    "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
);
const SERVER_TIME_CAP: &str = "server-time";
const MESSAGE_TAGS_CAP: &str = "message-tags";
const ECHO_MESSAGE_CAP: &str = "echo-message";
const SUPPORTED_CAPS: &[&str] = &[SERVER_TIME_CAP, MESSAGE_TAGS_CAP, ECHO_MESSAGE_CAP];

const SERVER_NAME: &str = "matrirc.local";
const VERSION: &str = concat!("matrirc-", env!("CARGO_PKG_VERSION"));
const MAX_LINE: usize = 8192;

// IRC numeric replies (RFC 1459 / RFC 2812 / modern IRCv3). Wire format is the
// decimal string, so we keep them as `&str` to drop into `srv(...)` directly.
mod rpl {
    pub const WELCOME: &str = "001";
    pub const YOURHOST: &str = "002";
    pub const CREATED: &str = "003";
    pub const MYINFO: &str = "004";
    pub const ADMINME: &str = "256";
    pub const ADMINLOC1: &str = "257";
    pub const ADMINLOC2: &str = "258";
    pub const ADMINEMAIL: &str = "259";
    pub const WHOISUSER: &str = "311";
    pub const WHOISSERVER: &str = "312";
    pub const ENDOFWHO: &str = "315";
    pub const ENDOFWHOIS: &str = "318";
    pub const WHOISCHANNELS: &str = "319";
    pub const LISTSTART: &str = "321";
    pub const LIST: &str = "322";
    pub const LISTEND: &str = "323";
    pub const NOTOPIC: &str = "331";
    pub const TOPIC: &str = "332";
    pub const WHOREPLY: &str = "352";
    pub const NAMREPLY: &str = "353";
    pub const LINKS: &str = "364";
    pub const ENDOFLINKS: &str = "365";
    pub const ENDOFNAMES: &str = "366";
    pub const INFO: &str = "371";
    pub const MOTD: &str = "372";
    pub const ENDOFINFO: &str = "374";
    pub const MOTDSTART: &str = "375";
    pub const ENDOFMOTD: &str = "376";
    pub const ERR_NOSUCHNICK: &str = "401";
    pub const ERR_NOSUCHCHANNEL: &str = "403";
}

const ECHO_NICK: &str = "echo";
const ECHO_PREFIX: &str = "echo!echo@matrirc.local";
const ECHO_CHAN: &str = "#echo";

/// One-line summaries of the `/msg matrirc <cmd>` surface. Shared between the
/// `help` bot output and the MOTD cheat-sheet so they can't drift apart.
const BOT_CMDS: &[&str] = &[
    "help                          this message",
    "rooms                         list bridged Matrix channels",
    "dms                           list known Matrix DMs",
    "search <term> [on <server>]   public-room directory",
    "join <!room:server | #alias>  join a matrix room by id or alias",
    "knock <target> [reason]       knock on an invite-only room",
    "ids [on|off|toggle|status]    toggle IRCv3 msgid tag for !r replies",
    "dump <window>                 show the reply-id ring for #chan or peer",
    "version                       matrirc version",
];
const ECHO_TOPIC: &str = "Echo channel — anything you say, echo will say back";
const BOT_PREFIX: &str = "matrirc!matrirc@matrirc.local";

fn user_prefix(nick: &str) -> String {
    format!("{nick}!{nick}@matrirc.local")
}

/// (short_id, event_id, sender_nick, body_snippet) tuples per window. The
/// snippet and sender let the outbound `!r` path render a quote line in the
/// echo-message bounce so the user sees what they replied to.
type ReplyEntry = (String, matrix_sdk::ruma::OwnedEventId, String, String);
type ReplyRing = HashMap<String, VecDeque<ReplyEntry>>;

#[derive(Default)]
struct State {
    nick: Option<String>,
    user: Option<String>,
    registered: bool,
    joined: HashSet<String>,
    caps: HashSet<String>,
    dm_backfilled: HashSet<matrix_sdk::ruma::OwnedRoomId>,
    dm_hinted: HashSet<matrix_sdk::ruma::OwnedRoomId>,
    /// Per-target ring (channel slug or peer nick, ASCII-lowercased) →
    /// recent (short_id, event_id) pairs. Lets `!r <id>` resolve back to a
    /// Matrix event for `m.in_reply_to`.
    reply_ids: ReplyRing,
    /// Emit IRCv3 `msgid` tag on inbound matrix messages. Toggled at runtime
    /// via the `ids` bot command; initial value comes from
    /// `Bridge::default_show_reply_ids`.
    show_reply_ids: bool,
    /// Monotonic counter feeding synthesised event ids for `#echo` replies,
    /// so the test bot exercises the same msgid/!r flow as Matrix.
    echo_counter: u64,
}

fn synth_echo_event_id(counter: &mut u64) -> matrix_sdk::ruma::OwnedEventId {
    *counter += 1;
    let s = format!("$echo{counter}:matrirc.local");
    matrix_sdk::ruma::OwnedEventId::try_from(s.as_str()).expect("synthetic echo event id parses")
}

const REPLY_RING_CAP: usize = 64;
/// IRC frames cap at 512 bytes including framing and CRLF. 400 leaves room
/// for any reasonable `:nick!nick@matrix PRIVMSG <target> :` prefix plus tags.
const SAFE_BODY_BYTES: usize = 400;

/// 3-letter short id derived from a Matrix event id via FNV-1a → base-26
/// (a-z) over 17 576 slots. Deterministic: the same event id always renders
/// the same short across daemon restarts. Collisions are handled by
/// `lookup_reply_id` walking newest-first (latest match wins).
fn short_event_id(id: &matrix_sdk::ruma::EventId) -> String {
    let mut h: u32 = 0x811c_9dc5;
    for b in id.as_str().bytes() {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    let mut v = (h as usize) % (26 * 26 * 26);
    let mut buf = [b'a'; 3];
    for slot in buf.iter_mut().rev() {
        *slot = b'a' + (v % 26) as u8;
        v /= 26;
    }
    String::from_utf8(buf.to_vec()).expect("ascii bytes")
}

fn remember_reply_id(
    ring: &mut ReplyRing,
    target: &str,
    event_id: matrix_sdk::ruma::OwnedEventId,
    sender: String,
    snippet: String,
) -> String {
    let short = short_event_id(&event_id);
    let q = ring.entry(target.to_ascii_lowercase()).or_default();
    if q.len() >= REPLY_RING_CAP { q.pop_front(); }
    q.push_back((short.clone(), event_id, sender, snippet));
    short
}

fn lookup_reply_id(
    ring: &ReplyRing,
    target: &str,
    short: &str,
) -> Option<(matrix_sdk::ruma::OwnedEventId, String, String)> {
    let q = ring.get(&target.to_ascii_lowercase())?;
    // Walk newest-first explicitly so the latest entry sharing `short` wins
    // (relevant only on hash collisions, but worth pinning the behaviour down).
    for entry in q.iter().rev() {
        if entry.0 == short {
            return Some((entry.1.clone(), entry.2.clone(), entry.3.clone()));
        }
    }
    None
}

/// First non-empty line of `body`, trimmed and capped — used as the quoted
/// snippet in outbound reply echoes. Strips colour codes that would corrupt
/// the IRC line when prepended verbatim.
fn body_snippet(body: &str) -> String {
    let line = body.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    let stripped: String = line.chars().filter(|c| !matches!(*c as u32, 0..=0x1f)).collect();
    if stripped.chars().count() <= 60 {
        stripped
    } else {
        let mut out: String = stripped.chars().take(57).collect();
        out.push_str("...");
        out
    }
}

fn flatten_newlines(body: &str) -> String {
    body.replace('\n', " ↵ ")
}

/// Splits `body` at byte budgets ≤ `SAFE_BODY_BYTES`, keeping UTF-8 boundaries
/// and preferring whitespace breaks. Single-element vec when already fits.
fn chunk_for_irc(body: &str) -> Vec<String> {
    if body.len() <= SAFE_BODY_BYTES {
        return vec![body.to_string()];
    }
    let mut out = Vec::new();
    let mut rest = body;
    while rest.len() > SAFE_BODY_BYTES {
        let mut split = SAFE_BODY_BYTES;
        while !rest.is_char_boundary(split) { split -= 1; }
        if let Some(ws) = rest[..split].rfind(char::is_whitespace) {
            split = ws.max(1);
        }
        let (head, tail) = rest.split_at(split);
        out.push(head.trim_end().to_string());
        rest = tail.trim_start();
    }
    if !rest.is_empty() { out.push(rest.to_string()); }
    out
}

/// Flattens, chunks, and (when reply ids are enabled and `event_id` is set)
/// stashes the short id + sender/snippet in the ring. Returns
/// `(short_id, pieces)`; the caller attaches `short_id` as the IRCv3 `msgid`
/// tag on the first PRIVMSG so client scripts can render it between time and
/// nick.
#[allow(clippy::too_many_arguments)]
fn make_payloads(
    body: &str,
    sender: &str,
    event_id: Option<matrix_sdk::ruma::OwnedEventId>,
    ring_key: &str,
    ring: &mut ReplyRing,
    show_reply_ids: bool,
) -> (Option<String>, Vec<String>) {
    let flattened = flatten_newlines(body);
    if flattened.is_empty() { return (None, Vec::new()); }
    let short = (show_reply_ids).then(|| event_id.map(|eid| {
        remember_reply_id(ring, ring_key, eid, sender.to_string(), body_snippet(body))
    })).flatten();
    (short, chunk_for_irc(&flattened))
}

pub async fn handle(sock: TcpStream, peer: SocketAddr, bridge: Bridge) -> Result<()> {
    let (read, mut write) = sock.into_split();
    let mut lines = BufReader::new(read).lines();
    let mut from_matrix = bridge.from_matrix.subscribe();
    let mut s = State {
        show_reply_ids: bridge
            .default_show_reply_ids
            .load(std::sync::atomic::Ordering::Relaxed),
        ..State::default()
    };

    loop {
        tokio::select! {
            line_res = read_line(&mut lines) => {
                let Some(line) = line_res? else { break; };
                let msg = match Message::parse(&line) {
                    Ok(m) => m,
                    Err(e) => { debug!(%peer, error = %e, "bad line"); continue; }
                };
                if handle_command(&mut write, &peer, &bridge, &msg, &mut s).await? { return Ok(()); }
                if !s.registered {
                    if let (Some(n), Some(_)) = (s.nick.clone(), s.user.clone()) {
                        send_welcome(&mut write, &n).await?;
                        s.registered = true;
                        info!(%peer, nick = %n, "client registered");
                        auto_join_all(&mut write, &n, &bridge, &mut s).await?;
                    }
                }
            }
            ev = from_matrix.recv() => {
                match ev {
                    Ok(e) => handle_matrix_event(&mut write, &bridge, &mut s, e).await?,
                    Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(n)) => warn!(%peer, "lagged {n} events"),
                }
            }
        }
    }
    if s.registered {
        info!(%peer, "client disconnected");
    } else {
        debug!(%peer, "probe closed");
    }
    Ok(())
}

/// Prepends `nick: ` to `body` so irssi's default highlight fires. No-op when
/// the nick already appears as a standalone token in the body, when the nick
/// is unknown, or when the body is empty.
fn ensure_self_mention(body: &str, own_nick: Option<&str>) -> String {
    let Some(nick) = own_nick else { return body.to_string(); };
    if nick.is_empty() || body.is_empty() {
        return body.to_string();
    }
    let lower = body.to_ascii_lowercase();
    let needle = nick.to_ascii_lowercase();
    let mut search = lower.as_str();
    let mut offset = 0;
    while let Some(pos) = search.find(&needle) {
        let abs = offset + pos;
        let before_ok = abs == 0
            || !lower.as_bytes()[abs - 1].is_ascii_alphanumeric();
        let after = abs + needle.len();
        let after_ok = after >= lower.len()
            || !lower.as_bytes()[after].is_ascii_alphanumeric();
        if before_ok && after_ok {
            return body.to_string();
        }
        offset = abs + 1;
        search = &lower[offset..];
    }
    format!("{nick}: {body}")
}

async fn handle_matrix_event(
    write: &mut (impl tokio::io::AsyncWrite + Unpin),
    bridge: &Bridge,
    s: &mut State,
    ev: FromMatrix,
) -> Result<()> {
    match ev {
        FromMatrix::Message { room, sender_nick, body, event_id, reply_quote, is_own, mentions_self } => {
            let (prefix, target, is_channel, ring_key) = if let Some(chan) = bridge.chan_for(&room) {
                if !s.joined.contains(&chan) { return Ok(()); }
                let key = chan.clone();
                (format!("{sender_nick}!{sender_nick}@matrix"), chan, true, key)
            } else if let Some(peer) = bridge.dm_nick_for(&room) {
                let Some(n) = s.nick.as_deref() else { return Ok(()); };
                let key = peer.clone();
                if is_own {
                    // Own message from another device: ZNC-style self→peer so it
                    // renders in the peer's query window, not a self-query.
                    (format!("{n}!{n}@matrirc.local"), peer, false, key)
                } else {
                    (format!("{sender_nick}!{sender_nick}@matrix"), n.to_string(), false, key)
                }
            } else { return Ok(()); };
            // DM windows already highlight unconditionally; only force-fire on channels.
            let body = if is_channel && mentions_self && !is_own {
                ensure_self_mention(&body, s.nick.as_deref())
            } else {
                body
            };
            if let Some(q) = reply_quote {
                send(write, Message::with_prefix(&prefix, "PRIVMSG", vec![target.clone(), q])).await?;
            }
            let (msgid, pieces) = make_payloads(
                &body, &sender_nick, event_id, &ring_key, &mut s.reply_ids, s.show_reply_ids,
            );
            for (i, payload) in pieces.into_iter().enumerate() {
                let mut out = Message::with_prefix(&prefix, "PRIVMSG", vec![target.clone(), payload]);
                if i == 0 {
                    if let Some(id) = &msgid {
                        if s.caps.contains(MESSAGE_TAGS_CAP) {
                            out = out.with_tag("msgid", id.clone());
                        }
                    }
                }
                send(write, out).await?;
            }
        }
        FromMatrix::RoomAdded { room, chan, topic } => {
            if !s.registered || s.joined.contains(&chan) { return Ok(()); }
            if let Some(n) = s.nick.as_deref() {
                join_bridged(write, n, &chan, &room, &topic, bridge, &s.caps, &mut s.reply_ids, s.show_reply_ids).await?;
                s.joined.insert(chan);
            }
        }
        FromMatrix::DmAdded { nick: dm } => {
            if s.registered {
                if let Some(n) = s.nick.as_deref() {
                    matrirc_notice(write, n, &format!("DM available: /msg {dm} ...")).await?;
                    // RoomAdded does backfill as part of join_bridged(), here do it directly.
                    if let Some(room) = bridge.dm_room_for(&dm) {
                        if s.dm_backfilled.insert(room.clone()) {
                            backfill_channel(write, n, &room, bridge, &s.caps, &mut s.reply_ids, s.show_reply_ids).await?;
                        }
                    }
                }
            }
        }
        FromMatrix::TopicChanged { chan, topic } => {
            if s.registered && s.joined.contains(&chan) {
                send(write, srv("TOPIC", vec![chan, topic])).await?;
            }
        }
        FromMatrix::MemberJoined { chan, nick } => {
            if !s.registered || !s.joined.contains(&chan) { return Ok(()); }
            let prefix = format!("{nick}!{nick}@matrix");
            send(write, Message::with_prefix(prefix, "JOIN", vec![chan])).await?;
        }
        FromMatrix::MemberLeft { chan, nick, reason } => {
            if !s.registered || !s.joined.contains(&chan) { return Ok(()); }
            let prefix = format!("{nick}!{nick}@matrix");
            let mut params = vec![chan];
            if let Some(r) = reason {
                params.push(r);
            }
            send(write, Message::with_prefix(prefix, "PART", params)).await?;
        }
        FromMatrix::RoomRemoved { chan } => {
            if !s.joined.remove(&chan) { return Ok(()); }
            let Some(n) = s.nick.as_deref() else { return Ok(()); };
            send(write, Message::with_prefix(user_prefix(n), "PART", vec![chan])).await?;
        }
        FromMatrix::DmRemoved { nick: peer } => {
            if !s.registered { return Ok(()); }
            if let Some(n) = s.nick.as_deref() {
                matrirc_notice(write, n, &format!("DM with {peer} ended (left on Matrix)")).await?;
            }
        }
    }
    Ok(())
}

async fn handle_command(
    write: &mut (impl tokio::io::AsyncWrite + Unpin),
    peer: &SocketAddr,
    bridge: &Bridge,
    msg: &Message,
    s: &mut State,
) -> Result<bool> {
    let p0 = msg.params.first().map(String::as_str);
    match msg.command.as_str() {
        "CAP" => handle_cap(write, msg, &mut s.caps).await?,
        "NICK" => if let Some(n) = p0 {
            let new = n.to_string();
            let old = s.nick.replace(new.clone());
            if s.registered {
                if let Some(prev) = old.filter(|p| p != &new) {
                    let prefix = format!("{prev}!{prev}@matrirc.local");
                    send(write, Message::with_prefix(&prefix, "NICK", vec![new.clone()])).await?;
                    let _ = bridge.to_matrix.try_send(ToMatrix::SetDisplayName { name: new });
                }
            }
        },
        "USER" => if let Some(u) = p0 { s.user = Some(u.into()); },
        "PING" => send(write, srv("PONG", vec![SERVER_NAME.into(), p0.unwrap_or("").into()])).await?,
        "JOIN" => if let Some(n) = s.nick.clone() {
            handle_join(write, &n, msg, &mut s.joined, bridge, &s.caps, &mut s.reply_ids, s.show_reply_ids).await?;
        },
        "PART" => if let Some(n) = s.nick.clone() {
            handle_part(write, &n, msg, &mut s.joined, bridge).await?;
        },
        "PRIVMSG" => if let Some(n) = s.nick.clone() {
            handle_privmsg(write, &n, msg, bridge, s).await?;
        },
        "WHOIS" => if let Some(n) = s.nick.clone() {
            handle_whois(write, &n, msg, bridge).await?;
        },
        "TOPIC" => if let Some(n) = s.nick.clone() {
            handle_topic(write, &n, msg, bridge).await?;
        },
        "LIST" => if let Some(n) = s.nick.clone() {
            handle_list(write, &n, bridge).await?;
        },
        "NAMES" => if let Some(n) = s.nick.clone() {
            handle_names(write, &n, msg, bridge).await?;
        },
        "WHO" => if let Some(n) = s.nick.clone() {
            handle_who(write, &n, msg, bridge).await?;
        },
        "LINKS" => if let Some(n) = s.nick.clone() {
            handle_links(write, &n).await?;
        },
        "ADMIN" => if let Some(n) = s.nick.clone() {
            handle_admin(write, &n).await?;
        },
        "INFO" => if let Some(n) = s.nick.clone() {
            handle_info(write, &n).await?;
        },
        "NOTICE" => if let Some(n) = s.nick.clone() {
            handle_notice(write, &n, msg, bridge).await?;
        },
        "QUIT" => {
            let _ = write.shutdown().await;
            info!(%peer, "client quit");
            return Ok(true);
        }
        other => debug!(%peer, %other, "unsupported"),
    }
    Ok(false)
}

async fn handle_cap(
    write: &mut (impl tokio::io::AsyncWrite + Unpin),
    msg: &Message,
    caps_enabled: &mut HashSet<String>,
) -> Result<()> {
    match msg.params.first().map(String::as_str) {
        Some("LS") => {
            let advertised = SUPPORTED_CAPS.join(" ");
            send(write, srv("CAP", vec!["*".into(), "LS".into(), advertised])).await?;
        }
        Some("END") => {}
        Some("LIST") => {
            let active: Vec<&str> = caps_enabled.iter().map(String::as_str).collect();
            send(
                write,
                srv("CAP", vec!["*".into(), "LIST".into(), active.join(" ")]),
            )
            .await?;
        }
        Some("REQ") => {
            let requested = msg.params.get(1).cloned().unwrap_or_default();
            let caps: Vec<&str> = requested.split_whitespace().collect();
            let all_supported = caps.iter().all(|c| {
                SUPPORTED_CAPS.contains(c) || SUPPORTED_CAPS.contains(&c.trim_start_matches('-'))
            });
            let verb = if all_supported { "ACK" } else { "NAK" };
            // server-time depends on message-tags per IRCv3 — irssi 1.4.5 only
            // requests `server-time` and then can't parse the @time= prefix,
            // so silently bundle message-tags into the ACK.
            let mut ack_caps: Vec<String> = caps.iter().map(|c| (*c).to_string()).collect();
            if all_supported
                && caps.contains(&SERVER_TIME_CAP)
                && !caps.contains(&MESSAGE_TAGS_CAP)
            {
                ack_caps.push(MESSAGE_TAGS_CAP.to_string());
            }
            if all_supported {
                for c in &ack_caps {
                    if let Some(removed) = c.strip_prefix('-') {
                        caps_enabled.remove(removed);
                    } else {
                        caps_enabled.insert(c.clone());
                    }
                }
            }
            let ack_payload = if all_supported { ack_caps.join(" ") } else { requested };
            send(
                write,
                srv("CAP", vec!["*".into(), verb.into(), ack_payload]),
            )
            .await?;
        }
        _ => debug!(?msg, "ignoring CAP subcommand"),
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_join(
    write: &mut (impl tokio::io::AsyncWrite + Unpin),
    nick: &str,
    msg: &Message,
    joined: &mut HashSet<String>,
    bridge: &Bridge,
    caps: &HashSet<String>,
    reply_ids: &mut ReplyRing,
    show_reply_ids: bool,
) -> Result<()> {
    let Some(target) = msg.params.first() else { return Ok(()); };
    for chan in target.split(',') {
        let chan = chan.trim();
        if joined.contains(chan) {
            continue;
        }
        if chan == ECHO_CHAN {
            join_echo(write, nick, joined).await?;
            continue;
        }
        if let Some(room) = bridge.room_for(chan) {
            // User may have typed an alias; the bridge stores by slug. Redirect.
            let canonical = bridge.chan_for(&room).unwrap_or_else(|| chan.to_string());
            if canonical != chan {
                matrirc_notice(write, nick,
                    &format!("'{chan}' is bridged as '{canonical}'")).await?;
            }
            if joined.insert(canonical.clone()) {
                let topic = bridge.topic_for(&canonical).unwrap_or_default();
                join_bridged(write, nick, &canonical, &room, &topic, bridge, caps, reply_ids, show_reply_ids).await?;
            }
            continue;
        }
        if is_matrix_alias(chan) {
            if let Err(e) = request_join_by_alias(write, nick, chan, bridge).await {
                warn!(%chan, "join-by-alias dispatch: {e}");
            }
            continue;
        }
        send(write, srv(rpl::ERR_NOSUCHCHANNEL, vec![nick.into(), chan.into(), "No such channel".into()])).await?;
    }
    Ok(())
}

fn is_matrix_alias(target: &str) -> bool {
    target.starts_with('#') && target.contains(':')
}

async fn request_join_by_alias(
    write: &mut (impl tokio::io::AsyncWrite + Unpin),
    nick: &str,
    alias: &str,
    bridge: &Bridge,
) -> Result<()> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    bridge
        .to_matrix
        .try_send(ToMatrix::JoinByAlias { alias: alias.to_string(), reply: tx })
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    matrirc_notice(write, nick, &format!("joining {alias} ...")).await?;
    tokio::spawn({
        let nick = nick.to_string();
        let alias = alias.to_string();
        async move {
            match rx.await {
                Ok(Ok(chan)) => tracing::info!(%nick, %alias, %chan, "joined via alias"),
                Ok(Err(e)) => tracing::warn!(%nick, %alias, "join failed: {e}"),
                Err(_) => tracing::warn!(%nick, %alias, "join reply dropped"),
            }
        }
    });
    Ok(())
}

async fn join_echo(
    write: &mut (impl tokio::io::AsyncWrite + Unpin),
    nick: &str,
    joined: &mut HashSet<String>,
) -> Result<()> {
    send_join(write, nick, ECHO_CHAN, ECHO_TOPIC, &[ECHO_NICK]).await?;
    joined.insert(ECHO_CHAN.to_string());
    Ok(())
}

#[allow(clippy::too_many_arguments)] // wraps existing wide signature plus reply-id state.
async fn join_bridged(
    write: &mut (impl tokio::io::AsyncWrite + Unpin),
    nick: &str,
    chan: &str,
    room: &matrix_sdk::ruma::RoomId,
    topic: &str,
    bridge: &Bridge,
    caps: &HashSet<String>,
    reply_ids: &mut ReplyRing,
    show_reply_ids: bool,
) -> Result<()> {
    let members = fetch_members(bridge, room).await;
    let names: Vec<&str> = members.iter().map(String::as_str).collect();
    send_join(write, nick, chan, topic, &names).await?;
    backfill_channel(write, chan, room, bridge, caps, reply_ids, show_reply_ids).await?;
    Ok(())
}

async fn send_join(
    write: &mut (impl tokio::io::AsyncWrite + Unpin),
    nick: &str,
    chan: &str,
    topic: &str,
    members: &[&str],
) -> Result<()> {
    send(write, Message::with_prefix(user_prefix(nick), "JOIN", vec![chan.into()])).await?;
    if topic.is_empty() {
        send(write, srv(rpl::NOTOPIC, vec![nick.into(), chan.into(), "No topic is set".into()])).await?;
    } else {
        send(write, srv(rpl::TOPIC, vec![nick.into(), chan.into(), topic.into()])).await?;
    }
    send_names(write, nick, chan, members).await?;
    Ok(())
}

async fn send_names(
    write: &mut (impl tokio::io::AsyncWrite + Unpin),
    nick: &str,
    chan: &str,
    members: &[&str],
) -> Result<()> {
    let mut names: Vec<&str> = members.to_vec();
    if !names.contains(&nick) {
        names.push(nick);
    }
    // IRC line limit is 512 bytes including prefix/CRLF. Batch 353 payloads.
    const BATCH_BYTES: usize = 400;
    let mut line = String::new();
    for n in &names {
        if !line.is_empty() && line.len() + 1 + n.len() > BATCH_BYTES {
            send(write, srv(rpl::NAMREPLY, vec![nick.into(), "=".into(), chan.into(), std::mem::take(&mut line)])).await?;
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(n);
    }
    if !line.is_empty() {
        send(write, srv(rpl::NAMREPLY, vec![nick.into(), "=".into(), chan.into(), line])).await?;
    }
    send(write, srv(rpl::ENDOFNAMES, vec![nick.into(), chan.into(), "End of /NAMES list".into()])).await?;
    Ok(())
}

async fn fetch_members(bridge: &Bridge, room: &matrix_sdk::ruma::RoomId) -> Vec<String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    if bridge
        .to_matrix
        .try_send(ToMatrix::Members { room: room.to_owned(), reply: tx })
        .is_err()
    {
        return Vec::new();
    }
    rx.await.unwrap_or_default()
}

async fn auto_join_all(
    write: &mut (impl tokio::io::AsyncWrite + Unpin),
    nick: &str,
    bridge: &Bridge,
    s: &mut State,
) -> Result<()> {
    let channels = bridge.snapshot();
    let dms = bridge.dms();
    if channels.is_empty() && dms.is_empty() {
        matrirc_notice(write, nick, "sync still in progress — channels will auto-join when ready").await?;
        return Ok(());
    }
    info!(%nick, channels = channels.len(), dms = dms.len(), "auto-join");

    let mut new_joins = Vec::new();
    for (chan, room) in &channels {
        if s.joined.contains(chan) { continue; }
        let topic = bridge.topic_for(chan).unwrap_or_default();
        let members = fetch_members(bridge, room).await;
        let member_refs: Vec<&str> = members.iter().map(String::as_str).collect();
        send_join(write, nick, chan, &topic, &member_refs).await?;
        s.joined.insert(chan.clone());
        new_joins.push((chan.clone(), room.clone()));
    }

    let chan_names: Vec<&str> = new_joins.iter().map(|(c, _)| c.as_str()).collect();
    let chan_list = if chan_names.is_empty() { "(none)".to_string() } else { chan_names.join(", ") };
    let dm_list = if dms.is_empty() {
        "(none)".to_string()
    } else {
        dms.iter().map(|(_, n)| n.as_str()).collect::<Vec<_>>().join(", ")
    };
    matrirc_notice(
        write,
        nick,
        &format!("channels: {chan_list}  |  DMs: {dm_list}"),
    ).await?;

    // DMs first: usually small + the user wants to see /query windows populated
    // immediately. A noisy channel's 1000-event backfill would otherwise block
    // DM history for tens of seconds.
    for (room, dm_nick) in dms {
        if s.dm_backfilled.insert(room.clone()) {
            let _ = dm_nick; // peer nick lands in backfill's own prefixes
            backfill_channel(write, nick, &room, bridge, &s.caps, &mut s.reply_ids, s.show_reply_ids).await?;
        }
    }
    for (chan, room) in new_joins {
        backfill_channel(write, &chan, &room, bridge, &s.caps, &mut s.reply_ids, s.show_reply_ids).await?;
    }
    Ok(())
}

async fn bot_line(write: &mut (impl tokio::io::AsyncWrite + Unpin), nick: &str, cmd: &str, body: &str) -> Result<()> {
    send(
        write,
        Message::with_prefix(BOT_PREFIX, cmd, vec![nick.into(), body.into()]),
    )
    .await
}

async fn matrirc_notice(write: &mut (impl tokio::io::AsyncWrite + Unpin), nick: &str, body: &str) -> Result<()> {
    bot_line(write, nick, "NOTICE", body).await
}

async fn matrirc_msg(write: &mut (impl tokio::io::AsyncWrite + Unpin), nick: &str, body: &str) -> Result<()> {
    bot_line(write, nick, "PRIVMSG", body).await
}

async fn handle_bot_command(
    write: &mut (impl tokio::io::AsyncWrite + Unpin),
    nick: &str,
    text: &str,
    bridge: &Bridge,
    s: &mut State,
) -> Result<()> {
    let cmd = text.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
    match cmd.as_str() {
        "" | "help" | "?" => {
            matrirc_msg(write, nick, "matrirc — local Matrix↔IRC bridge").await?;
            matrirc_msg(write, nick, "").await?;
            matrirc_msg(write, nick, "bot commands (to this nick):").await?;
            for line in BOT_CMDS {
                matrirc_msg(write, nick, &format!("  {line}")).await?;
            }
            matrirc_msg(write, nick, "").await?;
            matrirc_msg(write, nick, "IRC → Matrix:").await?;
            matrirc_msg(write, nick, "  /join #alias:server.org       join any public Matrix room").await?;
            matrirc_msg(write, nick, "  /msg @alice:server.org hi     open/create a DM").await?;
            matrirc_msg(write, nick, "  /msg <known-dm-nick> hi       existing DM (see `dms`)").await?;
            matrirc_msg(write, nick, "  /part #channel                leave the IRC channel (Matrix room keeps you)").await?;
            matrirc_msg(write, nick, "  /me does a thing              m.emote").await?;
            matrirc_msg(write, nick, "  !r <id> text                  reply to message [id] in this window").await?;
            matrirc_msg(write, nick, "").await?;
            matrirc_msg(write, nick, "daemon control (in your shell, not here):").await?;
            matrirc_msg(write, nick, "  matrirc status | stop | verify | reset").await?;
            matrirc_msg(write, nick, "docs: https://github.com/pawelb0/matrirc").await?;
        }
        "search" => {
            let rest = text.split_whitespace().skip(1).collect::<Vec<_>>().join(" ");
            let (query, server) = match rest.rsplit_once(" on ") {
                Some((q, s)) => (q.trim().to_string(), Some(s.trim().to_string())),
                None => (rest.trim().to_string(), None),
            };
            if query.is_empty() {
                matrirc_msg(write, nick, "usage: search <term> [on <server>]").await?;
                return Ok(());
            }
            let (tx, rx) = tokio::sync::oneshot::channel();
            if bridge.to_matrix.try_send(ToMatrix::SearchRooms { query, server, reply: tx }).is_err() {
                matrirc_msg(write, nick, "search dispatch failed").await?;
                return Ok(());
            }
            let rows = rx.await.unwrap_or_default();
            if rows.is_empty() {
                matrirc_msg(write, nick, "no matches").await?;
            } else {
                matrirc_msg(write, nick, &format!("{} result(s):", rows.len())).await?;
                for r in rows.iter().take(15) {
                    let alias = r.alias.as_deref().unwrap_or(&r.room_id);
                    matrirc_msg(write, nick, &format!("  {alias}  ({} members) — {}", r.members, r.name)).await?;
                }
                matrirc_msg(write, nick, "join with: /join #alias:server.org").await?;
            }
        }
        "rooms" => {
            let mut rows = bridge.snapshot();
            rows.sort_by(|a, b| a.0.cmp(&b.0));
            if rows.is_empty() {
                matrirc_msg(write, nick, "no channels bridged yet (sync still running?)").await?;
            } else {
                matrirc_msg(write, nick, &format!("{} channel(s):", rows.len())).await?;
                for (chan, room) in rows {
                    matrirc_msg(write, nick, &format!("  {chan}  →  {room}")).await?;
                }
            }
        }
        "dms" => {
            let nicks = bridge.dm_nicks();
            if nicks.is_empty() {
                matrirc_msg(write, nick, "no DMs registered").await?;
            } else {
                matrirc_msg(write, nick, &format!("{} DM(s):", nicks.len())).await?;
                for n in nicks {
                    matrirc_msg(write, nick, &format!("  /msg {n}")).await?;
                }
            }
        }
        "version" => {
            matrirc_msg(
                write,
                nick,
                concat!("matrirc ", env!("CARGO_PKG_VERSION"), " (matrix-sdk 0.14, rustls)"),
            )
            .await?;
        }
        "knock" => {
            let rest = text.split_whitespace().skip(1).collect::<Vec<_>>().join(" ");
            let (target, reason) = match rest.split_once(' ') {
                Some((t, r)) => {
                    let r = r.trim().to_string();
                    (t.trim().to_string(), if r.is_empty() { None } else { Some(r) })
                }
                None => (rest.trim().to_string(), None),
            };
            if target.is_empty() {
                matrirc_msg(write, nick, "usage: knock <!room:server or #alias:server> [reason]").await?;
                return Ok(());
            }
            let (tx, rx) = tokio::sync::oneshot::channel();
            if bridge.to_matrix.try_send(ToMatrix::Knock { target, reason, reply: tx }).is_err() {
                matrirc_msg(write, nick, "knock dispatch failed").await?;
                return Ok(());
            }
            match rx.await {
                Ok(Ok(msg)) => matrirc_msg(write, nick, &msg).await?,
                Ok(Err(e)) => matrirc_msg(write, nick, &format!("knock failed: {e}")).await?,
                Err(_) => matrirc_msg(write, nick, "knock cancelled").await?,
            }
        }
        "join" => {
            let arg = text.split_whitespace().nth(1).unwrap_or("");
            if arg.is_empty() {
                matrirc_msg(write, nick, "usage: join <!room_id:server or #alias:server>").await?;
                return Ok(());
            }
            let (tx, rx) = tokio::sync::oneshot::channel();
            if bridge.to_matrix.try_send(ToMatrix::JoinByAlias { alias: arg.to_string(), reply: tx }).is_err() {
                matrirc_msg(write, nick, "join dispatch failed").await?;
                return Ok(());
            }
            match rx.await {
                Ok(Ok(chan)) => matrirc_msg(write, nick, &format!("joined {chan}")).await?,
                Ok(Err(e)) => matrirc_msg(write, nick, &format!("join failed: {e}")).await?,
                Err(_) => matrirc_msg(write, nick, "join cancelled").await?,
            }
        }
        "dump" => {
            let target = text.split_whitespace().nth(1).unwrap_or("").to_ascii_lowercase();
            if target.is_empty() {
                if s.reply_ids.is_empty() {
                    matrirc_msg(write, nick, "no reply-id rings yet (join a channel or DM)").await?;
                } else {
                    matrirc_msg(write, nick, "windows with reply-id rings:").await?;
                    let mut keys: Vec<_> = s.reply_ids.iter().collect();
                    keys.sort_by(|a, b| a.0.cmp(b.0));
                    for (k, q) in keys {
                        matrirc_msg(write, nick, &format!("  {k}  ({} entries)", q.len())).await?;
                    }
                    matrirc_msg(write, nick, "usage: dump <window>").await?;
                }
                return Ok(());
            }
            match s.reply_ids.get(&target) {
                Some(q) => {
                    matrirc_msg(write, nick,
                        &format!("ring {target} entries={}/{REPLY_RING_CAP}", q.len())).await?;
                    for (short, eid, sender, snip) in q.iter().rev() {
                        matrirc_msg(write, nick,
                            &format!("  [{short}] {sender}: {snip}  ({eid})")).await?;
                    }
                }
                None => {
                    matrirc_msg(write, nick, &format!("no ring for '{target}'")).await?;
                    if !s.reply_ids.is_empty() {
                        let mut keys: Vec<&String> = s.reply_ids.keys().collect();
                        keys.sort();
                        let joined = keys.iter().map(|k| k.as_str()).collect::<Vec<_>>().join(", ");
                        matrirc_msg(write, nick, &format!("known: {joined}")).await?;
                    }
                }
            }
        }
        "ids" => {
            let arg = text.split_whitespace().nth(1).unwrap_or("").to_ascii_lowercase();
            let new = match arg.as_str() {
                "" | "status" => None,
                "on" | "true" | "1" => Some(true),
                "off" | "false" | "0" => Some(false),
                "toggle" => Some(!s.show_reply_ids),
                _ => {
                    matrirc_msg(write, nick, "usage: ids [on|off|toggle|status]").await?;
                    return Ok(());
                }
            };
            if let Some(v) = new { s.show_reply_ids = v; }
            let state = if s.show_reply_ids { "on" } else { "off" };
            matrirc_msg(write, nick,
                &format!("reply ids: {state} (IRCv3 msgid tag; new messages only)")).await?;
        }
        other => {
            matrirc_msg(write, nick, &format!("unknown command: {other}  (try `help`)")).await?;
        }
    }
    Ok(())
}

/// Messages requested per JOIN / DM open. The server returns up to 100 per
/// page; the backfill loop paginates until this many are collected or the
/// room has no more history.
const BACKFILL_LIMIT: u32 = 1000;

async fn backfill_channel(
    write: &mut (impl tokio::io::AsyncWrite + Unpin),
    chan: &str,
    room: &matrix_sdk::ruma::RoomId,
    bridge: &Bridge,
    caps: &HashSet<String>,
    reply_ids: &mut ReplyRing,
    show_reply_ids: bool,
) -> Result<()> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    if bridge
        .to_matrix
        .try_send(ToMatrix::Backfill {
            room: room.to_owned(),
            limit: BACKFILL_LIMIT,
            reply: tx,
        })
        .is_err()
    {
        warn!(%chan, "backfill: channel full or matrix sync down");
        return Ok(());
    }
    let msgs = match rx.await {
        Ok(m) => m,
        Err(_) => return Ok(()),
    };
    let server_time = caps.contains(SERVER_TIME_CAP);
    // `chan` is either `#channel` or the user's own nick (DM case).
    let is_dm = !chan.starts_with('#');
    let peer_nick = if is_dm { bridge.dm_nick_for(room) } else { None };

    // Ring key matches the irssi window name: chan for channels, peer for DMs.
    let ring_key: &str = peer_nick.as_deref().unwrap_or(chan);
    let tags_cap = caps.contains(MESSAGE_TAGS_CAP);
    for m in msgs {
        // For DM own-messages: ZNC-style replay → source=self, target=peer.
        // irssi renders these as outgoing in the peer's query window even
        // without echo-message cap.
        let (prefix, target): (String, &str) = if is_dm && m.is_own {
            let Some(ref peer) = peer_nick else { continue; };
            (format!("{chan}!{chan}@matrirc.local"), peer.as_str())
        } else {
            (format!("{}!{0}@matrix", m.sender_nick), chan)
        };
        let time_tag = server_time.then(|| ms_to_iso(m.origin_ms)).flatten();
        if let Some(ref q) = m.reply_quote {
            let mut out = Message::with_prefix(&prefix, "PRIVMSG", vec![target.into(), q.clone()]);
            if let Some(ref iso) = time_tag {
                out = out.with_tag("time", iso.clone());
            }
            send(write, out).await?;
        }
        let (msgid, pieces) = make_payloads(
            &m.body, &m.sender_nick, Some(m.event_id.clone()), ring_key, reply_ids, show_reply_ids,
        );
        for (i, payload) in pieces.into_iter().enumerate() {
            let mut out = Message::with_prefix(&prefix, "PRIVMSG", vec![target.into(), payload]);
            if let Some(ref iso) = time_tag {
                out = out.with_tag("time", iso.clone());
            }
            if i == 0 && tags_cap {
                if let Some(id) = &msgid {
                    out = out.with_tag("msgid", id.clone());
                }
            }
            send(write, out).await?;
        }
    }
    Ok(())
}

fn ms_to_iso(ms: i64) -> Option<String> {
    let nanos = i128::from(ms).checked_mul(1_000_000)?;
    OffsetDateTime::from_unix_timestamp_nanos(nanos)
        .ok()?
        .format(ISO_FMT)
        .ok()
}

async fn handle_whois(
    write: &mut (impl tokio::io::AsyncWrite + Unpin),
    nick: &str,
    msg: &Message,
    bridge: &Bridge,
) -> Result<()> {
    // WHOIS [<server>] <nick> — SKIP the optional server hint.
    let target = msg.params.iter().rfind(|p| !p.is_empty()).cloned();
    let Some(target) = target else { return Ok(()); };

    // Local pseudo-users.
    match target.as_str() {
        ECHO_NICK => {
            send_whois(write, nick, ECHO_NICK, "echo", "matrirc.local", "Echo bot", Some("matrirc.local"), &[ECHO_CHAN.to_string()]).await?;
            return Ok(());
        }
        "matrirc" => {
            send_whois(write, nick, "matrirc", "matrirc", "matrirc.local", "matrirc bridge control", Some("matrirc.local"), &[]).await?;
            return Ok(());
        }
        _ => {}
    }

    let (tx, rx) = tokio::sync::oneshot::channel();
    if bridge.to_matrix.try_send(ToMatrix::Whois { nick: target.clone(), reply: tx }).is_err() {
        send(write, srv(rpl::ERR_NOSUCHNICK, vec![nick.into(), target.clone(), "No such nick/channel".into()])).await?;
        send(write, srv(rpl::ENDOFWHOIS, vec![nick.into(), target, "End of /WHOIS list".into()])).await?;
        return Ok(());
    }
    match rx.await.ok().flatten() {
        Some(info) => {
            let realname = match &info.display_name {
                Some(d) if d != &info.nick => format!("{d} ({})", info.mxid),
                _ => info.mxid.clone(),
            };
            let server_hint = info.mxid.rsplit_once(':').map(|(_, s)| s);
            send_whois(write, nick, &info.nick, &info.nick, "matrix", &realname, server_hint, &info.rooms).await?;
        }
        None => {
            send(write, srv(rpl::ERR_NOSUCHNICK, vec![nick.into(), target.clone(), "No such nick/channel".into()])).await?;
            send(write, srv(rpl::ENDOFWHOIS, vec![nick.into(), target, "End of /WHOIS list".into()])).await?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn send_whois(
    write: &mut (impl tokio::io::AsyncWrite + Unpin),
    nick: &str,
    target_nick: &str,
    user: &str,
    host: &str,
    realname: &str,
    server: Option<&str>,
    channels: &[String],
) -> Result<()> {
    send(write, srv(rpl::WHOISUSER, vec![nick.into(), target_nick.into(), user.into(), host.into(), "*".into(), realname.into()])).await?;
    if let Some(s) = server {
        send(write, srv(rpl::WHOISSERVER, vec![nick.into(), target_nick.into(), s.into(), "Matrix homeserver".into()])).await?;
    }
    if !channels.is_empty() {
        send(write, srv(rpl::WHOISCHANNELS, vec![nick.into(), target_nick.into(), channels.join(" ")])).await?;
    }
    send(write, srv(rpl::ENDOFWHOIS, vec![nick.into(), target_nick.into(), "End of /WHOIS list".into()])).await?;
    Ok(())
}

async fn handle_topic(
    write: &mut (impl tokio::io::AsyncWrite + Unpin),
    nick: &str,
    msg: &Message,
    bridge: &Bridge,
) -> Result<()> {
    let Some(chan) = msg.params.first() else { return Ok(()); };
    let Some(room) = bridge.room_for(chan) else {
        return send(write, srv(rpl::ERR_NOSUCHCHANNEL, vec![nick.into(), chan.into(), "No such channel".into()])).await;
    };
    match msg.params.get(1) {
        Some(topic) => {
            let _ = bridge.to_matrix.try_send(ToMatrix::SetTopic { room, topic: topic.clone() });
        }
        None => {
            let topic = bridge.topic_for(chan).unwrap_or_default();
            let code = if topic.is_empty() { "331" } else { "332" };
            send(write, srv(code, vec![nick.into(), chan.into(), topic])).await?;
        }
    }
    Ok(())
}

async fn handle_list(
    write: &mut (impl tokio::io::AsyncWrite + Unpin),
    nick: &str,
    bridge: &Bridge,
) -> Result<()> {
    send(write, srv(rpl::LISTSTART, vec![nick.into(), "Channel".into(), "Users  Name".into()])).await?;
    for (chan, _) in bridge.snapshot() {
        let topic = bridge.topic_for(&chan).unwrap_or_default();
        send(write, srv(rpl::LIST, vec![nick.into(), chan, "0".into(), topic])).await?;
    }
    send(write, srv(rpl::LISTEND, vec![nick.into(), "End of /LIST".into()])).await
}

async fn handle_names(
    write: &mut (impl tokio::io::AsyncWrite + Unpin),
    nick: &str,
    msg: &Message,
    bridge: &Bridge,
) -> Result<()> {
    let Some(chan) = msg.params.first() else { return Ok(()); };
    let members = match bridge.room_for(chan) {
        Some(r) => fetch_members(bridge, &r).await,
        None => Vec::new(),
    };
    let mut line = String::new();
    for m in &members {
        if line.len() + m.len() + 1 > 400 {
            send(write, srv(rpl::NAMREPLY, vec![nick.into(), "=".into(), chan.into(), std::mem::take(&mut line)])).await?;
        }
        if !line.is_empty() { line.push(' '); }
        line.push_str(m);
    }
    if !line.is_empty() {
        send(write, srv(rpl::NAMREPLY, vec![nick.into(), "=".into(), chan.into(), line])).await?;
    }
    send(write, srv(rpl::ENDOFNAMES, vec![nick.into(), chan.into(), "End of /NAMES list".into()])).await
}

async fn handle_who(
    write: &mut (impl tokio::io::AsyncWrite + Unpin),
    nick: &str,
    msg: &Message,
    bridge: &Bridge,
) -> Result<()> {
    let Some(target) = msg.params.first() else { return Ok(()); };
    let members = match bridge.room_for(target) {
        Some(r) => fetch_members(bridge, &r).await,
        None => Vec::new(),
    };
    for m in &members {
        send(write, srv(rpl::WHOREPLY, vec![
            nick.into(), target.into(), m.into(), "matrix".into(),
            SERVER_NAME.into(), m.into(), "H".into(), format!("0 {m}"),
        ])).await?;
    }
    send(write, srv(rpl::ENDOFWHO, vec![nick.into(), target.into(), "End of /WHO list".into()])).await
}


async fn handle_links(write: &mut (impl tokio::io::AsyncWrite + Unpin), nick: &str) -> Result<()> {
    send(write, srv(rpl::LINKS, vec![nick.into(), SERVER_NAME.into(), SERVER_NAME.into(), format!("0 {VERSION}")])).await?;
    send(write, srv(rpl::ENDOFLINKS, vec![nick.into(), "*".into(), "End of /LINKS list".into()])).await
}

async fn handle_admin(write: &mut (impl tokio::io::AsyncWrite + Unpin), nick: &str) -> Result<()> {
    for (code, line) in [
        (rpl::ADMINME, format!("Administrative info about {SERVER_NAME}")),
        (rpl::ADMINLOC1, "matrirc — local Matrix↔IRC bridge".into()),
        (rpl::ADMINLOC2, "https://github.com/pawelb0/matrirc".into()),
        (rpl::ADMINEMAIL, "issues: github.com/pawelb0/matrirc/issues".into()),
    ] {
        send(write, srv(code, vec![nick.into(), line])).await?;
    }
    Ok(())
}

async fn handle_info(write: &mut (impl tokio::io::AsyncWrite + Unpin), nick: &str) -> Result<()> {
    for line in [
        format!("{VERSION} — local Matrix↔IRC bridge"),
        "https://github.com/pawelb0/matrirc".into(),
    ] {
        send(write, srv(rpl::INFO, vec![nick.into(), line])).await?;
    }
    send(write, srv(rpl::ENDOFINFO, vec![nick.into(), "End of /INFO list".into()])).await
}

async fn handle_notice(
    write: &mut (impl tokio::io::AsyncWrite + Unpin),
    nick: &str,
    msg: &Message,
    bridge: &Bridge,
) -> Result<()> {
    let Some(target) = msg.params.first() else { return Ok(()); };
    let Some(body) = msg.params.get(1) else { return Ok(()); };
    let Some(dest) = resolve_send_target(target, bridge) else {
        return no_such(write, nick, target).await;
    };
    let _ = bridge.to_matrix.try_send(make_send_cmd(dest, body.clone(), false, true, None));
    Ok(())
}

async fn handle_part(
    write: &mut (impl tokio::io::AsyncWrite + Unpin),
    nick: &str,
    msg: &Message,
    joined: &mut HashSet<String>,
    bridge: &Bridge,
) -> Result<()> {
    let Some(target) = msg.params.first() else { return Ok(()); };
    let reason = msg.params.get(1).cloned().unwrap_or_default();
    for chan in target.split(',') {
        let chan = chan.trim();
        if !joined.remove(chan) {
            continue;
        }
        let mut params = vec![chan.to_string()];
        if !reason.is_empty() {
            params.push(reason.clone());
        }
        send(write, Message::with_prefix(user_prefix(nick), "PART", params)).await?;

        // Skip the bot/echo channels — they have no Matrix room.
        let Some(room) = bridge.room_for(chan) else { continue; };
        let (tx, rx) = tokio::sync::oneshot::channel();
        if bridge.to_matrix.try_send(ToMatrix::LeaveRoom { room, reply: tx }).is_err() {
            warn!(%chan, "leave dispatch failed (channel full or matrix sync down)");
            continue;
        }
        let chan_log = chan.to_string();
        tokio::spawn(async move {
            match rx.await {
                Ok(Ok(())) => info!(chan = %chan_log, "left matrix room"),
                Ok(Err(e)) => warn!(chan = %chan_log, "leave failed: {e}"),
                Err(_) => warn!(chan = %chan_log, "leave reply dropped"),
            }
        });
    }
    Ok(())
}

async fn handle_privmsg(
    write: &mut (impl tokio::io::AsyncWrite + Unpin),
    nick: &str,
    msg: &Message,
    bridge: &Bridge,
    s: &mut State,
) -> Result<()> {
    let Some(target) = msg.params.first() else { return Ok(()); };
    let Some(raw) = msg.params.get(1) else { return Ok(()); };
    let (body, emote) = strip_ctcp_action(raw);

    if target.eq_ignore_ascii_case("matrirc") {
        if emote { return Ok(()); }
        if let Some(reply) = ctcp_reply_for(body) {
            return send(write, Message::with_prefix(BOT_PREFIX, "NOTICE", vec![nick.into(), format!("\x01{reply}\x01")])).await;
        }
        return handle_bot_command(write, nick, body, bridge, s).await;
    }

    // Reply syntax: `!r <id> <text>` where <id> is a known 4-lowercase-hex
    // short id from the current window's ring. Match + hit → reply. Match +
    // miss → strip the prefix, send body without a reply (so the `!r abcd`
    // cruft doesn't leak into Matrix) and notice the user. No match → send
    // unchanged.
    let (body, reply_target) = match parse_reply_prefix(body) {
        None => (body, None),
        Some((short, rest)) => match lookup_reply_id(&s.reply_ids, target, short) {
            Some(hit) => {
                info!(%target, %short, event_id = %hit.0, "reply lookup hit");
                (rest, Some(hit))
            }
            None => {
                // Mis-typed id → drop the message entirely instead of leaking
                // the intended-as-reply text to the room without context.
                matrirc_notice(write, nick,
                    &format!("reply id [{short}] not in ring for {target} — message dropped"),
                ).await?;
                let _ = rest;
                return Ok(());
            }
        }
    };
    let in_reply_to = reply_target.as_ref().map(|(eid, _, _)| eid.clone());

    if target == ECHO_CHAN || target.eq_ignore_ascii_case(ECHO_NICK) {
        // `dest` is the wire PRIVMSG target (#echo for channel, the user's
        // nick for /msg echo). `ring_key` is the irssi window the user types
        // into — equal to `target`, which is what `parse_reply_prefix` keys
        // off when resolving `!r`.
        let dest: &str = if target == ECHO_CHAN { ECHO_CHAN } else { nick };
        let ring_key: &str = if target == ECHO_CHAN { ECHO_CHAN } else { ECHO_NICK };
        if let Some((_, orig_sender, orig_snippet)) = &reply_target {
            send(write,
                Message::with_prefix(ECHO_PREFIX, "PRIVMSG",
                    vec![dest.into(), format!("\x0314↳ <{orig_sender}> {orig_snippet}\x0f")]),
            ).await?;
        }
        let body_out = format!("echo: {body}");
        let mut out = Message::with_prefix(ECHO_PREFIX, "PRIVMSG", vec![dest.into(), body_out.clone()]);
        if s.show_reply_ids {
            let eid = synth_echo_event_id(&mut s.echo_counter);
            let short = remember_reply_id(&mut s.reply_ids, ring_key, eid,
                ECHO_NICK.to_string(), body_snippet(&body_out));
            if s.caps.contains(MESSAGE_TAGS_CAP) {
                out = out.with_tag("msgid", short);
            }
        }
        send(write, out).await?;
        return Ok(());
    }

    // Reply context: emit the quote line as a PRIVMSG sourced from the
    // user's own nick. irssi renders it inline in the channel they typed in
    // (no echo-message cap needed). Other matrix users never see it because
    // matrirc only sends it down this IRC socket — the actual matrix reply
    // goes through the normal `to_matrix` send below.
    if let Some((_, orig_sender, orig_snippet)) = &reply_target {
        let source = format!("{nick}!{nick}@matrirc.local");
        send(write,
            Message::with_prefix(&source, "PRIVMSG",
                vec![target.clone(), format!("\x0314↳ <{orig_sender}> {orig_snippet}\x0f")]),
        ).await?;
    }

    // IRCv3 echo-message: if the client negotiated it, the client suppresses
    // local echo and waits for the server to bounce back the body.
    if s.caps.contains(ECHO_MESSAGE_CAP) {
        let source = format!("{nick}!{nick}@matrirc.local");
        let wire_body = if emote { format!("\x01ACTION {body}\x01") } else { body.to_string() };
        send(write, Message::with_prefix(&source, "PRIVMSG", vec![target.clone(), wire_body])).await?;
    }

    let Some(dest) = resolve_send_target(target, bridge) else {
        return no_such(write, nick, target).await;
    };
    if let SendTarget::Room(ref room) = dest {
        if let Some(canon) = bridge.dm_nick_for(room) {
            if !target.eq_ignore_ascii_case(&canon) && s.dm_hinted.insert(room.clone()) {
                matrirc_notice(
                    write, nick,
                    &format!("DM peer is '{canon}' — replies land in /query {canon}"),
                ).await?;
            }
        }
    }
    let cmd = make_send_cmd(dest, body.to_string(), emote, false, in_reply_to);
    if let Err(e) = bridge.to_matrix.try_send(cmd) {
        warn!("dropping outbound: {e}");
        send(write, srv("NOTICE", vec![nick.into(), format!("send dropped: {e}")])).await?;
    }
    Ok(())
}

/// Parses a `!r <id> <text>` prefix. Matches only the exact 3-lowercase-letter
/// short id format we emit (`aaa`..`zzz`); anything else passes through.
fn parse_reply_prefix(body: &str) -> Option<(&str, &str)> {
    let (id, text) = body.strip_prefix("!r ")?.split_once(' ')?;
    let is_short = id.len() == 3
        && id.bytes().all(|b| b.is_ascii_lowercase());
    (is_short && !text.is_empty()).then_some((id, text))
}

enum SendTarget {
    Room(matrix_sdk::ruma::OwnedRoomId),
    Mxid(matrix_sdk::ruma::OwnedUserId),
}

fn resolve_send_target(target: &str, bridge: &Bridge) -> Option<SendTarget> {
    if let Some(r) = bridge.room_for(target) { return Some(SendTarget::Room(r)); }
    if let Some(r) = bridge.dm_room_for(target) { return Some(SendTarget::Room(r)); }
    if !target.contains(':') { return None; }
    let canonical = if target.starts_with('@') { target.into() } else { format!("@{target}") };
    matrix_sdk::ruma::OwnedUserId::try_from(canonical.as_str()).ok().map(SendTarget::Mxid)
}

fn make_send_cmd(
    dest: SendTarget,
    body: String,
    emote: bool,
    notice: bool,
    in_reply_to: Option<matrix_sdk::ruma::OwnedEventId>,
) -> ToMatrix {
    match dest {
        SendTarget::Room(room) => ToMatrix::Send { room, body, emote, notice, in_reply_to },
        SendTarget::Mxid(mxid) => ToMatrix::SendToMxid { mxid, body, emote, notice, in_reply_to },
    }
}

fn strip_ctcp_action(text: &str) -> (&str, bool) {
    text.strip_prefix("\x01ACTION ")
        .and_then(|s| s.strip_suffix('\x01'))
        .map(|s| (s, true))
        .unwrap_or((text, false))
}

fn ctcp_reply_for(body: &str) -> Option<String> {
    let inner = body.strip_prefix('\x01')?.strip_suffix('\x01')?;
    let cmd = inner.split_whitespace().next()?.to_ascii_uppercase();
    match cmd.as_str() {
        "VERSION" => Some(format!("VERSION {VERSION}")),
        "PING" => Some(inner.to_string()),
        "TIME" => OffsetDateTime::now_utc().format(ISO_FMT).ok().map(|t| format!("TIME {t}")),
        _ => None,
    }
}

async fn no_such(write: &mut (impl tokio::io::AsyncWrite + Unpin), nick: &str, target: &str) -> Result<()> {
    send(write, srv(rpl::ERR_NOSUCHNICK, vec![nick.into(), target.into(), "No such nick/channel".into()])).await
}

async fn read_line<R: tokio::io::AsyncBufRead + Unpin>(
    lines: &mut tokio::io::Lines<R>,
) -> Result<Option<String>> {
    let Some(line) = lines.next_line().await.context("read line")? else { return Ok(None) };
    if line.len() > MAX_LINE {
        warn!(len = line.len(), "line over {MAX_LINE}; truncating");
        return Ok(Some(line.chars().take(MAX_LINE).collect()));
    }
    Ok(Some(line))
}

const MASCOT: &[&str] = &[
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢀⣤⡀⠀⣿⣿⠆",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢸⡿⠇⠀⣿⠁⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⣸⣧⣴⣶⣿⡀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⣴⣿⣿⣿⣿⣿⣿⡄",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢿⣿⣿⣿⣿⣿⣿⠇",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⣿⣿⣿⣿⣿⠿⠋⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⢀⣾⣿⣿⣿⣿⣿⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⣀⣴⣿⣿⣿⣿⣿⣿⣿⠀⠀⠀",
    "⠀⠀⠀⠀⠀⠀⠀⠀⠀⢀⣀⣤⣶⣿⣿⣿⣿⣿⣿⣿⣿⣿⡏⠀⠀⠀",
    "⠀⠀⣀⣀⣤⣴⣶⣶⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⠟⠀⠀⠀⠀",
    "⠐⠿⢿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⠿⠛⠁⠀⠀⠀⠀⠀",
];

async fn send_welcome(write: &mut (impl tokio::io::AsyncWrite + Unpin), nick: &str) -> Result<()> {
    let n = nick.to_string();
    let header: &[(&str, Vec<String>)] = &[
        (rpl::WELCOME, vec![n.clone(), format!("Welcome to matrirc, {nick}")]),
        (rpl::YOURHOST, vec![n.clone(), format!("Your host is {SERVER_NAME}, running {VERSION}")]),
        (rpl::CREATED, vec![n.clone(), "This server has no creation date".into()]),
        (rpl::MYINFO, vec![n.clone(), SERVER_NAME.into(), VERSION.into(), String::new(), String::new()]),
        (rpl::MOTDSTART, vec![n.clone(), format!("- {SERVER_NAME} Message of the day -")]),
    ];
    for (code, params) in header {
        send(write, srv(code, params.clone())).await?;
    }
    for line in MASCOT {
        send(write, srv(rpl::MOTD, vec![n.clone(), (*line).into()])).await?;
    }
    let mut footer: Vec<(&str, Vec<String>)> = vec![
        (rpl::MOTD, vec![n.clone(), "- Matrix rooms auto-joined after this line.".into()]),
        (rpl::MOTD, vec![n.clone(), "-".into()]),
        (rpl::MOTD, vec![n.clone(), "- Bot commands ( /msg matrirc <cmd> ):".into()]),
    ];
    for line in BOT_CMDS {
        footer.push((rpl::MOTD, vec![n.clone(), format!("-   {line}")]));
    }
    footer.extend([
        (rpl::MOTD, vec![n.clone(), "-".into()]),
        (rpl::MOTD, vec![n.clone(), "- IRC syntax:".into()]),
        (rpl::MOTD, vec![n.clone(), "-   /join #alias:server.org      join matrix room".into()]),
        (rpl::MOTD, vec![n.clone(), "-   /msg @user:server hi         DM by mxid".into()]),
        (rpl::MOTD, vec![n.clone(), "-   /me does a thing             m.emote".into()]),
        (rpl::MOTD, vec![n.clone(), "-   !r <id> text                 reply to msg with that [id]".into()]),
        (rpl::MOTD, vec![n.clone(), format!("-   /join {ECHO_CHAN}                       local echo bot for testing")]),
        (rpl::ENDOFMOTD, vec![n, "End of /MOTD command.".into()]),
    ]);
    for (code, params) in footer {
        send(write, srv(code, params.clone())).await?;
    }
    Ok(())
}

fn srv(command: &str, params: Vec<String>) -> Message {
    Message::with_prefix(SERVER_NAME, command, params)
}

async fn send(write: &mut (impl tokio::io::AsyncWrite + Unpin), msg: Message) -> Result<()> {
    let mut wire = msg.to_wire();
    debug!(out = %wire, "send");
    wire.push_str("\r\n");
    write.write_all(wire.as_bytes()).await.context("write")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::{Mapping, FromMatrix};
    use matrix_sdk::ruma::{OwnedRoomId, RoomId};

    fn room(s: &str) -> OwnedRoomId {
        RoomId::parse(s).unwrap()
    }

    fn registered_state(nick: &str) -> State {
        State {
            nick: Some(nick.into()),
            user: Some(nick.into()),
            registered: true,
            ..State::default()
        }
    }

    async fn drain(buf: Vec<u8>) -> String {
        String::from_utf8(buf).unwrap()
    }

    async fn route(ev: FromMatrix, bridge: &Bridge, s: &mut State) -> String {
        let mut out = Vec::<u8>::new();
        handle_matrix_event(&mut out, bridge, s, ev).await.unwrap();
        drain(out).await
    }

    #[test]
    fn ensure_self_mention_prepends_when_absent() {
        assert_eq!(
            ensure_self_mention("hello there", Some("alice")),
            "alice: hello there"
        );
    }

    #[test]
    fn ensure_self_mention_noop_when_present_as_token() {
        assert_eq!(
            ensure_self_mention("hi alice please", Some("alice")),
            "hi alice please"
        );
        assert_eq!(
            ensure_self_mention("@alice you there?", Some("alice")),
            "@alice you there?"
        );
    }

    #[test]
    fn ensure_self_mention_ignores_substring_match() {
        assert_eq!(
            ensure_self_mention("malice in wonderland", Some("alice")),
            "alice: malice in wonderland"
        );
    }

    #[test]
    fn ensure_self_mention_case_insensitive() {
        assert_eq!(
            ensure_self_mention("Hi Alice!", Some("alice")),
            "Hi Alice!"
        );
    }

    #[tokio::test]
    async fn channel_mention_self_prepends_nick_when_absent() {
        let (b, _rx) = Bridge::new(Mapping::default());
        let r = room("!a:server");
        b.add_mapping(r.clone(), "#room-a".into(), "topic".into(), &[]);
        let mut s = registered_state("alice");
        s.joined.insert("#room-a".into());
        let out = route(
            FromMatrix::Message {
                room: r,
                sender_nick: "bob".into(),
                body: "did you see that?".into(),
                event_id: None,
                reply_quote: None,
                is_own: false,
                mentions_self: true,
            },
            &b, &mut s,
        ).await;
        assert!(
            out.contains("PRIVMSG #room-a :alice: did you see that?"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn channel_mention_self_skipped_when_nick_already_present() {
        let (b, _rx) = Bridge::new(Mapping::default());
        let r = room("!a:server");
        b.add_mapping(r.clone(), "#room-a".into(), "topic".into(), &[]);
        let mut s = registered_state("alice");
        s.joined.insert("#room-a".into());
        let out = route(
            FromMatrix::Message {
                room: r,
                sender_nick: "bob".into(),
                body: "alice: ping".into(),
                event_id: None,
                reply_quote: None,
                is_own: false,
                mentions_self: true,
            },
            &b, &mut s,
        ).await;
        assert!(out.contains("PRIVMSG #room-a :alice: ping"), "{out}");
        assert!(!out.contains("alice: alice:"), "{out}");
    }

    #[tokio::test]
    async fn channel_peer_message() {
        let (b, _rx) = Bridge::new(Mapping::default());
        let r = room("!a:server");
        b.add_mapping(r.clone(), "#room-a".into(), "topic".into(), &[]);
        let mut s = registered_state("alice");
        s.joined.insert("#room-a".into());
        let out = route(
            FromMatrix::Message { room: r, sender_nick: "alice".into(), body: "hi".into(), event_id: None, reply_quote: None, is_own: false, mentions_self: false },
            &b, &mut s,
        ).await;
        assert_eq!(out, ":alice!alice@matrix PRIVMSG #room-a hi\r\n");
    }

    #[tokio::test]
    async fn dm_peer_message_targets_own_nick() {
        let (b, _rx) = Bridge::new(Mapping::default());
        let r = room("!dm:server");
        {
            let mut m = b.mapping.write().unwrap();
            m.insert_dm(r.clone(), "bob", &[]);
        }
        let mut s = registered_state("alice");
        let out = route(
            FromMatrix::Message { room: r, sender_nick: "bob".into(), body: "yo".into(), event_id: None, reply_quote: None, is_own: false, mentions_self: false },
            &b, &mut s,
        ).await;
        assert_eq!(out, ":bob!bob@matrix PRIVMSG alice yo\r\n");
    }

    #[tokio::test]
    async fn dm_own_message_routes_znc_style() {
        let (b, _rx) = Bridge::new(Mapping::default());
        let r = room("!dm:server");
        {
            let mut m = b.mapping.write().unwrap();
            m.insert_dm(r.clone(), "bob", &[]);
        }
        let mut s = registered_state("alice");
        let out = route(
            FromMatrix::Message { room: r, sender_nick: "alice".into(), body: "from other device".into(), event_id: None, reply_quote: None, is_own: true, mentions_self: false },
            &b, &mut s,
        ).await;
        assert_eq!(out, ":alice!alice@matrirc.local PRIVMSG bob :from other device\r\n");
    }

    #[tokio::test]
    async fn unknown_room_is_dropped() {
        let (b, _rx) = Bridge::new(Mapping::default());
        let mut s = registered_state("alice");
        let out = route(
            FromMatrix::Message { room: room("!nope:server"), sender_nick: "alice".into(), body: "hi".into(), event_id: None, reply_quote: None, is_own: false, mentions_self: false },
            &b, &mut s,
        ).await;
        assert!(out.is_empty(), "expected empty, got {out:?}");
    }

    #[tokio::test]
    async fn channel_not_joined_is_dropped() {
        let (b, _rx) = Bridge::new(Mapping::default());
        let r = room("!a:server");
        b.add_mapping(r.clone(), "#room-a".into(), "".into(), &[]);
        let mut s = registered_state("alice"); // no joined insert
        let out = route(
            FromMatrix::Message { room: r, sender_nick: "alice".into(), body: "hi".into(), event_id: None, reply_quote: None, is_own: false, mentions_self: false },
            &b, &mut s,
        ).await;
        assert!(out.is_empty(), "expected empty, got {out:?}");
    }

    #[tokio::test]
    async fn topic_changed_emits_topic() {
        let (b, _rx) = Bridge::new(Mapping::default());
        let mut s = registered_state("alice");
        s.joined.insert("#room-a".into());
        let out = route(
            FromMatrix::TopicChanged { chan: "#room-a".into(), topic: "new topic".into() },
            &b, &mut s,
        ).await;
        assert_eq!(out, ":matrirc.local TOPIC #room-a :new topic\r\n");
    }

    #[tokio::test]
    async fn dm_added_emits_notice() {
        let (b, _rx) = Bridge::new(Mapping::default());
        let mut s = registered_state("alice");
        let out = route(
            FromMatrix::DmAdded { nick: "bob".into() },
            &b, &mut s,
        ).await;
        assert!(out.contains("NOTICE alice"), "{out}");
        assert!(out.contains("bob"), "{out}");
    }

    async fn dispatch(
        cmd: &str,
        bridge: &Bridge,
        s: &mut State,
    ) -> String {
        let peer: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let msg = Message::parse(cmd).unwrap();
        let mut out = Vec::<u8>::new();
        handle_command(&mut out, &peer, bridge, &msg, s).await.unwrap();
        drain(out).await
    }

    #[tokio::test]
    async fn ping_replies_pong() {
        let (b, _rx) = Bridge::new(Mapping::default());
        let mut s = registered_state("alice");
        let out = dispatch("PING :abc", &b, &mut s).await;
        assert_eq!(out, ":matrirc.local PONG matrirc.local abc\r\n");
    }

    #[tokio::test]
    async fn join_echo_channel_emits_ack_topic_names() {
        let (b, _rx) = Bridge::new(Mapping::default());
        let mut s = registered_state("alice");
        let out = dispatch("JOIN #echo", &b, &mut s).await;
        assert!(out.contains(":alice!alice@matrirc.local JOIN #echo"), "{out}");
        assert!(out.contains(":matrirc.local 332 alice #echo"), "{out}");
        assert!(out.contains(" echo"), "names list should include echo: {out}");
        assert!(out.contains(":matrirc.local 366 alice #echo"), "{out}");
        assert!(s.joined.contains("#echo"));
    }

    #[tokio::test]
    async fn join_unknown_channel_returns_403() {
        let (b, _rx) = Bridge::new(Mapping::default());
        let mut s = registered_state("alice");
        let out = dispatch("JOIN #nope", &b, &mut s).await;
        assert!(out.contains(":matrirc.local 403 alice #nope"), "{out}");
    }

    #[tokio::test]
    async fn privmsg_echo_channel_echoes_back() {
        let (b, _rx) = Bridge::new(Mapping::default());
        let mut s = registered_state("alice");
        let out = dispatch("PRIVMSG #echo :hi there", &b, &mut s).await;
        assert!(out.contains(":echo!echo@matrirc.local PRIVMSG #echo :echo: hi there"), "{out}");
    }

    #[tokio::test]
    async fn privmsg_to_unknown_returns_401() {
        let (b, _rx) = Bridge::new(Mapping::default());
        let mut s = registered_state("alice");
        let out = dispatch("PRIVMSG nobody :hello", &b, &mut s).await;
        assert!(out.contains(":matrirc.local 401 alice nobody"), "{out}");
    }

    #[tokio::test]
    async fn privmsg_to_bot_help_lists_commands() {
        let (b, _rx) = Bridge::new(Mapping::default());
        let mut s = registered_state("alice");
        let out = dispatch("PRIVMSG matrirc :help", &b, &mut s).await;
        assert!(out.contains("matrirc — local Matrix↔IRC bridge"), "{out}");
        assert!(out.contains("help"), "{out}");
        assert!(out.contains("rooms"), "{out}");
    }

    #[tokio::test]
    async fn whois_echo_returns_311_and_318() {
        let (b, _rx) = Bridge::new(Mapping::default());
        let mut s = registered_state("alice");
        let out = dispatch("WHOIS echo", &b, &mut s).await;
        assert!(out.contains(":matrirc.local 311 alice echo"), "{out}");
        assert!(out.contains(":matrirc.local 318 alice echo"), "{out}");
    }

    #[tokio::test]
    async fn ctcp_action_to_bot_is_silent() {
        let (b, _rx) = Bridge::new(Mapping::default());
        let mut s = registered_state("alice");
        let out = dispatch("PRIVMSG matrirc :\x01ACTION waves\x01", &b, &mut s).await;
        assert!(out.is_empty(), "bot should not emit for CTCP: {out:?}");
    }

    #[tokio::test]
    async fn nick_post_registration_echoes_and_routes_to_matrix() {
        let (b, mut rx) = Bridge::new(Mapping::default());
        let mut s = registered_state("alice");
        let out = dispatch("NICK newnick", &b, &mut s).await;
        assert_eq!(out, ":alice!alice@matrirc.local NICK newnick\r\n");
        assert_eq!(s.nick.as_deref(), Some("newnick"));
        match rx.try_recv() {
            Ok(ToMatrix::SetDisplayName { name }) => assert_eq!(name, "newnick"),
            other => panic!("expected SetDisplayName, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn nick_same_value_is_noop() {
        let (b, mut rx) = Bridge::new(Mapping::default());
        let mut s = registered_state("alice");
        let out = dispatch("NICK alice", &b, &mut s).await;
        assert!(out.is_empty(), "{out:?}");
        assert!(rx.try_recv().is_err(), "no SetDisplayName for same nick");
    }

    #[tokio::test]
    async fn multi_line_body_flattens_to_single_privmsg() {
        let (b, _rx) = Bridge::new(Mapping::default());
        let r = room("!a:server");
        b.add_mapping(r.clone(), "#room-a".into(), "".into(), &[]);
        let mut s = registered_state("alice");
        s.joined.insert("#room-a".into());
        let out = route(
            FromMatrix::Message { room: r, sender_nick: "alice".into(), body: "one\ntwo".into(), event_id: None, reply_quote: None, is_own: false, mentions_self: false },
            &b, &mut s,
        ).await;
        assert_eq!(out, ":alice!alice@matrix PRIVMSG #room-a :one ↵ two\r\n");
    }

    #[tokio::test]
    async fn member_joined_emits_irc_join() {
        let (b, _rx) = Bridge::new(Mapping::default());
        let mut s = registered_state("alice");
        s.joined.insert("#room-a".into());
        let out = route(
            FromMatrix::MemberJoined { chan: "#room-a".into(), nick: "alice".into() },
            &b, &mut s,
        ).await;
        assert_eq!(out, ":alice!alice@matrix JOIN #room-a\r\n");
    }

    #[tokio::test]
    async fn member_left_emits_irc_part_with_reason() {
        let (b, _rx) = Bridge::new(Mapping::default());
        let mut s = registered_state("alice");
        s.joined.insert("#room-a".into());
        let out = route(
            FromMatrix::MemberLeft { chan: "#room-a".into(), nick: "alice".into(), reason: Some("see you later".into()) },
            &b, &mut s,
        ).await;
        assert_eq!(out, ":alice!alice@matrix PART #room-a :see you later\r\n");
    }

    #[tokio::test]
    async fn member_left_without_reason_omits_trailing() {
        let (b, _rx) = Bridge::new(Mapping::default());
        let mut s = registered_state("alice");
        s.joined.insert("#room-a".into());
        let out = route(
            FromMatrix::MemberLeft { chan: "#room-a".into(), nick: "alice".into(), reason: None },
            &b, &mut s,
        ).await;
        assert_eq!(out, ":alice!alice@matrix PART #room-a\r\n");
    }

    #[tokio::test]
    async fn member_event_unjoined_chan_is_dropped() {
        let (b, _rx) = Bridge::new(Mapping::default());
        let mut s = registered_state("alice");
        let out = route(
            FromMatrix::MemberJoined { chan: "#room-a".into(), nick: "alice".into() },
            &b, &mut s,
        ).await;
        assert!(out.is_empty(), "{out}");
    }

    #[tokio::test]
    async fn auto_join_sends_real_members_in_names() {
        use crate::bridge::ToMatrix;
        let (b, mut rx) = Bridge::new(Mapping::default());
        let r = room("!a:server");
        b.add_mapping(r.clone(), "#room-a".into(), "topic".into(), &[]);

        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if let ToMatrix::Members { reply, .. } = msg {
                    let _ = reply.send(vec!["alice".into(), "bob".into()]);
                }
            }
        });

        let mut s = State {
            nick: Some("carol".into()),
            user: Some("carol".into()),
            ..State::default()
        };

        let mut out = Vec::<u8>::new();
        auto_join_all(&mut out, "carol", &b, &mut s).await.unwrap();
        let out = String::from_utf8(out).unwrap();

        assert!(out.contains("353 carol = #room-a :alice bob carol"), "names: {out}");
        assert!(!out.contains(":matrix carol"), "placeholder leaked: {out}");
    }

    #[test]
    fn flatten_newlines_uses_visible_marker() {
        assert_eq!(flatten_newlines("one\ntwo\nthree"), "one ↵ two ↵ three");
        assert_eq!(flatten_newlines("plain"), "plain");
    }

    #[test]
    fn chunk_for_irc_under_budget_returns_single() {
        let s = "x".repeat(SAFE_BODY_BYTES);
        let v = chunk_for_irc(&s);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].len(), SAFE_BODY_BYTES);
    }

    #[test]
    fn chunk_for_irc_splits_on_word_boundary_when_oversized() {
        let part = "word ".repeat(120); // 600 bytes, plenty over budget
        let v = chunk_for_irc(&part);
        assert!(v.len() >= 2, "expected multiple pieces, got {}", v.len());
        for piece in &v {
            assert!(piece.len() <= SAFE_BODY_BYTES,
                "chunk exceeded budget ({} > {SAFE_BODY_BYTES}): {piece:?}", piece.len());
        }
        // Concatenating back with spaces recovers all words.
        let rejoined = v.join(" ");
        let words_in = part.split_whitespace().count();
        let words_out = rejoined.split_whitespace().count();
        assert_eq!(words_in, words_out, "lost words: in={words_in} out={words_out}");
    }

    #[test]
    fn chunk_for_irc_handles_multibyte_at_boundary() {
        let core = "é".repeat(SAFE_BODY_BYTES); // 2 bytes per char → 800 bytes
        let v = chunk_for_irc(&core);
        for piece in &v {
            assert!(piece.is_char_boundary(piece.len()));
            assert!(piece.len() <= SAFE_BODY_BYTES);
        }
    }

    #[test]
    fn parse_reply_prefix_recognises_only_known_format() {
        assert_eq!(parse_reply_prefix("!r abc hello"), Some(("abc", "hello")));
        assert_eq!(parse_reply_prefix("!r zzz multi word reply"), Some(("zzz", "multi word reply")));
        // Wrong length, wrong case, digits → not a reply prefix; user text passes through.
        assert_eq!(parse_reply_prefix("!r ab hi"), None);
        assert_eq!(parse_reply_prefix("!r ABC hi"), None);
        assert_eq!(parse_reply_prefix("!r 123 hi"), None);
        assert_eq!(parse_reply_prefix("!r abcd hi"), None);
        assert_eq!(parse_reply_prefix("!r abc "), None);
        assert_eq!(parse_reply_prefix("hello !r abc oops"), None);
    }

    fn eid_for(n: usize) -> matrix_sdk::ruma::OwnedEventId {
        matrix_sdk::ruma::OwnedEventId::try_from(format!("$e{n}:h").as_str()).unwrap()
    }

    #[test]
    fn short_event_id_is_deterministic_and_three_lowercase_letters() {
        for n in 0..50 {
            let s = short_event_id(&eid_for(n));
            assert_eq!(s.len(), 3, "{s} should be 3 chars");
            assert!(s.bytes().all(|b| b.is_ascii_lowercase()), "{s} non-letter");
            assert_eq!(short_event_id(&eid_for(n)), s, "hash must be stable across calls");
        }
    }

    #[test]
    fn ring_stores_short_event_id_and_pops_oldest_when_full() {
        let mut ring = ReplyRing::new();
        let mut shorts = Vec::new();
        for i in 0..(REPLY_RING_CAP + 3) {
            shorts.push(remember_reply_id(
                &mut ring, "#room", eid_for(i), "alice".into(), format!("body{i}"),
            ));
        }
        // The three oldest entries fell off the front.
        for stale in &shorts[..3] {
            // Only assert eviction when no later push reused this short by collision.
            let later_reused = shorts[3..].contains(stale);
            assert_eq!(
                lookup_reply_id(&ring, "#room", stale).is_some(),
                later_reused,
                "{stale}: later_reused={later_reused}",
            );
        }
        // Newest entry is reachable.
        let newest = shorts.last().unwrap();
        let (eid, _, _) = lookup_reply_id(&ring, "#room", newest).expect("newest present");
        assert_eq!(eid.as_str(), format!("$e{}:h", REPLY_RING_CAP + 2));
        // Target lookup is case-insensitive.
        assert!(lookup_reply_id(&ring, "#ROOM", newest).is_some());
    }

    #[test]
    fn lookup_returns_latest_on_short_collision() {
        // Find any two event ids that collide on the same short, push them
        // in known order, verify the latest-pushed wins.
        let mut seen: HashMap<String, matrix_sdk::ruma::OwnedEventId> = HashMap::new();
        let (older, newer) = (0..20_000).find_map(|i| {
            let id = eid_for(i);
            let short = short_event_id(&id);
            if let Some(prev) = seen.get(&short) {
                return Some((prev.clone(), id));
            }
            seen.insert(short, id);
            None
        }).expect("collision in 20k samples");
        eprintln!("collision: older={older} newer={newer}");
        assert_ne!(older, newer, "must be distinct event ids");
        assert_eq!(short_event_id(&older), short_event_id(&newer), "shorts must match");

        let mut ring = ReplyRing::new();
        let short = remember_reply_id(&mut ring, "#room", older.clone(), "a".into(), "old".into());
        let _ = remember_reply_id(&mut ring, "#room", newer.clone(), "b".into(), "new".into());
        let (hit, sender, snip) = lookup_reply_id(&ring, "#room", &short).expect("present");
        assert_eq!(hit, newer, "latest pushed wins");
        assert_eq!(sender, "b");
        assert_eq!(snip, "new");
    }
}
