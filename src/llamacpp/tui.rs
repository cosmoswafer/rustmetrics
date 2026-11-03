//! Rendering the llama.cpp monitor TUI.
//!
//! The renderer deliberately uses only Unicode block/box characters and standard
//! terminal colors: no font downloads, images, or extra runtime dependencies.

use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Cell, Padding, Paragraph, Row, Sparkline, Table, Wrap};
use ratatui::Frame;

use crate::llamacpp::collect::{Series, SlotInfo, State};

/// Color scheme used by the dashboard. The light scheme keeps the same accent
/// hues as the dark scheme, with darker variants that remain legible on white.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Theme {
    #[default]
    Dark,
    Light,
}

impl Theme {
    pub fn toggle(self) -> Self {
        match self {
            Self::Dark => Self::Light,
            Self::Light => Self::Dark,
        }
    }

    fn palette(self) -> Palette {
        match self {
            Self::Dark => DARK_PALETTE,
            Self::Light => LIGHT_PALETTE,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Palette {
    bg: Color,
    surface: Color,
    grid: Color,
    border: Color,
    text: Color,
    muted: Color,
    dim: Color,
    cyan: Color,
    green: Color,
    amber: Color,
    violet: Color,
    red: Color,
    heading: Color,
    fill_base: Color,
}

const DARK_PALETTE: Palette = Palette {
    bg: Color::Rgb(7, 11, 18),
    surface: Color::Rgb(13, 20, 30),
    grid: Color::Rgb(19, 29, 42),
    border: Color::Rgb(42, 57, 72),
    text: Color::Rgb(220, 231, 239),
    muted: Color::Rgb(128, 148, 163),
    dim: Color::Rgb(75, 94, 111),
    cyan: Color::Rgb(40, 210, 205),
    green: Color::Rgb(118, 211, 99),
    amber: Color::Rgb(245, 177, 66),
    violet: Color::Rgb(190, 132, 240),
    red: Color::Rgb(245, 99, 105),
    heading: Color::Rgb(244, 249, 252),
    fill_base: Color::Rgb(0, 0, 0),
};

const LIGHT_PALETTE: Palette = Palette {
    bg: Color::Rgb(241, 245, 247),
    surface: Color::Rgb(255, 255, 255),
    grid: Color::Rgb(218, 227, 232),
    border: Color::Rgb(178, 192, 201),
    text: Color::Rgb(37, 55, 68),
    muted: Color::Rgb(77, 98, 111),
    dim: Color::Rgb(96, 116, 128),
    cyan: Color::Rgb(0, 122, 119),
    green: Color::Rgb(38, 116, 57),
    amber: Color::Rgb(145, 82, 0),
    violet: Color::Rgb(113, 58, 151),
    red: Color::Rgb(176, 35, 52),
    heading: Color::Rgb(15, 39, 53),
    fill_base: Color::Rgb(255, 255, 255),
};

const PARTIAL_BLOCKS: [&str; 8] = ["", "▁", "▂", "▃", "▄", "▅", "▆", "▇"];

/// Read-only statistics used by both the KPI cards and chart headings.
#[derive(Debug, Clone, Copy)]
struct MetricStats {
    now: f64,
    avg: f64,
    peak: f64,
    trend: Trend,
}

impl MetricStats {
    fn from_series(series: Option<&Series>) -> Option<Self> {
        let series = series?;
        let now = series.last()?.v;
        let values = series.values();
        let previous = values.iter().rev().nth(1).copied();
        Some(Self {
            now,
            avg: series.avg().unwrap_or(now),
            peak: series.max().unwrap_or(now),
            trend: Trend::between(previous, now),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Trend {
    Up,
    Down,
    Steady,
    Unknown,
}

impl Trend {
    fn between(previous: Option<f64>, now: f64) -> Self {
        let Some(previous) = previous else {
            return Self::Unknown;
        };
        let tolerance = previous.abs().max(now.abs()).max(1.0) * 0.01;
        if now > previous + tolerance {
            Self::Up
        } else if now < previous - tolerance {
            Self::Down
        } else {
            Self::Steady
        }
    }

    fn symbol(self) -> &'static str {
        match self {
            Self::Up => "↗",
            Self::Down => "↘",
            Self::Steady => "→",
            Self::Unknown => "·",
        }
    }

    fn color(self, p: &Palette) -> Color {
        match self {
            Self::Up => p.green,
            Self::Down => p.red,
            Self::Steady => p.muted,
            Self::Unknown => p.dim,
        }
    }
}

pub fn draw(f: &mut Frame, st: &State, theme: Theme) {
    let p = theme.palette();
    f.render_widget(Block::default().style(Style::new().bg(p.bg)), f.area());
    if f.area().is_empty() {
        return;
    }

    let show_kpis = f.area().width >= 92 && f.area().height >= 25;
    let mut constraints = vec![Constraint::Length(4)];
    if show_kpis {
        constraints.push(Constraint::Length(5));
    }
    constraints.extend([Constraint::Min(3), Constraint::Length(1)]);
    let areas = Layout::vertical(constraints).split(f.area());

    draw_header(f, areas[0], st, &p);
    let main_index = if show_kpis {
        draw_kpis(f, areas[1], st, &p);
        2
    } else {
        1
    };
    let main = areas[main_index];

    if main.width >= 80 {
        let sidebar_width = if main.width >= 140 { 40 } else { 36 };
        let [left, right] =
            Layout::horizontal([Constraint::Length(sidebar_width), Constraint::Min(40)])
                .spacing(1)
                .areas(main);
        draw_sidebar(f, left, st, &p);
        draw_charts(f, right, st, &p);
    } else if main.height >= 16 {
        let [model, charts] =
            Layout::vertical([Constraint::Length(6), Constraint::Min(3)]).areas(main);
        draw_compact_model(f, model, st, &p);
        draw_charts(f, charts, st, &p);
    } else {
        draw_charts(f, main, st, &p);
    }

    draw_footer(f, areas[areas.len() - 1], st, theme, &p);
}

// ---------------------------------------------------------------------------
// shared chrome
// ---------------------------------------------------------------------------

fn panel<'a>(title: Line<'a>, accent: Color, p: &Palette) -> Block<'a> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(p.border).bg(p.surface))
        .style(Style::new().bg(p.surface))
        .title(title)
        .title_style(Style::new().fg(accent).add_modifier(Modifier::BOLD))
        .padding(Padding::horizontal(1))
}

fn title(text: impl Into<String>, accent: Color) -> Line<'static> {
    Line::from(Span::styled(
        format!(" {} ", text.into()),
        Style::new().fg(accent).add_modifier(Modifier::BOLD),
    ))
}

fn right_title(text: impl Into<String>, p: &Palette) -> Line<'static> {
    Line::from(Span::styled(
        format!(" {} ", text.into()),
        Style::new().fg(p.dim),
    ))
    .right_aligned()
}

// ---------------------------------------------------------------------------
// header / footer
// ---------------------------------------------------------------------------

fn draw_header(f: &mut Frame, area: Rect, st: &State, p: &Palette) {
    let status_color = if st.up { p.green } else { p.red };
    let status = if st.up { "● LIVE" } else { "● DOWN" };
    let block = panel(Line::default(), p.cyan, p).padding(Padding::horizontal(1));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height < 2 {
        return;
    }

    let rows = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(inner);
    let top = Layout::horizontal([Constraint::Min(30), Constraint::Length(26)]).split(rows[0]);
    let brand = Line::from(vec![
        Span::styled("◆ ", Style::new().fg(p.cyan)),
        Span::styled(
            "RUSTMETRICS",
            Style::new().fg(p.heading).add_modifier(Modifier::BOLD),
        ),
        Span::styled("  /  ", Style::new().fg(p.dim)),
        Span::styled(
            "LLAMA.CPP",
            Style::new().fg(p.cyan).add_modifier(Modifier::BOLD),
        ),
        Span::styled("  OBSERVABILITY", Style::new().fg(p.muted)),
    ]);
    f.render_widget(Paragraph::new(brand), top[0]);
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                status,
                Style::new().fg(status_color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  SESSION {}", fmt_uptime(st.uptime_secs())),
                Style::new().fg(p.muted),
            ),
        ]))
        .alignment(Alignment::Right),
        top[1],
    );

    let bottom = Layout::horizontal([Constraint::Min(30), Constraint::Length(28)]).split(rows[1]);
    let info = if let Some(error) = &st.last_error {
        Line::from(vec![
            Span::styled("! ", Style::new().fg(p.red).add_modifier(Modifier::BOLD)),
            Span::styled(format!("err: {error}"), Style::new().fg(p.red)),
        ])
    } else {
        let mut spans = vec![
            Span::styled("URL ", Style::new().fg(p.dim)),
            Span::styled(st.base.clone(), Style::new().fg(p.cyan)),
        ];
        if let Some(role) = &st.role {
            spans.push(Span::styled("   ·   ", Style::new().fg(p.dim)));
            spans.push(Span::styled(role.clone(), Style::new().fg(p.text)));
        }
        if let Some(build) = &st.build {
            spans.push(Span::styled("  build ", Style::new().fg(p.dim)));
            spans.push(Span::styled(build.clone(), Style::new().fg(p.muted)));
        }
        Line::from(spans)
    };
    f.render_widget(Paragraph::new(info), bottom[0]);
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!("{} models", st.models.len()),
                Style::new().fg(p.text).add_modifier(Modifier::BOLD),
            ),
            Span::styled(", ", Style::new().fg(p.dim)),
            Span::styled(
                format!("{} loaded", st.loaded_count()),
                Style::new().fg(p.green),
            ),
        ]))
        .alignment(Alignment::Right),
        bottom[1],
    );
}

