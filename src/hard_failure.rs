use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tracing::{error, info, warn};

use crate::config::HardFailureHookConfig;

#[derive(Debug, Clone, Serialize)]
pub struct HardFailureEvent {
    pub kind: String,
    pub component: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    pub message: String,
    pub metadata: Value,
}

impl HardFailureEvent {
    pub fn new(
        kind: impl Into<String>,
        component: impl Into<String>,
        target: Option<String>,
        message: impl Into<String>,
        metadata: Value,
    ) -> Self {
        Self {
            kind: kind.into(),
            component: component.into(),
            target,
            message: message.into(),
            metadata,
        }
    }

    fn dedupe_key(&self) -> String {
        format!(
            "{}:{}:{}",
            self.kind,
            self.component,
            self.target.as_deref().unwrap_or("")
        )
    }
}

#[derive(Clone)]
pub struct HardFailureHooks {
    hooks: Arc<Vec<HardFailureHookConfig>>,
    last_sent: Arc<Mutex<HashMap<String, Instant>>>,
}

impl HardFailureHooks {
    pub fn new(hooks: Vec<HardFailureHookConfig>) -> Self {
        Self {
            hooks: Arc::new(hooks.into_iter().filter(|hook| hook.enabled).collect()),
            last_sent: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    pub async fn fire(&self, event: HardFailureEvent) {
        if self.is_empty() {
            return;
        }

        let payload = match serde_json::to_string(&event) {
            Ok(payload) => payload,
            Err(err) => {
                error!(component = "hard_failure_hook", event = "hard_failure_hook_payload_failed", error = %err, "failed to serialize hard failure payload");
                return;
            }
        };

        for hook in self.hooks.iter() {
            if self.throttled(hook, &event) {
                info!(
                    component = "hard_failure_hook",
                    event = "hard_failure_hook_throttled",
                    hook_key = %hook.key,
                    failure_kind = %event.kind,
                    failure_component = %event.component,
                    failure_target = event.target.as_deref().unwrap_or(""),
                    "hard failure hook suppressed by throttle"
                );
                continue;
            }

            if let Err(err) = run_hook(hook, &event, &payload).await {
                warn!(
                    component = "hard_failure_hook",
                    event = "hard_failure_hook_failed",
                    hook_key = %hook.key,
                    failure_kind = %event.kind,
                    failure_component = %event.component,
                    failure_target = event.target.as_deref().unwrap_or(""),
                    error = %err,
                    "hard failure hook failed"
                );
            }
        }
    }

    fn throttled(&self, hook: &HardFailureHookConfig, event: &HardFailureEvent) -> bool {
        let throttle = Duration::from_millis(hook.throttle_ms);
        if throttle.is_zero() {
            return false;
        }

        let key = format!("{}:{}", hook.key, event.dedupe_key());
        let now = Instant::now();
        let mut last_sent = self
            .last_sent
            .lock()
            .expect("hard failure hook lock poisoned");
        if last_sent
            .get(&key)
            .is_some_and(|last| now.duration_since(*last) < throttle)
        {
            return true;
        }
        last_sent.insert(key, now);
        false
    }
}

async fn run_hook(
    hook: &HardFailureHookConfig,
    event: &HardFailureEvent,
    payload: &str,
) -> Result<()> {
    let mut command = Command::new(render_template(&hook.command, event, payload));
    command
        .args(
            hook.args
                .iter()
                .map(|arg| render_template(arg, event, payload)),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    for (key, value) in &hook.env {
        command.env(key, render_template(value, event, payload));
    }

    let mut child = command.spawn().context("spawn hard failure hook")?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(render_template(&hook.template, event, payload).as_bytes())
            .await
            .context("write hard failure hook payload")?;
    }

    let timeout = Duration::from_millis(hook.timeout_ms);
    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(status) => status.context("wait for hard failure hook")?,
        Err(_) => {
            let _ = child.kill().await;
            warn!(
                component = "hard_failure_hook",
                event = "hard_failure_hook_timeout",
                hook_key = %hook.key,
                timeout_ms = hook.timeout_ms,
                "hard failure hook timed out"
            );
            return Ok(());
        }
    };

    info!(
        component = "hard_failure_hook",
        event = "hard_failure_hook_exited",
        hook_key = %hook.key,
        exit_code = status.code(),
        success = status.success(),
        "hard failure hook exited"
    );
    Ok(())
}

fn render_template(template: &str, event: &HardFailureEvent, payload: &str) -> String {
    template
        .replace("{{payload}}", payload)
        .replace("{{kind}}", &event.kind)
        .replace("{{component}}", &event.component)
        .replace("{{target}}", event.target.as_deref().unwrap_or(""))
        .replace("{{message}}", &event.message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn payload_for_test() -> Value {
        json!({"test": true})
    }

    #[tokio::test]
    async fn hook_runs_arbitrary_command_with_payload_and_env() {
        let dir = TempDir::new().unwrap();
        let output = dir.path().join("hook-output.json");
        let hook = HardFailureHookConfig {
            key: "capture".to_string(),
            command: "/bin/sh".to_string(),
            args: vec![
                "-c".to_string(),
                format!("cat > \"$OUT\"; printf '%s' \"$HOOK_TARGET\" >> \"$OUT.env\""),
            ],
            env: HashMap::from([
                ("OUT".to_string(), output.display().to_string()),
                ("HOOK_TARGET".to_string(), "{{target}}".to_string()),
            ]),
            template: "{{payload}}".to_string(),
            timeout_ms: 5_000,
            throttle_ms: 0,
            enabled: true,
        };
        let hooks = HardFailureHooks::new(vec![hook]);

        hooks
            .fire(HardFailureEvent::new(
                "runtime",
                "subspace",
                Some("server-a".to_string()),
                "failed",
                payload_for_test(),
            ))
            .await;

        let written = std::fs::read_to_string(&output).unwrap();
        let parsed: Value = serde_json::from_str(&written).unwrap();
        assert_eq!(parsed["kind"], "runtime");
        assert_eq!(parsed["target"], "server-a");
        assert_eq!(
            std::fs::read_to_string(output.with_extension("json.env")).unwrap(),
            "server-a"
        );
    }

    #[tokio::test]
    async fn hook_dedupes_within_throttle_window() {
        let dir = TempDir::new().unwrap();
        let output = dir.path().join("count");
        let hook = HardFailureHookConfig {
            key: "capture".to_string(),
            command: "/bin/sh".to_string(),
            args: vec![
                "-c".to_string(),
                format!("printf x >> '{}'", output.display()),
            ],
            env: HashMap::new(),
            template: "{{payload}}".to_string(),
            timeout_ms: 5_000,
            throttle_ms: 60_000,
            enabled: true,
        };
        let hooks = HardFailureHooks::new(vec![hook]);
        let event = HardFailureEvent::new("daemon", "gateway", None, "failed", payload_for_test());

        hooks.fire(event.clone()).await;
        hooks.fire(event).await;

        assert_eq!(std::fs::read_to_string(output).unwrap(), "x");
    }
}
