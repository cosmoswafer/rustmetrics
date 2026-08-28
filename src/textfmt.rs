//! Prometheus text exposition format: boundary parser and encoder.

use std::collections::BTreeMap;
use std::fmt;

use crate::model::{
    LabelName, LabelValue, Labels, MetricKind, MetricMeta, MetricName, ScrapedSample, SeriesKey,
    TimestampMs,
};

#[derive(Debug, Clone, PartialEq)]
pub struct TextParseError {
    pub line: usize,
    pub kind: TextParseErrorKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TextParseErrorKind {
    InvalidMetricName(String),
    InvalidLabelName(String),
    DuplicateLabel(String),
    UnterminatedLabels,
    BadEscape(char),
    MissingValue,
    InvalidValue(String),
    InvalidTimestamp(String),
    InvalidType(String),
    Malformed(String),
}

impl fmt::Display for TextParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "text format line {}: ", self.line)?;
        match &self.kind {
            TextParseErrorKind::InvalidMetricName(s) => write!(f, "invalid metric name {s:?}"),
            TextParseErrorKind::InvalidLabelName(s) => write!(f, "invalid label name {s:?}"),
            TextParseErrorKind::DuplicateLabel(s) => write!(f, "duplicate label {s:?}"),
            TextParseErrorKind::UnterminatedLabels => write!(f, "unterminated label set"),
            TextParseErrorKind::BadEscape(c) => write!(f, "bad escape sequence \\{c}"),
            TextParseErrorKind::MissingValue => write!(f, "missing sample value"),
            TextParseErrorKind::InvalidValue(s) => write!(f, "invalid sample value {s:?}"),
            TextParseErrorKind::InvalidTimestamp(s) => write!(f, "invalid timestamp {s:?}"),
            TextParseErrorKind::InvalidType(s) => write!(f, "invalid TYPE {s:?}"),
            TextParseErrorKind::Malformed(s) => write!(f, "malformed line: {s}"),
        }
    }
}

impl std::error::Error for TextParseError {}

#[derive(Debug, Default, PartialEq)]
pub struct ParsedExposition {
    pub samples: Vec<ScrapedSample>,
    pub metas: Vec<(MetricName, MetricMeta)>,
}

/// Parse a full text-exposition payload (push body or scrape body).
pub fn parse(input: &str) -> Result<ParsedExposition, TextParseError> {
    let mut out = ParsedExposition::default();
    // name -> (help, kind) accumulated across HELP/TYPE lines
    let mut metas: BTreeMap<MetricName, MetricMeta> = BTreeMap::new();

    for (idx, raw) in input.lines().enumerate() {
        let lineno = idx + 1;
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(comment) = line.strip_prefix('#') {
            parse_comment(comment.trim_start(), lineno, &mut metas)?;
            continue;
        }
        out.samples.push(parse_sample_line(line, lineno)?);
    }
    out.metas = metas.into_iter().collect();
    Ok(out)
}

fn parse_comment(
    rest: &str,
    lineno: usize,
    metas: &mut BTreeMap<MetricName, MetricMeta>,
) -> Result<(), TextParseError> {
    let err = |kind| TextParseError { line: lineno, kind };
    if let Some(rest) = rest.strip_prefix("HELP ").or(rest.strip_prefix("HELP\t")) {
        let rest = rest.trim_start();
        let (name_str, help) = match rest.split_once(char::is_whitespace) {
            Some((n, h)) => (n, h.trim().to_string()),
            None => (rest, String::new()),
        };
        let name = MetricName::parse(name_str)
            .map_err(|_| err(TextParseErrorKind::InvalidMetricName(name_str.to_string())))?;
        metas.entry(name).or_default().help = unescape_help(&help);
    } else if let Some(rest) = rest.strip_prefix("TYPE ").or(rest.strip_prefix("TYPE\t")) {
        let mut it = rest.split_whitespace();
        let name_str = it.next().unwrap_or("");
        let kind_str = it.next().unwrap_or("");
        let name = MetricName::parse(name_str)
            .map_err(|_| err(TextParseErrorKind::InvalidMetricName(name_str.to_string())))?;
        let kind = MetricKind::parse(kind_str)
            .map_err(|_| err(TextParseErrorKind::InvalidType(kind_str.to_string())))?;
        metas.entry(name).or_default().kind = kind;
    }
    // other comments ignored
    Ok(())
}

