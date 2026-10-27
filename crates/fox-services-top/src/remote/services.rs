//! Which app uses which LLM model, per request — read from a node's live
//! map, not its compose config.
//!
//! A node's `/api/llm/live-map` aggregates actual LLM requests over a
//! window: a service → model matrix with real token counts, the server's
//! per-service status, and the newest requests with attribution. The node
//! matches services to compose projects itself; the tab only shows what it
//! reported, busiest status first.

use std::collections::BTreeMap;

use fox_services_top_core::view::{
    RecentRequestView, SERVICE_PROMPT_MAX_CHARS, ServiceModelView, ServiceRowView, ServicesView,
};
use serde_json::Value;

/// Recent requests a projection carries per node; the tab shows the newest.
const MAX_RECENT: usize = 40;
/// Services listed in the tab.
const MAX_SERVICES: usize = 32;
/// Models listed per service.
const MAX_MODELS_PER_SERVICE: usize = 6;

/// Services to read the live map over, in hours (1–168).
pub struct ServicePoll {
    pub window_hours: u32,
}

impl ServicePoll {
    pub fn new(window_hours: u32) -> Self {
        Self {
            window_hours: window_hours.clamp(1, 168),
        }
    }

    pub fn live_map_path(&self) -> String {
        format!("/api/llm/live-map?hours={}", self.window_hours)
    }

    /// One window of service → model flow, read from what the node reported.
    pub fn apply(&self, live_map: &Value) -> NodeServices {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|since| since.as_secs_f64());
        let status = live_map.get("service_status").and_then(Value::as_object);
        let mut services = Vec::new();
        if let Some(matrix) = live_map.get("matrix").and_then(Value::as_array) {
            // One row per service → model pair; group by service.
            let mut grouped: BTreeMap<String, Vec<ServiceModelView>> = BTreeMap::new();
            for row in matrix {
                let service = text(row, "service").unwrap_or_else(|| "unknown".to_owned());
                grouped.entry(service).or_default().push(ServiceModelView {
                    model: text(row, "model").unwrap_or_else(|| "unknown".to_owned()),
                    requests: u32::try_from(u64_at(row, "count")).unwrap_or(u32::MAX),
                    tokens: u64_at(row, "tokens"),
                    avg_ms: f64_at(row, "avg_ms"),
                });
            }
            for (service, mut models) in grouped {
                models.sort_by(|a, b| b.tokens.cmp(&a.tokens));
                let requests = models
                    .iter()
                    .fold(0u32, |sum, model| sum.saturating_add(model.requests));
                let tokens = models
                    .iter()
                    .fold(0u64, |sum, model| sum.saturating_add(model.tokens));
                models.truncate(MAX_MODELS_PER_SERVICE);
                let last_ts = matrix
                    .iter()
                    .filter(|row| text(row, "service").as_deref() == Some(service.as_str()))
                    .filter_map(|row| f64_at(row, "last_ts"))
                    .reduce(f64::max);
                let info = status.as_ref().and_then(|status| status.get(&service));
                services.push(ServiceRowView {
                    service,
                    status: info
                        .and_then(|info| text(info, "status"))
                        .unwrap_or_else(|| "Unknown".to_owned()),
                    models,
                    requests,
                    tokens,
                    ago_s: last_ts
                        .zip(now)
                        .map(|(then, now)| super::nonnegative_f64_to_u64((now - then).max(0.0))),
                });
            }
        }
        // Services the node knows but with no matrix rows still show.
        let no_status: serde_json::Map<String, Value> = serde_json::Map::new();
        let status = status.as_ref().map_or(&no_status, |status| status);
        for (service, info) in status {
            if services.iter().any(|row| row.service == *service) {
                continue;
            }
            let last_seen = f64_at(info, "last_seen_ts");
            services.push(ServiceRowView {
                service: service.clone(),
                status: text(info, "status").unwrap_or_else(|| "Unknown".to_owned()),
                models: Vec::new(),
                requests: u32::try_from(u64_at(info, "request_count")).unwrap_or(u32::MAX),
                tokens: u64_at(info, "total_tokens"),
                ago_s: last_seen
                    .zip(now)
                    .map(|(then, now)| super::nonnegative_f64_to_u64((now - then).max(0.0))),
            });
        }
        sort_services(&mut services);
        services.truncate(MAX_SERVICES);
        NodeServices {
            window_label: format!("{}h", self.window_hours),
            services,
            recent: recent_requests(live_map, now),
        }
    }
}

/// One node's Fox-Services rows.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NodeServices {
    pub window_label: String,
    pub services: Vec<ServiceRowView>,
    pub recent: Vec<RecentRequestView>,
}

/// Busiest status first (the web tab's order), then most tokens.
fn sort_services(services: &mut [ServiceRowView]) {
    services.sort_by(|a, b| {
        status_rank(&a.status)
            .cmp(&status_rank(&b.status))
            .then(b.tokens.cmp(&a.tokens))
    });
}

