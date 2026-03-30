use std::collections::HashSet;
use std::net::SocketAddr;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::TcpStream;
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use super::proto::Message;
use crate::bridge::{Bridge, FromMatrix, ToMatrix};

const SERVER_NAME: &str = "matrirc.local";
const VERSION: &str = concat!("matrirc-", env!("CARGO_PKG_VERSION"));
const MAX_LINE: usize = 8192;

const ECHO_NICK: &str = "echo";
const ECHO_PREFIX: &str = "echo!echo@matrirc.local";
const ECHO_CHAN: &str = "#echo";
const ECHO_TOPIC: &str = "Echo channel — anything you say, echo will say back";

pub async fn handle(sock: TcpStream, peer: SocketAddr, bridge: Bridge) -> Result<()> {
    let (read, mut write) = sock.into_split();
    let mut lines = BufReader::new(read).lines();
    let mut from_matrix = bridge.from_matrix.subscribe();

    let mut nick: Option<String> = None;
    let mut user: Option<String> = None;
    let mut registered = false;
    let mut joined: HashSet<String> = HashSet::new();

    loop {
        tokio::select! {
            line_res = read_line(&mut lines) => {
                let Some(line) = line_res? else { break; };
                let msg = match Message::parse(&line) {
                    Ok(m) => m,
                    Err(e) => {
                        debug!(%peer, error = %e, raw = %line, "skipping malformed line");
                        continue;
                    }
                };
                if handle_command(&mut write, &peer, &bridge, &msg, &mut nick, &mut user, &mut joined).await? {
                    return Ok(());
                }
                if !registered {
                    if let (Some(n), Some(_)) = (&nick, &user) {
                        send_welcome(&mut write, n).await?;
                        registered = true;
                        auto_join_all(&mut write, n, &bridge, &mut joined).await?;
                    }
                }
            }
            ev = from_matrix.recv() => {
                match ev {
                    Ok(FromMatrix::Message { room, sender_nick, body }) => {
                        let target = if let Some(chan) = bridge.chan_for(&room) {
                            if !joined.contains(&chan) { continue; }
                            chan
                        } else if bridge.dm_nick_for(&room).is_some() {
                            // DM: deliver to the client's own nick so irssi opens a query window
                            match nick.as_deref() { Some(n) => n.to_string(), None => continue }
                        } else {
                            continue;
                        };
                        let prefix = format!("{sender_nick}!{sender_nick}@matrix");
                        for piece in body.split('\n') {
                            if piece.is_empty() { continue; }
                            send(&mut write, Message::with_prefix(&prefix, "PRIVMSG", vec![target.clone(), piece.to_string()])).await?;
                        }
                    }
                    Ok(FromMatrix::RoomAdded { room, chan, topic }) => {
                        if !registered {
                            continue;
                        }
                        if joined.contains(&chan) { continue; }
                        if let Some(n) = nick.as_deref() {
                            join_bridged(&mut write, n, &chan, &room, &topic, &bridge).await?;
                            joined.insert(chan);
                        }
                    }
                    Ok(FromMatrix::DmAdded { nick: dm_nick }) => {
                        if !registered { continue; }
                        if let Some(n) = nick.as_deref() {
                            matrirc_notice(&mut write, n, &format!("DM available: /msg {dm_nick} ...")).await?;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(%peer, "irc client lagged {n} matrix events");
                    }
                }
            }
        }
    }

    info!(%peer, "client disconnected");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_command(
    write: &mut OwnedWriteHalf,
    peer: &SocketAddr,
    bridge: &Bridge,
    msg: &Message,
    nick: &mut Option<String>,
    user: &mut Option<String>,
    joined: &mut HashSet<String>,
) -> Result<bool> {
    match msg.command.as_str() {
        "CAP" => handle_cap(write, msg).await?,
        "NICK" => {
            if let Some(n) = msg.params.first() {
                *nick = Some(n.clone());
            }
        }
        "USER" => {
            if let Some(u) = msg.params.first() {
                *user = Some(u.clone());
            }
        }
        "PING" => {
            let token = msg.params.first().cloned().unwrap_or_default();
            send(write, srv("PONG", vec![SERVER_NAME.into(), token])).await?;
        }
        "JOIN" => {
            if let Some(n) = nick.as_deref() {
                handle_join(write, n, msg, joined, bridge).await?;
            }
        }
        "PART" => {
            if let Some(n) = nick.as_deref() {
                handle_part(write, n, msg, joined).await?;
            }
        }
        "PRIVMSG" => {
            if let Some(n) = nick.as_deref() {
                handle_privmsg(write, n, msg, bridge).await?;
            }
        }
        "NOTICE" => debug!(?msg, "client NOTICE ignored"),
        "QUIT" => {
            let _ = write.shutdown().await;
            info!(%peer, "client quit");
            return Ok(true);
        }
        other => debug!(%peer, command = %other, "ignoring unsupported command"),
    }
    Ok(false)
}

async fn handle_cap(write: &mut OwnedWriteHalf, msg: &Message) -> Result<()> {
    match msg.params.first().map(String::as_str) {
        Some("LS") => {
            send(write, srv("CAP", vec!["*".into(), "LS".into(), "".into()])).await?;
        }
        Some("END") => {}
        Some("LIST") => {
            send(write, srv("CAP", vec!["*".into(), "LIST".into(), "".into()])).await?;
        }
        Some("REQ") => {
            let requested = msg.params.get(1).cloned().unwrap_or_default();
            send(write, srv("CAP", vec!["*".into(), "NAK".into(), requested])).await?;
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
            let topic = format!("Matrix room {room}");
            join_bridged(write, nick, chan, &room, &topic, bridge).await?;
            joined.insert(chan.to_string());
            continue;
        }
        send(write, srv("403", vec![nick.into(), chan.into(), "No such channel".into()])).await?;
    }
    Ok(())
}

async fn join_echo(
    write: &mut OwnedWriteHalf,
    nick: &str,
    joined: &mut HashSet<String>,
) -> Result<()> {
    let user_prefix = format!("{nick}!{nick}@matrirc.local");
    send(write, Message::with_prefix(&user_prefix, "JOIN", vec![ECHO_CHAN.into()])).await?;
    send(write, srv("332", vec![nick.into(), ECHO_CHAN.into(), ECHO_TOPIC.into()])).await?;
    send(write, srv("353", vec![nick.into(), "=".into(), ECHO_CHAN.into(), format!("{nick} {ECHO_NICK}")])).await?;
    send(write, srv("366", vec![nick.into(), ECHO_CHAN.into(), "End of /NAMES list".into()])).await?;
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
) -> Result<()> {
    send_join_lines(write, nick, chan, topic).await?;
    backfill_channel(write, chan, room, bridge).await?;
    Ok(())
}

async fn send_join_lines(
    write: &mut OwnedWriteHalf,
    nick: &str,
    chan: &str,
    topic: &str,
) -> Result<()> {
    let user_prefix = format!("{nick}!{nick}@matrirc.local");
    send(write, Message::with_prefix(&user_prefix, "JOIN", vec![chan.into()])).await?;
    send(write, srv("332", vec![nick.into(), chan.into(), topic.into()])).await?;
    send(write, srv("353", vec![nick.into(), "=".into(), chan.into(), format!("{nick} matrix")])).await?;
    send(write, srv("366", vec![nick.into(), chan.into(), "End of /NAMES list".into()])).await?;
    Ok(())
}

async fn auto_join_all(
    write: &mut OwnedWriteHalf,
    nick: &str,
    bridge: &Bridge,
    joined: &mut HashSet<String>,
) -> Result<()> {
    let snapshot = bridge.snapshot();
    let dm_count = bridge.dm_count();
    if snapshot.is_empty() && dm_count == 0 {
        info!(%nick, "auto-join: no rooms mapped yet");
        matrirc_notice(write, nick, "no Matrix rooms mapped yet; sync still in progress (channels will auto-join when ready)").await?;
        return Ok(());
    }
    info!(%nick, channels = snapshot.len(), dms = dm_count, "auto-join: sending JOIN for all bridged rooms");
    let mut new_joins = Vec::new();
    for (chan, room) in &snapshot {
        if joined.contains(chan) { continue; }
        let topic = format!("Matrix room {room}");
        send_join_lines(write, nick, chan, &topic).await?;
        joined.insert(chan.clone());
        new_joins.push((chan.clone(), room.clone()));
    }
    let names: Vec<String> = new_joins.iter().map(|(c, _)| c.clone()).collect();
    matrirc_notice(
        write,
        nick,
        &format!(
            "bridged {} channel(s) + {} DM(s). channels: {}",
            names.len(),
            dm_count,
            if names.is_empty() { "(none)".into() } else { names.join(", ") }
        ),
    )
    .await?;
    for (chan, room) in new_joins {
        backfill_channel(write, &chan, &room, bridge).await?;
    }
    Ok(())
}

async fn matrirc_notice(write: &mut OwnedWriteHalf, nick: &str, body: &str) -> Result<()> {
    send(
        write,
        Message::with_prefix("matrirc!matrirc@matrirc.local", "NOTICE", vec![nick.into(), body.into()]),
    )
    .await
}

async fn backfill_channel(
    write: &mut OwnedWriteHalf,
    chan: &str,
    room: &matrix_sdk::ruma::RoomId,
    bridge: &Bridge,
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
    for m in msgs {
        let prefix = format!("{}!{0}@matrix", m.sender_nick);
        for piece in m.body.split('\n') {
            if piece.is_empty() { continue; }
            send(write, Message::with_prefix(&prefix, "PRIVMSG", vec![chan.into(), piece.into()])).await?;
        }
    }
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
        let user_prefix = format!("{nick}!{nick}@matrirc.local");
        let mut params = vec![chan.to_string()];
        if !reason.is_empty() {
            params.push(reason.clone());
        }
        send(write, Message::with_prefix(user_prefix, "PART", params)).await?;
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

    if let Some(room) = bridge.room_for(target) {
        if let Err(e) = bridge.to_matrix.try_send(ToMatrix::Send {
            room,
            body: text.clone(),
        }) {
            warn!("dropping outbound matrix message: {e}");
            send(write, srv("NOTICE", vec![nick.into(), format!("matrix send dropped: {e}")])).await?;
        }
        return Ok(());
    }

    if let Some(room) = bridge.dm_room_for(target) {
        if let Err(e) = bridge.to_matrix.try_send(ToMatrix::Send {
            room,
            body: text.clone(),
        }) {
            warn!("dropping outbound DM: {e}");
            send(write, srv("NOTICE", vec![nick.into(), format!("DM send dropped: {e}")])).await?;
        }
        return Ok(());
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
    send(write, srv("372", vec![n.clone(), format!("- try also /join {ECHO_CHAN} for the local echo channel.")])).await?;
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
