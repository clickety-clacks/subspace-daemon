pub mod client {
    use anyhow::{Context, Result, anyhow};
    use bytes::Bytes;
    use http::{Method, Request, StatusCode};
    use http_body_util::{BodyExt, Full};
    use hyper::client::conn::http1;
    use hyper_util::rt::TokioIo;
    use serde::{Deserialize, Serialize};
    use serde_json::{Value, json};
    use tokio::net::UnixStream;
    use uuid::Uuid;

    #[derive(Debug, Deserialize, Serialize, Clone)]
    pub struct ClientSendResult {
        pub server: String,
        pub sent: bool,
        #[serde(default)]
        pub subspace_message_id: Option<String>,
        #[serde(default)]
        pub idempotency_key: Option<String>,
        #[serde(default)]
        pub error: Option<String>,
    }

    #[derive(Debug, Deserialize, Serialize, Clone)]
    pub struct ClientResponse {
        pub ok: bool,
        #[serde(default)]
        pub results: Vec<ClientSendResult>,
        #[serde(default)]
        pub error: Option<Value>,
    }

    #[derive(Debug, Deserialize, Serialize, Clone, Default)]
    pub struct ClientSendRequest {
        pub text: String,
        #[serde(default)]
        pub idempotency_key: Option<String>,
        #[serde(default)]
        pub server: Option<String>,
        #[serde(default)]
        pub embeddings: Vec<crate::attention::MessageEmbedding>,
        #[serde(default)]
        pub generate_for_spaces: Vec<String>,
        #[serde(default)]
        pub generated_embeddings_override_supplied: bool,
    }

    pub async fn setup_via_socket(
        socket_path: &std::path::Path,
        request: &crate::setup::SetupRequest,
    ) -> Result<crate::setup::SetupResult> {
        let stream = UnixStream::connect(socket_path)
            .await
            .with_context(|| format!("failed connecting to {}", socket_path.display()))?;
        let io = TokioIo::new(stream);
        let (mut sender, conn) = http1::handshake(io).await?;
        tokio::spawn(async move {
            if let Err(err) = conn.await {
                tracing::debug!(error = %err, "ipc client connection ended");
            }
        });

        let request = Request::builder()
            .method(Method::POST)
            .uri("http://localhost/v1/setup")
            .header("content-type", "application/json")
            .body(Full::new(Bytes::from(serde_json::to_vec(request)?)))?;
        let response = sender.send_request(request).await?;
        let status = response.status();
        let body = response.into_body().collect().await?.to_bytes();
        if status != StatusCode::OK {
            return Err(anyhow!(
                "daemon returned {}: {}",
                status,
                String::from_utf8_lossy(&body)
            ));
        }
        let parsed: crate::setup::SetupResult =
            serde_json::from_slice(&body).with_context(|| {
                format!(
                    "failed parsing daemon response: {}",
                    String::from_utf8_lossy(&body)
                )
            })?;
        Ok(parsed)
    }

    pub async fn send_via_socket(
        socket_path: &std::path::Path,
        request: &ClientSendRequest,
    ) -> Result<ClientResponse> {
        let stream = UnixStream::connect(socket_path)
            .await
            .with_context(|| format!("failed connecting to {}", socket_path.display()))?;
        let io = TokioIo::new(stream);
        let (mut sender, conn) = http1::handshake(io).await?;
        tokio::spawn(async move {
            if let Err(err) = conn.await {
                tracing::debug!(error = %err, "ipc client connection ended");
            }
        });

        let body = json!({
            "text": request.text,
            "server": request.server,
            "idempotency_key": request.idempotency_key.clone().unwrap_or_else(|| Uuid::new_v4().to_string()),
            "embeddings": request.embeddings,
            "generate_for_spaces": request.generate_for_spaces,
            "generated_embeddings_override_supplied": request.generated_embeddings_override_supplied,
        });
        let request = Request::builder()
            .method(Method::POST)
            .uri("http://localhost/v1/messages")
            .header("content-type", "application/json")
            .body(Full::new(Bytes::from(serde_json::to_vec(&body)?)))?;
        let response = sender.send_request(request).await?;
        let status = response.status();
        let body = response.into_body().collect().await?.to_bytes();
        let parsed: ClientResponse = serde_json::from_slice(&body).with_context(|| {
            format!(
                "failed parsing daemon response: {}",
                String::from_utf8_lossy(&body)
            )
        })?;
        if status != StatusCode::OK {
            return Err(anyhow!(
                "daemon returned {}: {}",
                status,
                String::from_utf8_lossy(&body)
            ));
        }
        Ok(parsed)
    }
}

