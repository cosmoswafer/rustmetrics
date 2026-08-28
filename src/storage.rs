//! In-memory time-series store: ring buffers, retention, queries.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

use crate::model::{
    LabelName, LabelValue, Labels, MetricMeta, MetricName, Sample, ScrapedSample, SeriesKey,
    TimestampMs,
};

pub const DEFAULT_MAX_POINTS: usize = 10_000;

#[derive(Debug)]
struct Series {
    samples: VecDeque<Sample>,
}

#[derive(Debug, Default)]
struct StoreInner {
    series: HashMap<SeriesKey, Series>,
    metas: HashMap<MetricName, MetricMeta>,
}

#[derive(Debug)]
pub struct MetricStore {
    inner: RwLock<StoreInner>,
    retention_ms: i64,
    max_points: usize,
    ingested_total: AtomicU64,
    dropped_total: AtomicU64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MetricInfo {
    pub name: MetricName,
    pub meta: MetricMeta,
    pub series_count: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct QuerySeries {
    pub labels: Labels,
    pub points: Vec<Sample>,
}

/// Typed, already-validated query arguments.
#[derive(Debug, Clone)]
pub struct QueryRange {
    pub from: TimestampMs,
    pub to: TimestampMs,
    pub step_ms: i64,
}

/// A full dump of the store, shared shape between storage and snapshot.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct StoreDump {
    pub series: Vec<(SeriesKey, Vec<Sample>)>,
    pub metas: Vec<(MetricName, MetricMeta)>,
}

impl MetricStore {
    pub fn new(retention_ms: i64, max_points: usize) -> Self {
        MetricStore {
            inner: RwLock::new(StoreInner::default()),
            retention_ms,
            max_points,
            ingested_total: AtomicU64::new(0),
            dropped_total: AtomicU64::new(0),
        }
    }

    /// Ingest a parsed exposition batch. Samples without timestamps get `now`.
    /// Samples older than the series head are dropped (counted, not an error).
    pub fn ingest(
        &self,
        samples: Vec<ScrapedSample>,
        metas: Vec<(MetricName, MetricMeta)>,
        now: TimestampMs,
    ) {
        let cutoff = now.saturating_sub_millis(self.retention_ms);
        let mut ingested = 0u64;
        let mut dropped = 0u64;
        let mut inner = self.inner.write().unwrap_or_else(|e| e.into_inner());

        for (name, meta) in metas {
            let entry = inner.metas.entry(name).or_default();
            entry.kind = meta.kind;
            if !meta.help.is_empty() {
                entry.help = meta.help;
            }
        }

        for s in samples {
            let ts = s.ts.unwrap_or(now);
            if ts < cutoff {
                dropped += 1;
                continue;
            }
            let series = inner.series.entry(s.key).or_insert_with(|| Series {
                samples: VecDeque::new(),
            });
            if let Some(last) = series.samples.back() {
                if ts < last.ts {
                    dropped += 1;
                    continue;
                }
            }
            series.samples.push_back(Sample { ts, value: s.value });
            while series.samples.len() > self.max_points {
                series.samples.pop_front();
            }
            while series
                .samples
                .front()
                .is_some_and(|front| front.ts < cutoff)
            {
                series.samples.pop_front();
            }
            ingested += 1;
        }

        self.ingested_total.fetch_add(ingested, Ordering::Relaxed);
        self.dropped_total.fetch_add(dropped, Ordering::Relaxed);
    }

    /// Drop samples older than retention and remove empty series.
    pub fn prune(&self, now: TimestampMs) {
        let cutoff = now.saturating_sub_millis(self.retention_ms);
        let mut inner = self.inner.write().unwrap_or_else(|e| e.into_inner());
        inner.series.retain(|_, series| {
            while series
                .samples
                .front()
                .is_some_and(|front| front.ts < cutoff)
            {
                series.samples.pop_front();
            }
            !series.samples.is_empty()
        });
    }

    pub fn list_metrics(&self) -> Vec<MetricInfo> {
        let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
        let mut counts: BTreeMap<&MetricName, usize> = BTreeMap::new();
        for key in inner.series.keys() {
            *counts.entry(&key.name).or_insert(0) += 1;
        }
        counts
            .into_iter()
            .map(|(name, series_count)| MetricInfo {
                name: name.clone(),
                meta: inner.metas.get(name).cloned().unwrap_or_default(),
                series_count,
            })
            .collect()
    }

    /// All label names -> value sets seen for a metric.
    pub fn label_values(&self, metric: &MetricName) -> BTreeMap<LabelName, BTreeSet<LabelValue>> {
        let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
        let mut out: BTreeMap<LabelName, BTreeSet<LabelValue>> = BTreeMap::new();
        for key in inner.series.keys().filter(|k| &k.name == metric) {
            for (n, v) in key.labels.iter() {
                out.entry(n.clone()).or_default().insert(v.clone());
            }
        }
        out
    }

