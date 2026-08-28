//! Scrape loop: fetch targets, parse, ingest, mark `up`/duration series.

use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::http::client::{self, ScrapeUrl};
use crate::model::{
    LabelName, LabelValue, Labels, MetricKind, MetricMeta, MetricName, ScrapedSample, SeriesKey,
    TimestampMs,
};
use crate::storage::MetricStore;
use crate::textfmt;

/// Blocks forever; run on a dedicated thread. Round-robins all targets each
/// tick (per-target timeouts in the client keep one tick bounded).
pub fn run(store: Arc<MetricStore>, targets: Vec<ScrapeUrl>, interval: Duration) -> ! {
    loop {
        for target in &targets {
            scrape_one(&store, target);
        }
        thread::sleep(interval);
    }
}

pub fn scrape_one(store: &MetricStore, target: &ScrapeUrl) {
    let started = Instant::now();
    let result = client::fetch(target)
        .and_then(|body| textfmt::parse(&body).map_err(|e| client::ClientError::Io(e.to_string())));
    let duration = started.elapsed().as_secs_f64();
    let now = TimestampMs::now();

    let up = match result {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{QueryRange, DEFAULT_MAX_POINTS};
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn serve_once(response: &'static str) -> ScrapeUrl {
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
        ScrapeUrl::parse(&format!("http://127.0.0.1:{}/metrics", addr.port())).unwrap()
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

    #[test]
    fn successful_scrape_ingests_and_marks_up() {
        let store = MetricStore::new(i64::MAX / 2, DEFAULT_MAX_POINTS);
        let url = serve_once("# TYPE t gauge\nt 42\n");
        scrape_one(&store, &url);
        assert_eq!(query_value(&store, "t"), Some(42.0));
        assert_eq!(query_value(&store, "up"), Some(1.0));
        assert!(query_value(&store, "scrape_duration_seconds").is_some());
    }

    #[test]
    fn failed_scrape_marks_down() {
        let store = MetricStore::new(i64::MAX / 2, DEFAULT_MAX_POINTS);
        // reserve a port then drop the listener so the connect fails
        let url = {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            drop(listener);
            ScrapeUrl::parse(&format!("http://127.0.0.1:{port}/metrics")).unwrap()
        };
        scrape_one(&store, &url);
        assert_eq!(query_value(&store, "up"), Some(0.0));
    }

    #[test]
    fn unparseable_body_marks_down() {
        let store = MetricStore::new(i64::MAX / 2, DEFAULT_MAX_POINTS);
        let url = serve_once("!!! not metrics !!!\n");
        scrape_one(&store, &url);
        assert_eq!(query_value(&store, "up"), Some(0.0));
    }
}
