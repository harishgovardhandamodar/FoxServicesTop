//! Background polling of the configured fox-services nodes.
//!
//! One thread polls every node each `poll_interval_ms` (`/api/hardware/status`,
//! `/api/docker/stats`, `/api/docker/logs/overview`, and, with mesh
//! discovery, `/api/mesh/peers`) and publishes the latest [`ClusterSnapshot`].
//! The Manager only reads it through [`ClusterSource`]: polling, retries and
//! timeouts never touch the control thread.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use fox_services_top_core::config::RemoteConfig;
use fox_services_top_core::ports::ClusterSource;
use fox_services_top_core::view::{ClusterSnapshot, ClusterView, DockerView};

use super::client::{FoxServicesClient, NodePoll};

/// Polls the cluster on its own thread and serves the latest snapshot.
pub struct ClusterMonitor {
    state: Arc<Mutex<Arc<ClusterSnapshot>>>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl ClusterMonitor {
    /// Starts polling `config`'s nodes. Disabled configs (or an empty node
    /// list) publish a disabled snapshot and spawn no thread.
    pub fn start(config: &RemoteConfig) -> Self {
        let state = Arc::new(Mutex::new(Arc::new(ClusterSnapshot {
            cluster: Arc::new(ClusterView {
                enabled: config.enabled,
                ..ClusterView::default()
            }),
            docker: Arc::new(DockerView {
                enabled: config.enabled,
                ..DockerView::default()
            }),
        })));
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread = if config.enabled && !config.nodes.is_empty() {
            Some(spawn(
                config.clone(),
                Arc::clone(&state),
                Arc::clone(&shutdown),
            ))
        } else {
            None
        };
        Self {
            state,
            shutdown,
            thread,
        }
    }

    /// What the Manager reads every frame.
    pub fn source(&self) -> Arc<dyn ClusterSource> {
        Arc::new(MonitorSource {
            state: Arc::clone(&self.state),
        })
    }

    /// Stops the polling thread.
    pub fn shutdown(&mut self) {
        if let Some(thread) = self.stop() {
            let _ = thread.join();
        }
    }

    /// Asks the polling thread to stop and hands over its handle, so the
    /// owner can join it within a shutdown budget instead of blocking.
    pub fn stop(&mut self) -> Option<JoinHandle<()>> {
        self.shutdown.store(true, Ordering::Release);
        self.thread.take()
    }
}

impl Drop for ClusterMonitor {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct MonitorSource {
    state: Arc<Mutex<Arc<ClusterSnapshot>>>,
}

impl ClusterSource for MonitorSource {
    fn latest(&self) -> Option<Arc<ClusterSnapshot>> {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .into()
    }
}

fn spawn(
    config: RemoteConfig,
    state: Arc<Mutex<Arc<ClusterSnapshot>>>,
    shutdown: Arc<AtomicBool>,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("fox-services-top-cluster".to_owned())
        .spawn(move || {
            let timeout = Duration::from_millis(u64::from(config.timeout_ms.max(1)));
            let interval = Duration::from_millis(u64::from(config.poll_interval_ms.max(1)));
            while !shutdown.load(Ordering::Acquire) {
                poll_once(&config, &state, timeout);
                sleep_slice(interval, &shutdown);
            }
        })
        .expect("cluster monitor thread spawns")
}

fn poll_once(config: &RemoteConfig, state: &Mutex<Arc<ClusterSnapshot>>, timeout: Duration) {
    let mut cluster = ClusterView {
        enabled: true,
        nodes: Vec::new(),
        peers: Vec::new(),
    };
    let mut docker = DockerView {
        enabled: true,
        projects: Vec::new(),
        logs: Vec::new(),
    };
    for node in config.nodes.iter().take(16) {
        let client = FoxServicesClient::new(node.clone(), timeout);
        let NodePoll {
            node,
            projects,
            logs,
            peers,
        } = client.poll(config.discover_mesh);
        cluster.nodes.push(node);
        docker.projects.extend(projects);
        docker.logs.extend(logs);
        cluster.peers.extend(peers);
    }
    docker.projects.truncate(128);
    docker.logs.truncate(64);
    cluster.peers.truncate(128);
    let snapshot = Arc::new(ClusterSnapshot {
        cluster: Arc::new(cluster),
        docker: Arc::new(docker),
    });
    *state.lock().unwrap_or_else(PoisonError::into_inner) = snapshot;
}

/// Sleeps `total` in small slices so shutdown stays prompt.
fn sleep_slice(total: Duration, shutdown: &AtomicBool) {
    let step = Duration::from_millis(100);
    let mut slept = Duration::ZERO;
    while slept < total && !shutdown.load(Ordering::Acquire) {
        let slice = (total - slept).min(step);
        thread::sleep(slice);
        slept += slice;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fox_services_top_core::config::RemoteNodeConfig;

    fn test_monitor(nodes: Vec<RemoteNodeConfig>, enabled: bool) -> ClusterMonitor {
        ClusterMonitor::start(&RemoteConfig {
            enabled,
            poll_interval_ms: 1000,
            timeout_ms: 200,
            discover_mesh: false,
            nodes,
        })
    }

    #[test]
    fn disabled_configs_publish_a_disabled_snapshot_without_polling() {
        let monitor = test_monitor(vec![], false);
        assert!(monitor.thread.is_none());
        let snapshot = monitor.source().latest().unwrap();
        assert!(!snapshot.cluster.enabled);
        assert!(!snapshot.docker.enabled);
        // Enabled but node-less likewise spawns no thread.
        let monitor = test_monitor(vec![], true);
        assert!(monitor.thread.is_none());
        assert!(monitor.source().latest().unwrap().cluster.enabled);
        let mut monitor = test_monitor(vec![], false);
        monitor.shutdown();
    }

    #[test]
    fn an_unreachable_node_reports_offline() {
        let monitor = test_monitor(
            vec![RemoteNodeConfig {
                name: "gone".into(),
                base_url: "http://127.0.0.1:1".into(),
            }],
            true,
        );
        assert!(monitor.thread.is_some());
        // The first poll runs immediately: wait for the node row to appear.
        let source = monitor.source();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let snapshot = loop {
            let snapshot = source.latest().unwrap();
            if !snapshot.cluster.nodes.is_empty() || std::time::Instant::now() > deadline {
                break snapshot;
            }
            thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(snapshot.cluster.nodes.len(), 1);
        assert!(!snapshot.cluster.nodes[0].online);
        assert!(snapshot.cluster.nodes[0].error.is_some());
    }
}
