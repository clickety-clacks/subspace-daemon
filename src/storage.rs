use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::json;

use crate::attention::{AttentionResult, ReceptorMatch};
use crate::config::{SinkKind, StorageConfig};
use crate::supervisor::WakeEnvelope;

#[derive(Debug, Clone)]
pub struct DeliveryStore {
    database_path: PathBuf,
    artifact_root: PathBuf,
    auto_migrate: bool,
}

#[derive(Debug, Clone)]
pub struct EventRecord {
    pub daemon_event_id: i64,
    pub inserted: bool,
}

#[derive(Debug, Clone)]
pub struct DeliveryTicket {
    pub daemon_event_id: i64,
    pub delivery_id: i64,
    pub already_delivered: bool,
}

#[derive(Debug, Clone)]
pub struct SinkSnapshot<'a> {
    pub sink_key: &'a str,
    pub sink_kind: SinkKind,
    pub destination: &'a str,
    pub config_json: String,
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
    ) -> Result<EventRecord> {
        self.with_connection(|conn| {
            ensure_schema(conn, self.auto_migrate)?;
            let event = upsert_canonical_event(conn, envelope, attention)?;
            let db_destination = self.database_path.to_string_lossy().to_string();
            let snapshot = SinkSnapshot {
                sink_key: "db",
                sink_kind: SinkKind::Db,
                destination: &db_destination,
                config_json: json!({
                    "database_path": self.database_path,
                    "artifact_root": self.artifact_root,
                })
                .to_string(),
            };
            let sink_target_id = upsert_sink_target(conn, &snapshot)?;
            let delivery_id =
                upsert_sink_delivery(conn, event.daemon_event_id, sink_target_id, &snapshot)?;
            mark_delivery_delivered(conn, delivery_id)?;
            Ok(event)
        })
    }

    pub fn queue_wake_sink_delivery(
        &self,
        envelope: &WakeEnvelope,
        attention: &AttentionResult,
        snapshot: &SinkSnapshot<'_>,
    ) -> Result<DeliveryTicket> {
        self.with_connection(|conn| {
            ensure_schema(conn, self.auto_migrate)?;
            let event = upsert_canonical_event(conn, envelope, attention)?;
            let sink_target_id = upsert_sink_target(conn, snapshot)?;
            let delivery_id =
                upsert_sink_delivery(conn, event.daemon_event_id, sink_target_id, snapshot)?;
            let already_delivered = conn.query_row(
                "SELECT status = 'delivered' FROM sink_delivery WHERE id = ?1",
                params![delivery_id],
                |row| row.get::<_, bool>(0),
            )?;
            Ok(DeliveryTicket {
                daemon_event_id: event.daemon_event_id,
                delivery_id,
                already_delivered,
            })
        })
    }

    pub fn mark_delivery_attempted(&self, delivery_id: i64) -> Result<()> {
        self.with_connection(|conn| {
            ensure_schema(conn, self.auto_migrate)?;
            conn.execute(
                "UPDATE sink_delivery
                 SET status = 'attempted',
                     attempted_at = CURRENT_TIMESTAMP,
                     error = NULL
                 WHERE id = ?1",
                params![delivery_id],
            )?;
            Ok(())
        })
    }

    pub fn mark_delivery_failed(&self, delivery_id: i64, error: &str) -> Result<()> {
        self.with_connection(|conn| {
            ensure_schema(conn, self.auto_migrate)?;
            conn.execute(
                "UPDATE sink_delivery
                 SET status = 'failed',
                     attempted_at = COALESCE(attempted_at, CURRENT_TIMESTAMP),
                     failed_at = CURRENT_TIMESTAMP,
                     error = ?2
                 WHERE id = ?1",
                params![delivery_id, error],
            )?;
            Ok(())
        })
    }

    pub fn mark_delivery_delivered(&self, delivery_id: i64) -> Result<()> {
        self.with_connection(|conn| {
            ensure_schema(conn, self.auto_migrate)?;
            mark_delivery_delivered(conn, delivery_id)
        })
    }

    pub fn counts(&self) -> Result<(i64, i64, i64, i64, i64, i64, i64)> {
        self.with_connection(|conn| {
            ensure_schema(conn, self.auto_migrate)?;
            Ok((
                table_count(conn, "ingress_source")?,
                table_count(conn, "daemon_event")?,
                table_count(conn, "event_idempotency")?,
                table_count(conn, "receptor_match")?,
                table_count(conn, "sink_target")?,
                table_count(conn, "sink_delivery")?,
                table_count(conn, "event_artifact")?,
            ))
        })
    }

    fn with_connection<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        if let Some(parent) = self.database_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::create_dir_all(&self.artifact_root)?;
        let conn = Connection::open(&self.database_path)
            .with_context(|| format!("open sqlite {}", self.database_path.display()))?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;
        f(&conn)
    }
}

