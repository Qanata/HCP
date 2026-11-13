use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use hcp::app::{App, install_crypto_provider};
use hcp::client::AgentEnv;
use hcp::config::Config;
use hcp::store::Store;
use hcp::vault::Vault;

#[derive(Parser)]
#[command(name = "hcp", version, about = "Human Coordination Plane — routes agent requests to humans in Slack")]
struct Cli {
    /// Load configuration from this env file instead of ./.env
    #[arg(long, global = true, value_name = "PATH")]
    env_file: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the server
    Serve,
    /// Manage API keys
    #[command(subcommand)]
    Keys(KeysCommand),
    /// Run the MCP server on stdin/stdout, for agent harnesses (reads HCP_API_KEY, HCP_BASE_URL)
    Mcp,
    /// Handle a Claude Code hook event read from stdin (reads HCP_API_KEY, HCP_BASE_URL)
    Hook,
}

#[derive(Subcommand)]
enum KeysCommand {
    /// Create an API key. The secret is printed once and cannot be recovered.
    Create {
        /// The agent this key identifies, e.g. claude-code
        agent_id: String,
        /// A human-readable description
        #[arg(long, default_value = "API key")]
        label: String,
        /// Extra permission: `respond` (may answer requests through the API) or
        /// `admin` (may read every agent's requests). Agents need neither.
        #[arg(long = "scope", value_parser = ["respond", "admin"])]
        scopes: Vec<String>,
    },
    /// List API keys (never the secrets)
    List,
    /// Revoke an API key
    Revoke { key_id: String },
}

fn load_env(file: Option<&PathBuf>, implicit: bool) -> Result<()> {
    match file {
        Some(path) => {
            dotenvy::from_path(path).with_context(|| format!("cannot read env file {}", path.display()))?;
        }
        // A missing ./.env is fine; a malformed one is not.
        None if implicit => match dotenvy::dotenv() {
            Ok(_) => {}
            Err(err) if err.not_found() => {}
            Err(err) => return Err(err).context("cannot read ./.env"),
        },
        None => {}
    }
    Ok(())
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => {
            let _ = err.print();
            // Not clap's usual 2: when this binary is a Claude Code hook, exit code 2 means
            // "block the tool call" (or, on Stop, "keep working"). A typo must not say that.
            return if err.use_stderr() { ExitCode::from(64) } else { ExitCode::SUCCESS };
        }
    };
    match run(cli) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("Error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<ExitCode> {
    // `mcp` and `hook` run inside whatever project the agent is working on. They must not
    // pick up that project's ./.env: the agent being supervised could write one.
    let agent_side = matches!(cli.command, Command::Mcp | Command::Hook);
    let env_loaded = load_env(cli.env_file.as_ref(), !agent_side);
    if let Command::Hook = cli.command {
        return Ok(hcp::hook::main(env_loaded.err().map(|err| format!("{err:#}"))));
    }
    env_loaded?;

    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    match cli.command {
        Command::Serve => runtime.block_on(serve(Config::from_env()?))?,
        // Key management needs only the database, not a complete Slack and vault setup.
        Command::Keys(command) => {
            let database_url = hcp::config::database_url(|key| std::env::var(key).ok())?;
            runtime.block_on(keys(&database_url, command))?;
        }
        Command::Mcp => {
            install_crypto_provider();
            runtime.block_on(hcp::mcp::serve(AgentEnv::from_env()))?;
        }
        Command::Hook => unreachable!("handled above"),
    }
    Ok(ExitCode::SUCCESS)
}

async fn serve(config: Config) -> Result<()> {
    let vault_config = config
        .vault
        .clone()
        .context("HCP_OBSIDIAN_VAULT is not set: point it at the Obsidian vault folder that should hold the logs")?;
    let vault = Arc::new(Vault::open(&vault_config)?);

    let flush = hcp::logging::init(vault.clone());

    tracing::info!(vault = %vault.root().display(), "logging to the Obsidian vault");
    if !Vault::looks_like_obsidian(&vault_config) {
        tracing::warn!(
            path = %vault_config.path.display(),
            "this folder has no .obsidian directory; check that it is the vault and not a parent or subfolder"
        );
    }

    let result = async {
        let app = App::start(config, Some(vault)).await?;
        wait_for_signal().await?;
        tracing::info!("shutting down");
        app.shutdown().await;
        anyhow::Ok(())
    }
    .await;
    if let Err(err) = &result {
        tracing::error!(error = %format!("{err:#}"), "HCP failed");
    }
    flush.flush();
    result
}

async fn wait_for_signal() -> Result<()> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut terminate = signal(SignalKind::terminate())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result?,
        _ = terminate.recv() => {}
    }
    Ok(())
}

async fn keys(database_url: &str, command: KeysCommand) -> Result<()> {
    install_crypto_provider();
    let store = Store::connect(database_url, 2, false).await.context("cannot connect to the database")?;
    store.ensure_schema().await?;

    match command {
        KeysCommand::Create { agent_id, label, scopes } => {
            if agent_id.trim().is_empty() {
                bail!("agent_id must not be empty");
            }
            let (key, secret) = store.create_api_key(&agent_id, &label, &scopes).await?;
            println!("API key created for agent \"{}\":", key.agent_id);
            println!("  Key ID:  {}", key.key_id);
            println!("  Scopes:  {}", if key.scopes.is_empty() { "(none)".to_string() } else { key.scopes.join(", ") });
            println!("  API Key: {secret}");
            println!("Store this key securely — it cannot be retrieved again.");
        }
        KeysCommand::List => {
            let keys = store.list_api_keys().await?;
            if keys.is_empty() {
                println!("No API keys.");
            }
            for key in keys {
                let status = match key.revoked_at {
                    Some(at) => format!("revoked {}", at.format("%Y-%m-%d")),
                    None => "active".to_string(),
                };
                println!(
                    "{}  {:<20} {:<8} [{}]  {}  (created {})",
                    key.key_id,
                    key.agent_id,
                    status,
                    key.scopes.join(","),
                    key.label,
                    key.created_at.format("%Y-%m-%d"),
                );
            }
        }
        KeysCommand::Revoke { key_id } => {
            if !store.revoke_api_key(&key_id).await? {
                bail!("no active API key with ID {key_id}");
            }
            println!("Revoked {key_id}.");
        }
    }
    store.close().await;
    Ok(())
}
