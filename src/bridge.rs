use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, RwLock};

use matrix_sdk::ruma::{EventId, OwnedEventId, OwnedRoomId, OwnedUserId, RoomId};
use tokio::sync::{broadcast, mpsc, oneshot};

const RECENT_SENT_CAP: usize = 256;

#[derive(Debug, Default)]
pub struct Mapping {
    pub room_to_chan: HashMap<OwnedRoomId, String>,
    pub chan_to_room: HashMap<String, OwnedRoomId>,
    pub chan_to_topic: HashMap<String, String>,
    pub dm_room_to_nick: HashMap<OwnedRoomId, String>,
    pub nick_to_dm_room: HashMap<String, OwnedRoomId>,
}

impl Mapping {
    pub fn insert(&mut self, room: OwnedRoomId, chan: impl Into<String>, topic: impl Into<String>) {
        let chan = chan.into();
        self.chan_to_topic.insert(chan.clone(), topic.into());
        self.chan_to_room.insert(chan.clone(), room.clone());
        self.room_to_chan.insert(room, chan);
    }

    pub fn set_topic(&mut self, chan: &str, topic: String) {
        self.chan_to_topic.insert(chan.to_string(), topic);
    }

    pub fn insert_dm(&mut self, room: OwnedRoomId, nick: impl Into<String>, aliases: &[&str]) {
        let nick = nick.into();
        self.nick_to_dm_room.insert(nick.to_ascii_lowercase(), room.clone());
        for a in aliases {
            if !a.is_empty() {
                self.nick_to_dm_room.insert(a.to_ascii_lowercase(), room.clone());
            }
        }
        self.dm_room_to_nick.insert(room, nick);
    }
}

/// If MATRIRC_ROOM is set, returns the one room to bridge (dev-only). Otherwise
/// None — caller auto-discovers all joined rooms after sync.
pub fn env_override() -> Option<OwnedRoomId> {
    let s = std::env::var("MATRIRC_ROOM").ok().filter(|s| !s.is_empty())?;
    match RoomId::parse(&s) {
        Ok(room) => Some(room),
        Err(e) => {
            tracing::warn!("MATRIRC_ROOM not a valid room id ({s}): {e}");
            None
        }
    }
}

#[derive(Clone, Debug)]
pub enum FromMatrix {
    Message {
        room: OwnedRoomId,
        sender_nick: String,
        body: String,
        /// True when the sender is the logged-in user (message originated on
        /// another device). Lets IRC conn route as `self→peer` for DMs.
        is_own: bool,
    },
    RoomAdded {
        room: OwnedRoomId,
        chan: String,
        topic: String,
    },
    DmAdded {
        nick: String,
    },
    TopicChanged {
        chan: String,
        topic: String,
    },
    MemberJoined {
        chan: String,
        nick: String,
    },
    MemberLeft {
        chan: String,
        nick: String,
        reason: Option<String>,
    },
}

#[derive(Debug)]
pub enum ToMatrix {
    Send {
        room: OwnedRoomId,
        body: String,
        emote: bool,
        notice: bool,
    },
    SendToMxid {
        mxid: OwnedUserId,
        body: String,
        emote: bool,
        notice: bool,
    },
    Backfill {
        room: OwnedRoomId,
        limit: u32,
        reply: oneshot::Sender<Vec<BackfillMessage>>,
    },
    Members {
        room: OwnedRoomId,
        reply: oneshot::Sender<Vec<String>>,
    },
    SearchRooms {
        query: String,
        server: Option<String>,
        reply: oneshot::Sender<Vec<RoomListing>>,
    },
    JoinByAlias {
        alias: String,
        reply: oneshot::Sender<Result<String, String>>,
    },
    Whois {
        nick: String,
        reply: oneshot::Sender<Option<WhoisInfo>>,
    },
    SetDisplayName {
        name: String,
    },
    SetTopic {
        room: OwnedRoomId,
        topic: String,
    },
}

#[derive(Debug, Clone)]
pub struct BackfillMessage {
    pub sender_nick: String,
    pub body: String,
    pub origin_ms: i64,
    /// True when sender is the logged-in user. DM backfill drops these
    /// because IRC can't emit "self→peer" without echo-message.
    pub is_own: bool,
}

#[derive(Debug, Clone)]
pub struct RoomListing {
    pub alias: Option<String>,
    pub room_id: String,
    pub name: String,
    pub members: u64,
}

#[derive(Debug, Clone)]
pub struct WhoisInfo {
    pub nick: String,
    pub mxid: String,
    pub display_name: Option<String>,
    pub rooms: Vec<String>,
}

#[derive(Clone)]
pub struct Bridge {
    pub mapping: Arc<RwLock<Mapping>>,
    pub from_matrix: broadcast::Sender<FromMatrix>,
    pub to_matrix: mpsc::Sender<ToMatrix>,
    recent_sent: Arc<Mutex<VecDeque<OwnedEventId>>>,
}

