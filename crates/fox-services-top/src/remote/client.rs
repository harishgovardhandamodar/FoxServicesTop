//! One fox-services node over HTTP: the four polled endpoints and the
//! tolerant mapping onto the core view types.
//!
//! The mapping never fails on unexpected JSON: unknown fields are ignored,
//! missing numbers stay `None`, over-long lists and lines are truncated.
//! Only a transport error or an unparsable document marks a node offline.

use std::time::Duration;

use fox_services_top_core::config::RemoteNodeConfig;
use fox_services_top_core::view::{
    ClusterNodeView, DockerContainerView, DockerLogView, DockerProjectView, MeshPeerView,
};
use serde_json::Value;

use super::http::{HttpError, get_json};

/// Upper bounds of one node's snapshot (a projection stays bounded).
pub const MAX_PROJECTS_PER_NODE: usize = 64;
pub const MAX_CONTAINERS_PER_PROJECT: usize = 64;
pub const MAX_LOG_CONTAINERS_PER_NODE: usize = 12;
pub const MAX_LOG_LINES: usize = 60;
pub const MAX_LOG_LINE_CHARS: usize = 240;
pub const MAX_PEERS_PER_NODE: usize = 64;

/// What one poll of one node produced.
#[derive(Debug, Clone, Default)]
pub struct NodePoll {
    pub node: ClusterNodeView,
    pub projects: Vec<DockerProjectView>,
    pub logs: Vec<DockerLogView>,
    pub peers: Vec<MeshPeerView>,
}

/// A node's fox-services API.
pub struct FoxServicesClient {
    node: RemoteNodeConfig,
    timeout: Duration,
}

impl FoxServicesClient {
    pub fn new(node: RemoteNodeConfig, timeout: Duration) -> Self {
        Self { node, timeout }
    }

    fn get(&self, path: &str) -> Result<Value, HttpError> {
        get_json(&self.node.base_url, path, self.timeout)
    }

