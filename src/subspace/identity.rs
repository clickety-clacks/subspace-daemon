use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::SigningKey;
use ed25519_dalek::pkcs8::DecodePrivateKey;
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::runtime_store::write_json_atomic;

pub const DEFAULT_SUBSPACE_OWNER: &str = "openclaw";

#[derive(Debug, Clone)]
pub struct NamedIdentityRecord {
    pub name: String,
    pub public_key: String,
    signing_key: SigningKey,
}

#[derive(Debug, Deserialize, Serialize)]
struct IdentityFile {
    name: String,
    created_at: String,
    public_key: String,
    private_key: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct LegacySessionFile {
    version: u8,
    public_key: String,
    private_key: String,
    owner: String,
    name: String,
    session_token: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SubspaceSessionRecord {
    pub identity: String,
    pub agent_id: String,
    pub session_token: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LegacySubspaceSessionRecord {
    pub agent_id: String,
    pub registration_name: String,
    pub session_token: Option<String>,
    signing_key: SigningKey,
}

#[derive(Debug, Clone)]
pub enum LoadedSessionRecord {
    Current(SubspaceSessionRecord),
    Legacy(LegacySubspaceSessionRecord),
}

impl NamedIdentityRecord {
    pub fn validate_name(raw: &str) -> Result<String> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            bail!("identity name must not be empty");
        }
        if trimmed
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
        {
            Ok(trimmed.to_string())
        } else {
            bail!("identity name must be lowercase alphanumeric plus hyphens");
        }
    }

    pub fn load(path: &Path) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let raw = fs::read_to_string(path)?;
        let parsed: IdentityFile = serde_json::from_str(&raw)?;
        let stem = path
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        if parsed.name != stem {
            bail!(
                "identity file name mismatch: expected {stem:?}, got {:?}",
                parsed.name
            );
        }
        let private_key = URL_SAFE_NO_PAD
            .decode(parsed.private_key.as_bytes())
            .context("invalid identity private key")?;
        let private_key_bytes: [u8; 32] = private_key
            .try_into()
            .map_err(|_| anyhow::anyhow!("invalid Ed25519 private key length"))?;
        let signing_key = SigningKey::from_bytes(&private_key_bytes);
        Ok(Some(Self {
            name: parsed.name,
            public_key: parsed.public_key,
            signing_key,
        }))
    }

    pub fn load_or_create(dir: &Path, name: &str) -> Result<Self> {
        let identity_name = Self::validate_name(name)?;
        let path = dir.join(format!("{identity_name}.json"));
        if let Some(existing) = Self::load(&path)? {
            return Ok(existing);
        }

        let signing_key = SigningKey::generate(&mut OsRng);
        let record = Self {
            name: identity_name.clone(),
            public_key: URL_SAFE_NO_PAD.encode(signing_key.verifying_key().as_bytes()),
            signing_key,
        };
        record.persist(&path)?;
        Ok(record)
    }

    pub fn sign_canonical_payload(&self, payload: &str) -> String {
        use ed25519_dalek::Signer;
        URL_SAFE_NO_PAD.encode(self.signing_key.sign(payload.as_bytes()).to_bytes())
    }

    pub fn ensure_matches_agent_id(&self, agent_id: &str) -> Result<()> {
        if self.public_key == agent_id {
            return Ok(());
        }
        bail!(
            "identity {:?} public key does not match session agent_id {}",
            self.name,
            agent_id
        );
    }

    pub fn persist(&self, path: &Path) -> Result<()> {
        let file = IdentityFile {
            name: self.name.clone(),
            created_at: OffsetDateTime::now_utc().format(&Rfc3339)?,
            public_key: self.public_key.clone(),
            private_key: URL_SAFE_NO_PAD.encode(self.signing_key.to_bytes()),
        };
        write_json_atomic(path, &file)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }

