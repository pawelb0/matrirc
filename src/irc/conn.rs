use std::collections::HashSet;
use std::net::SocketAddr;

use anyhow::{Context, Result};
use time::format_description::FormatItem;
use time::macros::format_description;
use time::OffsetDateTime;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::OwnedWriteHalf;
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
const SUPPORTED_CAPS: &[&str] = &[SERVER_TIME_CAP, MESSAGE_TAGS_CAP];

const SERVER_NAME: &str = "matrirc.local";
const VERSION: &str = concat!("matrirc-", env!("CARGO_PKG_VERSION"));
const MAX_LINE: usize = 8192;

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
                        auto_join_all(&mut write, &n, &bridge, &mut s.joined, &s.caps).await?;
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
    info!(%peer, "client disconnected");
    Ok(())
}

async fn handle_matrix_event(
    write: &mut OwnedWriteHalf,
    bridge: &Bridge,
    s: &mut State,
    ev: FromMatrix,
) -> Result<()> {
    match ev {
        FromMatrix::Message { room, sender_nick, body } => {
            let target = if let Some(chan) = bridge.chan_for(&room) {
                if !s.joined.contains(&chan) { return Ok(()); }
                chan
            } else if bridge.dm_nick_for(&room).is_some() {
                // DM: deliver to the client's own nick so irssi opens a query window
                match s.nick.as_deref() { Some(n) => n.to_string(), None => return Ok(()) }
            } else { return Ok(()); };
            let prefix = format!("{sender_nick}!{sender_nick}@matrix");
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
    }
    Ok(())
}

async fn handle_command(
    write: &mut OwnedWriteHalf,
    peer: &SocketAddr,
    bridge: &Bridge,
    msg: &Message,
    s: &mut State,
) -> Result<bool> {
    let p0 = msg.params.first().map(String::as_str);
    match msg.command.as_str() {
        "CAP" => handle_cap(write, msg, &mut s.caps).await?,
        "NICK" => if let Some(n) = p0 { s.nick = Some(n.into()); },
        "USER" => if let Some(u) = p0 { s.user = Some(u.into()); },
        "PING" => send(write, srv("PONG", vec![SERVER_NAME.into(), p0.unwrap_or("").into()])).await?,
        "JOIN" => if let Some(n) = s.nick.clone() {
            handle_join(write, &n, msg, &mut s.joined, bridge, &s.caps).await?;
        },
        "PART" => if let Some(n) = s.nick.clone() {
            handle_part(write, &n, msg, &mut s.joined).await?;
        },
        "PRIVMSG" => if let Some(n) = s.nick.clone() {
            handle_privmsg(write, &n, msg, bridge).await?;
        },
        "WHOIS" => if let Some(n) = s.nick.clone() {
            handle_whois(write, &n, msg, bridge).await?;
        },
        "NOTICE" => {}
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
    write: &mut OwnedWriteHalf,
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
    write: &mut OwnedWriteHalf,
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
        send(write, srv("403", vec![nick.into(), chan.into(), "No such channel".into()])).await?;
    }
    Ok(())
}

fn is_matrix_alias(target: &str) -> bool {
    target.starts_with('#') && target.contains(':')
}

async fn request_join_by_alias(
    write: &mut OwnedWriteHalf,
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
    write: &mut OwnedWriteHalf,
    nick: &str,
    joined: &mut HashSet<String>,
) -> Result<()> {
    send_join(write, nick, ECHO_CHAN, ECHO_TOPIC, &[ECHO_NICK]).await?;
    joined.insert(ECHO_CHAN.to_string());
    Ok(())
}

async fn join_bridged(
    write: &mut OwnedWriteHalf,
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
    write: &mut OwnedWriteHalf,
    nick: &str,
    chan: &str,
    topic: &str,
    members: &[&str],
) -> Result<()> {
    send(write, Message::with_prefix(user_prefix(nick), "JOIN", vec![chan.into()])).await?;
    if topic.is_empty() {
        send(write, srv("331", vec![nick.into(), chan.into(), "No topic is set".into()])).await?;
    } else {
        send(write, srv("332", vec![nick.into(), chan.into(), topic.into()])).await?;
    }
    send_names(write, nick, chan, members).await?;
    Ok(())
}

async fn send_names(
    write: &mut OwnedWriteHalf,
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
            send(write, srv("353", vec![nick.into(), "=".into(), chan.into(), std::mem::take(&mut line)])).await?;
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(n);
    }
    if !line.is_empty() {
        send(write, srv("353", vec![nick.into(), "=".into(), chan.into(), line])).await?;
    }
    send(write, srv("366", vec![nick.into(), chan.into(), "End of /NAMES list".into()])).await?;
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
    write: &mut OwnedWriteHalf,
    nick: &str,
    bridge: &Bridge,
    joined: &mut HashSet<String>,
    caps: &HashSet<String>,
) -> Result<()> {
    let snapshot = bridge.snapshot();
    let dm_count = bridge.dm_count();
    if snapshot.is_empty() && dm_count == 0 {
        matrirc_notice(write, nick, "sync still in progress — channels will auto-join when ready").await?;
        return Ok(());
    }
    info!(%nick, channels = snapshot.len(), dms = dm_count, "auto-join");
    let mut new_joins = Vec::new();
    for (chan, room) in &snapshot {
        if joined.contains(chan) { continue; }
        let topic = bridge.topic_for(chan).unwrap_or_default();
        // Fast preamble with placeholder NAMES; proper member list comes with backfill.
        send_join(write, nick, chan, &topic, &["matrix"]).await?;
        joined.insert(chan.clone());
        new_joins.push((chan.clone(), room.clone()));
    }
    let names: Vec<&str> = new_joins.iter().map(|(c, _)| c.as_str()).collect();
    let joined_list = if names.is_empty() { "(none)".to_string() } else { names.join(", ") };
    matrirc_notice(
        write,
        nick,
        &format!("bridged {} channel(s) + {dm_count} DM(s). channels: {joined_list}", names.len()),
    )
    .await?;
    for (chan, room) in new_joins {
        backfill_channel(write, &chan, &room, bridge, caps).await?;
    }
    Ok(())
}