use std::error::Error as StdError;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use bytes::Bytes;
use http::{Method, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use tokio::net::UnixListener;
use tokio::sync::{RwLock, broadcast};
use tracing::{debug, error, info, warn};

use crate::attention::{MessageEmbedding, OutboundEmbeddingRequest, validate_generated_spaces};
use crate::config::canonicalize_base_url;
use crate::setup::{LiveSetupState, SetupRequest, perform_setup};
use crate::supervisor::{DaemonStatus, ServerSendResultEnvelope};

#[derive(Debug, Deserialize)]
struct SendRequest {
    text: String,
    #[serde(default)]
    idempotency_key: Option<String>,
    #[serde(default)]
    server: Option<String>,
    #[serde(default)]
    embeddings: Vec<MessageEmbedding>,
    #[serde(default)]
    generate_for_spaces: Vec<String>,
    #[serde(default)]
    generated_embeddings_override_supplied: bool,
}

#[derive(Debug, Serialize)]
struct ErrorBody<'a> {
    ok: bool,
    error: ErrorShape<'a>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    results: Vec<ServerSendResultEnvelope>,
}

#[derive(Debug, Serialize)]
struct ErrorShape<'a> {
    code: &'a str,
    message: String,
}

#[derive(Debug, Serialize)]
struct HealthBody {
    ok: bool,
    gateway_state: String,
    wake_session_key: String,
    servers: Vec<crate::supervisor::ServerHealth>,
}

#[derive(Debug, Serialize)]
struct SendBody {
    ok: bool,
    results: Vec<ServerSendResultEnvelope>,
}

#[derive(Clone)]
pub struct SendRouter {
    status: Arc<RwLock<DaemonStatus>>,
    handles: crate::setup::SharedServerHandles,
}

impl SendRouter {
    pub fn new(
        status: Arc<RwLock<DaemonStatus>>,
        handles: crate::setup::SharedServerHandles,
    ) -> Self {
        Self { status, handles }
    }

    pub async fn send(
        &self,
        text: String,
        idempotency_key: Option<String>,
        server: Option<String>,
        embedding_request: OutboundEmbeddingRequest,
    ) -> std::result::Result<Vec<ServerSendResultEnvelope>, SendRouteError> {
        let snapshot = self.status.read().await.clone();
        let handles = self.handles.read().await.clone();
        let targets = match server {
            Some(server) => {
                let canonical = canonicalize_base_url(&server).map_err(|err| {
                    SendRouteError::invalid_request(format!("invalid server url: {err}"))
                })?;
                if !handles.contains_key(&canonical) {
                    return Err(SendRouteError::unknown_server());
                }
                vec![canonical]
            }
            None => snapshot
                .servers_snapshot()
                .into_iter()
                .filter(|server| server.subspace_state == "live")
                .map(|server| server.server)
                .collect(),
        };

        if targets.is_empty() {
            return Err(SendRouteError::subspace_unavailable(
                "no targeted Subspace server is live".to_string(),
            ));
        }

        if let Some(non_live) = targets
            .iter()
            .find(|target| snapshot.server_state(target) != Some("live".to_string()))
        {
            return Err(SendRouteError::subspace_unavailable(format!(
                "targeted Subspace server is not live: {non_live}"
            )));
        }

        let mut results = Vec::with_capacity(targets.len());
        for target in targets {
            let handle = handles
                .get(&target)
                .ok_or_else(|| SendRouteError::unknown_server())?;
            match handle
                .send_message(
                    text.clone(),
                    idempotency_key.clone(),
                    embedding_request.clone(),
                )
                .await
            {
                Ok(result) => results.push(ServerSendResultEnvelope {
                    server: result.server,
                    sent: true,
                    subspace_message_id: result.subspace_message_id,
                    idempotency_key: Some(result.idempotency_key),
                    error: None,
                }),
                Err(err) => results.push(ServerSendResultEnvelope {
                    server: target,
                    sent: false,
                    subspace_message_id: None,
                    idempotency_key: idempotency_key.clone(),
                    error: Some(err.to_string()),
                }),
            }
        }

        if results.iter().all(|result| result.sent) {
            Ok(results)
        } else {
            Err(SendRouteError::mixed_failure(results))
        }
    }
}

