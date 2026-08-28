//! CLI configuration via clap.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use reqwest::Url;

/// Minimalist metrics collection & dashboard in one binary.
#[derive(Debug, Clone, Parser)]
#[command(name = "rustmetrics", version)]
pub struct Config {
    /// Listen address
    #[arg(long, default_value = "127.0.0.1:9090")]
    pub listen: SocketAddr,

    /// Scrape target URL, repeatable (http:// only)
    #[arg(long = "scrape", value_name = "URL", value_parser = parse_scrape_url)]
    pub scrape_targets: Vec<Url>,

    /// Scrape interval in seconds
    #[arg(long, default_value = "15", value_name = "SECS", value_parser = parse_secs)]
    pub scrape_interval: Duration,

    /// Snapshot directory
    #[arg(long, default_value = "./rustmetrics-data", value_name = "PATH")]
    pub data_dir: PathBuf,

    /// Snapshot interval in seconds
    #[arg(long, default_value = "60", value_name = "SECS", value_parser = parse_secs)]
    pub snapshot_interval: Duration,

    /// Sample retention window in seconds
    #[arg(long, default_value = "86400", value_name = "SECS", value_parser = parse_secs)]
    pub retention: Duration,

    /// Disable snapshot persistence
    #[arg(long = "no-snapshot", action = clap::ArgAction::SetFalse)]
    pub snapshots_enabled: bool,
}

fn parse_scrape_url(s: &str) -> Result<Url, String> {
    let url: Url = s.parse().map_err(|e| format!("invalid URL: {e}"))?;
    if url.scheme() != "http" {
        return Err(format!(
            "unsupported scheme {:?} (http:// only)",
            url.scheme()
        ));
    }
    if url.host_str().is_none() {
        return Err("missing host".to_string());
    }
    Ok(url)
}

fn parse_secs(s: &str) -> Result<Duration, String> {
    let secs: u64 = s
        .parse()
        .map_err(|_| format!("invalid seconds value {s:?}"))?;
    if secs == 0 {
        return Err("must be greater than 0".to_string());
    }
    Ok(Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Config, clap::Error> {
        Config::try_parse_from(std::iter::once("rustmetrics").chain(args.iter().copied()))
    }

    #[test]
    fn defaults_with_no_args() {
        let cfg = parse(&[]).unwrap();
        assert_eq!(cfg.listen, "127.0.0.1:9090".parse().unwrap());
        assert!(cfg.scrape_targets.is_empty());
        assert_eq!(cfg.scrape_interval, Duration::from_secs(15));
        assert_eq!(cfg.data_dir, PathBuf::from("./rustmetrics-data"));
        assert_eq!(cfg.snapshot_interval, Duration::from_secs(60));
        assert_eq!(cfg.retention, Duration::from_secs(86_400));
        assert!(cfg.snapshots_enabled);
    }

    #[test]
    fn parses_all_flags() {
        let cfg = parse(&[
            "--listen",
            "0.0.0.0:8000",
            "--scrape",
            "http://a:1/metrics",
            "--scrape",
            "http://b:2/metrics",
            "--scrape-interval",
            "5",
            "--data-dir",
            "/tmp/rmx",
            "--snapshot-interval",
            "30",
            "--retention",
            "3600",
            "--no-snapshot",
        ])
        .unwrap();
        assert_eq!(cfg.listen.port(), 8000);
        assert_eq!(cfg.scrape_targets.len(), 2);
        assert_eq!(cfg.scrape_interval, Duration::from_secs(5));
        assert_eq!(cfg.data_dir, PathBuf::from("/tmp/rmx"));
        assert_eq!(cfg.snapshot_interval, Duration::from_secs(30));
        assert_eq!(cfg.retention, Duration::from_secs(3600));
        assert!(!cfg.snapshots_enabled);
    }

    #[test]
    fn rejects_bad_input() {
        assert!(parse(&["--bogus"]).is_err());
        assert!(parse(&["--listen"]).is_err());
        assert!(parse(&["--listen", "not-an-addr"]).is_err());
        assert!(parse(&["--scrape", "ftp://x/"]).is_err());
        assert!(parse(&["--scrape", "https://x/metrics"]).is_err());
        assert!(parse(&["--retention", "0"]).is_err());
    }
}