fn draw_footer(f: &mut Frame, area: Rect, st: &State, theme: Theme, p: &Palette) {
    let columns = Layout::horizontal([Constraint::Min(48), Constraint::Length(58)]).split(area);
    let key = |key: &'static str| {
        Span::styled(
            format!(" {key} "),
            Style::new()
                .fg(p.bg)
                .bg(p.cyan)
                .add_modifier(Modifier::BOLD),
        )
    };
    let next_theme = match theme {
        Theme::Dark => "light",
        Theme::Light => "dark",
    };
    let keys = Line::from(vec![
        key("tab"),
        Span::styled("model  ", Style::new().fg(p.muted)),
        key("m"),
        Span::styled("metrics  ", Style::new().fg(p.muted)),
        key("t"),
        Span::styled(format!("{next_theme}  "), Style::new().fg(p.muted)),
        key("r"),
        Span::styled("refresh  ", Style::new().fg(p.muted)),
        key("q"),
        Span::styled("quit", Style::new().fg(p.muted)),
    ]);
    f.render_widget(
        Paragraph::new(keys).style(Style::new().bg(p.bg)),
        columns[0],
    );

    let source = match st.metrics_supported {
        Some(true) => "prometheus",
        Some(false) => "slot deltas",
        None => "discovering",
    };
    let mut spans = vec![
        Span::styled("● passive", Style::new().fg(p.green)),
        Span::styled(format!("  ·  {source}"), Style::new().fg(p.muted)),
        Span::styled(
            format!("  ·  {} history", fmt_window(st.window)),
            Style::new().fg(p.dim),
        ),
    ];
    if st.metrics_supported == Some(true) {
        spans.push(Span::styled(
            format!("  ·  {} series", st.prom_discovered),
            Style::new().fg(p.dim),
        ));
    }
    f.render_widget(
        Paragraph::new(Line::from(spans))
            .alignment(Alignment::Right)
            .style(Style::new().bg(p.bg)),
        columns[1],
    );
}

// ---------------------------------------------------------------------------
// KPI cards
// ---------------------------------------------------------------------------

fn draw_kpis(f: &mut Frame, area: Rect, st: &State, p: &Palette) {
    let focus = st.focus_model().map(|m| m.name.clone()).unwrap_or_default();
    let cards = Layout::horizontal([
        Constraint::Ratio(1, 4),
        Constraint::Ratio(1, 4),
        Constraint::Ratio(1, 4),
        Constraint::Ratio(1, 4),
    ])
    .spacing(1)
    .split(area);

    metric_card(
        f,
        cards[0],
        st.series.get(&format!("gen_tps/{focus}")),
        "GENERATION",
        "tok/s",
        p.cyan,
        p,
    );
    metric_card(
        f,
        cards[1],
        st.series.get(&format!("prompt_tps/{focus}")),
        "PROMPT",
        "tok/s",
        p.green,
        p,
    );
    metric_card(
        f,
        cards[2],
        st.series.get(&format!("cache_tokens/{focus}")),
        "KV CACHE",
        "tokens",
        p.amber,
        p,
    );
    metric_card(
        f,
        cards[3],
        st.series.get(&format!("ctx_pct/{focus}")),
        "CONTEXT",
        "%",
        p.violet,
        p,
    );
}

