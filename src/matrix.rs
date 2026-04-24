use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use matrix_sdk::authentication::matrix::MatrixSession;
use matrix_sdk::config::SyncSettings;
use matrix_sdk::room::MessagesOptions;
use matrix_sdk::ruma::events::reaction::SyncReactionEvent;
use matrix_sdk::ruma::events::room::encrypted::SyncRoomEncryptedEvent;
use matrix_sdk::ruma::events::room::message::{
    MessageType, Relation, RoomMessageEventContent, SyncRoomMessageEvent,
};
use matrix_sdk::ruma::events::room::topic::SyncRoomTopicEvent;
use matrix_sdk::ruma::events::{
    AnySyncMessageLikeEvent, AnySyncTimelineEvent, SyncMessageLikeEvent,
};
use matrix_sdk::store::RoomLoadSettings;
use matrix_sdk::{Client, EncryptionState, Room, RoomMemberships, RoomState, SessionMeta, SessionTokens};
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

/// IRC mIRC colour codes. 14 = grey, 5 = red, 15 = silver. Reset with \x0f.
const C_GREY: &str = "\x0314";
const C_RED: &str = "\x0305";
const C_SILVER: &str = "\x0315";
const C_RESET: &str = "\x0f";

async fn sender_nick(room: &Room, sender: &matrix_sdk::ruma::UserId) -> String {
    let display = match room.get_member_no_sync(sender).await {
        Ok(Some(m)) => m.display_name().map(ToOwned::to_owned),
        _ => None,
    };
    match display {
        Some(d) => sanitize_nick(&d),
        None => mxid_localpart(sender.as_str()).to_string(),
    }
}

fn sanitize_nick(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || "-_|[]{}".contains(c) {
            out.push(c);
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }
    let out = out.trim_matches('_').to_string();
    if out.is_empty() {
        return "_".into();
    }
    let mut capped: String = out.chars().take(16).collect();
    if capped.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) {
        capped.insert(0, '_');
    }
    capped
}

fn body_from_event(content: &RoomMessageEventContent, homeserver: &str) -> Option<String> {
    if let Some(Relation::Replacement(repl)) = &content.relates_to {
        let new_body = msgtype_body(&repl.new_content.msgtype, homeserver)?;
        return Some(format!("{C_GREY}* edit:{C_RESET} {}", strip_reply_fallback(&new_body)));
    }
    let raw = msgtype_body(&content.msgtype, homeserver)?;
    if matches!(content.msgtype, MessageType::Emote(_)) {
        return Some(raw);
    }
    let is_reply = matches!(&content.relates_to, Some(Relation::Reply { .. }))
        || matches!(&content.relates_to, Some(Relation::Thread(t)) if !t.is_falling_back);
    if is_reply {
        Some(format!("{C_GREY}↳{C_RESET} {}", strip_reply_fallback(&raw)))
    } else {
        Some(raw)
    }
}

fn msgtype_body(msg: &MessageType, homeserver: &str) -> Option<String> {
    match msg {
        MessageType::Text(t) => Some(t.body.clone()),
        MessageType::Notice(t) => Some(t.body.clone()),
        MessageType::Emote(t) => Some(format!("\x01ACTION {}\x01", t.body)),
        MessageType::Image(m) => Some(media_line("image", &m.body, &m.source, homeserver)),
        MessageType::File(m) => Some(media_line("file", &m.body, &m.source, homeserver)),
        MessageType::Audio(m) => Some(media_line("audio", &m.body, &m.source, homeserver)),
        MessageType::Video(m) => Some(media_line("video", &m.body, &m.source, homeserver)),
        MessageType::Location(m) => Some(format!("{C_SILVER}[location]{C_RESET} {}", m.body)),
        MessageType::ServerNotice(m) => Some(format!("{C_GREY}[server-notice]{C_RESET} {}", m.body)),
        _ => None,
    }
}

fn media_line(
    kind: &str,
    caption: &str,
    source: &matrix_sdk::ruma::events::room::MediaSource,
    homeserver: &str,
) -> String {
    use matrix_sdk::ruma::events::room::MediaSource;
    let url = match source {
        MediaSource::Plain(mxc) => mxc_to_https(mxc.as_str(), homeserver).unwrap_or_else(|| mxc.to_string()),
        MediaSource::Encrypted(file) => {
            let base = mxc_to_https(file.url.as_str(), homeserver).unwrap_or_else(|| file.url.to_string());
            format!("{base} (encrypted)")
        }
    };
    format!("{C_SILVER}[{kind}]{C_RESET} {caption} <{url}>")
}

