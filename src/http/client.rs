//! Minimal HTTP GET client for scraping (http:// only).

use std::fmt;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(5);
const MAX_RESPONSE_BYTES: u64 = 1024 * 1024;

/// A validated scrape target URL (http only). Constructed once in config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrapeUrl {
    host: String,
    port: u16,
    path: String,
    original: String,
}

impl ScrapeUrl {
    pub fn parse(s: &str) -> Result<Self, ClientError> {
        let rest = s
            .strip_prefix("http://")
            .ok_or_else(|| ClientError::UnsupportedScheme(s.to_string()))?;
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        if authority.is_empty() {
            return Err(ClientError::MissingHost(s.to_string()));
        }
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) => {
                let port: u16 = p
                    .parse()
                    .map_err(|_| ClientError::InvalidPort(p.to_string()))?;
                (h.to_string(), port)
            }
            None => (authority.to_string(), 80),
        };
        if host.is_empty() {
            return Err(ClientError::MissingHost(s.to_string()));
        }
        Ok(ScrapeUrl {
            host,
            port,
            path: path.to_string(),
            original: s.to_string(),
        })
    }

    pub fn as_str(&self) -> &str {
        &self.original
    }
}

impl fmt::Display for ScrapeUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.original)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ClientError {
    UnsupportedScheme(String),
    MissingHost(String),
    InvalidPort(String),
    Connect(String),
    Io(String),
    BadStatusLine(String),
    HttpStatus(u16),
    BodyNotUtf8,
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClientError::UnsupportedScheme(u) => {
                write!(f, "ScrapeUrl: unsupported scheme in {u:?} (http:// only)")
            }
            ClientError::MissingHost(u) => write!(f, "ScrapeUrl: missing host in {u:?}"),
            ClientError::InvalidPort(p) => write!(f, "ScrapeUrl: invalid port {p:?}"),
            ClientError::Connect(e) => write!(f, "scrape: connect failed: {e}"),
            ClientError::Io(e) => write!(f, "scrape: io error: {e}"),
            ClientError::BadStatusLine(l) => write!(f, "scrape: bad status line {l:?}"),
            ClientError::HttpStatus(code) => write!(f, "scrape: target returned HTTP {code}"),
            ClientError::BodyNotUtf8 => write!(f, "scrape: response body is not UTF-8"),
        }
    }
}

impl std::error::Error for ClientError {}

/// GET the target and return the body as UTF-8 text.
pub fn fetch(url: &ScrapeUrl) -> Result<String, ClientError> {
    let mut stream = TcpStream::connect((url.host.as_str(), url.port))
        .map_err(|e| ClientError::Connect(e.to_string()))?;
    stream
        .set_read_timeout(Some(TIMEOUT))
        .and_then(|_| stream.set_write_timeout(Some(TIMEOUT)))
        .map_err(|e| ClientError::Io(e.to_string()))?;

    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nAccept: text/plain\r\nConnection: close\r\n\r\n",
        url.path, url.host
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|e| ClientError::Io(e.to_string()))?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| ClientError::Io(e.to_string()))?;
    let status: u16 = line
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| ClientError::BadStatusLine(line.trim_end().to_string()))?;
    if status != 200 {
        return Err(ClientError::HttpStatus(status));
    }

    let mut content_length: Option<u64> = None;
    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .map_err(|e| ClientError::Io(e.to_string()))?;
        if n == 0 || line.trim_end().is_empty() {
            break;
        }
        if let Some((name, value)) = line.trim_end().split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().ok();
            }
        }
    }

    let limit = content_length
        .unwrap_or(MAX_RESPONSE_BYTES)
        .min(MAX_RESPONSE_BYTES);
    let mut body = Vec::new();
    reader
        .take(limit)
        .read_to_end(&mut body)
        .map_err(|e| ClientError::Io(e.to_string()))?;
    String::from_utf8(body).map_err(|_| ClientError::BodyNotUtf8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_urls() {
        let u = ScrapeUrl::parse("http://localhost:9100/metrics").unwrap();
        assert_eq!(u.host, "localhost");
        assert_eq!(u.port, 9100);
        assert_eq!(u.path, "/metrics");
        assert_eq!(u.as_str(), "http://localhost:9100/metrics");

        let u = ScrapeUrl::parse("http://example.com").unwrap();
        assert_eq!(u.port, 80);
        assert_eq!(u.path, "/");
    }

    #[test]
    fn rejects_bad_urls() {
        assert!(matches!(
            ScrapeUrl::parse("https://x/metrics"),
            Err(ClientError::UnsupportedScheme(_))
        ));
        assert!(matches!(
            ScrapeUrl::parse("http:///metrics"),
            Err(ClientError::MissingHost(_))
        ));
        assert!(matches!(
            ScrapeUrl::parse("http://host:notaport/"),
            Err(ClientError::InvalidPort(_))
        ));
    }
}
