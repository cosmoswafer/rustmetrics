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
