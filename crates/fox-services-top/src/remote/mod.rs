//! Fox-services cluster monitoring for the Nodes and Docker tabs.
//!
//! The composition root polls the statically configured nodes' HTTP APIs
//! (`/api/hardware/status`, `/api/docker/stats`, `/api/docker/logs/overview`,
//! `/api/mesh/peers`) on a background thread ([`ClusterMonitor`]) and hands
//! the Manager a read-only [`ClusterSource`]. Networking lives only here:
//! collectors, the Manager and the Viewer never touch sockets.

mod client;
mod http;
mod monitor;

pub use client::{FoxServicesClient, NodePoll};
pub use http::{HttpError, get_json};
pub use monitor::ClusterMonitor;