fn unescape_help(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn parse_sample_line(line: &str, lineno: usize) -> Result<ScrapedSample, TextParseError> {
    let err = |kind| TextParseError { line: lineno, kind };

    // metric name: up to '{' or whitespace
    let name_end = line
        .find(|c: char| c == '{' || c.is_whitespace())
        .unwrap_or(line.len());
    let name_str = &line[..name_end];
    let name = MetricName::parse(name_str)
        .map_err(|_| err(TextParseErrorKind::InvalidMetricName(name_str.to_string())))?;

    let mut rest = &line[name_end..];
    let labels = if rest.starts_with('{') {
        let (labels, consumed) = parse_labels(&rest[1..], lineno)?;
        rest = &rest[1 + consumed..];
        labels
    } else {
        Labels::empty()
    };

    let mut parts = rest.split_whitespace();
    let value_str = parts
        .next()
        .ok_or_else(|| err(TextParseErrorKind::MissingValue))?;
    let value = parse_value(value_str)
        .ok_or_else(|| err(TextParseErrorKind::InvalidValue(value_str.to_string())))?;

    let ts = match parts.next() {
        Some(ts_str) => {
            let ms: i64 = ts_str
                .parse()
                .map_err(|_| err(TextParseErrorKind::InvalidTimestamp(ts_str.to_string())))?;
            Some(
                TimestampMs::new(ms)
                    .map_err(|_| err(TextParseErrorKind::InvalidTimestamp(ts_str.to_string())))?,
            )
        }
        None => None,
    };
    if parts.next().is_some() {
        return Err(err(TextParseErrorKind::Malformed(
            "trailing tokens after timestamp".to_string(),
        )));
    }

    Ok(ScrapedSample {
        key: SeriesKey { name, labels },
        value,
        ts,
    })
}

/// Parse label pairs after the opening `{`. Returns labels and bytes consumed
/// including the closing `}`.
fn parse_labels(s: &str, lineno: usize) -> Result<(Labels, usize), TextParseError> {
    let err = |kind| TextParseError { line: lineno, kind };
    let mut pairs: Vec<(LabelName, LabelValue)> = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;

    loop {
        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b',') {
            i += 1;
        }
        if i >= bytes.len() {
            return Err(err(TextParseErrorKind::UnterminatedLabels));
        }
        if bytes[i] == b'}' {
            i += 1;
            break;
        }

        let name_start = i;
        while i < bytes.len() && bytes[i] != b'=' {
            i += 1;
        }
        if i >= bytes.len() {
            return Err(err(TextParseErrorKind::UnterminatedLabels));
        }
        let name_str = s[name_start..i].trim();
        let lname = LabelName::parse(name_str)
            .map_err(|_| err(TextParseErrorKind::InvalidLabelName(name_str.to_string())))?;
        i += 1; // skip '='

        while i < bytes.len() && bytes[i] == b' ' {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'"' {
            return Err(err(TextParseErrorKind::UnterminatedLabels));
        }
        i += 1; // skip opening quote

        let mut value = String::new();
        let mut closed = false;
        while i < bytes.len() {
            // label values are UTF-8; iterate chars from here
            let ch = s[i..].chars().next().expect("in-bounds char");
            i += ch.len_utf8();
            match ch {
                '"' => {
                    closed = true;
                    break;
                }
                '\\' => {
                    let esc = s[i..]
                        .chars()
                        .next()
                        .ok_or_else(|| err(TextParseErrorKind::UnterminatedLabels))?;
                    i += esc.len_utf8();
                    match esc {
                        'n' => value.push('\n'),
                        '\\' => value.push('\\'),
                        '"' => value.push('"'),
                        other => return Err(err(TextParseErrorKind::BadEscape(other))),
                    }
                }
                other => value.push(other),
            }
        }
        if !closed {
            return Err(err(TextParseErrorKind::UnterminatedLabels));
        }
        pairs.push((lname, LabelValue::new(value)));
    }

    let labels = Labels::new(pairs).map_err(|e| match e {
        crate::model::ModelError::DuplicateLabel(n) => err(TextParseErrorKind::DuplicateLabel(n)),
        other => err(TextParseErrorKind::Malformed(other.to_string())),
    })?;
    Ok((labels, i))
}

fn parse_value(s: &str) -> Option<f64> {
    match s {
        "NaN" | "nan" => Some(f64::NAN),
        "+Inf" | "Inf" | "inf" => Some(f64::INFINITY),
        "-Inf" | "-inf" => Some(f64::NEG_INFINITY),
        _ => s.parse::<f64>().ok(),
    }
}

/// Encode one sample line (used by /metrics self-exposition).
pub fn encode_sample(out: &mut String, key: &SeriesKey, value: f64, ts: Option<TimestampMs>) {
    out.push_str(key.name.as_str());
    if !key.labels.is_empty() {
        out.push('{');
        for (i, (n, v)) in key.labels.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(n.as_str());
            out.push_str("=\"");
            for c in v.as_str().chars() {
                match c {
                    '\\' => out.push_str("\\\\"),
                    '"' => out.push_str("\\\""),
                    '\n' => out.push_str("\\n"),
                    other => out.push(other),
                }
            }
            out.push('"');
        }
        out.push('}');
    }
    out.push(' ');
    out.push_str(&encode_value(value));
    if let Some(ts) = ts {
        out.push(' ');
        out.push_str(&ts.as_millis().to_string());
    }
    out.push('\n');
}

