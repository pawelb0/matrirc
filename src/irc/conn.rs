use std::net::SocketAddr;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tracing::{debug, info, warn};

use super::proto::Message;

const SERVER_NAME: &str = "matrirc.local";
const VERSION: &str = concat!("matrirc-", env!("CARGO_PKG_VERSION"));
const MAX_LINE: usize = 8192; // RFC says 512; clients may send IRCv3 longer lines.

pub async fn handle(sock: TcpStream, peer: SocketAddr) -> Result<()> {
    let (read, mut write) = sock.into_split();
    let mut lines = BufReader::new(read).lines();

    let mut nick: Option<String> = None;
    let mut user: Option<String> = None;
    let mut registered = false;

    while let Some(line) = read_line(&mut lines).await? {
        let msg = match Message::parse(&line) {
            Ok(m) => m,
            Err(e) => {
                debug!(%peer, error = %e, raw = %line, "skipping malformed line");
                continue;
            }
        };

        match msg.command.as_str() {
            "CAP" => match msg.params.first().map(String::as_str) {
                Some("LS") => {
                    send(&mut write, Message::with_prefix(SERVER_NAME, "CAP", vec!["*".into(), "LS".into(), "".into()])).await?;
                }
                Some("END") => {}
                Some("LIST") => {
                    send(&mut write, Message::with_prefix(SERVER_NAME, "CAP", vec!["*".into(), "LIST".into(), "".into()])).await?;
                }
                Some("REQ") => {
                    let requested = msg.params.get(1).cloned().unwrap_or_default();
                    send(&mut write, Message::with_prefix(SERVER_NAME, "CAP", vec!["*".into(), "NAK".into(), requested])).await?;
                }
                _ => debug!(?msg, "ignoring CAP subcommand"),
            },
            "NICK" => {
                if let Some(n) = msg.params.first() {
                    nick = Some(n.clone());
                }
            }
            "USER" => {
                if let Some(u) = msg.params.first() {
                    user = Some(u.clone());
                }
            }
            "PING" => {
                let token = msg.params.first().cloned().unwrap_or_default();
                send(&mut write, Message::with_prefix(SERVER_NAME, "PONG", vec![SERVER_NAME.into(), token])).await?;
            }
            "QUIT" => {
                let _ = write.shutdown().await;
                info!(%peer, "client quit");
                return Ok(());
            }
            other => debug!(%peer, command = %other, "ignoring unsupported command"),
        }

        if !registered {
            if let (Some(n), Some(_)) = (&nick, &user) {
                send_welcome(&mut write, n).await?;
                registered = true;
            }
        }
    }

    info!(%peer, "client disconnected");
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

async fn send_welcome(write: &mut tokio::net::tcp::OwnedWriteHalf, nick: &str) -> Result<()> {
    let n = nick.to_string();
    send(write, m("001", vec![n.clone(), format!("Welcome to matrirc, {nick}")])).await?;
    send(write, m("002", vec![n.clone(), format!("Your host is {SERVER_NAME}, running {VERSION}")])).await?;
    send(write, m("003", vec![n.clone(), "This server has no creation date".into()])).await?;
    send(write, m("004", vec![n.clone(), SERVER_NAME.into(), VERSION.into(), "".into(), "".into()])).await?;
    send(write, m("375", vec![n.clone(), format!("- {SERVER_NAME} Message of the day -")])).await?;
    send(write, m("372", vec![n.clone(), "- matrirc step 1: bare IRC server. No Matrix yet.".into()])).await?;
    send(write, m("376", vec![n, "End of /MOTD command.".into()])).await?;
    Ok(())
}

fn m(command: &str, params: Vec<String>) -> Message {
    Message::with_prefix(SERVER_NAME, command, params)
}

async fn send(
    write: &mut tokio::net::tcp::OwnedWriteHalf,
    msg: Message,
) -> Result<()> {
    let mut wire = msg.to_wire();
    debug!(out = %wire, "send");
    wire.push_str("\r\n");
    write.write_all(wire.as_bytes()).await.context("write")?;
    Ok(())
}
