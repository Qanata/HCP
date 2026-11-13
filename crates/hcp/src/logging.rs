//! Process-wide logging: human-readable lines on stderr, and HCP's own events in the vault.

use std::io::IsTerminal;
use std::sync::Arc;

use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::vault::{self, ServiceLogHandle, Vault};

/// Used unless `HCP_LOG` says otherwise. sqlx logs every statement and Postgres notice.
const DEFAULT_FILTER: &str = "info,sqlx=warn";

/// Installs the global subscriber. Call once, before anything logs.
///
/// There is exactly one filter and it is global, on purpose. With per-layer filters
/// (`Layer::with_filter`), tracing-subscriber 0.3 keeps per-thread state between asking
/// "is this enabled?" and recording the event. A dependency that only asks — sqlx does,
/// through the `log` bridge, for every Postgres NOTICE — leaves that state behind, and the
/// next event on the thread is silently dropped by every layer. The vault layer therefore
/// decides for itself what to keep (see [`vault::ServiceLogLayer`]).
pub fn init(vault: Arc<Vault>) -> ServiceLogHandle {
    let (service_log, handle) = vault::service_log(vault);
    let filter = EnvFilter::try_from_env("HCP_LOG").unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));
    let console =
        tracing_subscriber::fmt::layer().with_writer(std::io::stderr).with_ansi(std::io::stderr().is_terminal());
    tracing_subscriber::registry().with(filter).with(console).with(service_log).init();
    handle
}
