//! Token ingress/egress by app and model, read from a node's usage history
//! and its inference engine's models.
//!
//! Ingress is prompt (input) tokens, egress is completion (output) tokens:
//! the two directions apps send and receive. A node's `/api/usage/history`
//! lists recent requests with `service`, `model`, `prompt_tokens` and
//! `completion_tokens`; the rows inside the configured window are grouped
//! per app and per model here. `/api/ollama/stats` adds the engine's
//! models — installed, loaded, and what each holds in memory/VRAM.
//!
//! Only Ollama is read for now; LMS and vLLM need an equivalent endpoint.

use std::collections::BTreeMap;

use fox_services_top_core::view::{
    TokenAppView, TokenBucketView, TokenEngineModelView, TokenEngineView, TokenModelView,
    TokensView,
};
use serde_json::Value;

/// Rows a node may report; the poller aggregates what it read.
const MAX_ROWS: usize = 1000;
/// Apps listed in the tab.
const MAX_APPS: usize = 32;
/// Models listed per app.
const MAX_MODELS_PER_APP: usize = 8;
/// Engine models listed.
const MAX_ENGINE_MODELS: usize = 16;
/// Points the TPS timeline is charted in; a wider window buckets coarser.
const TIMELINE_POINTS: usize = 96;

/// Tokens to aggregate over `window_hours`, and the engine's models.
pub struct TokenPoll {
    pub usage_limit: u32,
    pub window_hours: u32,
}

impl TokenPoll {
    pub fn new(window_hours: u32, usage_limit: u32) -> Self {
        Self {
            usage_limit: usage_limit.clamp(1, 1000),
            window_hours: window_hours.clamp(1, 168),
        }
    }

    pub fn usage_path(&self) -> String {
        format!("/api/usage/history?limit={}", self.usage_limit)
    }

    /// One window of flow for `node`, read from what it reported.
    pub fn apply(&self, usage: &Value, stats: Option<&Value>) -> NodeTokens {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|since| since.as_secs_f64());
        let cutoff = window_cutoff(self.window_hours);
        let mut apps: BTreeMap<String, AppAccumulator> = BTreeMap::new();
        let mut buckets: Vec<BucketAccumulator> = (0..TIMELINE_POINTS)
            .map(|_| BucketAccumulator::default())
            .collect();
        let bucket_seconds = self.bucket_seconds();
        if let Some(rows) = usage.get("usage").and_then(Value::as_array) {
            for row in rows.iter().take(MAX_ROWS) {
                let Some(ts) = f64_at(row, "ts") else {
                    continue;
                };
                if cutoff.is_some_and(|cutoff| ts < cutoff) {
                    continue;
                }
                let app = text(row, "service").unwrap_or_else(|| "unknown".to_owned());
                let model = text(row, "model").unwrap_or_else(|| "unknown".to_owned());
                let ingress =
                    super::nonnegative_f64_to_u64(f64_at(row, "prompt_tokens").unwrap_or(0.0));
                let egress =
                    super::nonnegative_f64_to_u64(f64_at(row, "completion_tokens").unwrap_or(0.0));
                let duration_ms = f64_at(row, "duration_ms");
                apps.entry(app)
                    .or_default()
                    .push(model, ingress, egress, ts, duration_ms);
                if let Some(now) = now {
                    let index = bucket_index(ts, now, bucket_seconds);
                    let bucket = &mut buckets[index];
                    bucket.requests = bucket.requests.saturating_add(1);
                    bucket.ingress = bucket.ingress.saturating_add(ingress);
                    bucket.egress = bucket.egress.saturating_add(egress);
                }
            }
        }
        NodeTokens {
            window_label: format!("{}h", self.window_hours),
            apps: finish_apps(apps),
            timeline: finish_buckets(buckets, now, bucket_seconds),
            engines: stats
                .map(|stats| vec![ollama_engine(stats)])
                .unwrap_or_default(),
        }
    }

    /// Bucket width over the window: about a minute per hour, so a 1 h
    /// window reads as 60 points and a week stays under 100 columns.
    fn bucket_seconds(&self) -> f64 {
        let window = f64::from(self.window_hours) * 3600.0;
        (window / TIMELINE_POINTS as f64).max(60.0)
    }
}

