//! Runtime configuration, read from the environment (optionally via an env file).

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};

pub const DEFAULT_SLACK_API_BASE: &str = "https://slack.com/api";

/// Deliberately not `Debug`: it holds the database URL, which carries a password.
#[derive(Clone)]
pub struct Config {
    pub bind: IpAddr,
    pub port: u16,
    pub database_url: String,
    pub db_max_connections: u32,
    pub vault: Option<VaultConfig>,
    pub slack: Option<SlackConfig>,
    /// How often the scheduler looks for expired and undelivered requests.
    pub poll_interval: Duration,
    /// How long a request may sit unrouted before the scheduler retries delivery.
    pub routing_retry_after: Duration,
}

#[derive(Debug, Clone)]
pub struct VaultConfig {
    /// The Obsidian vault directory itself.
    pub path: PathBuf,
    /// Folder inside the vault that HCP owns.
    pub folder: String,
}

impl VaultConfig {
    pub fn root(&self) -> PathBuf {
        self.path.join(&self.folder)
    }
}

#[derive(Clone)]
pub struct SlackConfig {
    pub bot_token: String,
    /// App-level token (`xapp-…`). When set, interactions arrive over Socket Mode.
    pub app_token: Option<String>,
    /// When set, `POST /slack/interactions` is served and every request is signature-checked.
    pub signing_secret: Option<String>,
    /// Channel that receives every request needing a human.
    pub channel: String,
    /// Channel for one-way notifications. Unset: notifications are logged to the vault only,
    /// unless the request names a Slack channel itself.
    pub notify_channel: Option<String>,
    /// `responder_id` -> Slack user ID. A mapped responder is @-mentioned and is the only
    /// person allowed to answer.
    pub responders: HashMap<String, String>,
    pub api_base: String,
}

impl std::fmt::Debug for SlackConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlackConfig")
            .field("bot_token", &"<redacted>")
            .field("app_token", &self.app_token.as_ref().map(|_| "<redacted>"))
            .field("signing_secret", &self.signing_secret.as_ref().map(|_| "<redacted>"))
            .field("channel", &self.channel)
            .field("notify_channel", &self.notify_channel)
            .field("responders", &self.responders)
            .field("api_base", &self.api_base)
            .finish()
    }
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// Builds the configuration from any key lookup; empty values count as unset.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let get = |key: &str| lookup(key).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());

        let bind = match get("HCP_BIND") {
            Some(raw) => raw.parse().with_context(|| format!("HCP_BIND is not an IP address: {raw:?}"))?,
            None => IpAddr::V4(Ipv4Addr::LOCALHOST),
        };
        let port = parse_or(&get, "HCP_PORT", 3100u16)?;
        let database_url = database_url(&lookup)?;
        let db_max_connections = parse_or(&get, "HCP_DB_MAX_CONNECTIONS", 10u32)?;
        let poll_seconds = parse_or(&get, "HCP_TIMEOUT_POLL_SECONDS", 10u64)?;
        if poll_seconds == 0 {
            bail!("HCP_TIMEOUT_POLL_SECONDS must be at least 1");
        }

        let vault = get("HCP_OBSIDIAN_VAULT").map(|path| VaultConfig {
            path: expand_home(&path, get("HOME").as_deref()),
            folder: get("HCP_OBSIDIAN_FOLDER").unwrap_or_else(|| "HCP".to_string()),
        });
        if let Some(vault) = &vault {
            let folder = std::path::Path::new(&vault.folder);
            if folder.is_absolute() || folder.components().any(|c| c == std::path::Component::ParentDir) {
                bail!("HCP_OBSIDIAN_FOLDER must be a relative path inside the vault");
            }
        }

        let slack = match get("SLACK_BOT_TOKEN") {
            None => None,
            Some(bot_token) => {
                let app_token = get("SLACK_APP_TOKEN");
                let signing_secret = get("SLACK_SIGNING_SECRET");
                if app_token.is_none() && signing_secret.is_none() {
                    bail!(
                        "SLACK_BOT_TOKEN is set but Slack has no way to deliver button clicks: set \
                         SLACK_APP_TOKEN (Socket Mode, no public URL needed) or SLACK_SIGNING_SECRET \
                         (HTTP interactivity at /slack/interactions)"
                    );
                }
                let channel = get("HCP_SLACK_CHANNEL").context(
                    "SLACK_BOT_TOKEN is set but HCP_SLACK_CHANNEL is not: set it to the channel ID \
                     (e.g. C0123456789) that should receive approval requests",
                )?;
                Some(SlackConfig {
                    bot_token,
                    app_token,
                    signing_secret,
                    channel,
                    notify_channel: get("HCP_SLACK_NOTIFY_CHANNEL"),
                    responders: parse_responders(get("HCP_SLACK_RESPONDERS").as_deref())?,
                    api_base: get("SLACK_API_BASE_URL")
                        .unwrap_or_else(|| DEFAULT_SLACK_API_BASE.to_string())
                        .trim_end_matches('/')
                        .to_string(),
                })
            }
        };

        Ok(Config {
            bind,
            port,
            database_url,
            db_max_connections,
            vault,
            slack,
            poll_interval: Duration::from_secs(poll_seconds),
            routing_retry_after: Duration::from_secs(30),
        })
    }
}

