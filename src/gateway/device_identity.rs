use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::SigningKey;
use ed25519_dalek::pkcs8::spki::{DecodePublicKey, EncodePublicKey};
use ed25519_dalek::pkcs8::{DecodePrivateKey, EncodePrivateKey};
use pkcs8::LineEnding;
use rand::rngs::OsRng;
use sha2::{Digest, Sha256};

#[derive(Clone)]
pub struct GatewayDeviceIdentity {
    pub device_id: String,
    pub public_key_raw_base64url: String,
    signing_key: SigningKey,
}

impl GatewayDeviceIdentity {
    pub fn load_or_create(
        private_key_path: &Path,
        public_key_path: &Path,
        expected_device_id: Option<&str>,
    ) -> Result<Self> {
        if private_key_path.exists() && public_key_path.exists() {
            let private_key_pem = fs::read_to_string(private_key_path)
                .with_context(|| format!("failed reading {}", private_key_path.display()))?;
            let public_key_pem = fs::read_to_string(public_key_path)
                .with_context(|| format!("failed reading {}", public_key_path.display()))?;
            let signing_key = SigningKey::from_pkcs8_pem(&private_key_pem)?;
            let verifying_key = ed25519_dalek::VerifyingKey::from_public_key_pem(&public_key_pem)?;
            let public_key_raw_base64url = URL_SAFE_NO_PAD.encode(verifying_key.as_bytes());
            let device_id = fingerprint(verifying_key.as_bytes());
            if let Some(expected) = expected_device_id {
                if expected != device_id {
                    tracing::warn!(
                        configured_device_id = %expected,
                        derived_device_id = %device_id,
                        event = "gateway_device_id_ignored",
                        "configured gateway.device_id does not match stored key; using derived device id"
                    );
                }
            }
            return Ok(Self {
                device_id,
                public_key_raw_base64url,
                signing_key,
            });
        }

        if let Some(parent) = private_key_path.parent() {
            fs::create_dir_all(parent)?;
        }
        if let Some(parent) = public_key_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let signing_key = SigningKey::generate(&mut OsRng);
        let verifying_key = signing_key.verifying_key();
        let private_key_pem = signing_key.to_pkcs8_pem(LineEnding::LF)?.to_string();
        let public_key_pem = verifying_key.to_public_key_pem(LineEnding::LF)?;
        fs::write(private_key_path, &private_key_pem)?;
        fs::write(public_key_path, &public_key_pem)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(private_key_path, fs::Permissions::from_mode(0o600))?;
            fs::set_permissions(public_key_path, fs::Permissions::from_mode(0o600))?;
        }

        let public_key_raw_base64url = URL_SAFE_NO_PAD.encode(verifying_key.as_bytes());
        let device_id = fingerprint(verifying_key.as_bytes());
        if let Some(expected) = expected_device_id {
            if expected != device_id {
                tracing::warn!(
                    configured_device_id = %expected,
                    derived_device_id = %device_id,
                    event = "gateway_device_id_ignored",
                    "configured gateway.device_id does not match generated key; using derived device id"
                );
            }
        }
        Ok(Self {
            device_id,
            public_key_raw_base64url,
            signing_key,
        })
    }

    pub fn sign_payload(&self, payload: &str) -> String {
        use ed25519_dalek::Signer;
        URL_SAFE_NO_PAD.encode(self.signing_key.sign(payload.as_bytes()).to_bytes())
    }
}

fn fingerprint(public_key: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(public_key);
    hex(hasher.finalize())
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn creates_and_reloads_same_identity() {
        let dir = tempdir().unwrap();
        let private_key_path = dir.path().join("private.pem");
        let public_key_path = dir.path().join("public.pem");

        let created =
            GatewayDeviceIdentity::load_or_create(&private_key_path, &public_key_path, None)
                .unwrap();
        let reloaded =
            GatewayDeviceIdentity::load_or_create(&private_key_path, &public_key_path, None)
                .unwrap();

        assert_eq!(created.device_id, reloaded.device_id);
        assert_eq!(
            created.public_key_raw_base64url,
            reloaded.public_key_raw_base64url
        );
    }
}
