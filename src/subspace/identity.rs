use std::fs;
use std::path::Path;

use anyhow::Result;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::SigningKey;
use ed25519_dalek::pkcs8::{DecodePrivateKey, EncodePrivateKey};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};

use crate::runtime_store::write_json_atomic;

#[derive(Debug, Clone)]
pub struct SubspaceSessionRecord {
    pub public_key: String,
    pub owner: String,
    pub name: String,
    pub session_token: Option<String>,
    signing_key: SigningKey,
}

#[derive(Debug, Deserialize, Serialize)]
struct SessionFile {
    version: u8,
    public_key: String,
    private_key: String,
    owner: String,
    name: String,
    session_token: Option<String>,
}

impl SubspaceSessionRecord {
    pub fn new(owner: &str, name: &str) -> Self {
        let signing_key = SigningKey::generate(&mut OsRng);
        let public_key = URL_SAFE_NO_PAD.encode(signing_key.verifying_key().as_bytes());
        Self {
            public_key,
            owner: owner.to_string(),
            name: name.to_string(),
            session_token: None,
            signing_key,
        }
    }

    pub fn load(path: &Path) -> Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let raw = fs::read_to_string(path)?;
        let parsed: SessionFile = serde_json::from_str(&raw)?;
        let private_key =
            SigningKey::from_pkcs8_der(&URL_SAFE_NO_PAD.decode(parsed.private_key.as_bytes())?)?;
        Ok(Some(Self {
            public_key: parsed.public_key,
            owner: parsed.owner,
            name: parsed.name,
            session_token: parsed.session_token,
            signing_key: private_key,
        }))
    }

    pub fn sign_canonical_payload(&self, payload: &str) -> String {
        use ed25519_dalek::Signer;
        URL_SAFE_NO_PAD.encode(self.signing_key.sign(payload.as_bytes()).to_bytes())
    }

    pub fn persist(&self, path: &Path) -> Result<()> {
        let file = SessionFile {
            version: 1,
            public_key: self.public_key.clone(),
            private_key: URL_SAFE_NO_PAD.encode(self.signing_key.to_pkcs8_der()?.as_bytes()),
            owner: self.owner.clone(),
            name: self.name.clone(),
            session_token: self.session_token.clone(),
        };
        write_json_atomic(path, &file)
    }

    pub fn clear_session_token(&mut self) {
        self.session_token = None;
    }

    pub fn update_session_token(&mut self, token: String) {
        self.session_token = Some(token);
    }
}
