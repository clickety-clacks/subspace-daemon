use std::collections::{HashSet, VecDeque};
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

use crate::config::RetryConfig;
use crate::retry::jitter;

#[derive(Debug, Default)]
pub struct RuntimeStore {
    path: PathBuf,
    dedupe_window_size: usize,
    discard_before_ts: Option<String>,
    last_seen_ts: Option<String>,
    recent_ids_order: VecDeque<String>,
    recent_ids_set: HashSet<String>,
    pending_ids: HashSet<String>,
    reconnect_storm: ReconnectStormState,
    reconnect_failure_window_started_at: Option<OffsetDateTime>,
}

#[derive(Debug, Deserialize, Serialize)]
struct RuntimeFile {
    last_seen_ts: Option<String>,
    recent_ids: Vec<String>,
    #[serde(default)]
    reconnect_storm: ReconnectStormState,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ReconnectStormState {
    #[serde(default)]
    pub consecutive_failures: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_attempt_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error_kind: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconnectCooldown {
    pub consecutive_failures: u32,
    pub cooldown_ms: u64,
    pub next_attempt_at: String,
    pub last_error_kind: Option<String>,
}

impl RuntimeStore {
    pub fn load(
        path: PathBuf,
        dedupe_window_size: usize,
        discard_before_ts: Option<String>,
    ) -> Result<Self> {
        if !path.exists() {
            return Ok(Self {
                path,
                dedupe_window_size,
                discard_before_ts,
                ..Self::default()
            });
        }
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed reading runtime store {}", path.display()))?;
        let parsed: RuntimeFile = serde_json::from_str(&raw)
            .with_context(|| format!("failed parsing runtime store {}", path.display()))?;
        let mut store = Self {
            path,
            dedupe_window_size,
            discard_before_ts,
            last_seen_ts: parsed.last_seen_ts,
            recent_ids_order: VecDeque::new(),
            recent_ids_set: HashSet::new(),
            pending_ids: HashSet::new(),
            reconnect_failure_window_started_at: (parsed.reconnect_storm.consecutive_failures > 0
                && parsed.reconnect_storm.cooldown_ms.is_none())
            .then(OffsetDateTime::now_utc),
            reconnect_storm: parsed.reconnect_storm,
        };
        for id in parsed.recent_ids {
            store.insert_recent(id);
        }
        Ok(store)
    }

    pub fn should_enqueue(&mut self, message_id: &str, timestamp: &str) -> bool {
        if timestamp_before_floor(timestamp, self.discard_before_ts.as_deref()) {
            return false;
        }
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
                reconnect_storm: self.reconnect_storm.clone(),
            },
        )
    }

    pub fn reconnect_cooldown(&self) -> Option<ReconnectCooldown> {
        Some(ReconnectCooldown {
            consecutive_failures: self.reconnect_storm.consecutive_failures,
            cooldown_ms: self.reconnect_storm.cooldown_ms?,
            next_attempt_at: self.reconnect_storm.next_attempt_at.clone()?,
            last_error_kind: self.reconnect_storm.last_error_kind.clone(),
        })
    }