fn metric_card(
    f: &mut Frame,
    area: Rect,
    series: Option<&Series>,
    name: &str,
    unit: &str,
    accent: Color,
    p: &Palette,
) {
    let stats = MetricStats::from_series(series);
    let block = panel(title(name, accent), accent, p);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height < 3 {
        return;
    }
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(inner);

    let value = stats.map(|s| fmt_num(s.now)).unwrap_or_else(|| "—".into());
    let trend = stats.map(|s| s.trend).unwrap_or(Trend::Unknown);
    let top = Layout::horizontal([Constraint::Min(8), Constraint::Length(2)]).split(rows[0]);
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                value,
                Style::new().fg(p.heading).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" {unit}"), Style::new().fg(accent)),
        ])),
        top[0],
    );
    f.render_widget(
        Paragraph::new(Span::styled(
            trend.symbol(),
            Style::new().fg(trend.color(p)).add_modifier(Modifier::BOLD),
        ))
        .alignment(Alignment::Right),
        top[1],
    );

    if series.is_some() {
        // Preserve small percentages and sub-token changes in the u64-based
        // sparkline widget instead of rounding them all the way to zero.
        let (data, max) = spark_data_scaled(series, rows[1].width as usize, 100.0);
        let spark_max = nice_max(max.unwrap_or(0.0)) * 100.0;
        f.render_widget(
            Sparkline::default()
                .data(&data)
                .max(spark_max as u64)
                .style(Style::new().fg(accent).bg(p.surface)),
            rows[1],
        );
    } else {
        f.render_widget(
            Paragraph::new(Span::styled("waiting for samples", Style::new().fg(p.dim))),
            rows[1],
        );
    }

    let summary = match stats {
        Some(s) => format!("avg {}  ·  peak {}", fmt_num(s.avg), fmt_num(s.peak)),
        None => "avg —  ·  peak —".to_string(),
    };
    f.render_widget(
        Paragraph::new(Span::styled(summary, Style::new().fg(p.muted))),
        rows[2],
    );
}

// ---------------------------------------------------------------------------
// model / slot sidebar
// ---------------------------------------------------------------------------

fn draw_sidebar(f: &mut Frame, area: Rect, st: &State, p: &Palette) {
    let show_session = area.height >= 25;
    let constraints = if show_session {
        vec![
            Constraint::Length(9),
            Constraint::Min(8),
            Constraint::Length(8),
        ]
    } else {
        vec![Constraint::Length(9), Constraint::Min(8)]
    };
    let areas = Layout::vertical(constraints).spacing(1).split(area);
    draw_model_card(f, areas[0], st, p);
    draw_slots(f, areas[1], st, p);
    if show_session {
        draw_session(f, areas[2], st, p);
    }
}

fn draw_model_card(f: &mut Frame, area: Rect, st: &State, p: &Palette) {
    let focus = st.focus_model();
    let block =
        panel(title("active model", p.cyan), p.cyan, p).title(right_title("tab: active only", p));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let Some(model) = focus else {
        f.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled("no model selected", Style::new().fg(p.muted))),
                Line::from(Span::styled(
                    "waiting for /v1/models",
                    Style::new().fg(p.dim),
                )),
            ]),
            inner,
        );
        return;
    };
    if inner.height < 6 {
        f.render_widget(
            Paragraph::new(model.name.as_str()).style(Style::new().fg(p.heading)),
            inner,
        );
        return;
    }

    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Length(1),
    ])
    .split(inner);
    f.render_widget(
        Paragraph::new(Span::styled(
            model.name.clone(),
            Style::new().fg(p.heading).add_modifier(Modifier::BOLD),
        )),
        rows[0],
    );

    let state_color = model_state_color(&model.state, p);
    let mut state = vec![
        Span::styled("● ", Style::new().fg(state_color)),
        Span::styled(
            model.state.to_uppercase(),
            Style::new().fg(state_color).add_modifier(Modifier::BOLD),
        ),
    ];
    if let Some(spec) = &model.spec_type {
        state.push(Span::styled(
            format!("  ·  {spec}"),
            Style::new().fg(p.muted),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(state)), rows[1]);

    let mut metadata = Vec::new();
    if let Some(ctx) = model.ctx {
        metadata.push(Span::styled("CTX ", Style::new().fg(p.dim)));
        metadata.push(Span::styled(fmt_num(ctx as f64), Style::new().fg(p.text)));
    }
    if let Some(gpu) = &model.main_gpu {
        metadata.push(Span::styled("   GPU ", Style::new().fg(p.dim)));
        metadata.push(Span::styled(gpu.clone(), Style::new().fg(p.text)));
    }
    if let Some(layers) = &model.gpu_layers {
        metadata.push(Span::styled("   LAYERS ", Style::new().fg(p.dim)));
        metadata.push(Span::styled(layers.clone(), Style::new().fg(p.text)));
    }
    if metadata.is_empty() {
        metadata.push(Span::styled(
            "runtime metadata unavailable",
            Style::new().fg(p.dim),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(metadata)), rows[2]);
    f.render_widget(
        Paragraph::new(Span::styled("HF REPOSITORY", Style::new().fg(p.dim))),
        rows[3],
    );
    f.render_widget(
        Paragraph::new(model.hf_repo.as_deref().unwrap_or("not reported"))
            .style(Style::new().fg(if model.hf_repo.is_some() {
                p.muted
            } else {
                p.dim
            }))
            .wrap(Wrap { trim: false }),
        rows[4],
    );
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!("{:02}", st.loaded_count()),
                Style::new().fg(p.green).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" loaded  /  ", Style::new().fg(p.dim)),
            Span::styled(format!("{:02}", st.models.len()), Style::new().fg(p.text)),
            Span::styled(" models in fleet", Style::new().fg(p.dim)),
        ])),
        rows[5],
    );
}

