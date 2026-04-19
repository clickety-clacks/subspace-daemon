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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attention::AttentionConfig;
    use crate::config::{StoredServerConfig, default_config_path};
    use crate::subspace::client::test_handle;
    use crate::subspace::identity::NamedIdentityRecord;
    use serde_json::json;
    use std::env;
    use std::fs;
    use std::sync::{LazyLock, Mutex as StdMutex};
    use tempfile::tempdir;

    static HOME_LOCK: LazyLock<StdMutex<()>> = LazyLock::new(|| StdMutex::new(()));

    struct HomeGuard {
        original: Option<std::ffi::OsString>,
    }

    impl HomeGuard {
        fn set(path: &std::path::Path) -> Self {
            let original = env::var_os("HOME");
            unsafe {
                env::set_var("HOME", path);
            }
            Self { original }
        }
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            if let Some(value) = self.original.take() {
                unsafe {
                    env::set_var("HOME", value);
                }
            } else {
                unsafe {
                    env::remove_var("HOME");
                }
            }
        }
    }

    fn write_current_server_state(
        base_url: &str,
        registration_name: &str,
        identity_name: &str,
    ) -> Result<(crate::config::AppPaths, NamedIdentityRecord)> {
        let config_path = default_config_path();
        let paths = derive_app_paths(config_path.clone(), None)?;
        let server_key = derive_server_key(base_url)?;
        let server_dir = paths.root.join("servers").join(server_key);
        fs::create_dir_all(&server_dir)?;
        let identity = NamedIdentityRecord::load_or_create(&paths.identities_dir, identity_name)?;
        fs::write(
            &paths.config_path,
            serde_json::to_vec_pretty(&StoredConfig {
                servers: vec![StoredServerConfig {
                    base_url: base_url.to_string(),
                    registration_name: Some(registration_name.to_string()),
                    identity: Some(identity_name.to_string()),
                    local_pack_paths: None,
                    enabled: Some(true),
                    wake_session_key: None,
                }],
                ..StoredConfig::default()
            })?,
        )?;
        fs::write(
            server_dir.join("subspace-session.json"),
            serde_json::to_vec_pretty(&json!({
                "identity": identity_name,
                "agent_id": identity.public_key,
                "session_token": "token"
            }))?,
        )?;
        Ok((paths, identity))
    }

    #[tokio::test]
    async fn perform_setup_live_mutates_only_targeted_server_config() {
        let _home_lock = HOME_LOCK.lock().unwrap();
        let temp = tempdir().unwrap();
        let _home_guard = HomeGuard::set(temp.path());
        let base_url = "https://subspace.example.com";
        let (paths, identity) =
            write_current_server_state(base_url, "heimdal-old", "heimdal").unwrap();

        let status = Arc::new(RwLock::new(DaemonStatus {
            gateway_state: "live".to_string(),
            wake_session_key: "agent:heimdal:main".to_string(),
            servers: BTreeMap::from([(
                base_url.to_string(),
                crate::supervisor::ServerHealth {
                    server: base_url.to_string(),
                    server_key: derive_server_key(base_url).unwrap(),
                    subspace_state: "live".to_string(),
                    consecutive_failures: None,
                    cooldown_ms: None,
                    next_attempt_at: None,
                    last_error_kind: None,
                },
            )]),
        }));
        let server_handles: SharedServerHandles = Arc::new(RwLock::new(BTreeMap::from([(
            base_url.to_string(),
            test_handle(),
        )])));
        let server_tasks: SharedServerTasks = Arc::new(Mutex::new(Vec::new()));
        let (wake_tx, _wake_rx) = mpsc::channel(1);
        let (shutdown_tx, _shutdown_rx) = broadcast::channel(1);
        let runtime = LiveSetupState {
            status,
            server_handles: server_handles.clone(),
            server_tasks,
            mutation_lock: Arc::new(Mutex::new(())),
            wake_tx,
            retry: RetryConfig {
                base_ms: 1_000,
                max_ms: 60_000,
                jitter_ratio: 0.2,
                storm_guard: crate::config::StormGuardConfig::default(),
            },
            replay: ReplayConfig {
                dedupe_window_size: 500,
                discard_before_ts: None,
            },
            attention: AttentionConfig::default(),
            shutdown_tx,
        };

        let result = perform_setup(
            SetupRequest {
                url: base_url.to_string(),
                name: Some("heimdal-new".to_string()),
                identity: None,
            },
            Some(runtime),
        )
        .await
        .unwrap();

        let stored = StoredConfig::load(&paths.config_path).unwrap();
        assert!(result.applied_live);
        assert!(result.had_existing_session);
        assert_eq!(result.identity, "heimdal");
        assert_eq!(result.agent_id, identity.public_key);
        assert_eq!(
            stored.servers[0].registration_name.as_deref(),
            Some("heimdal-new")
        );
        assert_eq!(stored.servers[0].identity.as_deref(), Some("heimdal"));
        assert!(server_handles.read().await.contains_key(base_url));
    }
}