/// Which bucket a request at `ts` falls into, oldest first.
fn bucket_index(ts: f64, now: f64, bucket_seconds: f64) -> usize {
    let elapsed = (now - ts).max(0.0);
    let from_end = super::nonnegative_f64_to_usize((elapsed / bucket_seconds).floor());
    (TIMELINE_POINTS - 1).saturating_sub(from_end.min(TIMELINE_POINTS - 1))
}

/// Tokens per second over the request time an app or model consumed.
fn rate(tokens: u64, duration_ms: f64) -> f64 {
    if duration_ms <= 0.0 {
        return 0.0;
    }
    tokens as f64 / (duration_ms / 1000.0)
}

/// One node's token flow.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NodeTokens {
    pub window_label: String,
    pub apps: Vec<TokenAppView>,
    pub timeline: Vec<TokenBucketView>,
    pub engines: Vec<TokenEngineView>,
}

impl NodeTokens {
    pub fn ingress(&self) -> u64 {
        self.apps
            .iter()
            .fold(0u64, |sum, app| sum.saturating_add(app.ingress_tokens))
    }

    pub fn egress(&self) -> u64 {
        self.apps
            .iter()
            .fold(0u64, |sum, app| sum.saturating_add(app.egress_tokens))
    }

    pub fn requests(&self) -> u32 {
        self.apps
            .iter()
            .fold(0u32, |sum, app| sum.saturating_add(app.requests))
    }
}

/// Merges `other` into `self`, prefixing duplicate app names with their node
/// so two nodes' `quai-radar` stay apart.
pub fn merge(into: &mut TokensView, node: &str, tokens: NodeTokens) {
    into.total_ingress = into.total_ingress.saturating_add(tokens.ingress());
    into.total_egress = into.total_egress.saturating_add(tokens.egress());
    into.total_requests = into.total_requests.saturating_add(tokens.requests());
    if into.window_label.is_empty() {
        into.window_label = tokens.window_label.clone();
    }
    into.timeline = merge_timeline(&into.timeline, &tokens.timeline);
    // A name already listed belongs to another node: prefix it.
    let mut seen: Vec<String> = into.apps.iter().map(|app| app.app.clone()).collect();
    for mut app in tokens.apps {
        if seen.contains(&app.app) {
            let original = app.app.clone();
            app.app = format!("{node}/{original}");
        }
        seen.push(app.app.clone());
        into.apps.push(app);
    }
    into.apps.sort_by(|a, b| {
        b.ingress_tokens
            .cmp(&a.ingress_tokens)
            .then(b.egress_tokens.cmp(&a.egress_tokens))
    });
    into.apps.truncate(MAX_APPS);
    for engine in tokens.engines {
        match into
            .engines
            .iter_mut()
            .find(|held| held.engine == engine.engine)
        {
            Some(held) => {
                held.available = held.available || engine.available;
                held.loaded_count = held.loaded_count.saturating_add(engine.loaded_count);
                held.model_count = held.model_count.saturating_add(engine.model_count);
                held.vram_total_bytes = held
                    .vram_total_bytes
                    .saturating_add(engine.vram_total_bytes);
                held.models.extend(engine.models);
            }
            None => into.engines.push(engine),
        }
    }
    for engine in &mut into.engines {
        engine.models.sort_by(|a, b| {
            b.running
                .cmp(&a.running)
                .then(b.size_vram_bytes.cmp(&a.size_vram_bytes))
        });
        engine.models.truncate(MAX_ENGINE_MODELS);
    }
    into.idle = into.total_requests == 0;
}

/// Adds one node's buckets to another's, matching them on their start: two
/// nodes polled seconds apart still line up on the chart.
fn merge_timeline(into: &[TokenBucketView], other: &[TokenBucketView]) -> Vec<TokenBucketView> {
    if into.is_empty() {
        return other.to_vec();
    }
    if other.is_empty() {
        return into.to_vec();
    }
    let width = other
        .get(1)
        .map_or(1, |next| next.at_s.abs_diff(other[0].at_s).max(1));
    let merged: Vec<TokenBucketView> = other
        .iter()
        .map(|bucket| {
            let matching = into
                .iter()
                .find(|held| bucket.at_s.abs_diff(held.at_s) <= width / 2)
                .cloned();
            match matching {
                Some(held) => TokenBucketView {
                    at_s: bucket.at_s,
                    requests: held.requests.saturating_add(bucket.requests),
                    ingress_tps: held.ingress_tps + bucket.ingress_tps,
                    egress_tps: held.egress_tps + bucket.egress_tps,
                },
                None => bucket.clone(),
            }
        })
        .collect();
    merged
}

