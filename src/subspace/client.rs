use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{SinkExt, StreamExt};
use reqwest::Client;
use serde_json::{Value, json};
use tokio::sync::{Mutex, RwLock, broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{info, warn};
use uuid::Uuid;

use crate::config::{RetryConfig, ServerConfig};
use crate::retry::jitter;
use crate::runtime_store::RuntimeStore;
use crate::subspace::auth::acquire_session_token;
use crate::subspace::identity::SubspaceSessionRecord;
use crate::supervisor::{DaemonStatus, WakeEnvelope};

#[derive(Debug, Clone)]
pub struct OutboundSendResult {
    pub server: String,
    pub subspace_message_id: Option<String>,
    pub idempotency_key: String,
}

pub enum ServerCommand {
    SendMessage {
        text: String,
        idempotency_key: Option<String>,
        reply: oneshot::Sender<Result<OutboundSendResult>>,
    },
}

#[derive(Clone)]
pub struct ServerHandle {
    tx: mpsc::Sender<ServerCommand>,
}

impl ServerHandle {
    pub async fn send_message(
        &self,
        text: String,
        idempotency_key: Option<String>,
    ) -> Result<OutboundSendResult> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(ServerCommand::SendMessage {
                text,
                idempotency_key,
                reply: reply_tx,
            })
            .await
            .map_err(|_| anyhow!("subspace server command channel closed"))?;
        reply_rx
            .await
            .map_err(|_| anyhow!("subspace server reply channel closed"))?
    }
}

struct PendingSend {
    reply: oneshot::Sender<Result<OutboundSendResult>>,
    idempotency_key: String,
}

pub async fn start_server_manager(
    server: ServerConfig,
    retry: RetryConfig,
    status: Arc<RwLock<DaemonStatus>>,
    wake_tx: mpsc::Sender<WakeEnvelope>,
    shutdown: broadcast::Receiver<()>,
) -> Result<(ServerHandle, JoinHandle<Result<()>>)> {
    let (tx, rx) = mpsc::channel(128);
    let task = tokio::spawn(run_server_manager(
        server, retry, status, wake_tx, rx, shutdown,
    ));
    Ok((ServerHandle { tx }, task))
}

async fn run_server_manager(
    server: ServerConfig,
    retry: RetryConfig,
    status: Arc<RwLock<DaemonStatus>>,
    wake_tx: mpsc::Sender<WakeEnvelope>,
    mut cmd_rx: mpsc::Receiver<ServerCommand>,
    mut shutdown: broadcast::Receiver<()>,
) -> Result<()> {
    let http = Client::builder().build()?;
    let runtime = Arc::new(Mutex::new(RuntimeStore::load(server.runtime_path.clone())?));
    let mut backoff = retry.base_ms;

    loop {
        let mut session = match SubspaceSessionRecord::load(&server.session_path)? {
            Some(session) => session,
            None => {
                update_server_state(&status, &server, "subspace_auth_required").await;
                tokio::select! {
                    _ = shutdown.recv() => return Ok(()),
                    _ = tokio::time::sleep(Duration::from_millis(jitter(backoff, retry.jitter_ratio))) => {}
                }
                backoff = (backoff.saturating_mul(2)).min(retry.max_ms);
                continue;
            }
        };

        if session.session_token.is_none() {
            update_server_state(&status, &server, "authenticating").await;
            info!(component = "subspace", event = "subspace_connecting", server = %server.base_url, server_key = %server.server_key, phase = "auth", "authenticating to subspace");
            match acquire_session_token(&http, &server.base_url, &session).await {
                Ok(token) => {
                    session.update_session_token(token);
                    session.persist(&server.session_path)?;
                }
                Err(err) => {
                    update_server_state(&status, &server, "subspace_auth_required").await;
                    warn!(component = "subspace", event = "daemon_degraded", server = %server.base_url, server_key = %server.server_key, error = %err, "subspace auth failed");
                    tokio::select! {
                        _ = shutdown.recv() => return Ok(()),
                        _ = tokio::time::sleep(Duration::from_millis(jitter(backoff, retry.jitter_ratio))) => {}
                    }
                    backoff = (backoff.saturating_mul(2)).min(retry.max_ms);
                    continue;
                }
            }
        }

        match connect_once(
            &server,
            &retry,
            &status,
            &runtime,
            &mut session,
            &mut cmd_rx,
            &wake_tx,
            &mut shutdown,
        )
        .await
        {
            Ok(()) => return Ok(()),
            Err(err) => {
                update_server_state(&status, &server, "reconnecting").await;
                warn!(component = "subspace", event = "daemon_degraded", server = %server.base_url, server_key = %server.server_key, error = %err, "subspace disconnected");
                tokio::select! {
                    _ = shutdown.recv() => return Ok(()),
                    _ = tokio::time::sleep(Duration::from_millis(jitter(backoff, retry.jitter_ratio))) => {}
                }
                backoff = (backoff.saturating_mul(2)).min(retry.max_ms);
            }
        }
    }
}

