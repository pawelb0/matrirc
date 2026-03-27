use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use matrix_sdk::authentication::matrix::MatrixSession;
use matrix_sdk::config::SyncSettings;
use matrix_sdk::room::MessagesOptions;
use matrix_sdk::ruma::events::room::message::{
    MessageType, RoomMessageEventContent, SyncRoomMessageEvent,
};
use matrix_sdk::ruma::events::{
    AnySyncMessageLikeEvent, AnySyncTimelineEvent, SyncMessageLikeEvent,
};
use matrix_sdk::store::RoomLoadSettings;
use matrix_sdk::{Client, Room, RoomMemberships, RoomState, SessionMeta, SessionTokens};
use serde::Deserialize;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::bridge::{mxid_localpart, BackfillMessage, Bridge, FromMatrix, ToMatrix};
use crate::config::Config;
use crate::names::{preferred_channel_name, NameStore};

#[derive(Debug, Deserialize)]
struct WellKnown {
    #[serde(rename = "m.homeserver")]
    homeserver: WellKnownHomeserver,
}

#[derive(Debug, Deserialize)]
struct WellKnownHomeserver {
    base_url: String,
}

#[derive(Debug, Deserialize)]
pub struct WhoAmI {
    pub user_id: String,
    pub device_id: Option<String>,
}

pub fn server_name_from_mxid(mxid: &str) -> Result<&str> {
    let rest = mxid
        .strip_prefix('@')
        .ok_or_else(|| anyhow!("MXID must start with '@': {mxid}"))?;
    let (_local, server) = rest
        .split_once(':')
        .ok_or_else(|| anyhow!("MXID missing ':server': {mxid}"))?;
    if server.is_empty() {
        return Err(anyhow!("MXID has empty server name: {mxid}"));
    }
    Ok(server)
}

pub async fn discover_homeserver(http: &reqwest::Client, server_name: &str) -> Result<String> {
    let url = format!("https://{server_name}/.well-known/matrix/client");
    let resp = http.get(&url).send().await;
    if let Ok(r) = resp {
        if r.status().is_success() {
            if let Ok(wk) = r.json::<WellKnown>().await {
                return Ok(wk.homeserver.base_url.trim_end_matches('/').to_string());
            }
        }
    }
    Ok(format!("https://{server_name}"))
}

async fn dm_peer_nick(client: &Client, room: &Room) -> Option<String> {
    let me = client.user_id()?;
    let members = room
        .members(RoomMemberships::JOIN | RoomMemberships::INVITE)
        .await
        .ok()?;
    for m in members {
        if m.user_id() != me {
            return Some(mxid_localpart(m.user_id().as_str()).to_string());
        }
    }
    None
}

async fn backfill(client: &Client, room_id: &matrix_sdk::ruma::RoomId, limit: u32) -> Vec<BackfillMessage> {
    let Some(room) = client.get_room(room_id) else { return Vec::new(); };
    let mut opts = MessagesOptions::backward();
    opts.limit = limit.into();
    let msgs = match room.messages(opts).await {
        Ok(m) => m,
        Err(e) => {
            warn!("backfill {room_id} failed: {e}");
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for ev in msgs.chunk.iter().rev() {
        let Ok(AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(
            SyncMessageLikeEvent::Original(orig),
        ))) = ev.raw().deserialize() else { continue; };
        let body = match &orig.content.msgtype {
            MessageType::Text(t) => t.body.clone(),
            MessageType::Notice(t) => t.body.clone(),
            MessageType::Emote(t) => format!("\x01ACTION {}\x01", t.body),
            _ => continue,
        };
        out.push(BackfillMessage {
            sender_nick: mxid_localpart(orig.sender.as_str()).to_string(),
            body,
        });
    }
    out
}

pub fn store_path() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_DATA_HOME") {
        return Ok(PathBuf::from(dir).join("matrirc").join("store"));
    }
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME not set"))?;
    Ok(PathBuf::from(home)
        .join(".local")
        .join("share")
        .join("matrirc")
        .join("store"))
}

