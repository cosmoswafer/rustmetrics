//! Shared HTTP types: parsed requests, responses, query params.

pub mod client;
pub mod server;

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
}

impl Method {
    pub fn parse(s: &str) -> Result<Self, HttpError> {
        match s {
            "GET" => Ok(Method::Get),
            "POST" => Ok(Method::Post),
            other => Err(HttpError::UnsupportedMethod(other.to_string())),
        }
    }
}

/// Decoded query parameters, preserving order and repeats.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct QueryParams(Vec<(String, String)>);

impl QueryParams {
    pub fn parse(raw: &str) -> Result<Self, HttpError> {
        let mut pairs = Vec::new();
        for part in raw.split('&').filter(|p| !p.is_empty()) {
            let (k, v) = part.split_once('=').unwrap_or((part, ""));
            pairs.push((percent_decode(k)?, percent_decode(v)?));
        }
        Ok(QueryParams(pairs))
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }
}

/// A fully parsed HTTP request — the only request shape handlers ever see.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub method: Method,
    pub path: String,
    pub query: QueryParams,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

impl Response {
    pub fn json(status: u16, body: String) -> Self {
        Response {
            status,
            content_type: "application/json; charset=utf-8",
            body: body.into_bytes(),
        }
    }

    pub fn text(status: u16, body: String) -> Self {
        Response {
            status,
            content_type: "text/plain; charset=utf-8",
            body: body.into_bytes(),
        }
    }

    pub fn html(body: &'static str) -> Self {
        Response {
            status: 200,
            content_type: "text/html; charset=utf-8",
            body: body.as_bytes().to_vec(),
        }
    }

    pub fn no_content() -> Self {
        Response {
            status: 204,
            content_type: "text/plain; charset=utf-8",
            body: Vec::new(),
        }
    }

    pub fn json_error(status: u16, message: &str) -> Self {
        let mut w = crate::json::JsonWriter::new();
        w.begin_object();
        w.key("error").string(message);
        w.end_object();
        Response::json(status, w.finish())
    }
}

pub fn status_reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        411 => "Length Required",
        413 => "Payload Too Large",
        500 => "Internal Server Error",
        _ => "Unknown",
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum HttpError {
    MalformedRequestLine(String),
    UnsupportedMethod(String),
    MalformedHeader(String),
    HeadersTooLarge,
    BodyTooLarge(usize),
    LengthRequired,
    InvalidContentLength(String),
    BadPercentEncoding(String),
    Io(String),
}

impl HttpError {
    pub fn status(&self) -> u16 {
        match self {
            HttpError::UnsupportedMethod(_) => 405,
            HttpError::HeadersTooLarge => 413,
            HttpError::BodyTooLarge(_) => 413,
            HttpError::LengthRequired => 411,
            _ => 400,
        }
    }
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HttpError::MalformedRequestLine(l) => {
                write!(f, "Request: malformed request line {l:?}")
            }
            HttpError::UnsupportedMethod(m) => write!(f, "Request: unsupported method {m:?}"),
            HttpError::MalformedHeader(h) => write!(f, "Request: malformed header {h:?}"),
            HttpError::HeadersTooLarge => write!(f, "Request: header block too large"),
            HttpError::BodyTooLarge(n) => write!(f, "Request: body of {n} bytes too large"),
            HttpError::LengthRequired => write!(f, "Request: Content-Length required"),
            HttpError::InvalidContentLength(v) => {
                write!(f, "Request: invalid Content-Length {v:?}")
            }
            HttpError::BadPercentEncoding(s) => {
                write!(f, "QueryParams: bad percent-encoding in {s:?}")
            }
            HttpError::Io(e) => write!(f, "Request: io error: {e}"),
        }
    }
}

impl std::error::Error for HttpError {}

pub fn percent_decode(s: &str) -> Result<String, HttpError> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = bytes
                    .get(i + 1..i + 3)
                    .ok_or_else(|| HttpError::BadPercentEncoding(s.to_string()))?;
                let hs = std::str::from_utf8(hex)
                    .map_err(|_| HttpError::BadPercentEncoding(s.to_string()))?;
                let byte = u8::from_str_radix(hs, 16)
                    .map_err(|_| HttpError::BadPercentEncoding(s.to_string()))?;
                out.push(byte);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|_| HttpError::BadPercentEncoding(s.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decoding() {
        assert_eq!(percent_decode("a%20b+c").unwrap(), "a b c");
        assert_eq!(percent_decode("%E2%9C%93").unwrap(), "\u{2713}");
        assert!(percent_decode("%zz").is_err());
        assert!(percent_decode("%2").is_err());
    }

    #[test]
    fn query_params_parse() {
        let q = QueryParams::parse("metric=up&label.job=api%20server&empty=&flag").unwrap();
        assert_eq!(q.get("metric"), Some("up"));
        assert_eq!(q.get("label.job"), Some("api server"));
        assert_eq!(q.get("empty"), Some(""));
        assert_eq!(q.get("flag"), Some(""));
        assert_eq!(q.get("missing"), None);
    }
}
