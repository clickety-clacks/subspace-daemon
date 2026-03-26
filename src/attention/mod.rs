//! Semantic attention layer for receptor-based message filtering.
//!
//! This module implements:
//! - Receptor pack loading and vector computation
//! - Exec-based embedding plugin communication
//! - Cosine similarity scoring against receptor vectors
//! - Delivery decisions with graceful degradation

pub mod embedding_plugin;
pub mod receptor;
pub mod scoring;

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use tracing::{debug, error, info, warn};

use embedding_plugin::{EmbeddingBackendConfig, EmbeddingPluginClient};
use receptor::{ComputedReceptor, ReceptorClass, ReceptorDefinition, load_receptor_packs};
use scoring::{compute_receptor_vector, cosine_similarity};

/// Default threshold for receptor matching.
pub const DEFAULT_THRESHOLD: f32 = 0.45;
pub const OPENAI_TEXT_EMBEDDING_3_SMALL_SPACE_ID: &str = "openai:text-embedding-3-small:1536:v1";
pub const OPENAI_TEXT_EMBEDDING_3_LARGE_SPACE_ID: &str = "openai:text-embedding-3-large:3072:v1";

/// Configuration for the attention layer.
#[derive(Debug, Clone)]
pub struct AttentionConfig {
    /// Paths to receptor pack files or directories.
    pub local_pack_paths: Vec<String>,
    /// Configured embedding backends.
    pub embedding_backends: Vec<EmbeddingBackendConfig>,
    /// Similarity threshold for delivery (default 0.45).
    pub threshold: f32,
}

impl Default for AttentionConfig {
    fn default() -> Self {
        Self {
            local_pack_paths: Vec::new(),
            embedding_backends: Vec::new(),
            threshold: DEFAULT_THRESHOLD,
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq)]
pub struct MessageEmbedding {
    pub space_id: String,
    pub vector: Vec<f32>,
}

#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize, PartialEq)]
pub struct OutboundEmbeddingRequest {
    #[serde(default)]
    pub embeddings: Vec<MessageEmbedding>,
    #[serde(default)]
    pub generate_for_spaces: Vec<String>,
    #[serde(default)]
    pub generated_embeddings_override_supplied: bool,
}

/// A match result for a single receptor.
#[derive(Debug, Clone)]
pub struct ReceptorMatch {
    pub receptor_id: String,
    pub class: ReceptorClass,
    pub score: f32,
    pub above_threshold: bool,
}

/// The result of evaluating a message against all receptors.
#[derive(Debug, Clone)]
pub struct AttentionResult {
    /// Whether the message should be delivered.
    pub deliver: bool,
    /// All receptor matches (even those below threshold).
    pub matches: Vec<ReceptorMatch>,
    /// The space_id used for embedding, if any.
    pub space_id: Option<String>,
    /// Whether this was a fallback decision (no receptors or plugin failure).
    pub fallback: bool,
}

/// The attention layer handles receptor-based message filtering.
pub struct AttentionLayer {
    config: AttentionConfig,
    /// Computed receptors ready for matching.
    receptors: Vec<ComputedReceptor>,
    /// Embedding plugin client, if available.
    plugin: Option<EmbeddingPluginClient>,
    /// The active space_id.
    space_id: Option<String>,
    /// Whether the layer is in degraded mode (fallback to deliver all).
    degraded: bool,
}

impl AttentionLayer {
    /// Create a new attention layer that delivers all messages (no filtering).
    pub fn passthrough() -> Self {
        Self {
            config: AttentionConfig::default(),
            receptors: Vec::new(),
            plugin: None,
            space_id: None,
            degraded: false,
        }
    }

