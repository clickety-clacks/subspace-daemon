use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{RwLock, broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{info, warn};
use uuid::Uuid;

use crate::config::Config;
use crate::gateway::device_auth_store;
use crate::gateway::device_identity::GatewayDeviceIdentity;
use crate::gateway::protocol::{
    AuthPayload, ConnectClient, ConnectParams, DeviceAuthPayload, EventFrame, GatewayError,
    HelloOk, PROTOCOL_VERSION, RequestFrame, ResponseFrame, build_device_auth_payload_v3,
};
use crate::retry::jitter;
use crate::supervisor::DaemonStatus;

#[derive(Clone)]
pub struct GatewayClientHandle {
    tx: mpsc::Sender<GatewayCommand>,
}

impl GatewayClientHandle {
    pub async fn send_chat(
        &self,
        session_key: String,
        message: String,
        idempotency_key: String,
    ) -> Result<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(GatewayCommand::SendChat {
                session_key,
                message,
                idempotency_key,
                reply: reply_tx,
            })
            .await
            .map_err(|_| anyhow!("gateway command channel closed"))?;
        reply_rx
            .await
            .map_err(|_| anyhow!("gateway response channel closed"))?
    }
}

enum GatewayCommand {
    SendChat {
        session_key: String,
        message: String,
        idempotency_key: String,
        reply: oneshot::Sender<Result<()>>,
    },
}

struct PendingRequest {
    method: String,
    reply: oneshot::Sender<Result<Value>>,
}

#[derive(Debug, Clone)]
struct SelectedConnectAuth {
    auth_token: Option<String>,
    auth_device_token: Option<String>,
    auth_password: Option<String>,
    signature_token: Option<String>,
}

pub async fn start_gateway_client(
    config: Config,
    status: Arc<RwLock<DaemonStatus>>,
    shutdown: broadcast::Receiver<()>,
) -> Result<(GatewayClientHandle, JoinHandle<Result<()>>)> {
    let identity = GatewayDeviceIdentity::load_or_create(
        &config.paths.gateway_private_key_path,
        &config.paths.gateway_public_key_path,
        config.gateway.device_id.as_deref(),
    )?;
    let (tx, rx) = mpsc::channel(128);
    let task = tokio::spawn(run_gateway_task(config, status, identity, rx, shutdown));
    Ok((GatewayClientHandle { tx }, task))
}

async fn run_gateway_task(
    config: Config,
    status: Arc<RwLock<DaemonStatus>>,
    identity: GatewayDeviceIdentity,
    mut rx: mpsc::Receiver<GatewayCommand>,
    mut shutdown: broadcast::Receiver<()>,
) -> Result<()> {
    let mut backoff = config.retry.base_ms;
    loop {
        if shutdown.try_recv().is_ok() {
            return Ok(());
        }
        update_gateway_state(&status, "connecting").await;
        info!(component = "gateway", event = "gateway_connecting", url = %config.gateway.ws_url, "connecting to gateway");
        match connect_once(&config, &identity, &status, &mut rx, &mut shutdown).await {
            Ok(DisconnectReason::Restart) => {
                backoff = config.retry.base_ms;
            }
            Ok(DisconnectReason::AuthFailed) => {
                backoff = config.retry.base_ms;
                continue;
            }
            Ok(DisconnectReason::PairingRequired) => {
                backoff = config.retry.base_ms;
                warn!(
                    component = "gateway",
                    event = "daemon_degraded",
                    reason = ?DisconnectReason::PairingRequired,
                    "gateway disconnected"
                );
            }
            Ok(DisconnectReason::Transport) => {
                backoff = config.retry.base_ms;
                warn!(component = "gateway", event = "daemon_degraded", reason = ?DisconnectReason::Transport, "gateway disconnected");
            }
            Err(err) => {
                warn!(component = "gateway", event = "daemon_degraded", error = %err, "gateway connect attempt failed");
            }
        }
        tokio::select! {
            _ = shutdown.recv() => return Ok(()),
            _ = tokio::time::sleep(Duration::from_millis(jitter(
                backoff,
                config.retry.jitter_ratio,
            ))) => {}
        }
        backoff = (backoff.saturating_mul(2)).min(config.retry.max_ms);
    }
}

#[derive(Debug)]
enum DisconnectReason {
    Restart,
    PairingRequired,
    AuthFailed,
    Transport,
}

