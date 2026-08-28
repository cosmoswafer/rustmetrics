//! HTTP API: axum router and handlers.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;

use crate::dashboard;
use crate::model::{LabelName, LabelValue, MetricName, TimestampMs};
use crate::storage::{MetricStore, QueryRange};
use crate::textfmt;

const DEFAULT_RANGE_MS: i64 = 3_600_000;
const AUTO_STEP_BUCKETS: i64 = 300;

pub struct AppState {
    pub store: Arc<MetricStore>,
    pub http_requests_total: AtomicU64,
}

impl AppState {
    pub fn new(store: Arc<MetricStore>) -> Self {
        AppState {
            store,
            http_requests_total: AtomicU64::new(0),
        }
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(get_dashboard))
        .route("/api/push", post(post_push))
        .route("/api/metrics", get(get_metrics_list))
        .route("/api/labels", get(get_labels))
        .route("/api/query", get(get_query))
        .route("/metrics", get(get_self_metrics))
        .fallback(fallback)
        .layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            count_requests,
        ))
        .with_state(state)
}

async fn count_requests(
    State(state): State<Arc<AppState>>,
    req: axum::extract::Request,
    next: Next,
) -> Response {
    state.http_requests_total.fetch_add(1, Ordering::Relaxed);
    next.run(req).await
}

/// JSON error body with a status code; the only error shape handlers return.
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        ApiError {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = serde_json::json!({ "error": self.message });
        (self.status, Json(body)).into_response()
    }
}

async fn fallback() -> ApiError {
    ApiError {
        status: StatusCode::NOT_FOUND,
        message: "not found".to_string(),
    }
}

async fn get_dashboard() -> Html<&'static str> {
    Html(dashboard::PAGE)
}

