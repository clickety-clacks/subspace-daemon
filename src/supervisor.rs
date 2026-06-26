use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use serde::Serialize;
use tokio::sync::{Mutex, RwLock, broadcast, mpsc};
use tracing::{error, info, warn};

use crate::attention::{
    AttentionDisposition, AttentionLayer, AttentionResult, MessageEmbedding,
    format_attention_annotation,
};
use crate::config::{Config, RetryConfig, SinkConfig, SinkKind};
use crate::gateway::client::{GatewayClientHandle, start_gateway_client};
use crate::hard_failure::{HardFailureEvent, HardFailureHooks};
use crate::ipc::{SendRouter, run_ipc_server};
use crate::launchd::render_launchd_plist;
use crate::retry::jitter;
use crate::runtime_store::RuntimeStore;
use crate::setup::{LiveSetupState, SharedServerHandles, SharedServerTasks, spawn_server_manager};
use crate::state_lock::StateLock;
use crate::storage::{DeliveryStore, RoutingSnapshot, SinkRoutingEntry, SinkSnapshot};

#[derive(Debug, Clone, Serialize)]
pub struct AttentionHealth {
    pub receptor_count: usize,
    pub interest_receptor_count: usize,
    pub veto_receptor_count: usize,
    pub wildcard_receptor_count: usize,
    pub delivery_mode: String,
    pub degraded: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ServerHealth {
    pub server: String,
    pub server_key: String,
    pub subspace_state: String,
    pub session_expires_at: Option<String>,
    pub veto_enforcement_state: String,
    pub attention: AttentionHealth,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub consecutive_failures: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cooldown_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_attempt_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error_kind: Option<String>,
}

#[derive(Debug, Clone)]
pub struct DaemonStatus {
    pub gateway_state: String,
    pub wake_session_key: String,
    pub servers: BTreeMap<String, ServerHealth>,
}

impl DaemonStatus {
    pub fn new(config: &Config) -> Self {
        let servers = config
            .servers
            .iter()
            .filter(|server| server.enabled)
            .map(|server| {
                (
                    server.base_url.clone(),
                    ServerHealth {
                        server: server.base_url.clone(),
                        server_key: server.server_key.clone(),
                        subspace_state: "connecting".to_string(),
                        session_expires_at: None,
                        veto_enforcement_state: "not_configured".to_string(),
                        attention: AttentionHealth::not_configured(),
                        consecutive_failures: None,
                        cooldown_ms: None,
                        next_attempt_at: None,
                        last_error_kind: None,
                    },
                )
            })
            .collect();
        Self {
            gateway_state: "connecting".to_string(),
            wake_session_key: config.routing.wake_session_key.clone(),
            servers,
        }
    }

    pub fn is_healthy(&self) -> bool {
        matches!(self.gateway_state.as_str(), "live" | "pairing_required")
            && self
                .servers
                .values()
                .all(|server| match server.subspace_state.as_str() {
                    "live" => !session_expires_soon(server.session_expires_at.as_deref()),
                    "reconnecting" | "reconnect_cooldown" | "subspace_auth_required" => true,
                    _ => false,
                })
    }

    pub fn set_gateway_state(&mut self, value: impl Into<String>) {
        self.gateway_state = value.into();
    }

    pub fn set_server_state(&mut self, base_url: &str, server_key: &str, value: impl Into<String>) {
        let veto_enforcement_state = self
            .servers
            .get(base_url)
            .map(|server| server.veto_enforcement_state.clone())
            .unwrap_or_else(|| "not_configured".to_string());
        let attention = self
            .servers
            .get(base_url)
            .map(|server| server.attention.clone())
            .unwrap_or_else(AttentionHealth::not_configured);
        let session_expires_at = self
            .servers
            .get(base_url)
            .and_then(|server| server.session_expires_at.clone());
        self.servers.insert(
            base_url.to_string(),
            ServerHealth {
                server: base_url.to_string(),
                server_key: server_key.to_string(),
                subspace_state: value.into(),
                session_expires_at,
                veto_enforcement_state,
                attention,
                consecutive_failures: None,
                cooldown_ms: None,
                next_attempt_at: None,
                last_error_kind: None,
            },
        );
    }