fn model_state_color(state: &str, p: &Palette) -> Color {
    match state {
        "loaded" => p.green,
        "loading" | "sleeping" => p.amber,
        "unloaded" => p.dim,
        _ => p.cyan,
    }
}

fn draw_compact_model(f: &mut Frame, area: Rect, st: &State, p: &Palette) {
    let focus = st.focus_model();
    let name = focus
        .map(|m| m.name.as_str())
        .unwrap_or("no model selected");
    let block = panel(title("active model", p.cyan), p.cyan, p);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let columns = Layout::horizontal([
        Constraint::Percentage(45),
        Constraint::Percentage(25),
        Constraint::Percentage(30),
    ])
    .spacing(2)
    .split(inner);

    let state = focus.map(|m| m.state.as_str()).unwrap_or("offline");
    let repo = focus
        .and_then(|m| m.hf_repo.as_deref())
        .unwrap_or("waiting for model data");
    f.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                name,
                Style::new().fg(p.heading).add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                format!("● {}", state.to_uppercase()),
                Style::new().fg(model_state_color(state, p)),
            )),
            Line::from(Span::styled(repo, Style::new().fg(p.muted))),
        ])
        .wrap(Wrap { trim: true }),
        columns[0],
    );

    let busy = st.slots.iter().filter(|s| s.processing).count();
    let used = st.slots.iter().map(|s| s.prompt_tokens).sum::<u64>();
    let total = st.slots.iter().map(|s| s.n_ctx).sum::<u64>();
    f.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled(format!("{busy}"), Style::new().fg(p.amber)),
                Span::styled(
                    format!(" / {} busy", st.slots.len()),
                    Style::new().fg(p.muted),
                ),
            ]),
            Line::from(Span::styled("SLOT ACTIVITY", Style::new().fg(p.dim))),
            Line::from(Span::styled(
                format!("{} / {} ctx", fmt_count(used), fmt_count(total)),
                Style::new().fg(p.violet),
            )),
        ]),
        columns[1],
    );

    let generation = st
        .series
        .get(&format!("gen_tps/{name}"))
        .and_then(|series| series.last())
        .map(|sample| fmt_num(sample.v))
        .unwrap_or_else(|| "—".into());
    let prompt = st
        .series
        .get(&format!("prompt_tps/{name}"))
        .and_then(|series| series.last())
        .map(|sample| fmt_num(sample.v))
        .unwrap_or_else(|| "—".into());
    let activity_lines = vec![
        Line::from(Span::styled(
            format!("{generation} tok/s"),
            Style::new().fg(p.cyan).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            format!("{prompt} prompt tok/s"),
            Style::new().fg(p.green),
        )),
        Line::from(Span::styled("PASSIVE THROUGHPUT", Style::new().fg(p.dim))),
    ];
    f.render_widget(Paragraph::new(activity_lines), columns[2]);
}

fn draw_slots(f: &mut Frame, area: Rect, st: &State, p: &Palette) {
    let busy = st.slots.iter().filter(|s| s.processing).count();
    let block = panel(title("slot activity", p.violet), p.violet, p)
        .title(right_title(format!("{busy}/{} busy", st.slots.len()), p));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if st.slots.is_empty() {
        f.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled("○  NO ACTIVE SLOTS", Style::new().fg(p.muted))),
                Line::from(""),
                Line::from(Span::styled(
                    "the watched model is not loaded",
                    Style::new().fg(p.dim),
                )),
            ]),
            inner,
        );
        return;
    }

    let per_slot = if inner.height >= 5 { 5 } else { 4 };
    let visible = (inner.height as usize / per_slot)
        .max(1)
        .min(st.slots.len());
    for (index, slot) in st.slots.iter().take(visible).enumerate() {
        let height = per_slot.min(inner.height.saturating_sub((index * per_slot) as u16) as usize);
        if height == 0 {
            break;
        }
        let slot_area = Rect {
            x: inner.x,
            y: inner.y + (index * per_slot) as u16,
            width: inner.width,
            height: height as u16,
        };
        draw_slot(f, slot_area, slot, p);
    }

    let hidden = st.slots.len().saturating_sub(visible);
    if hidden > 0 && inner.height > (visible * per_slot) as u16 {
        f.render_widget(
            Paragraph::new(Span::styled(
                format!("+ {hidden} more slots"),
                Style::new().fg(p.dim),
            )),
            Rect {
                x: inner.x,
                y: inner.y + (visible * per_slot) as u16,
                width: inner.width,
                height: 1,
            },
        );
    }
}

fn draw_slot(f: &mut Frame, area: Rect, slot: &SlotInfo, p: &Palette) {
    if area.height < 3 {
        return;
    }
    let state = if slot.processing { "BUSY" } else { "IDLE" };
    let state_color = if slot.processing { p.amber } else { p.green };
    let ratio = if slot.n_ctx > 0 {
        slot.prompt_tokens as f64 / slot.n_ctx as f64
    } else {
        0.0
    }
    .clamp(0.0, 1.0);
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .split(area);

    let mut heading = vec![
        Span::styled(
            format!("#{:02}", slot.id),
            Style::new().fg(p.heading).add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ● ", Style::new().fg(state_color)),
        Span::styled(
            state,
            Style::new().fg(state_color).add_modifier(Modifier::BOLD),
        ),
    ];
    if let Some(temp) = slot.temperature {
        heading.push(Span::styled(
            format!("   T {temp:.2}"),
            Style::new().fg(p.dim),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(heading)), rows[0]);
    f.render_widget(
        Paragraph::new(meter_line(rows[1].width as usize, ratio, p.violet, p)),
        rows[1],
    );
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("ctx ", Style::new().fg(p.dim)),
            Span::styled(fmt_count(slot.prompt_tokens), Style::new().fg(p.text)),
            Span::styled(
                format!(" / {}", fmt_count(slot.n_ctx)),
                Style::new().fg(p.muted),
            ),
            Span::styled(
                format!("   {:.1}%", ratio * 100.0),
                Style::new().fg(p.violet),
            ),
        ])),
        rows[2],
    );
    if area.height >= 4 {
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("cache ", Style::new().fg(p.dim)),
                Span::styled(fmt_count(slot.tokens_cache), Style::new().fg(p.muted)),
                Span::styled("   processed ", Style::new().fg(p.dim)),
                Span::styled(fmt_count(slot.tokens_processed), Style::new().fg(p.muted)),
            ])),
            rows[3],
        );
    }
}

