//! Local HTTP proxy serving Matrix attachments by event id.
//! Decrypts E2EE blobs and forwards through authenticated media so IRC
//! scripts can curl `http://127.0.0.1:6680/attach/<event_id>` directly.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use matrix_sdk::media::{MediaFormat, MediaRequestParameters};
use matrix_sdk::ruma::events::room::MediaSource;
use matrix_sdk::ruma::{EventId, OwnedEventId};
use matrix_sdk::Client;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, info, warn};

const MAX_INDEX: usize = 1024;
const MAX_REQ_BYTES: usize = 8192;

/// Bounded `event_id → MediaSource` map, FIFO-evicted.
#[derive(Default)]
pub struct AttachIndex {
    state: Mutex<AttachState>,
}

#[derive(Default)]
struct AttachState {
    map: HashMap<OwnedEventId, MediaSource>,
    fifo: VecDeque<OwnedEventId>,
}

impl AttachIndex {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn insert(&self, event_id: OwnedEventId, source: MediaSource) {
        let mut s = self.state.lock().unwrap();
        if !s.map.contains_key(&event_id) {
            s.fifo.push_back(event_id.clone());
            while s.fifo.len() > MAX_INDEX {
                if let Some(old) = s.fifo.pop_front() {
                    s.map.remove(&old);
                }
            }
        }
        s.map.insert(event_id, source);
    }

    pub fn get(&self, id: &EventId) -> Option<MediaSource> {
        self.state.lock().unwrap().map.get(id).cloned()
    }
}

pub async fn run_proxy(addr: SocketAddr, client: Client, index: Arc<AttachIndex>) -> Result<()> {
    let listener = TcpListener::bind(addr).await?;
    info!("attach proxy listening on http://{addr}");
    loop {
        let (sock, _) = listener.accept().await?;
        let client = client.clone();
        let index = index.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(sock, client, index).await {
                debug!("proxy: {e:#}");
            }
        });
    }
}

async fn handle(mut sock: TcpStream, client: Client, index: Arc<AttachIndex>) -> Result<()> {
    let mut buf = vec![0u8; MAX_REQ_BYTES];
    let mut total = 0;
    loop {
        if total >= buf.len() {
            return write_status(&mut sock, 431, "Request Header Too Large").await;
        }
        let n = sock.read(&mut buf[total..]).await?;
        if n == 0 {
            return Err(anyhow!("eof before headers"));
        }
        total += n;
        if buf[..total].windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let head = std::str::from_utf8(&buf[..total]).unwrap_or("");
    let request_line = head.lines().next().unwrap_or("");
    let path = request_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| anyhow!("malformed request"))?;

    let id_str = match path.strip_prefix("/attach/") {
        Some(s) => percent_decode(s),
        None => return write_status(&mut sock, 404, "Not Found").await,
    };
    let event_id = match EventId::parse(&id_str) {
        Ok(id) => id,
        Err(_) => return write_status(&mut sock, 400, "Bad Event Id").await,
    };
    let Some(source) = index.get(&event_id) else {
        return write_status(&mut sock, 404, "Unknown Event").await;
    };

    let req = MediaRequestParameters { source, format: MediaFormat::File };
    let bytes = match client.media().get_media_content(&req, true).await {
        Ok(b) => b,
        Err(e) => {
            warn!("proxy fetch {event_id}: {e:#}");
            return write_status(&mut sock, 502, "Upstream Error").await;
        }
    };

    let header = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: application/octet-stream\r\n\
         Content-Length: {}\r\n\
         Cache-Control: private, max-age=86400\r\n\
         Connection: close\r\n\r\n",
        bytes.len()
    );
    sock.write_all(header.as_bytes()).await?;
    sock.write_all(&bytes).await?;
    sock.shutdown().await.ok();
    Ok(())
}

async fn write_status(sock: &mut TcpStream, code: u16, reason: &str) -> Result<()> {
    let resp = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    sock.write_all(resp.as_bytes()).await?;
    sock.shutdown().await.ok();
    Ok(())
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decode_basics() {
        assert_eq!(percent_decode("foo"), "foo");
        assert_eq!(percent_decode("%24abc%3Aserver"), "$abc:server");
        assert_eq!(percent_decode("%2"), "%2");
    }

    #[test]
    fn attach_index_evicts_oldest() {
        let idx = AttachIndex::new();
        for i in 0..(MAX_INDEX + 5) {
            let id_str = format!("$evt{i}:server.tld");
            let id: OwnedEventId = EventId::parse(&id_str).unwrap();
            idx.insert(id, MediaSource::Plain(format!("mxc://server/{i}").into()));
        }
        let oldest: OwnedEventId = EventId::parse("$evt0:server.tld").unwrap();
        let newest: OwnedEventId =
            EventId::parse(format!("$evt{}:server.tld", MAX_INDEX + 4)).unwrap();
        assert!(idx.get(&oldest).is_none());
        assert!(idx.get(&newest).is_some());
    }
}
