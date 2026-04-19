use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{SinkExt, StreamExt};
use reqwest::Client;
use serde_json::{Value, json};
use time::OffsetDateTime;
use tokio::sync::{Mutex, RwLock, broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{info, warn};
use uuid::Uuid;

use crate::attention::{MessageEmbedding, OutboundEmbeddingRequest, compose_outbound_embeddings};
use crate::config::{ReplayConfig, RetryConfig, ServerConfig};
use crate::retry::jitter;
use crate::runtime_store::RuntimeStore;
use crate::subspace::auth::{reauth_identity, reauth_legacy_identity};
use crate::subspace::identity::{
    LegacySubspaceSessionRecord, LoadedSessionRecord, NamedIdentityRecord, SubspaceSessionRecord,
    load_session_record,
};
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
        embedding_request: OutboundEmbeddingRequest,
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
        embedding_request: OutboundEmbeddingRequest,
    ) -> Result<OutboundSendResult> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(ServerCommand::SendMessage {
                text,
                idempotency_key,
                embedding_request,
                reply: reply_tx,
            })
            .await
            .map_err(|_| anyhow!("subspace server command channel closed"))?;
        reply_rx
            .await
            .map_err(|_| anyhow!("subspace server reply channel closed"))?
    }
}

#[cfg(test)]
pub(crate) fn test_handle() -> ServerHandle {
    let (tx, _rx) = mpsc::channel(1);
    ServerHandle { tx }
}

struct PendingSend {
    reply: oneshot::Sender<Result<OutboundSendResult>>,
    idempotency_key: String,
}

enum ActiveSession {
    Current {
        session: SubspaceSessionRecord,
        identity: NamedIdentityRecord,
    },
    Legacy(LegacySubspaceSessionRecord),
}

impl ActiveSession {
    fn agent_id(&self) -> &str {
        match self {
            Self::Current { session, .. } => &session.agent_id,
            Self::Legacy(session) => &session.agent_id,
        }
    }

    fn session_token(&self) -> Option<&str> {
        match self {
            Self::Current { session, .. } => session.session_token.as_deref(),
            Self::Legacy(session) => session.session_token.as_deref(),
        }
    }

    fn clear_session_token(&mut self) {
        match self {
            Self::Current { session, .. } => session.clear_session_token(),
            Self::Legacy(session) => session.clear_session_token(),
        }
    }

    fn update_session_token(&mut self, token: String) {
        match self {
            Self::Current { session, .. } => session.update_session_token(token),
            Self::Legacy(session) => session.update_session_token(token),
        }
    }

    fn persist(&self, path: &std::path::Path) -> Result<()> {
        match self {
            Self::Current { session, .. } => session.persist(path),
            Self::Legacy(session) => session.persist(path),
        }
    }

    async fn reauth(&self, client: &Client, base_url: &str) -> Result<String> {
        match self {
            Self::Current { session, identity } => {
                reauth_identity(client, base_url, session, identity).await
            }
            Self::Legacy(session) => reauth_legacy_identity(client, base_url, session).await,
        }
    }
}

pub async fn start_server_manager(
    server: ServerConfig,
    retry: RetryConfig,
    replay: ReplayConfig,
    identities_dir: PathBuf,
    generated_embedding_clients: BTreeMap<
        String,
        crate::attention::embedding_plugin::EmbeddingPluginClient,
    >,
    attention: Arc<crate::attention::AttentionLayer>,
    status: Arc<RwLock<DaemonStatus>>,
    wake_tx: mpsc::Sender<WakeEnvelope>,
    shutdown: broadcast::Receiver<()>,
) -> Result<(ServerHandle, JoinHandle<Result<()>>)> {
    let (tx, rx) = mpsc::channel(128);
    let task = tokio::spawn(run_server_manager(
        server,
        retry,
        replay,
        identities_dir,
        generated_embedding_clients,
        attention,
        status,
        wake_tx,
        rx,
        shutdown,
    ));
    Ok((ServerHandle { tx }, task))
}