async fn post_push(
    State(state): State<Arc<AppState>>,
    body: String,
) -> Result<StatusCode, ApiError> {
    let parsed = textfmt::parse(&body).map_err(|e| ApiError::bad_request(e.to_string()))?;
    state
        .store
        .ingest(parsed.samples, parsed.metas, TimestampMs::now());
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
struct MetricsResponse {
    metrics: Vec<MetricEntry>,
}

#[derive(Serialize)]
struct MetricEntry {
    name: String,
    kind: &'static str,
    help: String,
    series: usize,
}

async fn get_metrics_list(State(state): State<Arc<AppState>>) -> Json<MetricsResponse> {
    let metrics = state
        .store
        .list_metrics()
        .into_iter()
        .map(|info| MetricEntry {
            name: info.name.as_str().to_string(),
            kind: info.meta.kind.as_str(),
            help: info.meta.help,
            series: info.series_count,
        })
        .collect();
    Json(MetricsResponse { metrics })
}

#[derive(Serialize)]
struct LabelsResponse {
    metric: String,
    labels: BTreeMap<String, Vec<String>>,
}

async fn get_labels(
    State(state): State<Arc<AppState>>,
    Query(params): Query<BTreeMap<String, String>>,
) -> Result<Json<LabelsResponse>, ApiError> {
    let metric = parse_metric_param(&params)?;
    let labels = state
        .store
        .label_values(&metric)
        .into_iter()
        .map(|(name, values)| {
            (
                name.as_str().to_string(),
                values.into_iter().map(|v| v.as_str().to_string()).collect(),
            )
        })
        .collect();
    Ok(Json(LabelsResponse {
        metric: metric.as_str().to_string(),
        labels,
    }))
}

#[derive(Serialize)]
struct QueryResponse {
    metric: String,
    from: i64,
    to: i64,
    step: i64,
    series: Vec<SeriesEntry>,
}

#[derive(Serialize)]
struct SeriesEntry {
    labels: BTreeMap<String, String>,
    /// [ts_ms, value] pairs; non-finite values become null.
    points: Vec<(i64, Option<f64>)>,
}

async fn get_query(
    State(state): State<Arc<AppState>>,
    Query(params): Query<BTreeMap<String, String>>,
) -> Result<Json<QueryResponse>, ApiError> {
    let metric = parse_metric_param(&params)?;

    let now = TimestampMs::now();
    let to = parse_ts_param(&params, "to", now)?;
    let from = parse_ts_param(&params, "from", to.saturating_sub_millis(DEFAULT_RANGE_MS))?;
    if from > to {
        return Err(ApiError::bad_request("query: from is after to"));
    }

    let step_ms = match params.get("step").map(String::as_str) {
        None | Some("") => ((to.as_millis() - from.as_millis()) / AUTO_STEP_BUCKETS).max(1),
        Some(raw) => match raw.parse::<i64>() {
            Ok(v) if v > 0 => v,
            _ => {
                return Err(ApiError::bad_request(format!(
                    "query: invalid step {raw:?}"
                )))
            }
        },
    };

    let mut filters: Vec<(LabelName, LabelValue)> = Vec::new();
    for (k, v) in &params {
        if let Some(label) = k.strip_prefix("label.") {
            let name = LabelName::parse(label).map_err(|_| {
                ApiError::bad_request(format!("query: invalid label name {label:?}"))
            })?;
            filters.push((name, LabelValue::new(v.clone())));
        }
    }

    let range = QueryRange { from, to, step_ms };
    let series = state
        .store
        .query(&metric, &filters, &range)
        .into_iter()
        .map(|s| SeriesEntry {
            labels: s
                .labels
                .iter()
                .map(|(n, v)| (n.as_str().to_string(), v.as_str().to_string()))
                .collect(),
            points: s
                .points
                .into_iter()
                .map(|p| (p.ts.as_millis(), p.value.is_finite().then_some(p.value)))
                .collect(),
        })
        .collect();

    Ok(Json(QueryResponse {
        metric: metric.as_str().to_string(),
        from: from.as_millis(),
        to: to.as_millis(),
        step: step_ms,
        series,
    }))
}

async fn get_self_metrics(State(state): State<Arc<AppState>>) -> Response {
    use crate::model::{Labels, MetricKind, MetricMeta, SeriesKey};
    let stats = state.store.stats();
    let mut out = String::new();
    let entries: [(&str, MetricKind, &str, f64); 5] = [
        (
            "rustmetrics_ingested_samples_total",
            MetricKind::Counter,
            "Samples accepted into the store.",
            stats.ingested_total as f64,
        ),
        (
            "rustmetrics_dropped_samples_total",
            MetricKind::Counter,
            "Samples dropped (out of order or older than retention).",
            stats.dropped_total as f64,
        ),
        (
            "rustmetrics_series",
            MetricKind::Gauge,
            "Live time series count.",
            stats.series_count as f64,
        ),
        (
            "rustmetrics_metrics",
            MetricKind::Gauge,
            "Distinct metric names.",
            stats.metric_count as f64,
        ),
        (
            "rustmetrics_http_requests_total",
            MetricKind::Counter,
            "HTTP requests handled.",
            state.http_requests_total.load(Ordering::Relaxed) as f64,
        ),
    ];
    for (name_str, kind, help, value) in entries {
        let name = MetricName::parse(name_str).expect("static name");
        textfmt::encode_meta(
            &mut out,
            &name,
            &MetricMeta {
                kind,
                help: help.to_string(),
            },
        );
        textfmt::encode_sample(
            &mut out,
            &SeriesKey {
                name,
                labels: Labels::empty(),
            },
            value,
            None,
        );
    }
    ([("content-type", "text/plain; charset=utf-8")], out).into_response()
}

fn parse_metric_param(params: &BTreeMap<String, String>) -> Result<MetricName, ApiError> {
    let raw = params
        .get("metric")
        .ok_or_else(|| ApiError::bad_request("query: missing metric param"))?;
    MetricName::parse(raw)
        .map_err(|_| ApiError::bad_request(format!("query: invalid metric name {raw:?}")))
}

fn parse_ts_param(
    params: &BTreeMap<String, String>,
    key: &str,
    default: TimestampMs,
) -> Result<TimestampMs, ApiError> {
    match params.get(key).map(String::as_str) {
        None | Some("") => Ok(default),
        Some(raw) => raw
            .parse::<i64>()
            .ok()
            .and_then(|ms| TimestampMs::new(ms).ok())
            .ok_or_else(|| {
                ApiError::bad_request(format!("query: invalid {key} timestamp {raw:?}"))
            }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::DEFAULT_MAX_POINTS;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn app() -> Router {
        let state = Arc::new(AppState::new(Arc::new(MetricStore::new(
            i64::MAX / 2,
            DEFAULT_MAX_POINTS,
        ))));
        router(state)
    }

    async fn send(app: &Router, req: Request<Body>) -> (StatusCode, String) {
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    async fn get(app: &Router, uri: &str) -> (StatusCode, String) {
        send(app, Request::get(uri).body(Body::empty()).unwrap()).await
    }

    async fn post(app: &Router, uri: &str, body: &str) -> (StatusCode, String) {
        send(
            app,
            Request::post(uri)
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
    }

    #[tokio::test]
    async fn push_then_list_and_query() {
        let app = app();
        let now_ms = TimestampMs::now().as_millis();
        let push_body = format!(
            "# TYPE reqs counter\nreqs{{job=\"api\"}} 7 {now_ms}\nreqs{{job=\"web\"}} 3 {now_ms}\n"
        );
        let (status, body) = post(&app, "/api/push", &push_body).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

        let (status, body) = get(&app, "/api/metrics").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(r#""name":"reqs""#), "{body}");
        assert!(body.contains(r#""kind":"counter""#), "{body}");
        assert!(body.contains(r#""series":2"#), "{body}");

        let (status, body) = get(&app, "/api/query?metric=reqs&label.job=api").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(r#""labels":{"job":"api"}"#), "{body}");
        assert!(body.contains(&format!("[{now_ms},7.0]")), "{body}");
        assert!(!body.contains(r#""job":"web""#), "{body}");
    }

    #[tokio::test]
    async fn push_error_includes_line() {
        let app = app();
        let (status, body) = post(&app, "/api/push", "ok 1\n9bad 2\n").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.contains("line 2"), "{body}");
    }

    #[tokio::test]
    async fn labels_endpoint() {
        let app = app();
        post(&app, "/api/push", "m{a=\"1\",b=\"x\"} 1\nm{a=\"2\"} 1\n").await;
        let (status, body) = get(&app, "/api/labels?metric=m").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(r#""a":["1","2"]"#), "{body}");
        assert!(body.contains(r#""b":["x"]"#), "{body}");
    }

    #[tokio::test]
    async fn query_param_validation() {
        let app = app();
        for uri in [
            "/api/query",
            "/api/query?metric=9bad",
            "/api/query?metric=m&from=abc",
            "/api/query?metric=m&step=0",
            "/api/query?metric=m&from=2000&to=1000",
            "/api/query?metric=m&label.9x=1",
        ] {
            let (status, body) = get(&app, uri).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{uri} -> {body}");
            assert!(body.contains("error"), "{uri} -> {body}");
        }
    }

    #[tokio::test]
    async fn non_finite_points_are_null() {
        let app = app();
        let now_ms = TimestampMs::now().as_millis();
        post(&app, "/api/push", &format!("m NaN {now_ms}\n")).await;
        let (status, body) = get(&app, "/api/query?metric=m").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(&format!("[{now_ms},null]")), "{body}");
    }

    #[tokio::test]
    async fn self_metrics_and_dashboard() {
        let app = app();
        post(&app, "/api/push", "m 1\n").await;
        let (status, body) = get(&app, "/metrics").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains("rustmetrics_ingested_samples_total 1"),
            "{body}"
        );
        assert!(body.contains("# TYPE rustmetrics_series gauge"), "{body}");

        let (status, body) = get(&app, "/").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("<html"));
    }

    #[tokio::test]
    async fn unknown_route_and_wrong_method() {
        let app = app();
        assert_eq!(get(&app, "/nope").await.0, StatusCode::NOT_FOUND);
        assert_eq!(
            post(&app, "/api/query", "").await.0,
            StatusCode::METHOD_NOT_ALLOWED
        );
        assert_eq!(
            get(&app, "/api/push").await.0,
            StatusCode::METHOD_NOT_ALLOWED
        );
    }
}