fn status_rank(status: &str) -> u8 {
    match status {
        "Busy" => 0,
        "Live" => 1,
        "Running" => 2,
        "Unreachable" => 3,
        "Stopped" => 4,
        "Not deployed" => 5,
        _ => 6,
    }
}

/// The newest requests with attribution, newest first.
fn recent_requests(live_map: &Value, now: Option<f64>) -> Vec<RecentRequestView> {
    let Some(rows) = live_map.get("recent").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut recent: Vec<RecentRequestView> = rows
        .iter()
        .filter_map(|row| {
            let ts = f64_at(row, "ts")?;
            Some(RecentRequestView {
                node: String::new(),
                service: text(row, "service").unwrap_or_else(|| "unknown".to_owned()),
                client_ip: text(row, "client_ip").unwrap_or_default(),
                model: text(row, "model").unwrap_or_else(|| "unknown".to_owned()),
                status: text(row, "status").unwrap_or_else(|| "unknown".to_owned()),
                prompt_tokens: u64_at(row, "prompt_tokens"),
                completion_tokens: u64_at(row, "completion_tokens"),
                total_tokens: u64_at(row, "total_tokens"),
                duration_ms: f64_at(row, "duration_ms"),
                queue_ms: f64_at(row, "queue_ms"),
                query_type: text(row, "query_type").unwrap_or_default(),
                requestor: text(row, "requestor").unwrap_or_default(),
                prompt: truncate_prompt(&text(row, "prompt").unwrap_or_default()),
                ago_s: now
                    .map(|now| super::nonnegative_f64_to_u64((now - ts).max(0.0)))
                    .unwrap_or(0),
            })
        })
        .collect();
    // Newest first: the smallest age leads.
    recent.sort_by(|a, b| a.ago_s.cmp(&b.ago_s));
    recent.truncate(MAX_RECENT);
    recent
}

/// Merges `other` into the cluster view, prefixing duplicate service names
/// with their node so two nodes' `quai-radar` stay apart.
pub fn merge(into: &mut ServicesView, node: &str, services: NodeServices) {
    if into.window_label.is_empty() {
        into.window_label = services.window_label.clone();
    }
    // A name already listed belongs to another node: prefix it.
    let mut seen: Vec<String> = into
        .services
        .iter()
        .map(|row| row.service.clone())
        .collect();
    for mut row in services.services {
        if seen.contains(&row.service) {
            let original = row.service.clone();
            row.service = format!("{node}/{original}");
        }
        seen.push(row.service.clone());
        into.services.push(row);
    }
    sort_services(&mut into.services);
    into.services.truncate(MAX_SERVICES);
    // Only names listed by another node collide: rows of this batch share
    // their node's plain names.
    let prior: Vec<String> = into.recent.iter().map(|row| row.service.clone()).collect();
    for mut row in services.recent {
        row.node = node.to_owned();
        if prior.contains(&row.service) {
            let original = row.service.clone();
            row.service = format!("{node}/{original}");
        }
        into.recent.push(row);
    }
    into.recent.sort_by(|a, b| a.ago_s.cmp(&b.ago_s));
    into.recent.truncate(MAX_RECENT);
    into.idle = into.services.is_empty() && into.recent.is_empty();
}

/// A prompt snippet a projection carries: one line, bounded.
fn truncate_prompt(prompt: &str) -> String {
    let line = prompt.split('\n').next().unwrap_or_default().trim();
    let mut snippet: String = line.chars().take(SERVICE_PROMPT_MAX_CHARS).collect();
    if line.chars().count() > SERVICE_PROMPT_MAX_CHARS {
        snippet.push('…');
    }
    snippet
}

fn text(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(|text| text.to_owned())
}

fn f64_at(value: &Value, key: &str) -> Option<f64> {
    value.get(key).and_then(Value::as_f64)
}

