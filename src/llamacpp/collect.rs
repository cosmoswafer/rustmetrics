//! Collecting metrics from a llama.cpp `llama-server`.
//!
//! Works against a single instance or a router: `/v1/models` lists managed
//! models with their load state, and `/slots` + `/metrics` are scoped to one
//! model via `?model=`. Throughput comes from Prometheus gauges or passive
//! `/slots` token-counter deltas; the TUI never sends generation requests.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use crate::model::SeriesKey;
use crate::textfmt;

// ---------------------------------------------------------------------------
// rolling series
// ---------------------------------------------------------------------------

/// One chart point: unix seconds + value.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    pub t: f64,
    pub v: f64,
}

/// Rolling time series capped by both point count and age.
#[derive(Debug, Clone, Default)]
pub struct Series {
    buf: VecDeque<Sample>,
    cap: usize,
    window: f64,
}

impl Series {
    pub fn new(cap: usize, window: f64) -> Self {
        Series {
            buf: VecDeque::new(),
            cap: cap.max(2),
            window,
        }
    }

    pub fn push(&mut self, t: f64, v: f64) {
        self.buf.push_back(Sample { t, v });
        self.prune(t);
    }

    pub fn prune(&mut self, now: f64) {
        while self.buf.len() > self.cap {
            self.buf.pop_front();
        }
        let cutoff = now - self.window;
        while self.buf.front().is_some_and(|s| s.t < cutoff) {
            self.buf.pop_front();
        }
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn values(&self) -> Vec<f64> {
        self.buf.iter().map(|s| s.v).collect()
    }

    pub fn last(&self) -> Option<Sample> {
        self.buf.back().copied()
    }

    pub fn max(&self) -> Option<f64> {
        self.buf.iter().map(|s| s.v).reduce(f64::max)
    }

    pub fn avg(&self) -> Option<f64> {
        if self.buf.is_empty() {
            return None;
        }
        Some(self.buf.iter().map(|s| s.v).sum::<f64>() / self.buf.len() as f64)
    }
}

// ---------------------------------------------------------------------------
// server state snapshots
// ---------------------------------------------------------------------------

/// One entry of `/v1/models`: a managed model and its lifecycle state.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelInfo {
    pub name: String,
    /// `loaded`, `unloaded`, `loading`, ... Absent on plain instances.
    pub state: String,
    pub ctx: Option<u64>,
    pub hf_repo: Option<String>,
    pub main_gpu: Option<String>,
    pub gpu_layers: Option<String>,
    pub spec_type: Option<String>,
}

/// One slot of `/slots?model=X`.
#[derive(Debug, Clone, PartialEq)]
pub struct SlotInfo {
    pub id: u64,
    pub task_id: Option<i64>,
    pub processing: bool,
    pub n_ctx: u64,
    pub prompt_tokens: u64,
    pub tokens_processed: u64,
    pub decoded_tokens: u64,
    pub tokens_cache: u64,
    pub temperature: Option<f64>,
}

/// Last observed counters for deriving passive per-slot throughput.
#[derive(Debug, Clone, Copy)]
struct SlotProgress {
    sampled_at: f64,
    task_id: Option<i64>,
    prompt_processed: u64,
    decoded: u64,
}

#[derive(Debug, Clone, Copy, Default)]
struct SlotRates {
    generation: Option<f64>,
    prompt: Option<f64>,
}

/// Result of one generation probe.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProbeResult {
    pub model: String,
    pub wall_ms: f64,
    /// Server-measured prompt processing throughput (tok/s).
    pub prompt_tps: f64,
    /// Server-measured generation throughput (tok/s).
    pub gen_tps: f64,
    pub prompt_n: u64,
    pub gen_n: u64,
    pub cache_n: u64,
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// shared state
// ---------------------------------------------------------------------------

/// Everything the TUI renders. Shared between poller, prober and renderer;
/// mutation happens under the mutex in short critical sections only.
#[derive(Debug)]
pub struct State {
    pub base: String,
    pub window: f64,
    pub probe_enabled: bool,
    pub show_metrics: bool,

    pub up: bool,
    pub last_error: Option<String>,
    pub role: Option<String>,
    pub build: Option<String>,

    pub models: Vec<ModelInfo>,
    pub slots: Vec<SlotInfo>,
    /// Pinned model name (`Tab` cycles, `--model` pins at startup).
    pub focus: Option<String>,

