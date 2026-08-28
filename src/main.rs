use std::process::ExitCode;
use std::sync::Arc;
use std::thread;

use rustmetrics::api::{self, AppState};
use rustmetrics::config::{Config, ConfigError};
use rustmetrics::http::server;
use rustmetrics::model::TimestampMs;
use rustmetrics::scraper;
use rustmetrics::snapshot;
use rustmetrics::storage::{MetricStore, DEFAULT_MAX_POINTS};

fn main() -> ExitCode {
    let config = match Config::parse(std::env::args().skip(1)) {
        Ok(c) => c,
        Err(ConfigError::HelpRequested) => {
            print!("{}", rustmetrics::config::USAGE);
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("error: {e}\n\n{}", rustmetrics::config::USAGE);
            return ExitCode::from(2);
        }
    };

    let store = Arc::new(MetricStore::new(
        config.retention.as_millis() as i64,
        DEFAULT_MAX_POINTS,
    ));

    if config.snapshots_enabled {
        let dump = snapshot::load(&config.data_dir);
        if !dump.series.is_empty() {
            println!(
                "loaded snapshot: {} series from {}",
                dump.series.len(),
                snapshot::snapshot_path(&config.data_dir).display()
            );
        }
        store.restore(dump);
        store.prune(TimestampMs::now());

        let snap_store = Arc::clone(&store);
        let data_dir = config.data_dir.clone();
        let interval = config.snapshot_interval;
        thread::spawn(move || loop {
            thread::sleep(interval);
            snap_store.prune(TimestampMs::now());
            if let Err(e) = snapshot::save(&data_dir, &snap_store.dump()) {
                eprintln!("warn: snapshot save failed: {e}");
            }
        });
    }

    if !config.scrape_targets.is_empty() {
        let scrape_store = Arc::clone(&store);
        let targets = config.scrape_targets.clone();
        let interval = config.scrape_interval;
        println!(
            "scraping {} target(s) every {}s",
            targets.len(),
            interval.as_secs()
        );
        thread::spawn(move || scraper::run(scrape_store, targets, interval));
    }

    let listener = match std::net::TcpListener::bind(config.listen) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("error: cannot listen on {}: {e}", config.listen);
            return ExitCode::FAILURE;
        }
    };
    println!("rustmetrics listening on http://{}", config.listen);
    println!("  dashboard  http://{}/", config.listen);
    println!("  push       POST http://{}/api/push", config.listen);

    let state = Arc::new(AppState::new(store));
    server::serve(listener, Arc::new(move |req| api::handle(&state, req)))
}