    pub fn record_reconnect_failure(
        &mut self,
        now: OffsetDateTime,
        retry: &RetryConfig,
        last_error_kind: String,
    ) -> Option<ReconnectCooldown> {
        let window = Duration::milliseconds(retry.storm_guard.failure_window_ms as i64);
        if self.reconnect_storm.cooldown_ms.is_none()
            && self
                .reconnect_failure_window_started_at
                .is_none_or(|started_at| now - started_at > window)
        {
            self.reconnect_failure_window_started_at = Some(now);
            self.reconnect_storm.consecutive_failures = 0;
        }

        self.reconnect_storm.consecutive_failures =
            self.reconnect_storm.consecutive_failures.saturating_add(1);
        self.reconnect_storm.last_error_kind = Some(last_error_kind);

        if self.reconnect_storm.consecutive_failures
            < retry.storm_guard.consecutive_failure_threshold
        {
            return None;
        }

        let base_cooldown_ms = self
            .reconnect_storm
            .cooldown_ms
            .map(|cooldown| cooldown.saturating_mul(2))
            .unwrap_or(retry.storm_guard.cooldown_ms)
            .min(retry.storm_guard.max_cooldown_ms);
        let effective_cooldown_ms =
            jitter(base_cooldown_ms, retry.jitter_ratio).min(retry.storm_guard.max_cooldown_ms);
        let next_attempt_at =
            now + Duration::milliseconds(effective_cooldown_ms.min(i64::MAX as u64) as i64);
        self.reconnect_storm.cooldown_ms = Some(effective_cooldown_ms);
        self.reconnect_storm.next_attempt_at =
            Some(format_rfc3339(next_attempt_at).unwrap_or_else(|| next_attempt_at.to_string()));
        self.reconnect_cooldown()
    }

    pub fn clear_reconnect_storm(&mut self) {
        self.reconnect_storm = ReconnectStormState::default();
        self.reconnect_failure_window_started_at = None;
    }

    fn insert_recent(&mut self, id: String) {
        if self.recent_ids_set.insert(id.clone()) {
            self.recent_ids_order.push_back(id);
        }
        while self.recent_ids_order.len() > self.dedupe_window_size {
            if let Some(oldest) = self.recent_ids_order.pop_front() {
                self.recent_ids_set.remove(&oldest);
            }
        }
    }
}

fn format_rfc3339(timestamp: OffsetDateTime) -> Option<String> {
    timestamp
        .format(&time::format_description::well_known::Rfc3339)
        .ok()
}