    /// Create and initialize the attention layer from configuration.
    pub async fn new(config: AttentionConfig) -> Result<Self> {
        let mut layer = Self {
            config: config.clone(),
            receptors: Vec::new(),
            plugin: None,
            space_id: None,
            degraded: false,
        };

        // Find first enabled embedding backend
        let backend_config = first_enabled_backend(&config.embedding_backends);

        if let Some(backend_config) = backend_config {
            let client = EmbeddingPluginClient::new(backend_config);
            if client.is_available() {
                layer.plugin = Some(client);
                layer.space_id = layer.plugin.as_ref().map(|p| p.space_id().to_string());
                info!(
                    component = "attention",
                    event = "embedding_backend_configured",
                    backend_id = layer.plugin.as_ref().map(|p| p.backend_id()),
                    space_id = layer.space_id.as_deref(),
                    "embedding backend configured"
                );
            } else {
                warn!(
                    component = "attention",
                    event = "embedding_backend_unavailable",
                    "embedding plugin executable not found; attention layer degraded"
                );
                layer.degraded = true;
            }
        } else {
            debug!(
                component = "attention",
                event = "no_embedding_backend",
                "no embedding backend configured; attention layer disabled"
            );
            if !config.local_pack_paths.is_empty() {
                warn!(
                    component = "attention",
                    event = "embedding_backend_missing_for_receptors",
                    "receptors configured without an embedding backend; attention layer degraded"
                );
                layer.degraded = true;
            }
        }

        // Load receptor definitions
        if !config.local_pack_paths.is_empty() {
            match load_receptor_packs(&config.local_pack_paths) {
                Ok(definitions) => {
                    info!(
                        component = "attention",
                        event = "receptors_loaded",
                        count = definitions.len(),
                        "loaded receptor definitions"
                    );

                    // Compute receptor vectors
                    if let Err(err) = layer.compute_receptors(definitions).await {
                        error!(
                            component = "attention",
                            event = "receptor_computation_failed",
                            error = %err,
                            "failed to compute receptor vectors; attention layer degraded"
                        );
                        layer.degraded = true;
                    }
                }
                Err(err) => {
                    error!(
                        component = "attention",
                        event = "receptor_load_failed",
                        error = %err,
                        "failed to load receptor packs; attention layer degraded"
                    );
                    layer.degraded = true;
                }
            }
        }

        Ok(layer)
    }

    /// Compute vectors for all non-wildcard receptors.
    async fn compute_receptors(&mut self, definitions: Vec<ReceptorDefinition>) -> Result<()> {
        let Some(plugin) = &self.plugin else {
            // No plugin - just store wildcard receptors without vectors
            for def in definitions {
                self.receptors.push(ComputedReceptor {
                    receptor_id: def.receptor_id,
                    class: def.class,
                    vector: None,
                    space_id: None,
                });
            }
            return Ok(());
        };

        for def in definitions {
            if def.class == ReceptorClass::Wildcard {
                // Wildcard receptors don't need embedding
                self.receptors.push(ComputedReceptor {
                    receptor_id: def.receptor_id,
                    class: def.class,
                    vector: None,
                    space_id: None,
                });
                continue;
            }

            // Collect positive texts: description + positive_examples
            let mut pos_texts: Vec<&str> = Vec::new();
            if !def.description.trim().is_empty() {
                pos_texts.push(def.description.trim());
            }
            for example in &def.positive_examples {
                if !example.trim().is_empty() {
                    pos_texts.push(example.trim());
                }
            }

            if pos_texts.is_empty() {
                warn!(
                    component = "attention",
                    event = "receptor_no_positives",
                    receptor_id = %def.receptor_id,
                    "receptor has no positive examples; skipping"
                );
                continue;
            }

            // Embed positive texts
            let pos_vectors = plugin.embed(&pos_texts).await.with_context(|| {
                format!(
                    "failed to embed positive examples for receptor: {}",
                    def.receptor_id
                )
            })?;

            // Embed negative texts if any
            let neg_texts: Vec<&str> = def
                .negative_examples
                .iter()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .collect();

            let neg_vectors = if neg_texts.is_empty() {
                Vec::new()
            } else {
                plugin.embed(&neg_texts).await.with_context(|| {
                    format!(
                        "failed to embed negative examples for receptor: {}",
                        def.receptor_id
                    )
                })?
            };

            // Compute receptor vector
            let vector = compute_receptor_vector(&pos_vectors, &neg_vectors)
                .ok_or_else(|| anyhow::anyhow!("failed to compute receptor vector"))?;

            self.receptors.push(ComputedReceptor {
                receptor_id: def.receptor_id.clone(),
                class: def.class,
                vector: Some(vector),
                space_id: self.space_id.clone(),
            });

            debug!(
                component = "attention",
                event = "receptor_computed",
                receptor_id = %def.receptor_id,
                class = ?def.class,
                "computed receptor vector"
            );
        }

        info!(
            component = "attention",
            event = "receptors_ready",
            count = self.receptors.len(),
            "receptor vectors computed"
        );

        Ok(())
    }

