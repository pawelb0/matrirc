use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use matrix_sdk::authentication::matrix::MatrixSession;
use matrix_sdk::config::SyncSettings;
use matrix_sdk::room::MessagesOptions;
use matrix_sdk::ruma::events::reaction::SyncReactionEvent;
use matrix_sdk::ruma::events::room::encrypted::SyncRoomEncryptedEvent;
use matrix_sdk::ruma::events::room::member::{MembershipChange, SyncRoomMemberEvent};
use matrix_sdk::ruma::events::room::message::{
    MessageType, Relation, RoomMessageEventContent, SyncRoomMessageEvent,
};
use matrix_sdk::ruma::events::receipt::{ReceiptThread, ReceiptType};
use matrix_sdk::ruma::events::room::topic::SyncRoomTopicEvent;
use matrix_sdk::ruma::events::room::MediaSource;
use matrix_sdk::ruma::events::{
    AnySyncMessageLikeEvent, AnySyncTimelineEvent, Mentions, SyncMessageLikeEvent,
};
use matrix_sdk::store::RoomLoadSettings;
use matrix_sdk::{Client, EncryptionState, Room, RoomMemberships, RoomState, SessionMeta, SessionTokens};
use serde::Deserialize;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::bridge::{
    mxid_localpart, BackfillMessage, Bridge, FromMatrix, RoomListing, ToMatrix, WhoisInfo,
};
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
    let fallback = format!("https://{server_name}");
    let url = format!("{fallback}/.well-known/matrix/client");
    let Ok(resp) = http.get(&url).send().await else { return Ok(fallback); };
    if !resp.status().is_success() { return Ok(fallback); }
    let Ok(wk) = resp.json::<WellKnown>().await else { return Ok(fallback); };
    Ok(wk.homeserver.base_url.trim_end_matches('/').to_string())
}

// mIRC colour codes; reset with \x0f.
const C_GREY: &str = "\x0314";
const C_RED: &str = "\x0305";
const C_SILVER: &str = "\x0315";
const C_RESET: &str = "\x0f";

/// Returns `(sender_nick, is_own)` if the event should be forwarded, `None` to drop.
async fn accept_event(
    bridge: &Bridge,
    room: &Room,
    event_id: &matrix_sdk::ruma::EventId,
    sender: &matrix_sdk::ruma::UserId,
    own: &matrix_sdk::ruma::UserId,
) -> Option<(String, bool)> {
    if !bridge.has_room(room.room_id()) { return None; }
    if bridge.take_if_sent_by_us(event_id) { return None; }
    let nick = sender_nick(room, sender).await;
    Some((nick, sender == own))
}

#[allow(clippy::too_many_arguments)]
fn emit_message(
    bridge: &Bridge,
    room: &matrix_sdk::ruma::RoomId,
    nick: String,
    body: String,
    reply_quote: Option<String>,
    event_id: Option<matrix_sdk::ruma::OwnedEventId>,
    is_own: bool,
    mentions_self: bool,
) {
    let _ = bridge.from_matrix.send(FromMatrix::Message {
        room: room.to_owned(),
        sender_nick: nick,
        body,
        event_id,
        reply_quote,
        is_own,
        mentions_self,
    });
}

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

/// Punctuation accepted in IRC nicks. Used by `sanitize_nick` and
/// `is_nick_char`; keep them in sync.
const NICK_PUNCT: &str = "-_|[]{}";