    pub last_probe: Option<ProbeResult>,
    pub probes_ok: u64,
    pub probes_err: u64,

    /// None = not probed yet, Some(true) = `/metrics` parsed, Some(false) = unsupported.
    pub metrics_supported: Option<bool>,
    pub prom_discovered: usize,

    pub series: HashMap<String, Series>,
    pub started: Instant,
    cap: usize,
    slot_progress: HashMap<(String, u64), SlotProgress>,
}

impl State {
    pub fn new(base: impl Into<String>, window: f64, min_interval: f64) -> Self {
        let cap = ((window / min_interval.max(0.05)).ceil() as usize).clamp(128, 4096);
        State {
            base: base.into(),
            window,
            probe_enabled: true,
            show_metrics: true,
            up: false,
            last_error: None,
            role: None,
            build: None,
            models: Vec::new(),
            slots: Vec::new(),
            focus: None,
            last_probe: None,
            probes_ok: 0,
            probes_err: 0,
            metrics_supported: None,
            prom_discovered: 0,
            series: HashMap::new(),
            started: Instant::now(),
            cap,
            slot_progress: HashMap::new(),
        }
    }

    pub fn push(&mut self, key: &str, t: f64, v: f64) {
        self.series
            .entry(key.to_string())
            .or_insert_with(|| Series::new(self.cap, self.window))
            .push(t, v);
    }

    pub fn loaded_count(&self) -> usize {
        self.models.iter().filter(|m| m.state == "loaded").count()
    }

    pub fn uptime_secs(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }

    /// The model the UI is watching: an explicit pin, else the first active
    /// model. Inactive router models are hidden from automatic selection.
    pub fn focus_model(&self) -> Option<&ModelInfo> {
        if let Some(f) = &self.focus {
            if let Some(m) = self.models.iter().find(|m| &m.name == f) {
                return Some(m);
            }
        }
        self.models.iter().find(|m| visible_by_default(m))
    }

    /// Pin the next active model in the sorted list (`Tab`).
    pub fn cycle_focus(&mut self) {
        let names: Vec<&str> = self
            .models
            .iter()
            .filter(|model| visible_by_default(model))
            .map(|m| m.name.as_str())
            .collect();
        if names.is_empty() {
            return;
        }
        let next = match &self.focus {
            Some(f) => names
                .iter()
                .position(|n| n == f)
                .map(|i| (i + 1) % names.len())
                .unwrap_or(0),
            None => 0,
        };
        self.focus = Some(names[next].to_string());
    }
}

fn visible_by_default(model: &ModelInfo) -> bool {
    // Plain single-instance servers do not report a lifecycle state; their one
    // model is active by definition. Loading models are also useful to watch.
    matches!(model.state.as_str(), "loaded" | "loading" | "unknown")
}

// ---------------------------------------------------------------------------
// collector
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct HttpError {
    pub status: Option<u16>,
    pub message: String,
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.status {
            Some(code) => write!(f, "HTTP {code}: {}", self.message),
            None => write!(f, "{}", self.message),
        }
    }
}

enum PromErr {
    /// The server (or router child) has no metrics endpoint: 400/404/501.
    Unsupported,
    Failed(String),
}

/// Polls a llama.cpp server and commits results into `State`.
#[derive(Clone)]
pub struct Collector {
    base: String,
    poll_http: reqwest::Client,
    probe_http: reqwest::Client,
    api_key: Option<String>,
    pub model: Option<String>,
    probe_tokens: u32,
}

impl Collector {
    pub fn new(
        base: &str,
        model: Option<String>,
        probe_tokens: u32,
        api_key: Option<String>,
    ) -> Self {
        Collector {
            base: base.trim_end_matches('/').to_string(),
            poll_http: client_with(Duration::from_secs(10)),
            // a probe may trigger a cold model load (HF download included)
            probe_http: client_with(Duration::from_secs(180)),
            api_key,
            model,
            probe_tokens,
        }
    }

    fn url(&self, path: &str) -> reqwest::Url {
        reqwest::Url::parse(&format!("{}/{}", self.base, path.trim_start_matches('/')))
            .expect("validated base URL")
    }

    fn url_with_model(&self, path: &str, model: &str) -> reqwest::Url {
        let mut url = self.url(path);
        url.query_pairs_mut().append_pair("model", model);
        url
    }

    fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.api_key {
            Some(k) => req.header("Authorization", format!("Bearer {k}")),
            None => req,
        }
    }

    async fn get_text(&self, url: reqwest::Url) -> Result<String, HttpError> {
        let resp = self
            .auth(self.poll_http.get(url))
            .send()
            .await
            .map_err(|e| HttpError {
                status: None,
                message: e.to_string(),
            })?;
        let status = resp.status();
        let body = resp.text().await.map_err(|e| HttpError {
            status: Some(status.as_u16()),
            message: e.to_string(),
        })?;
        if !status.is_success() {
            return Err(HttpError {
                status: Some(status.as_u16()),
                message: summarize_error(&body),
            });
        }
        Ok(body)
    }

    async fn get_json(&self, url: reqwest::Url) -> Result<serde_json::Value, HttpError> {
        let body = self.get_text(url).await?;
        serde_json::from_str(&body).map_err(|e| HttpError {
            status: None,
            message: format!("invalid JSON: {e}"),
        })
    }

    async fn post_json(
        &self,
        url: reqwest::Url,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value, HttpError> {
        let resp = self
            .auth(self.probe_http.post(url))
            .header("Content-Type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| HttpError {
                status: None,
                message: e.to_string(),
            })?;
        let status = resp.status();
        let text = resp.text().await.map_err(|e| HttpError {
            status: Some(status.as_u16()),
            message: e.to_string(),
        })?;
        let v: serde_json::Value = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
        if !status.is_success() {
            return Err(HttpError {
                status: Some(status.as_u16()),
                message: summarize_error(&text),
            });
        }
        Ok(v)
    }

    /// One collection cycle. Never panics; failures land in `State::last_error`.
    pub async fn poll(&self, st: &Arc<Mutex<State>>) {
        // 1) liveness + identity + model list
        let health = self.get_json(self.url("health")).await;
        let up = matches!(&health, Ok(v) if v.get("error").is_none());
        let mut cycle_error = health
            .as_ref()
            .err()
            .map(|error| format!("health: {error}"));
        let props = self.get_json(self.url("props")).await.ok();
        let role = str_field(props.as_ref(), "role");
        let build = str_field(props.as_ref(), "build_info");
        let models = match self.get_json(self.url("v1/models")).await {
            Ok(v) => parse_models(&v),
            Err(_) => Vec::new(),
        };

        // 2) resolve what to fetch for the focused model
        let (focus, loaded, metrics_enabled) = {
            let mut g = st.lock().unwrap();
            g.up = up;
            g.role = role;
            g.build = build;
            g.models = models;
            let focus = g
                .focus
                .clone()
                .or_else(|| {
                    g.models
                        .iter()
                        .find(|m| m.state == "loaded")
                        .map(|m| m.name.clone())
                })
                .or_else(|| g.models.first().map(|m| m.name.clone()));
            let instance_like =
                !g.models.is_empty() && g.models.iter().all(|m| m.state == "unknown");
            let loaded = match &focus {
                Some(f) => {
                    let state = g
                        .models
                        .iter()
                        .find(|m| &m.name == f)
                        .map(|m| m.state.as_str())
                        .unwrap_or("unknown");
                    state == "loaded" || (instance_like && state == "unknown")
                }
                None => false,
            };
            let metrics_enabled = g.metrics_supported.unwrap_or(true);
            (focus, loaded, metrics_enabled)
        };

        // 3) slots + prometheus metrics (awaiting outside the lock). Fetch
        // concurrently: llama.cpp can block /slots during prompt processing,
        // which must not delay the independent real-time metrics snapshot.
        // Only touch loaded models so router monitoring never triggers a load.
        let slots_request = async {
            match (loaded, focus.as_deref()) {
                (true, Some(model)) => {
                    let result = self.get_json(self.url_with_model("slots", model)).await;
                    Some((unix_now(), result))
                }
                _ => None,
            }
        };
        let prom_request = async {
            match (metrics_enabled && loaded, focus.as_deref()) {
                (true, Some(model)) => Some(self.fetch_metrics(model).await),
                _ => None,
            }
        };
        // A busy llama.cpp child may defer /slots until prompt processing is
        // complete. Bound that wait so it cannot stall the Prometheus commit.
        let slots_request = tokio::time::timeout(Duration::from_millis(500), slots_request);
        let (slots_result, prom) = tokio::join!(slots_request, prom_request);
        let (slots_sampled_at, slots, slots_failed) = match slots_result {
            Ok(Some((sampled_at, Ok(value)))) => (sampled_at, Some(parse_slots(&value)), false),
            Ok(Some((sampled_at, Err(error)))) => {
                cycle_error = Some(format!("slots: {error}"));
                (sampled_at, None, true)
            }
            Ok(None) => (0.0, None, false),
            Err(_busy) => (0.0, None, true),
        };
        let prom_metric = |name: &str| match &prom {
            Some(Ok(series)) => series
                .iter()
                .find(|(id, _)| id == name)
                .map(|(_, value)| *value),
            _ => None,
        };
        let prom_generation = prom_metric("llamacpp:predicted_tokens_seconds");
        let prom_prompt = prom_metric("llamacpp:prompt_tokens_seconds");

        // 4) commit
        let now = unix_now();
        let mut g = st.lock().unwrap();
        if loaded {
            let slot_rates = slots
                .as_deref()
                .zip(focus.as_deref())
                .map(|(slots, model)| {
                    derive_slot_rates(&g.slot_progress, model, slots, slots_sampled_at)
                })
                .unwrap_or_default();
            if let Some(model) = focus.as_deref() {
                if let Some(slots) = slots.as_deref() {
                    update_slot_progress(&mut g.slot_progress, model, slots, slots_sampled_at);
                    let busy = slots.iter().filter(|s| s.processing).count() as f64;
                    let cache = slots.iter().map(|s| s.tokens_cache).sum::<u64>();
                    let used = slots.iter().map(|s| s.prompt_tokens).sum::<u64>();
                    let total = slots.iter().map(|s| s.n_ctx).sum::<u64>();
                    g.push(&format!("busy/{model}"), now, busy);
                    g.push(&format!("cache_tokens/{model}"), now, cache as f64);
                    if total > 0 {
                        g.push(
                            &format!("ctx_pct/{model}"),
                            now,
                            used as f64 / total as f64 * 100.0,
                        );
                    }
                }
                // Prefer the server's instantaneous gauges. Counter deltas keep
                // the charts live when only the metrics proxy is unavailable.
                if let Some(value) = prom_generation.or(slot_rates.generation) {
                    g.push(&format!("gen_tps/{model}"), now, value);
                }
                if let Some(value) = prom_prompt.or(slot_rates.prompt) {
                    g.push(&format!("prompt_tps/{model}"), now, value);
                }
            }
            if let Some(slots) = slots {
                g.slots = slots;
            }
            if slots_failed {
                // The next successful response is a new baseline, not a rate
                // averaged across a period with missing slot observations.
                g.slot_progress.clear();
            }
        } else {
            g.slots.clear();
            g.slot_progress.clear();
        }
        match prom {
            Some(Ok(series)) => {
                g.metrics_supported = Some(true);
                g.prom_discovered = series.len();
                for (id, value) in series {
                    g.push(&format!("prom/{id}"), now, value);
                }
            }
            Some(Err(PromErr::Unsupported)) => {
                if g.metrics_supported.is_none() {
                    g.metrics_supported = Some(false);
                }
            }
            Some(Err(PromErr::Failed(e))) => {
                cycle_error = Some(format!("metrics: {e}"));
            }
            None => {}
        }
        // Errors describe the latest completed cycle, rather than remaining
        // stuck in the header after the endpoint has recovered.
        g.last_error = cycle_error;
        for s in g.series.values_mut() {
            s.prune(now);
        }
    }

    async fn fetch_metrics(&self, model: &str) -> Result<Vec<(String, f64)>, PromErr> {
        let body = match self.get_text(self.url_with_model("metrics", model)).await {
            Ok(b) => b,
            Err(e) if matches!(e.status, Some(400) | Some(404) | Some(501)) => {
                return Err(PromErr::Unsupported);
            }
            Err(e) => return Err(PromErr::Failed(e.to_string())),
        };
        // a router may forward a JSON error body with HTTP 200; guard anyway
        if body.trim_start().starts_with('{') {
            return Err(PromErr::Unsupported);
        }
        let parsed = textfmt::parse(&body).map_err(|e| PromErr::Failed(e.to_string()))?;
        let mut values: HashMap<String, f64> = HashMap::new();
        for s in parsed.samples {
            values.insert(series_id(&s.key), s.value);
        }
        let mut out: Vec<(String, f64)> = values.into_iter().collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    /// Send a tiny generation probe and record server-measured throughput.
    pub async fn probe(&self, st: &Arc<Mutex<State>>) {
        let target = {
            let g = st.lock().unwrap();
            match &self.model {
                // explicit pin: probing may load the model, that is intended
                Some(m) => Some(m.clone()),
                // otherwise only probe something already loaded, so monitoring
                // never triggers a (potentially huge) surprise model load
                None => g
                    .models
                    .iter()
                    .find(|m| m.state == "loaded")
                    .map(|m| m.name.clone()),
            }
        };
        let Some(model) = target else {
            return;
        };

        let payload = serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": self.probe_tokens,
            "temperature": 0.0,
            "stream": false,
        });
        let started = Instant::now();
        let resp = self
            .post_json(self.url("v1/chat/completions"), &payload)
            .await;
        let wall_ms = started.elapsed().as_secs_f64() * 1000.0;

        let mut pr = ProbeResult {
            model,
            wall_ms,
            ..ProbeResult::default()
        };
        match resp {
            Ok(v) => {
                let chat: ChatResp = serde_json::from_value(v).unwrap_or_default();
                if let Some(e) = chat.error {
                    pr.error = Some(error_message(&e));
                }
                if let Some(t) = chat.timings {
                    pr.prompt_tps = t.prompt_per_second;
                    pr.gen_tps = t.predicted_per_second;
                    pr.prompt_n = t.prompt_n as u64;
                    pr.gen_n = t.predicted_n;
                    pr.cache_n = t.cache_n;
                }
                if let Some(u) = chat.usage {
                    if pr.prompt_n == 0 {
                        pr.prompt_n = u.prompt_tokens;
                    }
                    if pr.gen_n == 0 {
                        pr.gen_n = u.completion_tokens;
                    }
                }
                if pr.gen_tps == 0.0 && pr.gen_n > 0 && wall_ms > 0.0 {
                    pr.gen_tps = pr.gen_n as f64 / (wall_ms / 1000.0);
                }
            }
            Err(e) => pr.error = Some(e.to_string()),
        }

        let now = unix_now();
        let mut g = st.lock().unwrap();
        if pr.error.is_some() {
            g.probes_err += 1;
        } else {
            g.probes_ok += 1;
            let m = pr.model.clone();
            g.push(&format!("gen_tps/{m}"), now, pr.gen_tps);
            g.push(&format!("prompt_tps/{m}"), now, pr.prompt_tps);
            g.push(&format!("wall_ms/{m}"), now, pr.wall_ms);
        }
        g.last_probe = Some(pr);
    }
}