fn meter_line(width: usize, ratio: f64, color: Color, p: &Palette) -> Line<'static> {
    let mut filled = (width as f64 * ratio.clamp(0.0, 1.0)).round() as usize;
    if ratio > 0.0 && width > 0 {
        filled = filled.max(1);
    }
    filled = filled.min(width);
    Line::from(vec![
        Span::styled("━".repeat(filled), Style::new().fg(color)),
        Span::styled("─".repeat(width - filled), Style::new().fg(p.border)),
    ])
}

fn prom_value(st: &State, name: &str) -> Option<f64> {
    st.series
        .get(&format!("prom/{name}"))
        .and_then(|series| series.last())
        .map(|sample| sample.v)
}

fn draw_session(f: &mut Frame, area: Rect, st: &State, p: &Palette) {
    let block =
        panel(title("request activity", p.amber), p.amber, p).title(right_title("read only", p));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let value = |name: &str| {
        prom_value(st, name)
            .map(fmt_num)
            .unwrap_or_else(|| "—".into())
    };
    let busy = st.slots.iter().filter(|slot| slot.processing).count();
    let source = match st.metrics_supported {
        Some(true) => format!("prometheus  ·  {} series", st.prom_discovered),
        Some(false) => "slot counter deltas  ·  metrics unavailable".to_string(),
        None => "discovering metrics endpoint".to_string(),
    };
    let lines = vec![
        Line::from(vec![
            Span::styled("MODE  ", Style::new().fg(p.dim)),
            Span::styled("passive API monitor", Style::new().fg(p.text)),
        ]),
        Line::from(Span::styled(
            format!(
                "{} processing  ·  {} deferred",
                value("llamacpp:requests_processing"),
                value("llamacpp:requests_deferred")
            ),
            Style::new().fg(p.muted),
        )),
        Line::from(Span::styled(
            format!(
                "{} decode batches  ·  {} cached",
                value("llamacpp:n_decode_total"),
                value("llamacpp:prompt_tokens_cached_total")
            ),
            Style::new().fg(p.muted),
        )),
        Line::from(Span::styled(
            format!("{busy} busy  /  {} slots", st.slots.len()),
            Style::new().fg(p.amber),
        )),
        Line::from(Span::styled(
            "no inference requests sent",
            Style::new().fg(p.green),
        )),
        Line::from(Span::styled(source, Style::new().fg(p.dim))),
    ];
    f.render_widget(Paragraph::new(lines), inner);
}

// ---------------------------------------------------------------------------
// charts
// ---------------------------------------------------------------------------

fn draw_charts(f: &mut Frame, area: Rect, st: &State, p: &Palette) {
    let focus = st.focus_model().map(|m| m.name.clone()).unwrap_or_default();
    let show_prom = st.show_metrics && st.metrics_supported == Some(true);
    let secondary_height = if area.height >= 24 {
        8
    } else if area.height >= 16 {
        6
    } else {
        4
    };
    let bottom_height = if area.height >= 24 {
        8
    } else if area.height >= 16 {
        6
    } else {
        4
    };
    let areas = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(secondary_height),
        Constraint::Length(bottom_height),
    ])
    .spacing(if area.height >= 18 { 1 } else { 0 })
    .split(area);

    area_chart(
        f,
        areas[0],
        st.series.get(&format!("gen_tps/{focus}")),
        &format!("generation / {focus}"),
        "tok/s",
        p.cyan,
        p,
    );
    area_chart(
        f,
        areas[1],
        st.series.get(&format!("prompt_tps/{focus}")),
        "prompt throughput",
        "tok/s",
        p.green,
        p,
    );

    if show_prom {
        let bottom = Layout::horizontal([Constraint::Percentage(35), Constraint::Percentage(65)])
            .spacing(1)
            .split(areas[2]);
        area_chart(
            f,
            bottom[0],
            st.series.get(&format!("ctx_pct/{focus}")),
            "context window",
            "%",
            p.violet,
            p,
        );
        draw_prom_table(f, bottom[1], st, p);
    } else {
        area_chart(
            f,
            areas[2],
            st.series.get(&format!("ctx_pct/{focus}")),
            "context window",
            "%",
            p.violet,
            p,
        );
    }
}

fn area_chart(
    f: &mut Frame,
    area: Rect,
    series: Option<&Series>,
    name: &str,
    unit: &str,
    accent: Color,
    p: &Palette,
) {
    if area.is_empty() {
        return;
    }
    let stats = MetricStats::from_series(series);
    // Long headings are useful on the hero chart, but would completely hide
    // the title on a half-width or narrow terminal panel.
    let stats_label = match (stats, area.width) {
        (Some(s), width) if width >= 90 => format!(
            "now {} {} · avg {} · peak {}",
            fmt_num(s.now),
            unit,
            fmt_num(s.avg),
            fmt_num(s.peak)
        ),
        (Some(s), width) if width >= 38 => format!("{} {}", fmt_num(s.now), unit),
        (None, width) if width >= 38 => "waiting for samples".to_string(),
        _ => String::new(),
    };
    let block = panel(title(name, accent), accent, p);
    let block = if stats_label.is_empty() {
        block
    } else {
        block.title(right_title(stats_label, p))
    };
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width < 4 || inner.height < 2 {
        return;
    }

    let axis_width = if inner.width >= 30 { 6 } else { 0 };
    let time_height = if inner.height >= 4 { 1 } else { 0 };
    let graph = Rect {
        x: inner.x + axis_width,
        y: inner.y,
        width: inner.width.saturating_sub(axis_width),
        height: inner.height.saturating_sub(time_height),
    };
    if graph.is_empty() {
        return;
    }

    let values = series.map(Series::values).unwrap_or_default();
    let max = nice_max(stats.map(|s| s.peak).unwrap_or(0.0));
    draw_grid(f, inner, graph, max, axis_width, p);

    if values.is_empty() {
        let message = "○  WAITING FOR SAMPLES";
        let x = graph.x + graph.width.saturating_sub(message.len() as u16) / 2;
        let y = graph.y + graph.height / 2;
        f.buffer_mut().set_stringn(
            x,
            y,
            message,
            graph.width as usize,
            Style::new().fg(p.dim).bg(p.surface),
        );
    } else {
        let data = resample_series(&values, graph.width as usize);
        draw_area(f, graph, &data, max, accent, p);
    }

    if time_height > 0 {
        let y = inner.bottom().saturating_sub(1);
        let style = Style::new().fg(p.dim).bg(p.surface);
        f.buffer_mut()
            .set_stringn(graph.x, y, "older", graph.width as usize, style);
        if graph.width > 16 {
            let sample_label = format!("{} samples", values.len());
            let x = graph.x + graph.width.saturating_sub(sample_label.len() as u16) / 2;
            f.buffer_mut()
                .set_stringn(x, y, sample_label, graph.width as usize, style);
        }
        if graph.width >= 3 {
            f.buffer_mut()
                .set_stringn(graph.right().saturating_sub(3), y, "now", 3, style);
        }
    }
}