/// The one setting every command needs.
pub fn database_url(lookup: impl Fn(&str) -> Option<String>) -> Result<String> {
    lookup("DATABASE_URL")
        .map(|url| url.trim().to_string())
        .filter(|url| !url.is_empty())
        .context("DATABASE_URL is not set (a Postgres connection string, e.g. postgresql://localhost:5432/hcp)")
}

fn parse_or<T>(get: &impl Fn(&str) -> Option<String>, key: &str, default: T) -> Result<T>
where
    T: std::str::FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    match get(key) {
        Some(raw) => raw.parse().with_context(|| format!("{key} has an invalid value: {raw:?}")),
        None => Ok(default),
    }
}

/// Parses `responder=U123,other=U456`.
fn parse_responders(raw: Option<&str>) -> Result<HashMap<String, String>> {
    let mut map = HashMap::new();
    for pair in raw.unwrap_or_default().split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let (responder, user) = pair
            .split_once('=')
            .with_context(|| format!("HCP_SLACK_RESPONDERS entry {pair:?} is not responder=SLACK_USER_ID"))?;
        let (responder, user) = (responder.trim(), user.trim());
        if responder.is_empty() || user.is_empty() {
            bail!("HCP_SLACK_RESPONDERS entry {pair:?} is not responder=SLACK_USER_ID");
        }
        map.insert(responder.to_string(), user.to_string());
    }
    Ok(map)
}

fn expand_home(path: &str, home: Option<&str>) -> PathBuf {
    match (path.strip_prefix("~/"), home) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(pairs: &[(&str, &str)]) -> Result<Config> {
        let map: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        Config::from_lookup(|key| map.get(key).cloned())
    }

    #[test]
    fn defaults_bind_to_loopback() {
        let cfg = config(&[("DATABASE_URL", "postgresql://localhost/hcp")]).unwrap();
        assert_eq!(cfg.bind, IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_eq!(cfg.port, 3100);
        assert!(cfg.slack.is_none());
        assert!(cfg.vault.is_none());
    }

    #[test]
    fn database_url_is_required() {
        assert!(config(&[]).is_err());
    }

    #[test]
    fn slack_needs_a_channel_and_an_interaction_transport() {
        let base = [("DATABASE_URL", "postgresql://localhost/hcp"), ("SLACK_BOT_TOKEN", "xoxb-1")];
        assert!(config(&base).is_err());

        let mut no_channel = base.to_vec();
        no_channel.push(("SLACK_APP_TOKEN", "xapp-1"));
        assert!(config(&no_channel).is_err());

        let mut complete = no_channel.clone();
        complete.push(("HCP_SLACK_CHANNEL", "C123"));
        complete.push(("HCP_SLACK_RESPONDERS", "isaac=U1, ops-lead = U2"));
        let slack = config(&complete).unwrap().slack.unwrap();
        assert_eq!(slack.channel, "C123");
        assert_eq!(slack.responders["ops-lead"], "U2");
        assert_eq!(slack.api_base, DEFAULT_SLACK_API_BASE);
    }

    #[test]
    fn vault_path_expands_home_and_folder_must_stay_inside() {
        let cfg = config(&[
            ("DATABASE_URL", "postgresql://localhost/hcp"),
            ("HOME", "/Users/someone"),
            ("HCP_OBSIDIAN_VAULT", "~/Vaults/Main"),
        ])
        .unwrap();
        let vault = cfg.vault.unwrap();
        assert_eq!(vault.root(), PathBuf::from("/Users/someone/Vaults/Main/HCP"));

        assert!(
            config(&[
                ("DATABASE_URL", "postgresql://localhost/hcp"),
                ("HCP_OBSIDIAN_VAULT", "/tmp/vault"),
                ("HCP_OBSIDIAN_FOLDER", "../elsewhere"),
            ])
            .is_err()
        );
    }

    #[test]
    fn secrets_are_not_printed_by_debug() {
        let cfg = config(&[
            ("DATABASE_URL", "postgresql://localhost/hcp"),
            ("SLACK_BOT_TOKEN", "xoxb-secret"),
            ("SLACK_SIGNING_SECRET", "shhh"),
            ("HCP_SLACK_CHANNEL", "C123"),
        ])
        .unwrap();
        let printed = format!("{:?}", cfg.slack.unwrap());
        assert!(!printed.contains("xoxb-secret") && !printed.contains("shhh"));
    }
}
