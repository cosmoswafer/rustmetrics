//! End-to-end test: real TCP server, push -> list -> query -> dashboard.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;

use rustmetrics::api::{self, AppState};
use rustmetrics::http::server;
use rustmetrics::storage::{MetricStore, DEFAULT_MAX_POINTS};

fn start_server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let store = Arc::new(MetricStore::new(86_400_000, DEFAULT_MAX_POINTS));
    let state = Arc::new(AppState::new(store));
    thread::spawn(move || {
        server::serve(listener, Arc::new(move |req| api::handle(&state, req)));
    });
    port
}

fn request(port: u16, raw: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.write_all(raw.as_bytes()).unwrap();
    let mut reader = BufReader::new(stream);

    let mut status_line = String::new();
    reader.read_line(&mut status_line).unwrap();
    let status: u16 = status_line.split(' ').nth(1).unwrap().parse().unwrap();

    let mut content_length = 0usize;
    let mut line = String::new();
    loop {
        line.clear();
        reader.read_line(&mut line).unwrap();
        if line.trim_end().is_empty() {
            break;
        }
        if let Some((k, v)) = line.trim_end().split_once(':') {
            if k.eq_ignore_ascii_case("content-length") {
                content_length = v.trim().parse().unwrap();
            }
        }
    }
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body).unwrap();
    (status, String::from_utf8(body).unwrap())
}

fn get(port: u16, path: &str) -> (u16, String) {
    request(
        port,
        &format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n"),
    )
}

fn post(port: u16, path: &str, body: &str) -> (u16, String) {
    request(
        port,
        &format!(
            "POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ),
    )
}

#[test]
fn push_query_dashboard_end_to_end() {
    let port = start_server();
    let now = rustmetrics::model::TimestampMs::now().as_millis();

    // push
    let payload = format!(
        "# HELP temp Room temperature.\n# TYPE temp gauge\n\
         temp{{room=\"lab\"}} 21.5 {now}\ntemp{{room=\"hall\"}} 19 {now}\n"
    );
    let (status, body) = post(port, "/api/push", &payload);
    assert_eq!(status, 204, "{body}");

    // list
    let (status, body) = get(port, "/api/metrics");
    assert_eq!(status, 200);
    assert!(body.contains(r#""name":"temp""#), "{body}");
    assert!(body.contains(r#""kind":"gauge""#), "{body}");
    assert!(body.contains(r#""help":"Room temperature.""#), "{body}");
    assert!(body.contains(r#""series":2"#), "{body}");

    // labels
    let (status, body) = get(port, "/api/labels?metric=temp");
    assert_eq!(status, 200);
    assert!(body.contains(r#""room":["hall","lab"]"#), "{body}");

    // query with label filter
    let (status, body) = get(port, "/api/query?metric=temp&label.room=lab");
    assert_eq!(status, 200);
    assert!(body.contains(r#""labels":{"room":"lab"}"#), "{body}");
    assert!(body.contains(&format!("[{now},21.5]")), "{body}");
    assert!(!body.contains(r#""room":"hall""#), "{body}");

    // bad push reports the line
    let (status, body) = post(port, "/api/push", "ok 1\nbad{ 2\n");
    assert_eq!(status, 400);
    assert!(body.contains("line 2"), "{body}");

    // dashboard
    let (status, body) = get(port, "/");
    assert_eq!(status, 200);
    assert!(body.contains("<html"), "{body}");
    assert!(body.contains("rustmetrics"), "{body}");

    // self exposition
    let (status, body) = get(port, "/metrics");
    assert_eq!(status, 200);
    assert!(
        body.contains("rustmetrics_ingested_samples_total 2"),
        "{body}"
    );
    assert!(body.contains("rustmetrics_http_requests_total"), "{body}");

    // unknown route
    let (status, _) = get(port, "/definitely-missing");
    assert_eq!(status, 404);
}
