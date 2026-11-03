//! End-to-end test: real TCP server via axum, push -> list -> query -> dashboard.

use std::sync::Arc;

use rustmetrics::api::{router, AppState};
use rustmetrics::storage::{MetricStore, DEFAULT_MAX_POINTS};

async fn start_server() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let store = Arc::new(MetricStore::new(86_400_000, DEFAULT_MAX_POINTS));
    let app = router(Arc::new(AppState::new(store)));
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn push_query_dashboard_end_to_end() {
    let base = start_server().await;
    let client = reqwest::Client::new();
    let now = rustmetrics::model::TimestampMs::now().as_millis();

    // push
    let payload = format!(
        "# HELP temp Room temperature.\n# TYPE temp gauge\n\
         temp{{room=\"lab\"}} 21.5 {now}\ntemp{{room=\"hall\"}} 19 {now}\n"
    );
    let resp = client
        .post(format!("{base}/api/push"))
        .body(payload)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 204);

    // list
    let resp = client
        .get(format!("{base}/api/metrics"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains(r#""name":"temp""#), "{body}");
    assert!(body.contains(r#""kind":"gauge""#), "{body}");
    assert!(body.contains(r#""help":"Room temperature.""#), "{body}");
    assert!(body.contains(r#""series":2"#), "{body}");

    // labels
    let body = client
        .get(format!("{base}/api/labels?metric=temp"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains(r#""room":["hall","lab"]"#), "{body}");

    // query with label filter
    let resp = client
        .get(format!("{base}/api/query?metric=temp&label.room=lab"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains(r#""labels":{"room":"lab"}"#), "{body}");
    assert!(body.contains(&format!("[{now},21.5]")), "{body}");
    assert!(!body.contains(r#""room":"hall""#), "{body}");

    // bad push reports the line
    let resp = client
        .post(format!("{base}/api/push"))
        .body("ok 1\nbad{ 2\n")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
    let body = resp.text().await.unwrap();
    assert!(body.contains("line 2"), "{body}");

    // dashboard
    let resp = client.get(format!("{base}/")).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("<html"), "{body}");
    assert!(body.contains("rustmetrics"), "{body}");

    // self exposition
    let resp = client.get(format!("{base}/metrics")).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("rustmetrics_ingested_samples_total 2"),
        "{body}"
    );
    assert!(body.contains("rustmetrics_http_requests_total"), "{body}");

    // unknown route
    let resp = client
        .get(format!("{base}/definitely-missing"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);
}

// ---------------------------------------------------------------------------
// llama.cpp collector against a mock llama-server (router mode)
// ---------------------------------------------------------------------------

async fn mock_llama_server() -> String {
    use axum::routing::{get, post};
    use axum::Json;
    let models = serde_json::json!({"data": [
        {"id": "lfm25", "status": {"value": "loaded", "args":
            ["--alias", "lfm25", "--ctx-size", "32768", "--hf-repo",
             "unsloth/LFM2.5-8B-A1B-GGUF:UD-Q4_K_XL", "--main-gpu", "1"]}},
        {"id": "bonsai27", "status": {"value": "unloaded", "args":
            ["--alias", "bonsai27", "--ctx-size", "65536"]}}
    ]});
    let slots = serde_json::json!([{
        "id": 0, "n_ctx": 32768, "is_processing": true,
        "n_prompt_tokens": 11, "n_prompt_tokens_processed": 5,
        "n_prompt_tokens_cache": 2, "params": {"temperature": 0.2}
    }]);
    let stream = format!(
        "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({"choices": [{"delta": {"role": "assistant"}}]}),
        serde_json::json!({"choices": [{"delta": {"content": "hi"}}]}),
        serde_json::json!({
            "model": "lfm25",
            "choices": [],
            "timings": {"cache_n": 2, "prompt_n": 10, "prompt_per_second": 6.9,
                        "predicted_n": 16, "predicted_per_second": 103.9},
            "usage": {"prompt_tokens": 10, "completion_tokens": 16}
        })
    );

    let app = axum::Router::new()
        .route(
            "/health",
            get(|| async { Json(serde_json::json!({"status": "ok"})) }),
        )
        .route(
            "/props",
            get(|| async {
                Json(serde_json::json!({"role": "router", "build_info": "b11609-test"}))
            }),
        )
        .route("/v1/models", get(|| async { Json(models) }))
        .route("/slots", get(|| async { Json(slots) }))
        .route(
            "/metrics",
            get(|| async {
                (
                    axum::http::StatusCode::NOT_IMPLEMENTED,
                    Json(serde_json::json!({"error": {"code": 501,
                "message": "This server does not support metrics endpoint."}})),
                )
            }),
        )
        .route(
            "/v1/chat/completions",
            post(move || {
                let stream = stream.clone();
                async move {
                    (
                        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                        stream,
                    )
                }
            }),
        );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn llamacpp_collector_poll_and_probe() {
    let base = mock_llama_server().await;
    let collector = rustmetrics::llamacpp::collect::Collector::new(&base, None, 16, None);
    let state = Arc::new(std::sync::Mutex::new(
        rustmetrics::llamacpp::collect::State::new(base.clone(), 300.0, 2.0),
    ));

    collector.poll(&state).await;
    collector.probe(&state).await;
    // a second cycle exercises the series-update path
    collector.poll(&state).await;

    let g = state.lock().unwrap();
    assert!(g.up);
    assert_eq!(g.role.as_deref(), Some("router"));
    assert_eq!(g.build.as_deref(), Some("b11609-test"));
    assert_eq!(g.models.len(), 2);
    assert_eq!(g.loaded_count(), 1);

    // auto-focus picked the loaded model
    let focus = g.focus_model().unwrap();
    assert_eq!(focus.name, "lfm25");
    assert_eq!(focus.ctx, Some(32768));
    assert_eq!(
        focus.hf_repo.as_deref(),
        Some("unsloth/LFM2.5-8B-A1B-GGUF:UD-Q4_K_XL")
    );

    // slots committed and charted
    assert_eq!(g.slots.len(), 1);
    assert!(g.slots[0].processing);
    assert!(g.series.contains_key("busy/lfm25"));
    assert!(g.series.contains_key("ctx_pct/lfm25"));
    assert_eq!(g.series.get("busy/lfm25").unwrap().last().unwrap().v, 1.0);

    // probe produced server-measured throughput series
    let probe = g.last_probe.as_ref().unwrap();
    assert_eq!(probe.model, "lfm25");
    assert_eq!(probe.error, None);
    assert!((probe.gen_tps - 103.9).abs() < 0.01);
    assert!((probe.prompt_tps - 6.9).abs() < 0.01);
    assert_eq!(probe.gen_n, 16);
    assert_eq!(probe.cache_n, 2);
    let ttft_ms = probe.ttft_ms.expect("streamed token should set TTFT");
    assert!(ttft_ms > 0.0 && ttft_ms <= probe.wall_ms);
    assert_eq!(
        g.series.get("ttft_ms/lfm25").unwrap().last().unwrap().v,
        ttft_ms
    );
    assert!(g.probes_ok == 1 && g.probes_err == 0);
    // The second passive poll adds a slot-delta sample after the explicit
    // one-shot probe used by this collector test.
    assert_eq!(
        g.series.get("gen_tps/lfm25").unwrap().values(),
        vec![103.9, 0.0]
    );

    // server reports 501 -> metrics marked unsupported, not an error
    assert_eq!(g.metrics_supported, Some(false));
    assert!(!g.series.keys().any(|k| k.starts_with("prom/")));
}

#[tokio::test]
async fn llamacpp_collector_parses_prometheus_when_enabled() {
    use axum::routing::get;

    let app = axum::Router::new()
        .route(
            "/health",
            get(|| async {
                (
                    axum::http::StatusCode::OK,
                    r#"{"status":"ok","slots_idle":1}"#,
                )
            }),
        )
        .route(
            "/props",
            get(|| async {
                (
                    axum::http::StatusCode::OK,
                    r#"{"role":"instance","build_info":"b11609","model_path":"/m.gguf"}"#,
                )
            }),
        )
        .route(
            "/v1/models",
            get(|| async {
                (
                    axum::http::StatusCode::OK,
                    r#"{"data":[{"id":"mymodel","object":"model"}]}"#,
                )
            }),
        )
        .route(
            "/slots",
            get(|| async {
                (
                    axum::http::StatusCode::OK,
                    r#"[{"id":0,"n_ctx":4096,"is_processing":false,"n_prompt_tokens":0}]"#,
                )
            }),
        )
        .route(
            "/metrics",
            get(|| async {
                (
                    axum::http::StatusCode::OK,
                    "# HELP llamacpp:prompt_tokens_total Prompt tokens.\n\
              # TYPE llamacpp:prompt_tokens_total counter\n\
              llamacpp:prompt_tokens_total 1234\n\
              # TYPE llamacpp:predicted_tokens_seconds gauge\n\
              llamacpp:predicted_tokens_seconds 55.5\n\
              # TYPE llamacpp:prompt_tokens_seconds gauge\n\
               llamacpp:prompt_tokens_seconds 79.1\n\
               other_metric 1\n",
                )
            }),
        );
    // plain instance mode: no ?model= handling needed, query params are ignored

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let base = format!("http://{addr}");

    let collector = rustmetrics::llamacpp::collect::Collector::new(&base, None, 4, None);
    let state = Arc::new(std::sync::Mutex::new(
        rustmetrics::llamacpp::collect::State::new(base.clone(), 300.0, 2.0),
    ));
    collector.poll(&state).await;

    let g = state.lock().unwrap();
    assert!(g.up);
    // instance mode: model has no status field but is treated as loaded
    assert_eq!(g.models.len(), 1);
    assert!(g.slots.len() == 1);
    assert_eq!(g.metrics_supported, Some(true));
    assert_eq!(g.prom_discovered, 4);
    let prom = |name: &str| {
        g.series
            .get(&format!("prom/{name}"))
            .unwrap()
            .last()
            .unwrap()
            .v
    };
    assert_eq!(prom("llamacpp:prompt_tokens_total"), 1234.0);
    assert_eq!(prom("llamacpp:predicted_tokens_seconds"), 55.5);
    assert_eq!(prom("llamacpp:prompt_tokens_seconds"), 79.1);
    assert_eq!(prom("other_metric"), 1.0);
    // The first slot sample only establishes a baseline, so Prometheus supplies
    // the initial throughput estimate until a slot delta is available.
    assert_eq!(
        g.series.get("gen_tps/mymodel").unwrap().last().unwrap().v,
        55.5
    );
    assert_eq!(
        g.series
            .get("prompt_tps/mymodel")
            .unwrap()
            .last()
            .unwrap()
            .v,
        79.1
    );
}

// Official llama.cpp b12000 contract verified in docs and source:
// - /slots exposes live n_prompt_tokens_processed and stats.n_gen counters;
//   current source wraps next_token in an array even though the README's
//   example still shows an object (server-context.cpp, server_slot::to_json).
// - /metrics gauges are averages between scrapes, and every scrape resets their
//   buckets (server-context.cpp, get_metrics; server-common.h, server_metrics).
// - Generation stats are flushed only when a slot resets, so active generation
//   can legitimately produce zero/stale Prometheus gauges (callback_on_reset).
// - predicted_tokens_seconds uses decode steps, not accepted token count, to
//   avoid inflating speculative decoding rates. Our live tok/s charts therefore
//   prefer per-slot token deltas and use Prometheus only as a fallback.
// Sources: https://github.com/ggml-org/llama.cpp/blob/b12000/tools/server/README.md
// https://github.com/ggml-org/llama.cpp/blob/b12000/tools/server/server-context.cpp
// https://github.com/ggml-org/llama.cpp/blob/b12000/tools/server/server-common.h
#[tokio::test]
async fn llamacpp_passive_slot_fallback_and_metrics_recovery() {
    use axum::routing::get;
    use axum::Json;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    let fail_metrics = Arc::new(AtomicBool::new(true));
    let zero_metrics = Arc::new(AtomicBool::new(false));
    let prompt_progress = Arc::new(AtomicU64::new(100));
    let decode_progress = Arc::new(AtomicU64::new(20));
    let slot_delay_ms = Arc::new(AtomicU64::new(1000));
    let slots_finished = Arc::new(AtomicBool::new(false));
    let metrics_overlapped_slots = Arc::new(AtomicBool::new(false));

    let slots_prompt = Arc::clone(&prompt_progress);
    let slots_decode = Arc::clone(&decode_progress);
    let slots_done = Arc::clone(&slots_finished);
    let slots_delay = Arc::clone(&slot_delay_ms);
    let metrics_failure = Arc::clone(&fail_metrics);
    let metrics_zero = Arc::clone(&zero_metrics);
    let metrics_slots_done = Arc::clone(&slots_finished);
    let metrics_overlap = Arc::clone(&metrics_overlapped_slots);
    let app = axum::Router::new()
        .route(
            "/health",
            get(|| async { Json(serde_json::json!({"status": "ok"})) }),
        )
        .route(
            "/props",
            get(|| async { Json(serde_json::json!({"role": "instance"})) }),
        )
        .route(
            "/v1/models",
            get(|| async { Json(serde_json::json!({"data": [{"id": "mymodel"}]})) }),
        )
        .route(
            "/slots",
            get(move || {
                let prompt = Arc::clone(&slots_prompt);
                let decoded = Arc::clone(&slots_decode);
                let done = Arc::clone(&slots_done);
                let delay = Arc::clone(&slots_delay);
                async move {
                    tokio::time::sleep(std::time::Duration::from_millis(
                        delay.load(Ordering::Relaxed),
                    ))
                    .await;
                    done.store(true, Ordering::Relaxed);
                    Json(serde_json::json!([{
                        "id": 0,
                        "n_ctx": 4096,
                        "is_processing": true,
                        "n_prompt_tokens": 1000,
                        "n_prompt_tokens_processed": prompt.load(Ordering::Relaxed),
                        "id_task": 7,
                        "next_token": [{"n_decoded": decoded.load(Ordering::Relaxed)}]
                    }]))
                }
            }),
        )
        .route(
            "/metrics",
            get(move || {
                let fail = Arc::clone(&metrics_failure);
                let zero = Arc::clone(&metrics_zero);
                let slots_done = Arc::clone(&metrics_slots_done);
                let overlap = Arc::clone(&metrics_overlap);
                async move {
                    overlap.fetch_or(!slots_done.load(Ordering::Relaxed), Ordering::Relaxed);
                    if fail.load(Ordering::Relaxed) {
                        (
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                            r#"{"error":{"message":"proxy error: Could not establish connection"}}"#
                                .to_string(),
                        )
                    } else if zero.load(Ordering::Relaxed) {
                        (
                            axum::http::StatusCode::OK,
                            "llamacpp:predicted_tokens_seconds 0\n\
                             llamacpp:prompt_tokens_seconds 0\n"
                                .to_string(),
                        )
                    } else {
                        (
                            axum::http::StatusCode::OK,
                            "llamacpp:predicted_tokens_seconds 55.5\n\
                             llamacpp:prompt_tokens_seconds 79.1\n"
                                .to_string(),
                        )
                    }
                }
            }),
        );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let base = format!("http://{addr}");
    let collector = rustmetrics::llamacpp::collect::Collector::new(&base, None, 4, None);
    let state = Arc::new(std::sync::Mutex::new(
        rustmetrics::llamacpp::collect::State::new(base, 300.0, 0.1),
    ));

    let started = tokio::time::Instant::now();
    collector.poll(&state).await;
    assert!(started.elapsed() < std::time::Duration::from_millis(750));
    assert!(state.lock().unwrap().last_error.is_some());
    assert!(metrics_overlapped_slots.load(Ordering::Relaxed));

    // The next fast slot response establishes a fresh baseline after the
    // timeout; the following response produces the passive rate.
    slot_delay_ms.store(0, Ordering::Relaxed);
    collector.poll(&state).await;
    prompt_progress.store(300, Ordering::Relaxed);
    decode_progress.store(60, Ordering::Relaxed);
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    collector.poll(&state).await;
    {
        let g = state.lock().unwrap();
        assert!(g.series.get("gen_tps/mymodel").unwrap().last().unwrap().v > 0.0);
        assert!(
            g.series
                .get("prompt_tps/mymodel")
                .unwrap()
                .last()
                .unwrap()
                .v
                > 0.0
        );
        assert!(g.last_error.is_some());
    }

    // Recovered gauges may contain a non-zero scrape-window average, but the
    // live slot counters have not advanced. Do not display that average as the
    // current token rate.
    fail_metrics.store(false, Ordering::Relaxed);
    collector.poll(&state).await;
    {
        let g = state.lock().unwrap();
        assert_eq!(g.last_error, None);
        assert_eq!(
            g.series.get("gen_tps/mymodel").unwrap().last().unwrap().v,
            0.0
        );
        assert_eq!(
            g.series
                .get("prompt_tps/mymodel")
                .unwrap()
                .last()
                .unwrap()
                .v,
            0.0
        );
    }

    // Zero gauges must not mask later slot progress either.
    zero_metrics.store(true, Ordering::Relaxed);
    prompt_progress.store(500, Ordering::Relaxed);
    decode_progress.store(100, Ordering::Relaxed);
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    collector.poll(&state).await;
    let g = state.lock().unwrap();
    assert!(g.series.get("gen_tps/mymodel").unwrap().last().unwrap().v > 0.0);
    assert!(
        g.series
            .get("prompt_tps/mymodel")
            .unwrap()
            .last()
            .unwrap()
            .v
            > 0.0
    );
}
