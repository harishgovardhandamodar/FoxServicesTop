//! Fox-services cluster monitoring for the Nodes, Fox-Services and Tokens tabs.
//!
//! The composition root polls the statically configured nodes' HTTP APIs
//! (`/api/hardware/status`, `/api/llm/live-map`, `/api/usage/history`,
//! `/api/ollama/stats`, `/api/mesh/peers`) on a background thread
//! ([`ClusterMonitor`]) and hands the Manager a read-only [`ClusterSource`].
//! Networking lives only here: collectors, the Manager and the Viewer
//! never touch sockets.

mod client;
mod http;
mod monitor;
mod services;
mod tokens;

pub use client::{FoxServicesClient, NodePoll};
pub use http::{HttpError, get_json};
pub use monitor::ClusterMonitor;
pub use services::{NodeServices, ServicePoll, merge as merge_services};
pub use tokens::{NodeTokens, TokenPoll, merge as merge_tokens};
