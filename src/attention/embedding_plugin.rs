//! Exec-based embedding plugin client.
//!
//! Spawns a configured executable, sends JSON requests on stdin, reads JSON responses from stdout.
//! Per the CLI/plugin spec in subspace-embedding-cli-plugin-spec.md.

use std::path::Path;
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// Configuration for an embedding backend executable.
#[derive(Debug, Clone)]
pub struct EmbeddingBackendConfig {
    pub backend_id: String,
    pub exec_path: String,
    pub args: Vec<String>,
    pub default_space_id: String,
    pub enabled: bool,
}

/// Request to describe backend capabilities.
#[derive(Debug, Serialize)]
struct DescribeRequest {
    op: &'static str,
}


/// Response from describe operation.
#[derive(Debug, Deserialize)]
pub struct DescribeResponse {
    pub backend: BackendInfo,
    pub spaces: Vec<SpaceInfo>,
}

#[derive(Debug, Deserialize)]
pub struct BackendInfo {
    pub backend_id: String,
    #[serde(default)]
    pub backend_version: String,
    #[serde(default)]
    pub provider: String,
}

#[derive(Debug, Deserialize)]
pub struct SpaceInfo {
    pub space_id: String,
    #[serde(default)]
    pub model_id: String,
    #[serde(default)]
    pub dimensions: usize,
    #[serde(default)]
    pub distance: String,
}

/// Response from embed operation.
#[derive(Debug, Deserialize)]
pub struct EmbedResponse {
    pub space_id: String,
    #[serde(default)]
    pub model_id: String,
    #[serde(default)]
    pub generated_by: Option<GeneratedBy>,
    pub vectors: Vec<VectorResult>,
}

#[derive(Debug, Deserialize)]
pub struct GeneratedBy {
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub backend_id: String,
    #[serde(default)]
    pub backend_version: String,
}

#[derive(Debug, Deserialize)]
pub struct VectorResult {
    pub input_id: String,
    pub vector: Vec<f32>,
}

/// Error response from plugin.
#[derive(Debug, Deserialize)]
struct PluginError {
    error: ErrorInfo,
}

#[derive(Debug, Deserialize)]
struct ErrorInfo {
    code: String,
    message: String,
}

/// Client for communicating with an embedding plugin executable.
pub struct EmbeddingPluginClient {
    config: EmbeddingBackendConfig,
}

impl EmbeddingPluginClient {
    /// Create a new client with the given configuration.
    pub fn new(config: EmbeddingBackendConfig) -> Self {
        Self { config }
    }

    /// Get the configured space_id for this client.
    pub fn space_id(&self) -> &str {
        &self.config.default_space_id
    }

    /// Get the backend_id for this client.
    pub fn backend_id(&self) -> &str {
        &self.config.backend_id
    }

    /// Check if the configured executable exists.
    pub fn is_available(&self) -> bool {
        Path::new(&self.config.exec_path).exists()
    }

    /// Call the describe operation to get backend capabilities.
    pub async fn describe(&self) -> Result<DescribeResponse> {
        let request = DescribeRequest { op: "describe" };
        let response: DescribeResponse = self.invoke(&request).await?;
        Ok(response)
    }

    /// Embed one or more texts.
    ///
    /// Returns vectors in the same order as the input texts.
    pub async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        // Build inputs with owned strings for input_id
        let inputs: Vec<(String, &str)> = texts
            .iter()
            .enumerate()
            .map(|(i, text)| (format!("input_{i}"), *text))
            .collect();

        let request = serde_json::json!({
            "op": "embed",
            "space_id": &self.config.default_space_id,
            "inputs": inputs.iter().map(|(id, text)| {
                serde_json::json!({
                    "input_id": id,
                    "text": text
                })
            }).collect::<Vec<_>>()
        });

        let response: EmbedResponse = self.invoke(&request).await?;

        // Reorder vectors to match input order
        let mut result = Vec::with_capacity(texts.len());
        for (expected_id, _) in &inputs {
            let vector = response
                .vectors
                .iter()
                .find(|v| &v.input_id == expected_id)
                .map(|v| v.vector.clone())
                .ok_or_else(|| anyhow::anyhow!("missing vector for input: {expected_id}"))?;
            result.push(vector);
        }

        Ok(result)
    }

    /// Invoke the plugin with a JSON request and parse the response.
    async fn invoke<R: serde::de::DeserializeOwned>(&self, request: &impl Serialize) -> Result<R> {
        let request_json = serde_json::to_string(request)?;

        let mut child = Command::new(&self.config.exec_path)
            .args(&self.config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to spawn embedding plugin: {}", self.config.exec_path))?;

        let mut stdin = child.stdin.take().expect("stdin configured");
        stdin.write_all(request_json.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.shutdown().await?;

        let output = child.wait_with_output().await
            .with_context(|| "failed to wait for embedding plugin")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "embedding plugin exited with status {}: {}",
                output.status,
                stderr.trim()
            );
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let trimmed = stdout.trim();

        // Check for error response
        if let Ok(error) = serde_json::from_str::<PluginError>(trimmed) {
            bail!(
                "embedding plugin error ({}): {}",
                error.error.code,
                error.error.message
            );
        }

        serde_json::from_str(trimmed)
            .with_context(|| format!("failed to parse embedding plugin response: {trimmed}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_embed_request() {
        let request = serde_json::json!({
            "op": "embed",
            "space_id": "test:model:768:v1",
            "inputs": [
                {"input_id": "msg_1", "text": "hello world"}
            ]
        });
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"op\":\"embed\""));
        assert!(json.contains("\"space_id\":\"test:model:768:v1\""));
    }

    #[test]
    fn parses_embed_response() {
        let json = r#"{
            "space_id": "test:model:768:v1",
            "model_id": "test-model",
            "vectors": [
                {"input_id": "msg_1", "vector": [0.1, 0.2, 0.3]}
            ]
        }"#;
        let response: EmbedResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.space_id, "test:model:768:v1");
        assert_eq!(response.vectors.len(), 1);
        assert_eq!(response.vectors[0].vector, vec![0.1, 0.2, 0.3]);
    }
}