fn derive_slot_rates(
    previous: &HashMap<(String, u64), SlotProgress>,
    model: &str,
    slots: &[SlotInfo],
    sampled_at: f64,
) -> SlotRates {
    let mut generation = 0.0;
    let mut prompt = 0.0;
    let mut matched = 0usize;

    for slot in slots {
        let key = (model.to_string(), slot.id);
        let Some(previous) = previous.get(&key) else {
            continue;
        };
        let elapsed = sampled_at - previous.sampled_at;
        if elapsed <= 0.0
            || slot.task_id != previous.task_id
            || slot.tokens_processed < previous.prompt_processed
            || slot.decoded_tokens < previous.decoded
        {
            // A new request can reset either counter. Use this observation as
            // a fresh baseline rather than reporting a meaningless rate.
            continue;
        }
        generation += (slot.decoded_tokens - previous.decoded) as f64 / elapsed;
        prompt += (slot.tokens_processed - previous.prompt_processed) as f64 / elapsed;
        matched += 1;
    }

    if matched == 0 {
        SlotRates::default()
    } else {
        SlotRates {
            generation: Some(generation),
            prompt: Some(prompt),
        }
    }
}

fn update_slot_progress(
    previous: &mut HashMap<(String, u64), SlotProgress>,
    model: &str,
    slots: &[SlotInfo],
    sampled_at: f64,
) {
    // Do not compare counters across long gaps while another model was focused.
    previous.retain(|(name, _), _| name == model);
    for slot in slots {
        previous.insert(
            (model.to_string(), slot.id),
            SlotProgress {
                sampled_at,
                task_id: slot.task_id,
                prompt_processed: slot.tokens_processed,
                decoded: slot.decoded_tokens,
            },
        );
    }
}

