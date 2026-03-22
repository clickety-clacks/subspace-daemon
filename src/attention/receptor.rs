use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// Receptor class types as defined in the spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceptorClass {
    Broad,
    Intersection,
    Project,
    Wildcard,
    AntiReceptor,
}

impl Default for ReceptorClass {
    fn default() -> Self {
        Self::Broad
    }
}

/// A single receptor definition as loaded from a receptor pack JSON file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReceptorDefinition {
    pub receptor_id: String,
    #[serde(default)]
    pub class: ReceptorClass,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub positive_examples: Vec<String>,
    #[serde(default)]
    pub negative_examples: Vec<String>,
}

/// A receptor pack as loaded from a JSON file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReceptorPack {
    #[serde(default)]
    pub pack_id: String,
    #[serde(default)]
    pub version: String,
    pub receptors: Vec<ReceptorDefinition>,
}

/// A computed receptor ready for matching.
/// Wildcard receptors have no vector; they match everything.
#[derive(Debug, Clone)]
pub struct ComputedReceptor {
    pub receptor_id: String,
    pub class: ReceptorClass,
    /// None for wildcard class.
    pub vector: Option<Vec<f32>>,
    /// The space_id of the embedding, if computed.
    pub space_id: Option<String>,
}

impl ComputedReceptor {
    /// Returns true if this is a wildcard receptor that matches all messages.
    pub fn is_wildcard(&self) -> bool {
        self.class == ReceptorClass::Wildcard
    }
}

/// Loads all receptor packs from the given directory paths.
pub fn load_receptor_packs(pack_paths: &[String]) -> Result<Vec<ReceptorDefinition>> {
    let mut all_receptors = Vec::new();

    for path_str in pack_paths {
        let expanded = crate::config::expand_tilde(std::path::PathBuf::from(path_str));
        if !expanded.exists() {
            continue;
        }

        if expanded.is_file() && expanded.extension().is_some_and(|ext| ext == "json") {
            let receptors = load_pack_file(&expanded)?;
            all_receptors.extend(receptors);
        } else if expanded.is_dir() {
            let entries = std::fs::read_dir(&expanded)
                .with_context(|| format!("failed to read receptor pack directory: {}", expanded.display()))?;

            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() && path.extension().is_some_and(|ext| ext == "json") {
                    let receptors = load_pack_file(&path)?;
                    all_receptors.extend(receptors);
                }
            }
        }
    }

    // Validate unique receptor IDs
    let mut seen = std::collections::HashSet::new();
    for receptor in &all_receptors {
        if !seen.insert(&receptor.receptor_id) {
            bail!("duplicate receptor_id: {}", receptor.receptor_id);
        }
    }

    Ok(all_receptors)
}

fn load_pack_file(path: &Path) -> Result<Vec<ReceptorDefinition>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read receptor pack: {}", path.display()))?;

    let pack: ReceptorPack = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse receptor pack: {}", path.display()))?;

    Ok(pack.receptors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn parses_receptor_pack() {
        let dir = tempdir().unwrap();
        let pack_path = dir.path().join("test-pack.json");
        std::fs::write(&pack_path, r#"{
            "pack_id": "test",
            "version": "1.0.0",
            "receptors": [
                {
                    "receptor_id": "swift-visionos",
                    "class": "intersection",
                    "description": "SwiftUI and visionOS changes",
                    "positive_examples": ["SwiftUI immersive space lifecycle changed"],
                    "negative_examples": ["Unity mixed reality"]
                },
                {
                    "receptor_id": "all",
                    "class": "wildcard",
                    "description": "Accept all"
                }
            ]
        }"#).unwrap();

        let receptors = load_receptor_packs(&[pack_path.to_string_lossy().to_string()]).unwrap();
        assert_eq!(receptors.len(), 2);
        assert_eq!(receptors[0].receptor_id, "swift-visionos");
        assert_eq!(receptors[0].class, ReceptorClass::Intersection);
        assert_eq!(receptors[1].class, ReceptorClass::Wildcard);
    }

    #[test]
    fn rejects_duplicate_receptor_ids() {
        let dir = tempdir().unwrap();
        let pack_path = dir.path().join("dupe.json");
        std::fs::write(&pack_path, r#"{
            "receptors": [
                { "receptor_id": "same", "class": "broad" },
                { "receptor_id": "same", "class": "project" }
            ]
        }"#).unwrap();

        let result = load_receptor_packs(&[pack_path.to_string_lossy().to_string()]);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("duplicate receptor_id"));
    }
}