#[derive(Debug)]
pub struct SendRouteError {
    status: StatusCode,
    code: &'static str,
    message: String,
    results: Vec<ServerSendResultEnvelope>,
}

impl SendRouteError {
    fn invalid_request(message: String) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "invalid_request",
            message,
            results: Vec::new(),
        }
    }

    fn unknown_server() -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "unknown_server",
            message: "server not configured".to_string(),
            results: Vec::new(),
        }
    }

    fn subspace_unavailable(message: String) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code: "subspace_unavailable",
            message,
            results: Vec::new(),
        }
    }

    fn mixed_failure(results: Vec<ServerSendResultEnvelope>) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code: "subspace_unavailable",
            message: "one or more targeted Subspace servers failed".to_string(),
            results,
        }
    }
}

pub async fn run_ipc_server(
    socket_path: PathBuf,
    status: Arc<RwLock<DaemonStatus>>,
    send_router: SendRouter,
    setup_state: LiveSetupState,
    mut shutdown: broadcast::Receiver<()>,
) -> Result<()> {
    if let Some(parent) = socket_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    if tokio::fs::try_exists(&socket_path).await.unwrap_or(false) {
        let _ = tokio::fs::remove_file(&socket_path).await;
    }
    let listener = UnixListener::bind(&socket_path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(&socket_path, perms)?;
    }
    info!(component = "ipc", socket = %socket_path.display(), event = "ipc_listening", "unix socket listening");
    loop {
        tokio::select! {
            _ = shutdown.recv() => {
                return Ok(());
            }
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let io = TokioIo::new(stream);
                let status = status.clone();
                let send_router = send_router.clone();
                let setup_state = setup_state.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |req| {
                        handle_request(req, status.clone(), send_router.clone(), setup_state.clone())
                    });
                    if let Err(err) = http1::Builder::new().serve_connection(io, service).await {
                        let is_benign = err.is_incomplete_message()
                            || err.is_closed()
                            || err.is_canceled()
                            || err.source()
                                .and_then(|src| src.downcast_ref::<std::io::Error>())
                                .is_some_and(|io_err| io_err.kind() == std::io::ErrorKind::NotConnected);
                        if is_benign {
                            debug!(component = "ipc", error = %err, event = "ipc_connection_closed", "ipc client disconnected");
                        } else {
                            error!(component = "ipc", error = %err, event = "ipc_connection_failed", "ipc connection failed");
                        }
                    }
                });
            }
        }
    }
}

