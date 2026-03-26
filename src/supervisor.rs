use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use serde::Serialize;
use tokio::sync::{Mutex, RwLock, broadcast, mpsc};
use tracing::{error, info, warn};

use crate::attention::{
    AttentionLayer, AttentionResult, MessageEmbedding, format_attention_annotation,
};
use crate::config::{Config, RetryConfig};
use crate::gateway::client::{GatewayClientHandle, start_gateway_client};
use crate::ipc::{SendRouter, run_ipc_server};
use crate::launchd::render_launchd_plist;
use crate::retry::jitter;
use crate::runtime_store::RuntimeStore;
use crate::setup::{LiveSetupState, SharedServerHandles, SharedServerTasks, spawn_server_manager};
use crate::state_lock::StateLock;

#[derive(Debug, Clone, Serialize)]
pub struct ServerHealth {
    pub server: String,
    pub server_key: String,
    pub subspace_state: String,
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
            && self.servers.values().all(|server| {
                matches!(
                    server.subspace_state.as_str(),
                    "live" | "reconnecting" | "subspace_auth_required"
                )
            })
    }

    pub fn set_gateway_state(&mut self, value: impl Into<String>) {
        self.gateway_state = value.into();
    }

    pub fn set_server_state(&mut self, base_url: &str, server_key: &str, value: impl Into<String>) {
        self.servers.insert(
            base_url.to_string(),
            ServerHealth {
                server: base_url.to_string(),
                server_key: server_key.to_string(),
                subspace_state: value.into(),
            },
        );
    }

    pub fn server_state(&self, base_url: &str) -> Option<String> {
        self.servers
            .get(base_url)
            .map(|server| server.subspace_state.clone())
    }

    pub fn servers_snapshot(&self) -> Vec<ServerHealth> {
        self.servers.values().cloned().collect()
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
    let (shutdown_tx, _) = broadcast::channel(4);

    let (gateway, gateway_task) =
        start_gateway_client(config.clone(), status.clone(), shutdown_tx.subscribe()).await?;

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
                return handle_task_exit("gateway", result).await;
            }
            result = &mut wake_task => {
                let _ = shutdown_tx.send(());
                let _ = tokio::fs::remove_file(&config.paths.socket_path).await;
                return handle_task_exit("wake_queue", result).await;
            }
            result = &mut ipc_task => {
                let _ = shutdown_tx.send(());
                let _ = tokio::fs::remove_file(&config.paths.socket_path).await;
                return handle_task_exit("ipc", result).await;
            }
            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                if let Some(result) = check_server_exit(&server_tasks).await {
                    let _ = shutdown_tx.send(());
                    let _ = tokio::fs::remove_file(&config.paths.socket_path).await;
                    return result;
                }
            }
        }
    }
}

async fn check_server_exit(server_tasks: &SharedServerTasks) -> Option<Result<()>> {
    let mut tasks = server_tasks.lock().await;
    for (server_key, task) in tasks.iter_mut() {
        if task.is_finished() {
            let result = task.await;
            return Some(handle_task_exit(&format!("subspace:{server_key}"), result).await);
        }
    }
    None
}

async fn handle_task_exit(
    task_name: &str,
    result: std::result::Result<Result<()>, tokio::task::JoinError>,
) -> Result<()> {
    match result {
        Ok(Ok(())) => Err(anyhow!("{task_name} exited unexpectedly")),
        Ok(Err(err)) => {
            error!(component = "supervisor", event = "daemon_degraded", task = task_name, error = %err, "critical task failed");
            Err(err)
        }
        Err(err) => {
            error!(component = "supervisor", event = "daemon_degraded", task = task_name, error = %err, "critical task panicked");
            Err(anyhow!(err))
        }
    }
}

async fn process_wake_queue(
    mut rx: mpsc::Receiver<WakeEnvelope>,
    gateway: GatewayClientHandle,
    wake_session_key: String,
    retry: RetryConfig,
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

        if !attention_result.deliver {
            // Message filtered out by attention layer
            info!(
                component = "wake_router",
                event = "message_filtered",
                message_id = %item.message_id,
                server = %item.server,
                top_score = attention_result.matches.first().map(|m| m.score),
                "message filtered by attention layer"
            );
            // Mark as processed so we don't retry
            let mut runtime = item.runtime.lock().await;
            runtime.mark_processed(&item.message_id, &item.timestamp);
            runtime.flush()?;
            continue;
        }

        let rendered = render_inbound_wake(&item, &attention_result);
        let effective_session_key = item
            .wake_session_key_override
            .as_deref()
            .unwrap_or(&wake_session_key);
        let mut backoff_ms = retry.base_ms;
        loop {
            let send_future = gateway.send_chat(
                effective_session_key.to_string(),
                rendered.clone(),
                format!("{}:{}", item.server_key, item.message_id),
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
                    let mut runtime = item.runtime.lock().await;
                    runtime.mark_processed(&item.message_id, &item.timestamp);
                    runtime.flush()?;
                    info!(
                        component = "wake_router",
                        event = "wake_sent",
                        message_id = %item.message_id,
                        server = %item.server,
                        session_key = %effective_session_key,
                        receptor_matches = attention_result.matches.iter()
                            .filter(|m| m.above_threshold)
                            .count(),
                        "wake sent"
                    );
                    break;
                }
                Err(err) => {
                    warn!(component = "wake_router", event = "wake_failed", message_id = %item.message_id, server = %item.server, error = %err, "wake failed; retrying");
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
