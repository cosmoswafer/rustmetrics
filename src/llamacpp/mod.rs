//! `rustmetrics llamacpp` — live TUI monitor for a llama.cpp server.
//!
//! Polls `/health`, `/props`, `/v1/models` and `/slots`, parses `/metrics`
//! when the server was started with `--metrics`, with passive `/slots` counter
//! deltas as a fallback. The TUI never sends inference requests. Works against
//! a single instance or a router.

pub mod collect;
pub mod tui;

use std::io;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::ArgAction;
use ratatui::crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::DefaultTerminal;
use tokio::sync::mpsc::UnboundedReceiver;

use collect::{Collector, State};

/// Monitor a llama.cpp server with live TUI charts.
#[derive(Debug, Clone, clap::Args)]
pub struct LlamaArgs {
    /// Base URL of the llama.cpp server (router or instance)
    #[arg(
        long,
        env = "LLAMACPP_URL",
        value_name = "URL",
        default_value = "http://127.0.0.1:8080",
        value_parser = parse_base_url
    )]
    pub url: String,

    /// Poll interval in seconds
    #[arg(long, value_name = "SECS", default_value = "2", value_parser = parse_secs_f64)]
    pub interval: f64,

    /// Disable the one-shot generation probe used with --once

    #[arg(long = "no-probe", action = ArgAction::SetFalse)]
    pub probe: bool,

    /// Tokens to generate for the one-shot --once probe
    #[arg(long, value_name = "N", default_value_t = 16)]
    pub probe_tokens: u32,

    /// Pin the model to watch (default: first active/loaded model)
    #[arg(long, value_name = "NAME")]
    pub model: Option<String>,

    /// Chart history window in seconds
    #[arg(long, value_name = "SECS", default_value = "300", value_parser = parse_secs_f64)]
    pub window: f64,

    /// API key when the server requires auth
    #[arg(long, env = "LLAMACPP_API_KEY", value_name = "KEY")]
    pub api_key: Option<String>,

    /// Start with the light color scheme (toggle at runtime with `t`)
    #[arg(long)]
    pub light: bool,

    /// Poll once, print a summary and exit instead of running the TUI
    #[arg(long)]
    pub once: bool,
}

fn parse_base_url(s: &str) -> Result<String, String> {
    let url: reqwest::Url = s.parse().map_err(|e| format!("invalid URL: {e}"))?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(format!(
            "unsupported scheme {:?} (http/https only)",
            url.scheme()
        ));
    }
    if url.host_str().is_none() {
        return Err("missing host".to_string());
    }
    Ok(s.trim_end_matches('/').to_string())
}

fn parse_secs_f64(s: &str) -> Result<f64, String> {
    let v: f64 = s
        .parse()
        .map_err(|_| format!("invalid seconds value {s:?}"))?;
    if !v.is_finite() || v < 0.1 {
        return Err("must be >= 0.1".to_string());
    }
    Ok(v)
}

/// Entry point for `rustmetrics llamacpp`.
pub async fn run(args: LlamaArgs) -> ExitCode {
    let collector = Arc::new(Collector::new(
        &args.url,
        args.model.clone(),
        args.probe_tokens,
        args.api_key.clone(),
    ));
    let state = Arc::new(Mutex::new(State::new(
        args.url.clone(),
        args.window,
        args.interval,
    )));
    state.lock().unwrap().probe_enabled = args.probe;

    if args.once {
        return once(&collector, &state).await;
    }
    tui_run(collector, state, args).await
}

// ---------------------------------------------------------------------------
// --once: single cycle, human-readable summary (scripting / probe testing)
// ---------------------------------------------------------------------------

