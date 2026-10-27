//! One fox-services node over HTTP: the polled endpoints and the tolerant
//! mapping onto the core view types.
//!
//! The mapping never fails on unexpected JSON: unknown fields are ignored,
//! missing numbers stay `None`, over-long lists and lines are truncated.
//! Only a transport error or an unparsable document marks a node offline.

use std::time::Duration;

use fox_services_top_core::config::RemoteNodeConfig;
use fox_services_top_core::view::{ClusterNodeView, MeshPeerView};
use serde_json::Value;

use super::http::{HttpError, get_json};

/// Upper bounds of one node's snapshot (a projection stays bounded).
pub const MAX_PROJECTS_PER_NODE: usize = 64;
pub const MAX_CONTAINERS_PER_PROJECT: usize = 64;
pub const MAX_PEERS_PER_NODE: usize = 64;

/// What one poll of one node produced.
#[derive(Debug, Clone, Default)]
pub struct NodePoll {
    pub node: ClusterNodeView,
    pub peers: Vec<MeshPeerView>,
    /// Which app uses which LLM model, over the window.
    pub services: Option<super::services::NodeServices>,
    /// Token flow of the window, when the config asks for it.
    pub tokens: Option<super::tokens::NodeTokens>,
}

/// A node's fox-services API.
pub struct FoxServicesClient {
    node: RemoteNodeConfig,
    timeout: Duration,
    /// Service window to read the live map over.
    services: super::services::ServicePoll,
    /// Token window to aggregate, `None` when the config turns it off.
    tokens: Option<super::tokens::TokenPoll>,
}

impl FoxServicesClient {
    pub fn new(node: RemoteNodeConfig, timeout: Duration) -> Self {
        Self {
            node,
            timeout,
            services: super::services::ServicePoll::new(24),
            tokens: None,
        }
    }

    /// Reads the live map over `window_hours` instead of the default day.
    pub fn with_services(mut self, window_hours: u32) -> Self {
        self.services = super::services::ServicePoll::new(window_hours);
        self
    }

    /// Also aggregates token ingress/egress over `window_hours`.
    pub fn with_tokens(mut self, window_hours: u32, usage_limit: u32) -> Self {
        self.tokens = Some(super::tokens::TokenPoll::new(window_hours, usage_limit));
        self
    }

    fn get(&self, path: &str) -> Result<Value, HttpError> {
        get_json(&self.node.base_url, path, self.timeout)
    }

    /// Polls everything one node offers for the three tabs. Every endpoint
    /// answers from memory or the database; the per-container shell-outs
    /// (`/api/docker/stats`, log tails) are deliberately not polled, so one
    /// loaded node never stalls the next poll behind it (D-118).
    pub fn poll(&self, discover_mesh: bool) -> NodePoll {
        let mut poll = NodePoll {
            node: ClusterNodeView {
                name: self.node.name.clone(),
                base_url: self.node.base_url.clone(),
                online: true,
                ..ClusterNodeView::default()
            },
            ..NodePoll::default()
        };
        match self.get("/api/hardware/status") {
            Ok(hardware) => apply_hardware(&mut poll.node, &hardware),
            Err(error) => {
                poll.node.online = false;
                poll.node.error = Some(error.to_string());
                return poll;
            }
        }
        // Which app uses which model: the Fox-Services tab's own endpoint.
        if let Ok(live_map) = self.get(&self.services.live_map_path()) {
            poll.services = Some(self.services.apply(&live_map));
        }
        if let Some(tokens) = &self.tokens {
            // Either endpoint answering is enough: a node without Ollama
            // still shows app flow.
            let usage = self.get(&tokens.usage_path());
            let stats = self.get("/api/ollama/stats").ok();
            if let Ok(usage) = usage {
                poll.tokens = Some(tokens.apply(&usage, stats.as_ref()));
            }
        }
        // Container counts for the Nodes tab; the project list itself is
        // not shown anywhere, so only the totals are kept.
        if let Ok(projects) = self.get("/api/docker/projects") {
            apply_project_counts(&mut poll.node, &projects);
        }
        if discover_mesh && let Ok(peers) = self.get("/api/mesh/peers") {
            apply_peers(&mut poll, &peers);
        }
        poll
    }
}