fn client_with(timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .expect("static client config")
}

fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn str_field(v: Option<&serde_json::Value>, key: &str) -> Option<String> {
    v?.get(key)?.as_str().map(str::to_string)
}

fn error_message(v: &serde_json::Value) -> String {
    let msg = v
        .get("message")
        .and_then(|m| m.as_str())
        .unwrap_or("unknown error");
    truncate(msg, 160)
}

fn summarize_error(body: &str) -> String {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(e) = v.get("error") {
            return error_message(e);
        }
    }
    truncate(body.trim(), 160)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}...")
}

/// Canonical `name{k="v",...}` id of a parsed series.
fn series_id(key: &SeriesKey) -> String {
    if key.labels.is_empty() {
        return key.name.to_string();
    }
    let labels: Vec<String> = key
        .labels
        .iter()
        .map(|(n, v)| format!("{}={}", n, v.as_str()))
        .collect();
    format!("{}{{{}}}", key.name, labels.join(","))
}

// ---------------------------------------------------------------------------
// payload parsing (defensive: servers vary across llama.cpp versions)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ModelsResp {
    #[serde(default)]
    data: Vec<ModelEntry>,
}

#[derive(Deserialize)]
struct ModelEntry {
    id: String,
    #[serde(default)]
    status: Option<serde_json::Value>,
}

