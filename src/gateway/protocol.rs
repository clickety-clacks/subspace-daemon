use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: u64 = 4;

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
    #[serde(default)]
    pub server: Option<Value>,
    #[serde(default)]
    pub features: Option<Value>,
    #[serde(default)]
    pub snapshot: Option<Value>,
    #[serde(default)]
    pub auth: Option<HelloAuth>,
    #[serde(default)]
    pub policy: HelloPolicy,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HelloAuth {
    #[serde(default, rename = "deviceToken")]
    pub device_token: Option<String>,
    #[serde(default, rename = "deviceTokens")]
    pub device_tokens: Vec<HelloDeviceToken>,
    #[serde(default, rename = "issuedAtMs")]
    pub issued_at_ms: Option<u64>,
    pub role: String,
    pub scopes: Vec<String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct HelloDeviceToken {
    #[serde(rename = "deviceToken")]
    pub device_token: String,
    pub role: String,
    pub scopes: Vec<String>,
    #[serde(default, rename = "issuedAtMs")]
    pub issued_at_ms: Option<u64>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Default, Deserialize)]
pub struct HelloPolicy {
    #[serde(default, rename = "tickIntervalMs")]
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

pub fn describe_hello_payload(payload: &Value) -> String {
    let payload_type = payload
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("<missing>");
    let protocol = payload
        .get("protocol")
        .and_then(Value::as_u64)
        .map(|value| value.to_string())
        .unwrap_or_else(|| "<missing>".to_string());
    let auth = payload.get("auth");
    let auth_role = auth
        .and_then(|value| value.get("role"))
        .and_then(Value::as_str)
        .unwrap_or("<missing>");
    let auth_scope_count = auth
        .and_then(|value| value.get("scopes"))
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    let has_device_token = auth
        .and_then(|value| value.get("deviceToken"))
        .and_then(Value::as_str)
        .is_some();
    let device_token_count = auth
        .and_then(|value| value.get("deviceTokens"))
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);

    format!(
        "type={payload_type} protocol={protocol} auth_role={auth_role} auth_scope_count={auth_scope_count} has_device_token={has_device_token} device_token_count={device_token_count}"
    )
}

#[cfg(test)]
mod tests {
    use super::{HelloOk, describe_hello_payload};

    #[test]
    fn parses_minimal_hello_ok_payload() {
        let hello: HelloOk = serde_json::from_value(serde_json::json!({
            "type": "hello-ok",
            "protocol": 4,
            "policy": { "tickIntervalMs": 15000 }
        }))
        .unwrap();
        assert_eq!(hello.hello_type, "hello-ok");
        assert_eq!(hello.protocol, 4);
        assert_eq!(hello.policy.tick_interval_ms, 15000);
        assert!(hello.server.is_none());
        assert!(hello.features.is_none());
        assert!(hello.snapshot.is_none());
    }

    #[test]
    fn parses_current_openclaw_shared_auth_hello_without_device_token() {
        let hello: HelloOk = serde_json::from_value(serde_json::json!({
            "type": "hello-ok",
            "protocol": 4,
            "server": { "version": "2026.6.8", "connId": "conn-1" },
            "features": { "methods": ["chat.send"], "events": ["tick"] },
            "snapshot": { "stateVersion": {} },
            "auth": {
                "role": "operator",
                "scopes": ["operator.write"]
            },
            "policy": {
                "maxPayload": 26214400,
                "maxBufferedBytes": 52428800,
                "tickIntervalMs": 15000
            }
        }))
        .unwrap();
        let auth = hello.auth.unwrap();
        assert_eq!(hello.hello_type, "hello-ok");
        assert_eq!(hello.protocol, 4);
        assert_eq!(auth.device_token, None);
        assert_eq!(auth.role, "operator");
        assert_eq!(auth.scopes, vec!["operator.write"]);
        assert!(auth.device_tokens.is_empty());
    }

    #[test]
    fn parses_device_token_hello_for_pairing_tokens() {
        let hello: HelloOk = serde_json::from_value(serde_json::json!({
            "type": "hello-ok",
            "protocol": 4,
            "auth": {
                "deviceToken": "primary-token",
                "role": "node",
                "scopes": [],
                "issuedAtMs": 123,
                "deviceTokens": [
                    {
                        "deviceToken": "operator-token",
                        "role": "operator",
                        "scopes": ["operator.write"],
                        "issuedAtMs": 124
                    }
                ]
            },
            "policy": { "tickIntervalMs": 15000 }
        }))
        .unwrap();
        let auth = hello.auth.unwrap();
        assert_eq!(auth.device_token.as_deref(), Some("primary-token"));
        assert_eq!(auth.issued_at_ms, Some(123));
        assert_eq!(auth.device_tokens.len(), 1);
        assert_eq!(auth.device_tokens[0].device_token, "operator-token");
    }

    #[test]
    fn hello_payload_diagnostic_redacts_token_values() {
        let description = describe_hello_payload(&serde_json::json!({
            "type": "hello-ok",
            "protocol": 4,
            "auth": {
                "role": "operator",
                "scopes": ["operator.write"],
                "deviceToken": "secret-primary-token",
                "deviceTokens": [
                    {
                        "deviceToken": "secret-secondary-token",
                        "role": "operator",
                        "scopes": ["operator.read"]
                    }
                ]
            }
        }));
        assert_eq!(
            description,
            "type=hello-ok protocol=4 auth_role=operator auth_scope_count=1 has_device_token=true device_token_count=1"
        );
        assert!(!description.contains("secret-primary-token"));
        assert!(!description.contains("secret-secondary-token"));
    }
}
