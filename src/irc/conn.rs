use std::collections::HashSet;
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
const ECHO_TOPIC: &str = "Echo channel — anything you say, echo will say back";
const BOT_PREFIX: &str = "matrirc!matrirc@matrirc.local";

fn user_prefix(nick: &str) -> String {
    format!("{nick}!{nick}@matrirc.local")
}

#[derive(Default)]
struct State {
    nick: Option<String>,
    user: Option<String>,
    registered: bool,
    joined: HashSet<String>,
    caps: HashSet<String>,
    dm_backfilled: HashSet<matrix_sdk::ruma::OwnedRoomId>,
    dm_hinted: HashSet<matrix_sdk::ruma::OwnedRoomId>,
}

pub async fn handle(sock: TcpStream, peer: SocketAddr, bridge: Bridge) -> Result<()> {
    let (read, mut write) = sock.into_split();
    let mut lines = BufReader::new(read).lines();
    let mut from_matrix = bridge.from_matrix.subscribe();
    let mut s = State::default();

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

async fn handle_matrix_event(
    write: &mut (impl tokio::io::AsyncWrite + Unpin),
    bridge: &Bridge,
    s: &mut State,
    ev: FromMatrix,
) -> Result<()> {
    match ev {
        FromMatrix::Message { room, sender_nick, body, is_own } => {
            let (prefix, target) = if let Some(chan) = bridge.chan_for(&room) {
                if !s.joined.contains(&chan) { return Ok(()); }
                (format!("{sender_nick}!{sender_nick}@matrix"), chan)
            } else if let Some(peer) = bridge.dm_nick_for(&room) {
                let Some(n) = s.nick.as_deref() else { return Ok(()); };
                if is_own {
                    // Own message from another device: ZNC-style self→peer so it
                    // renders in the peer's query window, not a self-query.
                    (format!("{n}!{n}@matrirc.local"), peer)
                } else {
                    (format!("{sender_nick}!{sender_nick}@matrix"), n.to_string())
                }
            } else { return Ok(()); };
            for piece in body.split('\n').filter(|p| !p.is_empty()) {
                send(write, Message::with_prefix(&prefix, "PRIVMSG", vec![target.clone(), piece.into()])).await?;
            }
        }
        FromMatrix::RoomAdded { room, chan, topic } => {
            if !s.registered || s.joined.contains(&chan) { return Ok(()); }
            if let Some(n) = s.nick.as_deref() {
                join_bridged(write, n, &chan, &room, &topic, bridge, &s.caps).await?;
                s.joined.insert(chan);
            }
        }
        FromMatrix::DmAdded { nick: dm } => {
            if s.registered {
                if let Some(n) = s.nick.as_deref() {
                    matrirc_notice(write, n, &format!("DM available: /msg {dm} ...")).await?;
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
            handle_join(write, &n, msg, &mut s.joined, bridge, &s.caps).await?;
        },
        "PART" => if let Some(n) = s.nick.clone() {
            handle_part(write, &n, msg, &mut s.joined).await?;
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
            if all_supported {
                for c in &caps {
                    if let Some(removed) = c.strip_prefix('-') {
                        caps_enabled.remove(removed);
                    } else {
                        caps_enabled.insert((*c).to_string());
                    }
                }
            }
            send(
                write,
                srv("CAP", vec!["*".into(), verb.into(), requested]),
            )
            .await?;
        }
        _ => debug!(?msg, "ignoring CAP subcommand"),
    }
    Ok(())
}

async fn handle_join(
    write: &mut (impl tokio::io::AsyncWrite + Unpin),
    nick: &str,
    msg: &Message,
    joined: &mut HashSet<String>,
    bridge: &Bridge,
    caps: &HashSet<String>,
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
            let topic = bridge.topic_for(chan).unwrap_or_default();
            join_bridged(write, nick, chan, &room, &topic, bridge, caps).await?;
            joined.insert(chan.to_string());
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

async fn join_bridged(
    write: &mut (impl tokio::io::AsyncWrite + Unpin),
    nick: &str,
    chan: &str,
    room: &matrix_sdk::ruma::RoomId,
    topic: &str,
    bridge: &Bridge,
    caps: &HashSet<String>,
) -> Result<()> {
    let members = fetch_members(bridge, room).await;
    let names: Vec<&str> = members.iter().map(String::as_str).collect();
    send_join(write, nick, chan, topic, &names).await?;
    backfill_channel(write, chan, room, bridge, caps).await?;
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

    for (chan, room) in new_joins {
        backfill_channel(write, &chan, &room, bridge, &s.caps).await?;
    }
    // Eager DM backfill: populates one query window per DM under the canonical
    // peer nick. Otherwise irssi has nothing to display until first message.
    for (room, dm_nick) in dms {
        if s.dm_backfilled.insert(room.clone()) {
            let _ = dm_nick; // peer nick lands in backfill's own prefixes
            backfill_channel(write, nick, &room, bridge, &s.caps).await?;
        }
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
) -> Result<()> {
    let cmd = text.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
    match cmd.as_str() {
        "" | "help" | "?" => {
            for line in [
                "matrirc — local Matrix↔IRC bridge",
                "",
                "bot commands (to this nick):",
                "  help                          this message",
                "  rooms                         list bridged Matrix channels",
                "  dms                           list known Matrix DMs",
                "  search <term> [on <server>]   public-room directory",
                "  version                       matrirc version",
                "",
                "IRC → Matrix:",
                "  /join #alias:server.org       join any public Matrix room",
                "  /msg @alice:server.org hi     open/create a DM",
                "  /msg <known-dm-nick> hi       existing DM (see `dms`)",
                "  /part #channel                leave the IRC channel (Matrix room keeps you)",
                "  /me does a thing              m.emote",
                "",
                "daemon control (in your shell, not here):",
                "  matrirc status | stop | verify | reset",
                "docs: https://github.com/pawelb0/matrirc",
            ] {
                matrirc_msg(write, nick, line).await?;
            }
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
        other => {
            matrirc_msg(write, nick, &format!("unknown command: {other}  (try `help`)")).await?;
        }
    }
    Ok(())
}

async fn backfill_channel(
    write: &mut (impl tokio::io::AsyncWrite + Unpin),
    chan: &str,
    room: &matrix_sdk::ruma::RoomId,
    bridge: &Bridge,
    caps: &HashSet<String>,
) -> Result<()> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    if bridge
        .to_matrix
        .try_send(ToMatrix::Backfill {
            room: room.to_owned(),
            limit: 200,
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
        for piece in m.body.split('\n') {
            if piece.is_empty() { continue; }
            let mut out = Message::with_prefix(&prefix, "PRIVMSG", vec![target.into(), piece.into()]);
            if server_time {
                if let Some(iso) = ms_to_iso(m.origin_ms) {
                    out = out.with_tag("time", iso);
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
    let _ = bridge.to_matrix.try_send(make_send_cmd(dest, body.clone(), false, true));
    Ok(())
}

async fn handle_part(
    write: &mut (impl tokio::io::AsyncWrite + Unpin),
    nick: &str,
    msg: &Message,
    joined: &mut HashSet<String>,
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

    if target == ECHO_CHAN || target.eq_ignore_ascii_case(ECHO_NICK) {
        let dest: &str = if target == ECHO_CHAN { ECHO_CHAN } else { nick };
        send(write, Message::with_prefix(ECHO_PREFIX, "PRIVMSG", vec![dest.into(), format!("echo: {body}")])).await?;
        return Ok(());
    }
    if target.eq_ignore_ascii_case("matrirc") {
        if emote { return Ok(()); }
        if let Some(reply) = ctcp_reply_for(body) {
            return send(write, Message::with_prefix(BOT_PREFIX, "NOTICE", vec![nick.into(), format!("\x01{reply}\x01")])).await;
        }
        return handle_bot_command(write, nick, body, bridge).await;
    }

    // IRCv3 echo-message: if the client negotiated it, the client suppresses
    // local echo and waits for the server to bounce back. Do it.
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
    let cmd = make_send_cmd(dest, body.to_string(), emote, false);
    if let Err(e) = bridge.to_matrix.try_send(cmd) {
        warn!("dropping outbound: {e}");
        send(write, srv("NOTICE", vec![nick.into(), format!("send dropped: {e}")])).await?;
    }
    Ok(())
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

fn make_send_cmd(dest: SendTarget, body: String, emote: bool, notice: bool) -> ToMatrix {
    match dest {
        SendTarget::Room(room) => ToMatrix::Send { room, body, emote, notice },
        SendTarget::Mxid(mxid) => ToMatrix::SendToMxid { mxid, body, emote, notice },
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
    let footer: &[(&str, Vec<String>)] = &[
        (rpl::MOTD, vec![n.clone(), "- Matrix rooms auto-joined after this line.".into()]),
        (rpl::MOTD, vec![n.clone(), "- /msg matrirc help  for bridge commands.".into()]),
        (rpl::MOTD, vec![n.clone(), format!("- /join {ECHO_CHAN}  for a local echo channel.")]),
        (rpl::ENDOFMOTD, vec![n, "End of /MOTD command.".into()]),
    ];
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

    #[tokio::test]
    async fn channel_peer_message() {
        let (b, _rx) = Bridge::new(Mapping::default());
        let r = room("!a:server");
        b.add_mapping(r.clone(), "#room-a".into(), "topic".into());
        let mut s = registered_state("pawelb");
        s.joined.insert("#room-a".into());
        let out = route(
            FromMatrix::Message { room: r, sender_nick: "alice".into(), body: "hi".into(), is_own: false },
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
            m.insert_dm(r.clone(), "phobos", &[]);
        }
        let mut s = registered_state("pawelb");
        let out = route(
            FromMatrix::Message { room: r, sender_nick: "phobos".into(), body: "yo".into(), is_own: false },
            &b, &mut s,
        ).await;
        assert_eq!(out, ":phobos!phobos@matrix PRIVMSG pawelb yo\r\n");
    }

    #[tokio::test]
    async fn dm_own_message_routes_znc_style() {
        let (b, _rx) = Bridge::new(Mapping::default());
        let r = room("!dm:server");
        {
            let mut m = b.mapping.write().unwrap();
            m.insert_dm(r.clone(), "phobos", &[]);
        }
        let mut s = registered_state("pawelb");
        let out = route(
            FromMatrix::Message { room: r, sender_nick: "pawelb".into(), body: "from other device".into(), is_own: true },
            &b, &mut s,
        ).await;
        assert_eq!(out, ":pawelb!pawelb@matrirc.local PRIVMSG phobos :from other device\r\n");
    }

    #[tokio::test]
    async fn unknown_room_is_dropped() {
        let (b, _rx) = Bridge::new(Mapping::default());
        let mut s = registered_state("pawelb");
        let out = route(
            FromMatrix::Message { room: room("!nope:server"), sender_nick: "alice".into(), body: "hi".into(), is_own: false },
            &b, &mut s,
        ).await;
        assert!(out.is_empty(), "expected empty, got {out:?}");
    }

    #[tokio::test]
    async fn channel_not_joined_is_dropped() {
        let (b, _rx) = Bridge::new(Mapping::default());
        let r = room("!a:server");
        b.add_mapping(r.clone(), "#room-a".into(), "".into());
        let mut s = registered_state("pawelb"); // no joined insert
        let out = route(
            FromMatrix::Message { room: r, sender_nick: "alice".into(), body: "hi".into(), is_own: false },
            &b, &mut s,
        ).await;
        assert!(out.is_empty(), "expected empty, got {out:?}");
    }

    #[tokio::test]
    async fn topic_changed_emits_topic() {
        let (b, _rx) = Bridge::new(Mapping::default());
        let mut s = registered_state("pawelb");
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
        let mut s = registered_state("pawelb");
        let out = route(
            FromMatrix::DmAdded { nick: "phobos".into() },
            &b, &mut s,
        ).await;
        assert!(out.contains("NOTICE pawelb"), "{out}");
        assert!(out.contains("phobos"), "{out}");
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
        let mut s = registered_state("pawelb");
        let out = dispatch("PING :abc", &b, &mut s).await;
        assert_eq!(out, ":matrirc.local PONG matrirc.local abc\r\n");
    }

    #[tokio::test]
    async fn join_echo_channel_emits_ack_topic_names() {
        let (b, _rx) = Bridge::new(Mapping::default());
        let mut s = registered_state("pawelb");
        let out = dispatch("JOIN #echo", &b, &mut s).await;
        assert!(out.contains(":pawelb!pawelb@matrirc.local JOIN #echo"), "{out}");
        assert!(out.contains(":matrirc.local 332 pawelb #echo"), "{out}");
        assert!(out.contains(" echo"), "names list should include echo: {out}");
        assert!(out.contains(":matrirc.local 366 pawelb #echo"), "{out}");
        assert!(s.joined.contains("#echo"));
    }

    #[tokio::test]
    async fn join_unknown_channel_returns_403() {
        let (b, _rx) = Bridge::new(Mapping::default());
        let mut s = registered_state("pawelb");
        let out = dispatch("JOIN #nope", &b, &mut s).await;
        assert!(out.contains(":matrirc.local 403 pawelb #nope"), "{out}");
    }

    #[tokio::test]
    async fn privmsg_echo_channel_echoes_back() {
        let (b, _rx) = Bridge::new(Mapping::default());
        let mut s = registered_state("pawelb");
        let out = dispatch("PRIVMSG #echo :hi there", &b, &mut s).await;
        assert!(out.contains(":echo!echo@matrirc.local PRIVMSG #echo :echo: hi there"), "{out}");
    }

    #[tokio::test]
    async fn privmsg_to_unknown_returns_401() {
        let (b, _rx) = Bridge::new(Mapping::default());
        let mut s = registered_state("pawelb");
        let out = dispatch("PRIVMSG nobody :hello", &b, &mut s).await;
        assert!(out.contains(":matrirc.local 401 pawelb nobody"), "{out}");
    }

    #[tokio::test]
    async fn privmsg_to_bot_help_lists_commands() {
        let (b, _rx) = Bridge::new(Mapping::default());
        let mut s = registered_state("pawelb");
        let out = dispatch("PRIVMSG matrirc :help", &b, &mut s).await;
        assert!(out.contains("matrirc — local Matrix↔IRC bridge"), "{out}");
        assert!(out.contains("help"), "{out}");
        assert!(out.contains("rooms"), "{out}");
    }

    #[tokio::test]
    async fn whois_echo_returns_311_and_318() {
        let (b, _rx) = Bridge::new(Mapping::default());
        let mut s = registered_state("pawelb");
        let out = dispatch("WHOIS echo", &b, &mut s).await;
        assert!(out.contains(":matrirc.local 311 pawelb echo"), "{out}");
        assert!(out.contains(":matrirc.local 318 pawelb echo"), "{out}");
    }

    #[tokio::test]
    async fn ctcp_action_to_bot_is_silent() {
        let (b, _rx) = Bridge::new(Mapping::default());
        let mut s = registered_state("pawelb");
        let out = dispatch("PRIVMSG matrirc :\x01ACTION waves\x01", &b, &mut s).await;
        assert!(out.is_empty(), "bot should not emit for CTCP: {out:?}");
    }

    #[tokio::test]
    async fn nick_post_registration_echoes_and_routes_to_matrix() {
        let (b, mut rx) = Bridge::new(Mapping::default());
        let mut s = registered_state("pawelb");
        let out = dispatch("NICK newnick", &b, &mut s).await;
        assert_eq!(out, ":pawelb!pawelb@matrirc.local NICK newnick\r\n");
        assert_eq!(s.nick.as_deref(), Some("newnick"));
        match rx.try_recv() {
            Ok(ToMatrix::SetDisplayName { name }) => assert_eq!(name, "newnick"),
            other => panic!("expected SetDisplayName, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn nick_same_value_is_noop() {
        let (b, mut rx) = Bridge::new(Mapping::default());
        let mut s = registered_state("pawelb");
        let out = dispatch("NICK pawelb", &b, &mut s).await;
        assert!(out.is_empty(), "{out:?}");
        assert!(rx.try_recv().is_err(), "no SetDisplayName for same nick");
    }

    #[tokio::test]
    async fn multi_line_body_produces_multiple_privmsg() {
        let (b, _rx) = Bridge::new(Mapping::default());
        let r = room("!a:server");
        b.add_mapping(r.clone(), "#room-a".into(), "".into());
        let mut s = registered_state("pawelb");
        s.joined.insert("#room-a".into());
        let out = route(
            FromMatrix::Message { room: r, sender_nick: "alice".into(), body: "one\ntwo".into(), is_own: false },
            &b, &mut s,
        ).await;
        assert!(out.contains("PRIVMSG #room-a one\r\n"));
        assert!(out.contains("PRIVMSG #room-a two\r\n"));
    }

    #[tokio::test]
    async fn member_joined_emits_irc_join() {
        let (b, _rx) = Bridge::new(Mapping::default());
        let mut s = registered_state("pawelb");
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
        let mut s = registered_state("pawelb");
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
        let mut s = registered_state("pawelb");
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
        let mut s = registered_state("pawelb");
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
        b.add_mapping(r.clone(), "#room-a".into(), "topic".into());

        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if let ToMatrix::Members { reply, .. } = msg {
                    let _ = reply.send(vec!["alice".into(), "bob".into()]);
                }
            }
        });

        let mut s = State::default();
        s.nick = Some("pawelb".into());
        s.user = Some("pawelb".into());

        let mut out = Vec::<u8>::new();
        auto_join_all(&mut out, "pawelb", &b, &mut s).await.unwrap();
        let out = String::from_utf8(out).unwrap();

        assert!(out.contains("353 pawelb = #room-a :alice bob pawelb"), "names: {out}");
        assert!(!out.contains(":matrix pawelb"), "placeholder leaked: {out}");
    }
}
