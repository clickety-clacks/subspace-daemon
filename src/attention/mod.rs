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

use anyhow::{Context, Result};
use tracing::{debug, error, info, warn};

use embedding_plugin::{EmbeddingBackendConfig, EmbeddingPluginClient};
use receptor::{ComputedReceptor, ReceptorClass, ReceptorDefinition, load_receptor_packs};
use scoring::{compute_receptor_vector, cosine_similarity};

/// Default threshold for receptor matching.
pub const DEFAULT_THRESHOLD: f32 = 0.45;

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
        let backend_config = config
            .embedding_backends
            .iter()
            .find(|b| b.enabled)
            .cloned();

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
                format!("failed to embed positive examples for receptor: {}", def.receptor_id)
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
                    format!("failed to embed negative examples for receptor: {}", def.receptor_id)
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
    pub async fn evaluate(&self, message_text: &str) -> AttentionResult {
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
        let Some(plugin) = &self.plugin else {
            // No plugin but have non-wildcard receptors - this is a config error, deliver all
            return AttentionResult {
                deliver: true,
                matches: Vec::new(),
                space_id: None,
                fallback: true,
            };
        };

        let message_vector = match plugin.embed(&[message_text]).await {
            Ok(mut vecs) if !vecs.is_empty() => vecs.remove(0),
            Ok(_) => {
                warn!(
                    component = "attention",
                    event = "embed_empty_response",
                    "embedding plugin returned no vectors; falling back to deliver"
                );
                return AttentionResult {
                    deliver: true,
                    matches: Vec::new(),
                    space_id: self.space_id.clone(),
                    fallback: true,
                };
            }
            Err(err) => {
                warn!(
                    component = "attention",
                    event = "embed_failed",
                    error = %err,
                    "failed to embed message; falling back to deliver"
                );
                return AttentionResult {
                    deliver: true,
                    matches: Vec::new(),
                    space_id: self.space_id.clone(),
                    fallback: true,
                };
            }
        };

        // Score against each receptor
        let mut matches = Vec::new();
        let mut any_above_threshold = false;

        for receptor in &self.receptors {
            let Some(receptor_vector) = &receptor.vector else {
                continue; // Skip wildcard (already handled above)
            };

            // Only compare vectors from same space_id
            if receptor.space_id != self.space_id {
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
        matches.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));

        AttentionResult {
            deliver: any_above_threshold,
            matches,
            space_id: self.space_id.clone(),
            fallback: false,
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

    /// Check if the attention layer has any receptors configured.
    pub fn has_receptors(&self) -> bool {
        !self.receptors.is_empty()
    }
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

    #[tokio::test]
    async fn passthrough_delivers_all() {
        let layer = AttentionLayer::passthrough();
        let result = layer.evaluate("any message").await;
        assert!(result.deliver);
        assert!(result.fallback);
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
