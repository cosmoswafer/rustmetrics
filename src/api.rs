//! HTTP route table: parsed Request -> handler -> Response.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::dashboard;
use crate::http::{Method, QueryParams, Request, Response};
use crate::json::JsonWriter;
use crate::model::{LabelName, LabelValue, MetricMeta, MetricName, TimestampMs};
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

pub fn handle(state: &AppState, req: Request) -> Response {
    state.http_requests_total.fetch_add(1, Ordering::Relaxed);
    match (req.method, req.path.as_str()) {
        (Method::Get, "/") => Response::html(dashboard::PAGE),
        (Method::Post, "/api/push") => handle_push(state, &req.body),
        (Method::Get, "/api/metrics") => handle_list(state),
        (Method::Get, "/api/labels") => handle_labels(state, &req.query),
        (Method::Get, "/api/query") => handle_query(state, &req.query),
        (Method::Get, "/metrics") => handle_self_metrics(state),
        (Method::Post, "/")
        | (Method::Post, "/api/metrics")
        | (Method::Post, "/api/labels")
        | (Method::Post, "/api/query")
        | (Method::Post, "/metrics") => Response::json_error(405, "method not allowed"),
        (Method::Get, "/api/push") => Response::json_error(405, "use POST"),
        _ => Response::json_error(404, "not found"),
    }
}

fn handle_push(state: &AppState, body: &[u8]) -> Response {
    let text = match std::str::from_utf8(body) {
        Ok(t) => t,
        Err(_) => return Response::json_error(400, "push body is not valid UTF-8"),
    };
    match textfmt::parse(text) {
        Ok(parsed) => {
            state
                .store
                .ingest(parsed.samples, parsed.metas, TimestampMs::now());
            Response::no_content()
        }
        Err(e) => Response::json_error(400, &e.to_string()),
    }
}

fn handle_list(state: &AppState) -> Response {
    let mut w = JsonWriter::new();
    w.begin_object();
    w.key("metrics").begin_array();
    for info in state.store.list_metrics() {
        w.begin_object();
        w.key("name").string(info.name.as_str());
        w.key("kind").string(info.meta.kind.as_str());
        w.key("help").string(&info.meta.help);
        w.key("series").int(info.series_count as i64);
        w.end_object();
    }
    w.end_array();
    w.end_object();
    Response::json(200, w.finish())
}

fn handle_labels(state: &AppState, query: &QueryParams) -> Response {
    let metric = match parse_metric_param(query) {
        Ok(m) => m,
        Err(resp) => return *resp,
    };
    let mut w = JsonWriter::new();
    w.begin_object();
    w.key("metric").string(metric.as_str());
    w.key("labels").begin_object();
    for (name, values) in state.store.label_values(&metric) {
        w.key(name.as_str()).begin_array();
        for v in values {
            w.string(v.as_str());
        }
        w.end_array();
    }
    w.end_object();
    w.end_object();
    Response::json(200, w.finish())
}