fn apply_hardware(node: &mut ClusterNodeView, hardware: &Value) {
    node.node_id = str_at(hardware, "node_id");
    node.machine = str_at(hardware, "machine");
    node.uptime_s = f64_at(hardware, "uptime_s").map(|uptime| uptime.max(0.0) as u64);
    if let Some(cpu) = hardware.get("cpu") {
        node.cpu_busy_pct = f64_at(cpu, "busy_pct");
        node.cpu_cores = f64_at(cpu, "cores").map(|cores| cores.max(0.0) as u32);
    }
    if let Some(memory) = hardware.get("memory") {
        node.memory_used_bytes = f64_at(memory, "used_bytes").map(|bytes| bytes.max(0.0) as u64);
        node.memory_total_bytes = f64_at(memory, "total_bytes").map(|bytes| bytes.max(0.0) as u64);
        node.memory_busy_pct = f64_at(memory, "busy_pct");
    }
    if let Some(gpu) = hardware.get("gpu") {
        node.gpu_kind = str_at(gpu, "kind");
        let mut util = None;
        let mut used = 0u64;
        let mut total = 0u64;
        if let Some(gpus) = gpu.get("gpus").and_then(Value::as_array) {
            for card in gpus {
                if let Some(percent) = f64_at(card, "util_pct") {
                    util = Some(util.map_or(percent, |best: f64| best.max(percent)));
                }
                used += f64_at(card, "mem_used_bytes")
                    .map(|bytes| bytes.max(0.0) as u64)
                    .unwrap_or(0);
                total += f64_at(card, "mem_total_bytes")
                    .map(|bytes| bytes.max(0.0) as u64)
                    .unwrap_or(0);
            }
        }
        node.gpu_util_pct = util;
        if total > 0 {
            node.gpu_mem_used_bytes = Some(used);
            node.gpu_mem_total_bytes = Some(total);
        }
    }
    if let Some(network) = hardware.get("network") {
        node.net_rx_bytes_s =
            f64_at(network, "total_rx_bytes_s").map(|bytes| bytes.max(0.0) as u64);
        node.net_tx_bytes_s =
            f64_at(network, "total_tx_bytes_s").map(|bytes| bytes.max(0.0) as u64);
    }
    if let Some(tokens) = hardware.get("tokens") {
        node.tokens_per_sec = f64_at(tokens, "tokens_per_sec");
    }
}

/// Container counts for the Nodes tab. The project list itself is not
/// shown anywhere, so only the totals are kept.
fn apply_project_counts(node: &mut ClusterNodeView, projects: &Value) {
    let Some(entries) = projects.get("projects").and_then(Value::as_array) else {
        return;
    };
    let mut projects_running = 0u32;
    let mut containers_running = 0u32;
    let mut containers_total = 0u32;
    for project in entries.iter().take(MAX_PROJECTS_PER_NODE) {
        let mut running = 0u32;
        let mut total = 0u32;
        if let Some(containers) = project.get("containers").and_then(Value::as_array) {
            for container in containers.iter().take(MAX_CONTAINERS_PER_PROJECT) {
                total += 1;
                let status = container
                    .get("Status")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                running += u32::from(status.contains("Up"));
            }
        }
        projects_running += u32::from(running > 0);
        containers_running += running;
        containers_total += total;
    }
    node.projects_running = Some(projects_running);
    node.projects_total = Some(entries.len().min(MAX_PROJECTS_PER_NODE) as u32);
    node.containers_running = Some(containers_running);
    node.containers_total = Some(containers_total);
}

fn apply_peers(poll: &mut NodePoll, peers: &Value) {
    let Some(entries) = peers.get("peers").and_then(Value::as_array) else {
        return;
    };
    for entry in entries.iter().take(MAX_PEERS_PER_NODE) {
        let Some(node_id) = str_at(entry, "node_id") else {
            continue;
        };
        poll.peers.push(MeshPeerView {
            node_id: truncate(node_id, 128),
            name: str_at(entry, "name").map(|name| truncate(name, 128)),
            machine: str_at(entry, "machine").map(|machine| truncate(machine, 128)),
            online: entry
                .get("online")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            via: poll.node.name.clone(),
        });
    }
}