    /// Evaluate a message against all receptors and decide whether to deliver.
    ///
    /// Returns an AttentionResult with delivery decision and match details.
    pub async fn evaluate(&self, message_text: &str, sender_embeddings: &[MessageEmbedding]) -> AttentionResult {
        self.evaluate_with_embeddings(message_text, Some(sender_embeddings))
            .await
    }

    pub async fn evaluate_with_embeddings(
        &self,
        message_text: &str,
        supplied_embeddings: Option<&[MessageEmbedding]>,
    ) -> AttentionResult {
        // Fallback: deliver all if no receptors configured
        if self.receptors.is_empty() {
            return AttentionResult {
                deliver: true,
                matches: Vec::new(),
                space_id: None,
                fallback: true,
            };
        }

        // Fallback: deliver all if in degraded mode
        if self.degraded {
            return AttentionResult {
                deliver: true,
                matches: Vec::new(),
                space_id: self.space_id.clone(),
                fallback: true,
            };
        }

        // Check for wildcard receptor first
        for receptor in &self.receptors {
            if receptor.is_wildcard() {
                return AttentionResult {
                    deliver: true,
                    matches: vec![ReceptorMatch {
                        receptor_id: receptor.receptor_id.clone(),
                        class: receptor.class,
                        score: 1.0,
                        above_threshold: true,
                    }],
                    space_id: None,
                    fallback: false,
                };
            }
        }

        // Embed the message
        let Some((message_vector, vector_space_id)) = self
            .select_message_vector(message_text, supplied_embeddings)
            .await
        else {
            return AttentionResult {
                deliver: false,
                matches: Vec::new(),
                space_id: None,
                fallback: false,
            };
        };

        // Score against each receptor
        let mut matches = Vec::new();
        let mut any_above_threshold = false;

        for receptor in &self.receptors {
            let Some(receptor_vector) = &receptor.vector else {
                continue; // Skip wildcard (already handled above)
            };

            // Only compare vectors from same space_id
            if receptor.space_id.as_deref() != Some(vector_space_id.as_str()) {
                continue;
            }

            let score = cosine_similarity(&message_vector, receptor_vector);
            let above_threshold = score >= self.config.threshold;

            if above_threshold {
                any_above_threshold = true;
            }

            matches.push(ReceptorMatch {
                receptor_id: receptor.receptor_id.clone(),
                class: receptor.class,
                score,
                above_threshold,
            });
        }

        // Sort matches by score descending
        matches.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        AttentionResult {
            deliver: any_above_threshold,
            matches,
            space_id: Some(vector_space_id),
            fallback: false,
        }
    }

    async fn select_message_vector(
        &self,
        message_text: &str,
        supplied_embeddings: Option<&[MessageEmbedding]>,
    ) -> Option<(Vec<f32>, String)> {
        if let Some(space_id) = self.space_id.as_deref() {
            if let Some(supplied_embeddings) = supplied_embeddings {
                if let Some(sender_embedding) = supplied_embeddings
                    .iter()
                    .find(|embedding| embedding.space_id == space_id)
                {
                    return Some((
                        sender_embedding.vector.clone(),
                        sender_embedding.space_id.clone(),
                    ));
                }
            }
        }

        let Some(plugin) = &self.plugin else {
            return None;
        };

        match plugin.embed(&[message_text]).await {
            Ok(mut vectors) if !vectors.is_empty() => {
                Some((vectors.remove(0), plugin.space_id().to_string()))
            }
            Ok(_) => None,
            Err(err) => {
                warn!(
                    component = "attention",
                    event = "message_embedding_failed",
                    error = %err,
                    "failed to compute inbound message embedding"
                );
                None
            }
        }
    }

    /// Get the number of configured receptors.
    pub fn receptor_count(&self) -> usize {
        self.receptors.len()
    }