fn timestamp_before_floor(timestamp: &str, floor: Option<&str>) -> bool {
    let Some(floor) = floor else {
        return false;
    };
    match (
        OffsetDateTime::parse(timestamp, &time::format_description::well_known::Rfc3339),
        OffsetDateTime::parse(floor, &time::format_description::well_known::Rfc3339),
    ) {
        (Ok(timestamp), Ok(floor)) => timestamp < floor,
        _ => timestamp < floor,
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
        let mut store = RuntimeStore::load(dir.path().join("runtime.json"), 500, None).unwrap();
        assert!(store.should_enqueue("m1", "2026-03-18T12:00:00Z"));
        assert!(!store.should_enqueue("m1", "2026-03-18T12:00:00Z"));
        store.mark_processed("m1", "2026-03-18T12:00:00Z");
        assert!(!store.should_enqueue("m1", "2026-03-18T12:00:00Z"));
    }

    #[test]
    fn drops_messages_older_than_discard_floor() {
        let dir = tempdir().unwrap();
        let mut store = RuntimeStore::load(
            dir.path().join("runtime.json"),
            500,
            Some("2026-03-18T12:00:00Z".to_string()),
        )
        .unwrap();
        assert!(!store.should_enqueue("m1", "2026-03-18T11:59:59Z"));
        assert!(store.should_enqueue("m2", "2026-03-18T12:00:00Z"));
    }

    #[test]
    fn compares_discard_floor_by_timestamp_not_string_order() {
        let dir = tempdir().unwrap();
        let mut store = RuntimeStore::load(
            dir.path().join("runtime.json"),
            500,
            Some("2026-03-18T12:00:00Z".to_string()),
        )
        .unwrap();
        assert!(store.should_enqueue("m1", "2026-03-18T05:00:00-07:00"));
    }

    #[test]
    fn dedupe_window_size_bounds_recent_ids() {
        let dir = tempdir().unwrap();
        let mut store = RuntimeStore::load(dir.path().join("runtime.json"), 1, None).unwrap();
        assert!(store.should_enqueue("m1", "2026-03-18T12:00:00Z"));
        store.mark_processed("m1", "2026-03-18T12:00:00Z");
        assert!(store.should_enqueue("m2", "2026-03-18T12:00:01Z"));
        store.mark_processed("m2", "2026-03-18T12:00:01Z");
        assert!(store.should_enqueue("m1", "2026-03-18T12:00:02Z"));
    }

    #[test]
    fn records_reconnect_cooldown_after_threshold() {
        let dir = tempdir().unwrap();
        let mut store = RuntimeStore::load(dir.path().join("runtime.json"), 500, None).unwrap();
        let retry = RetryConfig {
            base_ms: 1,
            max_ms: 1,
            jitter_ratio: 0.0,
            storm_guard: crate::config::StormGuardConfig {
                failure_window_ms: 60_000,
                consecutive_failure_threshold: 2,
                cooldown_ms: 5_000,
                max_cooldown_ms: 60_000,
            },
        };
        let now = OffsetDateTime::parse(
            "2026-04-17T12:00:00Z",
            &time::format_description::well_known::Rfc3339,
        )
        .unwrap();

        assert!(
            store
                .record_reconnect_failure(now, &retry, "connection_refused".to_string())
                .is_none()
        );
        let cooldown = store
            .record_reconnect_failure(now, &retry, "connection_refused".to_string())
            .unwrap();

        assert_eq!(cooldown.consecutive_failures, 2);
        assert_eq!(cooldown.cooldown_ms, 5_000);
        assert_eq!(cooldown.next_attempt_at, "2026-04-17T12:00:05Z");
        assert_eq!(
            cooldown.last_error_kind.as_deref(),
            Some("connection_refused")
        );
    }

    #[test]
    fn persists_reconnect_storm_metadata() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("runtime.json");
        let retry = RetryConfig {
            base_ms: 1,
            max_ms: 1,
            jitter_ratio: 0.0,
            storm_guard: crate::config::StormGuardConfig {
                failure_window_ms: 60_000,
                consecutive_failure_threshold: 1,
                cooldown_ms: 5_000,
                max_cooldown_ms: 60_000,
            },
        };
        let now = OffsetDateTime::parse(
            "2026-04-17T12:00:00Z",
            &time::format_description::well_known::Rfc3339,
        )
        .unwrap();
        let mut store = RuntimeStore::load(path.clone(), 500, None).unwrap();
        store.record_reconnect_failure(now, &retry, "join_failed".to_string());
        store.flush().unwrap();

        let reloaded = RuntimeStore::load(path, 500, None).unwrap();
        let cooldown = reloaded.reconnect_cooldown().unwrap();
        assert_eq!(cooldown.consecutive_failures, 1);
        assert_eq!(cooldown.cooldown_ms, 5_000);
        assert_eq!(cooldown.next_attempt_at, "2026-04-17T12:00:05Z");
        assert_eq!(cooldown.last_error_kind.as_deref(), Some("join_failed"));
    }

    #[test]
    fn reloaded_pre_cooldown_failures_continue_counting() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("runtime.json");
        let retry = RetryConfig {
            base_ms: 1,
            max_ms: 1,
            jitter_ratio: 0.0,
            storm_guard: crate::config::StormGuardConfig {
                failure_window_ms: 60_000,
                consecutive_failure_threshold: 3,
                cooldown_ms: 5_000,
                max_cooldown_ms: 60_000,
            },
        };
        let now = OffsetDateTime::now_utc();
        let mut store = RuntimeStore::load(path.clone(), 500, None).unwrap();
        assert!(
            store
                .record_reconnect_failure(now, &retry, "timeout".to_string())
                .is_none()
        );
        store.flush().unwrap();

        let mut reloaded = RuntimeStore::load(path, 500, None).unwrap();
        assert!(
            reloaded
                .record_reconnect_failure(now, &retry, "timeout".to_string())
                .is_none()
        );
        let cooldown = reloaded
            .record_reconnect_failure(now, &retry, "timeout".to_string())
            .unwrap();

        assert_eq!(cooldown.consecutive_failures, 3);
        assert_eq!(cooldown.cooldown_ms, 5_000);
    }
}