    pub fn set_server_reconnect_cooldown(
        &mut self,
        base_url: &str,
        server_key: &str,
        consecutive_failures: u32,
        cooldown_ms: u64,
        next_attempt_at: String,
        last_error_kind: Option<String>,
    ) {
        let veto_enforcement_state = self
            .servers
            .get(base_url)
            .map(|server| server.veto_enforcement_state.clone())
            .unwrap_or_else(|| "not_configured".to_string());
        let attention = self
            .servers
            .get(base_url)
            .map(|server| server.attention.clone())
            .unwrap_or_else(AttentionHealth::not_configured);
        let session_expires_at = self
            .servers
            .get(base_url)
            .and_then(|server| server.session_expires_at.clone());
        self.servers.insert(
            base_url.to_string(),
            ServerHealth {
                server: base_url.to_string(),
                server_key: server_key.to_string(),
                subspace_state: "reconnect_cooldown".to_string(),
                session_expires_at,
                veto_enforcement_state,
                attention,
                consecutive_failures: Some(consecutive_failures),
                cooldown_ms: Some(cooldown_ms),
                next_attempt_at: Some(next_attempt_at),
                last_error_kind,
            },
        );
    }

    pub fn set_server_veto_enforcement_state(
        &mut self,
        base_url: &str,
        server_key: &str,
        state: impl Into<String>,
    ) {
        let state = state.into();
        if let Some(server) = self.servers.get_mut(base_url) {
            server.server_key = server_key.to_string();
            server.veto_enforcement_state = state;
            return;
        }

        self.servers.insert(
            base_url.to_string(),
            ServerHealth {
                server: base_url.to_string(),
                server_key: server_key.to_string(),
                subspace_state: "connecting".to_string(),
                session_expires_at: None,
                veto_enforcement_state: state,
                attention: AttentionHealth::not_configured(),
                consecutive_failures: None,
                cooldown_ms: None,
                next_attempt_at: None,
                last_error_kind: None,
            },
        );
    }

    pub fn set_server_attention_health(
        &mut self,
        base_url: &str,
        server_key: &str,
        attention: AttentionHealth,
    ) {
        if let Some(server) = self.servers.get_mut(base_url) {
            server.server_key = server_key.to_string();
            server.veto_enforcement_state = attention.veto_enforcement_state().to_string();
            server.attention = attention;
            return;
        }

        self.servers.insert(
            base_url.to_string(),
            ServerHealth {
                server: base_url.to_string(),
                server_key: server_key.to_string(),
                subspace_state: "connecting".to_string(),
                session_expires_at: None,
                veto_enforcement_state: attention.veto_enforcement_state().to_string(),
                attention,
                consecutive_failures: None,
                cooldown_ms: None,
                next_attempt_at: None,
                last_error_kind: None,
            },
        );
    }

    pub fn server_state(&self, base_url: &str) -> Option<String> {
        self.servers
            .get(base_url)
            .map(|server| server.subspace_state.clone())
    }

    pub fn set_server_session_expires_at(
        &mut self,
        base_url: &str,
        server_key: &str,
        session_expires_at: Option<String>,
    ) {
        if let Some(server) = self.servers.get_mut(base_url) {
            server.server_key = server_key.to_string();
            server.session_expires_at = session_expires_at;
            return;
        }

        self.servers.insert(
            base_url.to_string(),
            ServerHealth {
                server: base_url.to_string(),
                server_key: server_key.to_string(),
                subspace_state: "connecting".to_string(),
                session_expires_at,
                veto_enforcement_state: "not_configured".to_string(),
                attention: AttentionHealth::not_configured(),
                consecutive_failures: None,
                cooldown_ms: None,
                next_attempt_at: None,
                last_error_kind: None,
            },
        );
    }

    pub fn servers_snapshot(&self) -> Vec<ServerHealth> {
        self.servers.values().cloned().collect()
    }
}

fn session_expires_soon(session_expires_at: Option<&str>) -> bool {
    let Some(session_expires_at) = session_expires_at else {
        return true;
    };
    let Ok(expires_at) = time::OffsetDateTime::parse(
        session_expires_at,
        &time::format_description::well_known::Rfc3339,
    ) else {
        return true;
    };
    expires_at - time::OffsetDateTime::now_utc() <= time::Duration::hours(24)
}

impl AttentionHealth {
    pub fn from_layer(layer: &AttentionLayer) -> Self {
        Self {
            receptor_count: layer.receptor_count(),
            interest_receptor_count: layer.interest_receptor_count(),
            veto_receptor_count: layer.veto_receptor_count(),
            wildcard_receptor_count: layer.wildcard_receptor_count(),
            delivery_mode: layer.delivery_mode().to_string(),
            degraded: layer.is_degraded(),
        }
    }