fn ensure_schema(conn: &Connection, auto_migrate: bool) -> Result<()> {
    let already_initialized: Option<String> = conn
        .query_row(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='daemon_event'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if already_initialized.is_some() {
        return Ok(());
    }
    if legacy_schema_present(conn)? {
        bail!(
            "legacy accepted_message schema present; manual migration required before continuing"
        );
    }
    if !auto_migrate {
        bail!("database schema missing and storage.auto_migrate is false");
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS ingress_source (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            server TEXT NOT NULL,
            server_key TEXT NOT NULL UNIQUE,
            created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        );
        CREATE TABLE IF NOT EXISTS daemon_event (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            ingress_source_id INTEGER NOT NULL REFERENCES ingress_source(id) ON DELETE RESTRICT,
            message_id TEXT NOT NULL,
            message_timestamp TEXT NOT NULL,
            inbound_event TEXT NOT NULL,
            author_id TEXT NOT NULL,
            author_name TEXT NOT NULL,
            text TEXT NOT NULL,
            sender_embeddings_json TEXT,
            attention_space_id TEXT,
            attention_fallback INTEGER NOT NULL,
            payload_json TEXT,
            raw_body TEXT,
            accepted_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        );
        CREATE TABLE IF NOT EXISTS event_idempotency (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            ingress_source_id INTEGER NOT NULL REFERENCES ingress_source(id) ON DELETE RESTRICT,
            scope TEXT NOT NULL,
            idempotency_key TEXT NOT NULL,
            daemon_event_id INTEGER NOT NULL REFERENCES daemon_event(id) ON DELETE RESTRICT,
            created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            UNIQUE(ingress_source_id, scope, idempotency_key)
        );
        CREATE TABLE IF NOT EXISTS receptor_match (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            daemon_event_id INTEGER NOT NULL REFERENCES daemon_event(id) ON DELETE CASCADE,
            receptor_id TEXT NOT NULL,
            receptor_class TEXT NOT NULL,
            score REAL NOT NULL,
            threshold REAL NOT NULL,
            above_threshold INTEGER NOT NULL,
            routing_json TEXT NOT NULL,
            matched_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            UNIQUE(daemon_event_id, receptor_id, receptor_class)
        );
        CREATE TABLE IF NOT EXISTS sink_target (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            sink_key TEXT NOT NULL UNIQUE,
            sink_kind TEXT NOT NULL,
            destination TEXT NOT NULL,
            config_json TEXT NOT NULL,
            disabled_at TEXT,
            disabled_reason TEXT,
            created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        );
        CREATE TABLE IF NOT EXISTS sink_delivery (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            daemon_event_id INTEGER NOT NULL REFERENCES daemon_event(id) ON DELETE RESTRICT,
            sink_target_id INTEGER NOT NULL REFERENCES sink_target(id) ON DELETE RESTRICT,
            sink_key_snapshot TEXT NOT NULL,
            sink_kind_snapshot TEXT NOT NULL,
            destination_snapshot TEXT NOT NULL,
            config_json_snapshot TEXT NOT NULL,
            status TEXT NOT NULL,
            queued_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            attempted_at TEXT,
            delivered_at TEXT,
            failed_at TEXT,
            skipped_at TEXT,
            error TEXT,
            UNIQUE(daemon_event_id, sink_target_id)
        );
        CREATE TABLE IF NOT EXISTS event_artifact (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            daemon_event_id INTEGER NOT NULL REFERENCES daemon_event(id) ON DELETE CASCADE,
            artifact_kind TEXT NOT NULL,
            storage_uri TEXT NOT NULL,
            mime_type TEXT,
            byte_length INTEGER,
            sha256 TEXT,
            metadata_json TEXT,
            created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        );
        CREATE INDEX IF NOT EXISTS idx_daemon_event_ingress_message ON daemon_event(ingress_source_id, message_id);
        CREATE INDEX IF NOT EXISTS idx_receptor_match_event ON receptor_match(daemon_event_id);
        CREATE INDEX IF NOT EXISTS idx_sink_delivery_event ON sink_delivery(daemon_event_id);
        CREATE INDEX IF NOT EXISTS idx_sink_delivery_status ON sink_delivery(status);",
    )?;
    Ok(())
}

fn legacy_schema_present(conn: &Connection) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='accepted_message'",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn upsert_canonical_event(
    conn: &Connection,
    envelope: &WakeEnvelope,
    attention: &AttentionResult,
) -> Result<EventRecord> {
    let ingress_source_id = upsert_ingress_source(conn, &envelope.server, &envelope.server_key)?;
    let idempotency_key = compute_event_idempotency_key(envelope);
    if let Some(existing_id) = lookup_existing_event(conn, ingress_source_id, &idempotency_key)? {
        upsert_receptor_matches(conn, existing_id, attention)?;
        return Ok(EventRecord {
            daemon_event_id: existing_id,
            inserted: false,
        });
    }

    let sender_embeddings_json = serde_json::to_string(&envelope.sender_embeddings)?;
    let payload_json = json!({
        "server": envelope.server,
        "server_key": envelope.server_key,
        "message_id": envelope.message_id,
        "message_timestamp": envelope.timestamp,
        "inbound_event": envelope.inbound_event,
        "author_id": envelope.author_id,
        "author_name": envelope.author_name,
        "text": envelope.text,
    })
    .to_string();

    conn.execute(
        "INSERT INTO daemon_event (
            ingress_source_id,
            message_id,
            message_timestamp,
            inbound_event,
            author_id,
            author_name,
            text,
            sender_embeddings_json,
            attention_space_id,
            attention_fallback,
            payload_json,
            raw_body
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            ingress_source_id,
            envelope.message_id,
            envelope.timestamp,
            envelope.inbound_event,
            envelope.author_id,
            envelope.author_name,
            envelope.text,
            sender_embeddings_json,
            attention.space_id,
            if attention.fallback { 1 } else { 0 },
            payload_json,
            envelope.text,
        ],
    )?;
    let daemon_event_id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO event_idempotency (ingress_source_id, scope, idempotency_key, daemon_event_id)
         VALUES (?1, 'subspace_message', ?2, ?3)",
        params![ingress_source_id, idempotency_key, daemon_event_id],
    )?;
    upsert_receptor_matches(conn, daemon_event_id, attention)?;
    Ok(EventRecord {
        daemon_event_id,
        inserted: true,
    })
}