fn sanitize_nick(s: &str) -> String {
    // Remove accents if there are any.
    let s = unidecode::unidecode(s);

    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || NICK_PUNCT.contains(c) {
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

#[derive(Debug, Clone)]
struct MentionSpan {
    start: usize,
    end: usize,
    mxid: matrix_sdk::ruma::OwnedUserId,
    pill_text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MentionCandidate {
    start: usize,
    end: usize,
    /// Lowercased nick, ready to look up against `member_mention_index`.
    key: String,
    /// Span starts with `@`, so the rendered pill keeps the prefix.
    has_at: bool,
}

fn is_nick_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || NICK_PUNCT.as_bytes().contains(&c)
}

fn nick_span_end(body: &str, start: usize) -> Option<usize> {
    let bytes = body.as_bytes();
    let mut i = start;
    while i < bytes.len() && is_nick_char(bytes[i]) {
        i += 1;
    }
    (i > start).then_some(i)
}

/// True when `body[end..]` looks like the `:server` tail of an MXID.
fn looks_like_mxid_tail(body: &str, end: usize) -> bool {
    let bytes = body.as_bytes();
    if bytes.get(end) != Some(&b':') {
        return false;
    }
    bytes.get(end + 1).is_some_and(u8::is_ascii_alphanumeric)
}

fn scan_mention_candidates(body: &str) -> Vec<MentionCandidate> {
    let bytes = body.as_bytes();
    let mut out = Vec::new();
    if let Some(end) = nick_span_end(body, 0) {
        if let Some(&p) = bytes.get(end) {
            if matches!(p, b':' | b',') {
                let after = end + 1;
                let trailing_ok =
                    after >= bytes.len() || bytes[after].is_ascii_whitespace();
                if trailing_ok {
                    out.push(MentionCandidate {
                        start: 0,
                        end,
                        key: body[0..end].to_ascii_lowercase(),
                        has_at: false,
                    });
                }
            }
        }
    }
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'@' {
            i += 1;
            continue;
        }
        let boundary = i == 0 || !is_nick_char(bytes[i - 1]);
        if !boundary {
            i += 1;
            continue;
        }
        let Some(end) = nick_span_end(body, i + 1) else {
            i += 1;
            continue;
        };
        if looks_like_mxid_tail(body, end) {
            i = end;
            continue;
        }
        out.push(MentionCandidate {
            start: i,
            end,
            key: body[i + 1..end].to_ascii_lowercase(),
            has_at: true,
        });
        i = end;
    }
    out
}

fn html_escape_into(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
}

fn build_mention_html(body: &str, spans: &[MentionSpan]) -> String {
    let mut out = String::with_capacity(body.len() + spans.len() * 64);
    let mut i = 0;
    for s in spans {
        if s.start > i {
            html_escape_into(&body[i..s.start], &mut out);
        }
        out.push_str("<a href=\"https://matrix.to/#/");
        html_escape_into(s.mxid.as_str(), &mut out);
        out.push_str("\">");
        html_escape_into(&s.pill_text, &mut out);
        out.push_str("</a>");
        i = s.end;
    }
    if i < body.len() {
        html_escape_into(&body[i..], &mut out);
    }
    out
}

/// Drops ambiguous keys (>1 member with the same nick) so a typo never pings
/// the wrong account; callers fall back to plain text on miss.
async fn member_mention_index(
    room: &Room,
) -> HashMap<String, (matrix_sdk::ruma::OwnedUserId, String)> {
    let Ok(members) = room
        .members(RoomMemberships::JOIN | RoomMemberships::INVITE)
        .await
    else {
        return HashMap::new();
    };
    let mut counts: HashMap<String, usize> = HashMap::new();
    let mut entries: HashMap<String, (matrix_sdk::ruma::OwnedUserId, String)> =
        HashMap::new();
    for m in &members {
        let mxid = m.user_id().to_owned();
        let display = m.display_name().map(str::to_string);
        let pill = display
            .clone()
            .unwrap_or_else(|| mxid.localpart().to_string());
        let mut keys: Vec<String> = Vec::new();
        if let Some(d) = display.as_deref() {
            let s = sanitize_nick(d).to_ascii_lowercase();
            if !s.is_empty() && s != "_" {
                keys.push(s);
            }
        }
        let local = mxid.localpart().to_ascii_lowercase();
        if !local.is_empty() {
            keys.push(local);
        }
        for key in keys {
            *counts.entry(key.clone()).or_insert(0) += 1;
            entries
                .entry(key)
                .or_insert_with(|| (mxid.clone(), pill.clone()));
        }
    }
    counts
        .into_iter()
        .filter_map(|(k, n)| {
            if n == 1 {
                entries.remove(&k).map(|v| (k, v))
            } else {
                None
            }
        })
        .collect()
}

async fn resolve_mentions(room: &Room, body: &str) -> Vec<MentionSpan> {
    let candidates = scan_mention_candidates(body);
    if candidates.is_empty() {
        return Vec::new();
    }
    let index = member_mention_index(room).await;
    if index.is_empty() {
        return Vec::new();
    }
    candidates
        .into_iter()
        .filter_map(|c| {
            let (mxid, display) = index.get(&c.key)?;
            let pill_text = if c.has_at {
                format!("@{display}")
            } else {
                display.clone()
            };
            Some(MentionSpan {
                start: c.start,
                end: c.end,
                mxid: mxid.clone(),
                pill_text,
            })
        })
        .collect()
}

/// Decoded message body plus an optional one-line quote of the parent.
struct DecodedBody {
    body: String,
    /// Set when the event is a reply (or non-falling-back thread root). IRC
    /// layer prints it above the body so irssi shows the threading context.
    quote: Option<String>,
}

fn body_from_event(
    content: &RoomMessageEventContent,
    event_id: &matrix_sdk::ruma::EventId,
    attach_base: &str,
) -> Option<DecodedBody> {
    if let Some(Relation::Replacement(repl)) = &content.relates_to {
        let new_body = msgtype_body(&repl.new_content.msgtype, event_id, attach_base)?;
        return Some(DecodedBody {
            body: format!("{C_GREY}* edit:{C_RESET} {}", strip_reply_fallback(&new_body)),
            quote: None,
        });
    }
    let raw = msgtype_body(&content.msgtype, event_id, attach_base)?;
    if matches!(content.msgtype, MessageType::Emote(_)) {
        return Some(DecodedBody { body: raw, quote: None });
    }
    let is_reply = matches!(&content.relates_to, Some(Relation::Reply { .. }))
        || matches!(&content.relates_to, Some(Relation::Thread(t)) if !t.is_falling_back);
    if !is_reply {
        return Some(DecodedBody { body: raw, quote: None });
    }
    let quote = extract_reply_quote(&raw);
    let clean = strip_reply_fallback(&raw);
    // When a quote line will be emitted above the body, skip the inline `↳`
    // marker — past (backfill) and present (live) renders both end up as
    // "<quote>\n<clean body>" then.
    let body = if quote.is_some() {
        clean
    } else {
        format!("{C_GREY}↳{C_RESET} {clean}")
    };
    Some(DecodedBody { body, quote })
}

/// Pulls a single-line synopsis out of a matrix reply fallback. Matrix prefixes
/// the original event's body with `> <@user:server> ...` lines followed by a
/// blank line and the reply itself; we keep the first non-empty quoted line
/// (with the MXID compressed to its localpart for readability).
fn extract_reply_quote(body: &str) -> Option<String> {
    if !body.starts_with("> ") { return None; }
    let head = body.split_once("\n\n").map(|(q, _)| q).unwrap_or(body);
    let line = head.lines().next()?;
    let stripped = line.trim_start_matches('>').trim();
    let condensed = stripped
        .strip_prefix('<')
        .and_then(|s| s.split_once('>'))
        .map(|(mxid, rest)| {
            let nick = mxid_localpart(mxid);
            format!("<{nick}>{rest}")
        })
        .unwrap_or_else(|| stripped.to_string());
    Some(format!("{C_GREY}↳ {condensed}{C_RESET}"))
}

/// Strips the leading `{C_GREY}↳{C_RESET} ` we add in `body_from_event` when
/// the synthesise path later supplies a quote — keeps past/present rendering
/// consistent (quote above, plain body below).
fn strip_grey_arrow_prefix(body: String) -> String {
    let prefix = format!("{C_GREY}↳{C_RESET} ");
    body.strip_prefix(&prefix).map(str::to_string).unwrap_or(body)
}

/// Async fallback for replies whose body doesn't carry the matrix `>`-quoted
/// fallback (matrirc's own pre-fix outbounds, mostly). Fetches the parent
/// event from the room store and builds a single-line `↳ <sender> snippet`.
/// Returns `None` if the event isn't a reply, the parent can't be fetched,
/// or its content isn't a text-like message.
async fn synthesise_reply_quote(
    room: &Room,
    content: &RoomMessageEventContent,
) -> Option<String> {
    let target_id = match &content.relates_to {
        Some(Relation::Reply { in_reply_to }) => in_reply_to.event_id.clone(),
        Some(Relation::Thread(t)) if !t.is_falling_back => t.in_reply_to.as_ref()?.event_id.clone(),
        _ => return None,
    };
    let evt = room.event(&target_id, None).await.ok()?;
    let parsed = evt.raw().deserialize().ok()?;
    let AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(
        SyncMessageLikeEvent::Original(orig),
    )) = parsed
    else { return None; };
    let body = match &orig.content.msgtype {
        MessageType::Text(t) => t.body.clone(),
        MessageType::Notice(t) => t.body.clone(),
        MessageType::Emote(t) => format!("/me {}", t.body),
        _ => return None,
    };
    let first_line = strip_reply_fallback(&body);
    let snippet = first_line.lines().next().unwrap_or("").trim();
    if snippet.is_empty() { return None; }
    let nick = sender_nick(room, &orig.sender).await;
    let capped: String = snippet.chars().take(60).collect();
    let suffix = if snippet.chars().count() > 60 { "..." } else { "" };
    Some(format!("{C_GREY}↳ <{nick}> {capped}{suffix}{C_RESET}"))
}