fn draw_grid(f: &mut Frame, inner: Rect, graph: Rect, max: f64, axis_width: u16, p: &Palette) {
    let buf = f.buffer_mut();
    for y in graph.y..graph.bottom() {
        for x in graph.x..graph.right() {
            buf[(x, y)]
                .set_symbol("·")
                .set_style(Style::new().fg(p.grid).bg(p.surface));
        }
    }
    if axis_width == 0 {
        return;
    }

    let mut labels = vec![(graph.y, max)];
    if graph.height >= 3 {
        labels.push((graph.y + (graph.height - 1) / 2, max / 2.0));
    }
    labels.push((graph.bottom().saturating_sub(1), 0.0));
    for (y, value) in labels {
        let label = format!("{:>5}", fmt_num(value));
        buf.set_stringn(
            inner.x,
            y,
            label,
            axis_width.saturating_sub(1) as usize,
            Style::new().fg(p.dim).bg(p.surface),
        );
    }
}

fn draw_area(f: &mut Frame, area: Rect, values: &[f64], max: f64, accent: Color, p: &Palette) {
    let buf = f.buffer_mut();
    let height = area.height as usize;
    for (column, value) in values.iter().enumerate() {
        let x = area.x + column as u16;
        if x >= area.right() {
            break;
        }
        let units = (value.max(0.0) / max.max(f64::EPSILON) * (height * 8) as f64)
            .round()
            .clamp(0.0, (height * 8) as f64) as usize;
        let full_rows = units / 8;
        let remainder = units % 8;

        for level in 0..full_rows.min(height) {
            let y = area.bottom() - 1 - level as u16;
            let is_top = level + 1 == full_rows && remainder == 0;
            let strength = 0.24 + 0.58 * (level + 1) as f64 / height as f64;
            let color = if is_top {
                accent
            } else {
                shade(accent, strength, p.fill_base)
            };
            buf[(x, y)]
                .set_symbol("█")
                .set_style(Style::new().fg(color).bg(p.surface));
        }
        if remainder > 0 && full_rows < height {
            let y = area.bottom() - 1 - full_rows as u16;
            buf[(x, y)]
                .set_symbol(PARTIAL_BLOCKS[remainder])
                .set_style(Style::new().fg(accent).bg(p.surface));
        }
    }
}

/// Fit samples to the available cells. Small histories are linearly
/// interpolated (a normal line-chart view); larger histories are averaged into
/// buckets so every sample contributes to the rendered shape.
fn resample_series(values: &[f64], width: usize) -> Vec<f64> {
    if width == 0 || values.is_empty() {
        return Vec::new();
    }
    if values.len() == 1 {
        return vec![values[0].max(0.0); width];
    }
    if values.len() > width {
        return (0..width)
            .map(|column| {
                let start = column * values.len() / width;
                let end = ((column + 1) * values.len() / width)
                    .max(start + 1)
                    .min(values.len());
                values[start..end].iter().sum::<f64>().max(0.0) / (end - start) as f64
            })
            .collect();
    }
    if width == 1 {
        return vec![values[values.len() - 1].max(0.0)];
    }

    (0..width)
        .map(|column| {
            let position = column as f64 * (values.len() - 1) as f64 / (width - 1) as f64;
            let left = position.floor() as usize;
            let right = position.ceil() as usize;
            let fraction = position - left as f64;
            (values[left] + (values[right] - values[left]) * fraction).max(0.0)
        })
        .collect()
}

/// Blend an accent toward the theme's chart-fill base. Dark mode fades toward
/// black as before; light mode fades toward white instead of creating black bars.
fn shade(color: Color, factor: f64, base: Color) -> Color {
    match (color, base) {
        (Color::Rgb(r, g, b), Color::Rgb(br, bg, bb)) => {
            let blend = |channel: u8, base: u8| {
                (base as f64 + (channel as f64 - base as f64) * factor)
                    .round()
                    .clamp(0.0, 255.0) as u8
            };
            Color::Rgb(blend(r, br), blend(g, bg), blend(b, bb))
        }
        (other, _) => other,
    }
}

// ---------------------------------------------------------------------------
// Prometheus grid
// ---------------------------------------------------------------------------