fn f64_at(value: &Value, key: &str) -> Option<f64> {
    value
        .get(key)
        .and_then(Value::as_f64)
        .filter(|number| number.is_finite())
}

fn str_at(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(|text| truncate(text.to_owned(), 256))
        .filter(|text| !text.is_empty())
}

fn truncate(text: String, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text
    } else {
        text.chars().take(max_chars).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (Value, Value, Value) {
        let hardware: Value = serde_json::from_str(
            r#"{
            "node_id": "abc-axiom-8210", "machine": "axiom", "uptime_s": 3600.5,
            "cpu": {"cores": 20, "busy_pct": 12.5, "per_core": []},
            "memory": {"total_bytes": 128849018880, "used_bytes": 32212254720, "busy_pct": 25.0},
            "gpu": {"available": true, "kind": "cuda", "gpus": [
                {"name": "GB10", "util_pct": 41.0, "mem_used_bytes": 4294967296, "mem_total_bytes": 34359738368}
            ]},
            "network": {"total_rx_bytes_s": 1024, "total_tx_bytes_s": 2048},
            "tokens": {"tokens_per_sec": 33.3}
        }"#,
        )
        .unwrap();
        let projects: Value = serde_json::from_str(
            r#"{"projects": [
            {"name": "fox-services", "status": "running",
             "containers": [
                {"Names": "fox", "Status": "Up 3 hours"},
                {"Names": "old", "Status": "Exited (0) 2 days ago"}
             ]}
        ]}"#,
        )
        .unwrap();
        let peers: Value = serde_json::from_str(
            r#"{"peers": [{"node_id": "x", "name": "axiom-dgx", "machine": "dgx", "online": true}], "count": 1}"#,
        )
        .unwrap();
        (hardware, projects, peers)
    }

    fn node() -> RemoteNodeConfig {
        RemoteNodeConfig {
            name: "axiom".into(),
            base_url: "http://192.168.1.63:8210".into(),
        }
    }

    #[test]
    fn mapping_reads_the_documented_shapes() {
        let (hardware, projects, peers) = fixture();
        let mut poll = NodePoll {
            node: ClusterNodeView {
                name: "axiom".into(),
                base_url: node().base_url.clone(),
                online: true,
                ..ClusterNodeView::default()
            },
            ..NodePoll::default()
        };
        apply_hardware(&mut poll.node, &hardware);
        apply_project_counts(&mut poll.node, &projects);
        apply_peers(&mut poll, &peers);
        assert_eq!(poll.node.node_id.as_deref(), Some("abc-axiom-8210"));
        assert_eq!(poll.node.cpu_busy_pct, Some(12.5));
        assert_eq!(poll.node.cpu_cores, Some(20));
        assert_eq!(poll.node.gpu_kind.as_deref(), Some("cuda"));
        assert_eq!(poll.node.gpu_util_pct, Some(41.0));
        assert_eq!(poll.node.tokens_per_sec, Some(33.3));
        assert_eq!(poll.node.projects_running, Some(1));
        assert_eq!(poll.node.projects_total, Some(1));
        assert_eq!(poll.node.containers_running, Some(1));
        assert_eq!(poll.node.containers_total, Some(2));
        assert_eq!(poll.peers[0].node_id, "x");
        assert!(poll.peers[0].online);
    }

    #[test]
    fn partial_documents_stay_online_with_gaps() {
        let mut poll = NodePoll {
            node: ClusterNodeView {
                name: "axiom".into(),
                online: true,
                ..ClusterNodeView::default()
            },
            ..NodePoll::default()
        };
        apply_hardware(&mut poll.node, &serde_json::json!({"uptime_s": 1}));
        assert!(poll.node.online);
        assert_eq!(poll.node.cpu_busy_pct, None);
        apply_project_counts(&mut poll.node, &serde_json::json!({}));
        assert_eq!(poll.node.containers_running, None);
    }

    #[test]
    fn offline_poll_reports_the_error() {
        let client = FoxServicesClient::new(
            RemoteNodeConfig {
                name: "gone".into(),
                base_url: "http://127.0.0.1:1".into(),
            },
            Duration::from_millis(200),
        );
        let poll = client.poll(false);
        assert!(!poll.node.online);
        assert!(poll.node.error.is_some());
    }
}
