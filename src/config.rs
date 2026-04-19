use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::attention::{AttentionConfig, embedding_plugin::EmbeddingBackendConfig};
use crate::runtime_store::write_json_atomic;

#[derive(Debug, Clone)]
pub struct Config {
    pub gateway: GatewayConfig,
    pub servers: Vec<ServerConfig>,
    pub attention: AttentionConfig,
    pub routing: RoutingConfig,
    pub replay: ReplayConfig,
    pub logging: LoggingConfig,
    pub retry: RetryConfig,
    pub paths: AppPaths,
}

#[derive(Debug, Clone)]
pub struct GatewayConfig {
    pub ws_url: String,
    pub client_id: String,
    pub client_mode: String,
    pub display_name: String,
    pub device_id: Option<String>,
    pub shared_token: Option<String>,
    pub shared_password: Option<String>,
    pub requested_role: String,
    pub requested_scopes: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub base_url: String,
    pub websocket_url: String,
    pub enabled: bool,
    pub server_key: String,
    pub registration_name: String,
    pub identity: Option<String>,
    pub local_pack_paths: Option<Vec<String>>,
    pub session_path: PathBuf,
    pub runtime_path: PathBuf,
    pub wake_session_key: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RoutingConfig {
    pub wake_session_key: String,
}

#[derive(Debug, Clone)]
pub struct ReplayConfig {
    pub dedupe_window_size: usize,
    pub discard_before_ts: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LoggingConfig {
    pub level: String,
    pub json: bool,
}

#[derive(Debug, Clone)]
pub struct RetryConfig {
    pub base_ms: u64,
    pub max_ms: u64,
    pub jitter_ratio: f64,
    pub storm_guard: StormGuardConfig,
}

#[derive(Debug, Clone)]
pub struct StormGuardConfig {
    pub failure_window_ms: u64,
    pub consecutive_failure_threshold: u32,
    pub cooldown_ms: u64,
    pub max_cooldown_ms: u64,
}

impl Default for StormGuardConfig {
    fn default() -> Self {
        Self {
            failure_window_ms: 300_000,
            consecutive_failure_threshold: 10,
            cooldown_ms: 300_000,
            max_cooldown_ms: 3_600_000,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub root: PathBuf,
    pub config_path: PathBuf,
    pub socket_path: PathBuf,
    pub log_file: PathBuf,
    pub identities_dir: PathBuf,
    pub gateway_private_key_path: PathBuf,
    pub gateway_public_key_path: PathBuf,
    pub gateway_device_auth_store_path: PathBuf,
    pub launchd_plist_path: PathBuf,
    pub state_lock_path: PathBuf,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StoredConfig {
    #[serde(default)]
    pub gateway: StoredGatewayConfig,
    #[serde(default)]
    pub servers: Vec<StoredServerConfig>,
    #[serde(default)]
    pub attention: StoredAttentionConfig,
    #[serde(default)]
    pub routing: StoredRoutingConfig,
    #[serde(default)]
    pub replay: StoredReplayConfig,
    #[serde(default)]
    pub ipc: StoredIpcConfig,
    #[serde(default)]
    pub logging: StoredLoggingConfig,
    #[serde(default)]
    pub retry: StoredRetryConfig,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct StoredGatewayConfig {
    pub ws_url: Option<String>,
    pub client_id: Option<String>,
    pub client_mode: Option<String>,
    pub display_name: Option<String>,
    pub device_id: Option<String>,
    pub requested_role: Option<String>,
    pub requested_scopes: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StoredServerConfig {
    pub base_url: String,
    #[serde(default)]
    pub registration_name: Option<String>,
    #[serde(default)]
    pub identity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_pack_paths: Option<Vec<String>>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_session_key: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct StoredRoutingConfig {
    pub wake_session_key: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct StoredReplayConfig {
    pub dedupe_window_size: Option<usize>,
    pub discard_before_ts: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct StoredIpcConfig {
    pub socket_path: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct StoredLoggingConfig {
    pub level: Option<String>,
    pub file: Option<String>,
    pub json: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct StoredRetryConfig {
    pub base_ms: Option<u64>,
    pub max_ms: Option<u64>,
    pub jitter_ratio: Option<f64>,
    #[serde(default)]
    pub storm_guard: StoredStormGuardConfig,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct StoredStormGuardConfig {
    pub failure_window_ms: Option<u64>,
    pub consecutive_failure_threshold: Option<u32>,
    pub cooldown_ms: Option<u64>,
    pub max_cooldown_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct StoredAttentionConfig {
    #[serde(default)]
    pub local_pack_paths: Vec<String>,
    #[serde(default)]
    pub embedding_backends: Vec<StoredEmbeddingBackend>,
    pub threshold: Option<f32>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct StoredEmbeddingBackend {
    pub backend_id: String,
    pub exec: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub default_space_id: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Environment variables to pass to the plugin subprocess.
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
}

fn default_enabled() -> bool {
    true
}

impl Default for StoredConfig {
    fn default() -> Self {
        Self {
            gateway: StoredGatewayConfig::default(),
            servers: vec![],
            attention: StoredAttentionConfig::default(),
            routing: StoredRoutingConfig::default(),
            replay: StoredReplayConfig::default(),
            ipc: StoredIpcConfig::default(),
            logging: StoredLoggingConfig::default(),
            retry: StoredRetryConfig::default(),
        }
    }
}

impl Config {
    pub fn load(path: PathBuf) -> Result<Self> {
        let paths = derive_app_paths(path.clone(), None)?;
        let stored = StoredConfig::load(&paths.config_path)?;
        Self::from_stored(stored, paths)
    }

    pub fn from_stored(stored: StoredConfig, paths: AppPaths) -> Result<Self> {
        let home = dirs::home_dir().ok_or_else(|| anyhow!("home directory unavailable"))?;
        let openclaw_config_path = home.join(".openclaw").join("openclaw.json");
        let local_gateway_auth = load_local_gateway_auth(&openclaw_config_path)?;

        let gateway = GatewayConfig {
            ws_url: stored
                .gateway
                .ws_url
                .unwrap_or_else(|| "ws://127.0.0.1:18789".to_string()),
            client_id: require_gateway_client_id(stored.gateway.client_id.as_deref())?,
            client_mode: normalize_gateway_client_mode(stored.gateway.client_mode.as_deref()),
            display_name: stored
                .gateway
                .display_name
                .unwrap_or_else(|| "Subspace Daemon".to_string()),
            device_id: stored.gateway.device_id,
            shared_token: local_gateway_auth.shared_token,
            shared_password: local_gateway_auth.shared_password,
            requested_role: stored
                .gateway
                .requested_role
                .unwrap_or_else(|| "operator".to_string()),
            requested_scopes: normalize_scopes(
                stored
                    .gateway
                    .requested_scopes
                    .unwrap_or_else(|| vec!["operator.write".to_string()]),
            ),
        };
        if gateway.requested_scopes != vec!["operator.write".to_string()] {
            bail!("gateway.requested_scopes must be [\"operator.write\"] in v1");
        }

        let mut seen = BTreeSet::new();
        let mut servers = Vec::with_capacity(stored.servers.len());
        for server in stored.servers {
            let base_url = canonicalize_base_url(&server.base_url)?;
            if !seen.insert(base_url.clone()) {
                bail!("duplicate server base_url: {base_url}");
            }
            let server_key = derive_server_key(&base_url)?;
            let server_dir = paths.root.join("servers").join(&server_key);
            servers.push(ServerConfig {
                websocket_url: derive_subspace_ws_url(&base_url)?,
                enabled: server.enabled.unwrap_or(true),
                registration_name: server
                    .registration_name
                    .unwrap_or_else(default_registration_name),
                identity: server.identity,
                local_pack_paths: server.local_pack_paths,
                session_path: server_dir.join("subspace-session.json"),
                runtime_path: server_dir.join("runtime.json"),
                server_key,
                base_url,
                wake_session_key: server.wake_session_key,
            });
        }

        // Build attention config
        let embedding_backends = stored
            .attention
            .embedding_backends
            .into_iter()
            .map(|b| EmbeddingBackendConfig {
                backend_id: b.backend_id,
                exec_path: expand_tilde(PathBuf::from(&b.exec))
                    .to_string_lossy()
                    .to_string(),
                args: b.args,
                default_space_id: b.default_space_id,
                enabled: b.enabled,
                env: b.env,
            })
            .collect();

        let attention = AttentionConfig {
            local_pack_paths: stored.attention.local_pack_paths,
            embedding_backends,
            threshold: stored
                .attention
                .threshold
                .unwrap_or(crate::attention::DEFAULT_THRESHOLD),
        };

        Ok(Self {
            gateway,
            servers,
            attention,
            routing: RoutingConfig {
                wake_session_key: stored
                    .routing
                    .wake_session_key
                    .unwrap_or_else(|| "agent:heimdal:main".to_string()),
            },
            replay: ReplayConfig {
                dedupe_window_size: stored.replay.dedupe_window_size.unwrap_or(500),
                discard_before_ts: stored
                    .replay
                    .discard_before_ts
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty()),
            },
            logging: LoggingConfig {
                level: stored.logging.level.unwrap_or_else(|| "info".to_string()),
                json: stored.logging.json.unwrap_or(true),
            },
            retry: RetryConfig {
                base_ms: stored.retry.base_ms.unwrap_or(1_000),
                max_ms: stored.retry.max_ms.unwrap_or(60_000),
                jitter_ratio: stored.retry.jitter_ratio.unwrap_or(0.2),
                storm_guard: StormGuardConfig {
                    failure_window_ms: stored
                        .retry
                        .storm_guard
                        .failure_window_ms
                        .unwrap_or(300_000),
                    consecutive_failure_threshold: stored
                        .retry
                        .storm_guard
                        .consecutive_failure_threshold
                        .unwrap_or(10),
                    cooldown_ms: stored.retry.storm_guard.cooldown_ms.unwrap_or(300_000),
                    max_cooldown_ms: stored
                        .retry
                        .storm_guard
                        .max_cooldown_ms
                        .unwrap_or(3_600_000),
                },
            },
            paths,
        })
    }
}

impl StoredConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let raw_text = fs::read_to_string(path)
            .with_context(|| format!("failed reading config {}", path.display()))?;
        serde_json::from_str(&raw_text)
            .with_context(|| format!("failed parsing config {}", path.display()))
    }

    pub fn load_or_default(path: &Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(raw) => serde_json::from_str(&raw)
                .with_context(|| format!("failed parsing config {}", path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => {
                Err(err).with_context(|| format!("failed reading config {}", path.display()))
            }
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        write_json_atomic(path, self)
    }

    pub fn upsert_server(&mut self, base_url: String, registration_name: String, identity: String) {
        if let Some(existing) = self.servers.iter_mut().find(|server| {
            canonicalize_base_url(&server.base_url)
                .map(|value| value == base_url)
                .unwrap_or(false)
        }) {
            existing.base_url = base_url;
            existing.registration_name = Some(registration_name);
            existing.identity = Some(identity);
            existing.enabled = Some(true);
            return;
        }
        self.servers.push(StoredServerConfig {
            base_url,
            registration_name: Some(registration_name),
            identity: Some(identity),
            local_pack_paths: None,
            enabled: Some(true),
            wake_session_key: None,
        });
    }
}

impl ServerConfig {
    pub fn effective_local_pack_paths(&self, global_paths: &[String]) -> Vec<String> {
        self.local_pack_paths
            .clone()
            .unwrap_or_else(|| global_paths.to_vec())
    }
}

#[derive(Debug, Default)]
struct LocalGatewayAuth {
    shared_token: Option<String>,
    shared_password: Option<String>,
}

pub fn default_config_path() -> PathBuf {
    expand_tilde(PathBuf::from("~/.openclaw/subspace-daemon/config.json"))
}

pub fn expand_tilde(path: PathBuf) -> PathBuf {
    let raw = path.to_string_lossy();
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    path
}

pub fn default_registration_name() -> String {
    let host = fs::read_to_string("/etc/hostname")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "host".to_string());
    format!("subspace-daemon-{host}")
}

pub fn canonicalize_base_url(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("missing server base_url");
    }
    let parsed = Url::parse(trimmed).context("invalid server base_url")?;
    match parsed.scheme() {
        "http" | "https" => {}
        other => bail!("unsupported server URL scheme: {other}"),
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        bail!("server URL must not include userinfo");
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        bail!("server URL must not include query or fragment");
    }
    let host = parsed
        .host_str()
        .map(|value| value.to_ascii_lowercase())
        .ok_or_else(|| anyhow!("server URL missing host"))?;
    let scheme = parsed.scheme().to_ascii_lowercase();
    let effective_port = effective_port(&parsed)?;
    let default_port = match scheme.as_str() {
        "http" => 80,
        "https" => 443,
        _ => unreachable!(),
    };
    let path = normalize_path(parsed.path());
    let port_suffix = if effective_port == default_port {
        String::new()
    } else {
        format!(":{effective_port}")
    };
    let path_suffix = if path == "/" { String::new() } else { path };
    Ok(format!("{scheme}://{host}{port_suffix}{path_suffix}"))
}

pub fn derive_subspace_ws_url(base_url: &str) -> Result<String> {
    let canonical = canonicalize_base_url(base_url)?;
    let parsed = Url::parse(&canonical)?;
    let ws_scheme = match parsed.scheme() {
        "http" => "ws",
        "https" => "wss",
        other => bail!("unsupported server URL scheme: {other}"),
    };
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow!("server URL missing host"))?;
    let path = normalize_path(parsed.path());
    let port_suffix = match parsed.port() {
        Some(port) => format!(":{port}"),
        None => String::new(),
    };
    let base_path = if path == "/" { String::new() } else { path };
    Ok(format!(
        "{ws_scheme}://{host}{port_suffix}{base_path}/api/firehose/stream/websocket"
    ))
}

pub fn derive_server_key(base_url: &str) -> Result<String> {
    let canonical = canonicalize_base_url(base_url)?;
    let parsed = Url::parse(&canonical)?;
    let scheme = sanitize_server_key_component(parsed.scheme())
        .ok_or_else(|| anyhow!("server URL missing scheme"))?;
    let host = parsed
        .host_str()
        .and_then(sanitize_server_key_component)
        .ok_or_else(|| anyhow!("server URL missing host"))?;
    let port = effective_port(&parsed)?.to_string();

    let mut parts = vec![scheme, host, port];
    if let Some(segments) = parsed.path_segments() {
        for segment in segments.filter(|segment| !segment.is_empty()) {
            if let Some(sanitized) = sanitize_server_key_component(segment) {
                parts.push(sanitized);
            }
        }
    }
    Ok(parts.join("_"))
}

pub fn derive_app_paths(config_path: PathBuf, socket_override: Option<&str>) -> Result<AppPaths> {
    let config_path = expand_tilde(config_path);
    let root = config_path
        .parent()
        .ok_or_else(|| anyhow!("config path must have parent directory"))?
        .to_path_buf();
    let home = dirs::home_dir().ok_or_else(|| anyhow!("home directory unavailable"))?;
    let socket_path = socket_override
        .map(|value| expand_tilde(PathBuf::from(value)))
        .unwrap_or_else(|| root.join("daemon.sock"));
    Ok(AppPaths {
        root: root.clone(),
        config_path,
        socket_path,
        log_file: root.join("logs").join("daemon.log"),
        identities_dir: root.join("identities"),
        gateway_private_key_path: root.join("device").join("private.pem"),
        gateway_public_key_path: root.join("device").join("public.pem"),
        gateway_device_auth_store_path: root.join("device-auth.json"),
        launchd_plist_path: home
            .join("Library")
            .join("LaunchAgents")
            .join("ai.openclaw.subspace-daemon.plist"),
        state_lock_path: root.join("state.lock"),
    })
}

fn effective_port(parsed: &Url) -> Result<u16> {
    parsed
        .port_or_known_default()
        .ok_or_else(|| anyhow!("server URL missing effective port"))
}

fn normalize_path(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed.is_empty() || trimmed == "/" {
        return "/".to_string();
    }
    let mut normalized = trimmed.to_string();
    while normalized.ends_with('/') && normalized != "/" {
        normalized.pop();
    }
    if !normalized.starts_with('/') {
        normalized.insert(0, '/');
    }
    normalized
}

fn sanitize_server_key_component(raw: &str) -> Option<String> {
    let mut out = String::with_capacity(raw.len());
    let mut last_was_underscore = false;
    for ch in raw.chars() {
        let normalized = ch.to_ascii_lowercase();
        let is_safe = normalized.is_ascii_lowercase() || normalized.is_ascii_digit();
        if is_safe {
            out.push(normalized);
            last_was_underscore = false;
        } else if !last_was_underscore {
            out.push('_');
            last_was_underscore = true;
        }
    }
    let trimmed = out.trim_matches('_').to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn require_gateway_client_id(raw: Option<&str>) -> Result<String> {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        Some("gateway-client") => Ok("gateway-client".to_string()),
        Some(other) => bail!("gateway.client_id must be \"gateway-client\"; got {other:?}"),
        None => Ok("gateway-client".to_string()),
    }
}

fn normalize_gateway_client_mode(raw: Option<&str>) -> String {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        Some("backend") => "backend".to_string(),
        Some(other) => other.to_string(),
        None => "backend".to_string(),
    }
}

fn normalize_scopes(scopes: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = scopes
        .into_iter()
        .map(|scope| scope.trim().to_string())
        .filter(|scope| !scope.is_empty())
        .collect();
    out.sort();
    out.dedup();
    out
}

fn load_local_gateway_auth(path: &Path) -> Result<LocalGatewayAuth> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(LocalGatewayAuth::default());
        }
        Err(err) => return Err(err).with_context(|| format!("failed reading {}", path.display())),
    };
    let parsed: serde_json::Value =
        serde_json::from_str(&raw).with_context(|| format!("failed parsing {}", path.display()))?;
    let gateway = parsed.get("gateway").and_then(serde_json::Value::as_object);
    let auth = gateway.and_then(|gateway| gateway.get("auth"));
    let shared_token = auth
        .and_then(|auth| auth.get("token"))
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned);
    let shared_password = auth
        .and_then(|auth| auth.get("password"))
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned);
    Ok(LocalGatewayAuth {
        shared_token,
        shared_password,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn canonicalizes_server_urls() {
        assert_eq!(
            canonicalize_base_url("HTTP://Example.COM/").unwrap(),
            "http://example.com"
        );
        assert_eq!(
            canonicalize_base_url("https://example.com/subspace/").unwrap(),
            "https://example.com/subspace"
        );
        assert_eq!(
            canonicalize_base_url("http://example.com:8080/x/").unwrap(),
            "http://example.com:8080/x"
        );
    }

    #[test]
    fn derives_websocket_url_from_base_url() {
        assert_eq!(
            derive_subspace_ws_url("http://example.com").unwrap(),
            "ws://example.com/api/firehose/stream/websocket"
        );
        assert_eq!(
            derive_subspace_ws_url("https://example.com/subspace").unwrap(),
            "wss://example.com/subspace/api/firehose/stream/websocket"
        );
    }

    #[test]
    fn derives_server_key_from_components() {
        assert_eq!(
            derive_server_key("http://146.190.132.104").unwrap(),
            "http_146_190_132_104_80"
        );
        assert_eq!(
            derive_server_key("https://subspace.example.net").unwrap(),
            "https_subspace_example_net_443"
        );
        assert_eq!(
            derive_server_key("https://example.com/subspace").unwrap(),
            "https_example_com_443_subspace"
        );
    }

    #[test]
    fn loads_defaults_and_derives_paths() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("config.json");
        fs::write(
            &config_path,
            r#"{
              "servers": [
                { "base_url": "http://example.com", "registration_name": "heimdal", "identity": "heimdal" }
              ]
            }"#,
        )
        .unwrap();
        let config = Config::load(config_path.clone()).unwrap();
        assert_eq!(config.servers.len(), 1);
        assert_eq!(
            config.servers[0].websocket_url,
            "ws://example.com/api/firehose/stream/websocket"
        );
        assert_eq!(config.servers[0].registration_name, "heimdal");
        assert_eq!(config.servers[0].identity.as_deref(), Some("heimdal"));
        assert!(config.servers[0].runtime_path.ends_with("runtime.json"));
        assert_eq!(config.paths.config_path, config_path);
    }

    #[test]
    fn loads_multiple_servers_with_isolated_state_paths() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("config.json");
        fs::write(
            &config_path,
            r#"{
              "servers": [
                { "base_url": "http://146.190.132.104", "registration_name": "heimdal", "identity": "heimdal" },
                { "base_url": "https://example.com/subspace", "registration_name": "backup", "identity": "backup" }
              ]
            }"#,
        )
        .unwrap();

        let config = Config::load(config_path).unwrap();
        assert_eq!(config.servers.len(), 2);
        assert_eq!(config.servers[0].server_key, "http_146_190_132_104_80");
        assert_eq!(
            config.servers[1].server_key,
            "https_example_com_443_subspace"
        );
        assert!(
            config.servers[0]
                .session_path
                .ends_with("servers/http_146_190_132_104_80/subspace-session.json")
        );
        assert!(
            config.servers[1]
                .runtime_path
                .ends_with("servers/https_example_com_443_subspace/runtime.json")
        );
    }

    #[test]
    fn per_server_wake_session_key_override() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("config.json");
        fs::write(
            &config_path,
            r#"{
              "servers": [
                { "base_url": "http://146.190.132.104", "registration_name": "a", "identity": "a" },
                { "base_url": "http://64.23.172.52", "registration_name": "b", "identity": "b", "wake_session_key": "agent:custom:target" }
              ],
              "routing": { "wake_session_key": "agent:global:main" }
            }"#,
        )
        .unwrap();