    /// Check if the attention layer is in degraded mode.
    pub fn is_degraded(&self) -> bool {
        self.degraded
    }
}

pub fn validate_generated_spaces(requested_spaces: &[String]) -> Result<()> {
    for raw_space in requested_spaces {
        let space_id = raw_space.trim();
        if space_id.is_empty() {
            anyhow::bail!("generate_for_spaces entries must not be empty");
        }
        if !is_supported_generated_space(space_id) {
            anyhow::bail!(
                "unsupported generated embedding space: {space_id}; supported spaces are {} and {}",
                OPENAI_TEXT_EMBEDDING_3_SMALL_SPACE_ID,
                OPENAI_TEXT_EMBEDDING_3_LARGE_SPACE_ID
            );
        }
    }
    Ok(())
}

pub fn configured_generated_embedding_clients(
    config: &AttentionConfig,
) -> BTreeMap<String, EmbeddingPluginClient> {
    config
        .embedding_backends
        .iter()
        .filter(|backend| backend.enabled)
        .filter(|backend| is_supported_generated_space(&backend.default_space_id))
        .filter_map(|backend| {
            let client = EmbeddingPluginClient::new(backend.clone());
            if client.is_available() {
                Some((backend.default_space_id.clone(), client))
            } else {
                warn!(
                    component = "attention",
                    event = "generated_embedding_backend_unavailable",
                    backend_id = %backend.backend_id,
                    space_id = %backend.default_space_id,
                    "generated embedding backend unavailable; omitting space"
                );
                None
            }
        })
        .collect()
}

pub async fn compose_outbound_embeddings(
    text: &str,
    request: &OutboundEmbeddingRequest,
    generated_clients: &BTreeMap<String, EmbeddingPluginClient>,
) -> Vec<MessageEmbedding> {
    let mut generated_embeddings = Vec::new();

    for space_id in request
        .generate_for_spaces
        .iter()
        .map(|space| space.trim())
        .filter(|space| !space.is_empty())
        .collect::<BTreeSet<_>>()
    {
        if !request.generated_embeddings_override_supplied
            && request
                .embeddings
                .iter()
                .any(|embedding| embedding.space_id == space_id)
        {
            continue;
        }
        let Some(client) = generated_clients.get(space_id) else {
            warn!(
                component = "attention",
                event = "generated_embedding_backend_missing",
                space_id = %space_id,
                "requested generated embedding space is not configured locally; omitting space"
            );
            continue;
        };
        match client.embed(&[text]).await {
            Ok(mut vectors) if !vectors.is_empty() => generated_embeddings.push(MessageEmbedding {
                space_id: space_id.to_string(),
                vector: vectors.remove(0),
            }),
            Ok(_) => warn!(
                component = "attention",
                event = "generated_embedding_empty",
                space_id = %space_id,
                "generated embedding backend returned no vectors; omitting space"
            ),
            Err(err) => warn!(
                component = "attention",
                event = "generated_embedding_failed",
                space_id = %space_id,
                error = %err,
                "generated embedding failed; sending without that space"
            ),
        }
    }

    merge_outbound_embeddings(
        &request.embeddings,
        generated_embeddings,
        request.generated_embeddings_override_supplied,
    )
}

fn is_supported_generated_space(space_id: &str) -> bool {
    matches!(
        space_id,
        OPENAI_TEXT_EMBEDDING_3_SMALL_SPACE_ID | OPENAI_TEXT_EMBEDDING_3_LARGE_SPACE_ID
    )
}

fn merge_outbound_embeddings(
    supplied_embeddings: &[MessageEmbedding],
    generated_embeddings: Vec<MessageEmbedding>,
    generated_embeddings_override_supplied: bool,
) -> Vec<MessageEmbedding> {
    let mut embeddings_by_space = BTreeMap::<String, MessageEmbedding>::new();
    if generated_embeddings_override_supplied {
        for embedding in supplied_embeddings {
            embeddings_by_space.insert(embedding.space_id.clone(), embedding.clone());
        }
        for embedding in generated_embeddings {
            embeddings_by_space.insert(embedding.space_id.clone(), embedding);
        }
    } else {
        for embedding in generated_embeddings {
            embeddings_by_space.insert(embedding.space_id.clone(), embedding);
        }
        for embedding in supplied_embeddings {
            embeddings_by_space.insert(embedding.space_id.clone(), embedding.clone());
        }
    }
    embeddings_by_space.into_values().collect()
}

fn first_enabled_backend(
    embedding_backends: &[EmbeddingBackendConfig],
) -> Option<EmbeddingBackendConfig> {
    embedding_backends.iter().find(|b| b.enabled).cloned()
}

/// Format receptor matches for inclusion in a delivered message.
pub fn format_attention_annotation(result: &AttentionResult) -> Option<String> {
    if result.matches.is_empty() {
        return None;
    }

    let matched: Vec<&ReceptorMatch> = result
        .matches
        .iter()
        .filter(|m| m.above_threshold)
        .collect();

    if matched.is_empty() {
        return None;
    }

    let mut lines = vec!["ReceptorMatches:".to_string()];
    for m in matched {
        lines.push(format!("  - {} ({:.3})", m.receptor_id, m.score));
    }

    Some(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::fs;
    use tempfile::tempdir;

    #[tokio::test]
    async fn passthrough_delivers_all() {
        let layer = AttentionLayer::passthrough();
        let result = layer.evaluate("any message", &[]).await;
        assert!(result.deliver);
        assert!(result.fallback);
    }

    #[tokio::test]
    async fn prefers_sender_embedding_for_matching_space() {
        let layer = AttentionLayer {
            config: AttentionConfig {
                threshold: 0.5,
                ..AttentionConfig::default()
            },
            receptors: vec![ComputedReceptor {
                receptor_id: "match".to_string(),
                class: ReceptorClass::Broad,
                vector: Some(vec![1.0, 0.0]),
                space_id: Some("test:model:2:v1".to_string()),
            }],
            plugin: None,
            space_id: Some("test:model:2:v1".to_string()),
            degraded: false,
        };

        let result = layer
            .evaluate(
                "ignored because sender supplied vector",
                &[MessageEmbedding {
                    space_id: "test:model:2:v1".to_string(),
                    vector: vec![0.9, 0.1],
                }],
            )
            .await;

        assert!(result.deliver);
        assert!(!result.fallback);
        assert_eq!(result.space_id.as_deref(), Some("test:model:2:v1"));
    }

    #[tokio::test]
    async fn falls_back_to_local_embedding_when_sender_embedding_is_missing() {
        let dir = tempdir().unwrap();
        let plugin_path = dir.path().join("embed.sh");
        fs::write(
            &plugin_path,
            "#!/bin/sh\ncat <<'JSON'\n{\"space_id\":\"test:model:2:v1\",\"vectors\":[{\"input_id\":\"input_0\",\"vector\":[1.0,0.0]}]}\nJSON\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&plugin_path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let layer = AttentionLayer {
            config: AttentionConfig {
                threshold: 0.5,
                ..AttentionConfig::default()
            },
            receptors: vec![ComputedReceptor {
                receptor_id: "match".to_string(),
                class: ReceptorClass::Broad,
                vector: Some(vec![1.0, 0.0]),
                space_id: Some("test:model:2:v1".to_string()),
            }],
            plugin: Some(EmbeddingPluginClient::new(EmbeddingBackendConfig {
                backend_id: "test".to_string(),
                exec_path: plugin_path.display().to_string(),
                args: Vec::new(),
                default_space_id: "test:model:2:v1".to_string(),
                enabled: true,
                env: HashMap::new(),
            })),
            space_id: Some("test:model:2:v1".to_string()),
            degraded: false,
        };

        let result = layer.evaluate("no attached embedding", &[]).await;

        assert!(result.deliver);
        assert!(!result.fallback);
        assert_eq!(result.space_id.as_deref(), Some("test:model:2:v1"));
    }

    #[tokio::test]
    async fn supplied_embedding_wins_over_local_compute() {
        let dir = tempdir().unwrap();
        let plugin_path = dir.path().join("embed.sh");
        fs::write(
            &plugin_path,
            "#!/bin/sh\ncat <<'JSON'\n{\"space_id\":\"test:model:2:v1\",\"vectors\":[{\"input_id\":\"input_0\",\"vector\":[0.0,1.0]}]}\nJSON\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&plugin_path, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let layer = AttentionLayer {
            config: AttentionConfig {
                threshold: 0.5,
                ..AttentionConfig::default()
            },
            receptors: vec![ComputedReceptor {
                receptor_id: "match".to_string(),
                class: ReceptorClass::Broad,
                vector: Some(vec![1.0, 0.0]),
                space_id: Some("test:model:2:v1".to_string()),
            }],
            plugin: Some(EmbeddingPluginClient::new(EmbeddingBackendConfig {
                backend_id: "test".to_string(),
                exec_path: plugin_path.display().to_string(),
                args: Vec::new(),
                default_space_id: "test:model:2:v1".to_string(),
                enabled: true,
                env: HashMap::new(),
            })),
            space_id: Some("test:model:2:v1".to_string()),
            degraded: false,
        };

        let result = layer
            .evaluate_with_embeddings(
                "plugin would miss",
                Some(&[MessageEmbedding {
                    space_id: "test:model:2:v1".to_string(),
                    vector: vec![1.0, 0.0],
                }]),
            )
            .await;

        assert!(result.deliver);
        assert_eq!(result.space_id.as_deref(), Some("test:model:2:v1"));
    }

    #[tokio::test]
    async fn missing_backend_with_receptors_degrades_to_passthrough() {
        let layer = AttentionLayer {
            config: AttentionConfig::default(),
            receptors: vec![ComputedReceptor {
                receptor_id: "match".to_string(),
                class: ReceptorClass::Broad,
                vector: None,
                space_id: None,
            }],
            plugin: None,
            space_id: None,
            degraded: true,
        };

        let result = layer.evaluate("plaintext only", &[]).await;

        assert!(result.deliver);
        assert!(result.fallback);
    }

    #[tokio::test]
    async fn new_degrades_when_receptors_exist_without_backend() {
        let dir = tempdir().unwrap();
        let pack_path = dir.path().join("pack.json");
        fs::write(
            &pack_path,
            r#"{
              "pack_id": "test-pack",
              "version": "1.0.0",
              "receptors": [
                {
                  "receptor_id": "match",
                  "class": "broad",
                  "description": "subspace daemon review findings"
                }
              ]
            }"#,
        )
        .unwrap();

        let layer = AttentionLayer::new(AttentionConfig {
            local_pack_paths: vec![pack_path.display().to_string()],
            embedding_backends: Vec::new(),
            threshold: DEFAULT_THRESHOLD,
        })
        .await
        .unwrap();

        let result = layer.evaluate("plaintext only", &[]).await;

        assert!(layer.is_degraded());
        assert!(result.deliver);
        assert!(result.fallback);
    }

    #[test]
    fn supplied_embeddings_win_by_default_on_duplicate_space() {
        let merged = merge_outbound_embeddings(
            &[MessageEmbedding {
                space_id: OPENAI_TEXT_EMBEDDING_3_SMALL_SPACE_ID.to_string(),
                vector: vec![1.0],
            }],
            vec![MessageEmbedding {
                space_id: OPENAI_TEXT_EMBEDDING_3_SMALL_SPACE_ID.to_string(),
                vector: vec![2.0],
            }],
            false,
        );

        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].vector, vec![1.0]);
    }

    #[test]
    fn generated_embeddings_can_override_supplied_on_duplicate_space() {
        let merged = merge_outbound_embeddings(
            &[MessageEmbedding {
                space_id: OPENAI_TEXT_EMBEDDING_3_SMALL_SPACE_ID.to_string(),
                vector: vec![1.0],
            }],
            vec![MessageEmbedding {
                space_id: OPENAI_TEXT_EMBEDDING_3_SMALL_SPACE_ID.to_string(),
                vector: vec![2.0],
            }],
            true,
        );

        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].vector, vec![2.0]);
    }

    #[tokio::test]
    async fn missing_generated_backend_does_not_block_plaintext_send() {
        let embeddings = compose_outbound_embeddings(
            "plaintext still sends",
            &OutboundEmbeddingRequest {
                generate_for_spaces: vec![OPENAI_TEXT_EMBEDDING_3_SMALL_SPACE_ID.to_string()],
                ..OutboundEmbeddingRequest::default()
            },
            &BTreeMap::new(),
        )
        .await;

        assert!(embeddings.is_empty());
    }

    #[test]
    fn formats_attention_annotation() {
        let result = AttentionResult {
            deliver: true,
            matches: vec![
                ReceptorMatch {
                    receptor_id: "swift-visionos".to_string(),
                    class: ReceptorClass::Intersection,
                    score: 0.72,
                    above_threshold: true,
                },
                ReceptorMatch {
                    receptor_id: "general-dev".to_string(),
                    class: ReceptorClass::Broad,
                    score: 0.38,
                    above_threshold: false,
                },
            ],
            space_id: Some("test:model:768:v1".to_string()),
            fallback: false,
        };

        let annotation = format_attention_annotation(&result).unwrap();
        assert!(annotation.contains("swift-visionos"));
        assert!(annotation.contains("0.720"));
        assert!(!annotation.contains("general-dev")); // Below threshold
    }
}
