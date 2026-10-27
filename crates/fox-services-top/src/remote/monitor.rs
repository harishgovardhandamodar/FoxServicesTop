//! Background polling of the configured fox-services nodes.
//!
//! One thread polls every node each `poll_interval_ms` (`/api/hardware/status`,
//! `/api/llm/live-map`, `/api/usage/history`, `/api/ollama/stats`, and, with
//! mesh discovery, `/api/mesh/peers`) and publishes the latest
//! [`ClusterSnapshot`]. Every endpoint answers from memory or the database,
//! so one loaded node never stalls the next poll behind it. The Manager only
//! reads the snapshot through [`ClusterSource`]: polling, retries and
//! timeouts never touch the control thread.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use fox_services_top_core::config::RemoteConfig;
use fox_services_top_core::ports::ClusterSource;
use fox_services_top_core::view::{ClusterSnapshot, ClusterView, ServicesView, TokensView};

use super::client::{FoxServicesClient, NodePoll};
use super::services::merge as merge_services;
use super::tokens::merge as merge_tokens;

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
            docker: Arc::new(ServicesView {
                enabled: config.enabled,
                ..ServicesView::default()
            }),
            tokens: Arc::new(TokensView {
                enabled: config.enabled,
                idle: true,
                ..TokensView::default()
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
    // A poll whose endpoints time out keeps the last answered rows rather
    // than blanking the tabs (D-118).
    let previous = state.lock().unwrap_or_else(PoisonError::into_inner).clone();
    let mut cluster = ClusterView {
        enabled: true,
        nodes: Vec::new(),
        peers: Vec::new(),
    };
    let mut services = ServicesView {
        enabled: true,
        ..ServicesView::default()
    };
    let mut tokens = TokensView {
        enabled: config.collect_tokens,
        ..TokensView::default()
    };
    let mut services_answered = false;
    let mut tokens_answered = false;
    for node in config.nodes.iter().take(16) {
        let client = FoxServicesClient::new(node.clone(), timeout)
            .with_services(config.service_window_hours);
        let client = if config.collect_tokens {
            client.with_tokens(config.token_window_hours, config.token_history_limit)
        } else {
            client
        };
        let NodePoll {
            node,
            peers,
            services: node_services,
            tokens: node_tokens,
        } = client.poll(config.discover_mesh);
        let node_name = node.name.clone();
        cluster.nodes.push(node);
        cluster.peers.extend(peers);
        if let Some(node_services) = node_services {
            services_answered = true;
            merge_services(&mut services, &node_name, node_services);
        }
        if let Some(node_tokens) = node_tokens {
            tokens_answered = true;
            merge_tokens(&mut tokens, &node_name, node_tokens);
        }
    }
    if !services_answered && previous.docker.enabled && !previous.docker.idle {
        services.services = previous.docker.services.clone();
        services.recent = previous.docker.recent.clone();
        services
            .window_label
            .clone_from(&previous.docker.window_label);
        services.stale = true;
    }
    if !tokens_answered && tokens.enabled && previous.tokens.enabled && !previous.tokens.idle {
        tokens.apps = previous.tokens.apps.clone();
        tokens.engines = previous.tokens.engines.clone();
        tokens.total_requests = previous.tokens.total_requests;
        tokens.total_ingress = previous.tokens.total_ingress;
        tokens.total_egress = previous.tokens.total_egress;
        tokens
            .window_label
            .clone_from(&previous.tokens.window_label);
        tokens.timeline = previous.tokens.timeline.clone();
        tokens.stale = true;
    }
    cluster.peers.truncate(128);
    let snapshot = Arc::new(ClusterSnapshot {
        cluster: Arc::new(cluster),
        docker: Arc::new(services),
        tokens: Arc::new(tokens),
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
    use fox_services_top_core::view::{ServiceRowView, TokenAppView};

    fn test_monitor(nodes: Vec<RemoteNodeConfig>, enabled: bool) -> ClusterMonitor {
        ClusterMonitor::start(&RemoteConfig {
            enabled,
            poll_interval_ms: 1000,
            timeout_ms: 200,
            discover_mesh: false,
            collect_tokens: false,
            token_window_hours: 24,
            token_history_limit: 500,
            service_window_hours: 24,
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

    /// A node answering hardware, the live map and usage history.
    fn fake_node(now: u64) -> String {
        use std::io::{Read as _, Write as _};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut socket) = stream else { continue };
                thread::spawn(move || {
                    let mut request = [0u8; 2048];
                    if socket.read(&mut request).is_err() {
                        return;
                    }
                    let text = String::from_utf8_lossy(&request).into_owned();
                    let body: Vec<u8> = if text.contains("/api/hardware/status") {
                        br#"{"machine":"axiom","uptime_s":10}"#.to_vec()
                    } else if text.contains("/api/llm/live-map") {
                        format!(
                            concat!(
                                r#"{{"matrix":[{{"service":"quai-radar","model":"qwen3.8:latest","count":5,"tokens":4000,"avg_ms":4388.0,"last_ts":{now}}}]"#,
                                r#","service_status":{{"quai-radar":{{"status":"Live","request_count":5,"total_tokens":4000,"last_seen_ts":{now}}}}}"#,
                                r#","recent":[{{"service":"quai-radar","model":"qwen3.8:latest","status":"complete","prompt_tokens":100,"completion_tokens":50,"total_tokens":150,"duration_ms":4388.0,"queue_ms":0.0,"query_type":"chat","requestor":"user","client_ip":"172.26.0.1","prompt":"hello","ts":{now}}}]}}"#
                            ),
                            now = now
                        )
                        .into_bytes()
                    } else if text.contains("/api/usage/history") {
                        format!(
                            r#"{{"usage":[{{"service":"quai-radar","model":"qwen3.8:latest","prompt_tokens":100,"completion_tokens":50,"ts":{now},"duration_ms":10}}]}}"#
                        )
                        .into_bytes()
                    } else {
                        b"{}".to_vec()
                    };
                    let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
                    let _ = socket.write_all(head.as_bytes());
                    let _ = socket.write_all(&body);
                });
            }
        });
        base
    }

    fn live_config(base: String) -> RemoteConfig {
        RemoteConfig {
            enabled: true,
            poll_interval_ms: 1000,
            timeout_ms: 2000,
            discover_mesh: false,
            collect_tokens: true,
            token_window_hours: 24,
            token_history_limit: 500,
            service_window_hours: 24,
            nodes: vec![RemoteNodeConfig {
                name: "axiom".into(),
                base_url: base,
            }],
        }
    }

    fn live_now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    #[test]
    fn one_poll_reads_services_and_tokens() {
        let config = live_config(fake_node(live_now()));
        let state = Mutex::new(Arc::new(ClusterSnapshot::default()));
        poll_once(&config, &state, Duration::from_millis(2000));
        let snapshot = state.lock().unwrap_or_else(PoisonError::into_inner).clone();
        assert!(snapshot.cluster.nodes[0].online);
        assert_eq!(snapshot.docker.services.len(), 1);
        assert_eq!(snapshot.docker.services[0].service, "quai-radar");
        assert_eq!(snapshot.docker.services[0].status, "Live");
        assert_eq!(snapshot.docker.recent.len(), 1);
        assert!(!snapshot.docker.stale, "read fresh");
        assert_eq!(snapshot.tokens.apps.len(), 1);
        assert!(!snapshot.tokens.stale, "read fresh");
    }

    #[test]
    fn a_failed_poll_keeps_the_last_rows() {
        // A poll whose endpoints time out keeps the last answered rows
        // instead of blanking the tabs (D-118).
        let state = Mutex::new(Arc::new(ClusterSnapshot {
            cluster: Arc::new(ClusterView {
                enabled: true,
                ..ClusterView::default()
            }),
            docker: Arc::new(ServicesView {
                enabled: true,
                services: vec![ServiceRowView {
                    service: "quai-radar".to_owned(),
                    status: "Live".to_owned(),
                    ..ServiceRowView::default()
                }],
                ..ServicesView::default()
            }),
            tokens: Arc::new(TokensView {
                enabled: true,
                apps: vec![TokenAppView {
                    app: "quai-radar".to_owned(),
                    ..TokenAppView::default()
                }],
                total_ingress: 581_671,
                total_egress: 683_004,
                window_label: "24h".to_owned(),
                ..TokensView::default()
            }),
        }));
        let config = RemoteConfig {
            enabled: true,
            poll_interval_ms: 1000,
            timeout_ms: 200,
            discover_mesh: false,
            collect_tokens: true,
            token_window_hours: 24,
            token_history_limit: 500,
            service_window_hours: 24,
            nodes: vec![RemoteNodeConfig {
                name: "gone".into(),
                base_url: "http://127.0.0.1:1".into(),
            }],
        };
        poll_once(&config, &state, Duration::from_millis(200));
        let snapshot = state.lock().unwrap_or_else(PoisonError::into_inner).clone();
        assert!(snapshot.docker.stale, "services marked as the last read");
        assert_eq!(snapshot.docker.services.len(), 1);
        assert!(snapshot.tokens.stale, "tokens marked as the last read");
        assert_eq!(snapshot.tokens.total_ingress, 581_671);
        assert_eq!(snapshot.tokens.total_egress, 683_004);
        assert!(
            snapshot.cluster.nodes[0].error.is_some(),
            "still reports offline"
        );
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