async fn bot_line(write: &mut OwnedWriteHalf, nick: &str, cmd: &str, body: &str) -> Result<()> {
    send(
        write,
        Message::with_prefix(BOT_PREFIX, cmd, vec![nick.into(), body.into()]),
    )
    .await
}

async fn matrirc_notice(write: &mut OwnedWriteHalf, nick: &str, body: &str) -> Result<()> {
    bot_line(write, nick, "NOTICE", body).await
}

async fn matrirc_msg(write: &mut OwnedWriteHalf, nick: &str, body: &str) -> Result<()> {
    bot_line(write, nick, "PRIVMSG", body).await
}

async fn handle_bot_command(
    write: &mut OwnedWriteHalf,
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
    write: &mut OwnedWriteHalf,
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
    for m in msgs {
        let prefix = format!("{}!{0}@matrix", m.sender_nick);
        for piece in m.body.split('\n') {
            if piece.is_empty() { continue; }
            let mut out = Message::with_prefix(&prefix, "PRIVMSG", vec![chan.into(), piece.into()]);
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
    write: &mut OwnedWriteHalf,
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
        send(write, srv("401", vec![nick.into(), target.clone(), "No such nick/channel".into()])).await?;
        send(write, srv("318", vec![nick.into(), target, "End of /WHOIS list".into()])).await?;
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
            send(write, srv("401", vec![nick.into(), target.clone(), "No such nick/channel".into()])).await?;
            send(write, srv("318", vec![nick.into(), target, "End of /WHOIS list".into()])).await?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn send_whois(
    write: &mut OwnedWriteHalf,
    nick: &str,
    target_nick: &str,
    user: &str,
    host: &str,
    realname: &str,
    server: Option<&str>,
    channels: &[String],
) -> Result<()> {
    send(write, srv("311", vec![nick.into(), target_nick.into(), user.into(), host.into(), "*".into(), realname.into()])).await?;
    if let Some(s) = server {
        send(write, srv("312", vec![nick.into(), target_nick.into(), s.into(), "Matrix homeserver".into()])).await?;
    }
    if !channels.is_empty() {
        send(write, srv("319", vec![nick.into(), target_nick.into(), channels.join(" ")])).await?;
    }
    send(write, srv("318", vec![nick.into(), target_nick.into(), "End of /WHOIS list".into()])).await?;
    Ok(())
}

async fn handle_part(
    write: &mut OwnedWriteHalf,
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
    write: &mut OwnedWriteHalf,
    nick: &str,
    msg: &Message,
    bridge: &Bridge,
) -> Result<()> {
    let Some(target) = msg.params.first() else { return Ok(()); };
    let Some(text) = msg.params.get(1) else { return Ok(()); };

    if target == ECHO_CHAN || target.eq_ignore_ascii_case(ECHO_NICK) {
        let echo_target: &str = if target == ECHO_CHAN { ECHO_CHAN } else { nick };
        let body = format!("echo: {text}");
        send(write, Message::with_prefix(ECHO_PREFIX, "PRIVMSG", vec![echo_target.into(), body])).await?;
        return Ok(());
    }

    if target.eq_ignore_ascii_case("matrirc") {
        handle_bot_command(write, nick, text, bridge).await?;
        return Ok(());
    }

    if let Some(room) = bridge.room_for(target).or_else(|| bridge.dm_room_for(target)) {
        if let Err(e) = bridge.to_matrix.try_send(ToMatrix::Send { room, body: text.clone() }) {
            warn!("dropping outbound matrix message: {e}");
            send(write, srv("NOTICE", vec![nick.into(), format!("send dropped: {e}")])).await?;
        }
        return Ok(());
    }

    // Explicit MXID target (`@alice:server.org` or bare `alice:server.org`) →
    // open or create a DM. irssi strips leading '@' in /query, accept both.
    if target.contains(':') {
        let canonical = if target.starts_with('@') { target.clone() } else { format!("@{target}") };
        if let Ok(mxid) = matrix_sdk::ruma::OwnedUserId::try_from(canonical.as_str()) {
            if let Err(e) = bridge.to_matrix.try_send(ToMatrix::SendToMxid { mxid, body: text.clone() }) {
                warn!("dropping outbound DM to MXID: {e}");
                send(write, srv("NOTICE", vec![nick.into(), format!("send dropped: {e}")])).await?;
            }
            return Ok(());
        }
    }

    send(write, srv("401", vec![nick.into(), target.clone(), "No such nick/channel".into()])).await?;
    Ok(())
}

async fn read_line<R: tokio::io::AsyncBufRead + Unpin>(
    lines: &mut tokio::io::Lines<R>,
) -> Result<Option<String>> {
    match lines.next_line().await.context("read line")? {
        Some(line) => {
            if line.len() > MAX_LINE {
                warn!(len = line.len(), "line exceeded max length, truncating");
                Ok(Some(line.chars().take(MAX_LINE).collect()))
            } else {
                Ok(Some(line))
            }
        }
        None => Ok(None),
    }
}

async fn send_welcome(write: &mut OwnedWriteHalf, nick: &str) -> Result<()> {
    let n = nick.to_string();
    send(write, srv("001", vec![n.clone(), format!("Welcome to matrirc, {nick}")])).await?;
    send(write, srv("002", vec![n.clone(), format!("Your host is {SERVER_NAME}, running {VERSION}")])).await?;
    send(write, srv("003", vec![n.clone(), "This server has no creation date".into()])).await?;
    send(write, srv("004", vec![n.clone(), SERVER_NAME.into(), VERSION.into(), "".into(), "".into()])).await?;
    send(write, srv("375", vec![n.clone(), format!("- {SERVER_NAME} Message of the day -")])).await?;
    send(write, srv("372", vec![n.clone(), "- matrirc: Matrix rooms auto-joined after this line.".into()])).await?;
    send(write, srv("372", vec![n.clone(), "- /msg matrirc help  for bridge commands.".into()])).await?;
    send(write, srv("372", vec![n.clone(), format!("- /join {ECHO_CHAN}  for a local echo channel.")])).await?;
    send(write, srv("376", vec![n, "End of /MOTD command.".into()])).await?;
    Ok(())
}

fn srv(command: &str, params: Vec<String>) -> Message {
    Message::with_prefix(SERVER_NAME, command, params)
}

async fn send(write: &mut OwnedWriteHalf, msg: Message) -> Result<()> {
    let mut wire = msg.to_wire();
    debug!(out = %wire, "send");
    wire.push_str("\r\n");
    write.write_all(wire.as_bytes()).await.context("write")?;
    Ok(())
}