    /// Range query with equality label filters. Points are downsampled to one
    /// (last) sample per step bucket.
    pub fn query(
        &self,
        metric: &MetricName,
        filters: &[(LabelName, LabelValue)],
        range: &QueryRange,
    ) -> Vec<QuerySeries> {
        let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
        let step = range.step_ms.max(1);
        let mut out: Vec<QuerySeries> = inner
            .series
            .iter()
            .filter(|(key, _)| {
                &key.name == metric
                    && filters
                        .iter()
                        .all(|(n, v)| key.labels.get(n.as_str()) == Some(v))
            })
            .map(|(key, series)| {
                let mut points: Vec<Sample> = Vec::new();
                for s in series
                    .samples
                    .iter()
                    .filter(|s| s.ts >= range.from && s.ts <= range.to)
                {
                    let bucket = (s.ts.as_millis() - range.from.as_millis()) / step;
                    match points.last() {
                        Some(last)
                            if (last.ts.as_millis() - range.from.as_millis()) / step == bucket =>
                        {
                            *points.last_mut().expect("non-empty") = *s;
                        }
                        _ => points.push(*s),
                    }
                }
                QuerySeries {
                    labels: key.labels.clone(),
                    points,
                }
            })
            .filter(|qs| !qs.points.is_empty())
            .collect();
        out.sort_by(|a, b| a.labels.cmp(&b.labels));
        out
    }

    pub fn dump(&self) -> StoreDump {
        let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
        let mut series: Vec<(SeriesKey, Vec<Sample>)> = inner
            .series
            .iter()
            .map(|(k, s)| (k.clone(), s.samples.iter().copied().collect()))
            .collect();
        series.sort_by(|a, b| a.0.cmp(&b.0));
        let mut metas: Vec<(MetricName, MetricMeta)> = inner
            .metas
            .iter()
            .map(|(n, m)| (n.clone(), m.clone()))
            .collect();
        metas.sort_by(|a, b| a.0.cmp(&b.0));
        StoreDump { series, metas }
    }

    /// Replace store contents from a snapshot dump (startup only).
    pub fn restore(&self, dump: StoreDump) {
        let mut inner = self.inner.write().unwrap_or_else(|e| e.into_inner());
        inner.series = dump
            .series
            .into_iter()
            .map(|(k, samples)| {
                (
                    k,
                    Series {
                        samples: samples.into_iter().collect(),
                    },
                )
            })
            .collect();
        inner.metas = dump.metas.into_iter().collect();
    }

    pub fn stats(&self) -> StoreStats {
        let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
        StoreStats {
            series_count: inner.series.len(),
            metric_count: inner
                .series
                .keys()
                .map(|k| &k.name)
                .collect::<BTreeSet<_>>()
                .len(),
            ingested_total: self.ingested_total.load(Ordering::Relaxed),
            dropped_total: self.dropped_total.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreStats {
    pub series_count: usize,
    pub metric_count: usize,
    pub ingested_total: u64,
    pub dropped_total: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(s: &str) -> MetricName {
        MetricName::parse(s).unwrap()
    }

    fn labels(pairs: &[(&str, &str)]) -> Labels {
        Labels::new(
            pairs
                .iter()
                .map(|(n, v)| (LabelName::parse(n).unwrap(), LabelValue::new(*v)))
                .collect(),
        )
        .unwrap()
    }

    fn sample(metric: &str, lbls: &[(&str, &str)], ts: i64, value: f64) -> ScrapedSample {
        ScrapedSample {
            key: SeriesKey {
                name: name(metric),
                labels: labels(lbls),
            },
            value,
            ts: Some(TimestampMs::new(ts).unwrap()),
        }
    }

    fn ts(ms: i64) -> TimestampMs {
        TimestampMs::new(ms).unwrap()
    }

    #[test]
    fn ingest_and_query_basic() {
        let store = MetricStore::new(1_000_000, 100);
        store.ingest(
            vec![
                sample("m", &[("a", "1")], 1000, 1.0),
                sample("m", &[("a", "1")], 2000, 2.0),
                sample("m", &[("a", "2")], 1500, 9.0),
            ],
            vec![],
            ts(2000),
        );
        let range = QueryRange {
            from: ts(0),
            to: ts(5000),
            step_ms: 1,
        };
        let result = store.query(&name("m"), &[], &range);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].labels, labels(&[("a", "1")]));
        assert_eq!(result[0].points.len(), 2);
        assert_eq!(result[1].points[0].value, 9.0);
    }

    #[test]
    fn label_filter_narrows_query() {
        let store = MetricStore::new(1_000_000, 100);
        store.ingest(
            vec![
                sample("m", &[("a", "1")], 1000, 1.0),
                sample("m", &[("a", "2")], 1000, 2.0),
            ],
            vec![],
            ts(1000),
        );
        let range = QueryRange {
            from: ts(0),
            to: ts(5000),
            step_ms: 1,
        };
        let result = store.query(
            &name("m"),
            &[(LabelName::parse("a").unwrap(), LabelValue::new("2"))],
            &range,
        );
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].points[0].value, 2.0);
    }

