//! Scrape loop: fetch targets, parse, ingest, mark `up`/duration series.

use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::Url;

use crate::model::{
    LabelName, LabelValue, Labels, MetricKind, MetricMeta, MetricName, ScrapedSample, SeriesKey,
    TimestampMs,
};
use crate::storage::MetricStore;
use crate::textfmt;

const SCRAPE_TIMEOUT: Duration = Duration::from_secs(5);

pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(SCRAPE_TIMEOUT)
        .build()
        .expect("static client config")
}

/// Runs until the task is aborted: scrapes all targets every `interval`.
pub async fn run(
    store: Arc<MetricStore>,
    client: reqwest::Client,
    targets: Vec<Url>,
    interval: Duration,
) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        for target in &targets {
            scrape_one(&store, &client, target).await;
        }
    }
}

pub async fn scrape_one(store: &MetricStore, client: &reqwest::Client, target: &Url) {
    let started = Instant::now();
    let result = fetch_body(client, target).await;
    let duration = started.elapsed().as_secs_f64();
    let now = TimestampMs::now();

    let up = match result
        .and_then(|body| textfmt::parse(&body).map_err(|e| format!("parse failed: {e}")))
    {
        Ok(parsed) => {
            store.ingest(parsed.samples, parsed.metas, now);
            1.0
        }
        Err(e) => {
            eprintln!("warn: scrape {target} failed: {e}");
            0.0
        }
    };

    let instance = Labels::new(vec![(
        LabelName::parse("instance").expect("static label name"),
        LabelValue::new(target.as_str()),
    )])
    .expect("single label cannot collide");
    let synth = |name: &str, value: f64| ScrapedSample {
        key: SeriesKey {
            name: MetricName::parse(name).expect("static metric name"),
            labels: instance.clone(),
        },
        value,
        ts: Some(now),
    };
    let metas = vec![
        (
            MetricName::parse("up").expect("static metric name"),
            MetricMeta {
                kind: MetricKind::Gauge,
                help: "1 if the last scrape of the target succeeded.".to_string(),
            },
        ),
        (
            MetricName::parse("scrape_duration_seconds").expect("static metric name"),
            MetricMeta {
                kind: MetricKind::Gauge,
                help: "Duration of the last scrape.".to_string(),
            },
        ),
    ];
    store.ingest(
        vec![synth("up", up), synth("scrape_duration_seconds", duration)],
        metas,
        now,
    );
}

async fn fetch_body(client: &reqwest::Client, target: &Url) -> Result<String, String> {
    let resp = client
        .get(target.clone())
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("target returned HTTP {}", resp.status().as_u16()));
    }
    resp.text().await.map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{QueryRange, DEFAULT_MAX_POINTS};
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    fn serve_once(response: &'static str) -> Url {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut req = Vec::new();
                let mut buf = [0u8; 1024];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{}",
                    response.len(),
                    response
                );
            }
        });
        format!("http://127.0.0.1:{}/metrics", addr.port())
            .parse()
            .unwrap()
    }

    fn query_value(store: &MetricStore, metric: &str) -> Option<f64> {
        let range = QueryRange {
            from: TimestampMs::new(0).unwrap(),
            to: TimestampMs::now(),
            step_ms: 1,
        };
        store
            .query(&MetricName::parse(metric).unwrap(), &[], &range)
            .first()
            .and_then(|s| s.points.last().map(|p| p.value))
    }

    #[tokio::test]
    async fn successful_scrape_ingests_and_marks_up() {
        let store = MetricStore::new(i64::MAX / 2, DEFAULT_MAX_POINTS);
        let url = serve_once("# TYPE t gauge\nt 42\n");
        scrape_one(&store, &client(), &url).await;
        assert_eq!(query_value(&store, "t"), Some(42.0));
        assert_eq!(query_value(&store, "up"), Some(1.0));
        assert!(query_value(&store, "scrape_duration_seconds").is_some());
    }

    #[tokio::test]
    async fn failed_scrape_marks_down() {
        let store = MetricStore::new(i64::MAX / 2, DEFAULT_MAX_POINTS);
        // reserve a port then drop the listener so the connect fails
        let url: Url = {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            drop(listener);
            format!("http://127.0.0.1:{port}/metrics").parse().unwrap()
        };
        scrape_one(&store, &client(), &url).await;
        assert_eq!(query_value(&store, "up"), Some(0.0));
    }

    #[tokio::test]
    async fn unparseable_body_marks_down() {
        let store = MetricStore::new(i64::MAX / 2, DEFAULT_MAX_POINTS);
        let url = serve_once("!!! not metrics !!!\n");
        scrape_one(&store, &client(), &url).await;
        assert_eq!(query_value(&store, "up"), Some(0.0));
    }
}