async fn handle_request(
    req: Request<Incoming>,
    status: Arc<RwLock<DaemonStatus>>,
    send_router: SendRouter,
    setup_state: LiveSetupState,
) -> Result<Response<Full<Bytes>>, hyper::Error> {
    let response = match (req.method(), req.uri().path()) {
        (&Method::GET, "/healthz") => {
            let snapshot = status.read().await.clone();
            json_response(
                StatusCode::OK,
                &HealthBody {
                    ok: snapshot.is_healthy(),
                    gateway_state: snapshot.gateway_state.clone(),
                    wake_session_key: snapshot.wake_session_key.clone(),
                    servers: snapshot.servers_snapshot(),
                },
            )
        }
        (&Method::POST, "/v1/messages") => {
            let body_bytes = req.into_body().collect().await?.to_bytes();
            let payload: SendRequest = match serde_json::from_slice(&body_bytes) {
                Ok(payload) => payload,
                Err(err) => {
                    warn!(
                        component = "ipc",
                        event = "ipc_outbound_rejected",
                        error = %err,
                        "invalid outbound request json"
                    );
                    return Ok(error_response(
                        StatusCode::BAD_REQUEST,
                        "invalid_request",
                        format!("invalid json: {err}"),
                        Vec::new(),
                    ));
                }
            };
            if payload.text.trim().is_empty() {
                warn!(
                    component = "ipc",
                    event = "ipc_outbound_rejected",
                    reason = "text required",
                    "invalid outbound request"
                );
                return Ok(error_response(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    "text required".to_string(),
                    Vec::new(),
                ));
            }
            if let Err(err) = validate_embedding_request(&payload) {
                warn!(
                    component = "ipc",
                    event = "ipc_outbound_rejected",
                    reason = %err,
                    "invalid outbound embedding request"
                );
                return Ok(error_response(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    err,
                    Vec::new(),
                ));
            }
            match send_router
                .send(
                    payload.text,
                    payload.idempotency_key,
                    payload.server,
                    OutboundEmbeddingRequest {
                        embeddings: payload.embeddings,
                        generate_for_spaces: payload.generate_for_spaces,
                        generated_embeddings_override_supplied: payload
                            .generated_embeddings_override_supplied,
                    },
                )
                .await
            {
                Ok(results) => {
                    info!(
                        component = "ipc",
                        event = "ipc_outbound_sent",
                        targets = results.len(),
                        "outbound subspace message sent"
                    );
                    json_response(StatusCode::OK, &SendBody { ok: true, results })
                }
                Err(err) => {
                    warn!(
                        component = "ipc",
                        event = "ipc_outbound_rejected",
                        error = %err.message,
                        code = err.code,
                        "subspace outbound send rejected"
                    );
                    error_response(err.status, err.code, err.message, err.results)
                }
            }
        }
        (&Method::POST, "/v1/setup") => {
            let body_bytes = req.into_body().collect().await?.to_bytes();
            let payload: SetupRequest = match serde_json::from_slice(&body_bytes) {
                Ok(payload) => payload,
                Err(err) => {
                    return Ok(error_response(
                        StatusCode::BAD_REQUEST,
                        "invalid_request",
                        format!("invalid json: {err}"),
                        Vec::new(),
                    ));
                }
            };
            match perform_setup(payload, Some(setup_state)).await {
                Ok(result) => json_response(StatusCode::OK, &result),
                Err(err) => error_response(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    err.to_string(),
                    Vec::new(),
                ),
            }
        }
        _ => error_response(
            StatusCode::NOT_FOUND,
            "not_found",
            "not found".to_string(),
            Vec::new(),
        ),
    };
    Ok(response)
}

fn validate_embedding_request(payload: &SendRequest) -> std::result::Result<(), String> {
    for embedding in &payload.embeddings {
        if embedding.space_id.trim().is_empty() {
            return Err("embeddings[].space_id must not be empty".to_string());
        }
        if embedding.vector.is_empty() {
            return Err(format!(
                "embeddings[{}].vector must not be empty",
                embedding.space_id
            ));
        }
    }
    validate_generated_spaces(&payload.generate_for_spaces).map_err(|err| err.to_string())
}

fn json_response<T: Serialize>(status: StatusCode, value: &T) -> Response<Full<Bytes>> {
    let body = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body)))
        .unwrap()
}

fn error_response(
    status: StatusCode,
    code: &'static str,
    message: String,
    results: Vec<ServerSendResultEnvelope>,
) -> Response<Full<Bytes>> {
    json_response(
        status,
        &ErrorBody {
            ok: false,
            error: ErrorShape { code, message },
            results,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeMap;

    use crate::subspace::client::test_handle;
    use crate::supervisor::ServerHealth;

    #[tokio::test]
    async fn targeted_send_to_reconnect_cooldown_returns_503() {
        let server = "https://subspace.example";
        let canonical = canonicalize_base_url(server).unwrap();
        let status = Arc::new(RwLock::new(DaemonStatus {
            gateway_state: "live".to_string(),
            wake_session_key: "agent:heimdal:main".to_string(),
            servers: BTreeMap::from([(
                canonical.clone(),
                ServerHealth {
                    server: canonical.clone(),
                    server_key: "https_subspace_example_443".to_string(),
                    subspace_state: "reconnect_cooldown".to_string(),
                    consecutive_failures: Some(10),
                    cooldown_ms: Some(300_000),
                    next_attempt_at: Some("2026-04-17T12:05:00Z".to_string()),
                    last_error_kind: Some("connect".to_string()),
                },
            )]),
        }));
        let handles = Arc::new(RwLock::new(BTreeMap::from([(
            canonical.clone(),
            test_handle(),
        )])));
        let router = SendRouter::new(status, handles);

        let err = router
            .send(
                "hello".to_string(),
                None,
                Some(server.to_string()),
                OutboundEmbeddingRequest::default(),
            )
            .await
            .unwrap_err();

        assert_eq!(err.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(err.code, "subspace_unavailable");
        assert!(err.message.contains("targeted Subspace server is not live"));
    }
}
