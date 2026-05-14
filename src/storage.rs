use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::json;

use crate::attention::{AttentionResult, ReceptorMatch};
use crate::config::StorageConfig;
use crate::supervisor::WakeEnvelope;

#[derive(Debug, Clone)]
pub struct DeliveryStore {
    database_path: PathBuf,
    artifact_root: PathBuf,
    auto_migrate: bool,
}

#[derive(Debug, Clone)]
pub struct AcceptedMessageRecord {
    pub accepted_message_id: i64,
    pub inserted: bool,
}

impl DeliveryStore {
    pub fn new(config: &StorageConfig) -> Self {
        Self {
            database_path: config.database_path.clone(),
            artifact_root: config.artifact_root.clone(),
            auto_migrate: config.auto_migrate,
        }
    }

    pub fn ensure_ready(&self) -> Result<()> {
        self.with_connection(|conn| {
            ensure_schema(conn, self.auto_migrate)?;
            Ok(())
        })
    }

    pub fn record_db_sink_delivery(
        &self,
        envelope: &WakeEnvelope,
        attention: &AttentionResult,
    ) -> Result<AcceptedMessageRecord> {
        self.with_connection(|conn| {
            ensure_schema(conn, self.auto_migrate)?;
            let accepted = upsert_accepted_message(conn, envelope, attention)?;
            let destination = self.database_path.to_string_lossy().to_string();
            let config_json = json!({
                "database_path": self.database_path,
                "artifact_root": self.artifact_root,
            })
            .to_string();
            upsert_sink_delivery(
                conn,
                accepted.accepted_message_id,
                "db",
                "db",
                &destination,
                &config_json,
            )?;
            Ok(accepted)
        })
    }

    pub fn record_wake_sink_delivery(
        &self,
        envelope: &WakeEnvelope,
        attention: &AttentionResult,
        sink_key: &str,
        destination: &str,
    ) -> Result<AcceptedMessageRecord> {
        self.with_connection(|conn| {
            ensure_schema(conn, self.auto_migrate)?;
            let accepted = upsert_accepted_message(conn, envelope, attention)?;
            let config_json = json!({
                "session_key": destination,
            })
            .to_string();
            upsert_sink_delivery(
                conn,
                accepted.accepted_message_id,
                sink_key,
                "agent_session_wake",
                destination,
                &config_json,
            )?;
            Ok(accepted)
        })
    }

    pub fn counts(&self) -> Result<(i64, i64)> {
        self.with_connection(|conn| {
            ensure_schema(conn, self.auto_migrate)?;
            let accepted_count: i64 =
                conn.query_row("SELECT COUNT(*) FROM accepted_message", [], |row| {
                    row.get(0)
                })?;
            let delivery_count: i64 =
                conn.query_row("SELECT COUNT(*) FROM sink_delivery", [], |row| row.get(0))?;
            Ok((accepted_count, delivery_count))
        })
    }

    fn with_connection<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        if let Some(parent) = self.database_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::create_dir_all(&self.artifact_root)?;
        let conn = Connection::open(&self.database_path)
            .with_context(|| format!("open sqlite {}", self.database_path.display()))?;
        f(&conn)
    }
}

fn ensure_schema(conn: &Connection, auto_migrate: bool) -> Result<()> {
    let already_initialized: Option<String> = conn
        .query_row(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='accepted_message'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if already_initialized.is_some() {
        return Ok(());
    }
    if !auto_migrate {
        bail!("database schema missing and storage.auto_migrate is false");
    }
    conn.execute_batch(
        "PRAGMA foreign_keys=ON;
        CREATE TABLE IF NOT EXISTS accepted_message (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            server TEXT NOT NULL,
            server_key TEXT NOT NULL,
            message_id TEXT NOT NULL,
            message_timestamp TEXT NOT NULL,
            inbound_event TEXT NOT NULL,
            author_id TEXT NOT NULL,
            author_name TEXT NOT NULL,
            text TEXT NOT NULL,
            sender_embeddings_json TEXT,
            attention_space_id TEXT,
            attention_fallback INTEGER NOT NULL,
            receptor_matches_json TEXT NOT NULL,
            accepted_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            UNIQUE(server_key, message_id)
        );
        CREATE TABLE IF NOT EXISTS sink_delivery (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            accepted_message_id INTEGER NOT NULL REFERENCES accepted_message(id) ON DELETE RESTRICT,
            sink_key TEXT NOT NULL,
            sink_kind TEXT NOT NULL,
            destination TEXT NOT NULL,
            config_json TEXT NOT NULL,
            status TEXT NOT NULL,
            delivered_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            error TEXT,
            UNIQUE(accepted_message_id, sink_key)
        );",
    )?;
    Ok(())
}

fn upsert_accepted_message(
    conn: &Connection,
    envelope: &WakeEnvelope,
    attention: &AttentionResult,
) -> Result<AcceptedMessageRecord> {
    let sender_embeddings_json = serde_json::to_string(&envelope.sender_embeddings)?;
    let receptor_matches_json = serde_json::to_string(
        &attention
            .matches
            .iter()
            .map(receptor_match_json)
            .collect::<Vec<_>>(),
    )?;
    let inserted = conn.execute(
        "INSERT OR IGNORE INTO accepted_message (
            server,
            server_key,
            message_id,
            message_timestamp,
            inbound_event,
            author_id,
            author_name,
            text,
            sender_embeddings_json,
            attention_space_id,
            attention_fallback,
            receptor_matches_json
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            envelope.server,
            envelope.server_key,
            envelope.message_id,
            envelope.timestamp,
            envelope.inbound_event,
            envelope.author_id,
            envelope.author_name,
            envelope.text,
            sender_embeddings_json,
            attention.space_id,
            if attention.fallback { 1 } else { 0 },
            receptor_matches_json,
        ],
    )? > 0;

    let accepted_message_id = conn
        .query_row(
            "SELECT id FROM accepted_message WHERE server_key = ?1 AND message_id = ?2",
            params![envelope.server_key, envelope.message_id],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| anyhow::anyhow!("accepted_message row missing after upsert"))?;

    Ok(AcceptedMessageRecord {
        accepted_message_id,
        inserted,
    })
}

