use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Sparkline};

use crate::activity::{ActivityTotals, graph_value};
use crate::app::AppState;
use crate::column::{Align, format_bytes, u64_to_f64};
use crate::overview::fit_cell_text;

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
        " j/k: focus  Space: select  Esc: clear | history auto-scale ",
    ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let rows =
        Layout::vertical(vec![Constraint::Length(1); usize::from(inner.height)]).split(inner);
    let Some(selector) = rows.first() else {
        return;
    };
    draw_servers(frame, app, *selector);
    let Some(summary) = rows.get(1) else {
        return;
    };
    let cpu = totals
        .cpu
        .map_or_else(|| "-".into(), |cpu| format!("{cpu:.1}%"));
    let summary_text = if rows.len() >= 5 {
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
    if rows.len() >= 5 {
        graph(
            frame,
            app,
            rows[2],
            format!("CPU {cpu} (100% = 1 core)"),
            |total| total.cpu.map(|cpu| graph_value(cpu * 100.0)),
            Color::Green,
        );
        graph(
            frame,
            app,
            rows[3],
            format!("Ops {}/s", super::format_optional_u64(totals.ops)),
            |total| total.ops,
            Color::Cyan,
        );
        graph(
            frame,
            app,
            rows[4],
            format!("Net ↓{}/s ↑{}/s", bytes(totals.input), bytes(totals.output)),
            |total| {
                total
                    .input
                    .zip(total.output)
                    .map(|(input, output)| input.saturating_add(output))
            },
            Color::Magenta,
        );
    } else if let Some(row) = rows.get(2) {
        graph(
            frame,
            app,
            *row,
            format!("Ops {}/s", super::format_optional_u64(totals.ops)),
            |total| total.ops,
            Color::Cyan,
        );
    }
}

fn bytes(value: Option<u64>) -> String {
    value.map_or_else(|| "-".into(), format_bytes)
}

fn draw_servers(frame: &mut ratatui::Frame<'_>, app: &AppState, area: Rect) {
    let rows = app.visible_rows();
    if rows.is_empty() {
        frame.render_widget(Paragraph::new("No visible servers"), area);
        return;
    }
    let count = usize::from((area.width / 24).max(1));
    let width = usize::from(area.width) / count;
    let start = app.selected_index / count * count;
    let mut spans = Vec::new();
    for (index, row) in rows.iter().enumerate().skip(start).take(count) {
        let Some(instance) = app.instances.get(&row.key) else {
            continue;
        };
        let focused = index == app.selected_index;
        let marker = match (focused, app.is_server_selected(&row.key)) {
            (true, true) => "▶",
            (true, false) => ">",
            (false, true) => "●",
            (false, false) => " ",
        };
        let style = if focused {
            Style::default()
                .fg(super::carat_color(app))
                .add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else {
            Style::default()
        };
        spans.push(Span::styled(
            fit_cell_text(
                &format!("{marker} {}", instance.display_name()),
                width,
                Align::Left,
            ),
            style,
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn graph(
    frame: &mut ratatui::Frame<'_>,
    app: &AppState,
    area: Rect,
    label: String,
    metric: fn(&ActivityTotals) -> Option<u64>,
    color: Color,
) {
    let [label_area, chart] = Layout::horizontal([
        Constraint::Length(38.min(area.width / 2)),
        Constraint::Min(0),
    ])
    .areas(area);
    frame.render_widget(Paragraph::new(label), label_area);
    let width = usize::from(chart.width);
    let skip = app.activity.history.len().saturating_sub(width);
    let values: Vec<_> = app.activity.history.iter().skip(skip).map(metric).collect();
    let peak = values.iter().flatten().copied().max().unwrap_or(1).max(1);
    // Normalize before Sparkline multiplies values by bar height, avoiding overflow.
    let values = std::iter::repeat_n(None, width.saturating_sub(values.len())).chain(
        values.into_iter().map(|value| {
            value.map(|value| graph_value(u64_to_f64(value) / u64_to_f64(peak) * 1000.0))
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
