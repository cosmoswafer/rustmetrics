//! Binary snapshot persistence: `RMX1` + version, length-prefixed records.

use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::model::{
    LabelName, LabelValue, Labels, MetricKind, MetricMeta, MetricName, Sample, SeriesKey,
    TimestampMs,
};
use crate::storage::StoreDump;

const MAGIC: &[u8; 4] = b"RMX1";
const VERSION: u32 = 1;
const MAX_STR_LEN: u32 = 1 << 20;
const MAX_COUNT: u32 = 1 << 24;

#[derive(Debug)]
pub enum SnapshotError {
    Io(io::Error),
    BadMagic,
    UnsupportedVersion(u32),
    Truncated(&'static str),
    FieldTooLarge(&'static str, u32),
    InvalidUtf8(&'static str),
    InvalidField(&'static str, String),
}

impl fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SnapshotError::Io(e) => write!(f, "snapshot: io error: {e}"),
            SnapshotError::BadMagic => write!(f, "snapshot: bad magic header"),
            SnapshotError::UnsupportedVersion(v) => {
                write!(f, "snapshot: unsupported version {v}")
            }
            SnapshotError::Truncated(field) => {
                write!(f, "snapshot: truncated while reading {field}")
            }
            SnapshotError::FieldTooLarge(field, n) => {
                write!(f, "snapshot: field {field} too large ({n})")
            }
            SnapshotError::InvalidUtf8(field) => {
                write!(f, "snapshot: field {field} is not valid UTF-8")
            }
            SnapshotError::InvalidField(field, v) => {
                write!(f, "snapshot: field {field} invalid: {v}")
            }
        }
    }
}

impl std::error::Error for SnapshotError {}

impl From<io::Error> for SnapshotError {
    fn from(e: io::Error) -> Self {
        SnapshotError::Io(e)
    }
}

pub fn encode(dump: &StoreDump) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(MAGIC);
    put_u32(&mut buf, VERSION);

    put_u32(&mut buf, dump.metas.len() as u32);
    for (name, meta) in &dump.metas {
        put_str(&mut buf, name.as_str());
        put_str(&mut buf, meta.kind.as_str());
        put_str(&mut buf, &meta.help);
    }

    put_u32(&mut buf, dump.series.len() as u32);
    for (key, samples) in &dump.series {
        put_str(&mut buf, key.name.as_str());
        put_u32(&mut buf, key.labels.len() as u32);
        for (n, v) in key.labels.iter() {
            put_str(&mut buf, n.as_str());
            put_str(&mut buf, v.as_str());
        }
        put_u32(&mut buf, samples.len() as u32);
        for s in samples {
            buf.extend_from_slice(&s.ts.as_millis().to_le_bytes());
            buf.extend_from_slice(&s.value.to_le_bytes());
        }
    }
    buf
}

pub fn decode(bytes: &[u8]) -> Result<StoreDump, SnapshotError> {
    let mut r = Reader { bytes, pos: 0 };
    if r.take(4, "magic")? != MAGIC {
        return Err(SnapshotError::BadMagic);
    }
    let version = r.u32("version")?;
    if version != VERSION {
        return Err(SnapshotError::UnsupportedVersion(version));
    }

    let meta_count = r.count("meta count")?;
    let mut metas = Vec::with_capacity(meta_count as usize);
    for _ in 0..meta_count {
        let name_str = r.string("meta name")?;
        let name = MetricName::parse(&name_str)
            .map_err(|_| SnapshotError::InvalidField("meta name", name_str.clone()))?;
        let kind_str = r.string("meta kind")?;
        let kind = MetricKind::parse(&kind_str)
            .map_err(|_| SnapshotError::InvalidField("meta kind", kind_str.clone()))?;
        let help = r.string("meta help")?;
        metas.push((name, MetricMeta { kind, help }));
    }

    let series_count = r.count("series count")?;
    let mut series = Vec::with_capacity(series_count as usize);
    for _ in 0..series_count {
        let name_str = r.string("series name")?;
        let name = MetricName::parse(&name_str)
            .map_err(|_| SnapshotError::InvalidField("series name", name_str.clone()))?;
        let label_count = r.count("label count")?;
        let mut pairs = Vec::with_capacity(label_count as usize);
        for _ in 0..label_count {
            let ln_str = r.string("label name")?;
            let ln = LabelName::parse(&ln_str)
                .map_err(|_| SnapshotError::InvalidField("label name", ln_str.clone()))?;
            let lv = LabelValue::new(r.string("label value")?);
            pairs.push((ln, lv));
        }
        let labels =
            Labels::new(pairs).map_err(|e| SnapshotError::InvalidField("labels", e.to_string()))?;
        let sample_count = r.count("sample count")?;
        let mut samples = Vec::with_capacity(sample_count as usize);
        for _ in 0..sample_count {
            let ts_ms =
                i64::from_le_bytes(r.take(8, "sample ts")?.try_into().expect("8-byte slice"));
            let value =
                f64::from_le_bytes(r.take(8, "sample value")?.try_into().expect("8-byte slice"));
            let ts = TimestampMs::new(ts_ms)
                .map_err(|_| SnapshotError::InvalidField("sample ts", ts_ms.to_string()))?;
            samples.push(Sample { ts, value });
        }
        series.push((SeriesKey { name, labels }, samples));
    }

    Ok(StoreDump { series, metas })
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize, field: &'static str) -> Result<&'a [u8], SnapshotError> {
        if self.pos + n > self.bytes.len() {
            return Err(SnapshotError::Truncated(field));
        }
        let out = &self.bytes[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    fn u32(&mut self, field: &'static str) -> Result<u32, SnapshotError> {
        Ok(u32::from_le_bytes(
            self.take(4, field)?.try_into().expect("4-byte slice"),
        ))
    }

    fn count(&mut self, field: &'static str) -> Result<u32, SnapshotError> {
        let n = self.u32(field)?;
        if n > MAX_COUNT {
            return Err(SnapshotError::FieldTooLarge(field, n));
        }
        Ok(n)
    }

    fn string(&mut self, field: &'static str) -> Result<String, SnapshotError> {
        let len = self.u32(field)?;
        if len > MAX_STR_LEN {
            return Err(SnapshotError::FieldTooLarge(field, len));
        }
        let bytes = self.take(len as usize, field)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| SnapshotError::InvalidUtf8(field))
    }
}

fn put_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}

