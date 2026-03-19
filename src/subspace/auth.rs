use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde_json::{Value, json};

use crate::subspace::identity::SubspaceSessionRecord;

pub async fn acquire_session_token(
    client: &Client,
    base_url: &str,
    record: &SubspaceSessionRecord,
) -> Result<String> {
    let start = client
        .post(format!("{base_url}/api/agents/register/start"))
        .json(&json!({
            "name": record.name,
            "owner": record.owner,
            "publicKey": record.public_key,
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
        serde_json::to_string(&record.name)?,
        serde_json::to_string(&record.owner)?,
        serde_json::to_string(&record.public_key)?,
    );
    let signature = record.sign_canonical_payload(&canonical_payload);

    let verify = client
        .post(format!("{base_url}/api/agents/register/verify"))
        .json(&json!({
            "challengeId": challenge_id,
            "name": record.name,
            "owner": record.owner,
            "publicKey": record.public_key,
            "signature": signature,
        }))
        .send()
        .await
        .context("subspace register/verify failed")?;
    let status = verify.status();
    let verify_json: Value = verify.json().await.unwrap_or_else(|_| json!({}));
    match status.as_u16() {
        201 | 200 => read_string(&verify_json, "sessionToken"),
        409 => reauth(client, base_url, record).await,
        _ => bail!("register_verify failed with status {}", status.as_u16()),
    }
}

async fn reauth(client: &Client, base_url: &str, record: &SubspaceSessionRecord) -> Result<String> {
    let start = client
        .post(format!("{base_url}/api/agents/reauth/start"))
        .json(&json!({ "agentId": record.public_key }))
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
        serde_json::to_string(&record.public_key)?,
        serde_json::to_string(&challenge)?,
    );
    let signature = record.sign_canonical_payload(&canonical_payload);
    let verify = client
        .post(format!("{base_url}/api/agents/reauth/verify"))
        .json(&json!({
            "challengeId": challenge_id,
            "agentId": record.public_key,
            "signature": signature,
        }))
        .send()
        .await
        .context("subspace reauth/verify failed")?;
    let status = verify.status();
    let verify_json: Value = verify.json().await.unwrap_or_else(|_| json!({}));
    if !status.is_success() {
        bail!("reauth_verify failed with status {}", status.as_u16());
    }
    read_string(&verify_json, "sessionToken")
}

fn read_string(json: &Value, key: &str) -> Result<String> {
    json.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .context(format!("expected response field: {key}"))
}