async fn connect_once(
    server: &ServerConfig,
    _retry: &RetryConfig,
    status: &Arc<RwLock<DaemonStatus>>,
    runtime: &Arc<Mutex<RuntimeStore>>,
    session: &mut SubspaceSessionRecord,
    cmd_rx: &mut mpsc::Receiver<ServerCommand>,
    wake_tx: &mpsc::Sender<WakeEnvelope>,
    shutdown: &mut broadcast::Receiver<()>,
) -> Result<()> {
    update_server_state(status, server, "connecting").await;
    let (ws, _) = connect_async(&server.websocket_url)
        .await
        .context("subspace websocket connect failed")?;
    let (mut write, mut read) = ws.split();

    let join_ref = Uuid::new_v4().to_string();
    write
        .send(Message::Text(
            serde_json::to_string(&json!({
                "topic": "firehose",
                "event": "phx_join",
                "payload": {
                    "agent_id": session.public_key,
                    "session_token": session.session_token.clone().unwrap_or_default(),
                },
                "ref": join_ref,
            }))?
            .into(),
        ))
        .await?;

    loop {
        tokio::select! {
            _ = shutdown.recv() => {
                let _ = write.send(Message::Close(None)).await;
                return Ok(());
            }
            next = read.next() => {
                let next = next.ok_or_else(|| anyhow!("subspace closed during join"))??;
                let text = match ws_text(next)? {
                    WsFrame::Text(text) => text,
                    WsFrame::Ignore => continue,
                    WsFrame::Closed { code, reason } => {
                        bail!("subspace websocket closed during join{}{}",
                            code.map(|code| format!(" code={code}")).unwrap_or_default(),
                            if reason.is_empty() { String::new() } else { format!(" reason={reason:?}") }
                        );
                    }
                };
                let parsed: Value = serde_json::from_str(&text)?;
                if parsed.get("event") == Some(&Value::String("phx_reply".to_string()))
                    && parsed.get("ref") == Some(&Value::String(join_ref.clone()))
                {
                    let status_value = parsed
                        .get("payload")
                        .and_then(|payload| payload.get("status"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if status_value == "ok" {
                        break;
                    }
                    let error_value = parsed
                        .get("payload")
                        .and_then(|payload| payload.get("response"))
                        .and_then(|response| response.get("error"))
                        .and_then(Value::as_str)
                        .unwrap_or("join_failed");
                    if matches!(error_value, "TOKEN_INVALID" | "TOKEN_REVOKED") {
                        session.clear_session_token();
                        session.persist(&server.session_path)?;
                    }
                    bail!("subspace join failed: {error_value}");
                }
            }
        }
    }

    update_server_state(status, server, "live").await;
    info!(component = "subspace", event = "subspace_live", server = %server.base_url, server_key = %server.server_key, "subspace connection is live");
    let mut heartbeat = tokio::time::interval(Duration::from_secs(30));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut pending: HashMap<String, PendingSend> = HashMap::new();

    loop {
        tokio::select! {
            _ = shutdown.recv() => {
                let _ = write.send(Message::Close(None)).await;
                return Ok(());
            }
            _ = heartbeat.tick() => {
                write.send(Message::Text(
                    serde_json::to_string(&json!({
                        "topic": "phoenix",
                        "event": "heartbeat",
                        "payload": {},
                        "ref": Uuid::new_v4().to_string(),
                    }))?.into(),
                )).await?;
            }
            maybe_cmd = cmd_rx.recv() => {
                let Some(cmd) = maybe_cmd else {
                    let _ = write.send(Message::Close(None)).await;
                    return Ok(());
                };
                match cmd {
                    ServerCommand::SendMessage { text, idempotency_key, reply } => {
                        let ref_id = Uuid::new_v4().to_string();
                        let idem = idempotency_key.unwrap_or_else(|| Uuid::new_v4().to_string());
                        write.send(Message::Text(
                            serde_json::to_string(&json!({
                                "topic": "firehose",
                                "event": "post_message",
                                "payload": { "text": text, "idempotency_key": idem },
                                "ref": ref_id,
                            }))?.into(),
                        )).await?;
                        pending.insert(ref_id, PendingSend { reply, idempotency_key: idem });
                    }
                }
            }
            incoming = read.next() => {
                let Some(incoming) = incoming else {
                    bail!("subspace websocket closed");
                };
                let text = match ws_text(incoming?)? {
                    WsFrame::Text(text) => text,
                    WsFrame::Ignore => continue,
                    WsFrame::Closed { code, reason } => {
                        bail!(
                            "subspace websocket closed{}{}",
                            code.map(|code| format!(" code={code}")).unwrap_or_default(),
                            if reason.is_empty() { String::new() } else { format!(" reason={reason:?}") }
                        );
                    }
                };
                let parsed: Value = match serde_json::from_str(&text) {
                    Ok(parsed) => parsed,
                    Err(err) => {
                        warn!(component = "subspace", event = "subspace_inbound_malformed", server = %server.base_url, server_key = %server.server_key, error = %err, "dropping malformed subspace frame");
                        continue;
                    }
                };
                if parsed.get("topic") == Some(&Value::String("phoenix".to_string()))
                    && parsed.get("event") == Some(&Value::String("heartbeat".to_string()))
                {
                    continue;
                }
                if parsed.get("topic") != Some(&Value::String("firehose".to_string())) {
                    continue;
                }
                match parsed.get("event").and_then(Value::as_str).unwrap_or("") {
                    "new_message" => {
                        let payload = parsed.get("payload").cloned().unwrap_or_else(|| json!({}));
                        let author_id = payload.get("agentId").and_then(Value::as_str).unwrap_or("").to_string();
                        if author_id == session.public_key {
                            continue;
                        }
                        let message_id = payload.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                        let text = payload.get("text").and_then(Value::as_str).unwrap_or("").to_string();
                        let timestamp = payload.get("ts").and_then(Value::as_str).unwrap_or("").to_string();
                        if message_id.is_empty() || timestamp.is_empty() {
                            warn!(component = "subspace", event = "subspace_inbound_malformed", server = %server.base_url, server_key = %server.server_key, "dropping malformed new_message payload");
                            continue;
                        }
                        let should_enqueue = {
                            let mut runtime = runtime.lock().await;
                            runtime.should_enqueue(&message_id)
                        };
                        if !should_enqueue {
                            continue;
                        }
                        let envelope = WakeEnvelope {
                            server: server.base_url.clone(),
                            server_key: server.server_key.clone(),
                            message_id: message_id.clone(),
                            timestamp,
                            author_id,
                            author_name: payload
                                .get("agentName")
                                .and_then(Value::as_str)
                                .or_else(|| payload.get("agentId").and_then(Value::as_str))
                                .unwrap_or("")
                                .to_string(),
                            text,
                            runtime: runtime.clone(),
                        };
                        match wake_tx.try_send(envelope) {
                            Ok(()) => {
                                info!(component = "subspace", event = "subspace_inbound_received", server = %server.base_url, server_key = %server.server_key, message_id = %message_id, "received inbound subspace message");
                            }
                            Err(tokio::sync::mpsc::error::TrySendError::Full(envelope)) => {
                                let mut runtime = envelope.runtime.lock().await;
                                runtime.mark_failed(&envelope.message_id);
                                warn!(component = "subspace", event = "wake_queue_full", server = %server.base_url, server_key = %server.server_key, message_id = %message_id, "wake queue full; forcing reconnect");
                                bail!("wake queue full");
                            }
                            Err(tokio::sync::mpsc::error::TrySendError::Closed(envelope)) => {
                                let mut runtime = envelope.runtime.lock().await;
                                runtime.mark_failed(&envelope.message_id);
                                bail!("wake queue closed");
                            }
                        }
                    }
                    "phx_reply" => {
                        if let Some(reference) = parsed.get("ref").and_then(Value::as_str) {
                            if let Some(pending_send) = pending.remove(reference) {
                                let payload = parsed.get("payload").cloned().unwrap_or_else(|| json!({}));
                                let status_value = payload.get("status").and_then(Value::as_str).unwrap_or("");
                                if status_value == "ok" {
                                    let subspace_message_id = payload
                                        .get("response")
                                        .and_then(|response| response.get("id"))
                                        .or_else(|| payload.get("response").and_then(|response| response.get("messageId")))
                                        .and_then(Value::as_str)
                                        .map(ToOwned::to_owned);
                                    let _ = pending_send.reply.send(Ok(OutboundSendResult {
                                        server: server.base_url.clone(),
                                        subspace_message_id,
                                        idempotency_key: pending_send.idempotency_key,
                                    }));
                                } else {
                                    let error_value = payload
                                        .get("response")
                                        .and_then(|response| response.get("error"))
                                        .and_then(Value::as_str)
                                        .unwrap_or("send_failed");
                                    if matches!(error_value, "TOKEN_INVALID" | "TOKEN_REVOKED") {
                                        session.clear_session_token();
                                        session.persist(&server.session_path)?;
                                    }
                                    let _ = pending_send.reply.send(Err(anyhow!(error_value.to_string())));
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

async fn update_server_state(
    status: &Arc<RwLock<DaemonStatus>>,
    server: &ServerConfig,
    value: &str,
) {
    status
        .write()
        .await
        .set_server_state(&server.base_url, &server.server_key, value.to_string());
}

enum WsFrame {
    Text(String),
    Ignore,
    Closed { code: Option<u16>, reason: String },
}

fn ws_text(message: Message) -> Result<WsFrame> {
    match message {
        Message::Text(text) => Ok(WsFrame::Text(text.to_string())),
        Message::Binary(bytes) => Ok(WsFrame::Text(
            String::from_utf8(bytes.to_vec()).context("non-utf8 websocket message")?,
        )),
        Message::Close(close) => Ok(WsFrame::Closed {
            code: close.as_ref().map(|frame| u16::from(frame.code)),
            reason: close
                .as_ref()
                .map(|frame| frame.reason.to_string())
                .unwrap_or_default(),
        }),
        Message::Ping(_) | Message::Pong(_) => Ok(WsFrame::Ignore),
        Message::Frame(_) => Ok(WsFrame::Ignore),
    }
}