fn mxc_to_https(mxc: &str, homeserver: &str) -> Option<String> {
    let rest = mxc.strip_prefix("mxc://")?;
    let (server, media_id) = rest.split_once('/')?;
    Some(format!(
        "{}/_matrix/media/v3/download/{server}/{media_id}",
        homeserver.trim_end_matches('/')
    ))
}

/// Matrix replies embed "> <sender> quoted\n> ...\n\nactual body" in `body`
/// for clients that don't render the relation. Trim back to the actual body.
fn strip_reply_fallback(body: &str) -> String {
    if !body.starts_with("> ") {
        return body.to_string();
    }
    match body.split_once("\n\n") {
        Some((_, rest)) => rest.to_string(),
        None => body.to_string(),
    }
}

async fn send_to_mxid(
    client: &Client,
    bridge: &Bridge,
    mxid: &matrix_sdk::ruma::UserId,
    body: &str,
) {
    let room = match existing_dm_room(client, mxid).await {
        Some(r) => r,
        None => match client.create_dm(mxid).await {
            Ok(r) => r,
            Err(e) => {
                warn!("create_dm {mxid} failed: {e:#}");
                notify_failure(bridge, mxid.as_str(), &format!("could not start DM: {e}"));
                return;
            }
        },
    };
    let rid = room.room_id().to_owned();
    let nick = dm_peer_nick(client, &room)
        .await
        .unwrap_or_else(|| mxid_localpart(mxid.as_str()).to_string());
    bridge.add_dm(rid.clone(), nick);
    send_to_room(client, bridge, &rid, body).await;
}

async fn existing_dm_room(client: &Client, mxid: &matrix_sdk::ruma::UserId) -> Option<Room> {
    for room in client.rooms() {
        if !room.is_direct().await.unwrap_or(false) {
            continue;
        }
        let members = room.members(RoomMemberships::JOIN | RoomMemberships::INVITE).await.ok()?;
        if members.iter().any(|m| m.user_id() == mxid) {
            return Some(room);
        }
    }
    None
}

fn notify_failure(bridge: &Bridge, dest: &str, reason: &str) {
    let _ = bridge.from_matrix.send(FromMatrix::Message {
        room: matrix_sdk::ruma::OwnedRoomId::try_from("!local:matrirc.local").unwrap(),
        sender_nick: "matrirc".into(),
        body: format!("[to {dest}] {reason}"),
    });
}

async fn send_to_room(
    client: &Client,
    bridge: &Bridge,
    room_id: &matrix_sdk::ruma::RoomId,
    body: &str,
) {
    let Some(room) = client.get_room(room_id) else {
        warn!("matrix room not found: {room_id}");
        return;
    };
    let content = RoomMessageEventContent::text_plain(body);
    match room.send(content).await {
        Ok(resp) => bridge.note_sent_by_us(resp.event_id),
        Err(e) => {
            warn!(%room_id, "matrix send failed: {e:#}");
            let _ = bridge.from_matrix.send(FromMatrix::Message {
                room: room_id.to_owned(),
                sender_nick: "matrirc".into(),
                body: format!("[send failed: {e}]"),
            });
        }
    }
}

async fn fetch_members(client: &Client, room_id: &matrix_sdk::ruma::RoomId) -> Vec<String> {
    let Some(room) = client.get_room(room_id) else { return Vec::new(); };
    let members = match room.members(RoomMemberships::JOIN).await {
        Ok(m) => m,
        Err(e) => {
            warn!("members {room_id} failed: {e}");
            return Vec::new();
        }
    };
    members
        .into_iter()
        .map(|m| match m.display_name() {
            Some(d) => sanitize_nick(d),
            None => mxid_localpart(m.user_id().as_str()).to_string(),
        })
        .collect()
}

async fn dm_peer_nick(client: &Client, room: &Room) -> Option<String> {
    let me = client.user_id()?;
    let members = room
        .members(RoomMemberships::JOIN | RoomMemberships::INVITE)
        .await
        .ok()?;
    // Must match sender_nick()'s output: display name (sanitized) first, MXID
    // localpart fallback. Otherwise /msg <nick> won't route to the same room
    // that inbound messages appear from.
    members.into_iter().find(|m| m.user_id() != me).map(|m| match m.display_name() {
        Some(d) => sanitize_nick(d),
        None => mxid_localpart(m.user_id().as_str()).to_string(),
    })
}