fn handle_query(state: &AppState, query: &QueryParams) -> Response {
    let metric = match parse_metric_param(query) {
        Ok(m) => m,
        Err(resp) => return *resp,
    };

    let now = TimestampMs::now();
    let to = match parse_ts_param(query, "to", now) {
        Ok(v) => v,
        Err(resp) => return *resp,
    };
    let from = match parse_ts_param(query, "from", to.saturating_sub_millis(DEFAULT_RANGE_MS)) {
        Ok(v) => v,
        Err(resp) => return *resp,
    };
    if from > to {
        return Response::json_error(400, "query: from is after to");
    }

    let step_ms = match query.get("step") {
        Some(raw) => match raw.parse::<i64>() {
            Ok(v) if v > 0 => v,
            _ => {
                return Response::json_error(400, &format!("query: invalid step {raw:?}"));
            }
        },
        None => ((to.as_millis() - from.as_millis()) / AUTO_STEP_BUCKETS).max(1),
    };

    let mut filters: Vec<(LabelName, LabelValue)> = Vec::new();
    for (k, v) in query.iter() {
        if let Some(label) = k.strip_prefix("label.") {
            match LabelName::parse(label) {
                Ok(name) => filters.push((name, LabelValue::new(v))),
                Err(_) => {
                    return Response::json_error(
                        400,
                        &format!("query: invalid label name {label:?}"),
                    );
                }
            }
        }
    }

    let range = QueryRange { from, to, step_ms };
    let result = state.store.query(&metric, &filters, &range);

    let mut w = JsonWriter::new();
    w.begin_object();
    w.key("metric").string(metric.as_str());
    w.key("from").int(from.as_millis());
    w.key("to").int(to.as_millis());
    w.key("step").int(step_ms);
    w.key("series").begin_array();
    for s in result {
        w.begin_object();
        w.key("labels").begin_object();
        for (n, v) in s.labels.iter() {
            w.key(n.as_str()).string(v.as_str());
        }
        w.end_object();
        w.key("points").begin_array();
        for p in s.points {
            w.begin_array()
                .int(p.ts.as_millis())
                .number(p.value)
                .end_array();
        }
        w.end_array();
        w.end_object();
    }
    w.end_array();
    w.end_object();
    Response::json(200, w.finish())
}