fn upsert_sink_delivery(
    conn: &Connection,
    accepted_message_id: i64,
    sink_key: &str,
    sink_kind: &str,
    destination: &str,
    config_json: &str,
) -> Result<()> {
    if destination.trim().is_empty() {
        bail!("sink destination must not be empty");
    }
    conn.execute(
        "INSERT OR IGNORE INTO sink_delivery (
            accepted_message_id,
            sink_key,
            sink_kind,
            destination,
            config_json,
            status,
            error
        ) VALUES (?1, ?2, ?3, ?4, ?5, 'delivered', NULL)",
        params![
            accepted_message_id,
            sink_key,
            sink_kind,
            destination,
            config_json
        ],
    )?;
    Ok(())
}

fn receptor_match_json(m: &ReceptorMatch) -> serde_json::Value {
    json!({
        "receptor_id": m.receptor_id,
        "class": format!("{:?}", m.class),
        "score": m.score,
        "threshold": m.threshold,
        "above_threshold": m.above_threshold,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attention::AttentionResult;
    use crate::runtime_store::RuntimeStore;
    use std::path::Path;
    use std::sync::Arc;
    use tempfile::tempdir;
    use tokio::sync::Mutex;

    fn test_attention() -> AttentionResult {
        AttentionResult {
            deliver: true,
            matches: vec![],
            space_id: Some("space:test".to_string()),
            fallback: false,
        }
    }

    fn test_envelope(dir: &Path) -> WakeEnvelope {
        WakeEnvelope {
            server: "https://subspace.example.com".to_string(),
            server_key: "subspace_example".to_string(),
            message_id: "msg-1".to_string(),
            timestamp: "2026-05-13T18:00:00Z".to_string(),
            inbound_event: "new_message".to_string(),
            author_id: "agent:sender".to_string(),
            author_name: "Sender".to_string(),
            text: "hello".to_string(),
            sender_embeddings: vec![],
            attention: Arc::new(crate::attention::AttentionLayer::passthrough()),
            runtime: Arc::new(Mutex::new(
                RuntimeStore::load(dir.join("runtime.json"), 500, None).unwrap(),
            )),
            wake_session_key_override: None,
        }
    }

    #[test]
    fn auto_migrate_false_requires_existing_schema() {
        let dir = tempdir().unwrap();
        let store = DeliveryStore::new(&StorageConfig {
            database_path: dir.path().join("daemon.sqlite3"),
            artifact_root: dir.path().join("artifacts"),
            auto_migrate: false,
        });

        let err = store.ensure_ready().unwrap_err().to_string();
        assert!(err.contains("storage.auto_migrate is false"));
    }

    #[test]
    fn db_sink_is_idempotent_for_replay() {
        let dir = tempdir().unwrap();
        let store = DeliveryStore::new(&StorageConfig {
            database_path: dir.path().join("daemon.sqlite3"),
            artifact_root: dir.path().join("artifacts"),
            auto_migrate: true,
        });
        let envelope = test_envelope(dir.path());
        let attention = test_attention();

        let first = store
            .record_db_sink_delivery(&envelope, &attention)
            .unwrap();
        let second = store
            .record_db_sink_delivery(&envelope, &attention)
            .unwrap();
        let counts = store.counts().unwrap();

        assert!(first.inserted);
        assert!(!second.inserted);
        assert_eq!(counts, (1, 1));
    }

    #[test]
    fn wake_sink_audit_is_idempotent_for_replay() {
        let dir = tempdir().unwrap();
        let store = DeliveryStore::new(&StorageConfig {
            database_path: dir.path().join("daemon.sqlite3"),
            artifact_root: dir.path().join("artifacts"),
            auto_migrate: true,
        });
        let envelope = test_envelope(dir.path());
        let attention = test_attention();

        store
            .record_wake_sink_delivery(
                &envelope,
                &attention,
                "agent_session_wake",
                "agent:test:main",
            )
            .unwrap();
        store
            .record_wake_sink_delivery(
                &envelope,
                &attention,
                "agent_session_wake",
                "agent:test:main",
            )
            .unwrap();
        let counts = store.counts().unwrap();

        assert_eq!(counts, (1, 1));
    }
}
