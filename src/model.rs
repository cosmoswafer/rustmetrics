use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

/// Validated metric name: `[a-zA-Z_:][a-zA-Z0-9_:]*`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MetricName(String);

impl MetricName {
    pub fn parse(s: &str) -> Result<Self, ModelError> {
        let mut chars = s.chars();
        let valid_first = |c: char| c.is_ascii_alphabetic() || c == '_' || c == ':';
        let valid_rest = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == ':';
        match chars.next() {
            Some(c) if valid_first(c) && chars.all(valid_rest) => Ok(MetricName(s.to_string())),
            _ => Err(ModelError::InvalidMetricName(s.to_string())),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for MetricName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Validated label name: `[a-zA-Z_][a-zA-Z0-9_]*`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LabelName(String);

impl LabelName {
    pub fn parse(s: &str) -> Result<Self, ModelError> {
        let mut chars = s.chars();
        let valid_first = |c: char| c.is_ascii_alphabetic() || c == '_';
        let valid_rest = |c: char| c.is_ascii_alphanumeric() || c == '_';
        match chars.next() {
            Some(c) if valid_first(c) && chars.all(valid_rest) => Ok(LabelName(s.to_string())),
            _ => Err(ModelError::InvalidLabelName(s.to_string())),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for LabelName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Label value: any UTF-8 string (escaping is the encoders' concern).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LabelValue(String);

impl LabelValue {
    pub fn new(s: impl Into<String>) -> Self {
        LabelValue(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Canonical sorted label set with unique names.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Labels(Vec<(LabelName, LabelValue)>);

impl Labels {
    pub fn empty() -> Self {
        Labels(Vec::new())
    }

    pub fn new(mut pairs: Vec<(LabelName, LabelValue)>) -> Result<Self, ModelError> {
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
        for w in pairs.windows(2) {
            if w[0].0 == w[1].0 {
                return Err(ModelError::DuplicateLabel(w[0].0.as_str().to_string()));
            }
        }
        Ok(Labels(pairs))
    }

    pub fn iter(&self) -> impl Iterator<Item = &(LabelName, LabelValue)> {
        self.0.iter()
    }

    pub fn get(&self, name: &str) -> Option<&LabelValue> {
        self.0
            .iter()
            .find(|(n, _)| n.as_str() == name)
            .map(|(_, v)| v)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }
}

/// Unique identity of a time series.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SeriesKey {
    pub name: MetricName,
    pub labels: Labels,
}

/// Metric kind from `# TYPE` metadata; a display hint only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricKind {
    Counter,
    Gauge,
    Histogram,
    Summary,
    Untyped,
}

impl MetricKind {
    pub fn parse(s: &str) -> Result<Self, ModelError> {
        match s {
            "counter" => Ok(MetricKind::Counter),
            "gauge" => Ok(MetricKind::Gauge),
            "histogram" => Ok(MetricKind::Histogram),
            "summary" => Ok(MetricKind::Summary),
            "untyped" => Ok(MetricKind::Untyped),
            _ => Err(ModelError::InvalidMetricKind(s.to_string())),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            MetricKind::Counter => "counter",
            MetricKind::Gauge => "gauge",
            MetricKind::Histogram => "histogram",
            MetricKind::Summary => "summary",
            MetricKind::Untyped => "untyped",
        }
    }
}

/// Per-metric metadata from HELP/TYPE lines.
#[derive(Debug, Clone, PartialEq)]
pub struct MetricMeta {
    pub kind: MetricKind,
    pub help: String,
}

impl Default for MetricMeta {
    fn default() -> Self {
        MetricMeta {
            kind: MetricKind::Untyped,
            help: String::new(),
        }
    }
}

/// Unix timestamp in milliseconds, bounded to a sane range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TimestampMs(i64);

impl TimestampMs {
    // years ~1970-9999
    const MAX: i64 = 253_402_300_799_000;

    pub fn new(ms: i64) -> Result<Self, ModelError> {
        if (0..=Self::MAX).contains(&ms) {
            Ok(TimestampMs(ms))
        } else {
            Err(ModelError::TimestampOutOfRange(ms))
        }
    }

    pub fn now() -> Self {
        let ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        TimestampMs(ms.min(Self::MAX))
    }

    pub fn as_millis(&self) -> i64 {
        self.0
    }

    pub fn saturating_sub_millis(&self, ms: i64) -> Self {
        TimestampMs(self.0.saturating_sub(ms).max(0))
    }
}

/// A single measured point.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    pub ts: TimestampMs,
    pub value: f64,
}

/// One parsed sample from a push body or scrape, before storage.
#[derive(Debug, Clone, PartialEq)]
pub struct ScrapedSample {
    pub key: SeriesKey,
    pub value: f64,
    /// None means "stamp with server time at ingestion".
    pub ts: Option<TimestampMs>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelError {
    InvalidMetricName(String),
    InvalidLabelName(String),
    InvalidMetricKind(String),
    DuplicateLabel(String),
    TimestampOutOfRange(i64),
}

impl fmt::Display for ModelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ModelError::InvalidMetricName(s) => write!(f, "MetricName: invalid name {s:?}"),
            ModelError::InvalidLabelName(s) => write!(f, "LabelName: invalid name {s:?}"),
            ModelError::InvalidMetricKind(s) => write!(f, "MetricKind: unknown kind {s:?}"),
            ModelError::DuplicateLabel(s) => write!(f, "Labels: duplicate label name {s:?}"),
            ModelError::TimestampOutOfRange(ms) => {
                write!(f, "TimestampMs: value {ms} out of range")
            }
        }
    }
}

impl std::error::Error for ModelError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metric_name_accepts_valid() {
        for s in ["a", "http_requests_total", "_x", "ns:sub:metric", "A9_z"] {
            assert!(MetricName::parse(s).is_ok(), "{s}");
        }
    }

    #[test]
    fn metric_name_rejects_invalid() {
        for s in ["", "9abc", "with-dash", "with space", "é"] {
            assert!(MetricName::parse(s).is_err(), "{s}");
        }
    }

    #[test]
    fn label_name_accepts_and_rejects() {
        assert!(LabelName::parse("instance").is_ok());
        assert!(LabelName::parse("_priv").is_ok());
        assert!(LabelName::parse("le").is_ok());
        assert!(LabelName::parse("9x").is_err());
        assert!(LabelName::parse("a:b").is_err());
        assert!(LabelName::parse("").is_err());
    }

    #[test]
    fn labels_sorted_and_unique() {
        let l = Labels::new(vec![
            (LabelName::parse("b").unwrap(), LabelValue::new("2")),
            (LabelName::parse("a").unwrap(), LabelValue::new("1")),
        ])
        .unwrap();
        let names: Vec<&str> = l.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);

        let dup = Labels::new(vec![
            (LabelName::parse("a").unwrap(), LabelValue::new("1")),
            (LabelName::parse("a").unwrap(), LabelValue::new("2")),
        ]);
        assert_eq!(dup, Err(ModelError::DuplicateLabel("a".to_string())));
    }

    #[test]
    fn labels_identity_is_order_independent() {
        let a = Labels::new(vec![
            (LabelName::parse("x").unwrap(), LabelValue::new("1")),
            (LabelName::parse("y").unwrap(), LabelValue::new("2")),
        ])
        .unwrap();
        let b = Labels::new(vec![
            (LabelName::parse("y").unwrap(), LabelValue::new("2")),
            (LabelName::parse("x").unwrap(), LabelValue::new("1")),
        ])
        .unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn timestamp_bounds() {
        assert!(TimestampMs::new(0).is_ok());
        assert!(TimestampMs::new(-1).is_err());
        assert!(TimestampMs::new(i64::MAX).is_err());
        assert!(TimestampMs::now().as_millis() > 1_600_000_000_000);
    }

    #[test]
    fn metric_kind_parse() {
        assert_eq!(MetricKind::parse("gauge"), Ok(MetricKind::Gauge));
        assert!(MetricKind::parse("Gauge").is_err());
    }
}
