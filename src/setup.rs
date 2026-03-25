use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Result, anyhow, bail};
use reqwest::Client;
use tokio::sync::{Mutex, RwLock, broadcast, mpsc};
use tracing::info;

use crate::attention::{AttentionConfig, AttentionLayer, configured_generated_embedding_clients};
use crate::config::{
    Config, ReplayConfig, RetryConfig, ServerConfig, StoredConfig, canonicalize_base_url,
    default_config_path, default_registration_name, derive_app_paths, derive_server_key,
};
use crate::subspace::auth::register_identity;
use crate::subspace::client::{ServerHandle, start_server_manager};
use crate::subspace::identity::{
    LoadedSessionRecord, NamedIdentityRecord, SubspaceSessionRecord, load_session_record,
};
use crate::supervisor::{DaemonStatus, WakeEnvelope};

pub type SharedServerHandles = Arc<RwLock<BTreeMap<String, ServerHandle>>>;
pub type SharedServerTasks = Arc<Mutex<Vec<(String, tokio::task::JoinHandle<Result<()>>)>>>;

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct SetupRequest {
    pub url: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub identity: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct SetupResult {
    pub base_url: String,
    pub server_key: String,
    pub session_path: String,
    pub config_path: String,
    pub agent_id: String,
    pub identity: String,
    pub had_existing_session: bool,
    pub applied_live: bool,
}

#[derive(Clone)]
pub struct LiveSetupState {
    pub status: Arc<RwLock<DaemonStatus>>,
    pub server_handles: SharedServerHandles,
    pub server_tasks: SharedServerTasks,
    pub mutation_lock: Arc<Mutex<()>>,
    pub wake_tx: mpsc::Sender<WakeEnvelope>,
    pub retry: RetryConfig,
    pub replay: ReplayConfig,
    pub attention: AttentionConfig,
    pub shutdown_tx: broadcast::Sender<()>,
}

pub async fn perform_setup(
    request: SetupRequest,
    runtime: Option<LiveSetupState>,
) -> Result<SetupResult> {
    let _mutation_guard = if let Some(runtime) = runtime.as_ref() {
        Some(runtime.mutation_lock.lock().await)
    } else {
        None
    };
    let config_path = default_config_path();
    let paths = derive_app_paths(config_path.clone(), None)?;
    let base_url = canonicalize_base_url(&request.url)?;
    let server_key = derive_server_key(&base_url)?;
    let server_dir = paths.root.join("servers").join(&server_key);
    let session_path = server_dir.join("subspace-session.json");

    let mut stored = StoredConfig::load_or_default(&config_path)?;
    let existing_config = stored
        .servers
        .iter()
        .find(|server| {
            canonicalize_base_url(&server.base_url)
                .map(|value| value == base_url)
                .unwrap_or(false)
        })
        .cloned();
    let existing_session = load_session_record(&session_path)?;

    let registration_name = resolve_registration_name(
        request.name.as_deref(),
        existing_config.as_ref(),
        existing_session.as_ref(),
    )?;
    let had_existing_session = existing_session.is_some();

    let (identity_name, agent_id) = if let Some(session) = existing_session {
        match session {
            LoadedSessionRecord::Current(session) => {
                let identity_name = match request.identity.as_deref() {
                    Some(identity) => {
                        let identity = NamedIdentityRecord::validate_name(identity)?;
                        if identity != session.identity {
                            bail!(
                                "server identity is {}, not {}; delete the server state directory to switch identities",
                                session.identity,
                                identity
                            );
                        }
                        identity
                    }
                    None => session.identity.clone(),
                };
                let identity_path = paths.identities_dir.join(format!("{identity_name}.json"));
                let identity = NamedIdentityRecord::load(&identity_path)?
                    .ok_or_else(|| anyhow!("missing identity file: {}", identity_path.display()))?;
                identity.ensure_matches_agent_id(&session.agent_id)?;
                (identity_name, session.agent_id)
            }
            LoadedSessionRecord::Legacy(session) => {
                let requested_identity = request
                    .identity
                    .as_deref()
                    .ok_or_else(|| anyhow!("legacy session file requires --identity to migrate"))?;
                let identity = NamedIdentityRecord::load_or_create_from_legacy(
                    &paths.identities_dir,
                    requested_identity,
                    &session,
                )?;
                let migrated = session.migrate_to_identity(&identity)?;
                migrated.persist(&session_path)?;
                (identity.name, migrated.agent_id)
            }
        }
    } else {
        let requested_identity = request
            .identity
            .as_deref()
            .ok_or_else(|| anyhow!("setup requires --identity for a new server"))?;
        let identity =
            NamedIdentityRecord::load_or_create(&paths.identities_dir, requested_identity)?;
        let session_token = register_identity(
            &Client::builder().build()?,
            &base_url,
            &registration_name,
            &identity,
        )
        .await?;
        let mut session =
            SubspaceSessionRecord::new(identity.name.clone(), identity.public_key.clone());
        session.update_session_token(session_token);
        session.persist(&session_path)?;
        (identity.name, identity.public_key)
    };

    stored.upsert_server(
        base_url.clone(),
        registration_name.clone(),
        identity_name.clone(),
    );
    stored.save(&config_path)?;

    let applied_live = runtime.is_some();
    if let Some(ref runtime) = runtime {
        ensure_server_manager(
            runtime,
            &stored,
            &paths.config_path,
            &paths.identities_dir,
            &base_url,
        )
        .await?;
    }

    Ok(SetupResult {
        base_url,
        server_key,
        session_path: session_path.display().to_string(),
        config_path: config_path.display().to_string(),
        agent_id,
        identity: identity_name,
        had_existing_session,
        applied_live,
    })
}

async fn ensure_server_manager(
    runtime: &LiveSetupState,
    stored: &StoredConfig,
    config_path: &PathBuf,
    identities_dir: &PathBuf,
    base_url: &str,
) -> Result<()> {
    if runtime.server_handles.read().await.contains_key(base_url) {
        return Ok(());
    }

    let config = Config::from_stored(stored.clone(), derive_app_paths(config_path.clone(), None)?)?;
    let server = config
        .servers
        .into_iter()
        .find(|server| server.base_url == base_url && server.enabled)
        .ok_or_else(|| anyhow!("configured server not found after setup"))?;
    spawn_server_manager(runtime, server, identities_dir.clone()).await
}

pub(crate) async fn spawn_server_manager(
    runtime: &LiveSetupState,
    server: ServerConfig,
    identities_dir: PathBuf,
) -> Result<()> {
    let server_attention = AttentionConfig {
        local_pack_paths: server.effective_local_pack_paths(&runtime.attention.local_pack_paths),
        ..runtime.attention.clone()
    };
    let attention = Arc::new(AttentionLayer::new(server_attention).await?);
    info!(
        component = "supervisor",
        event = "attention_layer_initialized",
        server = %server.base_url,
        server_key = %server.server_key,
        receptor_count = attention.receptor_count(),
        degraded = attention.is_degraded(),
        "attention layer initialized"
    );
    let generated_embeddings = configured_generated_embedding_clients(&runtime.attention);
    let (handle, task) = start_server_manager(
        server.clone(),
        runtime.retry.clone(),
        runtime.replay.clone(),
        identities_dir,
        generated_embeddings,
        attention,
        runtime.status.clone(),
        runtime.wake_tx.clone(),
        runtime.shutdown_tx.subscribe(),
    )
    .await?;
    runtime
        .server_handles
        .write()
        .await
        .insert(server.base_url.clone(), handle);
    runtime
        .server_tasks
        .lock()
        .await
        .push((server.server_key.clone(), task));
    Ok(())
}

fn resolve_registration_name(
    requested_name: Option<&str>,
    existing_config: Option<&crate::config::StoredServerConfig>,
    existing_session: Option<&LoadedSessionRecord>,
) -> Result<String> {
    let resolved = match requested_name {
        Some(name) => name.trim().to_string(),
        None => existing_config
            .and_then(|server| server.registration_name.clone())
            .or_else(|| match existing_session {
                Some(LoadedSessionRecord::Legacy(session)) => {
                    Some(session.registration_name.clone())
                }
                _ => None,
            })
            .unwrap_or_else(default_registration_name),
    };
    if resolved.is_empty() {
        bail!("registration name must not be empty");
    }
    Ok(resolved)
}
