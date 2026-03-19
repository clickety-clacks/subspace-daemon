use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::runtime_store::write_json_atomic;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DeviceAuthEntry {
    pub token: String,
    pub role: String,
    pub scopes: Vec<String>,
    pub updated_at_ms: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct DeviceAuthFile {
    version: u8,
    #[serde(rename = "deviceId")]
    device_id: String,
    tokens: BTreeMap<String, DeviceAuthEntryWire>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct DeviceAuthEntryWire {
    token: String,
    role: String,
    scopes: Vec<String>,
    #[serde(rename = "updatedAtMs")]
    updated_at_ms: u64,
}

pub fn load_token(path: &Path, device_id: &str, role: &str) -> Option<DeviceAuthEntry> {
    let raw = fs::read_to_string(path).ok()?;
    let parsed: DeviceAuthFile = serde_json::from_str(&raw).ok()?;
    if parsed.device_id != device_id {
        return None;
    }
    let entry = parsed.tokens.get(role)?;
    Some(DeviceAuthEntry {
        token: entry.token.clone(),
        role: entry.role.clone(),
        scopes: entry.scopes.clone(),
        updated_at_ms: entry.updated_at_ms,
    })
}

pub fn store_token(
    path: &Path,
    device_id: &str,
    role: &str,
    token: &str,
    scopes: &[String],
) -> Result<()> {
    let mut file = if path.exists() {
        let raw = fs::read_to_string(path)?;
        serde_json::from_str::<DeviceAuthFile>(&raw).unwrap_or(DeviceAuthFile {
            version: 1,
            device_id: device_id.to_string(),
            tokens: BTreeMap::new(),
        })
    } else {
        DeviceAuthFile {
            version: 1,
            device_id: device_id.to_string(),
            tokens: BTreeMap::new(),
        }
    };
    file.device_id = device_id.to_string();
    let mut normalized_scopes = scopes
        .iter()
        .map(|scope| scope.trim().to_string())
        .filter(|scope| !scope.is_empty())
        .collect::<Vec<_>>();
    normalized_scopes.sort();
    normalized_scopes.dedup();
    file.tokens.insert(
        role.to_string(),
        DeviceAuthEntryWire {
            token: token.to_string(),
            role: role.to_string(),
            scopes: normalized_scopes,
            updated_at_ms: current_time_ms(),
        },
    );
    write_json_atomic(path, &file)
}

pub fn clear_token(path: &Path, device_id: &str, role: &str) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let raw = fs::read_to_string(path)?;
    let mut parsed: DeviceAuthFile = serde_json::from_str(&raw)?;
    if parsed.device_id != device_id {
        return Ok(());
    }
    parsed.tokens.remove(role);
    write_json_atomic(path, &parsed)
}

fn current_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