pub fn parse_models(v: &serde_json::Value) -> Vec<ModelInfo> {
    let Ok(resp) = serde_json::from_value::<ModelsResp>(v.clone()) else {
        return Vec::new();
    };
    let mut out: Vec<ModelInfo> = resp
        .data
        .into_iter()
        .map(|e| {
            let mut m = ModelInfo {
                name: e.id,
                state: "unknown".to_string(),
                ctx: None,
                hf_repo: None,
                main_gpu: None,
                gpu_layers: None,
                spec_type: None,
            };
            match e.status {
                Some(st) if st.is_object() => {
                    if let Some(val) = st.get("value").and_then(|v| v.as_str()) {
                        m.state = val.to_string();
                    }
                    if let Some(arr) = st.get("args").and_then(|v| v.as_array()) {
                        let args: Vec<String> = arr
                            .iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect();
                        scan_instance_args(&mut m, &args);
                    }
                }
                Some(st) if st.is_string() => {
                    m.state = st.as_str().unwrap_or("unknown").to_string();
                }
                _ => {}
            }
            m
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn scan_instance_args(m: &mut ModelInfo, args: &[String]) {
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--ctx-size" => m.ctx = it.next().and_then(|v| v.parse().ok()),
            "--hf-repo" => m.hf_repo = it.next().cloned(),
            "--main-gpu" => m.main_gpu = it.next().cloned(),
            "--n-gpu-layers" => m.gpu_layers = it.next().cloned(),
            "--spec-type" => m.spec_type = it.next().cloned(),
            _ => {}
        }
    }
}

#[derive(Deserialize)]
struct SlotEntry {
    #[serde(default)]
    id: Option<u64>,
    #[serde(default)]
    id_task: Option<i64>,
    #[serde(default)]
    is_processing: Option<bool>,
    #[serde(default)]
    n_ctx: Option<u64>,
    #[serde(default)]
    n_prompt_tokens: Option<u64>,
    #[serde(default)]
    n_prompt_tokens_processed: Option<u64>,
    #[serde(default)]
    n_decoded: Option<u64>,
    #[serde(default)]
    next_token: Option<NextToken>,
    #[serde(default)]
    n_prompt_tokens_cache: Option<u64>,
    #[serde(default)]
    params: Option<SlotParams>,
}

#[derive(Deserialize)]
struct NextToken {
    #[serde(default)]
    n_decoded: Option<u64>,
}

#[derive(Deserialize)]
struct SlotParams {
    #[serde(default)]
    temperature: Option<f64>,
}

pub fn parse_slots(v: &serde_json::Value) -> Vec<SlotInfo> {
    let Ok(entries) = serde_json::from_value::<Vec<SlotEntry>>(v.clone()) else {
        return Vec::new();
    };
    entries
        .into_iter()
        .map(|e| SlotInfo {
            id: e.id.unwrap_or(0),
            task_id: e.id_task,
            processing: e.is_processing.unwrap_or(false),
            n_ctx: e.n_ctx.unwrap_or(0),
            prompt_tokens: e.n_prompt_tokens.unwrap_or(0),
            tokens_processed: e.n_prompt_tokens_processed.unwrap_or(0),
            decoded_tokens: e
                .n_decoded
                .or_else(|| e.next_token.and_then(|next| next.n_decoded))
                .unwrap_or(0),
            tokens_cache: e.n_prompt_tokens_cache.unwrap_or(0),
            temperature: e.params.and_then(|p| p.temperature),
        })
        .collect()
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ChatResp {
    timings: Option<Timings>,
    usage: Option<Usage>,
    error: Option<serde_json::Value>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Timings {
    prompt_per_second: f64,
    predicted_per_second: f64,
    prompt_n: f64,
    predicted_n: u64,
    cache_n: u64,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Usage {
    prompt_tokens: u64,
    completion_tokens: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const MODELS_JSON: &str = r#"{"data":[
        {"id":"lfm25","status":{"value":"loaded","args":["--ctx-size","32768","--hf-repo","unsloth/LFM2.5-8B-A1B-GGUF:UD-Q4_K_XL","--main-gpu","1","--n-gpu-layers","999"]}},
        {"id":"bonsai27","aliases":[],"tags":[],"object":"model","owned_by":"llamacpp","created":1793335948,
         "status":{"value":"unloaded","args":["/path/llama-server","--host","127.0.0.1","--port","0","--alias","bonsai27","--ctx-size","65536","--cache-type-k","q4_0","--flash-attn","on","--hf-repo","prism-ml/Bonsai-27B-gguf:Q1_0","--main-gpu","1","--n-gpu-layers","999","--parallel","1","--spec-type","draft-mtp"]}}
    ]}"#;

    #[test]
    fn parses_models_payload() {
        let v: serde_json::Value = serde_json::from_str(MODELS_JSON).unwrap();
        let models = parse_models(&v);
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].name, "bonsai27"); // sorted by name
        let bonsai = &models[0];
        assert_eq!(bonsai.state, "unloaded");
        assert_eq!(bonsai.ctx, Some(65536));
        assert_eq!(
            bonsai.hf_repo.as_deref(),
            Some("prism-ml/Bonsai-27B-gguf:Q1_0")
        );
        assert_eq!(bonsai.main_gpu.as_deref(), Some("1"));
        assert_eq!(bonsai.gpu_layers.as_deref(), Some("999"));
        assert_eq!(bonsai.spec_type.as_deref(), Some("draft-mtp"));
        assert_eq!(models[1].state, "loaded");
    }

    #[test]
    fn models_without_status_are_unknown() {
        let v = json!({"data": [{"id": "gemma4", "object": "model"}]});
        let models = parse_models(&v);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].state, "unknown");
        assert_eq!(models[0].ctx, None);
    }

    #[test]
    fn parses_slots_payload() {
        let v = json!([{
            "id": 0, "n_ctx": 32768, "speculative": false, "is_processing": true, "id_task": 42,
            "n_prompt_tokens": 11, "n_prompt_tokens_processed": 0, "n_prompt_tokens_cache": 0,
            "next_token": {"has_next_token": true, "n_decoded": 4},
            "params": {"seed": 4294967295i64, "temperature": 0.2}
        }]);
        let slots = parse_slots(&v);
        assert_eq!(slots.len(), 1);
        let s = &slots[0];
        assert!(s.processing);
        assert_eq!(s.task_id, Some(42));
        assert_eq!(s.n_ctx, 32768);
        assert_eq!(s.prompt_tokens, 11);
        assert_eq!(s.decoded_tokens, 4);
        assert_eq!(s.temperature, Some(0.2));
    }

    #[test]
    fn parses_probe_timings() {
        let v = json!({
            "choices": [{"finish_reason": "length", "message": {"content": ""}}],
            "model": "lfm25",
            "timings": {"cache_n": 0, "prompt_n": 10, "prompt_ms": 1446.8, "prompt_per_token_ms": 144.68,
                        "prompt_per_second": 6.911805363560962, "predicted_n": 16, "predicted_ms": 144.386,
                        "predicted_per_token_ms": 9.625, "predicted_per_second": 103.88818860554348},
            "usage": {"completion_tokens": 16, "prompt_tokens": 10, "total_tokens": 39,
                      "prompt_tokens_details": {"cached_tokens": 0}}
        });
        let chat: ChatResp = serde_json::from_value(v).unwrap();
        let t = chat.timings.unwrap();
        assert!((t.predicted_per_second - 103.888).abs() < 0.01);
        assert!((t.prompt_per_second - 6.9118).abs() < 0.001);
        assert_eq!(t.predicted_n, 16);
        assert_eq!(t.cache_n, 0);
    }

    #[test]
    fn parses_error_payload() {
        let v = json!({"error": {"code": 400, "message": "model 'x' not found",
                                 "type": "invalid_request_error"}});
        assert_eq!(
            error_message(v.get("error").unwrap()),
            "model 'x' not found"
        );
    }

    #[test]
    fn series_window_and_stats() {
        let mut s = Series::new(4, 60.0);
        for i in 0..10 {
            s.push(i as f64, i as f64);
        }
        assert_eq!(s.len(), 4);
        assert_eq!(s.max(), Some(9.0));
        assert_eq!(s.avg(), Some(7.5));
        assert_eq!(s.last().map(|x| x.v), Some(9.0));
        s.prune(100.0);
        assert!(s.is_empty());
    }

    #[test]
    fn derives_passive_slot_rates_and_handles_resets() {
        let slot = |prompt_processed, decoded| SlotInfo {
            id: 0,
            task_id: Some(7),
            processing: true,
            n_ctx: 4096,
            prompt_tokens: 100,
            tokens_processed: prompt_processed,
            decoded_tokens: decoded,
            tokens_cache: 0,
            temperature: None,
        };
        let mut previous = HashMap::new();
        update_slot_progress(&mut previous, "model", &[slot(100, 20)], 100.0);

        let rates = derive_slot_rates(&previous, "model", &[slot(300, 60)], 104.0);
        assert_eq!(rates.prompt, Some(50.0));
        assert_eq!(rates.generation, Some(10.0));

        update_slot_progress(&mut previous, "model", &[slot(300, 60)], 104.0);
        let reset = derive_slot_rates(&previous, "model", &[slot(5, 1)], 106.0);
        assert_eq!(reset.prompt, None);
        assert_eq!(reset.generation, None);

        let mut next_task = slot(1000, 1000);
        next_task.task_id = Some(8);
        let changed = derive_slot_rates(&previous, "model", &[next_task], 106.0);
        assert_eq!(changed.prompt, None);
        assert_eq!(changed.generation, None);
    }

    #[test]
    fn state_focus_and_cycle() {
        let mut st = State::new("http://x", 300.0, 2.0);
        st.models = vec![
            ModelInfo {
                name: "a".into(),
                state: "unloaded".into(),
                ctx: None,
                hf_repo: None,
                main_gpu: None,
                gpu_layers: None,
                spec_type: None,
            },
            ModelInfo {
                name: "b".into(),
                state: "loaded".into(),
                ctx: None,
                hf_repo: None,
                main_gpu: None,
                gpu_layers: None,
                spec_type: None,
            },
        ];
        // Auto-focus and cycling skip the unloaded model.
        assert_eq!(st.focus_model().unwrap().name, "b");
        st.cycle_focus();
        assert_eq!(st.focus.as_deref(), Some("b"));
        st.cycle_focus();
        assert_eq!(st.focus.as_deref(), Some("b"));

        // An explicit pin can still inspect an inactive model.
        st.focus = Some("a".into());
        assert_eq!(st.focus_model().unwrap().name, "a");
        st.focus = None;
        st.models[1].state = "unloaded".into();
        assert!(st.focus_model().is_none());
    }

    #[test]
    fn series_id_renders_labels() {
        let key = SeriesKey {
            name: crate::model::MetricName::parse("llamacpp:kv_cache_usage_ratio").unwrap(),
            labels: crate::model::Labels::new(vec![(
                crate::model::LabelName::parse("slot").unwrap(),
                crate::model::LabelValue::new("0"),
            )])
            .unwrap(),
        };
        assert_eq!(series_id(&key), "llamacpp:kv_cache_usage_ratio{slot=0}");
    }
}