async fn connect_once(
    config: &Config,
    identity: &GatewayDeviceIdentity,
    status: &Arc<RwLock<DaemonStatus>>,
    rx: &mut mpsc::Receiver<GatewayCommand>,
    shutdown: &mut broadcast::Receiver<()>,
) -> Result<DisconnectReason> {
    let (ws, _) = connect_async(&config.gateway.ws_url)
        .await
        .context("gateway websocket connect failed")?;
    let (mut write, mut read) = ws.split();

    let nonce = loop {
        let next = tokio::select! {
            _ = shutdown.recv() => return Ok(DisconnectReason::Restart),
            next = read.next() => next.ok_or_else(|| anyhow!("gateway closed before connect challenge"))??,
        };
        let text = ws_text(next)?;
        if text.is_empty() {
            continue;
        }
        if let Ok(frame) = serde_json::from_str::<EventFrame>(&text) {
            if frame.frame_type == "event" && frame.event == "connect.challenge" {
                if let Some(nonce) = frame.payload.get("nonce").and_then(Value::as_str) {
                    break nonce.to_string();
                }
                bail!("gateway connect challenge missing nonce");
            }
        }
    };

    let connect_id = Uuid::new_v4().to_string();
    let stored_token = device_auth_store::load_token(
        &config.paths.gateway_device_auth_store_path,
        &identity.device_id,
        &config.gateway.requested_role,
    )
    .map(|entry| entry.token);
    let selected_auth = select_connect_auth(
        config.gateway.shared_token.clone(),
        config.gateway.shared_password.clone(),
        stored_token,
    );
    let signed_at = now_ms();
    let signature_payload = build_device_auth_payload_v3(
        &identity.device_id,
        &config.gateway.client_id,
        &config.gateway.client_mode,
        &config.gateway.requested_role,
        &config.gateway.requested_scopes,
        signed_at,
        selected_auth.signature_token.as_deref(),
        &nonce,
        std::env::consts::OS,
        None,
    );
    let connect = RequestFrame {
        frame_type: "req".to_string(),
        id: connect_id.clone(),
        method: "connect".to_string(),
        params: Some(serde_json::to_value(ConnectParams {
            min_protocol: PROTOCOL_VERSION,
            max_protocol: PROTOCOL_VERSION,
            client: ConnectClient {
                id: config.gateway.client_id.clone(),
                display_name: Some(config.gateway.display_name.clone()),
                version: env!("CARGO_PKG_VERSION").to_string(),
                platform: std::env::consts::OS.to_string(),
                device_family: None,
                mode: config.gateway.client_mode.clone(),
                instance_id: None,
            },
            caps: vec![],
            commands: None,
            permissions: None,
            path_env: None,
            role: config.gateway.requested_role.clone(),
            scopes: config.gateway.requested_scopes.clone(),
            device: Some(DeviceAuthPayload {
                id: identity.device_id.clone(),
                public_key: identity.public_key_raw_base64url.clone(),
                signature: identity.sign_payload(&signature_payload),
                signed_at,
                nonce,
            }),
            auth: if selected_auth.auth_token.is_some()
                || selected_auth.auth_device_token.is_some()
                || selected_auth.auth_password.is_some()
            {
                Some(AuthPayload {
                    token: selected_auth.auth_token.clone(),
                    bootstrap_token: None,
                    device_token: selected_auth.auth_device_token.clone(),
                    password: selected_auth.auth_password.clone(),
                })
            } else {
                None
            },
        })?),
    };
    write
        .send(Message::Text(serde_json::to_string(&connect)?))
        .await?;

    let hello = loop {
        let next = tokio::select! {
            _ = shutdown.recv() => {
                let _ = write.send(Message::Close(None)).await;
                return Ok(DisconnectReason::Restart);
            }
            next = read.next() => next.ok_or_else(|| anyhow!("gateway closed during connect"))??,
        };
        let text = ws_text(next)?;
        if text.is_empty() {
            continue;
        }
        if let Ok(res) = serde_json::from_str::<ResponseFrame>(&text) {
            if res.frame_type == "res" && res.id == connect_id {
                if res.ok {
                    break serde_json::from_value::<HelloOk>(res.payload)
                        .context("invalid hello-ok payload")?;
                }
                let error = res.error.unwrap_or(GatewayError {
                    code: None,
                    message: "connect failed".to_string(),
                    details: None,
                });
                let detail_code = error
                    .details
                    .as_ref()
                    .and_then(|details| details.get("code"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if detail_code == "PAIRING_REQUIRED" {
                    update_gateway_state(status, "pairing_required").await;
                    warn!(
                        component = "gateway",
                        event = "gateway_pairing_required",
                        "gateway pairing required"
                    );
                    return Ok(DisconnectReason::PairingRequired);
                }
                if detail_code == "AUTH_TOKEN_MISMATCH"
                    || detail_code == "AUTH_DEVICE_TOKEN_MISMATCH"
                {
                    let _ = device_auth_store::clear_token(
                        &config.paths.gateway_device_auth_store_path,
                        &identity.device_id,
                        &config.gateway.requested_role,
                    );
                    warn!(
                        component = "gateway",
                        event = "gateway_token_cleared",
                        "cleared stale device token"
                    );
                    return Ok(DisconnectReason::AuthFailed);
                }
                return Err(anyhow!(error.message));
            }
        }
    };

    if let Some(auth) = hello.auth {
        device_auth_store::store_token(
            &config.paths.gateway_device_auth_store_path,
            &identity.device_id,
            &auth.role,
            &auth.device_token,
            &auth.scopes,
        )?;
    }
    update_gateway_state(status, "live").await;
    info!(
        component = "gateway",
        event = "gateway_live",
        "gateway connection is live"
    );

    let mut pending: HashMap<String, PendingRequest> = HashMap::new();
    loop {
        tokio::select! {
            _ = shutdown.recv() => {
                let _ = write.send(Message::Close(None)).await;
                return Ok(DisconnectReason::Restart);
            }
            maybe_cmd = rx.recv() => {
                let Some(cmd) = maybe_cmd else {
                    let _ = write.send(Message::Close(None)).await;
                    return Ok(DisconnectReason::Restart);
                };
                match cmd {
                    GatewayCommand::SendChat { session_key, message, idempotency_key, reply } => {
                        let request_id = Uuid::new_v4().to_string();
                        let payload = json!({
                            "sessionKey": session_key,
                            "message": message,
                            "idempotencyKey": idempotency_key,
                        });
                        let frame = RequestFrame {
                            frame_type: "req".to_string(),
                            id: request_id.clone(),
                            method: "chat.send".to_string(),
                            params: Some(payload),
                        };
                        write.send(Message::Text(serde_json::to_string(&frame)?)).await?;
                        pending.insert(request_id, PendingRequest {
                            method: "chat.send".to_string(),
                            reply: oneshot_map_ok(reply),
                        });
                    }
                }
            }
            incoming = read.next() => {
                let Some(incoming) = incoming else {
                    return Ok(DisconnectReason::Transport);
                };
                let message = incoming?;
                let text = ws_text(message)?;
                if text.is_empty() {
                    continue;
                }
                if let Ok(event) = serde_json::from_str::<EventFrame>(&text) {
                    if event.frame_type == "event" && event.event == "tick" {
                        continue;
                    }
                }
                if let Ok(response) = serde_json::from_str::<ResponseFrame>(&text) {
                    if let Some(pending_request) = pending.remove(&response.id) {
                        if response.ok {
                            let _ = pending_request.reply.send(Ok(response.payload));
                        } else {
                            let message = response.error.map(|err| err.message).unwrap_or_else(|| format!("{} failed", pending_request.method));
                            let _ = pending_request.reply.send(Err(anyhow!(message)));
                        }
                    }
                }
            }
        }
    }
}

fn select_connect_auth(
    shared_token: Option<String>,
    shared_password: Option<String>,
    stored_device_token: Option<String>,
) -> SelectedConnectAuth {
    if let Some(token) = shared_token {
        return SelectedConnectAuth {
            auth_token: Some(token.clone()),
            auth_device_token: None,
            auth_password: None,
            signature_token: Some(token),
        };
    }
    if let Some(password) = shared_password {
        return SelectedConnectAuth {
            auth_token: None,
            auth_device_token: None,
            auth_password: Some(password),
            signature_token: None,
        };
    }
    if let Some(device_token) = stored_device_token {
        return SelectedConnectAuth {
            auth_token: Some(device_token.clone()),
            auth_device_token: Some(device_token.clone()),
            auth_password: None,
            signature_token: Some(device_token),
        };
    }
    SelectedConnectAuth {
        auth_token: None,
        auth_device_token: None,
        auth_password: None,
        signature_token: None,
    }
}

fn oneshot_map_ok(reply: oneshot::Sender<Result<()>>) -> oneshot::Sender<Result<Value>> {
    let (tx, rx) = oneshot::channel();
    tokio::spawn(async move {
        let result = match rx.await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(err)) => Err(err),
            Err(_) => Err(anyhow!("gateway request dropped")),
        };
        let _ = reply.send(result);
    });
    tx
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::select_connect_auth;

    #[test]
    fn prefers_shared_token_for_first_boot() {
        let auth = select_connect_auth(
            Some("shared-token".to_string()),
            None,
            Some("device-token".to_string()),
        );
        assert_eq!(auth.auth_token.as_deref(), Some("shared-token"));
        assert_eq!(auth.auth_device_token, None);
        assert_eq!(auth.signature_token.as_deref(), Some("shared-token"));
    }

    #[test]
    fn uses_device_token_when_shared_secret_is_absent() {
        let auth = select_connect_auth(None, None, Some("device-token".to_string()));
        assert_eq!(auth.auth_token.as_deref(), Some("device-token"));
        assert_eq!(auth.auth_device_token.as_deref(), Some("device-token"));
        assert_eq!(auth.signature_token.as_deref(), Some("device-token"));
    }

    #[test]
    fn uses_shared_password_without_device_token_fallback() {
        let auth = select_connect_auth(
            None,
            Some("shared-password".to_string()),
            Some("device-token".to_string()),
        );
        assert_eq!(auth.auth_token, None);
        assert_eq!(auth.auth_device_token, None);
        assert_eq!(auth.auth_password.as_deref(), Some("shared-password"));
        assert_eq!(auth.signature_token, None);
    }
}

fn ws_text(message: Message) -> Result<String> {
    match message {
        Message::Text(text) => Ok(text.to_string()),
        Message::Binary(bytes) => {
            String::from_utf8(bytes.to_vec()).context("non-utf8 websocket message")
        }
        Message::Close(_) => bail!("websocket closed"),
        Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => Ok(String::new()),
    }
}

async fn update_gateway_state(status: &Arc<RwLock<DaemonStatus>>, value: &str) {
    status.write().await.set_gateway_state(value.to_string());
}