fn draw_prom_table(f: &mut Frame, area: Rect, st: &State, p: &Palette) {
    let mut rows: Vec<(String, f64)> = st
        .series
        .iter()
        .filter(|(key, _)| key.starts_with("prom/"))
        .filter_map(|(key, series)| {
            series
                .last()
                .map(|sample| (key.trim_start_matches("prom/").to_string(), sample.v))
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows.sort_by_key(|(name, _)| !name.starts_with("llamacpp:"));

    let block = panel(title("prometheus", p.cyan), p.cyan, p).title(right_title(
        format!("{} series · m to hide", st.prom_discovered),
        p,
    ));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.is_empty() {
        return;
    }
    if rows.is_empty() {
        f.render_widget(
            Paragraph::new(Span::styled(
                "waiting for prometheus series…",
                Style::new().fg(p.dim),
            )),
            inner,
        );
        return;
    }

    let column_count = if inner.width >= 72 { 2 } else { 1 };
    let visible = rows.len().min(inner.height as usize * column_count);
    rows.truncate(visible);
    let columns = Layout::horizontal(vec![
        Constraint::Ratio(1, column_count as u32);
        column_count
    ])
    .spacing(2)
    .split(inner);
    let rows_per_column = inner.height as usize;

    for (column_index, column) in columns.iter().enumerate() {
        let table_rows: Vec<Row> = rows
            .iter()
            .skip(column_index * rows_per_column)
            .take(rows_per_column)
            .map(|(name, value)| {
                Row::new(vec![
                    Cell::from(name.clone()).style(Style::new().fg(p.muted)),
                    Cell::from(Line::from(Span::styled(
                        fmt_num(*value),
                        Style::new().fg(p.cyan),
                    )))
                    .style(Style::new().fg(p.cyan)),
                ])
            })
            .collect();
        if table_rows.is_empty() {
            continue;
        }
        let value_width = if column.width >= 30 { 11 } else { 8 };
        let table = Table::new(
            table_rows,
            [Constraint::Min(12), Constraint::Length(value_width)],
        )
        .column_spacing(1)
        .style(Style::new().bg(p.surface));
        f.render_widget(table, *column);
    }
}

// ---------------------------------------------------------------------------
// formatting helpers
// ---------------------------------------------------------------------------

/// Tail of a series as scaled u64 values, newest last.
fn spark_data_scaled(series: Option<&Series>, width: usize, scale: f64) -> (Vec<u64>, Option<f64>) {
    let Some(series) = series else {
        return (Vec::new(), None);
    };
    let max = series.max();
    let data: Vec<u64> = series
        .values()
        .into_iter()
        .rev()
        .take(width)
        .rev()
        .map(|value| (value * scale).round().max(0.0) as u64)
        .collect();
    (data, max)
}

/// Compact number formatting for chart titles.
pub fn fmt_num(value: f64) -> String {
    let absolute = value.abs();
    if absolute >= 1_000_000.0 {
        format!("{:.1}M", value / 1_000_000.0)
    } else if absolute >= 10_000.0 {
        format!("{:.1}k", value / 1_000.0)
    } else if absolute >= 1_000.0 {
        format!("{:.0}", value)
    } else if absolute >= 10.0 {
        format!("{:.1}", value)
    } else if value == 0.0 {
        "0".to_string()
    } else {
        format!("{:.2}", value)
    }
}

fn fmt_count(value: u64) -> String {
    if value < 10_000 {
        value.to_string()
    } else {
        fmt_num(value as f64)
    }
}

/// Round an axis maximum up to a 1/2/5×10^k value.
pub fn nice_max(value: f64) -> f64 {
    if value <= 0.0 {
        return 1.0;
    }
    let base = 10f64.powf(value.log10().floor());
    let mantissa = value / base;
    let nice = if mantissa <= 1.0 {
        1.0
    } else if mantissa <= 2.0 {
        2.0
    } else if mantissa <= 5.0 {
        5.0
    } else {
        10.0
    };
    nice * base
}

pub fn fmt_uptime(secs: f64) -> String {
    let seconds = secs.max(0.0) as u64;
    let (hours, minutes, seconds) = (seconds / 3600, (seconds % 3600) / 60, seconds % 60);
    if hours > 0 {
        format!("{hours}h{minutes:02}m{seconds:02}s")
    } else {
        format!("{minutes}m{seconds:02}s")
    }
}

fn fmt_window(secs: f64) -> String {
    if secs >= 3600.0 {
        format!("{:.1}h", secs / 3600.0)
    } else if secs >= 60.0 {
        format!("{:.0}m", secs / 60.0)
    } else {
        format!("{secs:.0}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llamacpp::collect::{ModelInfo, SlotInfo, State};
    use ratatui::{backend::TestBackend, Terminal};

    fn sample_state() -> State {
        let mut st = State::new("http://localhost:2464", 300.0, 2.0);
        st.up = true;
        st.role = Some("router".to_string());
        st.build = Some("b11609-test".to_string());
        st.models = vec![
            ModelInfo {
                name: "bonsai27".into(),
                state: "loaded".into(),
                ctx: Some(65536),
                hf_repo: Some("prism-ml/Bonsai-27B-gguf:Q1_0".into()),
                main_gpu: None,
                gpu_layers: None,
                spec_type: None,
            },
            ModelInfo {
                name: "lfm25".into(),
                state: "unloaded".into(),
                ctx: Some(32768),
                hf_repo: None,
                main_gpu: None,
                gpu_layers: None,
                spec_type: None,
            },
        ];
        st.slots = vec![SlotInfo {
            id: 0,
            task_id: Some(1),
            processing: true,
            n_ctx: 65536,
            prompt_tokens: 11,
            tokens_processed: 0,
            decoded_tokens: 0,
            tokens_cache: 7,
            temperature: Some(0.2),
        }];
        for i in 0..10i64 {
            let t = 1000.0 + i as f64;
            st.push("gen_tps/bonsai27", t, 100.0 + i as f64);
            st.push("prompt_tps/bonsai27", t, 6.9);
            st.push("busy/bonsai27", t, 1.0);
            st.push("cache_tokens/bonsai27", t, 7.0);
            st.push("ctx_pct/bonsai27", t, 11.0 / 65536.0 * 100.0);
        }
        st.push("prom/llamacpp:requests_processing", 1000.0, 1.0);
        st.push("prom/llamacpp:requests_deferred", 1000.0, 0.0);
        st.push("prom/llamacpp:n_decode_total", 1000.0, 4551.0);
        st
    }

    fn render_at(st: &State, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, st, Theme::Dark)).unwrap();
        let buf = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    fn render(st: &State) -> String {
        render_at(st, 120, 36)
    }

    #[test]
    fn draws_all_panels() {
        let screen = render(&sample_state());
        assert!(screen.contains("LLAMA.CPP"), "{screen}");
        assert!(screen.contains("http://localhost:2464"), "{screen}");
        assert!(screen.contains("router"), "{screen}");
        assert!(screen.contains("2 models, 1 loaded"), "{screen}");
        assert!(screen.contains("active model"), "{screen}");
        assert!(screen.contains("bonsai27"), "{screen}");
        assert!(screen.contains("slot activity"), "{screen}");
        assert!(screen.contains("1/1 busy"), "{screen}");
        assert!(screen.contains("GENERATION"), "{screen}");
        assert!(screen.contains("PROMPT"), "{screen}");
        assert!(screen.contains("KV CACHE"), "{screen}");
        assert!(screen.contains("CONTEXT"), "{screen}");
        assert!(screen.contains("generation / bonsai27"), "{screen}");
        assert!(screen.contains("context window"), "{screen}");
        assert!(screen.contains("request activity"), "{screen}");
        assert!(screen.contains("no inference requests sent"), "{screen}");
        assert!(screen.contains("t light"), "{screen}");
        assert!(screen.contains("q quit"), "{screen}");
        assert!(screen.contains("passive"), "{screen}");
    }

    #[test]
    fn light_theme_uses_light_surfaces_and_dark_text() {
        let backend = TestBackend::new(120, 36);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| draw(f, &sample_state(), Theme::Light))
            .unwrap();
        let buf = terminal.backend().buffer();

        assert_eq!(buf[(0, 0)].bg, LIGHT_PALETTE.surface);
        assert_eq!(buf[(119, 35)].bg, LIGHT_PALETTE.bg);
        assert_eq!(buf[(4, 1)].fg, LIGHT_PALETTE.heading);
        assert_eq!(buf[(2, 1)].fg, LIGHT_PALETTE.cyan);
    }

    #[test]
    fn theme_toggles_between_dark_and_light() {
        assert_eq!(Theme::Dark.toggle(), Theme::Light);
        assert_eq!(Theme::Light.toggle(), Theme::Dark);
    }

    #[test]
    fn light_chart_fills_blend_toward_white() {
        assert_eq!(
            shade(Color::Rgb(100, 150, 200), 0.5, LIGHT_PALETTE.fill_base),
            Color::Rgb(178, 203, 228)
        );
    }

    #[test]
    fn down_server_shows_error() {
        let mut st = State::new("http://nowhere:1", 300.0, 2.0);
        st.last_error = Some("health: connection refused".to_string());
        let screen = render(&st);
        assert!(screen.contains("DOWN"), "{screen}");
        assert!(
            screen.contains("err: health: connection refused"),
            "{screen}"
        );
    }

    #[test]
    fn metrics_panel_toggles_off() {
        let mut st = sample_state();
        st.metrics_supported = Some(true);
        st.prom_discovered = 2;
        st.push("prom/llamacpp:prompt_tokens_total", 1000.0, 1234.0);
        st.push("prom/llamacpp:requests_processing", 1000.0, 1.0);
        let with = render(&st);
        assert!(with.contains("prometheus"), "{with}");
        assert!(with.contains("llamacpp:prompt_tokens_total"), "{with}");

        st.show_metrics = false;
        let without = render(&st);
        assert!(!without.contains("m to hide"), "{without}");
        assert!(
            !without.contains("llamacpp:prompt_tokens_total"),
            "{without}"
        );
    }

    #[test]
    fn compact_layout_still_renders_charts() {
        let screen = render_at(&sample_state(), 72, 28);
        assert!(screen.contains("active model"), "{screen}");
        assert!(screen.contains("generation / bonsai27"), "{screen}");
        assert!(screen.contains("prompt throughput"), "{screen}");
        assert!(!screen.contains("busy slots"), "{screen}");
    }

    #[test]
    fn tiny_layout_does_not_panic() {
        let screen = render_at(&sample_state(), 44, 14);
        assert!(screen.contains("RUSTMETRICS"), "{screen}");
        assert!(
            screen.contains("DOWN") || screen.contains("LIVE"),
            "{screen}"
        );
    }

    #[test]
    fn nice_max_rounds_to_powers() {
        assert_eq!(nice_max(0.0), 1.0);
        assert_eq!(nice_max(1.2), 2.0);
        assert_eq!(nice_max(4.7), 5.0);
        assert_eq!(nice_max(8.3), 10.0);
        assert_eq!(nice_max(104.1), 200.0);
        assert_eq!(nice_max(2500.0), 5000.0);
    }

    #[test]
    fn fmt_num_variants() {
        assert_eq!(fmt_num(0.0), "0");
        assert_eq!(fmt_num(6.91), "6.91");
        assert_eq!(fmt_num(103.888), "103.9");
        assert_eq!(fmt_num(1234.0), "1234");
        assert_eq!(fmt_num(15_234.0), "15.2k");
        assert_eq!(fmt_num(3_400_000.0), "3.4M");
    }

    #[test]
    fn fmt_uptime_variants() {
        assert_eq!(fmt_uptime(5.0), "0m05s");
        assert_eq!(fmt_uptime(83.0), "1m23s");
        assert_eq!(fmt_uptime(3723.0), "1h02m03s");
    }

    #[test]
    fn spark_data_tails_and_clamps() {
        let mut series = Series::new(10, 60.0);
        for i in 0..8 {
            series.push(i as f64, i as f64 * 10.0);
        }
        let (data, max) = spark_data_scaled(Some(&series), 4, 1.0);
        assert_eq!(data, vec![40, 50, 60, 70]);
        assert_eq!(max, Some(70.0));
        let (empty, none) = spark_data_scaled(None, 4, 1.0);
        assert!(empty.is_empty());
        assert_eq!(none, None);
    }

    #[test]
    fn resamples_up_and_down() {
        assert_eq!(
            resample_series(&[0.0, 10.0], 5),
            vec![0.0, 2.5, 5.0, 7.5, 10.0]
        );
        assert_eq!(resample_series(&[42.0], 3), vec![42.0, 42.0, 42.0]);
        assert_eq!(
            resample_series(&[0.0, 10.0, 20.0, 30.0], 2),
            vec![5.0, 25.0]
        );
        assert!(resample_series(&[1.0], 0).is_empty());
    }

    #[test]
    fn trend_uses_a_small_tolerance() {
        assert_eq!(Trend::between(Some(100.0), 110.0), Trend::Up);
        assert_eq!(Trend::between(Some(100.0), 90.0), Trend::Down);
        assert_eq!(Trend::between(Some(100.0), 100.5), Trend::Steady);
        assert_eq!(Trend::between(None, 100.0), Trend::Unknown);
    }
}
