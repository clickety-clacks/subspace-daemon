use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde_json::{Value, json};

use crate::subspace::identity::{
    DEFAULT_SUBSPACE_OWNER, LegacySubspaceSessionRecord, NamedIdentityRecord, SubspaceSessionRecord,
};

#[derive(Debug, Clone)]
pub struct SessionAuth {
    pub token: String,
    pub expires_at: String,
}

pub async fn register_identity(
    client: &Client,
    base_url: &str,
    registration_name: &str,
    identity: &NamedIdentityRecord,
) -> Result<SessionAuth> {
    let start = client
        .post(format!("{base_url}/api/agents/register/start"))
        .json(&json!({
            "name": registration_name,
            "owner": DEFAULT_SUBSPACE_OWNER,
            "publicKey": identity.public_key,
        }))
        .send()
        .await
        .context("subspace register/start failed")?;
    let start_status = start.status();
    let start_json: Value = start.json().await.unwrap_or_else(|_| json!({}));
    if !start_status.is_success() {
        bail!(
            "register_start failed with status {}",
            start_status.as_u16()
        );
    }
    let challenge = read_string(&start_json, "challenge")?;
    let challenge_id = read_string(&start_json, "challengeId")?;
    let canonical_payload = format!(
        "{{\"challenge\":{},\"name\":{},\"owner\":{},\"publicKey\":{}}}",
        serde_json::to_string(&challenge)?,
        serde_json::to_string(registration_name)?,
        serde_json::to_string(DEFAULT_SUBSPACE_OWNER)?,
        serde_json::to_string(&identity.public_key)?,
    );
    let signature = identity.sign_canonical_payload(&canonical_payload);

    let verify = client
        .post(format!("{base_url}/api/agents/register/verify"))
        .json(&json!({
            "challengeId": challenge_id,
            "name": registration_name,
            "owner": DEFAULT_SUBSPACE_OWNER,
            "publicKey": identity.public_key,
            "signature": signature,
        }))
        .send()
        .await
        .context("subspace register/verify failed")?;
    let status = verify.status();
    let verify_json: Value = verify.json().await.unwrap_or_else(|_| json!({}));
    match status.as_u16() {
        200 | 201 => read_session_auth(&verify_json),
        409 => bail!(
            "name {:?} is already registered by a different agent on this server",
            registration_name
        ),
        _ => bail!("register_verify failed with status {}", status.as_u16()),
    }
}

pub async fn reauth_identity(
    client: &Client,
    base_url: &str,
    session: &SubspaceSessionRecord,
    identity: &NamedIdentityRecord,
) -> Result<SessionAuth> {
    let start = client
        .post(format!("{base_url}/api/agents/reauth/start"))
        .json(&json!({
            "agentId": session.agent_id,
        }))
        .send()
        .await
        .context("subspace reauth/start failed")?;
    let start_status = start.status();
    let start_json: Value = start.json().await.unwrap_or_else(|_| json!({}));
    if !start_status.is_success() {
        bail!("reauth_start failed with status {}", start_status.as_u16());
    }
    let challenge = read_string(&start_json, "challenge")?;
    let challenge_id = read_string(&start_json, "challengeId")?;
    let canonical_payload = format!(
        "{{\"agentId\":{},\"challenge\":{}}}",
        serde_json::to_string(&session.agent_id)?,
        serde_json::to_string(&challenge)?,
    );
    let signature = identity.sign_canonical_payload(&canonical_payload);

    let verify = client
        .post(format!("{base_url}/api/agents/reauth/verify"))
        .json(&json!({
            "challengeId": challenge_id,
            "agentId": session.agent_id,
            "signature": signature,
        }))
        .send()
        .await
        .context("subspace reauth/verify failed")?;
    let status = verify.status();
    let verify_json: Value = verify.json().await.unwrap_or_else(|_| json!({}));
    match status.as_u16() {
        200 | 201 => read_session_auth(&verify_json),
        _ => bail!("reauth_verify failed with status {}", status.as_u16()),
    }
}

pub async fn reauth_legacy_identity(
    client: &Client,
    base_url: &str,
    session: &LegacySubspaceSessionRecord,
) -> Result<SessionAuth> {
    let start = client
        .post(format!("{base_url}/api/agents/reauth/start"))
        .json(&json!({
            "agentId": session.agent_id,
        }))
        .send()
        .await
        .context("subspace reauth/start failed")?;
    let start_status = start.status();
    let start_json: Value = start.json().await.unwrap_or_else(|_| json!({}));
    if !start_status.is_success() {
        bail!("reauth_start failed with status {}", start_status.as_u16());
    }
    let challenge = read_string(&start_json, "challenge")?;
    let challenge_id = read_string(&start_json, "challengeId")?;
    let canonical_payload = format!(
        "{{\"agentId\":{},\"challenge\":{}}}",
        serde_json::to_string(&session.agent_id)?,
        serde_json::to_string(&challenge)?,
    );
    let signature = session.sign_canonical_payload(&canonical_payload);

    let verify = client
        .post(format!("{base_url}/api/agents/reauth/verify"))
        .json(&json!({
            "challengeId": challenge_id,
            "agentId": session.agent_id,
            "signature": signature,
        }))
        .send()
        .await
        .context("subspace reauth/verify failed")?;
    let status = verify.status();
    let verify_json: Value = verify.json().await.unwrap_or_else(|_| json!({}));
    match status.as_u16() {
        200 | 201 => read_session_auth(&verify_json),
        _ => bail!("reauth_verify failed with status {}", status.as_u16()),
    }
}

fn read_session_auth(json: &Value) -> Result<SessionAuth> {
    Ok(SessionAuth {
        token: read_string(json, "sessionToken")?,
        expires_at: read_string(json, "sessionExpiresAt")?,
    })
}

fn read_string(json: &Value, key: &str) -> Result<String> {
    json.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .context(format!("expected response field: {key}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_session_expiry_from_auth_response() {
        let auth = read_session_auth(&json!({
            "sessionToken": "token",
            "sessionExpiresAt": "2099-01-01T00:00:00Z"
        }))
        .unwrap();

        assert_eq!(auth.token, "token");
        assert_eq!(auth.expires_at, "2099-01-01T00:00:00Z");
    }

    #[test]
    fn requires_session_expiry_from_auth_response() {
        assert!(read_session_auth(&json!({"sessionToken": "token"})).is_err());
    }
}