#[derive(Default)]
struct AppAccumulator {
    requests: u32,
    ingress: u64,
    egress: u64,
    durations: f64,
    durations_seen: u32,
    last_seen: f64,
    models: BTreeMap<String, ModelAccumulator>,
}

#[derive(Default)]
struct ModelAccumulator {
    requests: u32,
    ingress: u64,
    egress: u64,
    durations: f64,
}

/// One slice of the timeline.
#[derive(Default)]
struct BucketAccumulator {
    requests: u32,
    ingress: u64,
    egress: u64,
}

impl AppAccumulator {
    fn push(
        &mut self,
        model: String,
        ingress: u64,
        egress: u64,
        ts: f64,
        duration_ms: Option<f64>,
    ) {
        self.requests = self.requests.saturating_add(1);
        self.ingress = self.ingress.saturating_add(ingress);
        self.egress = self.egress.saturating_add(egress);
        if let Some(ms) = duration_ms {
            self.durations += ms.max(0.0);
            self.durations_seen = self.durations_seen.saturating_add(1);
        }
        self.last_seen = self.last_seen.max(ts);
        let entry = self.models.entry(model).or_default();
        entry.requests = entry.requests.saturating_add(1);
        entry.ingress = entry.ingress.saturating_add(ingress);
        entry.egress = entry.egress.saturating_add(egress);
        // The model's rate uses only the requests that reported a duration.
        if let Some(ms) = duration_ms {
            entry.durations += ms.max(0.0);
        }
    }
}

fn finish_apps(apps: BTreeMap<String, AppAccumulator>) -> Vec<TokenAppView> {
    apps.into_iter()
        .map(|(app, acc)| {
            let mut models: Vec<TokenModelView> = acc
                .models
                .into_iter()
                .map(|(model, model_acc)| TokenModelView {
                    model,
                    requests: model_acc.requests,
                    ingress_tokens: model_acc.ingress,
                    egress_tokens: model_acc.egress,
                    ingress_tps: rate(model_acc.ingress, model_acc.durations),
                    egress_tps: rate(model_acc.egress, model_acc.durations),
                })
                .collect();
            models.sort_by(|a, b| {
                b.ingress_tokens
                    .cmp(&a.ingress_tokens)
                    .then(b.egress_tokens.cmp(&a.egress_tokens))
            });
            models.truncate(MAX_MODELS_PER_APP);
            TokenAppView {
                app,
                requests: acc.requests,
                ingress_tokens: acc.ingress,
                egress_tokens: acc.egress,
                ingress_tps: rate(acc.ingress, acc.durations),
                egress_tps: rate(acc.egress, acc.durations),
                avg_duration_ms: (acc.durations_seen > 0)
                    .then(|| acc.durations / f64::from(acc.durations_seen)),
                last_seen_s: (acc.last_seen > 0.0)
                    .then(|| super::nonnegative_f64_to_u64(acc.last_seen)),
                models,
            }
        })
        .collect()
}

/// The timeline as the tab charts it: one point per bucket, oldest first.
/// A bucket with no requests is left at zero — the usage history only
/// holds the newest rows, so its start is quiet rather than missing.
fn finish_buckets(
    buckets: Vec<BucketAccumulator>,
    now: Option<f64>,
    bucket_seconds: f64,
) -> Vec<TokenBucketView> {
    let Some(now) = now else {
        return Vec::new();
    };
    let newest_start = now - (TIMELINE_POINTS - 1) as f64 * bucket_seconds;
    buckets
        .into_iter()
        .enumerate()
        .map(|(index, bucket)| TokenBucketView {
            at_s: super::nonnegative_f64_to_u64(
                (newest_start + index as f64 * bucket_seconds).max(0.0),
            ),
            requests: bucket.requests,
            ingress_tps: rate(bucket.ingress, bucket_seconds * 1000.0),
            egress_tps: rate(bucket.egress, bucket_seconds * 1000.0),
        })
        .collect()
}