    pub fn load_or_create_from_legacy(
        dir: &Path,
        name: &str,
        legacy: &LegacySubspaceSessionRecord,
    ) -> Result<Self> {
        let identity_name = Self::validate_name(name)?;
        let path = dir.join(format!("{identity_name}.json"));
        if let Some(existing) = Self::load(&path)? {
            existing.ensure_matches_agent_id(&legacy.agent_id)?;
            return Ok(existing);
        }

        let record = Self {
            name: identity_name,
            public_key: legacy.agent_id.clone(),
            signing_key: SigningKey::from_bytes(&legacy.signing_key.to_bytes()),
        };
        record.persist(&path)?;
        Ok(record)
    }
}

impl SubspaceSessionRecord {
    pub fn new(identity: String, agent_id: String) -> Self {
        Self {
            identity,
            agent_id,
            session_token: None,
        }
    }

    pub fn load(path: &Path) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let raw = fs::read_to_string(path)?;
        let parsed: Self = serde_json::from_str(&raw)?;
        Ok(Some(parsed))
    }

    pub fn persist(&self, path: &Path) -> Result<()> {
        write_json_atomic(path, self)
    }

    pub fn clear_session_token(&mut self) {
        self.session_token = None;
    }

    pub fn update_session_token(&mut self, token: String) {
        self.session_token = Some(token);
    }
}

impl LegacySubspaceSessionRecord {
    pub fn migrate_to_identity(
        &self,
        identity: &NamedIdentityRecord,
    ) -> Result<SubspaceSessionRecord> {
        identity.ensure_matches_agent_id(&self.agent_id)?;
        let mut session = SubspaceSessionRecord::new(identity.name.clone(), self.agent_id.clone());
        if let Some(token) = self.session_token.clone() {
            session.update_session_token(token);
        }
        Ok(session)
    }
}

pub fn load_session_record(path: &Path) -> Result<Option<LoadedSessionRecord>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(path)?;
    if let Ok(parsed) = serde_json::from_str::<SubspaceSessionRecord>(&raw) {
        return Ok(Some(LoadedSessionRecord::Current(parsed)));
    }
    let parsed: LegacySessionFile = serde_json::from_str(&raw)?;
    let signing_key = SigningKey::from_pkcs8_der(
        &URL_SAFE_NO_PAD
            .decode(parsed.private_key.as_bytes())
            .context("invalid legacy session private key")?,
    )?;
    Ok(Some(LoadedSessionRecord::Legacy(
        LegacySubspaceSessionRecord {
            agent_id: parsed.public_key,
            registration_name: parsed.name,
            session_token: parsed.session_token,
            signing_key,
        },
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::pkcs8::EncodePrivateKey;
    use tempfile::tempdir;

    #[test]
    fn creates_named_identity_once() {
        let dir = tempdir().unwrap();
        let created = NamedIdentityRecord::load_or_create(dir.path(), "heimdal").unwrap();
        let reloaded = NamedIdentityRecord::load_or_create(dir.path(), "heimdal").unwrap();
        assert_eq!(created.name, "heimdal");
        assert_eq!(created.public_key, reloaded.public_key);
    }

    #[test]
    fn validates_identity_name() {
        assert!(NamedIdentityRecord::validate_name("heimdal-1").is_ok());
        assert!(NamedIdentityRecord::validate_name("Heimdal").is_err());
        assert!(NamedIdentityRecord::validate_name("heimdal/main").is_err());
    }

    #[test]
    fn loads_legacy_session_record() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("subspace-session.json");
        let legacy = LegacySessionFile {
            version: 1,
            public_key: "agent".to_string(),
            private_key: URL_SAFE_NO_PAD.encode(
                SigningKey::generate(&mut OsRng)
                    .to_pkcs8_der()
                    .unwrap()
                    .as_bytes(),
            ),
            owner: "openclaw".to_string(),
            name: "heimdal".to_string(),
            session_token: Some("token".to_string()),
        };
        fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();

        let loaded = load_session_record(&path).unwrap().unwrap();
        match loaded {
            LoadedSessionRecord::Current(_) => panic!("expected legacy session"),
            LoadedSessionRecord::Legacy(session) => {
                assert_eq!(session.registration_name, "heimdal");
                assert_eq!(session.session_token.as_deref(), Some("token"));
            }
        }
    }
}