pub async fn run_sync(
    cfg: Config,
    bridge: Bridge,
    mut to_matrix: mpsc::Receiver<ToMatrix>,
    name_store: Arc<NameStore>,
    env_override_room: Option<matrix_sdk::ruma::OwnedRoomId>,
) -> Result<()> {
    let store = store_path()?;
    std::fs::create_dir_all(&store).with_context(|| format!("create {}", store.display()))?;

    let client = Client::builder()
        .homeserver_url(&cfg.homeserver_url)
        .sqlite_store(&store, None)
        .build()
        .await
        .context("build matrix client")?;

    let user_id = matrix_sdk::ruma::OwnedUserId::try_from(cfg.mxid.as_str())
        .with_context(|| format!("parse mxid {}", cfg.mxid))?;
    let device_id = matrix_sdk::ruma::OwnedDeviceId::from(cfg.device_id.as_str());

    let session = MatrixSession {
        meta: SessionMeta { user_id, device_id },
        tokens: SessionTokens {
            access_token: cfg.access_token.clone(),
            refresh_token: None,
        },
    };
    client
        .matrix_auth()
        .restore_session(session, RoomLoadSettings::default())
        .await
        .context("restore session")?;

    info!("matrix: session restored, running initial sync");

    let initial = client
        .sync_once(SyncSettings::default())
        .await
        .context("initial sync")?;
    info!("matrix: initial sync done (next_batch={})", initial.next_batch);

    for room in client.rooms() {
        let name = room
            .display_name()
            .await
            .map(|n| n.to_string())
            .unwrap_or_else(|_| "<no name>".to_string());
        let state = room.state();
        info!(
            target: "matrirc::rooms",
            room = %room.room_id(),
            "{name} [{:?}]",
            state
        );

        if !matches!(state, RoomState::Joined) {
            continue;
        }
        if let Some(only) = &env_override_room {
            if room.room_id() != only {
                continue;
            }
        }

        if room.is_direct().await.unwrap_or(false) {
            match dm_peer_nick(&client, &room).await {
                Some(nick) => {
                    bridge.add_dm(room.room_id().to_owned(), nick);
                }
                None => warn!("DM room {} has no identifiable peer", room.room_id()),
            }
            continue;
        }

        let preferred = preferred_channel_name(room.room_id(), Some(&name));
        let chan = match name_store.assign_or_get(room.room_id(), &preferred) {
            Ok(c) => c,
            Err(e) => {
                warn!("name assign failed for {}: {e}", room.room_id());
                continue;
            }
        };
        let topic = room.topic().unwrap_or_else(|| name.clone());
        bridge.add_mapping(room.room_id().to_owned(), chan, topic);
    }

    info!(
        channels = bridge.snapshot().len(),
        dms = bridge.dm_count(),
        "matrix: bridge populated"
    );
    info!("matrix: starting incremental sync loop");

    let bridge_for_handler = bridge.clone();
    client.add_event_handler(move |ev: SyncRoomMessageEvent, room: Room| {
        let bridge = bridge_for_handler.clone();
        async move {
            if !bridge.has_room(room.room_id()) {
                return;
            }
            let Some(orig) = ev.as_original() else { return; };
            // Suppress only events we just sent from IRC, identified by event ID.
            // Other devices' messages from the same MXID still flow through.
            if bridge.take_if_sent_by_us(&orig.event_id) {
                return;
            }
            let body = match &orig.content.msgtype {
                MessageType::Text(t) => t.body.clone(),
                MessageType::Notice(t) => t.body.clone(),
                MessageType::Emote(t) => format!("\x01ACTION {}\x01", t.body),
                _ => return,
            };
            let nick = mxid_localpart(orig.sender.as_str()).to_string();
            let _ = bridge.from_matrix.send(FromMatrix::Message {
                room: room.room_id().to_owned(),
                sender_nick: nick,
                body,
            });
        }
    });

    let send_client = client.clone();
    let send_bridge = bridge.clone();
    tokio::spawn(async move {
        while let Some(cmd) = to_matrix.recv().await {
            match cmd {
                ToMatrix::Send { room, body } => match send_client.get_room(&room) {
                    Some(r) => {
                        let content = RoomMessageEventContent::text_plain(&body);
                        match r.send(content).await {
                            Ok(resp) => send_bridge.note_sent_by_us(resp.event_id),
                            Err(e) => warn!("matrix send to {room} failed: {e}"),
                        }
                    }
                    None => warn!("matrix room not found: {room}"),
                },
                ToMatrix::Backfill { room, limit, reply } => {
                    let result = backfill(&send_client, &room, limit).await;
                    let _ = reply.send(result);
                }
            }
        }
    });

    let settings = SyncSettings::default().token(initial.next_batch);
    if let Err(e) = client.sync(settings).await {
        warn!("sync ended: {e}");
    }
    Ok(())
}

pub async fn whoami(http: &reqwest::Client, homeserver: &str, token: &str) -> Result<WhoAmI> {
    let url = format!("{homeserver}/_matrix/client/v3/account/whoami");
    let resp = http
        .get(&url)
        .bearer_auth(token)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("whoami failed ({status}): {body}"));
    }
    let who: WhoAmI = resp.json().await.context("parse whoami response")?;
    Ok(who)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mxid() {
        assert_eq!(server_name_from_mxid("@me:example.org").unwrap(), "example.org");
        assert_eq!(server_name_from_mxid("@a:matrix.org").unwrap(), "matrix.org");
    }

    #[test]
    fn rejects_bad_mxid() {
        assert!(server_name_from_mxid("me:example.org").is_err());
        assert!(server_name_from_mxid("@me").is_err());
        assert!(server_name_from_mxid("@me:").is_err());
    }
}