fn msgtype_body(
    msg: &MessageType,
    event_id: &matrix_sdk::ruma::EventId,
    attach_base: &str,
) -> Option<String> {
    match msg {
        MessageType::Text(t) => Some(t.body.clone()),
        MessageType::Notice(t) => Some(t.body.clone()),
        MessageType::Emote(t) => Some(format!("\x01ACTION {}\x01", t.body)),
        MessageType::Image(m) => Some(media_line("image", &m.body, event_id, attach_base)),
        MessageType::File(m) => Some(media_line("file", &m.body, event_id, attach_base)),
        MessageType::Audio(m) => Some(media_line("audio", &m.body, event_id, attach_base)),
        MessageType::Video(m) => Some(media_line("video", &m.body, event_id, attach_base)),
        MessageType::Location(m) => Some(format!("{C_SILVER}[location]{C_RESET} {}", m.body)),
        MessageType::ServerNotice(m) => Some(format!("{C_GREY}[server-notice]{C_RESET} {}", m.body)),
        _ => None,
    }
}

fn media_line(
    kind: &str,
    caption: &str,
    event_id: &matrix_sdk::ruma::EventId,
    attach_base: &str,
) -> String {
    format!(
        "{C_SILVER}[{kind}]{C_RESET} {caption} <{attach_base}/attach/{event_id}>"
    )
}

fn media_source_of(msg: &MessageType) -> Option<MediaSource> {
    match msg {
        MessageType::Image(m) => Some(m.source.clone()),
        MessageType::File(m) => Some(m.source.clone()),
        MessageType::Audio(m) => Some(m.source.clone()),
        MessageType::Video(m) => Some(m.source.clone()),
        _ => None,
    }
}

fn index_attachments(
    index: &crate::proxy::AttachIndex,
    event_id: &matrix_sdk::ruma::EventId,
    content: &RoomMessageEventContent,
) {
    if let Some(src) = media_source_of(&content.msgtype) {
        index.insert(event_id.to_owned(), src);
    }
    if let Some(Relation::Replacement(repl)) = &content.relates_to {
        if let Some(src) = media_source_of(&repl.new_content.msgtype) {
            index.insert(event_id.to_owned(), src);
        }
    }
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
    emote: bool,
    notice: bool,
    in_reply_to: Option<matrix_sdk::ruma::OwnedEventId>,
) {
    let room = match find_or_create_dm(client, mxid).await {
        Ok(r) => r,
        Err(e) => {
            warn!(%mxid, "DM open/create failed: {e:#}");
            return;
        }
    };
    let rid = room.room_id().to_owned();
    let nick = dm_peer_nick(client, &room)
        .await
        .unwrap_or_else(|| mxid_localpart(mxid.as_str()).to_string());
    // Hint the canonical nick since the user's query window is currently
    // labelled with the MXID form they typed.
    let _ = bridge.from_matrix.send(FromMatrix::DmAdded { nick: nick.clone() });
    let localpart = mxid_localpart(mxid.as_str()).to_string();
    bridge.add_dm(rid.clone(), nick, &[mxid.as_str(), &localpart]);
    send_to_room(client, bridge, &rid, body, emote, notice, in_reply_to).await;
}

async fn find_or_create_dm(client: &Client, mxid: &matrix_sdk::ruma::UserId) -> Result<Room> {
    for room in client.rooms() {
        if !room.is_direct().await.unwrap_or(false) {
            continue;
        }
        let Ok(members) = room.members(RoomMemberships::JOIN | RoomMemberships::INVITE).await else {
            continue;
        };
        if members.iter().any(|m| m.user_id() == mxid) {
            return Ok(room);
        }
    }
    client.create_dm(mxid).await.context("create_dm")
}

fn plain_content(body: &str, emote: bool, notice: bool) -> RoomMessageEventContent {
    if emote {
        RoomMessageEventContent::emote_plain(body)
    } else if notice {
        RoomMessageEventContent::notice_plain(body)
    } else {
        RoomMessageEventContent::text_plain(body)
    }
}

fn html_content(
    body: &str,
    html: String,
    emote: bool,
    notice: bool,
) -> RoomMessageEventContent {
    if emote {
        RoomMessageEventContent::emote_html(body, html)
    } else if notice {
        RoomMessageEventContent::notice_html(body, html)
    } else {
        RoomMessageEventContent::text_html(body, html)
    }
}

fn build_outgoing_content(
    body: &str,
    mentions: &[MentionSpan],
    emote: bool,
    notice: bool,
) -> RoomMessageEventContent {
    if mentions.is_empty() {
        return plain_content(body, emote, notice);
    }
    let html = build_mention_html(body, mentions);
    let mxids: Vec<matrix_sdk::ruma::OwnedUserId> =
        mentions.iter().map(|m| m.mxid.clone()).collect();
    html_content(body, html, emote, notice)
        .add_mentions(Mentions::with_user_ids(mxids))
}

/// Wraps `content` as a reply to `target_id`, populating both the
/// `m.in_reply_to` relation and the `> <sender> ...\n\nbody` body fallback
/// (and adding the original author to `m.mentions`). Falls back to a bare
/// relation when the target event isn't reachable, so we never lose the link.
async fn build_reply_content(
    room: &Room,
    content: RoomMessageEventContent,
    target_id: &matrix_sdk::ruma::EventId,
) -> RoomMessageEventContent {
    use matrix_sdk::ruma::events::room::message::{
        AddMentions, ForwardThread, SyncRoomMessageEvent,
    };
    use matrix_sdk::ruma::events::{AnySyncMessageLikeEvent, AnySyncTimelineEvent};
    if let Ok(evt) = room.event(target_id, None).await {
        if let Ok(AnySyncTimelineEvent::MessageLike(
            AnySyncMessageLikeEvent::RoomMessage(SyncRoomMessageEvent::Original(orig)),
        )) = evt.raw().deserialize()
        {
            let full = orig.into_full_event(room.room_id().to_owned());
            return content.make_reply_to(&full, ForwardThread::Yes, AddMentions::Yes);
        }
    }
    use matrix_sdk::ruma::events::relation::InReplyTo;
    use matrix_sdk::ruma::events::room::message::Relation;
    let mut fallback = content;
    fallback.relates_to = Some(Relation::Reply { in_reply_to: InReplyTo::new(target_id.to_owned()) });
    fallback
}

async fn send_to_room(
    client: &Client,
    bridge: &Bridge,
    room_id: &matrix_sdk::ruma::RoomId,
    body: &str,
    emote: bool,
    notice: bool,
    in_reply_to: Option<matrix_sdk::ruma::OwnedEventId>,
) {
    let Some(room) = client.get_room(room_id) else {
        warn!("matrix room not found: {room_id}");
        return;
    };
    let mentions = resolve_mentions(&room, body).await;
    let mut content = build_outgoing_content(body, &mentions, emote, notice);
    if let Some(target) = in_reply_to {
        content = build_reply_content(&room, content, &target).await;
    }
    match room.send(content).await {
        Ok(resp) => bridge.note_sent_by_us(resp.event_id),
        Err(e) => {
            warn!(%room_id, "matrix send failed: {e:#}");
            let _ = bridge.from_matrix.send(FromMatrix::Message {
                room: room_id.to_owned(),
                sender_nick: "matrirc".into(),
                body: format!("[send failed: {e}]"),
                event_id: None,
                reply_quote: None,
                is_own: false,
                mentions_self: false,
            });
        }
    }
}

