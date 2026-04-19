use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// Operator-facing receptor class types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceptorClass {
    Broad,
    Intersection,
    Project,
    Wildcard,
    Veto,
}

impl Default for ReceptorClass {
    fn default() -> Self {
        Self::Broad
    }
}

impl ReceptorClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Broad => "broad",
            Self::Intersection => "intersection",
            Self::Project => "project",
            Self::Wildcard => "wildcard",
            Self::Veto => "veto",
        }
    }
}

/// A single receptor definition as loaded from a receptor pack JSON file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceptorDefinition {
    pub receptor_id: String,
    #[serde(default)]
    pub class: ReceptorClass,
    #[serde(default)]
    pub query: String,
    #[serde(default)]
    pub threshold: Option<f32>,
    #[serde(default)]
    pub space_id: Option<String>,
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
    pub threshold: f32,
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

    pub fn is_veto(&self) -> bool {
        self.class == ReceptorClass::Veto
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
            let entries = std::fs::read_dir(&expanded).with_context(|| {
                format!(
                    "failed to read receptor pack directory: {}",
                    expanded.display()
                )
            })?;

            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() && path.extension().is_some_and(|ext| ext == "json") {
                    let receptors = load_pack_file(&path)?;
                    all_receptors.extend(receptors);
                }
            }
        }
    }

    // Validate unique receptor IDs and required authoring fields.
    let mut seen = std::collections::HashSet::new();
    for receptor in &all_receptors {
        if !seen.insert(&receptor.receptor_id) {
            bail!("duplicate receptor_id: {}", receptor.receptor_id);
        }
        validate_receptor_definition(receptor)?;
    }

    Ok(all_receptors)
}

fn validate_receptor_definition(receptor: &ReceptorDefinition) -> Result<()> {
    if receptor.receptor_id.trim().is_empty() {
        bail!("receptor_id must not be empty");
    }
    if receptor.class == ReceptorClass::Wildcard {
        return Ok(());
    }
    if receptor.query.trim().is_empty() {
        bail!("receptor {} requires query", receptor.receptor_id);
    }
    if receptor.threshold.is_none() {
        bail!("receptor {} requires threshold", receptor.receptor_id);
    }
    if receptor
        .space_id
        .as_deref()
        .is_some_and(|space_id| space_id.trim().is_empty())
    {
        bail!("receptor {} has empty space_id", receptor.receptor_id);
    }
    Ok(())
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
        std::fs::write(
            &pack_path,
            r#"{
            "pack_id": "test",
            "version": "1.0.0",
            "receptors": [
                {
                    "receptor_id": "swift-visionos",
                    "class": "intersection",
                    "query": "SwiftUI immersive space lifecycle changed for visionOS",
                    "threshold": 0.72,
                    "space_id": "openai:text-embedding-3-small:1536:v1"
                },
                {
                    "receptor_id": "all",
                    "class": "wildcard"
                }
            ]
        }"#,
        )
        .unwrap();

        let receptors = load_receptor_packs(&[pack_path.to_string_lossy().to_string()]).unwrap();
        assert_eq!(receptors.len(), 2);
        assert_eq!(receptors[0].receptor_id, "swift-visionos");
        assert_eq!(receptors[0].class, ReceptorClass::Intersection);
        assert_eq!(
            receptors[0].space_id.as_deref(),
            Some("openai:text-embedding-3-small:1536:v1")
        );
        assert_eq!(receptors[1].class, ReceptorClass::Wildcard);
    }

    #[test]
    fn rejects_duplicate_receptor_ids() {
        let dir = tempdir().unwrap();
        let pack_path = dir.path().join("dupe.json");
        std::fs::write(
            &pack_path,
            r#"{
            "receptors": [
                { "receptor_id": "same", "class": "broad", "query": "one", "threshold": 0.7 },
                { "receptor_id": "same", "class": "project", "query": "two", "threshold": 0.7 }
            ]
        }"#,
        )
        .unwrap();

        let result = load_receptor_packs(&[pack_path.to_string_lossy().to_string()]);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("duplicate receptor_id")
        );
    }

    #[test]
    fn rejects_legacy_negative_examples_field() {
        let dir = tempdir().unwrap();
        let pack_path = dir.path().join("legacy.json");
        std::fs::write(
            &pack_path,
            r#"{
            "receptors": [
                {
                    "receptor_id": "legacy",
                    "class": "broad",
                    "query": "valid query",
                    "threshold": 0.7,
                    "negative_examples": ["old shape"]
                }
            ]
        }"#,
        )
        .unwrap();

        let result = load_receptor_packs(&[pack_path.to_string_lossy().to_string()]);
        assert!(result.is_err());
    }

    #[test]
    fn rejects_anti_receptor_class() {
        let dir = tempdir().unwrap();
        let pack_path = dir.path().join("anti.json");
        std::fs::write(
            &pack_path,
            r#"{
            "receptors": [
                {
                    "receptor_id": "old-anti",
                    "class": "anti_receptor",
                    "query": "legacy anti receptor",
                    "threshold": 0.7
                }
            ]
        }"#,
        )
        .unwrap();

        let result = load_receptor_packs(&[pack_path.to_string_lossy().to_string()]);
        assert!(result.is_err());
    }

    #[test]
    fn requires_query_and_threshold_for_scored_receptors() {
        let dir = tempdir().unwrap();
        let pack_path = dir.path().join("missing.json");
        std::fs::write(
            &pack_path,
            r#"{
            "receptors": [
                { "receptor_id": "missing", "class": "veto", "query": "spam" }
            ]
        }"#,
        )
        .unwrap();

        let result = load_receptor_packs(&[pack_path.to_string_lossy().to_string()]);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("requires threshold")
        );
    }
}
