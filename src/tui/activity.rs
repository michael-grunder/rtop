use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Sparkline};

use crate::activity::{ActivityMetric, graph_value};
use crate::app::AppState;
use crate::column::{format_bytes, u64_to_f64};

pub(super) fn draw(frame: &mut ratatui::Frame<'_>, app: &AppState, area: Rect) {
    let totals = &app.activity.current;
    let scope = if app.selected_server_count() == 0 {
        "all"
    } else {
        "selected"
    };
    let partial = if totals.partial {
        " | partial data"
    } else {
        ""
    };
    let title = format!(
        "Activity | {scope} {} | {}/{} live{partial}",
        totals.servers, totals.available, totals.servers
    );
    let block = super::bordered(app, title).title_bottom(Line::from(
        " Net graph = in+out | right = full scale (retained peaks) ",
    ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let rows =
        Layout::vertical(vec![Constraint::Length(1); usize::from(inner.height)]).split(inner);
    let Some(summary) = rows.first() else {
        return;
    };
    let cpu = totals
        .cpu
        .map_or_else(|| "-".into(), |cpu| format!("{cpu:.1}%"));
    let summary_text = if rows.len() >= 4 {
        format!(
            "Memory {}  Clients {}  Ops {}/s",
            bytes(totals.memory),
            super::format_optional_u64(totals.clients),
            super::format_optional_u64(totals.ops),
        )
    } else {
        format!(
            "Memory {}  Clients {}  CPU {cpu}  Net {}/s",
            bytes(totals.memory),
            super::format_optional_u64(totals.clients),
            bytes(
                totals
                    .input
                    .zip(totals.output)
                    .map(|(a, b)| a.saturating_add(b))
            ),
        )
    };
    frame.render_widget(Paragraph::new(summary_text), *summary);
    if rows.len() >= 4 {
        graph(
            frame,
            app,
            rows[1],
            format!("CPU {cpu} (100% = 1 core)"),
            ActivityMetric::Cpu,
            Color::Green,
        );
        graph(
            frame,
            app,
            rows[2],
            format!("Ops {}/s", super::format_optional_u64(totals.ops)),
            ActivityMetric::Ops,
            Color::Cyan,
        );
        graph(
            frame,
            app,
            rows[3],
            format!("Net ↓{}/s ↑{}/s", bytes(totals.input), bytes(totals.output)),
            ActivityMetric::Network,
            Color::Magenta,
        );
    } else if let Some(row) = rows.get(1) {
        graph(
            frame,
            app,
            *row,
            format!("Ops {}/s", super::format_optional_u64(totals.ops)),
            ActivityMetric::Ops,
            Color::Cyan,
        );
    }
}

fn bytes(value: Option<u64>) -> String {
    value.map_or_else(|| "-".into(), format_bytes)
}

fn scale_label(app: &AppState, metric: ActivityMetric) -> String {
    let scale = app.activity.scale(metric);
    format!(
        " {} max",
        match metric {
            ActivityMetric::Cpu => format!("{:.0}%", u64_to_f64(scale) / 100.0),
            ActivityMetric::Ops => format!("{}/s", super::format_with_commas(scale)),
            ActivityMetric::Network => format!("{}/s", format_bytes(scale)),
        }
    )
}

fn graph(
    frame: &mut ratatui::Frame<'_>,
    app: &AppState,
    area: Rect,
    label: String,
    metric: ActivityMetric,
    color: Color,
) {
    let scale = app.activity.scale(metric);
    // Equal label widths keep samples from the same tick aligned across all rows.
    let scale_width = ActivityMetric::ALL
        .into_iter()
        .map(|metric| Line::from(scale_label(app, metric)).width())
        .max()
        .unwrap_or(0);
    let scale_width = u16::try_from(scale_width).unwrap_or(u16::MAX);
    let [label_area, chart, scale_area] = Layout::horizontal([
        Constraint::Length(38.min(area.width / 2)),
        Constraint::Min(0),
        Constraint::Length(scale_width.min(area.width / 3)),
    ])
    .areas(area);
    frame.render_widget(Paragraph::new(label), label_area);
    frame.render_widget(
        Paragraph::new(Line::from(scale_label(app, metric)).right_aligned()),
        scale_area,
    );
    let width = usize::from(chart.width);
    let skip = app.activity.history.len().saturating_sub(width);
    let values: Vec<_> = app
        .activity
        .history
        .iter()
        .skip(skip)
        .map(|totals| metric.value(totals))
        .collect();
    // Normalize before Sparkline multiplies values by bar height, avoiding overflow.
    let values = std::iter::repeat_n(None, width.saturating_sub(values.len())).chain(
        values.into_iter().map(|value| {
            value.map(|value| graph_value(u64_to_f64(value) / u64_to_f64(scale) * 1000.0))
        }),
    );
    frame.render_widget(
        Sparkline::default()
            .data(values)
            .max(1000)
            .style(Style::default().fg(color)),
        chart,
    );
}