impl Bridge {
    pub fn new(mapping: Mapping) -> (Self, mpsc::Receiver<ToMatrix>) {
        let (from_tx, _) = broadcast::channel(256);
        let (to_tx, to_rx) = mpsc::channel(64);
        (
            Self {
                mapping: Arc::new(RwLock::new(mapping)),
                from_matrix: from_tx,
                to_matrix: to_tx,
                recent_sent: Arc::new(Mutex::new(VecDeque::with_capacity(RECENT_SENT_CAP))),
            },
            to_rx,
        )
    }

    /// Adds the mapping + broadcasts RoomAdded so live IRC connections auto-join.
    pub fn add_mapping(&self, room: OwnedRoomId, chan: String, topic: String) {
        let mut m = self.mapping.write().unwrap();
        m.insert(room.clone(), chan.clone(), topic.clone());
        drop(m);
        let _ = self.from_matrix.send(FromMatrix::RoomAdded { room, chan, topic });
    }

    pub fn update_topic(&self, room: &RoomId, topic: String) {
        let mut m = self.mapping.write().unwrap();
        let Some(chan) = m.room_to_chan.get(room).cloned() else { return; };
        m.set_topic(&chan, topic.clone());
        drop(m);
        let _ = self.from_matrix.send(FromMatrix::TopicChanged { chan, topic });
    }

    pub fn topic_for(&self, chan: &str) -> Option<String> {
        self.mapping.read().unwrap().chan_to_topic.get(chan).cloned()
    }

    /// Register a DM. `aliases` are extra lookup keys — typically the peer's
    /// MXID localpart and full MXID so `/msg <any-form>` hits the same room.
    pub fn add_dm(&self, room: OwnedRoomId, nick: String, aliases: &[&str]) {
        let mut m = self.mapping.write().unwrap();
        m.insert_dm(room, nick.clone(), aliases);
        drop(m);
        let _ = self.from_matrix.send(FromMatrix::DmAdded { nick });
    }

    pub fn chan_for(&self, room: &RoomId) -> Option<String> {
        self.mapping.read().unwrap().room_to_chan.get(room).cloned()
    }

    pub fn dm_nick_for(&self, room: &RoomId) -> Option<String> {
        self.mapping.read().unwrap().dm_room_to_nick.get(room).cloned()
    }

    pub fn dm_room_for(&self, target: &str) -> Option<OwnedRoomId> {
        let m = self.mapping.read().unwrap();
        m.nick_to_dm_room.get(&target.to_ascii_lowercase()).cloned()
            // irssi /query strips the leading '@' — accept bare `name:server` too.
            .or_else(|| m.nick_to_dm_room.get(&format!("@{target}").to_ascii_lowercase()).cloned())
    }

    pub fn room_for(&self, chan: &str) -> Option<OwnedRoomId> {
        self.mapping.read().unwrap().chan_to_room.get(chan).cloned()
    }

    pub fn has_room(&self, room: &RoomId) -> bool {
        let m = self.mapping.read().unwrap();
        m.room_to_chan.contains_key(room) || m.dm_room_to_nick.contains_key(room)
    }

    /// Channels only, no DMs — auto-join iterates this.
    pub fn snapshot(&self) -> Vec<(String, OwnedRoomId)> {
        self.mapping
            .read()
            .unwrap()
            .chan_to_room
            .iter()
            .map(|(c, r)| (c.clone(), r.clone()))
            .collect()
    }

    pub fn dm_count(&self) -> usize {
        self.mapping.read().unwrap().dm_room_to_nick.len()
    }

    pub fn dm_nicks(&self) -> Vec<String> {
        let m = self.mapping.read().unwrap();
        let mut v: Vec<String> = m.dm_room_to_nick.values().cloned().collect();
        v.sort();
        v
    }

    /// Snapshot of `(room_id, canonical_nick)` — lets the IRC side drive DM
    /// backfill on client connect.
    pub fn dms(&self) -> Vec<(OwnedRoomId, String)> {
        self.mapping
            .read()
            .unwrap()
            .dm_room_to_nick
            .iter()
            .map(|(r, n)| (r.clone(), n.clone()))
            .collect()
    }

    pub fn note_sent_by_us(&self, id: OwnedEventId) {
        let mut q = self.recent_sent.lock().unwrap();
        q.push_back(id);
        while q.len() > RECENT_SENT_CAP {
            q.pop_front();
        }
    }

    /// Returns true and removes the entry if `id` was recently sent by us.
    pub fn take_if_sent_by_us(&self, id: &EventId) -> bool {
        let mut q = self.recent_sent.lock().unwrap();
        if let Some(pos) = q.iter().position(|e| e == id) {
            q.remove(pos);
            true
        } else {
            false
        }
    }
}