        let config = Config::load(config_path).unwrap();
        assert_eq!(config.routing.wake_session_key, "agent:global:main");
        assert_eq!(config.servers[0].wake_session_key, None);
        assert_eq!(
            config.servers[1].wake_session_key.as_deref(),
            Some("agent:custom:target")
        );
    }

    #[test]
    fn existing_config_without_per_server_key_still_loads() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("config.json");
        fs::write(
            &config_path,
            r#"{
              "servers": [
                { "base_url": "http://example.com", "registration_name": "test", "identity": "test" }
              ]
            }"#,
        )
        .unwrap();

        let config = Config::load(config_path).unwrap();
        assert_eq!(config.servers[0].wake_session_key, None);
        assert_eq!(config.routing.wake_session_key, "agent:heimdal:main");
    }

    #[test]
    fn per_server_local_pack_paths_override_global_defaults() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("config.json");
        fs::write(
            &config_path,
            r#"{
              "servers": [
                {
                  "base_url": "http://146.190.132.104",
                  "registration_name": "a",
                  "identity": "a",
                  "local_pack_paths": ["~/.openclaw/subspace-daemon/receptors/packs/server-a"]
                },
                {
                  "base_url": "http://64.23.172.52",
                  "registration_name": "b",
                  "identity": "b"
                }
              ],
              "attention": {
                "local_pack_paths": ["~/.openclaw/subspace-daemon/receptors/packs/global"]
              }
            }"#,
        )
        .unwrap();

        let config = Config::load(config_path).unwrap();
        assert_eq!(
            config.servers[0].effective_local_pack_paths(&config.attention.local_pack_paths),
            vec!["~/.openclaw/subspace-daemon/receptors/packs/server-a".to_string()]
        );
        assert_eq!(
            config.servers[1].effective_local_pack_paths(&config.attention.local_pack_paths),
            vec!["~/.openclaw/subspace-daemon/receptors/packs/global".to_string()]
        );
    }

    #[test]
    fn empty_per_server_local_pack_paths_allow_passthrough() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("config.json");
        fs::write(
            &config_path,
            r#"{
              "servers": [
                {
                  "base_url": "http://146.190.132.104",
                  "registration_name": "a",
                  "identity": "a",
                  "local_pack_paths": []
                }
              ],
              "attention": {
                "local_pack_paths": ["~/.openclaw/subspace-daemon/receptors/packs/global"]
              }
            }"#,
        )
        .unwrap();

        let config = Config::load(config_path).unwrap();
        assert!(
            config.servers[0]
                .effective_local_pack_paths(&config.attention.local_pack_paths)
                .is_empty()
        );
    }

    #[test]
    fn loads_retry_storm_guard_config() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("config.json");
        fs::write(
            &config_path,
            r#"{
              "retry": {
                "base_ms": 50,
                "max_ms": 1000,
                "jitter_ratio": 0.0,
                "storm_guard": {
                  "failure_window_ms": 60000,
                  "consecutive_failure_threshold": 3,
                  "cooldown_ms": 5000,
                  "max_cooldown_ms": 30000
                }
              }
            }"#,
        )
        .unwrap();

        let config = Config::load(config_path).unwrap();
        assert_eq!(config.retry.base_ms, 50);
        assert_eq!(config.retry.storm_guard.failure_window_ms, 60_000);
        assert_eq!(config.retry.storm_guard.consecutive_failure_threshold, 3);
        assert_eq!(config.retry.storm_guard.cooldown_ms, 5_000);
        assert_eq!(config.retry.storm_guard.max_cooldown_ms, 30_000);
    }
}