pub fn encode_meta(out: &mut String, name: &MetricName, meta: &MetricMeta) {
    if !meta.help.is_empty() {
        out.push_str("# HELP ");
        out.push_str(name.as_str());
        out.push(' ');
        for c in meta.help.chars() {
            match c {
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                other => out.push(other),
            }
        }
        out.push('\n');
    }
    out.push_str("# TYPE ");
    out.push_str(name.as_str());
    out.push(' ');
    out.push_str(meta.kind.as_str());
    out.push('\n');
}

fn encode_value(v: f64) -> String {
    if v.is_nan() {
        "NaN".to_string()
    } else if v == f64::INFINITY {
        "+Inf".to_string()
    } else if v == f64::NEG_INFINITY {
        "-Inf".to_string()
    } else {
        format!("{v}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(name: &str, labels: &[(&str, &str)]) -> SeriesKey {
        SeriesKey {
            name: MetricName::parse(name).unwrap(),
            labels: Labels::new(
                labels
                    .iter()
                    .map(|(n, v)| (LabelName::parse(n).unwrap(), LabelValue::new(*v)))
                    .collect(),
            )
            .unwrap(),
        }
    }

    #[test]
    fn parses_bare_sample() {
        let p = parse("up 1\n").unwrap();
        assert_eq!(p.samples.len(), 1);
        assert_eq!(p.samples[0].key, key("up", &[]));
        assert_eq!(p.samples[0].value, 1.0);
        assert_eq!(p.samples[0].ts, None);
    }

    #[test]
    fn parses_labels_value_timestamp() {
        let p = parse("http_requests_total{method=\"post\",code=\"200\"} 1027 1395066363000\n")
            .unwrap();
        let s = &p.samples[0];
        assert_eq!(
            s.key,
            key(
                "http_requests_total",
                &[("code", "200"), ("method", "post")]
            )
        );
        assert_eq!(s.value, 1027.0);
        assert_eq!(s.ts.unwrap().as_millis(), 1_395_066_363_000);
    }

    #[test]
    fn parses_escapes_in_label_values() {
        let p = parse(r#"msg{path="C:\\dir",text="a\"b\nc"} 1"#).unwrap();
        let s = &p.samples[0];
        assert_eq!(s.key.labels.get("path").unwrap().as_str(), "C:\\dir");
        assert_eq!(s.key.labels.get("text").unwrap().as_str(), "a\"b\nc");
    }

    #[test]
    fn parses_special_values() {
        let p = parse("a NaN\nb +Inf\nc -Inf\nd 3.5e-2\n").unwrap();
        assert!(p.samples[0].value.is_nan());
        assert_eq!(p.samples[1].value, f64::INFINITY);
        assert_eq!(p.samples[2].value, f64::NEG_INFINITY);
        assert_eq!(p.samples[3].value, 0.035);
    }

    #[test]
    fn parses_help_and_type() {
        let p = parse(
            "# HELP http_requests_total Total requests.\n# TYPE http_requests_total counter\nhttp_requests_total 5\n",
        )
        .unwrap();
        assert_eq!(p.metas.len(), 1);
        let (name, meta) = &p.metas[0];
        assert_eq!(name.as_str(), "http_requests_total");
        assert_eq!(meta.kind, MetricKind::Counter);
        assert_eq!(meta.help, "Total requests.");
    }

    #[test]
    fn ignores_plain_comments_and_blank_lines() {
        let p = parse("# just a comment\n\n  \nup 1\n").unwrap();
        assert_eq!(p.samples.len(), 1);
        assert!(p.metas.is_empty());
    }

    #[test]
    fn errors_name_line_numbers() {
        let e = parse("up 1\n9bad 2\n").unwrap_err();
        assert_eq!(e.line, 2);
        assert_eq!(
            e.kind,
            TextParseErrorKind::InvalidMetricName("9bad".to_string())
        );
    }

    #[test]
    fn errors_on_missing_value_and_bad_value() {
        assert_eq!(
            parse("up\n").unwrap_err().kind,
            TextParseErrorKind::MissingValue
        );
        assert_eq!(
            parse("up abc\n").unwrap_err().kind,
            TextParseErrorKind::InvalidValue("abc".to_string())
        );
    }

    #[test]
    fn errors_on_unterminated_labels() {
        assert_eq!(
            parse("up{a=\"1\" 1\n").unwrap_err().kind,
            TextParseErrorKind::UnterminatedLabels
        );
    }

    #[test]
    fn errors_on_duplicate_label() {
        assert_eq!(
            parse("up{a=\"1\",a=\"2\"} 1\n").unwrap_err().kind,
            TextParseErrorKind::DuplicateLabel("a".to_string())
        );
    }

    #[test]
    fn histogram_series_ingest_as_plain_samples() {
        let p = parse("h_bucket{le=\"0.5\"} 3\nh_bucket{le=\"+Inf\"} 5\nh_sum 12.5\nh_count 5\n")
            .unwrap();
        assert_eq!(p.samples.len(), 4);
        assert_eq!(p.samples[0].key.labels.get("le").unwrap().as_str(), "0.5");
    }

    #[test]
    fn encode_parse_round_trip() {
        let k = key("weird", &[("a", "x\\y\"z\nw"), ("b", "plain")]);
        let mut out = String::new();
        encode_meta(
            &mut out,
            &k.name,
            &MetricMeta {
                kind: MetricKind::Gauge,
                help: "multi\nline \\help".to_string(),
            },
        );
        encode_sample(&mut out, &k, 42.5, Some(TimestampMs::new(1000).unwrap()));
        encode_sample(&mut out, &key("inf_val", &[]), f64::INFINITY, None);

        let p = parse(&out).unwrap();
        assert_eq!(p.samples[0].key, k);
        assert_eq!(p.samples[0].value, 42.5);
        assert_eq!(p.samples[0].ts.unwrap().as_millis(), 1000);
        assert_eq!(p.samples[1].value, f64::INFINITY);
        assert_eq!(p.metas[0].1.help, "multi\nline \\help");
        assert_eq!(p.metas[0].1.kind, MetricKind::Gauge);
    }
}