    /// Polls everything one node offers for the two tabs.
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
        if let Ok(stats) = self.get("/api/docker/stats") {
            apply_docker_stats(&mut poll, &stats);
        }
        if let Ok(overview) = self.get("/api/docker/logs/overview?tail=40&max_containers=6") {
            apply_logs(&mut poll, &overview);
        }
        if discover_mesh
            && let Ok(peers) = self.get("/api/mesh/peers")
        {
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
                used += f64_at(card, "mem_used_bytes").map(|bytes| bytes.max(0.0) as u64).unwrap_or(0);
                total += f64_at(card, "mem_total_bytes").map(|bytes| bytes.max(0.0) as u64).unwrap_or(0);
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

fn apply_docker_stats(poll: &mut NodePoll, stats: &Value) {
    let projects = stats.get("projects").and_then(Value::as_array);
    let Some(projects) = projects else { return };
    let mut running = 0u32;
    let mut total = 0u32;
    for project in projects.iter().take(MAX_PROJECTS_PER_NODE) {
        let name = str_at(project, "name").unwrap_or_else(|| "?".to_owned());
        let status = str_at(project, "status").unwrap_or_else(|| "stopped".to_owned());
        let mut view = DockerProjectView {
            node: poll.node.name.clone(),
            project: truncate(name, 128),
            status: truncate(status, 16),
            running: 0,
            total: 0,
            containers: Vec::new(),
        };
        if let Some(containers) = project.get("containers").and_then(Value::as_array) {
            for container in containers.iter().take(MAX_CONTAINERS_PER_PROJECT) {
                let entry = container_view(container);
                view.running += u32::from(entry.running);
                view.total += 1;
                view.containers.push(entry);
            }
        }
        if view.running > 0 {
            running += 1;
        }
        total += 1;
        poll.projects.push(view);
    }
    poll.node.projects_running = Some(running);
    poll.node.projects_total = Some(total);
    let containers_running: u32 = poll.projects.iter().map(|project| project.running).sum();
    let containers_total: u32 = poll.projects.iter().map(|project| project.total).sum();
    poll.node.containers_running = Some(containers_running);
    poll.node.containers_total = Some(containers_total);
}

fn container_view(container: &Value) -> DockerContainerView {
    let name = container
        .get("Names")
        .or_else(|| container.get("Name"))
        .and_then(Value::as_str)
        .unwrap_or("?");
    let status = container
        .get("Status")
        .and_then(Value::as_str)
        .unwrap_or("");
    let stats = container.get("_stats");
    DockerContainerView {
        name: truncate(name.trim_start_matches('/').to_owned(), 128),
        status: truncate(status.to_owned(), 128),
        running: status.contains("Up"),
        image: truncate(
            container.get("Image").and_then(Value::as_str).unwrap_or("").to_owned(),
            256,
        ),
        ports: truncate(
            container.get("Ports").and_then(Value::as_str).unwrap_or("").to_owned(),
            256,
        ),
        cpu_pct: stats.and_then(|stats| f64_at(stats, "CPUPerc_num")),
        mem_pct: stats.and_then(|stats| f64_at(stats, "MemPerc_num")),
        mem_used_mib: stats.and_then(|stats| f64_at(stats, "MemUsed_MiB")),
    }
}

fn apply_logs(poll: &mut NodePoll, overview: &Value) {
    let Some(entries) = overview.get("logs").and_then(Value::as_array) else { return };
    for entry in entries.iter().take(MAX_LOG_CONTAINERS_PER_NODE) {
        let container = entry
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("?");
        let text = entry.get("logs").and_then(Value::as_str).unwrap_or("");
        let lines: Vec<String> = text
            .lines()
            .rev()
            .take(MAX_LOG_LINES)
            .map(|line| truncate(line.to_owned(), MAX_LOG_LINE_CHARS))
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        poll.logs.push(DockerLogView {
            node: poll.node.name.clone(),
            container: truncate(container.to_owned(), 128),
            lines,
        });
    }
}

fn apply_peers(poll: &mut NodePoll, peers: &Value) {
    let Some(entries) = peers.get("peers").and_then(Value::as_array) else { return };
    for entry in entries.iter().take(MAX_PEERS_PER_NODE) {
        let Some(node_id) = str_at(entry, "node_id") else { continue };
        poll.peers.push(MeshPeerView {
            node_id: truncate(node_id, 128),
            name: str_at(entry, "name").map(|name| truncate(name, 128)),
            machine: str_at(entry, "machine").map(|machine| truncate(machine, 128)),
            online: entry.get("online").and_then(Value::as_bool).unwrap_or(false),
            via: poll.node.name.clone(),
        });
    }
}

fn f64_at(value: &Value, key: &str) -> Option<f64> {
    value.get(key).and_then(Value::as_f64).filter(|number| number.is_finite())
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

    fn fixture() -> (Value, Value, Value, Value) {
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
        let stats: Value = serde_json::from_str(
            r#"{"stats": [], "projects": [
            {"name": "fox-services", "status": "running",
             "containers": [
                {"Names": "fox", "Status": "Up 3 hours", "Image": "fox:latest", "Ports": "8210/tcp",
                 "_stats": {"CPUPerc_num": 1.5, "MemPerc_num": 2.5, "MemUsed_MiB": 320.0}},
                {"Names": "old", "Status": "Exited (0) 2 days ago", "Image": "old:1", "Ports": ""}
             ]}
        ]}"#,
        )
        .unwrap();
        let logs: Value = serde_json::from_str(
            r#"{"logs": [{"name": "fox", "logs": "a\nb\nc"}], "count": 1}"#,
        )
        .unwrap();
        let peers: Value = serde_json::from_str(
            r#"{"peers": [{"node_id": "x", "name": "axiom-dgx", "machine": "dgx", "online": true}], "count": 1}"#,
        )
        .unwrap();
        (hardware, stats, logs, peers)
    }

    fn node() -> RemoteNodeConfig {
        RemoteNodeConfig {
            name: "axiom".into(),
            base_url: "http://192.168.1.63:8210".into(),
        }
    }

    #[test]
    fn mapping_reads_the_documented_shapes() {
        let (hardware, stats, logs, peers) = fixture();
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
        apply_docker_stats(&mut poll, &stats);
        apply_logs(&mut poll, &logs);
        apply_peers(&mut poll, &peers);
        assert_eq!(poll.node.node_id.as_deref(), Some("abc-axiom-8210"));
        assert_eq!(poll.node.cpu_busy_pct, Some(12.5));
        assert_eq!(poll.node.cpu_cores, Some(20));
        assert_eq!(poll.node.gpu_kind.as_deref(), Some("cuda"));
        assert_eq!(poll.node.gpu_util_pct, Some(41.0));
        assert_eq!(poll.node.tokens_per_sec, Some(33.3));
        assert_eq!(poll.projects.len(), 1);
        assert_eq!(poll.projects[0].containers.len(), 2);
        assert!(poll.projects[0].containers[0].running);
        assert!(!poll.projects[0].containers[1].running);
        assert_eq!(poll.projects[0].containers[0].cpu_pct, Some(1.5));
        assert_eq!(poll.node.projects_running, Some(1));
        assert_eq!(poll.node.containers_running, Some(1));
        assert_eq!(poll.logs[0].lines, vec!["a", "b", "c"]);
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
        apply_docker_stats(&mut poll, &serde_json::json!({}));
        assert_eq!(poll.projects.len(), 0);
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
