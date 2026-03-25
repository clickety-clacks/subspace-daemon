mod attention;
mod config;
mod gateway;
mod ipc;
mod launchd;
mod logging;
mod retry;
mod runtime_store;
mod setup;
mod state_lock;
mod subspace;
mod supervisor;

use std::ffi::OsStr;
use std::io::{self, Write};
use std::path::PathBuf;

use crate::config::{
    Config, StoredConfig, canonicalize_base_url, default_config_path, default_registration_name,
    derive_app_paths, derive_server_key,
};
use crate::ipc::client::{ClientSendRequest, send_via_socket, setup_via_socket};
use crate::setup::{SetupRequest, perform_setup};
use crate::state_lock::StateLock;
use crate::subspace::identity::{LoadedSessionRecord, load_session_record};
use crate::supervisor::run_supervisor;
use anyhow::{Result, bail};
use clap::{Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "subspace-daemon")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Parser, Debug)]
#[command(name = "subspace-send")]
struct SendCli {
    #[command(flatten)]
    args: SendArgs,
}

#[derive(Subcommand, Debug)]
enum Command {
    Serve(ServeArgs),
    Send(SendArgs),
    Setup(SetupArgs),
}

#[derive(Args, Debug)]
struct ServeArgs {
    #[arg(long, default_value = "~/.openclaw/subspace-daemon/config.json")]
    config: PathBuf,
}

#[derive(Args, Debug, Clone)]
struct SendArgs {
    #[arg(long, default_value = "~/.openclaw/subspace-daemon/config.json")]
    config: PathBuf,
    #[arg(long)]
    server: Option<String>,
    #[arg(long)]
    idempotency_key: Option<String>,
    text: String,
}

#[derive(Args, Debug, Clone)]
struct SetupArgs {
    url: String,
    #[arg(long)]
    name: Option<String>,
    #[arg(long)]
    identity: Option<String>,
}

fn argv0_mode() -> Option<&'static str> {
    let argv0 = std::env::args_os().next()?;
    let stem = PathBuf::from(argv0);
    let name = stem.file_name().and_then(OsStr::to_str)?;
    if name == "subspace-send" {
        Some("send")
    } else {
        None
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    if argv0_mode() == Some("send") {
        return send(SendCli::parse().args).await;
    }

    let cli = Cli::parse();
    match cli.command {
        Some(Command::Serve(args)) => serve(args).await,
        Some(Command::Send(args)) => send(args).await,
        Some(Command::Setup(args)) => setup(args).await,
        None => {
            serve(ServeArgs {
                config: default_config_path(),
            })
            .await
        }
    }
}

async fn serve(args: ServeArgs) -> Result<()> {
    let config = Config::load(args.config)?;
    let _guards = logging::init_logging(
        &config.paths.log_file,
        &config.logging.level,
        config.logging.json,
    )?;
    run_supervisor(config).await
}

async fn send(args: SendArgs) -> Result<()> {
    let server = match args.server.as_deref() {
        None => {
            bail!(
                "--server is required. Use --server <url> to target a specific server, or --server '*' to broadcast to all."
            );
        }
        Some("*") => None,
        Some(s) => Some(s),
    };
    let config = Config::load(args.config)?;
    let response = send_via_socket(
        &config.paths.socket_path,
        &ClientSendRequest {
            text: args.text,
            idempotency_key: args.idempotency_key,
            server: server.map(ToOwned::to_owned),
            ..ClientSendRequest::default()
        },
    )
    .await?;
    println!("{}", serde_json::to_string_pretty(&response)?);
    Ok(())
}

async fn setup(args: SetupArgs) -> Result<()> {
    let config_path = default_config_path();
    let base_url = canonicalize_base_url(&args.url)?;
    let existing_name = existing_setup_registration_name(&config_path, &base_url)?;
    let request = SetupRequest {
        url: args.url,
        name: match args.name {
            Some(name) => Some(validate_registration_name(name)?),
            None if existing_name.is_some() => None,
            None => Some(prompt_with_default(
                "Subspace registration name",
                &default_registration_name(),
            )?),
        },
        identity: args.identity,
    };
    let paths = derive_app_paths(config_path, None)?;
    let result = match StateLock::try_acquire(&paths.state_lock_path)? {
        Some(lock_guard) => {
            let result = perform_setup(request, None).await?;
            drop(lock_guard);
            result
        }
        None => setup_via_socket(&paths.socket_path, &request).await?,
    };
    print_setup_result(&result);
    Ok(())
}

