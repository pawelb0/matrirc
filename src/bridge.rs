use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, RwLock};

use matrix_sdk::ruma::{EventId, OwnedEventId, OwnedRoomId, RoomId};
use tokio::sync::{broadcast, mpsc, oneshot};

const RECENT_SENT_CAP: usize = 256;

#[derive(Debug, Default)]
pub struct Mapping {
    pub room_to_chan: HashMap<OwnedRoomId, String>,
    pub chan_to_room: HashMap<String, OwnedRoomId>,
    pub dm_room_to_nick: HashMap<OwnedRoomId, String>,
    pub nick_to_dm_room: HashMap<String, OwnedRoomId>,
}

impl Mapping {
    pub fn insert(&mut self, room: OwnedRoomId, chan: impl Into<String>) {
        let chan = chan.into();
        self.chan_to_room.insert(chan.clone(), room.clone());
        self.room_to_chan.insert(room, chan);
    }

    pub fn insert_dm(&mut self, room: OwnedRoomId, nick: impl Into<String>) {
        let nick = nick.into();
        let key = nick.to_ascii_lowercase();
        self.nick_to_dm_room.insert(key, room.clone());
        self.dm_room_to_nick.insert(room, nick);
    }
}

/// If MATRIRC_ROOM is set, returns a one-entry override mapping for dev. Else None,
/// meaning the caller should auto-discover rooms after sync.
pub fn env_override() -> Option<(OwnedRoomId, &'static str)> {
    let s = std::env::var("MATRIRC_ROOM").ok().filter(|s| !s.is_empty())?;
    match RoomId::parse(&s) {
        Ok(room) => Some((room, "#matrix")),
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
    },
    RoomAdded {
        room: OwnedRoomId,
        chan: String,
        topic: String,
    },
    DmAdded {
        nick: String,
    },
}

#[derive(Debug)]
pub enum ToMatrix {
    Send {
        room: OwnedRoomId,
        body: String,
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
}

#[derive(Debug, Clone)]
pub struct BackfillMessage {
    pub sender_nick: String,
    pub body: String,
    pub origin_ms: i64,
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
        m.insert(room.clone(), chan.clone());
        drop(m);
        let _ = self.from_matrix.send(FromMatrix::RoomAdded { room, chan, topic });
    }

    pub fn add_dm(&self, room: OwnedRoomId, nick: String) {
        let mut m = self.mapping.write().unwrap();
        m.insert_dm(room, nick.clone());
        drop(m);
        let _ = self.from_matrix.send(FromMatrix::DmAdded { nick });
    }

    pub fn chan_for(&self, room: &RoomId) -> Option<String> {
        self.mapping.read().unwrap().room_to_chan.get(room).cloned()
    }

    pub fn dm_nick_for(&self, room: &RoomId) -> Option<String> {
        self.mapping.read().unwrap().dm_room_to_nick.get(room).cloned()
    }

    pub fn dm_room_for(&self, nick: &str) -> Option<OwnedRoomId> {
        self.mapping
            .read()
            .unwrap()
            .nick_to_dm_room
            .get(&nick.to_ascii_lowercase())
            .cloned()
    }

    pub fn room_for(&self, chan: &str) -> Option<OwnedRoomId> {
        self.mapping.read().unwrap().chan_to_room.get(chan).cloned()
    }

    pub fn has_room(&self, room: &RoomId) -> bool {
        let m = self.mapping.read().unwrap();
        m.room_to_chan.contains_key(room) || m.dm_room_to_nick.contains_key(room)
    }

    /// Snapshot of all (chan, room) for bulk auto-join (channels only, no DMs).
    pub fn snapshot(&self) -> Vec<(String, OwnedRoomId)> {
        self.mapping
            .read()
            .unwrap()
            .chan_to_room
            .iter()
            .map(|(c, r)| (c.clone(), r.clone()))
            .collect()
    }

    /// Snapshot of DM count for summary messages.
    pub fn dm_count(&self) -> usize {
        self.mapping.read().unwrap().dm_room_to_nick.len()
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
        m.insert(r.clone(), "#matrix");
        assert_eq!(m.room_to_chan.get(&r), Some(&"#matrix".to_string()));
        assert_eq!(m.chan_to_room.get("#matrix"), Some(&r));
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
}
