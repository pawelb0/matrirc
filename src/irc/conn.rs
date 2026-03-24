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
                    }
                }
            }
            ev = from_matrix.recv() => {
                match ev {
                    Ok(FromMatrix::Message { room, sender_nick, body }) => {
                        let Some(chan) = bridge.mapping.room_to_chan.get(&room) else { continue; };
                        if !joined.contains(chan) { continue; }
                        let prefix = format!("{sender_nick}!{sender_nick}@matrix");
                        for piece in body.split('\n') {
                            if piece.is_empty() { continue; }
                            send(&mut write, Message::with_prefix(&prefix, "PRIVMSG", vec![chan.clone(), piece.to_string()])).await?;
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
        let bridged_room = bridge.mapping.chan_to_room.get(chan).cloned();
        let (topic, names) = if chan == ECHO_CHAN {
            (ECHO_TOPIC.to_string(), format!("{nick} {ECHO_NICK}"))
        } else if let Some(ref room_id) = bridged_room {
            (
                format!("Matrix room {room_id}"),
                format!("{nick} matrix"),
            )
        } else {
            send(write, srv("403", vec![nick.into(), chan.into(), "No such channel".into()])).await?;
            continue;
        };
        if !joined.insert(chan.to_string()) {
            continue;
        }
        let user_prefix = format!("{nick}!{nick}@matrirc.local");
        send(write, Message::with_prefix(&user_prefix, "JOIN", vec![chan.into()])).await?;
        send(write, srv("332", vec![nick.into(), chan.into(), topic])).await?;
        send(write, srv("353", vec![nick.into(), "=".into(), chan.into(), names])).await?;
        send(write, srv("366", vec![nick.into(), chan.into(), "End of /NAMES list".into()])).await?;

        if let Some(room) = bridged_room {
            backfill_channel(write, chan, &room, bridge).await?;
        }
    }
    Ok(())
}

async fn backfill_channel(
    write: &mut OwnedWriteHalf,
    chan: &str,
    room: &matrix_sdk::ruma::OwnedRoomId,
    bridge: &Bridge,
) -> Result<()> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    if bridge
        .to_matrix
        .try_send(ToMatrix::Backfill {
            room: room.clone(),
            limit: 20,
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

    if let Some(room) = bridge.mapping.chan_to_room.get(target) {
        if let Err(e) = bridge.to_matrix.try_send(ToMatrix::Send {
            room: room.clone(),
            body: text.clone(),
        }) {
            warn!("dropping outbound matrix message: {e}");
            send(write, srv("NOTICE", vec![nick.into(), format!("matrix send dropped: {e}")])).await?;
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
    send(write, srv("372", vec![n.clone(), format!("- matrirc step 2: try /join {ECHO_CHAN}")])).await?;
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