async fn search_rooms(client: &Client, query: &str, server: Option<&str>) -> Vec<RoomListing> {
    use matrix_sdk::ruma::api::client::directory::get_public_rooms_filtered;
    use matrix_sdk::ruma::directory::Filter;

    let mut req = get_public_rooms_filtered::v3::Request::new();
    let mut filter = Filter::new();
    if !query.is_empty() {
        filter.generic_search_term = Some(query.to_string());
    }
    req.filter = filter;
    req.limit = Some(20u32.into());
    if let Some(s) = server {
        if let Ok(name) = matrix_sdk::ruma::OwnedServerName::try_from(s) {
            req.server = Some(name);
        }
    }

    let resp = match client.public_rooms_filtered(req).await {
        Ok(r) => r,
        Err(e) => {
            warn!("public_rooms_filtered failed: {e:#}");
            return Vec::new();
        }
    };
    resp.chunk
        .into_iter()
        .map(|c| RoomListing {
            alias: c.canonical_alias.map(|a| a.to_string()),
            room_id: c.room_id.to_string(),
            name: c.name.unwrap_or_default(),
            members: u64::from(c.num_joined_members),
        })
        .collect()
}

async fn join_by_alias(
    client: &Client,
    bridge: &Bridge,
    name_store: &Arc<NameStore>,
    alias: &str,
) -> Result<String, String> {
    use matrix_sdk::ruma::{OwnedServerName, RoomAliasId, RoomOrAliasId};

    let parsed = <&RoomOrAliasId>::try_from(alias).map_err(|e| format!("bad alias: {e}"))?;
    // For aliases, resolve first so we can pass the directory's `servers` as
    // `via` hints. Without those, the homeserver answers
    // `M_UNKNOWN: no servers that are in the room have been provided` whenever
    // the alias's host isn't itself a resident of the room.
    let via: Vec<OwnedServerName> = if let Ok(alias_id) = <&RoomAliasId>::try_from(alias) {
        match client.resolve_room_alias(alias_id).await {
            Ok(r) => r.servers,
            Err(e) => {
                warn!(%alias, "resolve_room_alias failed, joining without via: {e:#}");
                Vec::new()
            }
        }
    } else {
        alias
            .rsplit_once(':')
            .and_then(|(_, server)| OwnedServerName::try_from(server).ok())
            .into_iter()
            .collect()
    };
    let room = client
        .join_room_by_id_or_alias(parsed, &via)
        .await
        .map_err(|e| format!("{e:#}"))?;

    register_joined_room(client, bridge, name_store, &room).await;
    bridge
        .chan_for(room.room_id())
        .or_else(|| bridge.dm_nick_for(room.room_id()))
        .ok_or_else(|| "joined but no chan registered (DM detection?)".into())
}

/// Registers a joined room. DMs route through `add_dm`; channels assign a
/// slug, register aliases, and broadcast `RoomAdded`. Idempotent.
async fn register_joined_room(
    client: &Client,
    bridge: &Bridge,
    name_store: &Arc<NameStore>,
    room: &Room,
) {
    let rid = room.room_id();
    if room.is_direct().await.unwrap_or(false) {
        register_dm(client, bridge, room).await;
        return;
    }
    let name = room
        .display_name()
        .await
        .map(|n| n.to_string())
        .unwrap_or_else(|_| "<no name>".into());
    let preferred = preferred_channel_name(rid, Some(&name));
    let chan = match name_store.assign_or_get(rid, &preferred) {
        Ok(c) => c,
        Err(e) => {
            warn!(%rid, "name assign: {e}");
            return;
        }
    };
    let topic = room.topic().unwrap_or_else(|| name.clone());
    let mut alias_strings: Vec<String> = room
        .canonical_alias()
        .into_iter()
        .map(|a| a.to_string())
        .chain(room.alt_aliases().into_iter().map(|a| a.to_string()))
        .collect();
    alias_strings.sort();
    alias_strings.dedup();
    let alias_refs: Vec<&str> = alias_strings.iter().map(String::as_str).collect();
    bridge.add_mapping(rid.to_owned(), chan, topic, &alias_refs);
}