/// Ollama's `/api/ollama/stats`: installed models, which are loaded, and
/// what each holds.
fn ollama_engine(stats: &Value) -> TokenEngineView {
    let mut engine = TokenEngineView::empty("ollama");
    let Some(models) = stats.get("models").and_then(Value::as_array) else {
        return engine;
    };
    engine.available = true;
    engine.model_count = u32::try_from(models.len()).unwrap_or(u32::MAX);
    engine.models = models
        .iter()
        .take(MAX_ENGINE_MODELS)
        .map(|model| {
            let details = model.get("details");
            TokenEngineModelView {
                model: text(model, "name")
                    .or_else(|| text(model, "model"))
                    .unwrap_or_else(|| "?".to_owned()),
                running: model
                    .get("running")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                size_bytes: super::nonnegative_f64_to_u64(f64_at(model, "size").unwrap_or(0.0)),
                size_vram_bytes: f64_at(model, "size_vram").map(super::nonnegative_f64_to_u64),
                parameter_size: details
                    .and_then(|d| text(d, "parameter_size"))
                    .unwrap_or_default(),
                quantization: details
                    .and_then(|d| text(d, "quantization_level"))
                    .unwrap_or_default(),
                expires_in_s: None,
            }
        })
        .collect();
    engine.loaded_count = u32::try_from(engine.models.iter().filter(|model| model.running).count())
        .unwrap_or(u32::MAX);
    engine.vram_total_bytes =
        super::nonnegative_f64_to_u64(f64_at(stats, "total_vram_bytes").unwrap_or(0.0));
    engine
}

/// Unix seconds `hours` ago, or `None` for an unknown node clock.
fn window_cutoff(hours: u32) -> Option<f64> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs_f64();
    Some(now - f64::from(hours) * 3600.0)
}

fn f64_at(value: &Value, key: &str) -> Option<f64> {
    value
        .get(key)
        .and_then(Value::as_f64)
        .filter(|number| number.is_finite())
}