fn upsert_ingress_source(conn: &Connection, server: &str, server_key: &str) -> Result<i64> {
    conn.execute(
        "INSERT INTO ingress_source (server, server_key)
         VALUES (?1, ?2)
         ON CONFLICT(server_key) DO UPDATE SET server = excluded.server",
        params![server, server_key],
    )?;
    conn.query_row(
        "SELECT id FROM ingress_source WHERE server_key = ?1",
        params![server_key],
        |row| row.get(0),
    )
    .map_err(Into::into)
}

fn lookup_existing_event(
    conn: &Connection,
    ingress_source_id: i64,
    idempotency_key: &str,
) -> Result<Option<i64>> {
    conn.query_row(
        "SELECT daemon_event_id FROM event_idempotency
         WHERE ingress_source_id = ?1 AND scope = 'subspace_message' AND idempotency_key = ?2",
        params![ingress_source_id, idempotency_key],
        |row| row.get(0),
    )
    .optional()
    .map_err(Into::into)
}

fn upsert_receptor_matches(
    conn: &Connection,
    daemon_event_id: i64,
    attention: &AttentionResult,
) -> Result<()> {
    for matched in &attention.matches {
        let routing_json = receptor_routing_json(matched).to_string();
        conn.execute(
            "INSERT INTO receptor_match (
                daemon_event_id,
                receptor_id,
                receptor_class,
                score,
                threshold,
                above_threshold,
                routing_json
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            ON CONFLICT(daemon_event_id, receptor_id, receptor_class)
            DO UPDATE SET
                score = excluded.score,
                threshold = excluded.threshold,
                above_threshold = excluded.above_threshold,
                routing_json = excluded.routing_json",
            params![
                daemon_event_id,
                matched.receptor_id,
                matched.class.as_str(),
                matched.score,
                matched.threshold,
                if matched.above_threshold { 1 } else { 0 },
                routing_json,
            ],
        )?;
    }
    Ok(())
}