async fn whois_lookup(client: &Client, nick: &str) -> Option<WhoisInfo> {
    let needle = nick.to_ascii_lowercase();
    let me = client.user_id()?;
    let mut hit_mxid: Option<matrix_sdk::ruma::OwnedUserId> = None;
    let mut hit_display: Option<String> = None;
    let mut rooms = Vec::new();
    for room in client.rooms() {
        let Ok(members) = room.members(RoomMemberships::JOIN).await else { continue };
        for m in members {
            if m.user_id() == me { continue; }
            let raw_display = m.display_name();
            let matches = raw_display.map(sanitize_nick).as_deref().map(str::to_ascii_lowercase) == Some(needle.clone())
                || mxid_localpart(m.user_id().as_str()).eq_ignore_ascii_case(nick);
            if !matches { continue; }
            if hit_mxid.is_none() {
                hit_mxid = Some(m.user_id().to_owned());
                hit_display = raw_display.map(ToOwned::to_owned);
            }
            if hit_mxid.as_deref() == Some(m.user_id()) {
                let name = room.display_name().await.map(|n| n.to_string()).unwrap_or_default();
                rooms.push(if name.is_empty() { room.room_id().to_string() } else { name });
            }
        }
    }
    hit_mxid.map(|mxid| WhoisInfo {
        nick: nick.to_string(),
        mxid: mxid.to_string(),
        display_name: hit_display,
        rooms,
    })
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

async fn register_dm(client: &Client, bridge: &Bridge, room: &Room) {
    let Some((nick, mxid)) = dm_peer(client, room).await else {
        warn!(room = %room.room_id(), "DM has no identifiable peer");
        return;
    };
    let localpart = mxid_localpart(mxid.as_str()).to_string();
    bridge.add_dm(room.room_id().to_owned(), nick, &[mxid.as_str(), &localpart]);
    // DMs skip backfill, so pull any server-side megolm keys in the background.
    if matches!(room.encryption_state(), EncryptionState::Encrypted) {
        let c = client.clone();
        let rid = room.room_id().to_owned();
        tokio::spawn(async move {
            if let Err(e) = c.encryption().backups().download_room_keys_for_room(&rid).await {
                tracing::debug!(%rid, "DM key download: {e}");
            }
        });
    }
}

async fn dm_peer_nick(client: &Client, room: &Room) -> Option<String> {
    dm_peer(client, room).await.map(|(nick, _)| nick)
}

/// Returns `(canonical_nick, mxid)` for the non-self member of a DM room.
/// Canonical nick matches `sender_nick()` so inbound and outbound align.
async fn dm_peer(client: &Client, room: &Room) -> Option<(String, matrix_sdk::ruma::OwnedUserId)> {
    let me = client.user_id()?;
    let members = room.members(RoomMemberships::JOIN | RoomMemberships::INVITE).await.ok()?;

    // First try to find a member that matches the room's display name.
    // This is common in bridged DMs where the room is named after the peer.
    let room_display_name = room.display_name().await.ok().map(|n| n.to_string());
    if let Some(ref rdn) = room_display_name {
        if let Some(m) = members.iter().find(|m| {
            m.user_id() != me && m.display_name() == Some(rdn.as_str())
        }) {
            let nick = sanitize_nick(rdn);
            return Some((nick, m.user_id().to_owned()));
        }
    }

    members.into_iter().find(|m| m.user_id() != me).map(|m| {
        let nick = m.display_name()
            .map(sanitize_nick)
            .unwrap_or_else(|| mxid_localpart(m.user_id().as_str()).to_string());
        (nick, m.user_id().to_owned())
    })
}

/// Decides if `ev` is an own message or not.
fn event_is_own(
    ev: &matrix_sdk::deserialized_responses::TimelineEvent,
    me: Option<&matrix_sdk::ruma::UserId>,
) -> bool {
    let Some(me) = me else { return false; };
    matches!(
        ev.raw().get_field::<matrix_sdk::ruma::OwnedUserId>("sender"),
        Ok(Some(sender)) if sender.as_str() == me.as_str()
    )
}

/// Index of the first already-seen event in `events`. None when all events are unread.
fn unread_boundary(
    events: &[matrix_sdk::deserialized_responses::TimelineEvent],
    read_markers: &[matrix_sdk::ruma::OwnedEventId],
    me: Option<&matrix_sdk::ruma::UserId>,
) -> Option<usize> {
    events.iter().position(|ev| {
        ev.event_id().is_some_and(|id| read_markers.contains(&id)) || event_is_own(ev, me)
    })
}

async fn backfill(
    client: &Client,
    room_id: &matrix_sdk::ruma::RoomId,
    limit: u32,
    only_unread: bool,
    attach_base: &str,
    attach_index: &crate::proxy::AttachIndex,
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
    let mut read_markers = Vec::<matrix_sdk::ruma::OwnedEventId>::new();
    if only_unread {
        // Backfill only unread messages: collect various read receipts.
        if let Some(me) = client.user_id() {
            for receipt_type in [ReceiptType::Read, ReceiptType::ReadPrivate] {
                for thread in [ReceiptThread::Main, ReceiptThread::Unthreaded] {
                    match room.load_user_receipt(receipt_type.clone(), thread.clone(), me).await {
                        Ok(Some((event_id, _))) => read_markers.push(event_id),
                        Ok(None) => {}
                        Err(e) => warn!(room = %room_id, "read receipt load failed: {e}"),
                    }
                }
            }
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
        if only_unread {
            if let Some(pos) = unread_boundary(&collected, &read_markers, client.user_id()) {
                collected.truncate(pos);
                break;
            }
        }
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
                index_attachments(attach_index, &orig.event_id, &orig.content);
                let Some(mut decoded) = body_from_event(&orig.content, &orig.event_id, attach_base) else { continue; };
                if decoded.quote.is_none() {
                    decoded.quote = synthesise_reply_quote(&room, &orig.content).await;
                    if decoded.quote.is_some() {
                        decoded.body = strip_grey_arrow_prefix(decoded.body);
                    }
                }
                out.push(BackfillMessage {
                    sender_nick: sender_nick(&room, &orig.sender).await,
                    body: decoded.body,
                    reply_quote: decoded.quote,
                    origin_ms: orig.origin_server_ts.0.into(),
                    event_id: orig.event_id.clone(),
                    is_own: Some(orig.sender.as_ref()) == client.user_id(),
                });
            }
            AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomEncrypted(
                SyncMessageLikeEvent::Original(orig),
            )) => {
                out.push(BackfillMessage {
                    sender_nick: sender_nick(&room, &orig.sender).await,
                    body: format!("{C_RED}[encrypted — run `matrirc bootstrap-e2ee` to decrypt]{C_RESET}"),
                    reply_quote: None,
                    origin_ms: orig.origin_server_ts.0.into(),
                    event_id: orig.event_id.clone(),
                    is_own: Some(orig.sender.as_ref()) == client.user_id(),
                });
            }
            AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::Reaction(
                SyncMessageLikeEvent::Original(orig),
            )) => {
                out.push(BackfillMessage {
                    sender_nick: sender_nick(&room, &orig.sender).await,
                    body: format!("\x01ACTION reacted {}\x01", orig.content.relates_to.key),
                    reply_quote: None,
                    origin_ms: orig.origin_server_ts.0.into(),
                    event_id: orig.event_id.clone(),
                    is_own: Some(orig.sender.as_ref()) == client.user_id(),
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
    let cfg = Config::load(&cfg_path).with_context(|| format!("load {}", cfg_path.display()))?;
    println!("bootstrap-e2ee: {} on {}", cfg.mxid, cfg.homeserver_url);

    let client = build_client_restored(&cfg).await?;
    client.sync_once(SyncSettings::default()).await.context("initial sync")?;
    client.encryption().recovery().recover(&recovery_key).await.context("recover")?;
    drop(recovery_key);

    // Self-sign with the imported self-signing key so other clients trust us.
    let verified = matches!(
        client.encryption().get_own_device().await.context("get own device")?,
        Some(d) if d.verify().await.is_ok()
    );

    println!("✓ secrets imported");
    println!("{} device verified", if verified { "✓" } else { "✗" });
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
    let initial = loop {
        match client.sync_once(SyncSettings::default()).await {
            Ok(r) => break r,
            Err(e) => {
                warn!("initial sync failed, retrying in 10s: {e:#}");
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            }
        }
    };

    for room in client.rooms() {
        let rid = room.room_id();
        let name = room.display_name().await.map(|n| n.to_string()).unwrap_or_else(|_| "<no name>".into());
        info!(target: "matrirc::rooms", room = %rid, "{name} [{:?}]", room.state());
        if !matches!(room.state(), RoomState::Joined) { continue; }
        if env_override_room.as_deref().map(|o| o != rid).unwrap_or(false) { continue; }
        register_joined_room(&client, &bridge, &name_store, &room).await;
    }

    info!(
        channels = bridge.snapshot().len(),
        dms = bridge.dm_count(),
        "bridge populated; starting sync loop"
    );

    let attach_index = crate::proxy::AttachIndex::new();
    let attach_addr: std::net::SocketAddr = std::env::var("MATRIRC_ATTACH_BIND")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| "127.0.0.1:6680".parse().unwrap());
    let attach_base = format!("http://{attach_addr}");
    {
        let proxy_client = client.clone();
        let proxy_index = attach_index.clone();
        let proxy_bridge = bridge.clone();
        tokio::spawn(async move {
            if let Err(e) = crate::proxy::run_proxy(
                attach_addr,
                proxy_client,
                proxy_index,
                proxy_bridge,
            ).await {
                warn!("attach proxy stopped: {e:#}");
            }
        });
    }

    {
        let bridge = bridge.clone();
        client.add_event_handler(move |ev: SyncRoomTopicEvent, room: Room| {
            let bridge = bridge.clone();
            async move {
                if let Some(orig) = ev.as_original() {
                    bridge.update_topic(room.room_id(), orig.content.topic.clone());
                }
            }
        });
    }

    let own_id = client.user_id().ok_or_else(|| anyhow!("client has no user_id"))?.to_owned();

    {
        let bridge = bridge.clone();
        let own = own_id.clone();
        client.add_event_handler(move |ev: SyncReactionEvent, room: Room| {
            let bridge = bridge.clone();
            let own = own.clone();
            async move {
                let Some(orig) = ev.as_original() else { return; };
                let Some((nick, is_own)) = accept_event(&bridge, &room, &orig.event_id, &orig.sender, &own).await else { return; };
                emit_message(&bridge, room.room_id(), nick,
                    format!("\x01ACTION reacted {}\x01", orig.content.relates_to.key),
                    None, Some(orig.event_id.clone()), is_own, false);
            }
        });
    }

    // UTD path: SDK couldn't decrypt; surface a placeholder so the user sees activity.
    {
        let bridge = bridge.clone();
        let own = own_id.clone();
        client.add_event_handler(move |ev: SyncRoomEncryptedEvent, room: Room| {
            let bridge = bridge.clone();
            let own = own.clone();
            async move {
                let Some(orig) = ev.as_original() else { return; };
                let Some((nick, is_own)) = accept_event(&bridge, &room, &orig.event_id, &orig.sender, &own).await else { return; };
                emit_message(&bridge, room.room_id(), nick,
                    format!("{C_RED}[encrypted — run `matrirc bootstrap-e2ee` once to decrypt]{C_RESET}"),
                    None, Some(orig.event_id.clone()), is_own, false);
            }
        });
    }

    {
        let bridge = bridge.clone();
        let own = own_id.clone();
        let name_store_for_member = name_store.clone();
        let client_for_member = client.clone();
        client.add_event_handler(move |ev: SyncRoomMemberEvent, room: Room| {
            let bridge = bridge.clone();
            let own = own.clone();
            let name_store = name_store_for_member.clone();
            let client = client_for_member.clone();
            async move {
                let Some(orig) = ev.as_original() else { return; };
                if orig.state_key == own {
                    match orig.membership_change() {
                        MembershipChange::Joined | MembershipChange::InvitationAccepted => {
                            register_joined_room(&client, &bridge, &name_store, &room).await;
                        }
                        MembershipChange::Left
                        | MembershipChange::Kicked
                        | MembershipChange::Banned
                        | MembershipChange::KickedAndBanned => {
                            bridge.remove_mapping(room.room_id());
                            bridge.remove_dm(room.room_id());
                        }
                        _ => {}
                    }
                    return;
                }
                let Some(chan) = bridge.chan_for(room.room_id()) else { return; };
                let nick = match orig.content.displayname.as_deref() {
                    Some(d) => sanitize_nick(d),
                    None => mxid_localpart(orig.state_key.as_str()).to_string(),
                };
                let event = match orig.membership_change() {
                    MembershipChange::Joined | MembershipChange::InvitationAccepted => {
                        FromMatrix::MemberJoined { chan, nick }
                    }
                    MembershipChange::Left
                    | MembershipChange::Kicked
                    | MembershipChange::Banned
                    | MembershipChange::KickedAndBanned => {
                        let reason = orig.content.reason.clone();
                        FromMatrix::MemberLeft { chan, nick, reason }
                    }
                    _ => return,
                };
                let _ = bridge.from_matrix.send(event);
            }
        });
    }

    {
        let bridge = bridge.clone();
        let own = own_id;
        let attach_base = attach_base.clone();
        let attach_index = attach_index.clone();
        client.add_event_handler(move |ev: SyncRoomMessageEvent, room: Room| {
            let bridge = bridge.clone();
            let own = own.clone();
            let attach_base = attach_base.clone();
            let attach_index = attach_index.clone();
            async move {
                let Some(orig) = ev.as_original() else { return; };
                let Some((nick, is_own)) = accept_event(&bridge, &room, &orig.event_id, &orig.sender, &own).await else { return; };
                index_attachments(&attach_index, &orig.event_id, &orig.content);
                let Some(mut decoded) = body_from_event(&orig.content, &orig.event_id, &attach_base) else { return; };
                if decoded.quote.is_none() {
                    decoded.quote = synthesise_reply_quote(&room, &orig.content).await;
                    if decoded.quote.is_some() {
                        decoded.body = strip_grey_arrow_prefix(decoded.body);
                    }
                }
                let mentions_self = orig
                    .content
                    .mentions
                    .as_ref()
                    .is_some_and(|m| m.user_ids.contains(&own));
                emit_message(&bridge, room.room_id(), nick, decoded.body, decoded.quote,
                    Some(orig.event_id.clone()), is_own, mentions_self);
            }
        });
    }

    let send_client = client.clone();
    let send_bridge = bridge.clone();
    let attach_base_sender = attach_base.clone();
    let attach_index_sender = attach_index.clone();
    let name_store_for_sender = name_store.clone();
    tokio::spawn(async move {
        while let Some(cmd) = to_matrix.recv().await {
            match cmd {
                ToMatrix::Send { room, body, emote, notice, in_reply_to } => {
                    send_to_room(&send_client, &send_bridge, &room, &body, emote, notice, in_reply_to).await;
                }
                ToMatrix::SendToMxid { mxid, body, emote, notice, in_reply_to } => {
                    send_to_mxid(&send_client, &send_bridge, &mxid, &body, emote, notice, in_reply_to).await;
                }
                ToMatrix::Backfill { room, limit, reply } => {
                    let only_unread = !send_bridge
                        .default_backfill_read_messages
                        .load(std::sync::atomic::Ordering::Relaxed);
                    let result = backfill(&send_client, &room, limit, only_unread, &attach_base_sender, &attach_index_sender).await;
                    let _ = reply.send(result);
                }
                ToMatrix::Members { room, reply } => {
                    let _ = reply.send(fetch_members(&send_client, &room).await);
                }
                ToMatrix::SearchRooms { query, server, reply } => {
                    let _ = reply.send(search_rooms(&send_client, &query, server.as_deref()).await);
                }
                ToMatrix::JoinByAlias { alias, reply } => {
                    let _ = reply.send(
                        join_by_alias(&send_client, &send_bridge, &name_store_for_sender, &alias).await,
                    );
                }
                ToMatrix::Whois { nick, reply } => {
                    let _ = reply.send(whois_lookup(&send_client, &nick).await);
                }
                ToMatrix::SetDisplayName { name } => {
                    if let Err(e) = send_client.account().set_display_name(Some(&name)).await {
                        warn!("set display name: {e:#}");
                    }
                }
                ToMatrix::LeaveRoom { room, reply } => {
                    let result = match send_client.get_room(&room) {
                        Some(r) => r.leave().await
                            .map(|_| ())
                            .map_err(|e| format!("{e:#}")),
                        None => Err("unknown room".into()),
                    };
                    let _ = reply.send(result);
                }
                ToMatrix::SetTopic { room, topic } => {
                    if let Some(r) = send_client.get_room(&room) {
                        if let Err(e) = r.set_room_topic(&topic).await {
                            warn!(%room, "set topic: {e:#}");
                        }
                    }
                }
                ToMatrix::Knock { target, reason, reply } => {
                    use matrix_sdk::ruma::{OwnedRoomOrAliasId, OwnedServerName};
                    let result = match OwnedRoomOrAliasId::try_from(target.as_str()) {
                        Err(e) => Err(format!("bad target: {e}")),
                        Ok(parsed) => {
                            let via: Vec<OwnedServerName> = target
                                .rsplit_once(':')
                                .and_then(|(_, s)| OwnedServerName::try_from(s).ok())
                                .into_iter()
                                .collect();
                            match send_client.knock(parsed, reason, via).await {
                                Ok(room) => Ok(format!("knock sent for {}", room.room_id())),
                                Err(e) => Err(format!("{e:#}")),
                            }
                        }
                    };
                    let _ = reply.send(result);
                }
            }
        }
    });

    let mut settings = SyncSettings::default().token(initial.next_batch);
    loop {
        match client.sync(settings.clone()).await {
            Ok(()) => break,
            Err(e) => {
                warn!("sync error, retrying in 10s: {e:#}");
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                settings = SyncSettings::default();
            }
        }
    }
    Ok(())
}

pub async fn login_with_password(
    homeserver: &str,
    mxid: &str,
    password: &str,
) -> Result<(Config, Client)> {
    let client = new_client(homeserver).await?;
    let resp = client
        .matrix_auth()
        .login_username(mxid, password)
        .initial_device_display_name(&device_display_name())
        .send()
        .await
        .context("m.login.password")?;
    Ok((
        Config {
            mxid: resp.user_id.to_string(),
            homeserver_url: homeserver.trim_end_matches('/').to_string(),
            access_token: resp.access_token,
            device_id: resp.device_id.to_string(),
            show_reply_ids: true,
            backfill_read_messages: true,
        },
        client,
    ))
}

pub async fn login_with_token(homeserver: &str, mxid: &str, token: &str) -> Result<(Config, Client)> {
    let http = reqwest::Client::builder()
        .user_agent(concat!("matrirc/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let who = whoami(&http, homeserver, token).await?;
    if who.user_id != mxid {
        return Err(anyhow!("token belongs to {} not {mxid}", who.user_id));
    }
    let device_id = who.device_id.ok_or_else(|| anyhow!("no device_id returned (guest token?)"))?;
    let cfg = Config {
        mxid: mxid.to_string(),
        homeserver_url: homeserver.trim_end_matches('/').to_string(),
        access_token: token.to_string(),
        device_id,
        show_reply_ids: true,
        backfill_read_messages: true,
    };
    let client = build_client_restored(&cfg).await?;
    Ok((cfg, client))
}

fn device_display_name() -> String {
    let host = std::env::var("HOSTNAME").ok()
        .or_else(|| std::process::Command::new("hostname").output().ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "unknown".into());
    format!("matrirc ({host})")
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
    let recovery_label = match recovery_state {
        RecoveryState::Enabled => "enabled",
        RecoveryState::Incomplete => "incomplete",
        RecoveryState::Disabled => "disabled",
        RecoveryState::Unknown => "unknown",
    };

    println!("  device cross-signed:     {}", if verified { "yes" } else { "no" });
    println!("  server-side key backup:  {}", if backup_on_server { "exists" } else { "none" });
    println!("  local recovery state:    {recovery_label}");
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
    use matrix_sdk::ruma::events::room::message::{
        EmoteMessageEventContent, ImageMessageEventContent, NoticeMessageEventContent,
        TextMessageEventContent,
    };
    use matrix_sdk::ruma::OwnedMxcUri;

    const ATTACH: &str = "http://127.0.0.1:6680";

    fn content(m: MessageType) -> RoomMessageEventContent {
        RoomMessageEventContent::new(m)
    }

    fn evt(s: &str) -> matrix_sdk::ruma::OwnedEventId {
        matrix_sdk::ruma::EventId::parse(s).unwrap()
    }

    #[test]
    fn parses_mxid() {
        assert_eq!(server_name_from_mxid("@me:example.org").unwrap(), "example.org");
        assert_eq!(server_name_from_mxid("@a:matrix.org").unwrap(), "matrix.org");
    }

    #[test]
    fn rejects_bad_mxid() {
        for bad in ["me:example.org", "@me", "@me:", "", "@"] {
            assert!(server_name_from_mxid(bad).is_err(), "{bad:?} should fail");
        }
    }

    #[test]
    fn body_text_and_notice_pass_through() {
        let id = evt("$abc:server.tld");
        let t = content(MessageType::Text(TextMessageEventContent::plain("hi")));
        assert_eq!(body_from_event(&t, &id, ATTACH).map(|d| d.body).as_deref(), Some("hi"));
        let n = content(MessageType::Notice(NoticeMessageEventContent::plain("bye")));
        assert_eq!(body_from_event(&n, &id, ATTACH).map(|d| d.body).as_deref(), Some("bye"));
    }

    #[test]
    fn body_emote_wraps_ctcp_action() {
        let id = evt("$abc:server.tld");
        let e = content(MessageType::Emote(EmoteMessageEventContent::plain("waves")));
        assert_eq!(body_from_event(&e, &id, ATTACH).map(|d| d.body).as_deref(), Some("\x01ACTION waves\x01"));
    }

    #[test]
    fn body_image_emits_attach_proxy_url() {
        let id = evt("$abc:server.tld");
        let mxc = OwnedMxcUri::from("mxc://example.org/abc123");
        let img = content(MessageType::Image(ImageMessageEventContent::plain("kitten.png".into(), mxc)));
        let out = body_from_event(&img, &id, ATTACH).unwrap().body;
        assert!(out.contains("[image]"), "{out}");
        assert!(out.contains("kitten.png"), "{out}");
        assert!(out.contains("http://127.0.0.1:6680/attach/$abc:server.tld"), "{out}");
    }

    #[test]
    fn strip_reply_fallback_noop_when_no_prefix() {
        assert_eq!(strip_reply_fallback("plain text"), "plain text");
    }

    #[test]
    fn strip_reply_fallback_drops_quoted_header() {
        let src = "> <@a:h> first line\n> second line\n\nactual reply";
        assert_eq!(strip_reply_fallback(src), "actual reply");
    }

    fn cand(start: usize, end: usize, key: &str, has_at: bool) -> MentionCandidate {
        MentionCandidate { start, end, key: key.into(), has_at }
    }

    #[test]
    fn scan_leading_nick_colon() {
        let v = scan_mention_candidates("alice: hi");
        assert_eq!(v, vec![cand(0, 5, "alice", false)]);
    }

    #[test]
    fn scan_leading_nick_comma() {
        let v = scan_mention_candidates("bob, hello");
        assert_eq!(v, vec![cand(0, 3, "bob", false)]);
    }

    #[test]
    fn scan_leading_no_trailing_space() {
        assert_eq!(scan_mention_candidates("alice:").len(), 1);
        assert!(scan_mention_candidates("alice:foo").is_empty());
    }

    #[test]
    fn scan_at_mention_anywhere() {
        let v = scan_mention_candidates("hey @bob look");
        assert_eq!(v, vec![cand(4, 8, "bob", true)]);
    }

    #[test]
    fn scan_lowercases_keys() {
        let v = scan_mention_candidates("Alice: hi");
        assert_eq!(v, vec![cand(0, 5, "alice", false)]);
    }

    #[test]
    fn scan_skips_mxid() {
        assert!(scan_mention_candidates("see @alice:matrix.org").is_empty());
    }

    #[test]
    fn scan_skips_mid_word_at() {
        assert!(scan_mention_candidates("foo@bar.com").is_empty());
    }

    #[test]
    fn scan_combines_leading_and_at() {
        let v = scan_mention_candidates("alice: ping @bob");
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].key, "alice");
        assert_eq!(v[1].key, "bob");
    }

    #[test]
    fn html_escapes_and_splices() {
        let mxid = matrix_sdk::ruma::OwnedUserId::try_from("@alice:matrix.org").unwrap();
        let spans = vec![MentionSpan {
            start: 0,
            end: 5,
            mxid,
            pill_text: "Alice".into(),
        }];
        let html = build_mention_html("alice: <hi>", &spans);
        assert_eq!(
            html,
            r#"<a href="https://matrix.to/#/@alice:matrix.org">Alice</a>: &lt;hi&gt;"#
        );
    }

    #[test]
    fn html_pill_keeps_at_prefix() {
        let mxid = matrix_sdk::ruma::OwnedUserId::try_from("@bob:matrix.org").unwrap();
        let spans = vec![MentionSpan {
            start: 4,
            end: 8,
            mxid,
            pill_text: "@Bob".into(),
        }];
        let html = build_mention_html("hey @bob", &spans);
        assert_eq!(
            html,
            r#"hey <a href="https://matrix.to/#/@bob:matrix.org">@Bob</a>"#
        );
    }

    #[test]
    fn sanitize_nick_edges() {
        assert_eq!(sanitize_nick("Alice"), "Alice");
        assert_eq!(sanitize_nick("Paweł"), "Pawel");
        assert_eq!(sanitize_nick("hi there!"), "hi_there");
        assert_eq!(sanitize_nick("123foo"), "_123foo");
        assert_eq!(sanitize_nick("!!!"), "_");
        assert_eq!(sanitize_nick(""), "_");
        assert_eq!(sanitize_nick("a".repeat(40).as_str()).len(), 16);
    }

    fn uid(s: &str) -> matrix_sdk::ruma::OwnedUserId {
        matrix_sdk::ruma::OwnedUserId::try_from(s).unwrap()
    }

    fn timeline_event(event_id: &str, sender: &str) -> matrix_sdk::deserialized_responses::TimelineEvent {
        // Deserialize from a string: `Raw` wraps a serde_json RawValue, which
        // cannot be built via `from_value`.
        let json = format!(
            r#"{{"type":"m.room.message","event_id":"{event_id}","sender":"{sender}","origin_server_ts":0,"content":{{"msgtype":"m.text","body":"hi"}}}}"#
        );
        let raw: matrix_sdk::ruma::serde::Raw<AnySyncTimelineEvent> =
            serde_json::from_str(&json).unwrap();
        matrix_sdk::deserialized_responses::TimelineEvent::from_plaintext(raw)
    }

    #[test]
    fn event_is_own_matches_sender() {
        let me = uid("@me:server.tld");
        let mine = timeline_event("$1:server.tld", "@me:server.tld");
        let theirs = timeline_event("$2:server.tld", "@peer:server.tld");
        assert!(event_is_own(&mine, Some(&me)));
        assert!(!event_is_own(&theirs, Some(&me)));
        // No logged-in user: nothing is "own".
        assert!(!event_is_own(&mine, None));
    }

    #[test]
    fn unread_boundary_stops_at_read_receipt() {
        // newest -> oldest; the read receipt points at the 3rd event.
        let events = [
            timeline_event("$new2:server.tld", "@peer:server.tld"),
            timeline_event("$new1:server.tld", "@peer:server.tld"),
            timeline_event("$read:server.tld", "@peer:server.tld"),
            timeline_event("$old:server.tld", "@peer:server.tld"),
        ];
        let markers = vec![evt("$read:server.tld")];
        assert_eq!(unread_boundary(&events, &markers, None), Some(2));
    }

    #[test]
    fn unread_boundary_prefers_newer_own_message() {
        let me = uid("@me:server.tld");
        // Our own reply is newer than the read receipt, so it is the boundary.
        let events = [
            timeline_event("$incoming:server.tld", "@peer:server.tld"),
            timeline_event("$myreply:server.tld", "@me:server.tld"),
            timeline_event("$read:server.tld", "@peer:server.tld"),
        ];
        let markers = vec![evt("$read:server.tld")];
        assert_eq!(unread_boundary(&events, &markers, Some(&me)), Some(1));
    }

    #[test]
    fn unread_boundary_none_when_all_unread() {
        let me = uid("@me:server.tld");
        let events = [
            timeline_event("$a:server.tld", "@peer:server.tld"),
            timeline_event("$b:server.tld", "@peer:server.tld"),
        ];
        assert_eq!(unread_boundary(&events, &[], Some(&me)), None);
    }
}