fn validate_registration_name(name: String) -> Result<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        bail!("registration name must not be empty");
    }
    Ok(trimmed.to_string())
}

fn existing_setup_registration_name(config_path: &PathBuf, base_url: &str) -> Result<Option<String>> {
    let stored = StoredConfig::load_or_default(config_path)?;
    if let Some(name) = stored
        .servers
        .into_iter()
        .find(|server| {
            canonicalize_base_url(&server.base_url)
                .map(|value| value == base_url)
                .unwrap_or(false)
        })
        .and_then(|server| server.registration_name)
    {
        return Ok(Some(name));
    }

    let paths = derive_app_paths(config_path.clone(), None)?;
    let server_key = derive_server_key(base_url)?;
    let session_path = paths
        .root
        .join("servers")
        .join(server_key)
        .join("subspace-session.json");
    let existing_session = load_session_record(&session_path)?;
    Ok(match existing_session {
        Some(LoadedSessionRecord::Legacy(session)) => Some(session.registration_name),
        _ => None,
    })
}

fn prompt_with_default(label: &str, default: &str) -> Result<String> {
    print!("{label} [{default}]: ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(default.to_string());
    }
    validate_registration_name(trimmed.to_string())
}

fn print_setup_result(result: &crate::setup::SetupResult) {
    println!("configured Subspace server");
    println!("base_url: {}", result.base_url);
    println!("server_key: {}", result.server_key);
    println!("session_path: {}", result.session_path);
    println!("config_path: {}", result.config_path);
    if result.had_existing_session {
        println!("identity: preserved existing identity assignment");
    } else {
        println!("identity: {}", result.identity);
    }
    if result.applied_live {
        println!("live_apply: true");
    }
    println!("agent_id: {}", result.agent_id);
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use ed25519_dalek::SigningKey;
    use ed25519_dalek::pkcs8::EncodePrivateKey;
    use rand::rngs::OsRng;
    use serde_json::json;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn existing_setup_registration_name_prefers_config_entry() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("config.json");
        fs::write(
            &config_path,
            r#"{
              "servers": [
                {
                  "base_url": "https://subspace.example.com",
                  "registration_name": "heimdal",
                  "identity": "heimdal"
                }
              ]
            }"#,
        )
        .unwrap();

        let name =
            existing_setup_registration_name(&config_path, "https://subspace.example.com").unwrap();
        assert_eq!(name.as_deref(), Some("heimdal"));
    }

    #[test]
    fn existing_setup_registration_name_uses_legacy_session_when_config_missing() {
        let dir = tempdir().unwrap();
        let config_path = dir.path().join("config.json");
        fs::write(&config_path, "{}").unwrap();

        let paths = derive_app_paths(config_path.clone(), None).unwrap();
        let server_key = derive_server_key("https://subspace.example.com").unwrap();
        let session_path = paths
            .root
            .join("servers")
            .join(server_key)
            .join("subspace-session.json");
        fs::create_dir_all(session_path.parent().unwrap()).unwrap();
        let signing_key = SigningKey::generate(&mut OsRng);
        fs::write(
            &session_path,
            serde_json::to_vec_pretty(&json!({
                "version": 1,
                "public_key": "agent",
                "private_key": URL_SAFE_NO_PAD.encode(signing_key.to_pkcs8_der().unwrap().as_bytes()),
                "owner": "openclaw",
                "name": "legacy-name",
                "session_token": "token"
            }))
            .unwrap(),
        )
        .unwrap();

        let name =
            existing_setup_registration_name(&config_path, "https://subspace.example.com").unwrap();
        assert_eq!(name.as_deref(), Some("legacy-name"));
    }
}