async fn once(collector: &Arc<Collector>, state: &Arc<Mutex<State>>) -> ExitCode {
    collector.poll(state).await;
    if state.lock().unwrap().probe_enabled {
        collector.probe(state).await;
    }
    let g = state.lock().unwrap();

    println!("llama.cpp {}", g.base);
    println!(
        "  status   {} · role {} · build {}",
        if g.up { "up" } else { "DOWN" },
        g.role.as_deref().unwrap_or("?"),
        g.build.as_deref().unwrap_or("?")
    );
    println!(
        "  models   {} total, {} loaded",
        g.models.len(),
        g.loaded_count()
    );
    for m in &g.models {
        let parts = [
            format!("{:>9}", m.state),
            match m.ctx {
                Some(c) => format!("ctx {c}"),
                None => "-".to_string(),
            },
            match &m.hf_repo {
                Some(r) => r.to_string(),
                None => String::new(),
            },
        ];
        println!("    {:<20} {}", m.name, parts.join("  "));
    }
    if let Some(pr) = &g.last_probe {
        match &pr.error {
            Some(e) => println!("  probe    {} failed: {e}", pr.model),
            None => println!(
                "  probe    {}: gen {} tok/s · prompt {} tok/s · wall {}ms · {} gen tok · cache {}",
                pr.model,
                tui::fmt_num(pr.gen_tps),
                tui::fmt_num(pr.prompt_tps),
                pr.wall_ms as u64,
                pr.gen_n,
                pr.cache_n
            ),
        }
        println!("  probes   {} ok / {} err", g.probes_ok, g.probes_err);
    } else {
        println!("  probe    skipped (no loaded model; use --model NAME to opt in)");
    }
    match g.metrics_supported {
        Some(true) => println!("  metrics  {} prometheus series", g.prom_discovered),
        Some(false) => println!("  metrics  no /metrics endpoint — using passive /slots deltas"),
        None => println!("  metrics  not fetched"),
    }
    if let Some(e) = &g.last_error {
        println!("  error    {e}");
        return ExitCode::FAILURE;
    }
    if g.up {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

// ---------------------------------------------------------------------------
// TUI mode
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum PollCmd {
    Refresh,
}

async fn tui_run(collector: Arc<Collector>, state: Arc<Mutex<State>>, args: LlamaArgs) -> ExitCode {
    let mut terminal = match ratatui::try_init() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: terminal init failed: {e}");
            return ExitCode::FAILURE;
        }
    };

    // blocking input reader on its own thread; events flow through a channel
    let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    std::thread::spawn(move || {
        while let Ok(ev) = ratatui::crossterm::event::read() {
            if ev_tx.send(ev).is_err() {
                break;
            }
        }
    });

    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<PollCmd>();
    tokio::spawn(poll_task(
        Arc::clone(&collector),
        Arc::clone(&state),
        cmd_rx,
        args.interval,
    ));
    let theme = if args.light {
        tui::Theme::Light
    } else {
        tui::Theme::Dark
    };
    let res = event_loop(&mut terminal, &state, &mut ev_rx, &cmd_tx, theme).await;
    ratatui::restore();
    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn poll_task(
    collector: Arc<Collector>,
    state: Arc<Mutex<State>>,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<PollCmd>,
    interval: f64,
) {
    let mut ticker = tokio::time::interval(Duration::from_secs_f64(interval));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            cmd = rx.recv() => match cmd {
                Some(PollCmd::Refresh) => {}
                None => return,
            }
        }
        collector.poll(&state).await;
    }
}

async fn event_loop(
    terminal: &mut DefaultTerminal,
    state: &Arc<Mutex<State>>,
    ev_rx: &mut UnboundedReceiver<Event>,
    cmd_tx: &tokio::sync::mpsc::UnboundedSender<PollCmd>,
    mut theme: tui::Theme,
) -> io::Result<()> {
    let mut tick = tokio::time::interval(Duration::from_millis(200));
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            ev = ev_rx.recv() => match ev {
                None => break,
                Some(Event::Key(k)) if k.kind == KeyEventKind::Press => {
                    match k.code {
                        KeyCode::Char('q') | KeyCode::Esc => break,
                        KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => break,
                        KeyCode::Tab => {
                            state.lock().unwrap().cycle_focus();
                        }
                        KeyCode::Char('m') => {
                            let mut g = state.lock().unwrap();
                            g.show_metrics = !g.show_metrics;
                        }
                        KeyCode::Char('t') => {
                            theme = theme.toggle();
                        }
                        KeyCode::Char('r') => {
                            let _ = cmd_tx.send(PollCmd::Refresh);
                        }
                        _ => {}
                    }
                }
                Some(_) => {}
            }
        }
        let g = state.lock().unwrap();
        terminal.draw(|f| tui::draw(f, &g, theme))?;
    }
    Ok(())
}