fn put_str(buf: &mut Vec<u8>, s: &str) {
    put_u32(buf, s.len() as u32);
    buf.extend_from_slice(s.as_bytes());
}

pub fn snapshot_path(data_dir: &Path) -> PathBuf {
    data_dir.join("snapshot.rmx")
}

/// Atomic save: write to `.tmp`, then rename over the target.
pub fn save(data_dir: &Path, dump: &StoreDump) -> Result<(), SnapshotError> {
    fs::create_dir_all(data_dir)?;
    let path = snapshot_path(data_dir);
    let tmp = data_dir.join("snapshot.rmx.tmp");
    let mut f = fs::File::create(&tmp)?;
    f.write_all(&encode(dump))?;
    f.sync_all()?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

/// Load on startup. Missing file -> empty dump. Corrupt file -> renamed to
/// `.corrupt`, empty dump returned (never refuses to boot).
pub fn load(data_dir: &Path) -> StoreDump {
    let path = snapshot_path(data_dir);
    let bytes = match fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return StoreDump::default(),
        Err(e) => {
            eprintln!("warn: cannot read snapshot {}: {e}", path.display());
            return StoreDump::default();
        }
    };
    match decode(&bytes) {
        Ok(dump) => dump,
        Err(e) => {
            eprintln!("warn: {e}; starting fresh");
            let corrupt = data_dir.join("snapshot.rmx.corrupt");
            if let Err(re) = fs::rename(&path, &corrupt) {
                eprintln!("warn: cannot quarantine corrupt snapshot: {re}");
            }
            StoreDump::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ScrapedSample;
    use crate::storage::MetricStore;

    fn make_dump() -> StoreDump {
        let store = MetricStore::new(i64::MAX / 2, 1000);
        let name = MetricName::parse("http_requests_total").unwrap();
        let labels = Labels::new(vec![(
            LabelName::parse("code").unwrap(),
            LabelValue::new("2\"00\n\\"),
        )])
        .unwrap();
        store.ingest(
            vec![
                ScrapedSample {
                    key: SeriesKey {
                        name: name.clone(),
                        labels,
                    },
                    value: 12.5,
                    ts: Some(TimestampMs::new(1000).unwrap()),
                },
                ScrapedSample {
                    key: SeriesKey {
                        name: MetricName::parse("nan_metric").unwrap(),
                        labels: Labels::empty(),
                    },
                    value: f64::NAN,
                    ts: Some(TimestampMs::new(2000).unwrap()),
                },
            ],
            vec![(
                name,
                MetricMeta {
                    kind: MetricKind::Counter,
                    help: "Total requests".to_string(),
                },
            )],
            TimestampMs::new(2000).unwrap(),
        );
        store.dump()
    }

    #[test]
    fn encode_decode_round_trip() {
        let dump = make_dump();
        let decoded = decode(&encode(&dump)).unwrap();
        // NaN != NaN, so compare piecewise
        assert_eq!(decoded.metas, dump.metas);
        assert_eq!(decoded.series.len(), dump.series.len());
        for ((k1, s1), (k2, s2)) in decoded.series.iter().zip(dump.series.iter()) {
            assert_eq!(k1, k2);
            assert_eq!(s1.len(), s2.len());
            for (a, b) in s1.iter().zip(s2.iter()) {
                assert_eq!(a.ts, b.ts);
                assert!(a.value == b.value || (a.value.is_nan() && b.value.is_nan()));
            }
        }
    }

    #[test]
    fn rejects_bad_magic_and_version() {
        assert!(matches!(decode(b"NOPE"), Err(SnapshotError::BadMagic)));
        let mut bytes = encode(&StoreDump::default());
        bytes[4] = 99;
        assert!(matches!(
            decode(&bytes),
            Err(SnapshotError::UnsupportedVersion(99))
        ));
    }

    #[test]
    fn rejects_truncated() {
        let bytes = encode(&make_dump());
        let e = decode(&bytes[..bytes.len() - 4]).unwrap_err();
        assert!(matches!(e, SnapshotError::Truncated(_)), "{e}");
    }

    #[test]
    fn save_load_round_trip_and_corruption_quarantine() {
        let dir = std::env::temp_dir().join(format!(
            "rustmetrics-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);

        // missing file -> empty
        assert_eq!(load(&dir), StoreDump::default());

        let dump = {
            let mut d = make_dump();
            // drop the NaN series to allow direct equality
            d.series.retain(|(k, _)| k.name.as_str() != "nan_metric");
            d
        };
        save(&dir, &dump).unwrap();
        assert_eq!(load(&dir), dump);

        // corrupt file -> quarantined, empty dump
        fs::write(snapshot_path(&dir), b"garbage").unwrap();
        assert_eq!(load(&dir), StoreDump::default());
        assert!(dir.join("snapshot.rmx.corrupt").exists());
        assert!(!snapshot_path(&dir).exists());

        let _ = fs::remove_dir_all(&dir);
    }
}
