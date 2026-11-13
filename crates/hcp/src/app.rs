//! Wires the pieces together: database, vault, Slack, scheduler and HTTP server.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::api::{self, AppState};
use crate::config::Config;
use crate::engine::Engine;
use crate::slack::{self, SlackClient};
use crate::store::Store;
use crate::vault::{self, Vault};

/// TLS for Postgres, Slack's Web API and Socket Mode all goes through rustls; pick its
/// crypto provider once, explicitly, rather than relying on feature unification.
pub fn install_crypto_provider() {
    // An error only means a provider is already installed.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

/// A running HCP server.
pub struct App {
    pub addr: SocketAddr,
    pub engine: Engine,
    shutdown: CancellationToken,
    server: JoinHandle<()>,
    workers: Vec<JoinHandle<()>>,
}

impl App {
    /// Starts everything and returns once the server is accepting connections. `vault`
    /// must be the vault named in `config`, already opened (the caller usually needs it
    /// first, to send the service log there).
    pub async fn start(config: Config, vault: Option<Arc<Vault>>) -> Result<App> {
        install_crypto_provider();

        let store = Store::connect(&config.database_url, config.db_max_connections, vault.is_some())
            .await
            .context("cannot connect to the database")?;
        store.ensure_schema().await.context("cannot prepare the database schema")?;

        let slack = config.slack.clone().map(SlackClient::new).transpose().context("cannot build the Slack client")?;
        match &slack {
            Some(client) => match client.auth_test().await {
                Ok(bot) => tracing::info!(bot, channel = %client.config().channel, "Slack connected"),
                Err(err) => tracing::warn!(error = %err, "Slack credentials could not be verified; will keep trying"),
            },
            None => tracing::warn!(
                "Slack is not configured: requests can only be answered through POST /v1/requests/{{id}}/respond"
            ),
        }

        let engine = Engine::new(store.clone(), slack.clone(), config.routing_retry_after);
        let shutdown = CancellationToken::new();
        let mut workers = Vec::new();

        workers.push(tokio::spawn(engine.clone().run_scheduler(config.poll_interval, shutdown.clone())));
        if let Some(vault) = vault {
            workers.push(tokio::spawn(vault::run(vault, store, engine.vault_wake(), shutdown.clone())));
        }
        if let Some(client) = slack.filter(|client| client.config().app_token.is_some()) {
            workers.push(tokio::spawn(slack::socket::run(engine.clone(), client, shutdown.clone())));
        }

        let listener = TcpListener::bind((config.bind, config.port))
            .await
            .with_context(|| format!("cannot listen on {}:{}", config.bind, config.port))?;
        let addr = listener.local_addr()?;
        let router = api::router(AppState::new(engine.clone(), shutdown.clone()));
        let stop = shutdown.clone();
        let server = tokio::spawn(async move {
            let served = axum::serve(listener, router).with_graceful_shutdown(stop.cancelled_owned()).await;
            if let Err(err) = served {
                tracing::error!(error = %err, "HTTP server stopped unexpectedly");
            }
        });

        tracing::info!(%addr, "HCP listening");
        Ok(App { addr, engine, shutdown, server, workers })
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Stops accepting work, lets in-flight work finish, and flushes the vault outbox.
    pub async fn shutdown(self) {
        self.shutdown.cancel();
        let work = async {
            let _ = self.server.await;
            for worker in self.workers {
                let _ = worker.await;
            }
        };
        if tokio::time::timeout(Duration::from_secs(15), work).await.is_err() {
            tracing::warn!("shutdown timed out waiting for background work");
        }
        self.engine.store().close().await;
        tracing::info!("HCP stopped");
    }
}
