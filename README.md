# rustmetrics

A minimalist, lightweight, zero-config metrics collection & dashboard system written in Rust — a zero-dependency clone of Prometheus + Grafana in one binary. Collects metrics and renders dashboards.

## Quick start

```sh
cargo run --release
```

The server starts on `http://127.0.0.1:9090` — open it in a browser for the dashboard.

Push some metrics (Prometheus text format):

```sh
curl -X POST --data-binary '
# TYPE cpu_usage gauge
cpu_usage{core="0"} 37.5
cpu_usage{core="1"} 42.1
' http://127.0.0.1:9090/api/push
```

Or scrape existing Prometheus-style targets instead:

```sh
rustmetrics --scrape http://localhost:9100/metrics --scrape-interval 15
```

Select `cpu_usage` (or `up` when scraping) in the dashboard sidebar to plot it.

### Endpoints

| Endpoint | Description |
| --- | --- |
| `GET /` | Dashboard UI |
| `POST /api/push` | Ingest metrics (Prometheus text format) |
| `GET /api/metrics` | List known metrics (JSON) |
| `GET /api/labels?metric=NAME` | Label names/values for a metric |
| `GET /api/query?metric=NAME&from=MS&to=MS&step=MS&label.KEY=VALUE` | Range query (JSON) |
| `GET /metrics` | Self-exposition (scrape rustmetrics itself) |

### Options

Data is kept in memory (24h retention) and snapshotted to `./rustmetrics-data` every 60s, so it survives restarts. Run `rustmetrics --help` for all flags: `--listen`, `--scrape`, `--scrape-interval`, `--data-dir`, `--snapshot-interval`, `--retention`, `--no-snapshot`.