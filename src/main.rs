mod attention;
mod config;
mod gateway;
mod ipc;
mod launchd;
mod logging;
mod retry;
mod runtime_store;
mod state_lock;
mod subspace;
mod supervisor;

use std::ffi::OsStr;
use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use reqwest::Client;

use crate::config::{
    Config, StoredConfig, canonicalize_base_url, default_config_path, default_registration_name,
    derive_app_paths, derive_server_key,
};
use crate::ipc::client::send_via_socket;
use crate::state_lock::StateLock;
use crate::subspace::auth::acquire_session_token;
use crate::subspace::identity::SubspaceSessionRecord;
use crate::supervisor::run_supervisor;

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
    let config = Config::load(args.config)?;
    let response = send_via_socket(
        &config.paths.socket_path,
        &args.text,
        args.idempotency_key.as_deref(),
        args.server.as_deref(),
    )
    .await?;
    println!("{}", serde_json::to_string_pretty(&response)?);
    Ok(())
}

async fn setup(args: SetupArgs) -> Result<()> {
    let config_path = default_config_path();
    let paths = derive_app_paths(config_path.clone(), None)?;
    let _lock = StateLock::try_acquire(&paths.state_lock_path)?;

    let base_url = canonicalize_base_url(&args.url)?;
    let server_key = derive_server_key(&base_url)?;
    let server_dir = paths.root.join("servers").join(&server_key);
    let session_path = server_dir.join("subspace-session.json");

    let requested_name = match args.name {
        Some(name) => validate_registration_name(name)?,
        None => prompt_with_default("Subspace registration name", &default_registration_name())?,
    };

    let mut stored = StoredConfig::load_or_default(&config_path)?;
    let session_exists = SubspaceSessionRecord::load(&session_path)?.is_some();
    if !session_exists {
        let http = Client::builder().build()?;
        let mut session = SubspaceSessionRecord::new("openclaw", &requested_name);
        let token = acquire_session_token(&http, &base_url, &session).await?;
        session.update_session_token(token);
        session.persist(&session_path)?;
    }

    stored.upsert_server(base_url.clone(), requested_name.clone());
    stored.save(&config_path)?;

    println!("configured Subspace server");
    println!("base_url: {base_url}");
    println!("server_key: {server_key}");
    println!("session_path: {}", session_path.display());
    println!("config_path: {}", config_path.display());
    if session_exists {
        println!("identity: preserved existing session");
    } else {
        let session = SubspaceSessionRecord::load(&session_path)?
            .context("session file missing after setup")?;
        println!("agent_id: {}", session.public_key);
    }
    Ok(())
}

fn validate_registration_name(name: String) -> Result<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        bail!("registration name must not be empty");
    }
    Ok(trimmed.to_string())
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
