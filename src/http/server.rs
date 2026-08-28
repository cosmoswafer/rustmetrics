//! HTTP/1.1 server subset: request parsing + threaded accept loop.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use super::{status_reason, HttpError, Method, QueryParams, Request, Response};

const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;
const READ_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CONNECTIONS: usize = 64;

/// Parse one HTTP request from a buffered reader. Boundary parser: everything
/// downstream sees only `Request`.
pub fn parse_request<R: BufRead>(reader: &mut R) -> Result<Request, HttpError> {
    let mut header_bytes = 0usize;
    let mut line = String::new();
    read_line(reader, &mut line, &mut header_bytes)?;
    let request_line = line.trim_end();

    let mut parts = request_line.split(' ');
    let method_str = parts.next().unwrap_or("");
    let target = parts
        .next()
        .ok_or_else(|| HttpError::MalformedRequestLine(request_line.to_string()))?;
    let version = parts.next().unwrap_or("");
    if !version.starts_with("HTTP/1.") || parts.next().is_some() {
        return Err(HttpError::MalformedRequestLine(request_line.to_string()));
    }
    let method = Method::parse(method_str)?;

    let (path_raw, query_raw) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target, ""),
    };
    let path = super::percent_decode(path_raw)?;
    let query = QueryParams::parse(query_raw)?;

    let mut content_length: Option<usize> = None;
    loop {
        line.clear();
        read_line(reader, &mut line, &mut header_bytes)?;
        let header = line.trim_end();
        if header.is_empty() {
            break;
        }
        let (name, value) = header
            .split_once(':')
            .ok_or_else(|| HttpError::MalformedHeader(header.to_string()))?;
        if name.trim().eq_ignore_ascii_case("content-length") {
            let value = value.trim();
            let n: usize = value
                .parse()
                .map_err(|_| HttpError::InvalidContentLength(value.to_string()))?;
            content_length = Some(n);
        }
    }

    let body = match (method, content_length) {
        (Method::Post, None) => return Err(HttpError::LengthRequired),
        (_, Some(n)) if n > MAX_BODY_BYTES => return Err(HttpError::BodyTooLarge(n)),
        (_, Some(n)) => {
            let mut body = vec![0u8; n];
            reader
                .read_exact(&mut body)
                .map_err(|e| HttpError::Io(e.to_string()))?;
            body
        }
        (Method::Get, None) => Vec::new(),
    };

    Ok(Request {
        method,
        path,
        query,
        body,
    })
}

fn read_line<R: BufRead>(
    reader: &mut R,
    line: &mut String,
    header_bytes: &mut usize,
) -> Result<(), HttpError> {
    let n = reader
        .read_line(line)
        .map_err(|e| HttpError::Io(e.to_string()))?;
    if n == 0 {
        return Err(HttpError::Io("connection closed mid-request".to_string()));
    }
    *header_bytes += n;
    if *header_bytes > MAX_HEADER_BYTES {
        return Err(HttpError::HeadersTooLarge);
    }
    Ok(())
}

pub fn write_response(stream: &mut impl Write, resp: &Response) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        resp.status,
        status_reason(resp.status),
        resp.content_type,
        resp.body.len()
    )?;
    stream.write_all(&resp.body)?;
    stream.flush()
}

/// Accept loop: thread per connection, capped at MAX_CONNECTIONS in flight.
/// Blocks forever; run on a dedicated thread.
pub fn serve<H>(listener: TcpListener, handler: Arc<H>) -> !
where
    H: Fn(Request) -> Response + Send + Sync + 'static,
{
    let active = Arc::new(AtomicUsize::new(0));
    loop {
        let (stream, _) = match listener.accept() {
            Ok(pair) => pair,
            Err(e) => {
                eprintln!("warn: accept failed: {e}");
                continue;
            }
        };
        if active.load(Ordering::Relaxed) >= MAX_CONNECTIONS {
            let mut s = stream;
            let _ = write_response(&mut s, &Response::text(500, "server busy\n".to_string()));
            continue;
        }
        active.fetch_add(1, Ordering::Relaxed);
        let handler = Arc::clone(&handler);
        let active = Arc::clone(&active);
        thread::spawn(move || {
            handle_connection(stream, handler.as_ref());
            active.fetch_sub(1, Ordering::Relaxed);
        });
    }
}

fn handle_connection<H>(mut stream: TcpStream, handler: &H)
where
    H: Fn(Request) -> Response,
{
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let _ = stream.set_write_timeout(Some(READ_TIMEOUT));
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    let response = match parse_request(&mut reader) {
        Ok(req) => handler(req),
        Err(e) => Response::text(e.status(), format!("{e}\n")),
    };
    let _ = write_response(&mut stream, &response);
    let _ = stream.shutdown(std::net::Shutdown::Both);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn parse(raw: &str) -> Result<Request, HttpError> {
        parse_request(&mut Cursor::new(raw.as_bytes().to_vec()))
    }

    #[test]
    fn parses_get_with_query() {
        let req =
            parse("GET /api/query?metric=up&label.job=a%20b HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        assert_eq!(req.method, Method::Get);
        assert_eq!(req.path, "/api/query");
        assert_eq!(req.query.get("metric"), Some("up"));
        assert_eq!(req.query.get("label.job"), Some("a b"));
        assert!(req.body.is_empty());
    }

    #[test]
    fn parses_post_with_body() {
        let req = parse("POST /api/push HTTP/1.1\r\nContent-Length: 5\r\n\r\nup 1\n").unwrap();
        assert_eq!(req.method, Method::Post);
        assert_eq!(req.body, b"up 1\n");
    }

    #[test]
    fn post_without_length_is_411() {
        let e = parse("POST /api/push HTTP/1.1\r\n\r\n").unwrap_err();
        assert_eq!(e, HttpError::LengthRequired);
        assert_eq!(e.status(), 411);
    }

    #[test]
    fn rejects_unsupported_method_and_bad_request_line() {
        assert_eq!(
            parse("DELETE / HTTP/1.1\r\n\r\n").unwrap_err().status(),
            405
        );
        assert!(matches!(
            parse("GARBAGE\r\n\r\n").unwrap_err(),
            HttpError::MalformedRequestLine(_)
        ));
    }

    #[test]
    fn rejects_oversized_body_and_headers() {
        let e = parse("POST / HTTP/1.1\r\nContent-Length: 99999999\r\n\r\n").unwrap_err();
        assert!(matches!(e, HttpError::BodyTooLarge(_)));

        let mut raw = String::from("GET / HTTP/1.1\r\n");
        for i in 0..2000 {
            raw.push_str(&format!("X-Filler-{i}: aaaaaaaaaaaaaaaa\r\n"));
        }
        raw.push_str("\r\n");
        assert_eq!(parse(&raw).unwrap_err(), HttpError::HeadersTooLarge);
    }

    #[test]
    fn rejects_bad_content_length() {
        assert!(matches!(
            parse("POST / HTTP/1.1\r\nContent-Length: abc\r\n\r\n").unwrap_err(),
            HttpError::InvalidContentLength(_)
        ));
    }

    #[test]
    fn response_writing() {
        let mut buf = Vec::new();
        write_response(&mut buf, &Response::json(200, "{}".to_string())).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(s.contains("Content-Length: 2\r\n"));
        assert!(s.ends_with("\r\n\r\n{}"));
    }
}
