//! What the server knows about its connectors right now: whether Slack's Socket Mode is
//! connected, when the Slack API and the vault last worked, and the last error from each.
//! Reported by `GET /v1/status` for the desktop dashboard.

use std::sync::Mutex;

use chrono::{DateTime, Utc};
use serde::Serialize;

/// The health of one connector. Cheap to update from any task.
#[derive(Debug, Default)]
pub struct Tracker {
    inner: Mutex<Snapshot>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Snapshot {
    /// A connector-specific state, e.g. `connected` or `reconnecting` for Socket Mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<&'static str>,
    #[serde(with = "crate::domain::timestamp::option")]
    pub since: Option<DateTime<Utc>>,
    #[serde(with = "crate::domain::timestamp::option")]
    pub last_ok_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    #[serde(with = "crate::domain::timestamp::option")]
    pub last_error_at: Option<DateTime<Utc>>,
}

impl Tracker {
    fn update(&self, change: impl FnOnce(&mut Snapshot)) {
        // A poisoned lock only means another task panicked mid-update; the data is still usable.
        let mut snapshot = self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        change(&mut snapshot);
    }

    pub fn ok(&self) {
        self.update(|s| s.last_ok_at = Some(Utc::now()));
    }

    pub fn fail(&self, error: impl std::fmt::Display) {
        self.update(|s| {
            s.last_error = Some(error.to_string());
            s.last_error_at = Some(Utc::now());
        });
    }

    /// Moves to a new state; `since` only changes when the state does.
    pub fn set_state(&self, state: &'static str) {
        self.update(|s| {
            if s.state != Some(state) {
                s.state = Some(state);
                s.since = Some(Utc::now());
            }
        });
    }

    pub fn snapshot(&self) -> Snapshot {
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_successes_failures_and_state_changes() {
        let tracker = Tracker::default();
        assert_eq!(tracker.snapshot(), Snapshot::default());

        tracker.set_state("connecting");
        let since = tracker.snapshot().since;
        tracker.set_state("connecting");
        assert_eq!(tracker.snapshot().since, since, "same state keeps its start time");

        tracker.fail("socket closed");
        tracker.ok();
        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.state, Some("connecting"));
        assert_eq!(snapshot.last_error.as_deref(), Some("socket closed"));
        assert!(snapshot.last_ok_at.is_some() && snapshot.last_error_at.is_some());

        let json = serde_json::to_value(&snapshot).unwrap();
        assert!(json["last_ok_at"].as_str().unwrap().ends_with('Z'));
    }
}
