//! CLI argument boundary parser -> typed Config.

use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use crate::http::client::ScrapeUrl;

pub const USAGE: &str = "\
rustmetrics — minimalist metrics collection & dashboard in one binary

USAGE:
    rustmetrics [OPTIONS]

OPTIONS:
    --listen <addr>             Listen address (default: 127.0.0.1:9090)
    --scrape <url>              Scrape target, repeatable (http:// only)
    --scrape-interval <secs>    Scrape interval (default: 15)
    --data-dir <path>           Snapshot directory (default: ./rustmetrics-data)
    --snapshot-interval <secs>  Snapshot interval (default: 60)
    --retention <secs>          Sample retention window (default: 86400)
    --no-snapshot               Disable snapshot persistence
    --help                      Show this help
";

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub listen: SocketAddr,
    pub scrape_targets: Vec<ScrapeUrl>,
    pub scrape_interval: Duration,
    pub data_dir: PathBuf,
    pub snapshot_interval: Duration,
    pub retention: Duration,
    pub snapshots_enabled: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            listen: "127.0.0.1:9090".parse().expect("valid literal addr"),
            scrape_targets: Vec::new(),
            scrape_interval: Duration::from_secs(15),
            data_dir: PathBuf::from("./rustmetrics-data"),
            snapshot_interval: Duration::from_secs(60),
            retention: Duration::from_secs(86_400),
            snapshots_enabled: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ConfigError {
    HelpRequested,
    UnknownFlag(String),
    MissingValue(&'static str),
    InvalidValue(&'static str, String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::HelpRequested => f.write_str(USAGE),
            ConfigError::UnknownFlag(flag) => write!(f, "Config: unknown flag {flag:?}"),
            ConfigError::MissingValue(flag) => write!(f, "Config: {flag} requires a value"),
            ConfigError::InvalidValue(flag, v) => {
                write!(f, "Config: invalid value for {flag}: {v}")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, ConfigError> {
        let mut cfg = Config::default();
        let mut it = args.into_iter();
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--help" | "-h" => return Err(ConfigError::HelpRequested),
                "--listen" => {
                    let v = it.next().ok_or(ConfigError::MissingValue("--listen"))?;
                    cfg.listen = v
                        .parse()
                        .map_err(|_| ConfigError::InvalidValue("--listen", v))?;
                }
                "--scrape" => {
                    let v = it.next().ok_or(ConfigError::MissingValue("--scrape"))?;
                    let url = ScrapeUrl::parse(&v)
                        .map_err(|e| ConfigError::InvalidValue("--scrape", e.to_string()))?;
                    cfg.scrape_targets.push(url);
                }
                "--scrape-interval" => {
                    cfg.scrape_interval = parse_secs("--scrape-interval", &mut it)?;
                }
                "--data-dir" => {
                    let v = it.next().ok_or(ConfigError::MissingValue("--data-dir"))?;
                    cfg.data_dir = PathBuf::from(v);
                }
                "--snapshot-interval" => {
                    cfg.snapshot_interval = parse_secs("--snapshot-interval", &mut it)?;
                }
                "--retention" => {
                    cfg.retention = parse_secs("--retention", &mut it)?;
                }
                "--no-snapshot" => cfg.snapshots_enabled = false,
                other => return Err(ConfigError::UnknownFlag(other.to_string())),
            }
        }
        Ok(cfg)
    }
}

fn parse_secs(
    flag: &'static str,
    it: &mut impl Iterator<Item = String>,
) -> Result<Duration, ConfigError> {
    let v = it.next().ok_or(ConfigError::MissingValue(flag))?;
    let secs: u64 = v
        .parse()
        .map_err(|_| ConfigError::InvalidValue(flag, v.clone()))?;
    if secs == 0 {
        return Err(ConfigError::InvalidValue(flag, v));
    }
    Ok(Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Config, ConfigError> {
        Config::parse(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn defaults_with_no_args() {
        let cfg = parse(&[]).unwrap();
        assert_eq!(cfg, Config::default());
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
        assert_eq!(
            parse(&["--bogus"]),
            Err(ConfigError::UnknownFlag("--bogus".to_string()))
        );
        assert_eq!(
            parse(&["--listen"]),
            Err(ConfigError::MissingValue("--listen"))
        );
        assert!(matches!(
            parse(&["--listen", "not-an-addr"]),
            Err(ConfigError::InvalidValue("--listen", _))
        ));
        assert!(matches!(
            parse(&["--scrape", "ftp://x/"]),
            Err(ConfigError::InvalidValue("--scrape", _))
        ));
        assert!(matches!(
            parse(&["--retention", "0"]),
            Err(ConfigError::InvalidValue("--retention", _))
        ));
        assert_eq!(parse(&["--help"]), Err(ConfigError::HelpRequested));
    }
}