pub fn mxid_localpart(mxid: &str) -> &str {
    let s = mxid.strip_prefix('@').unwrap_or(mxid);
    s.split_once(':').map(|(l, _)| l).unwrap_or(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_localpart() {
        assert_eq!(mxid_localpart("@alice:matrix.org"), "alice");
        assert_eq!(mxid_localpart("@bob:foo.bar.baz"), "bob");
        assert_eq!(mxid_localpart("noatsign:server"), "noatsign");
        assert_eq!(mxid_localpart("@noserver"), "noserver");
    }

    #[test]
    fn mapping_round_trip() {
        let mut m = Mapping::default();
        let r = RoomId::parse("!abc:server.org").unwrap();
        m.insert(r.clone(), "#matrix", "topic here");
        assert_eq!(m.room_to_chan.get(&r), Some(&"#matrix".to_string()));
        assert_eq!(m.chan_to_room.get("#matrix"), Some(&r));
        assert_eq!(m.chan_to_topic.get("#matrix").map(String::as_str), Some("topic here"));
    }

    #[test]
    fn bridge_add_mapping_broadcasts() {
        let (b, _rx) = Bridge::new(Mapping::default());
        let mut sub = b.from_matrix.subscribe();
        let r = RoomId::parse("!abc:server.org").unwrap();
        b.add_mapping(r.clone(), "#test-abc".into(), "topic".into());
        let ev = sub.try_recv().unwrap();
        match ev {
            FromMatrix::RoomAdded { room, chan, topic } => {
                assert_eq!(room, r);
                assert_eq!(chan, "#test-abc");
                assert_eq!(topic, "topic");
            }
            _ => panic!("wrong event"),
        }
        assert_eq!(b.chan_for(&r), Some("#test-abc".into()));
    }

    #[test]
    fn insert_dm_resolves_every_alias() {
        let mut m = Mapping::default();
        let r = RoomId::parse("!xyz:server.org").unwrap();
        m.insert_dm(r.clone(), "Alice", &["@alice:server.org", "alice"]);
        for key in ["alice", "Alice", "ALICE", "@alice:server.org", "@ALICE:server.org"] {
            let hit = m.nick_to_dm_room.get(&key.to_ascii_lowercase());
            assert_eq!(hit, Some(&r), "alias {key:?} should resolve");
        }
    }

    #[test]
    fn dm_room_for_accepts_bare_mxid() {
        let (b, _rx) = Bridge::new(Mapping::default());
        let r = RoomId::parse("!xyz:server.org").unwrap();
        {
            let mut m = b.mapping.write().unwrap();
            m.insert_dm(r.clone(), "alice", &["@alice:server.org"]);
        }
        assert_eq!(b.dm_room_for("@alice:server.org"), Some(r.clone()));
        // irssi strips '@' on /query; bare form must still resolve.
        assert_eq!(b.dm_room_for("alice:server.org"), Some(r));
    }

    #[test]
    fn update_topic_broadcasts_and_is_noop_for_unknown() {
        let (b, _rx) = Bridge::new(Mapping::default());
        let mut sub = b.from_matrix.subscribe();
        let r = RoomId::parse("!abc:server.org").unwrap();
        b.add_mapping(r.clone(), "#c".into(), "old".into());
        // drain RoomAdded
        let _ = sub.try_recv();
        b.update_topic(&r, "new topic".into());
        match sub.try_recv().unwrap() {
            FromMatrix::TopicChanged { chan, topic } => {
                assert_eq!(chan, "#c");
                assert_eq!(topic, "new topic");
            }
            _ => panic!("wrong event"),
        }
        assert_eq!(b.topic_for("#c").as_deref(), Some("new topic"));

        // Unknown room → no event, no state change
        let r2 = RoomId::parse("!never:server.org").unwrap();
        b.update_topic(&r2, "ignored".into());
        assert!(sub.try_recv().is_err());
    }

    #[test]
    fn take_if_sent_by_us_removes_matched_event() {
        let (b, _rx) = Bridge::new(Mapping::default());
        let id = matrix_sdk::ruma::OwnedEventId::try_from("$one:h").unwrap();
        b.note_sent_by_us(id.clone());
        assert!(b.take_if_sent_by_us(&id));
        assert!(!b.take_if_sent_by_us(&id), "second call should be false (consumed)");
    }

    #[test]
    fn recent_sent_is_capped() {
        let (b, _rx) = Bridge::new(Mapping::default());
        // Overflow by more than the cap; oldest must be evicted.
        for i in 0..(RECENT_SENT_CAP + 10) {
            let id = matrix_sdk::ruma::OwnedEventId::try_from(format!("$e{i}:h").as_str()).unwrap();
            b.note_sent_by_us(id);
        }
        let first = matrix_sdk::ruma::OwnedEventId::try_from("$e0:h").unwrap();
        assert!(!b.take_if_sent_by_us(&first), "oldest should have been evicted");
        let recent = matrix_sdk::ruma::OwnedEventId::try_from(format!("$e{}:h", RECENT_SENT_CAP + 9).as_str()).unwrap();
        assert!(b.take_if_sent_by_us(&recent));
    }
}
