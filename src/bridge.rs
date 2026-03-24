use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use matrix_sdk::ruma::{EventId, OwnedEventId, OwnedRoomId, RoomId};
use tokio::sync::{broadcast, mpsc, oneshot};

const RECENT_SENT_CAP: usize = 256;

#[derive(Debug, Default)]
pub struct Mapping {
    pub room_to_chan: HashMap<OwnedRoomId, String>,
    pub chan_to_room: HashMap<String, OwnedRoomId>,
}

impl Mapping {
    pub fn insert(&mut self, room: OwnedRoomId, chan: impl Into<String>) {
        let chan = chan.into();
        self.chan_to_room.insert(chan.clone(), room.clone());
        self.room_to_chan.insert(room, chan);
    }

    pub fn from_env() -> Self {
        let mut m = Self::default();
        if let Some(s) = std::env::var("MATRIRC_ROOM").ok().filter(|s| !s.is_empty()) {
            match RoomId::parse(&s) {
                Ok(room) => m.insert(room, "#matrix"),
                Err(e) => tracing::warn!("MATRIRC_ROOM not a valid room id ({s}): {e}"),
            }
        }
        m
    }
}

#[derive(Clone, Debug)]
pub enum FromMatrix {
    Message {
        room: OwnedRoomId,
        sender_nick: String,
        body: String,
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
}

#[derive(Debug, Clone)]
pub struct BackfillMessage {
    pub sender_nick: String,
    pub body: String,
}

#[derive(Clone)]
pub struct Bridge {
    pub mapping: Arc<Mapping>,
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
                mapping: Arc::new(mapping),
                from_matrix: from_tx,
                to_matrix: to_tx,
                recent_sent: Arc::new(Mutex::new(VecDeque::with_capacity(RECENT_SENT_CAP))),
            },
            to_rx,
        )
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
}
