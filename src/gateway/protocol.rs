use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: u64 = 3;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectParams {
    #[serde(rename = "minProtocol")]
    pub min_protocol: u64,
    #[serde(rename = "maxProtocol")]
    pub max_protocol: u64,
    pub client: ConnectClient,
    #[serde(default)]
    pub caps: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commands: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permissions: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "pathEnv")]
    pub path_env: Option<String>,
    pub role: String,
    pub scopes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<DeviceAuthPayload>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthPayload>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectClient {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none", rename = "displayName")]
    pub display_name: Option<String>,
    pub version: String,
    pub platform: String,
    #[serde(skip_serializing_if = "Option::is_none", rename = "deviceFamily")]
    pub device_family: Option<String>,
    pub mode: String,
    #[serde(skip_serializing_if = "Option::is_none", rename = "instanceId")]
    pub instance_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceAuthPayload {
    pub id: String,
    #[serde(rename = "publicKey")]
    pub public_key: String,
    pub signature: String,
    #[serde(rename = "signedAt")]
    pub signed_at: u64,
    pub nonce: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthPayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "bootstrapToken")]
    pub bootstrap_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "deviceToken")]
    pub device_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestFrame {
    #[serde(rename = "type")]
    pub frame_type: String,
    pub id: String,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResponseFrame {
    #[serde(rename = "type")]
    pub frame_type: String,
    pub id: String,
    pub ok: bool,
    #[serde(default)]
    pub payload: Value,
    #[serde(default)]
    pub error: Option<GatewayError>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct EventFrame {
    #[serde(rename = "type")]
    pub frame_type: String,
    pub event: String,
    #[serde(default)]
    pub payload: Value,
    #[serde(default)]
    pub seq: Option<u64>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct GatewayError {
    pub code: Option<String>,
    pub message: String,
    #[serde(default)]
    pub details: Option<Value>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct HelloOk {
    #[serde(rename = "type")]
    pub hello_type: String,
    pub protocol: u64,
    pub server: Value,
    pub features: Value,
    pub snapshot: Value,
    pub auth: Option<HelloAuth>,
    pub policy: HelloPolicy,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HelloAuth {
    #[serde(rename = "deviceToken")]
    pub device_token: String,
    pub role: String,
    pub scopes: Vec<String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct HelloPolicy {
    #[serde(rename = "tickIntervalMs")]
    pub tick_interval_ms: u64,
}

pub fn build_device_auth_payload_v3(
    device_id: &str,
    client_id: &str,
    client_mode: &str,
    role: &str,
    scopes: &[String],
    signed_at_ms: u64,
    token: Option<&str>,
    nonce: &str,
    platform: &str,
    device_family: Option<&str>,
) -> String {
    format!(
        "v3|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
        device_id,
        client_id,
        client_mode,
        role,
        scopes.join(","),
        signed_at_ms,
        token.unwrap_or(""),
        nonce,
        platform.trim().to_lowercase(),
        device_family.unwrap_or("").trim().to_lowercase(),
    )
}