fn handle_self_metrics(state: &AppState) -> Response {
    use crate::model::{Labels, MetricKind, SeriesKey};
    let stats = state.store.stats();
    let mut out = String::new();
    let gauge = |name: &str, help: &str| {
        (
            MetricName::parse(name).expect("static name"),
            MetricMeta {
                kind: MetricKind::Gauge,
                help: help.to_string(),
            },
        )
    };
    let counter = |name: &str, help: &str| {
        (
            MetricName::parse(name).expect("static name"),
            MetricMeta {
                kind: MetricKind::Counter,
                help: help.to_string(),
            },
        )
    };
    let entries: [(_, f64); 5] = [
        (
            counter(
                "rustmetrics_ingested_samples_total",
                "Samples accepted into the store.",
            ),
            stats.ingested_total as f64,
        ),
        (
            counter(
                "rustmetrics_dropped_samples_total",
                "Samples dropped (out of order or older than retention).",
            ),
            stats.dropped_total as f64,
        ),
        (
            gauge("rustmetrics_series", "Live time series count."),
            stats.series_count as f64,
        ),
        (
            gauge("rustmetrics_metrics", "Distinct metric names."),
            stats.metric_count as f64,
        ),
        (
            counter("rustmetrics_http_requests_total", "HTTP requests handled."),
            state.http_requests_total.load(Ordering::Relaxed) as f64,
        ),
    ];
    for ((name, meta), value) in entries {
        textfmt::encode_meta(&mut out, &name, &meta);
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
    Response::text(200, out)
}

fn parse_metric_param(query: &QueryParams) -> Result<MetricName, Box<Response>> {
    let raw = query
        .get("metric")
        .ok_or_else(|| Box::new(Response::json_error(400, "query: missing metric param")))?;
    MetricName::parse(raw).map_err(|_| {
        Box::new(Response::json_error(
            400,
            &format!("query: invalid metric name {raw:?}"),
        ))
    })
}

fn parse_ts_param(
    query: &QueryParams,
    key: &str,
    default: TimestampMs,
) -> Result<TimestampMs, Box<Response>> {
    match query.get(key) {
        None | Some("") => Ok(default),
        Some(raw) => raw
            .parse::<i64>()
            .ok()
            .and_then(|ms| TimestampMs::new(ms).ok())
            .ok_or_else(|| {
                Box::new(Response::json_error(
                    400,
                    &format!("query: invalid {key} timestamp {raw:?}"),
                ))
            }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::DEFAULT_MAX_POINTS;

    fn state() -> AppState {
        AppState::new(Arc::new(MetricStore::new(i64::MAX / 2, DEFAULT_MAX_POINTS)))
    }

    fn get(path_query: &str) -> Request {
        let (path, query) = match path_query.split_once('?') {
            Some((p, q)) => (p.to_string(), QueryParams::parse(q).unwrap()),
            None => (path_query.to_string(), QueryParams::default()),
        };
        Request {
            method: Method::Get,
            path,
            query,
            body: Vec::new(),
        }
    }

    fn post(path: &str, body: &str) -> Request {
        Request {
            method: Method::Post,
            path: path.to_string(),
            query: QueryParams::default(),
            body: body.as_bytes().to_vec(),
        }
    }

    fn body_str(r: &Response) -> String {
        String::from_utf8(r.body.clone()).unwrap()
    }

    #[test]
    fn push_then_list_and_query() {
        let st = state();
        let now_ms = TimestampMs::now().as_millis();
        let push_body = format!(
            "# TYPE reqs counter\nreqs{{job=\"api\"}} 7 {now_ms}\nreqs{{job=\"web\"}} 3 {now_ms}\n"
        );
        let r = handle(&st, post("/api/push", &push_body));
        assert_eq!(r.status, 204, "{}", body_str(&r));

        let r = handle(&st, get("/api/metrics"));
        assert_eq!(r.status, 200);
        let b = body_str(&r);
        assert!(b.contains(r#""name":"reqs""#), "{b}");
        assert!(b.contains(r#""kind":"counter""#), "{b}");
        assert!(b.contains(r#""series":2"#), "{b}");

        let r = handle(&st, get("/api/query?metric=reqs&label.job=api"));
        assert_eq!(r.status, 200);
        let b = body_str(&r);
        assert!(b.contains(r#""labels":{"job":"api"}"#), "{b}");
        assert!(b.contains(&format!("[{now_ms},7]")), "{b}");
        assert!(!b.contains(r#""job":"web""#), "{b}");
    }

    #[test]
    fn push_error_includes_line() {
        let st = state();
        let r = handle(&st, post("/api/push", "ok 1\n9bad 2\n"));
        assert_eq!(r.status, 400);
        assert!(body_str(&r).contains("line 2"), "{}", body_str(&r));
    }

    #[test]
    fn labels_endpoint() {
        let st = state();
        handle(
            &st,
            post("/api/push", "m{a=\"1\",b=\"x\"} 1\nm{a=\"2\"} 1\n"),
        );
        let r = handle(&st, get("/api/labels?metric=m"));
        assert_eq!(r.status, 200);
        let b = body_str(&r);
        assert!(b.contains(r#""a":["1","2"]"#), "{b}");
        assert!(b.contains(r#""b":["x"]"#), "{b}");
    }

    #[test]
    fn query_param_validation() {
        let st = state();
        assert_eq!(handle(&st, get("/api/query")).status, 400);
        assert_eq!(handle(&st, get("/api/query?metric=9bad")).status, 400);
        assert_eq!(handle(&st, get("/api/query?metric=m&from=abc")).status, 400);
        assert_eq!(handle(&st, get("/api/query?metric=m&step=0")).status, 400);
        assert_eq!(
            handle(&st, get("/api/query?metric=m&from=2000&to=1000")).status,
            400
        );
        assert_eq!(
            handle(&st, get("/api/query?metric=m&label.9x=1")).status,
            400
        );
    }

    #[test]
    fn self_metrics_and_dashboard() {
        let st = state();
        handle(&st, post("/api/push", "m 1\n"));
        let r = handle(&st, get("/metrics"));
        assert_eq!(r.status, 200);
        let b = body_str(&r);
        assert!(b.contains("rustmetrics_ingested_samples_total 1"), "{b}");
        assert!(b.contains("# TYPE rustmetrics_series gauge"), "{b}");

        let r = handle(&st, get("/"));
        assert_eq!(r.status, 200);
        assert!(body_str(&r).contains("<html"));
    }

    #[test]
    fn unknown_route_and_wrong_method() {
        let st = state();
        assert_eq!(handle(&st, get("/nope")).status, 404);
        assert_eq!(handle(&st, post("/api/query", "")).status, 405);
        assert_eq!(handle(&st, get("/api/push")).status, 405);
    }
}