    pub fn not_configured() -> Self {
        Self {
            receptor_count: 0,
            interest_receptor_count: 0,
            veto_receptor_count: 0,
            wildcard_receptor_count: 0,
            delivery_mode: "no_active_receptors".to_string(),
            degraded: false,
        }
    }

    pub fn veto_enforcement_state(&self) -> &'static str {
        if self.delivery_mode == "veto_enforcement_unavailable" {
            "unavailable"
        } else if self.veto_receptor_count > 0 {
            "ready"
        } else {
            "not_configured"
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ServerSendResultEnvelope {
    pub server: String,
    pub sent: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subspace_message_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub struct WakeEnvelope {
    pub server: String,
    pub server_key: String,
    pub message_id: String,
    pub timestamp: String,
    pub inbound_event: String,
    pub author_id: String,
    pub author_name: String,
    pub text: String,
    pub sender_embeddings: Vec<MessageEmbedding>,
    pub attention: Arc<AttentionLayer>,
    pub runtime: Arc<Mutex<RuntimeStore>>,
    pub wake_session_key_override: Option<String>,
}

pub async fn run_supervisor(config: Config) -> Result<()> {
    let _lock = StateLock::acquire(&config.paths.state_lock_path)?;
    tokio::fs::create_dir_all(&config.paths.root).await?;
    tokio::fs::create_dir_all(config.paths.log_file.parent().unwrap()).await?;
    if let Ok(current_exe) = std::env::current_exe() {
        tokio::fs::write(
            &config.paths.launchd_plist_path,
            render_launchd_plist(
                current_exe.as_path(),
                &config.paths.config_path,
                &dirs::home_dir().expect("home dir"),
            ),
        )
        .await
        .ok();
    }

    let status = Arc::new(RwLock::new(DaemonStatus::new(&config)));
    let hard_failure_hooks = HardFailureHooks::new(config.hard_failure_hooks.clone());
    let delivery_store = if config
        .sinks
        .iter()
        .any(|sink| sink.enabled && sink.kind == SinkKind::Db)
    {
        let store = DeliveryStore::new(&config.storage);
        if let Err(err) = store.ensure_ready() {
            hard_failure_hooks
                .fire(HardFailureEvent::new(
                    "db_sink_ready_failed",
                    "storage",
                    Some(config.storage.database_path.display().to_string()),
                    err.to_string(),
                    serde_json::json!({
                        "database_path": config.storage.database_path,
                    }),
                ))
                .await;
            return Err(err);
        }
        Some(store)
    } else {
        None
    };
    let (shutdown_tx, _) = broadcast::channel(4);

    let (gateway, gateway_task) = start_gateway_client(
        config.clone(),
        status.clone(),
        hard_failure_hooks.clone(),
        shutdown_tx.subscribe(),
    )
    .await?;

    let (wake_tx, wake_rx) = mpsc::channel(1000);
    let server_tasks: SharedServerTasks = Arc::new(Mutex::new(Vec::new()));
    let server_handles: SharedServerHandles = Arc::new(RwLock::new(BTreeMap::new()));
    let setup_state = LiveSetupState {
        status: status.clone(),
        server_handles: server_handles.clone(),
        server_tasks: server_tasks.clone(),
        mutation_lock: Arc::new(Mutex::new(())),
        wake_tx: wake_tx.clone(),
        retry: config.retry.clone(),
        replay: config.replay.clone(),
        attention: config.attention.clone(),
        hard_failure_hooks: hard_failure_hooks.clone(),
        shutdown_tx: shutdown_tx.clone(),
    };
    for server in config.servers.iter().filter(|server| server.enabled) {
        spawn_server_manager(
            &setup_state,
            server.clone(),
            config.paths.identities_dir.clone(),
        )
        .await?;
    }

    let send_router = SendRouter::new(status.clone(), server_handles.clone());
    let wake_task = tokio::spawn(process_wake_queue(
        wake_rx,
        gateway.clone(),
        config.routing.wake_session_key.clone(),
        config.retry.clone(),
        config.sinks.clone(),
        delivery_store,
        hard_failure_hooks.clone(),
        shutdown_tx.subscribe(),
    ));
    let ipc_task = tokio::spawn(run_ipc_server(
        config.paths.socket_path.clone(),
        status.clone(),
        send_router,
        setup_state,
        shutdown_tx.subscribe(),
    ));

    info!(
        component = "supervisor",
        event = "daemon_started",
        "subspace daemon started"
    );

    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);
    let mut gateway_task = gateway_task;
    let mut wake_task = wake_task;
    let mut ipc_task = ipc_task;

    loop {
        tokio::select! {
            _ = &mut shutdown => {
                warn!(component = "supervisor", event = "daemon_stopping", "received ctrl-c");
                let _ = shutdown_tx.send(());
                let _ = tokio::time::timeout(Duration::from_secs(5), &mut wake_task).await;
                let _ = tokio::time::timeout(Duration::from_secs(5), &mut gateway_task).await;
                let _ = tokio::time::timeout(Duration::from_secs(5), &mut ipc_task).await;
                for (_, task) in server_tasks.lock().await.iter_mut() {
                    let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
                }
                let _ = tokio::fs::remove_file(&config.paths.socket_path).await;
                return Ok(());
            }
            result = &mut gateway_task => {
                let _ = shutdown_tx.send(());
                let _ = tokio::fs::remove_file(&config.paths.socket_path).await;
                return handle_task_exit("gateway", result, &hard_failure_hooks).await;
            }
            result = &mut wake_task => {
                let _ = shutdown_tx.send(());
                let _ = tokio::fs::remove_file(&config.paths.socket_path).await;
                return handle_task_exit("wake_queue", result, &hard_failure_hooks).await;
            }
            result = &mut ipc_task => {
                let _ = shutdown_tx.send(());
                let _ = tokio::fs::remove_file(&config.paths.socket_path).await;
                return handle_task_exit("ipc", result, &hard_failure_hooks).await;
            }
            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                if let Some(result) = check_server_exit(&server_tasks, &hard_failure_hooks).await {
                    let _ = shutdown_tx.send(());
                    let _ = tokio::fs::remove_file(&config.paths.socket_path).await;
                    return result;
                }
            }
        }
    }
}

async fn check_server_exit(
    server_tasks: &SharedServerTasks,
    hard_failure_hooks: &HardFailureHooks,
) -> Option<Result<()>> {
    let mut tasks = server_tasks.lock().await;
    for (server_key, task) in tasks.iter_mut() {
        if task.is_finished() {
            let result = task.await;
            return Some(
                handle_task_exit(
                    &format!("subspace:{server_key}"),
                    result,
                    hard_failure_hooks,
                )
                .await,
            );
        }
    }
    None
}

async fn handle_task_exit(
    task_name: &str,
    result: std::result::Result<Result<()>, tokio::task::JoinError>,
    hard_failure_hooks: &HardFailureHooks,
) -> Result<()> {
    match result {
        Ok(Ok(())) => {
            let err = anyhow!("{task_name} exited unexpectedly");
            fire_task_exit_hook(task_name, &err, hard_failure_hooks).await;
            Err(err)
        }
        Ok(Err(err)) => {
            error!(component = "supervisor", event = "daemon_degraded", task = task_name, error = %err, "critical task failed");
            fire_task_exit_hook(task_name, &err, hard_failure_hooks).await;
            Err(err)
        }
        Err(err) => {
            error!(component = "supervisor", event = "daemon_degraded", task = task_name, error = %err, "critical task panicked");
            let err = anyhow!(err);
            fire_task_exit_hook(task_name, &err, hard_failure_hooks).await;
            Err(err)
        }
    }
}

async fn fire_task_exit_hook(task_name: &str, err: &anyhow::Error, hooks: &HardFailureHooks) {
    hooks
        .fire(HardFailureEvent::new(
            "critical_task_exit",
            "supervisor",
            Some(task_name.to_string()),
            err.to_string(),
            serde_json::json!({ "task": task_name }),
        ))
        .await;
}

async fn process_wake_queue(
    mut rx: mpsc::Receiver<WakeEnvelope>,
    gateway: GatewayClientHandle,
    wake_session_key: String,
    retry: RetryConfig,
    sinks: Vec<SinkConfig>,
    delivery_store: Option<DeliveryStore>,
    hard_failure_hooks: HardFailureHooks,
    mut shutdown: broadcast::Receiver<()>,
) -> Result<()> {
    let mut shutting_down = false;
    let mut shutdown_deadline = None;
    loop {
        let next = if shutting_down {
            match rx.try_recv() {
                Ok(item) => Some(item),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => return Ok(()),
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => return Ok(()),
            }
        } else {
            tokio::select! {
                _ = shutdown.recv() => {
                    shutting_down = true;
                    shutdown_deadline = Some(tokio::time::Instant::now() + Duration::from_secs(5));
                    continue;
                }
                item = rx.recv() => item,
            }
        };
        let Some(item) = next else {
            return Ok(());
        };

        // Evaluate message against receptors
        let attention_result = item
            .attention
            .evaluate_with_embeddings(
                &item.text,
                (!item.sender_embeddings.is_empty()).then_some(item.sender_embeddings.as_slice()),
            )
            .await;

        if attention_result.veto_not_evaluated {
            info!(
                component = "wake_router",
                event = "veto_not_evaluated",
                message_id = %item.message_id,
                server = %item.server,
                space_id = attention_result.space_id.as_deref(),
                "veto receptors were not evaluated because no compatible supplied embedding was available"
            );
        }

        if let Some(store) = delivery_store.as_ref() {
            if let Err(err) = store.record_attention_decision(&item, &attention_result) {
                hard_failure_hooks
                    .fire(storage_hard_failure_event(
                        "db_attention_decision_failed",
                        &item,
                        &err,
                    ))
                    .await;
                return Err(err);
            }
        }

        if !attention_result.deliver {
            match attention_result.disposition {
                AttentionDisposition::Vetoed => {
                    let receptor_ids = attention_result
                        .matches
                        .iter()
                        .filter(|receptor_match| {
                            receptor_match.class.as_str() == "veto"
                                && receptor_match.above_threshold
                        })
                        .map(|receptor_match| receptor_match.receptor_id.as_str())
                        .collect::<Vec<_>>()
                        .join(",");
                    info!(
                        component = "wake_router",
                        event = "message_vetoed",
                        message_id = %item.message_id,
                        server = %item.server,
                        receptor_ids = %receptor_ids,
                        top_score = attention_result.matches.first().map(|m| m.score),
                        space_id = attention_result.space_id.as_deref(),
                        "message vetoed by attention layer"
                    );
                }
                AttentionDisposition::VetoEnforcementUnavailable => {
                    info!(
                        component = "wake_router",
                        event = "message_filtered",
                        reason = "veto_enforcement_unavailable",
                        message_id = %item.message_id,
                        server = %item.server,
                        "message filtered because veto enforcement is unavailable"
                    );
                }
                AttentionDisposition::NoActiveReceptor => {
                    warn!(
                        component = "wake_router",
                        event = "delivery_blocked_no_receptors",
                        message_id = %item.message_id,
                        server = %item.server,
                        disposition = ?attention_result.disposition,
                        "message cannot be delivered because no eligible receptors are configured"
                    );
                }
                AttentionDisposition::Filtered
                | AttentionDisposition::EvaluationUnavailable
                | AttentionDisposition::Deliver => {
                    info!(
                        component = "wake_router",
                        event = "message_filtered",
                        message_id = %item.message_id,
                        server = %item.server,
                        disposition = ?attention_result.disposition,
                        top_score = attention_result.matches.first().map(|m| m.score),
                        "message filtered by attention layer"
                    );
                }
            }
            // Mark as processed so we don't retry
            let mut runtime = item.runtime.lock().await;
            runtime.mark_processed(&item.message_id, &item.timestamp);
            runtime.flush()?;
            continue;
        }

        let active_sink_count = enabled_sink_count(&sinks);
        if active_sink_count == 0 {
            warn!(
                component = "wake_router",
                event = "delivery_blocked_no_sinks",
                message_id = %item.message_id,
                server = %item.server,
                disposition = ?attention_result.disposition,
                receptor_matches = attention_result
                    .matches
                    .iter()
                    .filter(|receptor_match| receptor_match.above_threshold)
                    .count(),
                "message matched receptor policy but cannot be delivered because no sinks are configured"
            );
            hard_failure_hooks
                .fire(HardFailureEvent::new(
                    "sink_delivery_blocked_no_sinks",
                    "wake_router",
                    Some(item.server.clone()),
                    "message matched receptor policy but no sinks are configured",
                    serde_json::json!({
                        "server": item.server,
                        "server_key": item.server_key,
                        "message_id": item.message_id,
                    }),
                ))
                .await;
            let mut runtime = item.runtime.lock().await;
            runtime.mark_processed(&item.message_id, &item.timestamp);
            runtime.flush()?;
            continue;
        }

        if let Some(store) = delivery_store.as_ref() {
            if let Err(err) = store.reconcile_sink_targets(&sinks, &wake_session_key) {
                hard_failure_hooks
                    .fire(HardFailureEvent::new(
                        "db_sink_target_reconcile_failed",
                        "storage",
                        None,
                        err.to_string(),
                        serde_json::json!({}),
                    ))
                    .await;
                return Err(err);
            }
        }

        let candidate_sinks = routing_entries_for_sinks(
            &sinks,
            item.wake_session_key_override.as_deref(),
            &wake_session_key,
            delivery_store.as_ref(),
        );

        let db_sink = sinks
            .iter()
            .find(|sink| sink.enabled && sink.kind == SinkKind::Db);
        let wake_sinks: Vec<&SinkConfig> = sinks
            .iter()
            .filter(|sink| sink.enabled && sink.kind == SinkKind::AgentSessionWake)
            .collect();
        let mut selected_sinks = Vec::new();
        if let Some(sink) = db_sink {
            selected_sinks.push(routing_entry_for_sink(
                sink,
                item.wake_session_key_override.as_deref(),
                &wake_session_key,
                delivery_store.as_ref(),
            ));
        }
        selected_sinks.extend(wake_sinks.iter().map(|sink| {
            routing_entry_for_sink(
                sink,
                item.wake_session_key_override.as_deref(),
                &wake_session_key,
                delivery_store.as_ref(),
            )
        }));
        let routing = RoutingSnapshot {
            candidate_sinks: &candidate_sinks,
            selected_sinks: &selected_sinks,
        };

        if let Some(sink) = db_sink {
            if let Some(store) = delivery_store.as_ref() {
                if let Err(err) =
                    store.record_db_sink_delivery(&item, &attention_result, sink, &routing)
                {
                    hard_failure_hooks
                        .fire(storage_hard_failure_event(
                            "db_sink_delivery_failed",
                            &item,
                            &err,
                        ))
                        .await;
                    return Err(err);
                }
            }
        }

        for sink in wake_sinks {
            let rendered = render_inbound_wake(&item, &attention_result);
            let effective_session_key = sink
                .destination
                .as_deref()
                .or(item.wake_session_key_override.as_deref())
                .unwrap_or(&wake_session_key);
            let delivery_ticket = if let Some(store) = delivery_store.as_ref() {
                let snapshot = SinkSnapshot {
                    sink_key: &sink.key,
                    sink_kind: SinkKind::AgentSessionWake,
                    destination: effective_session_key,
                    config_json: serde_json::json!({
                        "session_key": effective_session_key,
                    })
                    .to_string(),
                };
                match store.queue_wake_sink_delivery(&item, &attention_result, &snapshot, &routing)
                {
                    Ok(ticket) => Some(ticket),
                    Err(err) => {
                        hard_failure_hooks
                            .fire(storage_hard_failure_event(
                                "db_wake_sink_queue_failed",
                                &item,
                                &err,
                            ))
                            .await;
                        return Err(err);
                    }
                }
            } else {
                None
            };
            if delivery_ticket
                .as_ref()
                .is_some_and(|ticket| ticket.already_delivered)
            {
                info!(
                    component = "wake_router",
                    event = "wake_already_delivered",
                    message_id = %item.message_id,
                    server = %item.server,
                    session_key = %effective_session_key,
                    sink_key = %sink.key,
                    "wake already recorded as delivered; skipping duplicate send"
                );
                continue;
            }
            let mut backoff_ms = retry.base_ms;
            loop {
                if let Some(store) = delivery_store.as_ref() {
                    if let Some(ticket) = delivery_ticket.as_ref() {
                        store.mark_delivery_attempted(ticket.delivery_id)?;
                    }
                }
                let send_future = gateway.send_chat(
                    effective_session_key.to_string(),
                    rendered.clone(),
                    format!("{}:{}:{}", item.server_key, item.message_id, sink.key),
                );
                let result = if let Some(deadline) = shutdown_deadline {
                    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                    if remaining.is_zero() {
                        return Ok(());
                    }
                    match tokio::time::timeout(remaining, send_future).await {
                        Ok(result) => result,
                        Err(_) => return Ok(()),
                    }
                } else {
                    send_future.await
                };

                match result {
                    Ok(()) => {
                        if let Some(store) = delivery_store.as_ref() {
                            if let Some(ticket) = delivery_ticket.as_ref() {
                                store.mark_delivery_delivered(ticket.delivery_id)?;
                            }
                        }
                        info!(
                            component = "wake_router",
                            event = "wake_sent",
                            message_id = %item.message_id,
                            server = %item.server,
                            session_key = %effective_session_key,
                            sink_key = %sink.key,
                            receptor_matches = attention_result.matches.iter()
                                .filter(|m| m.above_threshold)
                                .count(),
                            "wake sent"
                        );
                        break;
                    }
                    Err(err) => {
                        if let Some(store) = delivery_store.as_ref() {
                            if let Some(ticket) = delivery_ticket.as_ref() {
                                store.mark_delivery_failed(ticket.delivery_id, &err.to_string())?;
                            }
                        }
                        warn!(component = "wake_router", event = "wake_failed", message_id = %item.message_id, server = %item.server, sink_key = %sink.key, error = %err, "wake failed; retrying");
                        hard_failure_hooks
                            .fire(HardFailureEvent::new(
                                "wake_delivery_failed",
                                "wake_router",
                                Some(format!("{}:{}", item.server, sink.key)),
                                err.to_string(),
                                serde_json::json!({
                                    "server": item.server,
                                    "server_key": item.server_key,
                                    "message_id": item.message_id,
                                    "sink_key": sink.key,
                                }),
                            ))
                            .await;
                        let delay = Duration::from_millis(jitter(backoff_ms, retry.jitter_ratio));
                        if let Some(deadline) = shutdown_deadline {
                            let remaining =
                                deadline.saturating_duration_since(tokio::time::Instant::now());
                            if remaining.is_zero() {
                                return Ok(());
                            }
                            tokio::time::sleep(delay.min(remaining)).await;
                        } else {
                            tokio::time::sleep(delay).await;
                        }
                        backoff_ms = (backoff_ms.saturating_mul(2)).min(retry.max_ms);
                    }
                }
            }
        }

        let mut runtime = item.runtime.lock().await;
        runtime.mark_processed(&item.message_id, &item.timestamp);
        runtime.flush()?;
    }
}

fn storage_hard_failure_event(
    kind: &str,
    item: &WakeEnvelope,
    err: &anyhow::Error,
) -> HardFailureEvent {
    HardFailureEvent::new(
        kind,
        "storage",
        Some(item.server.clone()),
        err.to_string(),
        serde_json::json!({
            "server": item.server,
            "server_key": item.server_key,
            "message_id": item.message_id,
        }),
    )
}

fn routing_entries_for_sinks(
    sinks: &[SinkConfig],
    wake_session_key_override: Option<&str>,
    default_wake_session_key: &str,
    delivery_store: Option<&DeliveryStore>,
) -> Vec<SinkRoutingEntry> {
    sinks
        .iter()
        .filter(|sink| sink.enabled)
        .map(|sink| {
            routing_entry_for_sink(
                sink,
                wake_session_key_override,
                default_wake_session_key,
                delivery_store,
            )
        })
        .collect()
}

fn enabled_sink_count(sinks: &[SinkConfig]) -> usize {
    sinks.iter().filter(|sink| sink.enabled).count()
}

fn routing_entry_for_sink(
    sink: &SinkConfig,
    wake_session_key_override: Option<&str>,
    default_wake_session_key: &str,
    delivery_store: Option<&DeliveryStore>,
) -> SinkRoutingEntry {
    let destination = sink.destination.clone().unwrap_or_else(|| match sink.kind {
        SinkKind::Db => delivery_store
            .map(|store| store.database_path().display().to_string())
            .unwrap_or_else(|| "db".to_string()),
        SinkKind::AgentSessionWake => wake_session_key_override
            .unwrap_or(default_wake_session_key)
            .to_string(),
    });
    SinkRoutingEntry {
        sink_key: sink.key.clone(),
        sink_kind: sink.kind.clone(),
        destination,
    }
}

fn render_inbound_wake(message: &WakeEnvelope, attention: &AttentionResult) -> String {
    let from = if message.author_name.trim().is_empty() {
        message.author_id.as_str()
    } else {
        message.author_name.as_str()
    };

    let mut lines = vec![
        "[Subspace inbound]".to_string(),
        format!("Server: {}", message.server),
        format!("ServerKey: {}", message.server_key),
        format!("From: {}", from),
        format!("MessageId: {}", message.message_id),
    ];

    // Add receptor match annotations if any
    if let Some(annotation) = format_attention_annotation(attention) {
        lines.push(annotation);
    }

    lines.push(String::new()); // blank line before text
    lines.push(message.text.clone());

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnect_cooldown_counts_as_healthy_readiness_state() {
        let status = DaemonStatus {
            gateway_state: "live".to_string(),
            wake_session_key: "agent:target:main".to_string(),
            servers: BTreeMap::from([(
                "https://subspace.example".to_string(),
                ServerHealth {
                    server: "https://subspace.example".to_string(),
                    server_key: "https_subspace_example_443".to_string(),
                    subspace_state: "reconnect_cooldown".to_string(),
                    session_expires_at: Some("2099-04-17T12:05:00Z".to_string()),
                    veto_enforcement_state: "not_configured".to_string(),
                    attention: AttentionHealth::not_configured(),
                    consecutive_failures: Some(10),
                    cooldown_ms: Some(300_000),
                    next_attempt_at: Some("2026-04-17T12:05:00Z".to_string()),
                    last_error_kind: Some("connection_refused".to_string()),
                },
            )]),
        };

        assert!(status.is_healthy());
    }

    #[test]
    fn missing_session_expiry_is_unhealthy() {
        let status = DaemonStatus {
            gateway_state: "live".to_string(),
            wake_session_key: "agent:target:main".to_string(),
            servers: BTreeMap::from([(
                "https://subspace.example".to_string(),
                ServerHealth {
                    server: "https://subspace.example".to_string(),
                    server_key: "https_subspace_example_443".to_string(),
                    subspace_state: "live".to_string(),
                    session_expires_at: None,
                    veto_enforcement_state: "not_configured".to_string(),
                    attention: AttentionHealth::not_configured(),
                    consecutive_failures: None,
                    cooldown_ms: None,
                    next_attempt_at: None,
                    last_error_kind: None,
                },
            )]),
        };

        assert!(!status.is_healthy());
    }

    #[test]
    fn server_status_preserves_veto_enforcement_state_across_connection_updates() {
        let mut status = DaemonStatus {
            gateway_state: "live".to_string(),
            wake_session_key: "agent:target:main".to_string(),
            servers: BTreeMap::new(),
        };

        status.set_server_veto_enforcement_state(
            "https://subspace.example",
            "https_subspace_example_443",
            "unavailable",
        );
        status.set_server_state(
            "https://subspace.example",
            "https_subspace_example_443",
            "live",
        );
        assert_eq!(
            status
                .servers
                .get("https://subspace.example")
                .unwrap()
                .veto_enforcement_state,
            "unavailable"
        );

        status.set_server_reconnect_cooldown(
            "https://subspace.example",
            "https_subspace_example_443",
            10,
            300_000,
            "2026-04-17T12:05:00Z".to_string(),
            Some("connection_refused".to_string()),
        );
        assert_eq!(
            status
                .servers
                .get("https://subspace.example")
                .unwrap()
                .veto_enforcement_state,
            "unavailable"
        );
    }

    #[test]
    fn enabled_sink_count_treats_empty_and_disabled_sinks_as_no_delivery_targets() {
        assert_eq!(enabled_sink_count(&[]), 0);
        assert_eq!(
            enabled_sink_count(&[SinkConfig {
                key: "db".to_string(),
                kind: SinkKind::Db,
                enabled: false,
                destination: None,
            }]),
            0
        );
        assert_eq!(
            enabled_sink_count(&[SinkConfig {
                key: "db".to_string(),
                kind: SinkKind::Db,
                enabled: true,
                destination: None,
            }]),
            1
        );
    }
}
