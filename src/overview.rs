use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::app::{AppState, DisplayRow};
use crate::column::{Align, EmphasisStyle};
use crate::model::{SortDirection, UiColor};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OverviewFrame {
    pub timestamp: String,
    pub header: OverviewHeader,
    pub columns: Vec<OverviewColumn>,
    pub rows: Vec<OverviewRow>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OverviewHeader {
    pub refresh_interval_ms: u128,
    pub view_mode: &'static str,
    pub sort: OverviewSort,
    pub host_rendering: &'static str,
    pub filter: String,
    pub is_filtering: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OverviewSort {
    pub key: String,
    pub label: String,
    pub direction: SortDirectionLabel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SortDirectionLabel {
    Asc,
    Desc,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OverviewColumn {
    pub key: String,
    pub label: String,
    pub align: AlignmentLabel,
    pub emphasis_style: Option<OverviewEmphasisStyle>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AlignmentLabel {
    Left,
    Right,
    Center,
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OverviewEmphasisStyle {
    pub bold: bool,
    pub italic: bool,
    pub underlined: bool,
    pub dim: bool,
    pub reversed: bool,
    pub foreground_color: Option<ColorLabel>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ColorLabel {
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    Gray,
    White,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OverviewRow {
    pub key: String,
    pub tree_prefix: String,
    pub stale: bool,
    pub selected: bool,
    pub cluster_gutter: Option<OverviewClusterGutter>,
    pub cells: Vec<OverviewCell>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OverviewClusterGutter {
    pub token: String,
    pub color: ClusterGutterColor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClusterGutterColor {
    Cyan,
    Yellow,
    Green,
    Magenta,
    Blue,
    Red,
    Gray,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OverviewCell {
    pub column_key: String,
    pub value: String,
    pub emphasized: bool,
}

impl AppState {
    pub fn build_overview_frame(&mut self) -> OverviewFrame {
        let rows = self.visible_rows();
        let column_keys = self.visible_column_keys();
        let cluster_labels = self.cluster_labels();
        let emphasized = self.take_emphasized_rows_by_column(&rows);
        let host_rendering = if self.force_show_host {
            "shown"
        } else if self.should_omit_host_in_rendering() {
            "omitted_auto"
        } else {
            "shown_auto"
        };

        let columns = column_keys
            .iter()
            .filter_map(|key| {
                let column = self.column_registry.column(key)?;
                Some(OverviewColumn {
                    key: key.clone(),
                    label: sortable_header(
                        column.header(),
                        &self.sort_by,
                        self.sort_direction,
                        key,
                    ),
                    align: AlignmentLabel::from(column.align()),
                    emphasis_style: column.emphasis().map(|_| {
                        OverviewEmphasisStyle::from(
                            column
                                .emphasis_style()
                                .unwrap_or_else(|| self.column_registry.overview_emphasis_style()),
                        )
                    }),
                })
            })
            .collect::<Vec<_>>();

        let rows = rows
            .iter()
            .enumerate()
            .map(|(idx, row)| OverviewRow {
                key: row.key.clone(),
                tree_prefix: row.tree_prefix.clone(),
                stale: row.stale,
                selected: idx == self.selected_index,
                cluster_gutter: overview_cluster_gutter(self, row, &cluster_labels),
                cells: column_keys
                    .iter()
                    .map(|key| OverviewCell {
                        column_key: key.clone(),
                        value: self.render_cell(row, key).unwrap_or_default(),
                        emphasized: emphasized.get(key).is_some_and(|winner| winner == &row.key),
                    })
                    .collect(),
            })
            .collect();

        OverviewFrame {
            timestamp: format_microtime(SystemTime::now()),
            header: OverviewHeader {
                refresh_interval_ms: self.settings.refresh_interval.as_millis(),
                view_mode: self.view_mode.as_str(),
                sort: OverviewSort {
                    key: self.sort_by.clone(),
                    label: self.sort_label(),
                    direction: SortDirectionLabel::from(self.sort_direction),
                },
                host_rendering,
                filter: self.filter.clone(),
                is_filtering: self.is_filtering,
            },
            columns,
            rows,
        }
    }
}

fn format_microtime(timestamp: SystemTime) -> String {
    timestamp.duration_since(UNIX_EPOCH).map_or_else(
        |_| "0.000000".to_string(),
        |duration| format!("{}.{:06}", duration.as_secs(), duration.subsec_micros()),
    )
}

pub fn render_plain_text(frame: &OverviewFrame) -> String {
    if frame.rows.is_empty() {
        return "No Redis/Valkey instances found.".to_string();
    }

    if frame.columns.is_empty() {
        return "No overview columns are enabled.".to_string();
    }

    let rendered_rows = frame
        .rows
        .iter()
        .map(|row| {
            row.cells
                .iter()
                .map(|cell| cell.value.clone())
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();

    let mut widths = frame
        .columns
        .iter()
        .map(|column| plain_text_width(&column.label))
        .collect::<Vec<_>>();

    for row in &rendered_rows {
        for (idx, cell) in row.iter().enumerate() {
            widths[idx] = widths[idx].max(plain_text_width(cell));
        }
    }

    let header = frame
        .columns
        .iter()
        .enumerate()
        .map(|(idx, column)| fit_cell_text(&column.label, widths[idx], column.align.into()))
        .collect::<Vec<_>>()
        .join(" ");
    let separator = widths
        .iter()
        .map(|width| "-".repeat(*width))
        .collect::<Vec<_>>()
        .join(" ");
    let body = rendered_rows
        .iter()
        .map(|row| {
            row.iter()
                .enumerate()
                .map(|(idx, cell)| {
                    fit_cell_text(cell, widths[idx], frame.columns[idx].align.into())
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n");

    format!("{header}\n{separator}\n{body}")
}

pub fn sortable_header(
    label: &str,
    active_sort_key: &str,
    sort_direction: SortDirection,
    column_key: &str,
) -> String {
    if active_sort_key == column_key {
        format!("{label} {}", sort_direction_symbol(sort_direction))
    } else {
        label.to_string()
    }
}

pub const fn sort_direction_symbol(direction: SortDirection) -> &'static str {
    match direction {
        SortDirection::Asc => "↑",
        SortDirection::Desc => "↓",
    }
}

pub fn fit_cell_text(text: &str, width: usize, align: Align) -> String {
    if width == 0 {
        return String::new();
    }
    let mut chars = text.chars().collect::<Vec<char>>();
    if chars.len() > width {
        chars.truncate(width);
    }
    let truncated: String = chars.into_iter().collect();
    let len = truncated.chars().count();
    if len >= width {
        return truncated;
    }
    let pad = width - len;
    match align {
        Align::Left => format!("{truncated}{:pad$}", "", pad = pad),
        Align::Right => format!("{:pad$}{truncated}", "", pad = pad),
        Align::Center => {
            let left = pad / 2;
            let right = pad - left;
            format!(
                "{:left$}{truncated}{:right$}",
                "",
                "",
                left = left,
                right = right
            )
        }
    }
}

pub fn plain_text_width(text: &str) -> usize {
    text.chars().count()
}

fn overview_cluster_gutter(
    app: &AppState,
    row: &DisplayRow,
    cluster_labels: &std::collections::HashMap<String, String>,
) -> Option<OverviewClusterGutter> {
    let instance = app.instances.get(&row.key)?;
    let token = instance.cluster_id.as_deref().map_or_else(
        || replication_group_token(app, instance),
        |raw_cluster| cluster_labels.get(raw_cluster).cloned(),
    )?;

    Some(OverviewClusterGutter {
        color: cluster_color_for_token(&token),
        token,
    })
}

fn replication_group_token(
    app: &AppState,
    instance: &crate::model::InstanceState,
) -> Option<String> {
    match instance.kind {
        crate::model::InstanceType::Primary => app
            .instances
            .values()
            .any(|candidate| candidate.parent_addr.as_deref() == Some(instance.addr.as_str()))
            .then(|| instance.addr.clone()),
        crate::model::InstanceType::Replica => instance
            .parent_addr
            .as_deref()
            .map(|parent| resolve_replication_group_addr(app, parent)),
        crate::model::InstanceType::Standalone | crate::model::InstanceType::Cluster => None,
    }
}

fn resolve_replication_group_addr(app: &AppState, parent: &str) -> String {
    app.instances
        .values()
        .find(|candidate| candidate.key == parent || candidate.addr == parent)
        .map_or_else(|| parent.to_string(), |candidate| candidate.addr.clone())
}

pub fn cluster_color_for_token(token: &str) -> ClusterGutterColor {
    const PALETTE: [ClusterGutterColor; 7] = [
        ClusterGutterColor::Cyan,
        ClusterGutterColor::Yellow,
        ClusterGutterColor::Green,
        ClusterGutterColor::Magenta,
        ClusterGutterColor::Blue,
        ClusterGutterColor::Red,
        ClusterGutterColor::Gray,
    ];

    let index = token.bytes().fold(0usize, |acc, byte| {
        acc.wrapping_mul(33).wrapping_add(usize::from(byte))
    });
    PALETTE[index % PALETTE.len()]
}

impl From<Align> for AlignmentLabel {
    fn from(value: Align) -> Self {
        match value {
            Align::Left => Self::Left,
            Align::Right => Self::Right,
            Align::Center => Self::Center,
        }
    }
}

impl From<AlignmentLabel> for Align {
    fn from(value: AlignmentLabel) -> Self {
        match value {
            AlignmentLabel::Left => Self::Left,
            AlignmentLabel::Right => Self::Right,
            AlignmentLabel::Center => Self::Center,
        }
    }
}

impl From<SortDirection> for SortDirectionLabel {
    fn from(value: SortDirection) -> Self {
        match value {
            SortDirection::Asc => Self::Asc,
            SortDirection::Desc => Self::Desc,
        }
    }
}

impl From<EmphasisStyle> for OverviewEmphasisStyle {
    fn from(value: EmphasisStyle) -> Self {
        Self {
            bold: value.bold,
            italic: value.italic,
            underlined: value.underlined,
            dim: value.dim,
            reversed: value.reversed,
            foreground_color: value.foreground.map(ColorLabel::from),
        }
    }
}

impl From<UiColor> for ColorLabel {
    fn from(value: UiColor) -> Self {
        match value {
            UiColor::Black => Self::Black,
            UiColor::Red => Self::Red,
            UiColor::Green => Self::Green,
            UiColor::Yellow => Self::Yellow,
            UiColor::Blue => Self::Blue,
            UiColor::Magenta => Self::Magenta,
            UiColor::Cyan => Self::Cyan,
            UiColor::Gray => Self::Gray,
            UiColor::White => Self::White,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ClusterGutterColor, render_plain_text};
    use crate::app::AppState;
    use crate::config::default_settings;
    use crate::model::{InstanceState, Status, ViewMode};
    use crate::registry::ColumnRegistry;

    fn test_registry() -> ColumnRegistry {
        ColumnRegistry::load(None, true, crate::model::SortMode::Address)
    }

    #[test]
    fn overview_frame_includes_selected_row_and_cluster_gutter() {
        let mut app = AppState::new(default_settings(), test_registry());
        app.view_mode = ViewMode::Flat;

        let mut a = InstanceState::new("a".into(), "127.0.0.1:6379".into());
        a.alias = Some("alpha".into());
        a.cluster_id = Some("cluster-b".into());
        a.status = Status::Ok;
        a.last_updated = Some(std::time::Instant::now());

        let mut b = InstanceState::new("b".into(), "127.0.0.1:6380".into());
        b.alias = Some("beta".into());
        b.cluster_id = Some("cluster-a".into());
        b.status = Status::Down;
        b.last_updated = Some(std::time::Instant::now());

        app.apply_update(a);
        app.apply_update(b);
        app.selected_index = 1;

        let frame = app.build_overview_frame();

        assert_eq!(frame.header.view_mode, "flat");
        assert_eq!(frame.rows.len(), 2);
        assert!(frame.rows[1].selected);
        assert_eq!(
            frame.rows[0]
                .cluster_gutter
                .as_ref()
                .map(|gutter| gutter.color),
            Some(ClusterGutterColor::Yellow)
        );
    }

    #[test]
    fn plain_text_renderer_uses_shared_overview_frame() {
        let mut app = AppState::new(default_settings(), test_registry());
        app.view_mode = ViewMode::Flat;

        let mut a = InstanceState::new("a".into(), "127.0.0.1:6379".into());
        a.alias = Some("alpha".into());
        a.status = Status::Ok;
        a.last_updated = Some(std::time::Instant::now());
        app.apply_update(a);

        let rendered = render_plain_text(&app.build_overview_frame());

        assert!(rendered.contains("Alias"));
        assert!(rendered.contains("Status"));
        assert!(rendered.contains("alpha"));
    }

    #[test]
    fn overview_frame_serializes_to_json() {
        let mut app = AppState::new(default_settings(), test_registry());
        app.view_mode = ViewMode::Flat;
        app.filter = "alp".into();
        app.is_filtering = true;

        let mut a = InstanceState::new("a".into(), "127.0.0.1:6379".into());
        a.alias = Some("alpha".into());
        a.status = Status::Ok;
        a.last_updated = Some(std::time::Instant::now());
        app.apply_update(a);

        let json = serde_json::to_value(app.build_overview_frame()).expect("frame serializes");

        assert!(
            json["timestamp"]
                .as_str()
                .expect("timestamp serialized as a string")
                .chars()
                .all(|ch| ch.is_ascii_digit() || ch == '.')
        );
        let parts = json["timestamp"]
            .as_str()
            .expect("timestamp serialized as a string")
            .split('.')
            .collect::<Vec<_>>();
        assert_eq!(parts.len(), 2);
        assert_ne!(parts[0], "");
        assert_eq!(parts[1].len(), 6);
        assert_eq!(json["header"]["view_mode"], "flat");
        assert_eq!(json["header"]["filter"], "alp");
        assert_eq!(json["header"]["is_filtering"], true);
        assert_eq!(json["rows"][0]["cells"][0]["value"], "alpha");
    }

    #[test]
    fn overview_frame_reports_primary_view_mode() {
        let mut app = AppState::new(default_settings(), test_registry());
        app.view_mode = ViewMode::Primary;

        let frame = app.build_overview_frame();

        assert_eq!(frame.header.view_mode, "primary");
    }
}
