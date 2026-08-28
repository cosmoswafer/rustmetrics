use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;

use rustmetrics::api::AppState;
use rustmetrics::config::Config;
use rustmetrics::model::TimestampMs;
use rustmetrics::scraper;
use rustmetrics::snapshot;
use rustmetrics::storage::{MetricStore, DEFAULT_MAX_POINTS};

#[tokio::main]
async fn main() -> ExitCode {
    let config = Config::parse();

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
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            ticker.tick().await; // first tick fires immediately; skip it
            loop {
                ticker.tick().await;
                snap_store.prune(TimestampMs::now());
                if let Err(e) = snapshot::save(&data_dir, &snap_store.dump()) {
                    eprintln!("warn: snapshot save failed: {e}");
                }
            }
        });
    }

    if !config.scrape_targets.is_empty() {
        println!(
            "scraping {} target(s) every {}s",
            config.scrape_targets.len(),
            config.scrape_interval.as_secs()
        );
        tokio::spawn(scraper::run(
            Arc::clone(&store),
            scraper::client(),
            config.scrape_targets.clone(),
            config.scrape_interval,
        ));
    }

    let listener = match tokio::net::TcpListener::bind(config.listen).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("error: cannot listen on {}: {e}", config.listen);
            return ExitCode::FAILURE;
        }
    };
    println!("rustmetrics listening on http://{}", config.listen);
    println!("  dashboard  http://{}/", config.listen);
    println!("  push       POST http://{}/api/push", config.listen);

    let app = rustmetrics::api::router(Arc::new(AppState::new(Arc::clone(&store))));
    let served = axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            println!("shutting down");
        })
        .await;
    if let Err(e) = served {
        eprintln!("error: server failed: {e}");
        return ExitCode::FAILURE;
    }

    if config.snapshots_enabled {
        if let Err(e) = snapshot::save(&config.data_dir, &store.dump()) {
            eprintln!("warn: final snapshot save failed: {e}");
        }
    }
    ExitCode::SUCCESS
}