fn u64_at(value: &Value, key: &str) -> u64 {
    super::nonnegative_f64_to_u64(f64_at(value, key).unwrap_or(0.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn live_map() -> Value {
        json!({
            "matrix": [
                {"service": "quai-radar", "model": "qwen3.8:latest", "count": 21, "tokens": 19000, "avg_ms": 4388.0, "last_ts": 1791979000.0},
                {"service": "mapper", "model": "qwen3.8:27b", "count": 628, "tokens": 1600000, "avg_ms": 19732.0, "last_ts": 1791979261.0},
                {"service": "mapper", "model": "qwen3.8:latest", "count": 120, "tokens": 368600, "avg_ms": 9000.0, "last_ts": 1791979200.0},
            ],
            "service_status": {
                "quai-radar": {"status": "Not deployed", "request_count": 21, "total_tokens": 19000, "last_seen_ts": 1791979000.0},
                "mapper": {"status": "Live", "request_count": 748, "total_tokens": 1968600, "last_seen_ts": 1791979261.0},
                "idle-svc": {"status": "Stopped", "request_count": 0, "total_tokens": 0, "last_seen_ts": null},
            },
            "recent": [
                {"service": "mapper", "model": "qwen3.8:27b", "status": "complete",
                 "prompt_tokens": 97, "completion_tokens": 400, "total_tokens": 497,
                 "duration_ms": 4789.0, "queue_ms": 0.0, "query_type": "research",
                 "requestor": "user", "client_ip": "172.26.0.1",
                 "prompt": "Brief: flagged brief\nsecond line", "ts": 1791979261.0},
                {"service": "mapper", "model": "qwen3.8:latest", "status": "cancelled",
                 "prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0,
                 "duration_ms": 14608.0, "queue_ms": 0.0, "query_type": "chat",
                 "requestor": "user", "client_ip": "172.26.0.1",
                 "prompt": "Model: what adversarial scenarios", "ts": 1791979200.0},
            ],
        })
    }

    #[test]
    fn service_totals_include_models_beyond_the_display_limit() {
        let matrix: Vec<_> = (0..=MAX_MODELS_PER_SERVICE)
            .map(|index| {
                serde_json::json!({
                    "service": "demo",
                    "model": format!("model-{index}"),
                    "count": 1,
                    "tokens": 1
                })
            })
            .collect();
        let services = ServicePoll::new(24).apply(&serde_json::json!({"matrix": matrix}));

        assert_eq!(services.services[0].models.len(), MAX_MODELS_PER_SERVICE);
        assert_eq!(
            services.services[0].requests,
            u32::try_from(MAX_MODELS_PER_SERVICE + 1).unwrap()
        );
        assert_eq!(
            services.services[0].tokens,
            u64::try_from(MAX_MODELS_PER_SERVICE + 1).unwrap()
        );
    }

    #[test]
    fn integer_counters_saturate_at_their_view_type_limit() {
        let services = ServicePoll::new(24).apply(&serde_json::json!({
            "matrix": [{
                "service": "demo",
                "model": "model",
                "count": 18446744073709551615_u64,
                "tokens": 18446744073709551615_u64
            }]
        }));

        assert_eq!(services.services[0].requests, u32::MAX);
        assert_eq!(services.services[0].tokens, u64::MAX);
    }

    #[test]
    fn matrix_groups_by_service_with_busiest_status_first() {
        let services = ServicePoll::new(24).apply(&live_map()).services;
        // Live sorts before Not deployed, whatever the token counts.
        assert_eq!(services.len(), 3);
        assert_eq!(services[0].service, "mapper");
        assert_eq!(services[0].status, "Live");
        assert_eq!(services[0].requests, 748);
        assert_eq!(services[0].tokens, 1968600);
        assert_eq!(services[0].models.len(), 2);
        assert_eq!(services[0].models[0].model, "qwen3.8:27b");
        // A service the status map knows but with no matrix rows still shows.
        assert_eq!(services[1].service, "idle-svc");
        assert_eq!(services[1].status, "Stopped");
        assert_eq!(services[2].service, "quai-radar");
    }

    #[test]
    fn recent_rows_carry_attribution_and_one_line_prompts() {
        let recent = ServicePoll::new(24).apply(&live_map()).recent;
        assert_eq!(recent.len(), 2);
        // Newest first.
        assert!(recent[0].ago_s <= recent[1].ago_s);
        let row = &recent[0];
        assert_eq!(row.service, "mapper");
        assert_eq!(row.model, "qwen3.8:27b");
        assert_eq!(row.status, "complete");
        assert_eq!(
            (row.prompt_tokens, row.completion_tokens, row.total_tokens),
            (97, 400, 497)
        );
        assert_eq!(row.query_type, "research");
        assert_eq!(row.client_ip, "172.26.0.1");
        assert_eq!(row.prompt, "Brief: flagged brief");
        assert!(!row.prompt.contains('\n'));
    }

    #[test]
    fn merging_two_nodes_separates_equal_service_names() {
        let mut into = ServicesView::default();
        merge(&mut into, "axiom", ServicePoll::new(24).apply(&live_map()));
        // One node keeps plain names, even across many rows of one service.
        let recent: Vec<&str> = into.recent.iter().map(|row| row.service.as_str()).collect();
        assert!(recent.iter().all(|name| *name == "mapper"), "{recent:?}");
        merge(
            &mut into,
            "axiom-dgx",
            ServicePoll::new(24).apply(&live_map()),
        );
        let names: Vec<&str> = into
            .services
            .iter()
            .map(|row| row.service.as_str())
            .collect();
        assert!(names.contains(&"mapper"), "{names:?}");
        assert!(names.contains(&"axiom-dgx/mapper"), "{names:?}");
        assert!(!into.idle);
    }
}