async fn backfill(
    client: &Client,
    room_id: &matrix_sdk::ruma::RoomId,
    limit: u32,
    homeserver: &str,
) -> Vec<BackfillMessage> {
    let Some(room) = client.get_room(room_id) else { return Vec::new(); };
    if matches!(room.encryption_state(), EncryptionState::Encrypted) {
        // Pre-fetch megolm keys from server backup so history decrypts. Silent on
        // failure (no backup yet, or network hiccup).
        if let Err(e) = client
            .encryption()
            .backups()
            .download_room_keys_for_room(room_id)
            .await
        {
            tracing::debug!(room = %room_id, "key backup download skipped: {e}");
        }
    }
    let mut collected = Vec::<matrix_sdk::deserialized_responses::TimelineEvent>::new();
    let mut next_token: Option<String> = None;
    while collected.len() < limit as usize {
        let mut opts = MessagesOptions::backward();
        let want = std::cmp::min(limit as usize - collected.len(), 100) as u32;
        opts.limit = want.into();
        if let Some(t) = &next_token {
            opts = opts.from(t.as_str());
        }
        let page = match room.messages(opts).await {
            Ok(m) => m,
            Err(e) => {
                warn!("backfill {room_id} page failed: {e}");
                break;
            }
        };
        if page.chunk.is_empty() {
            break;
        }
        collected.extend(page.chunk);
        match page.end {
            Some(t) => next_token = Some(t),
            None => break,
        }
    }
    let mut out = Vec::new();
    for ev in collected.iter().rev() {
        let Ok(parsed) = ev.raw().deserialize() else { continue; };
        match parsed {
            AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(
                SyncMessageLikeEvent::Original(orig),
            )) => {
                let Some(body) = body_from_event(&orig.content, homeserver) else { continue; };
                out.push(BackfillMessage {
                    sender_nick: sender_nick(&room, &orig.sender).await,
                    body,
                    origin_ms: orig.origin_server_ts.0.into(),
                });
            }
            AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomEncrypted(
                SyncMessageLikeEvent::Original(orig),
            )) => {
                out.push(BackfillMessage {
                    sender_nick: sender_nick(&room, &orig.sender).await,
                    body: format!("{C_RED}[encrypted — run `matrirc bootstrap-e2ee` to decrypt]{C_RESET}"),
                    origin_ms: orig.origin_server_ts.0.into(),
                });
            }
            AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::Reaction(
                SyncMessageLikeEvent::Original(orig),
            )) => {
                out.push(BackfillMessage {
                    sender_nick: sender_nick(&room, &orig.sender).await,
                    body: format!("\x01ACTION reacted {}\x01", orig.content.relates_to.key),
                    origin_ms: orig.origin_server_ts.0.into(),
                });
            }
            _ => continue,
        }
    }
    out
}

pub async fn build_client_restored(cfg: &Config) -> Result<Client> {
    let client = new_client(&cfg.homeserver_url).await?;
    client
        .matrix_auth()
        .restore_session(session_from_cfg(cfg)?, RoomLoadSettings::default())
        .await
        .context("restore session")?;
    Ok(client)
}

async fn new_client(homeserver: &str) -> Result<Client> {
    let store = store_path()?;
    ensure_secret_dir(&store)?;
    Client::builder()
        .homeserver_url(homeserver)
        .sqlite_store(&store, None)
        .build()
        .await
        .context("build matrix client")
}

fn session_from_cfg(cfg: &Config) -> Result<MatrixSession> {
    let user_id = matrix_sdk::ruma::OwnedUserId::try_from(cfg.mxid.as_str())
        .with_context(|| format!("parse mxid {}", cfg.mxid))?;
    let device_id = matrix_sdk::ruma::OwnedDeviceId::from(cfg.device_id.as_str());
    Ok(MatrixSession {
        meta: SessionMeta { user_id, device_id },
        tokens: SessionTokens {
            access_token: cfg.access_token.clone(),
            refresh_token: None,
        },
    })
}

