//! HCP — Human Coordination Plane.
//!
//! Agents submit coordination requests (approvals, clarifications, escalations) over HTTP;
//! humans answer them in Slack; every step is logged to an Obsidian vault.

pub mod api;
pub mod app;
pub mod client;
pub mod config;
pub mod domain;
pub mod engine;
pub mod hook;
pub mod logging;
pub mod mcp;
pub mod slack;
pub mod store;
pub mod vault;