    #[test]
    fn out_of_order_samples_dropped() {
        let store = MetricStore::new(1_000_000, 100);
        store.ingest(
            vec![sample("m", &[], 2000, 1.0), sample("m", &[], 1000, 0.5)],
            vec![],
            ts(2000),
        );
        let stats = store.stats();
        assert_eq!(stats.ingested_total, 1);
        assert_eq!(stats.dropped_total, 1);
    }

    #[test]
    fn ring_buffer_caps_points() {
        let store = MetricStore::new(i64::MAX / 2, 3);
        let samples = (1..=5)
            .map(|i| sample("m", &[], i * 1000, i as f64))
            .collect();
        store.ingest(samples, vec![], ts(5000));
        let range = QueryRange {
            from: ts(0),
            to: ts(10_000),
            step_ms: 1,
        };
        let result = store.query(&name("m"), &[], &range);
        assert_eq!(result[0].points.len(), 3);
        assert_eq!(result[0].points[0].value, 3.0);
    }

    #[test]
    fn retention_prunes_old_samples() {
        let store = MetricStore::new(10_000, 100);
        store.ingest(vec![sample("m", &[], 1000, 1.0)], vec![], ts(1000));
        store.ingest(vec![sample("m", &[], 20_000, 2.0)], vec![], ts(20_000));
        let range = QueryRange {
            from: ts(0),
            to: ts(30_000),
            step_ms: 1,
        };
        let result = store.query(&name("m"), &[], &range);
        assert_eq!(result[0].points.len(), 1);
        assert_eq!(result[0].points[0].value, 2.0);

        store.prune(ts(100_000));
        assert_eq!(store.stats().series_count, 0);
    }

    #[test]
    fn too_old_incoming_sample_dropped() {
        let store = MetricStore::new(10_000, 100);
        store.ingest(vec![sample("m", &[], 1000, 1.0)], vec![], ts(50_000));
        assert_eq!(store.stats().dropped_total, 1);
        assert_eq!(store.stats().series_count, 0);
    }

    #[test]
    fn step_bucketing_keeps_last_per_bucket() {
        let store = MetricStore::new(1_000_000, 100);
        let samples = (0..10)
            .map(|i| sample("m", &[], i * 100, i as f64))
            .collect();
        store.ingest(samples, vec![], ts(1000));
        let range = QueryRange {
            from: ts(0),
            to: ts(1000),
            step_ms: 500,
        };
        let result = store.query(&name("m"), &[], &range);
        // buckets [0,500) and [500,1000): last of each = 4.0 and 9.0
        let values: Vec<f64> = result[0].points.iter().map(|p| p.value).collect();
        assert_eq!(values, vec![4.0, 9.0]);
    }

    #[test]
    fn metas_merge_and_list() {
        use crate::model::{MetricKind, MetricMeta};
        let store = MetricStore::new(1_000_000, 100);
        store.ingest(
            vec![sample("m", &[], 1000, 1.0)],
            vec![(
                name("m"),
                MetricMeta {
                    kind: MetricKind::Counter,
                    help: "helpful".to_string(),
                },
            )],
            ts(1000),
        );
        let list = store.list_metrics();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].meta.kind, MetricKind::Counter);
        assert_eq!(list[0].meta.help, "helpful");
        assert_eq!(list[0].series_count, 1);
    }

    #[test]
    fn dump_restore_round_trip() {
        let store = MetricStore::new(1_000_000, 100);
        store.ingest(
            vec![
                sample("m", &[("a", "1")], 1000, 1.0),
                sample("n", &[], 2000, 2.0),
            ],
            vec![],
            ts(2000),
        );
        let dump = store.dump();
        let store2 = MetricStore::new(1_000_000, 100);
        store2.restore(dump.clone());
        assert_eq!(store2.dump(), dump);
    }

    #[test]
    fn label_values_collects() {
        let store = MetricStore::new(1_000_000, 100);
        store.ingest(
            vec![
                sample("m", &[("a", "1"), ("b", "x")], 1000, 1.0),
                sample("m", &[("a", "2")], 1000, 1.0),
            ],
            vec![],
            ts(1000),
        );
        let lv = store.label_values(&name("m"));
        assert_eq!(lv.len(), 2);
        assert_eq!(lv[&LabelName::parse("a").unwrap()].len(), 2);
    }
}