fn text(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(|text| text.chars().take(128).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(service: &str, model: &str, ingress: f64, egress: f64, ts: f64) -> String {
        format!(
            r#"{{"service":"{service}","model":"{model}","prompt_tokens":{ingress},
                "completion_tokens":{egress},"total_tokens":0,"duration_ms":2500,"ts":{ts}}}"#
        )
    }

    fn usage(rows: &[String]) -> Value {
        serde_json::json!({ "usage": rows.iter().map(|row| serde_json::from_str(row).unwrap()).collect::<Vec<Value>>() })
    }

    fn now() -> f64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64()
    }

    #[test]
    fn flow_is_grouped_per_app_and_model_within_the_window() {
        let now = now();
        let rows = vec![
            row("quai-radar", "qwen3.8:27b", 100.0, 900.0, now - 60.0),
            row("quai-radar", "qwen3.8:27b", 50.0, 400.0, now - 120.0),
            row("quai-radar", "nomic-embed-text", 30.0, 0.0, now - 300.0),
            row("other-app", "llama3", 10.0, 20.0, now - 600.0),
        ];
        let tokens = TokenPoll::new(24, 500).apply(&usage(&rows), None);
        assert_eq!(tokens.apps.len(), 2);
        let radar = tokens
            .apps
            .iter()
            .find(|app| app.app == "quai-radar")
            .unwrap();
        assert_eq!(radar.requests, 3);
        assert_eq!(radar.ingress_tokens, 180);
        assert_eq!(radar.egress_tokens, 1300);
        assert_eq!(radar.models.len(), 2);
        assert_eq!(radar.models[0].model, "qwen3.8:27b", "most ingress first");
        assert_eq!(radar.models[0].egress_tokens, 1300);
        assert_eq!(radar.avg_duration_ms, Some(2500.0));
        assert!(radar.last_seen_s.is_some());
        assert_eq!(tokens.ingress(), 190);
        assert_eq!(tokens.egress(), 1320);
    }

    #[test]
    fn rows_older_than_the_window_are_left_out() {
        let rows = vec![row("stale-app", "old", 10.0, 10.0, now() - 7_200.0)];
        let tokens = TokenPoll::new(1, 500).apply(&usage(&rows), None);
        assert!(tokens.apps.is_empty(), "2h old, outside 1h");
        let wide = TokenPoll::new(24, 500).apply(&usage(&rows), None);
        assert_eq!(wide.apps.len(), 1, "inside 24h");
    }

    #[test]
    fn token_totals_saturate_and_single_bucket_timelines_merge() {
        let tokens = NodeTokens {
            apps: vec![
                TokenAppView {
                    ingress_tokens: u64::MAX,
                    egress_tokens: u64::MAX,
                    requests: u32::MAX,
                    ..TokenAppView::default()
                },
                TokenAppView {
                    ingress_tokens: 1,
                    egress_tokens: 1,
                    requests: 1,
                    ..TokenAppView::default()
                },
            ],
            ..NodeTokens::default()
        };
        assert_eq!(tokens.ingress(), u64::MAX);
        assert_eq!(tokens.egress(), u64::MAX);
        assert_eq!(tokens.requests(), u32::MAX);

        let bucket = TokenBucketView {
            at_s: u64::MAX,
            ..TokenBucketView::default()
        };
        let merged = merge_timeline(std::slice::from_ref(&bucket), std::slice::from_ref(&bucket));
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].at_s, u64::MAX);
    }

    #[test]
    fn ollama_models_are_read_installed_and_loaded() {
        let stats: Value = serde_json::from_str(
            r#"{"total_models": 2, "total_vram_bytes": 8589934592, "models": [
                {"name": "qwen3.8:27b", "size": 17741872154, "running": true,
                 "size_vram": 8589934592, "details": {"parameter_size": "27B", "quantization_level": "Q4_K_M"}},
                {"name": "llama3:latest", "size": 4294967296, "running": false, "details": {}}
            ]}"#,
        )
        .unwrap();
        let tokens = TokenPoll::new(24, 500).apply(&serde_json::json!({"usage": []}), Some(&stats));
        assert_eq!(tokens.engines.len(), 1);
        let ollama = &tokens.engines[0];
        assert_eq!(ollama.engine, "ollama");
        assert!(ollama.available);
        assert_eq!(ollama.model_count, 2);
        assert_eq!(ollama.loaded_count, 1);
        assert_eq!(ollama.vram_total_bytes, 8_589_934_592);
        assert_eq!(ollama.models[0].model, "qwen3.8:27b", "loaded first");
        assert!(ollama.models[0].running);
        assert_eq!(ollama.models[0].parameter_size, "27B");
        assert_eq!(ollama.models[0].quantization, "Q4_K_M");
        assert_eq!(ollama.models[0].size_vram_bytes, Some(8_589_934_592));
        assert!(!ollama.models[1].running);
    }

    #[test]
    fn merging_two_nodes_separates_equal_app_names_and_sums_totals() {
        let rows = vec![row("quai-radar", "m", 10.0, 20.0, now())];
        let tokens = TokenPoll::new(24, 500).apply(&usage(&rows), None);
        let mut view = TokensView {
            enabled: true,
            ..TokensView::default()
        };
        merge(&mut view, "axiom", tokens.clone());
        merge(&mut view, "axiom-dgx", tokens);
        assert_eq!(view.apps.len(), 2);
        let names: Vec<&str> = view.apps.iter().map(|app| app.app.as_str()).collect();
        assert!(names.contains(&"quai-radar"), "{names:?}");
        assert!(names.contains(&"axiom-dgx/quai-radar"), "{names:?}");
        assert_eq!(view.total_ingress, 20);
        assert_eq!(view.total_egress, 40);
        assert_eq!(view.total_requests, 2);
        assert!(!view.idle);
        assert_eq!(view.window_label, "24h");
    }

    #[test]
    fn an_empty_window_reads_as_idle_not_broken() {
        let tokens = TokenPoll::new(24, 500).apply(&serde_json::json!({}), None);
        assert!(tokens.apps.is_empty());
        assert!(tokens.engines.is_empty());
        let mut view = TokensView {
            enabled: true,
            ..TokensView::default()
        };
        merge(&mut view, "axiom", tokens);
        assert!(view.idle);
        assert_eq!(view.total_requests, 0);
    }
}