async fn run_server_manager(
    server: ServerConfig,
    retry: RetryConfig,
    replay: ReplayConfig,
    identities_dir: PathBuf,
    generated_embedding_clients: BTreeMap<
        String,
        crate::attention::embedding_plugin::EmbeddingPluginClient,
    >,
    attention: Arc<crate::attention::AttentionLayer>,
    status: Arc<RwLock<DaemonStatus>>,
    wake_tx: mpsc::Sender<WakeEnvelope>,
    mut cmd_rx: mpsc::Receiver<ServerCommand>,
    mut shutdown: broadcast::Receiver<()>,
) -> Result<()> {
    let http = Client::builder().build()?;
    let runtime = Arc::new(Mutex::new(RuntimeStore::load(
        server.runtime_path.clone(),
        replay.dedupe_window_size,
        replay.discard_before_ts.clone(),
    )?));
    let mut backoff = retry.base_ms;

    loop {
        if wait_for_reconnect_cooldown_if_needed(&status, &server, &runtime, &mut shutdown).await {
            return Ok(());
        }

        let mut session = match load_session_record(&server.session_path)? {
            Some(LoadedSessionRecord::Current(session)) => {
                let identity_path = identities_dir.join(format!("{}.json", session.identity));
                let identity = NamedIdentityRecord::load(&identity_path)?
                    .ok_or_else(|| anyhow!("missing identity file: {}", identity_path.display()))?;
                identity.ensure_matches_agent_id(&session.agent_id)?;
                ActiveSession::Current { session, identity }
            }
            Some(LoadedSessionRecord::Legacy(session)) => {
                warn!(
                    component = "subspace",
                    event = "legacy_session_runtime_fallback",
                    server = %server.base_url,
                    server_key = %server.server_key,
                    "using legacy inline-keypair session at runtime; migrate via `subspace-daemon setup <url> --identity <name>`"
                );
                ActiveSession::Legacy(session)
            }
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

        if session.session_token().is_none() {
            update_server_state(&status, &server, "authenticating").await;
            info!(component = "subspace", event = "subspace_connecting", server = %server.base_url, server_key = %server.server_key, phase = "auth", "authenticating to subspace");
            match session.reauth(&http, &server.base_url).await {
                Ok(token) => {
                    session.update_session_token(token);
                    session.persist(&server.session_path)?;
                }
                Err(err) => {
                    update_server_state(&status, &server, "subspace_auth_required").await;
                    let error_kind = reconnect_error_kind(&err);
                    warn!(component = "subspace", event = "daemon_degraded", server = %server.base_url, server_key = %server.server_key, error_kind = %error_kind, "subspace auth failed");
                    if record_reconnect_failure(&status, &server, &runtime, &retry, error_kind)
                        .await?
                    {
                        continue;
                    }
                    tokio::select! {
                        _ = shutdown.recv() => return Ok(()),
                        _ = tokio::time::sleep(Duration::from_millis(jitter(backoff, retry.jitter_ratio))) => {}
                    }
                    backoff = (backoff.saturating_mul(2)).min(retry.max_ms);
                    continue;
                }
            }
        }

        let mut reached_live = false;
        match connect_once(
            &server,
            &retry,
            &status,
            &runtime,
            &mut session,
            &generated_embedding_clients,
            &attention,
            &mut cmd_rx,
            &wake_tx,
            &mut shutdown,
            &mut reached_live,
        )
        .await
        {
            Ok(()) => return Ok(()),
            Err(err) => {
                if reached_live {
                    backoff = retry.base_ms;
                }
                update_server_state(&status, &server, "reconnecting").await;
                let error_kind = reconnect_error_kind(&err);
                warn!(component = "subspace", event = "daemon_degraded", server = %server.base_url, server_key = %server.server_key, error_kind = %error_kind, "subspace disconnected");
                if record_reconnect_failure(&status, &server, &runtime, &retry, error_kind).await? {
                    continue;
                }
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
    session: &mut ActiveSession,
    generated_embedding_clients: &BTreeMap<
        String,
        crate::attention::embedding_plugin::EmbeddingPluginClient,
    >,
    attention: &Arc<crate::attention::AttentionLayer>,
    cmd_rx: &mut mpsc::Receiver<ServerCommand>,
    wake_tx: &mpsc::Sender<WakeEnvelope>,
    shutdown: &mut broadcast::Receiver<()>,
    reached_live: &mut bool,
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
                    "agent_id": session.agent_id(),
                    "session_token": session.session_token().unwrap_or_default(),
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
    *reached_live = true;
    {
        let mut runtime = runtime.lock().await;
        runtime.clear_reconnect_storm();
        runtime.flush()?;
    }
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
                    ServerCommand::SendMessage { text, idempotency_key, embedding_request, reply } => {
                        let ref_id = Uuid::new_v4().to_string();
                        let idem = idempotency_key.unwrap_or_else(|| Uuid::new_v4().to_string());
                        let embeddings = compose_outbound_embeddings(
                            &text,
                            &embedding_request,
                            generated_embedding_clients,
                        ).await;
                        write.send(Message::Text(
                            serde_json::to_string(&json!({
                                "topic": "firehose",
                                "event": "post_message",
                                "payload": {
                                    "text": text,
                                    "idempotency_key": idem,
                                    "embeddings": embeddings,
                                },
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
                    "new_message" | "replay_message" => {
                        let payload = parsed.get("payload").cloned().unwrap_or_else(|| json!({}));
                        let author_id = payload.get("agentId").and_then(Value::as_str).unwrap_or("").to_string();
                        if author_id == session.agent_id() {
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
                            runtime.should_enqueue(&message_id, &timestamp)
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
                            sender_embeddings: parse_message_embeddings(&payload),
                            attention: attention.clone(),
                            runtime: runtime.clone(),
                            wake_session_key_override: server.wake_session_key.clone(),
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
                    "server_hello" | "replay_done" => {}
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

async fn update_server_reconnect_cooldown(
    status: &Arc<RwLock<DaemonStatus>>,
    server: &ServerConfig,
    cooldown: &crate::runtime_store::ReconnectCooldown,
) {
    status.write().await.set_server_reconnect_cooldown(
        &server.base_url,
        &server.server_key,
        cooldown.consecutive_failures,
        cooldown.cooldown_ms,
        cooldown.next_attempt_at.clone(),
        cooldown.last_error_kind.clone(),
    );
}

async fn wait_for_reconnect_cooldown_if_needed(
    status: &Arc<RwLock<DaemonStatus>>,
    server: &ServerConfig,
    runtime: &Arc<Mutex<RuntimeStore>>,
    shutdown: &mut broadcast::Receiver<()>,
) -> bool {
    let Some(cooldown) = runtime.lock().await.reconnect_cooldown() else {
        return false;
    };
    update_server_reconnect_cooldown(status, server, &cooldown).await;

    let now = OffsetDateTime::now_utc();
    let next_attempt_at = parse_rfc3339(&cooldown.next_attempt_at).unwrap_or(now);
    if next_attempt_at > now {
        let wait_ms = (next_attempt_at - now).whole_milliseconds().max(0) as u64;
        tokio::select! {
            _ = shutdown.recv() => return true,
            _ = tokio::time::sleep(Duration::from_millis(wait_ms)) => {}
        }
    }

    info!(
        component = "subspace",
        event = "subspace_reconnect_cooldown_attempt",
        server = %server.base_url,
        server_key = %server.server_key,
        consecutive_failures = cooldown.consecutive_failures,
        cooldown_ms = cooldown.cooldown_ms,
        next_attempt_at = %cooldown.next_attempt_at,
        last_error_kind = cooldown.last_error_kind.as_deref().unwrap_or("unknown"),
        "attempting reconnect after cooldown"
    );
    false
}

async fn record_reconnect_failure(
    status: &Arc<RwLock<DaemonStatus>>,
    server: &ServerConfig,
    runtime: &Arc<Mutex<RuntimeStore>>,
    retry: &RetryConfig,
    error_kind: String,
) -> Result<bool> {
    let cooldown = {
        let mut runtime = runtime.lock().await;
        let cooldown =
            runtime.record_reconnect_failure(OffsetDateTime::now_utc(), retry, error_kind);
        runtime.flush()?;
        cooldown
    };
    let Some(cooldown) = cooldown else {
        return Ok(false);
    };

    update_server_reconnect_cooldown(status, server, &cooldown).await;
    info!(
        component = "subspace",
        event = "subspace_reconnect_cooldown_entered",
        server = %server.base_url,
        server_key = %server.server_key,
        consecutive_failures = cooldown.consecutive_failures,
        cooldown_ms = cooldown.cooldown_ms,
        next_attempt_at = %cooldown.next_attempt_at,
        last_error_kind = cooldown.last_error_kind.as_deref().unwrap_or("unknown"),
        "subspace reconnect cooldown entered"
    );
    Ok(true)
}

fn parse_rfc3339(value: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339).ok()
}

fn reconnect_error_kind(err: &anyhow::Error) -> String {
    let message = err.to_string().to_ascii_lowercase();
    if message.contains("connection refused") {
        "connection_refused"
    } else if message.contains("timed out") || message.contains("timeout") {
        "timeout"
    } else if message.contains("join failed") {
        "join_failed"
    } else if message.contains("auth") {
        "auth_failed"
    } else if message.contains("websocket connect") {
        "websocket_connect_failed"
    } else {
        "transport_failed"
    }
    .to_string()
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

fn parse_message_embeddings(payload: &Value) -> Vec<MessageEmbedding> {
    payload
        .get("supplied_embeddings")
        .or_else(|| payload.get("suppliedEmbeddings"))
        .or_else(|| payload.get("embeddings"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|value| {
            let space_id = value
                .get("space_id")
                .or_else(|| value.get("spaceId"))
                .and_then(Value::as_str)?
                .trim()
                .to_string();
            let vector = value
                .get("vector")
                .and_then(Value::as_array)?
                .iter()
                .filter_map(Value::as_f64)
                .map(|value| value as f32)
                .collect::<Vec<_>>();
            if space_id.is_empty() || vector.is_empty() {
                return None;
            }
            Some(MessageEmbedding { space_id, vector })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_supplied_embeddings_payload_key() {
        let payload = json!({
            "supplied_embeddings": [
                {"space_id": "test:space", "vector": [1.0, 0.0]}
            ],
            "embeddings": [
                {"space_id": "wrong:space", "vector": [0.0, 1.0]}
            ]
        });

        let parsed = parse_message_embeddings(&payload);

        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].space_id, "test:space");
        assert_eq!(parsed[0].vector, vec![1.0, 0.0]);
    }

    #[test]
    fn parses_camel_case_supplied_embeddings_payload_key() {
        let payload = json!({
            "suppliedEmbeddings": [
                {"spaceId": "test:space", "vector": [1.0, 0.0]}
            ]
        });

        let parsed = parse_message_embeddings(&payload);

        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].space_id, "test:space");
        assert_eq!(parsed[0].vector, vec![1.0, 0.0]);
    }
}