pub async fn bootstrap_e2ee(recovery_key: String) -> Result<()> {
    let cfg_path = crate::config::config_path()?;
    let cfg = Config::load(&cfg_path)
        .with_context(|| format!("load config {}", cfg_path.display()))?;
    let store = store_path()?;

    println!("bootstrap-e2ee: {} on {}", cfg.mxid, cfg.homeserver_url);

    let client = build_client_restored(&cfg).await?;
    client
        .sync_once(SyncSettings::default())
        .await
        .context("initial sync")?;
    client
        .encryption()
        .recovery()
        .recover(recovery_key.as_str())
        .await
        .context("recover with recovery key")?;
    drop(recovery_key);

    // Self-sign with the imported self-signing key so other clients trust us
    // enough to share megolm sessions.
    let verified = match client.encryption().get_own_device().await.context("get own device")? {
        Some(device) => device.verify().await.is_ok(),
        None => false,
    };

    println!("✓ secrets imported");
    println!("{} device verified", if verified { "✓" } else { "✗" });
    println!();
    println!("stored:");
    println!("  {}  (token, 0600)", cfg_path.display());
    println!("  {}  (crypto store, 0700)", store.display());
    println!("recovery key not persisted.");
    println!("next: restart daemon.");
    Ok(())
}

#[cfg(unix)]
fn ensure_secret_dir(p: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(p).with_context(|| format!("create {}", p.display()))?;
    let perms = std::fs::Permissions::from_mode(0o700);
    std::fs::set_permissions(p, perms)
        .with_context(|| format!("chmod 0700 {}", p.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn ensure_secret_dir(p: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(p).with_context(|| format!("create {}", p.display()))
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
    let client = build_client_restored(&cfg).await?;
    let initial = client
        .sync_once(SyncSettings::default())
        .await
        .context("initial sync")?;

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
                    if matches!(room.encryption_state(), EncryptionState::Encrypted) {
                        let c = client.clone();
                        let rid = room.room_id().to_owned();
                        tokio::spawn(async move {
                            if let Err(e) = c
                                .encryption()
                                .backups()
                                .download_room_keys_for_room(&rid)
                                .await
                            {
                                tracing::debug!(room = %rid, "DM key download failed: {e}");
                            }
                        });
                    }
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
        "bridge populated; starting sync loop"
    );

    let bridge_for_topic = bridge.clone();
    client.add_event_handler(move |ev: SyncRoomTopicEvent, room: Room| {
        let bridge = bridge_for_topic.clone();
        async move {
            if let Some(orig) = ev.as_original() {
                bridge.update_topic(room.room_id(), orig.content.topic.clone());
            }
        }
    });


    let bridge_for_reactions = bridge.clone();
    client.add_event_handler(move |ev: SyncReactionEvent, room: Room| {
        let bridge = bridge_for_reactions.clone();
        async move {
            if !bridge.has_room(room.room_id()) { return; }
            let Some(orig) = ev.as_original() else { return; };
            if bridge.take_if_sent_by_us(&orig.event_id) { return; }
            let key = &orig.content.relates_to.key;
            let nick = sender_nick(&room, &orig.sender).await;
            let _ = bridge.from_matrix.send(FromMatrix::Message {
                room: room.room_id().to_owned(),
                sender_nick: nick.clone(),
                body: format!("\x01ACTION reacted {key}\x01"),
            });
        }
    });

    // UTD path: SDK couldn't decrypt; surface a placeholder so the user sees activity.
    let bridge_for_utd = bridge.clone();
    client.add_event_handler(move |ev: SyncRoomEncryptedEvent, room: Room| {
        let bridge = bridge_for_utd.clone();
        async move {
            let rid = room.room_id();
            if !bridge.has_room(rid) {
                tracing::debug!(%rid, "drop UTD: room not mapped");
                return;
            }
            let Some(orig) = ev.as_original() else { return; };
            if bridge.take_if_sent_by_us(&orig.event_id) { return; }
            let nick = sender_nick(&room, &orig.sender).await;
            tracing::info!(%rid, %nick, "inbound UTD (no key)");
            let _ = bridge.from_matrix.send(FromMatrix::Message {
                room: rid.to_owned(),
                sender_nick: nick,
                body: format!("{C_RED}[encrypted — run `matrirc bootstrap-e2ee` once to decrypt]{C_RESET}"),
            });
        }
    });

    let bridge_for_handler = bridge.clone();
    let homeserver_for_handler = cfg.homeserver_url.clone();
    client.add_event_handler(move |ev: SyncRoomMessageEvent, room: Room| {
        let bridge = bridge_for_handler.clone();
        let homeserver = homeserver_for_handler.clone();
        async move {
            let rid = room.room_id();
            if !bridge.has_room(rid) {
                tracing::debug!(%rid, "drop: room not mapped");
                return;
            }
            let Some(orig) = ev.as_original() else { return; };
            if bridge.take_if_sent_by_us(&orig.event_id) {
                return;
            }
            let Some(body) = body_from_event(&orig.content, &homeserver) else {
                tracing::debug!(%rid, "drop: unsupported msgtype");
                return;
            };
            let nick = sender_nick(&room, &orig.sender).await;
            tracing::info!(%rid, %nick, bytes = body.len(), "inbound message");
            let _ = bridge.from_matrix.send(FromMatrix::Message {
                room: rid.to_owned(),
                sender_nick: nick,
                body,
            });
        }
    });

    let send_client = client.clone();
    let send_bridge = bridge.clone();
    let homeserver_for_sender = cfg.homeserver_url.clone();
    tokio::spawn(async move {
        while let Some(cmd) = to_matrix.recv().await {
            match cmd {
                ToMatrix::Send { room, body } => {
                    send_to_room(&send_client, &send_bridge, &room, &body).await;
                }
                ToMatrix::SendToMxid { mxid, body } => {
                    send_to_mxid(&send_client, &send_bridge, &mxid, &body).await;
                }
                ToMatrix::Backfill { room, limit, reply } => {
                    let result = backfill(&send_client, &room, limit, &homeserver_for_sender).await;
                    let _ = reply.send(result);
                }
                ToMatrix::Members { room, reply } => {
                    let _ = reply.send(fetch_members(&send_client, &room).await);
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

pub async fn login_with_password(
    homeserver: &str,
    mxid: &str,
    password: &str,
) -> Result<(Config, Client)> {
    let store = store_path()?;
    ensure_secret_dir(&store)?;
    let client = Client::builder()
        .homeserver_url(homeserver)
        .sqlite_store(&store, None)
        .build()
        .await
        .context("build matrix client")?;
    let device_display = device_display_name();
    let resp = client
        .matrix_auth()
        .login_username(mxid, password)
        .initial_device_display_name(&device_display)
        .send()
        .await
        .context("m.login.password")?;
    let cfg = Config {
        mxid: resp.user_id.to_string(),
        homeserver_url: homeserver.trim_end_matches('/').to_string(),
        access_token: resp.access_token,
        device_id: resp.device_id.to_string(),
    };
    Ok((cfg, client))
}

pub async fn login_with_token(
    homeserver: &str,
    mxid: &str,
    token: &str,
) -> Result<(Config, Client)> {
    let http = reqwest::Client::builder()
        .user_agent(concat!("matrirc/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let who = whoami(&http, homeserver, token).await?;
    if who.user_id != mxid {
        return Err(anyhow!(
            "token belongs to {} but you specified {mxid}",
            who.user_id
        ));
    }
    let device_id = who
        .device_id
        .ok_or_else(|| anyhow!("homeserver did not return a device_id; token may be guest"))?;
    let cfg = Config {
        mxid: mxid.to_string(),
        homeserver_url: homeserver.trim_end_matches('/').to_string(),
        access_token: token.to_string(),
        device_id,
    };
    let client = build_client_restored(&cfg).await?;
    Ok((cfg, client))
}

fn device_display_name() -> String {
    let host = hostname().unwrap_or_else(|| "unknown".to_string());
    format!("matrirc ({host})")
}

fn hostname() -> Option<String> {
    std::env::var("HOSTNAME").ok().or_else(|| {
        std::process::Command::new("hostname")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    })
}

/// Prints encryption posture + actionable hints. Does a couple of short extra
/// syncs first so any to-device secret-send in flight gets applied before we
/// read the state.
pub async fn report_encryption_state(client: &Client) {
    use matrix_sdk::encryption::recovery::RecoveryState;
    use std::time::Duration;

    for _ in 0..3 {
        if let Err(e) = client.sync_once(SyncSettings::default().timeout(Duration::from_secs(10))).await {
            warn!("post-verify sync failed: {e:#}");
            break;
        }
    }

    let verified = matches!(client.encryption().get_own_device().await, Ok(Some(d)) if d.is_verified());
    let backup_on_server = client.encryption().backups().are_enabled().await;
    let recovery_state = client.encryption().recovery().state();

    println!("  device cross-signed:     {}", if verified { "yes" } else { "no" });
    println!("  server-side key backup:  {}", if backup_on_server { "exists" } else { "none" });
    println!("  local recovery state:    {:?}", recovery_state);
    println!();
    match recovery_state {
        RecoveryState::Enabled => {
            println!("✓ backup key present. /part+/join an encrypted channel to pull old keys.");
        }
        RecoveryState::Incomplete => {
            println!("partial secrets. Options:");
            println!("  - Element → open matrirc session → 'Share session keys'");
            println!("  - matrirc bootstrap-e2ee   (import via recovery key)");
        }
        RecoveryState::Disabled => {
            println!("account has no key backup — no way to pull old megolm keys.");
            println!("Set up key backup in Element, then retry login.");
        }
        RecoveryState::Unknown => {
            println!("state still resolving — try again shortly or run bootstrap-e2ee.");
        }
    }
}

/// Runs SAS emoji verification against another already-verified device.
/// `Ok(true)` on success, `Ok(false)` if already trusted, `Err` on failure.
pub async fn run_sas_bootstrap(client: &Client) -> Result<bool> {
    use matrix_sdk::encryption::verification::{SasState, VerificationRequestState};
    use std::time::Duration;

    client
        .sync_once(SyncSettings::default())
        .await
        .context("initial sync")?;

    if matches!(client.encryption().get_own_device().await, Ok(Some(d)) if d.is_verified()) {
        return Ok(false);
    }

    let own_id = client.user_id().ok_or_else(|| anyhow!("no user id"))?.to_owned();
    let identity = client
        .encryption()
        .request_user_identity(&own_id)
        .await
        .context("request own identity")?
        .ok_or_else(|| anyhow!("no cross-signing identity — set up key backup in Element first"))?;
    let request = identity
        .request_verification()
        .await
        .context("start verification request")?;

    println!("matrirc sent a verification request.");
    println!("→ Element → Settings → Sessions → {} → Verify. Waiting 5 min ...", device_display_name());

    wait_until(&request, Duration::from_secs(300), |r| match r.state() {
        VerificationRequestState::Ready { .. } => Some(Ok(false)),
        VerificationRequestState::Done => Some(Ok(true)),
        VerificationRequestState::Cancelled(info) => Some(Err(anyhow!("cancelled: {:?}", info))),
        _ => None,
    })
    .await??;

    let sas = request
        .start_sas()
        .await
        .context("start_sas")?
        .ok_or_else(|| anyhow!("peer did not support SAS"))?;

    wait_until(&sas, Duration::from_secs(60), |s| match s.state() {
        SasState::Cancelled(info) => Some(Err(anyhow!("SAS cancelled: {:?}", info))),
        SasState::Done { .. } => Some(Ok(true)),
        _ => s.emoji().map(|_| Ok(false)),
    })
    .await??;

    if let Some(emoji) = sas.emoji() {
        println!();
        println!("compare with the other device:");
        for e in &emoji {
            println!("  {} ({})", e.symbol, e.description);
        }
        println!();
    }

    use std::io::Write;
    eprint!("match? [y/N] ");
    std::io::stderr().flush().ok();
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer).context("read answer")?;
    if !answer.trim().eq_ignore_ascii_case("y") {
        let _ = sas.cancel().await;
        return Err(anyhow!("user cancelled"));
    }
    sas.confirm().await.context("confirm SAS")?;

    wait_until(&sas, Duration::from_secs(60), |s| match s.state() {
        SasState::Done { .. } => Some(Ok(true)),
        SasState::Cancelled(info) => Some(Err(anyhow!("cancelled after confirm: {:?}", info))),
        _ => None,
    })
    .await?
}

async fn wait_until<T, R>(
    item: &T,
    timeout: std::time::Duration,
    mut check: impl FnMut(&T) -> Option<R>,
) -> Result<R> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(r) = check(item) {
            return Ok(r);
        }
        if std::time::Instant::now() > deadline {
            return Err(anyhow!("timed out after {timeout:?}"));
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
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
