use std::collections::{HashSet, VecDeque};
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Default)]
pub struct RuntimeStore {
    path: PathBuf,
    last_seen_ts: Option<String>,
    recent_ids_order: VecDeque<String>,
    recent_ids_set: HashSet<String>,
    pending_ids: HashSet<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct RuntimeFile {
    last_seen_ts: Option<String>,
    recent_ids: Vec<String>,
}

impl RuntimeStore {
    pub fn load(path: PathBuf) -> Result<Self> {
        if !path.exists() {
            return Ok(Self {
                path,
                ..Self::default()
            });
        }
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed reading runtime store {}", path.display()))?;
        let parsed: RuntimeFile = serde_json::from_str(&raw)
            .with_context(|| format!("failed parsing runtime store {}", path.display()))?;
        let mut store = Self {
            path,
            last_seen_ts: parsed.last_seen_ts,
            recent_ids_order: VecDeque::new(),
            recent_ids_set: HashSet::new(),
            pending_ids: HashSet::new(),
        };
        for id in parsed.recent_ids {
            store.insert_recent(id);
        }
        Ok(store)
    }

    pub fn should_enqueue(&mut self, message_id: &str) -> bool {
        if self.recent_ids_set.contains(message_id) || self.pending_ids.contains(message_id) {
            return false;
        }
        self.pending_ids.insert(message_id.to_string());
        true
    }

    pub fn mark_failed(&mut self, message_id: &str) {
        self.pending_ids.remove(message_id);
    }

    pub fn mark_processed(&mut self, message_id: &str, timestamp: &str) {
        self.pending_ids.remove(message_id);
        self.insert_recent(message_id.to_string());
        match &self.last_seen_ts {
            Some(current) if current.as_str() >= timestamp => {}
            _ => self.last_seen_ts = Some(timestamp.to_string()),
        }
    }

    pub fn flush(&self) -> Result<()> {
        write_json_atomic(
            &self.path,
            &RuntimeFile {
                last_seen_ts: self.last_seen_ts.clone(),
                recent_ids: self.recent_ids_order.iter().cloned().collect(),
            },
        )
    }

    fn insert_recent(&mut self, id: String) {
        if self.recent_ids_set.insert(id.clone()) {
            self.recent_ids_order.push_back(id);
        }
        while self.recent_ids_order.len() > 500 {
            if let Some(oldest) = self.recent_ids_order.pop_front() {
                self.recent_ids_set.remove(&oldest);
            }
        }
    }
}

pub fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    let bytes = serde_json::to_vec_pretty(value)?;
    let mut file = File::create(&tmp)?;
    use std::io::Write;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp, path)?;
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all().ok();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn dedupes_pending_and_recent_ids() {
        let dir = tempdir().unwrap();
        let mut store = RuntimeStore::load(dir.path().join("runtime.json")).unwrap();
        assert!(store.should_enqueue("m1"));
        assert!(!store.should_enqueue("m1"));
        store.mark_processed("m1", "2026-03-18T12:00:00Z");
        assert!(!store.should_enqueue("m1"));
    }
}