fn upsert_sink_target(conn: &Connection, snapshot: &SinkSnapshot<'_>) -> Result<i64> {
    if snapshot.destination.trim().is_empty() {
        bail!("sink destination must not be empty");
    }
    conn.execute(
        "INSERT INTO sink_target (sink_key, sink_kind, destination, config_json, disabled_at, disabled_reason, updated_at)
         VALUES (?1, ?2, ?3, ?4, NULL, NULL, CURRENT_TIMESTAMP)
         ON CONFLICT(sink_key) DO UPDATE SET
            sink_kind = excluded.sink_kind,
            destination = excluded.destination,
            config_json = excluded.config_json,
            disabled_at = NULL,
            disabled_reason = NULL,
            updated_at = CURRENT_TIMESTAMP",
        params![
            snapshot.sink_key,
            snapshot.sink_kind.as_str(),
            snapshot.destination,
            snapshot.config_json,
        ],
    )?;
    conn.query_row(
        "SELECT id FROM sink_target WHERE sink_key = ?1",
        params![snapshot.sink_key],
        |row| row.get(0),
    )
    .map_err(Into::into)
}

fn upsert_sink_delivery(
    conn: &Connection,
    daemon_event_id: i64,
    sink_target_id: i64,
    snapshot: &SinkSnapshot<'_>,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO sink_delivery (
            daemon_event_id,
            sink_target_id,
            sink_key_snapshot,
            sink_kind_snapshot,
            destination_snapshot,
            config_json_snapshot,
            status
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'queued')
        ON CONFLICT(daemon_event_id, sink_target_id)
        DO UPDATE SET
            sink_key_snapshot = excluded.sink_key_snapshot,
            sink_kind_snapshot = excluded.sink_kind_snapshot,
            destination_snapshot = excluded.destination_snapshot,
            config_json_snapshot = excluded.config_json_snapshot,
            status = CASE WHEN sink_delivery.status = 'delivered' THEN sink_delivery.status ELSE 'queued' END,
            error = CASE WHEN sink_delivery.status = 'delivered' THEN sink_delivery.error ELSE NULL END,
            queued_at = CASE WHEN sink_delivery.status = 'delivered' THEN sink_delivery.queued_at ELSE CURRENT_TIMESTAMP END",
        params![
            daemon_event_id,
            sink_target_id,
            snapshot.sink_key,
            snapshot.sink_kind.as_str(),
            snapshot.destination,
            snapshot.config_json,
        ],
    )?;
    conn.query_row(
        "SELECT id FROM sink_delivery WHERE daemon_event_id = ?1 AND sink_target_id = ?2",
        params![daemon_event_id, sink_target_id],
        |row| row.get(0),
    )
    .map_err(Into::into)
}

fn mark_delivery_delivered(conn: &Connection, delivery_id: i64) -> Result<()> {
    conn.execute(
        "UPDATE sink_delivery
         SET status = 'delivered',
             attempted_at = COALESCE(attempted_at, CURRENT_TIMESTAMP),
             delivered_at = CURRENT_TIMESTAMP,
             failed_at = NULL,
             skipped_at = NULL,
             error = NULL
         WHERE id = ?1",
        params![delivery_id],
    )?;
    Ok(())
}

fn table_count(conn: &Connection, table: &str) -> Result<i64> {
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
        row.get(0)
    })
    .map_err(Into::into)
}

fn compute_event_idempotency_key(envelope: &WakeEnvelope) -> String {
    format!("{}:{}", envelope.server_key, envelope.message_id)
}

