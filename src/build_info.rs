use std::fs::File;
use std::io::Read;

use serde::Serialize;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize)]
pub struct BuildInfo {
    pub package: String,
    pub version: String,
    pub source_commit: String,
    pub build_timestamp: String,
    pub target: String,
    pub profile: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binary_hash: Option<String>,
}

pub fn current(binary_hash: Option<String>) -> BuildInfo {
    BuildInfo {
        package: env!("CARGO_PKG_NAME").to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        source_commit: option_env!("SUBSPACE_BUILD_COMMIT")
            .unwrap_or("unknown")
            .to_string(),
        build_timestamp: option_env!("SUBSPACE_BUILD_TIMESTAMP")
            .unwrap_or("unknown")
            .to_string(),
        target: option_env!("SUBSPACE_BUILD_TARGET")
            .unwrap_or("unknown")
            .to_string(),
        profile: option_env!("SUBSPACE_BUILD_PROFILE")
            .unwrap_or("unknown")
            .to_string(),
        binary_hash,
    }
}

pub fn current_exe_sha256() -> Option<String> {
    let path = std::env::current_exe().ok()?;
    let mut file = File::open(path).ok()?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 8192];
    loop {
        let bytes_read = file.read(&mut buffer).ok()?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }
    Some(format!("sha256:{}", hex_lower(&hasher.finalize())))
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_build_info_includes_build_metadata() {
        let info = current(Some("sha256:test".to_string()));

        assert_eq!(info.package, "subspace-daemon");
        assert_eq!(info.version, env!("CARGO_PKG_VERSION"));
        assert!(!info.source_commit.is_empty());
        assert!(!info.build_timestamp.is_empty());
        assert!(!info.target.is_empty());
        assert!(!info.profile.is_empty());
        assert_eq!(info.binary_hash.as_deref(), Some("sha256:test"));
    }
}
