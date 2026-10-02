use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::app::{AppState, RowCtx};
use crate::column::{Align, EmphasisStyle, Tone};
use crate::model::{InstanceState, InstanceType, SortDirection, UiColor};

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
    pub host_rendering: HostRendering,
    pub filter: String,
    pub is_filtering: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HostRendering {
    /// Forced on by the user.
    Shown,
    /// Hidden because every instance shares one host.
    OmittedAuto,
    /// Shown because instances span several hosts.
    ShownAuto,
}

impl HostRendering {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Shown => "shown",
            Self::OmittedAuto => "omitted(auto)",
            Self::ShownAuto => "shown(auto)",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OverviewSort {
    pub key: String,
    pub label: String,
    pub direction: SortDirection,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OverviewColumn {
    pub key: String,
    pub label: String,
    pub align: Align,
    pub emphasis_style: Option<EmphasisStyle>,
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
    pub color: UiColor,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OverviewCell {
    pub column_key: String,
    pub value: String,
    pub emphasized: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tone: Option<Tone>,
}

impl AppState {
    pub fn build_overview_frame(&mut self) -> OverviewFrame {
        let rows = self.visible_rows();
        let row_ctx = self.row_ctx();
        let emphasized = self.take_emphasized_rows_by_column(&rows, &row_ctx);
        let columns: Vec<_> = self
            .visible_column_keys()
            .into_iter()
            .filter_map(|key| Some((self.column_registry.column(&key)?.clone(), key)))
            .collect();
        let host_rendering = if self.force_show_host {
            HostRendering::Shown
        } else if self.should_omit_host_in_rendering() {
            HostRendering::OmittedAuto
        } else {
            HostRendering::ShownAuto
        };

        let overview_columns = columns
            .iter()
            .map(|(column, key)| OverviewColumn {
                key: key.clone(),
                label: sortable_header(column.header(), &self.sort_by, self.sort_direction, key),
                align: column.align(),
                emphasis_style: column.emphasis().map(|_| {
                    column
                        .emphasis_style()
                        .unwrap_or_else(|| self.column_registry.overview_emphasis_style())
                }),
            })
            .collect();

        let rows = rows
            .iter()
            .enumerate()
            .filter_map(|(idx, row)| {
                let node = self.instances.get(&row.key)?;
                let ctx = row_ctx.cell(node, &row.tree_prefix);
                Some(OverviewRow {
                    key: row.key.clone(),
                    tree_prefix: row.tree_prefix.clone(),
                    stale: row.stale,
                    selected: idx == self.selected_index,
                    cluster_gutter: overview_cluster_gutter(self, node, &row_ctx),
                    cells: columns
                        .iter()
                        .map(|(column, key)| OverviewCell {
                            column_key: key.clone(),
                            value: column.render_cell(&ctx),
                            emphasized: emphasized.get(key) == Some(&row.key),
                            tone: column.tone(&ctx),
                        })
                        .collect(),
                })
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
                    direction: self.sort_direction,
                },
                host_rendering,
                filter: self.filter.clone(),
                is_filtering: self.is_filtering,
            },
            columns: overview_columns,
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

    let mut widths: Vec<usize> = frame
        .columns
        .iter()
        .map(|column| plain_text_width(&column.label))
        .collect();
    for row in &frame.rows {
        for (width, cell) in widths.iter_mut().zip(&row.cells) {
            *width = (*width).max(plain_text_width(&cell.value));
        }
    }

    let render_line = |cells: &mut dyn Iterator<Item = &str>| {
        cells
            .zip(&frame.columns)
            .zip(&widths)
            .map(|((text, column), width)| fit_cell_text(text, *width, column.align))
            .collect::<Vec<_>>()
            .join(" ")
    };

    let header = render_line(&mut frame.columns.iter().map(|column| column.label.as_str()));
    let separator = widths
        .iter()
        .map(|width| "-".repeat(*width))
        .collect::<Vec<_>>()
        .join(" ");
    let body = frame
        .rows
        .iter()
        .map(|row| render_line(&mut row.cells.iter().map(|cell| cell.value.as_str())))
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
    let truncated: String = text.chars().take(width).collect();
    let pad = width - truncated.chars().count();
    let (left, right) = match align {
        Align::Left => (0, pad),
        Align::Right => (pad, 0),
        Align::Center => (pad / 2, pad - pad / 2),
    };
    format!("{:left$}{truncated}{:right$}", "", "")
}

pub fn plain_text_width(text: &str) -> usize {
    text.chars().count()
}

fn overview_cluster_gutter(
    app: &AppState,
    instance: &InstanceState,
    row_ctx: &RowCtx,
) -> Option<OverviewClusterGutter> {
    let token = instance.cluster_id.as_deref().map_or_else(
        || replication_group_token(app, instance),
        |raw_cluster| row_ctx.cluster_labels.get(raw_cluster).cloned(),
    )?;

    Some(OverviewClusterGutter {
        color: cluster_color_for_token(&token),
        token,
    })
}

fn replication_group_token(app: &AppState, instance: &InstanceState) -> Option<String> {
    match instance.kind {
        InstanceType::Primary => app
            .instances
            .values()
            .any(|candidate| candidate.parent_addr.as_deref() == Some(instance.addr.as_str()))
            .then(|| instance.addr.clone()),
        InstanceType::Replica => instance
            .parent_addr
            .as_deref()
            .map(|parent| resolve_replication_group_addr(app, parent)),
        InstanceType::Standalone | InstanceType::Cluster => None,
    }
}

fn resolve_replication_group_addr(app: &AppState, parent: &str) -> String {
    app.instances
        .get(parent)
        .or_else(|| {
            app.instances
                .values()
                .find(|candidate| candidate.addr == parent)
        })
        .map_or_else(|| parent.to_string(), |candidate| candidate.addr.clone())
}

pub fn cluster_color_for_token(token: &str) -> UiColor {
    const PALETTE: [UiColor; 7] = [
        UiColor::Cyan,
        UiColor::Yellow,
        UiColor::Green,
        UiColor::Magenta,
        UiColor::Blue,
        UiColor::Red,
        UiColor::Gray,
    ];

    let index = token.bytes().fold(0usize, |acc, byte| {
        acc.wrapping_mul(33).wrapping_add(usize::from(byte))
    });
    PALETTE[index % PALETTE.len()]
}

#[cfg(test)]
mod tests {
    use super::render_plain_text;
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
            Some(crate::model::UiColor::Yellow)
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