fn receptor_routing_json(matched: &ReceptorMatch) -> serde_json::Value {
    json!({
        "decision": if matched.above_threshold { "accepted" } else { "candidate" },
        "candidate_sinks": [],
        "selected_sinks": [],
        "score": matched.score,
        "threshold": matched.threshold,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attention::AttentionResult;
    use crate::runtime_store::RuntimeStore;
    use std::sync::Arc;
    use tempfile::tempdir;
    use tokio::sync::Mutex;

    fn test_attention() -> AttentionResult {
        AttentionResult {
            deliver: true,
            matches: vec![ReceptorMatch {
                receptor_id: "apple-platforms".to_string(),
                class: crate::attention::receptor::ReceptorClass::Broad,
                score: 0.9,
                threshold: 0.7,
                above_threshold: true,
            }],
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
    fn creates_spec_tables_and_artifact_root() {
        let dir = tempdir().unwrap();
        let store = DeliveryStore::new(&StorageConfig {
            database_path: dir.path().join("daemon.sqlite3"),
            artifact_root: dir.path().join("artifacts"),
            auto_migrate: true,
        });

        store.ensure_ready().unwrap();
        let counts = store.counts().unwrap();
        assert_eq!(counts, (0, 0, 0, 0, 0, 0, 0));
        assert!(dir.path().join("artifacts").exists());
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
        assert_eq!(counts, (1, 1, 1, 1, 1, 1, 0));
    }

    #[test]
    fn wake_sink_ticket_is_idempotent_for_replay() {
        let dir = tempdir().unwrap();
        let store = DeliveryStore::new(&StorageConfig {
            database_path: dir.path().join("daemon.sqlite3"),
            artifact_root: dir.path().join("artifacts"),
            auto_migrate: true,
        });
        let envelope = test_envelope(dir.path());
        let attention = test_attention();
        let snapshot = SinkSnapshot {
            sink_key: "wake-primary",
            sink_kind: SinkKind::AgentSessionWake,
            destination: "agent:test:main",
            config_json: json!({"session_key": "agent:test:main"}).to_string(),
        };

        let first = store
            .queue_wake_sink_delivery(&envelope, &attention, &snapshot)
            .unwrap();
        store.mark_delivery_attempted(first.delivery_id).unwrap();
        store.mark_delivery_delivered(first.delivery_id).unwrap();
        let second = store
            .queue_wake_sink_delivery(&envelope, &attention, &snapshot)
            .unwrap();
        let counts = store.counts().unwrap();

        assert!(second.already_delivered);
        assert_eq!(first.daemon_event_id, second.daemon_event_id);
        assert_eq!(counts, (1, 1, 1, 1, 1, 1, 0));
    }

    #[test]
    fn failed_delivery_row_is_reused_not_duplicated() {
        let dir = tempdir().unwrap();
        let store = DeliveryStore::new(&StorageConfig {
            database_path: dir.path().join("daemon.sqlite3"),
            artifact_root: dir.path().join("artifacts"),
            auto_migrate: true,
        });
        let envelope = test_envelope(dir.path());
        let attention = test_attention();
        let snapshot = SinkSnapshot {
            sink_key: "wake-a",
            sink_kind: SinkKind::AgentSessionWake,
            destination: "agent:a:main",
            config_json: json!({"session_key": "agent:a:main"}).to_string(),
        };

        let first = store
            .queue_wake_sink_delivery(&envelope, &attention, &snapshot)
            .unwrap();
        store.mark_delivery_attempted(first.delivery_id).unwrap();
        store
            .mark_delivery_failed(first.delivery_id, "network blew up")
            .unwrap();
        let second = store
            .queue_wake_sink_delivery(&envelope, &attention, &snapshot)
            .unwrap();
        assert_eq!(first.delivery_id, second.delivery_id);
        assert!(!second.already_delivered);

        let status: String = Connection::open(dir.path().join("daemon.sqlite3"))
            .unwrap()
            .query_row(
                "SELECT status FROM sink_delivery WHERE id = ?1",
                params![first.delivery_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "queued");
    }

    #[test]
    fn distinct_wake_sinks_keep_distinct_targets() {
        let dir = tempdir().unwrap();
        let store = DeliveryStore::new(&StorageConfig {
            database_path: dir.path().join("daemon.sqlite3"),
            artifact_root: dir.path().join("artifacts"),
            auto_migrate: true,
        });
        let envelope = test_envelope(dir.path());
        let attention = test_attention();
        let sink_a = SinkSnapshot {
            sink_key: "wake-a",
            sink_kind: SinkKind::AgentSessionWake,
            destination: "agent:a:main",
            config_json: json!({"session_key": "agent:a:main"}).to_string(),
        };
        let sink_b = SinkSnapshot {
            sink_key: "wake-b",
            sink_kind: SinkKind::AgentSessionWake,
            destination: "agent:b:main",
            config_json: json!({"session_key": "agent:b:main"}).to_string(),
        };

        store
            .queue_wake_sink_delivery(&envelope, &attention, &sink_a)
            .unwrap();
        store
            .queue_wake_sink_delivery(&envelope, &attention, &sink_b)
            .unwrap();

        let conn = Connection::open(dir.path().join("daemon.sqlite3")).unwrap();
        let destinations: Vec<String> = conn
            .prepare("SELECT destination FROM sink_target ORDER BY sink_key")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(|row| row.unwrap())
            .collect();
        assert_eq!(
            destinations,
            vec!["agent:a:main".to_string(), "agent:b:main".to_string()]
        );
    }

    #[test]
    fn legacy_schema_is_rejected() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("daemon.sqlite3");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch("CREATE TABLE accepted_message (id INTEGER PRIMARY KEY);")
            .unwrap();
        drop(conn);
        let store = DeliveryStore::new(&StorageConfig {
            database_path: db_path,
            artifact_root: dir.path().join("artifacts"),
            auto_migrate: true,
        });

        let err = store.ensure_ready().unwrap_err().to_string();
        assert!(err.contains("legacy accepted_message schema present"));
    }
}
