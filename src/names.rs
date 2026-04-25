use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{anyhow, Context, Result};
use matrix_sdk::ruma::RoomId;
use serde::{Deserialize, Serialize};

const SLUG_MAX: usize = 24;
const SUFFIX_LEN: usize = 6;
const DEFAULT_SLUG: &str = "room";

pub fn slugify(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-');
    let capped: String = trimmed.chars().take(SLUG_MAX).collect();
    let capped = capped.trim_end_matches('-').to_string();
    if capped.is_empty() {
        DEFAULT_SLUG.to_string()
    } else {
        capped
    }
}

pub fn room_id_suffix(room: &RoomId) -> String {
    room.as_str()
        .strip_prefix('!')
        .and_then(|s| s.split_once(':').map(|(l, _)| l))
        .unwrap_or("")
        .chars()
        .take(SUFFIX_LEN)
        .collect()
}

pub fn preferred_channel_name(room: &RoomId, display_name: Option<&str>) -> String {
    let slug = display_name.map(slugify).unwrap_or_else(|| DEFAULT_SLUG.to_string());
    let suffix = room_id_suffix(room);
    if suffix.is_empty() {
        format!("#{slug}")
    } else {
        format!("#{slug}-{suffix}")
    }
}

pub fn default_store_path() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_DATA_HOME") {
        return Ok(PathBuf::from(dir).join("matrirc").join("names.json"));
    }
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME not set"))?;
    Ok(PathBuf::from(home)
        .join(".local")
        .join("share")
        .join("matrirc")
        .join("names.json"))
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct StoreData {
    rooms: HashMap<String, String>,
}

pub struct NameStore {
    path: Option<PathBuf>,
    data: Mutex<StoreData>,
}

impl NameStore {
    pub fn load(path: PathBuf) -> Result<Self> {
        let data = match std::fs::read_to_string(&path) {
            Ok(s) => serde_json::from_str(&s)
                .with_context(|| format!("parse {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => StoreData::default(),
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        };
        Ok(Self { path: Some(path), data: Mutex::new(data) })
    }

    #[cfg(test)]
    pub fn in_memory() -> Self {
        Self { path: None, data: Mutex::new(StoreData::default()) }
    }

    #[cfg(test)]
    pub fn lookup(&self, room: &RoomId) -> Option<String> {
        self.data.lock().unwrap().rooms.get(room.as_str()).cloned()
    }

    /// Returns the stable channel name for `room`, assigning `preferred` (or a
    /// `_N`-suffixed variant on collision) on first seen. Persisted to disk.
    pub fn assign_or_get(&self, room: &RoomId, preferred: &str) -> Result<String> {
        let mut data = self.data.lock().unwrap();
        if let Some(existing) = data.rooms.get(room.as_str()) {
            return Ok(existing.clone());
        }
        let taken: std::collections::HashSet<&String> = data.rooms.values().collect();
        let mut candidate = preferred.to_string();
        let mut n = 2;
        while taken.contains(&candidate) {
            candidate = format!("{preferred}_{n}");
            n += 1;
        }
        data.rooms.insert(room.as_str().to_string(), candidate.clone());
        let snapshot = data.clone();
        drop(data);
        self.save(&snapshot)?;
        Ok(candidate)
    }

    fn save(&self, data: &StoreData) -> Result<()> {
        let Some(path) = self.path.as_ref() else { return Ok(()); };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        let tmp = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(data).context("serialize name store")?;
        std::fs::write(&tmp, &bytes).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Hello World"), "hello-world");
        assert_eq!(slugify("  Trim  Me  "), "trim-me");
        assert_eq!(slugify("Already-Slug"), "already-slug");
        assert_eq!(slugify("Mixed!@#Chars"), "mixed-chars");
    }

    #[test]
    fn slugify_empty_fallback() {
        assert_eq!(slugify(""), DEFAULT_SLUG);
        assert_eq!(slugify("!!!"), DEFAULT_SLUG);
        assert_eq!(slugify("żźć"), DEFAULT_SLUG);
    }

    #[test]
    fn slugify_caps_length() {
        let long = "a".repeat(100);
        assert_eq!(slugify(&long).len(), SLUG_MAX);
    }

    #[test]
    fn slugify_trims_trailing_dash_after_cap() {
        let s = format!("{}{}", "a".repeat(23), "---extra");
        let slug = slugify(&s);
        assert!(!slug.ends_with('-'));
        assert!(slug.len() <= SLUG_MAX);
    }

    #[test]
    fn suffix_extraction() {
        let r = RoomId::parse("!xSOrIjerDiOWOxmkjX:example.org").unwrap();
        assert_eq!(room_id_suffix(&r), "xSOrIj");
    }

    #[test]
    fn preferred_name_shape() {
        let r = RoomId::parse("!abc123defghi:server.org").unwrap();
        assert_eq!(preferred_channel_name(&r, Some("My Room")), "#my-room-abc123");
        assert_eq!(preferred_channel_name(&r, None), "#room-abc123");
        assert_eq!(preferred_channel_name(&r, Some("")), "#room-abc123");
    }

    #[test]
    fn store_assigns_and_persists() {
        let s = NameStore::in_memory();
        let r = RoomId::parse("!abc:server").unwrap();
        let a = s.assign_or_get(&r, "#my-room-abc123").unwrap();
        assert_eq!(a, "#my-room-abc123");
        let b = s.assign_or_get(&r, "#different").unwrap();
        assert_eq!(b, "#my-room-abc123", "re-assigns must be stable");
    }

    #[test]
    fn store_dedupes_collisions() {
        let s = NameStore::in_memory();
        let r1 = RoomId::parse("!one:server").unwrap();
        let r2 = RoomId::parse("!two:server").unwrap();
        let a = s.assign_or_get(&r1, "#chat").unwrap();
        let b = s.assign_or_get(&r2, "#chat").unwrap();
        assert_eq!(a, "#chat");
        assert_eq!(b, "#chat_2");
    }

    #[test]
    fn store_round_trips_via_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("names.json");
        let s1 = NameStore::load(path.clone()).unwrap();
        let r = RoomId::parse("!persist:server").unwrap();
        let a = s1.assign_or_get(&r, "#name").unwrap();
        drop(s1);
        let s2 = NameStore::load(path).unwrap();
        assert_eq!(s2.lookup(&r), Some(a));
    }
}
