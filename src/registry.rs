use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Result, bail};
use indexmap::IndexMap;
use serde::Deserialize;

use crate::column::{Align, Column, Emphasis, EmphasisStyle, FormatSpec, ValueType, WidthHint};
use crate::columns::calc::{CalcColumn, CalcKind};
use crate::columns::info::RedisInfoFieldColumn;
use crate::config;
use crate::model::{SortDirection, SortMode, UiColor};

const DEFAULT_COLUMNS_TOML: &str = include_str!("default_columns.toml");

pub struct ColumnRegistry {
    column_defs: HashMap<String, ColumnDef>,
    columns: HashMap<String, Arc<dyn Column>>,
    column_order: Vec<String>,
    pub visible_overview: Vec<String>,
    pub default_sort_by: String,
    pub default_sort_direction: SortDirection,
    pub overview_emphasis_style: EmphasisStyle,
}

impl ColumnRegistry {
    pub fn load(config_path: Option<&Path>, no_default_config: bool, cli_sort: SortMode) -> Self {
        let mut registry = Self {
            column_defs: HashMap::new(),
            columns: HashMap::new(),
            column_order: Vec::new(),
            visible_overview: Vec::new(),
            default_sort_by: legacy_sort_key(cli_sort).to_string(),
            default_sort_direction: legacy_sort_direction(cli_sort),
            overview_emphasis_style: EmphasisStyle::default_overview(),
        };

        if let Err(err) = registry.apply_layer(DEFAULT_COLUMNS_TOML) {
            eprintln!("warning: failed to parse built-in column config: {err}");
        }

        let user_config = config::resolve_config_path(config_path, no_default_config);
        if let Some(path) = user_config {
            match fs::read_to_string(&path) {
                Ok(content) => {
                    if let Err(err) = registry.apply_layer(&content) {
                        eprintln!(
                            "warning: failed to parse column definitions from {}: {err}",
                            path.display()
                        );
                    }
                }
                Err(err) => {
                    eprintln!(
                        "warning: failed to read config for column overrides {}: {err}",
                        path.display()
                    );
                }
            }
        }

        if registry.visible_overview.is_empty() {
            registry.visible_overview = default_visible_columns();
        }

        registry
            .visible_overview
            .retain(|key| registry.columns.contains_key(key));
        if registry.visible_overview.is_empty() {
            registry.visible_overview.clone_from(&registry.column_order);
        }

        if !registry.columns.contains_key(&registry.default_sort_by) {
            registry.default_sort_by = registry
                .visible_overview
                .first()
                .cloned()
                .unwrap_or_else(|| "alias".to_string());
        }

        registry
    }

    pub fn column(&self, key: &str) -> Option<&Arc<dyn Column>> {
        self.columns.get(key)
    }

    pub fn visible_columns(&self, show_address: bool) -> Vec<String> {
        self.visible_overview
            .iter()
            .filter(|key| show_address || key.as_str() != "addr")
            .filter(|key| self.columns.contains_key(key.as_str()))
            .cloned()
            .collect()
    }

    pub fn available_overview_columns(&self) -> Vec<String> {
        self.column_order
            .iter()
            .filter(|key| self.columns.contains_key(key.as_str()))
            .cloned()
            .collect()
    }

    pub fn default_visible_overview_columns(&self) -> Vec<String> {
        self.visible_overview.clone()
    }

    pub const fn overview_emphasis_style(&self) -> EmphasisStyle {
        self.overview_emphasis_style
    }

    fn apply_layer(&mut self, input: &str) -> Result<()> {
        let config: ColumnFileConfig = toml::from_str(input)?;

        if let Some(columns) = config.columns {
            for (key, def) in columns {
                if let Some(existing) = self.column_defs.get_mut(&key) {
                    existing.merge(def);
                } else {
                    self.column_defs.insert(key.clone(), def);
                    self.column_order.push(key);
                }
            }
        }

        if let Some(view) = config.view
            && let Some(overview) = view.overview
        {
            if let Some(visible) = overview.visible {
                self.visible_overview = visible;
            }
            if let Some(sort) = overview.sort {
                if let Some(by) = sort.by {
                    self.default_sort_by = by;
                }
                if let Some(dir) = sort.dir {
                    self.default_sort_direction = dir;
                }
            }
            if let Some(style) = overview.emphasis_style {
                self.overview_emphasis_style =
                    resolve_emphasis_style(self.overview_emphasis_style, &style)?;
            }
        }

        self.rebuild_columns()?;

        Ok(())
    }

    fn rebuild_columns(&mut self) -> Result<()> {
        let mut columns = HashMap::new();
        for key in &self.column_order {
            let Some(def) = self.column_defs.get(key) else {
                continue;
            };
            let column = build_column(key, def, self.overview_emphasis_style)?;
            let old = columns.insert(key.clone(), Arc::from(column));
            debug_assert!(old.is_none());
        }
        self.columns = columns;
        Ok(())
    }
}

#[derive(Debug, Deserialize, Default)]
struct ColumnFileConfig {
    columns: Option<IndexMap<String, ColumnDef>>,
    view: Option<ViewConfig>,
}

#[derive(Debug, Deserialize, Default)]
struct ViewConfig {
    overview: Option<OverviewConfig>,
}

#[derive(Debug, Deserialize, Default)]
struct OverviewConfig {
    visible: Option<Vec<String>>,
    sort: Option<SortConfig>,
    emphasis_style: Option<EmphasisStyleDef>,
}

#[derive(Debug, Deserialize, Default)]
struct SortConfig {
    by: Option<String>,
    dir: Option<SortDirection>,
}

#[derive(Debug, Deserialize, Clone, Default)]
struct ColumnDef {
    #[serde(rename = "type")]
    type_name: Option<String>,
    header: Option<String>,
    align: Option<String>,
    emphasis: Option<String>,
    emphasis_style: Option<EmphasisStyleDef>,
    min_width: Option<u16>,
    ideal_width: Option<u16>,
    max_width: Option<u16>,
    fixed_width: Option<u16>,

    info_key: Option<String>,
    value_type: Option<String>,
    format: Option<String>,
    missing: Option<String>,

    calc: Option<String>,
}

/// Layered config merging: every `Some` field in `incoming` replaces ours.
macro_rules! overwrite_present_fields {
    ($target:expr, $incoming:expr, $($field:ident),+ $(,)?) => {
        $(
            if $incoming.$field.is_some() {
                $target.$field = $incoming.$field;
            }
        )+
    };
}

impl ColumnDef {
    fn merge(&mut self, incoming: Self) {
        merge_option(
            &mut self.emphasis_style,
            incoming.emphasis_style,
            EmphasisStyleDef::merge,
        );
        overwrite_present_fields!(
            self,
            incoming,
            type_name,
            header,
            align,
            emphasis,
            min_width,
            ideal_width,
            max_width,
            fixed_width,
            info_key,
            value_type,
            format,
            missing,
            calc,
        );
    }
}

#[derive(Debug, Deserialize, Clone, Default)]
struct EmphasisStyleDef {
    bold: Option<bool>,
    italic: Option<bool>,
    underlined: Option<bool>,
    dim: Option<bool>,
    reversed: Option<bool>,
    foreground_color: Option<String>,
}

impl EmphasisStyleDef {
    fn merge(&mut self, incoming: Self) {
        overwrite_present_fields!(
            self,
            incoming,
            bold,
            italic,
            underlined,
            dim,
            reversed,
            foreground_color,
        );
    }
}

fn build_column(
    key: &str,
    def: &ColumnDef,
    base_emphasis_style: EmphasisStyle,
) -> Result<Box<dyn Column>> {
    let width_hint = WidthHint {
        min: def.min_width.unwrap_or(4),
        ideal: def.ideal_width.unwrap_or(8),
        max: def.max_width,
        fixed: def.fixed_width,
    };
    let header = def.header.clone().unwrap_or_else(|| key.to_string());
    let missing = def.missing.clone().unwrap_or_default();
    let emphasis = def.emphasis.as_deref().map(parse_emphasis).transpose()?;
    let emphasis_style = def
        .emphasis_style
        .as_ref()
        .map(|style| resolve_emphasis_style(base_emphasis_style, style))
        .transpose()?;
    let format = parse_format(def.format.as_deref().unwrap_or("raw"))?;
    let align = def.align.as_deref().map(parse_align).transpose()?;

    match def
        .type_name
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("column {key} missing type"))?
    {
        "info" => {
            let info_key = def
                .info_key
                .clone()
                .ok_or_else(|| anyhow::anyhow!("info column {key} missing info_key"))?;
            let value_type = parse_value_type(def.value_type.as_deref().unwrap_or("string"))?;
            let align = align.unwrap_or_else(|| default_align_for_value_type(value_type));

            Ok(Box::new(RedisInfoFieldColumn {
                header,
                info_key,
                value_type,
                format,
                missing,
                emphasis,
                emphasis_style,
                align,
                width_hint,
            }))
        }
        "calc" => {
            let calc_raw = def
                .calc
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("calc column {key} missing calc"))?;
            let calc_kind = parse_calc_kind(calc_raw)?;
            let align = align.unwrap_or_else(|| default_align_for_calc(&calc_kind));

            Ok(Box::new(CalcColumn {
                header,
                kind: calc_kind,
                format,
                missing,
                emphasis,
                emphasis_style,
                align,
                width_hint,
            }))
        }
        other => bail!("unsupported column type for {key}: {other}"),
    }
}

fn resolve_emphasis_style(base: EmphasisStyle, raw: &EmphasisStyleDef) -> Result<EmphasisStyle> {
    Ok(EmphasisStyle {
        bold: raw.bold.unwrap_or(base.bold),
        italic: raw.italic.unwrap_or(base.italic),
        underlined: raw.underlined.unwrap_or(base.underlined),
        dim: raw.dim.unwrap_or(base.dim),
        reversed: raw.reversed.unwrap_or(base.reversed),
        foreground: match raw.foreground_color.as_deref() {
            Some(color) => Some(parse_color(color)?),
            None => base.foreground,
        },
    })
}

fn parse_value_type(raw: &str) -> Result<ValueType> {
    match raw {
        "string" => Ok(ValueType::String),
        "u64" => Ok(ValueType::U64),
        "i64" => Ok(ValueType::I64),
        "f64" => Ok(ValueType::F64),
        "bytes" => Ok(ValueType::Bytes),
        "percent" => Ok(ValueType::Percent),
        "bool" => Ok(ValueType::Bool),
        other => bail!("unsupported value_type: {other}"),
    }
}

fn parse_emphasis(raw: &str) -> Result<Emphasis> {
    match raw {
        "max" => Ok(Emphasis::Max),
        "min" => Ok(Emphasis::Min),
        other => bail!("unsupported emphasis mode: {other}"),
    }
}

fn parse_color(raw: &str) -> Result<UiColor> {
    raw.parse()
        .map_err(|err| anyhow::anyhow!("invalid emphasis color: {err}"))
}

fn merge_option<T>(slot: &mut Option<T>, incoming: Option<T>, merge: impl FnOnce(&mut T, T)) {
    match (slot.as_mut(), incoming) {
        (Some(current), Some(value)) => merge(current, value),
        (None, Some(value)) => *slot = Some(value),
        (_, None) => {}
    }
}

fn parse_align(raw: &str) -> Result<Align> {
    match raw {
        "left" => Ok(Align::Left),
        "right" => Ok(Align::Right),
        "center" => Ok(Align::Center),
        other => bail!("unsupported align value: {other}"),
    }
}

fn parse_format(raw: &str) -> Result<FormatSpec> {
    if raw == "raw" {
        return Ok(FormatSpec::Raw);
    }
    if raw == "bytes_human" {
        return Ok(FormatSpec::BytesHuman);
    }
    if let Some((kind, tail)) = raw.split_once(':') {
        let decimals = tail.parse::<u8>()?;
        return match kind {
            "fixed" => Ok(FormatSpec::Fixed(decimals)),
            "pct" => Ok(FormatSpec::Percent(decimals)),
            "ms" => Ok(FormatSpec::Millis(decimals)),
            _ => bail!("unsupported format: {raw}"),
        };
    }
    bail!("unsupported format: {raw}")
}

fn parse_calc_kind(raw: &str) -> Result<CalcKind> {
    match raw {
        "$addr" => Ok(CalcKind::Addr),
        "$alias" => Ok(CalcKind::Alias),
        "$pid" => Ok(CalcKind::ProcessId),
        "$role" => Ok(CalcKind::Role),
        "$cluster" => Ok(CalcKind::Cluster),
        "$status" => Ok(CalcKind::Status),
        "slots_total" => Ok(CalcKind::SlotsTotal),
        "slots" => Ok(CalcKind::SlotRanges),
        "latency_last_ms" => Ok(CalcKind::LatencyLastMs),
        "latency_max_ms" => Ok(CalcKind::LatencyMaxMs),
        "maxmemory_percent" => Ok(CalcKind::MaxmemoryPercent {
            used_key: "used_memory".to_string(),
            max_key: "maxmemory".to_string(),
        }),
        "hitrate_percent" => Ok(CalcKind::HitratePercent {
            hits_key: "keyspace_hits".to_string(),
            misses_key: "keyspace_misses".to_string(),
        }),
        "clients_total" => Ok(CalcKind::ClientsTotal {
            key: "connected_clients".to_string(),
        }),
        "ops_per_sec" => Ok(CalcKind::OpsPerSec {
            key: "instantaneous_ops_per_sec".to_string(),
        }),
        other => bail!("unsupported calc: {other}"),
    }
}

const fn default_align_for_value_type(value_type: ValueType) -> Align {
    match value_type {
        ValueType::String => Align::Left,
        ValueType::Bool => Align::Center,
        ValueType::I64
        | ValueType::U64
        | ValueType::F64
        | ValueType::Bytes
        | ValueType::Percent => Align::Right,
    }
}

const fn default_align_for_calc(kind: &CalcKind) -> Align {
    match kind {
        CalcKind::Addr
        | CalcKind::Alias
        | CalcKind::ProcessId
        | CalcKind::Role
        | CalcKind::Status
        | CalcKind::Cluster
        | CalcKind::SlotRanges => Align::Left,
        _ => Align::Right,
    }
}

fn default_visible_columns() -> Vec<String> {
    [
        "alias", "addr", "role", "used_mem", "ops", "lat_last", "lat_max", "status",
    ]
    .map(str::to_string)
    .to_vec()
}

pub const fn legacy_sort_key(mode: SortMode) -> &'static str {
    match mode {
        SortMode::Alias => "alias",
        SortMode::Address => "addr",
        SortMode::Type => "role",
        SortMode::Cluster => "cluster",
        SortMode::Mem => "used_mem",
        SortMode::Ops => "ops",
        SortMode::Lat => "lat_last",
        SortMode::LatMax => "lat_max",
        SortMode::Status => "status",
    }
}

pub const fn legacy_sort_direction(mode: SortMode) -> SortDirection {
    match mode {
        SortMode::Alias
        | SortMode::Address
        | SortMode::Type
        | SortMode::Cluster
        | SortMode::Status => SortDirection::Asc,
        SortMode::Mem | SortMode::Ops | SortMode::Lat | SortMode::LatMax => SortDirection::Desc,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use std::sync::Arc;

    use super::ColumnRegistry;
    use crate::column::{CellCtx, Column, Emphasis, SortKey};
    use crate::model::{InstanceState, InstanceType, SlotRange, SortMode, UiColor};

    #[test]
    fn distribution_config_loads_settings_and_columns() {
        let content = include_str!("../rtop.dist.toml");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rtop.toml");
        fs::write(&path, content).unwrap();
        let loaded = crate::config::load_config(Some(&path), false).unwrap();
        let settings =
            crate::config::apply_overrides(crate::config::default_settings(), &loaded.overrides);
        assert!(settings.show_activity);
        assert!(
            loaded.targets.is_empty(),
            "example targets should be disabled"
        );
        let mut registry = ColumnRegistry::load(None, true, settings.default_sort);
        registry
            .apply_layer(content)
            .expect("valid example columns and view");
        for key in &registry.visible_overview {
            assert!(
                registry.column(key).is_some(),
                "unknown visible column: {key}"
            );
        }
    }

    #[test]
    fn loads_builtin_columns() {
        let registry = ColumnRegistry::load(None, true, SortMode::Address);
        assert!(registry.column("alias").is_some());
        assert!(registry.column("used_mem").is_some());
        assert!(registry.visible_overview.iter().any(|k| k == "alias"));
        assert_eq!(
            registry.available_overview_columns(),
            vec![
                "alias".to_string(),
                "addr".to_string(),
                "pid".to_string(),
                "role".to_string(),
                "cluster".to_string(),
                "slots_total".to_string(),
                "slots".to_string(),
                "connected_clients".to_string(),
                "master_repl_offset".to_string(),
                "used_mem".to_string(),
                "maxmem_pct".to_string(),
                "ops".to_string(),
                "lat_last".to_string(),
                "lat_max".to_string(),
                "status".to_string(),
            ]
        );
    }

    #[test]
    fn loads_builtin_column_emphasis() {
        let registry = ColumnRegistry::load(None, true, SortMode::Address);
        let lat_last = registry.column("lat_last").expect("lat_last column");
        let lat_max = registry.column("lat_max").expect("lat_max column");

        assert_eq!(lat_last.emphasis(), Some(Emphasis::Max));
        assert_eq!(lat_max.emphasis(), Some(Emphasis::Max));
    }

    #[test]
    fn slot_columns_render_only_for_nodes_serving_slots() {
        let registry = ColumnRegistry::load(None, true, SortMode::Address);
        let total = registry.column("slots_total").expect("slots_total column");
        let ranges = registry.column("slots").expect("slots column");

        let mut primary = InstanceState::new("primary".into(), "10.0.0.1:7000".into());
        primary.kind = InstanceType::Primary;
        primary.slots = vec![
            SlotRange { start: 0, end: 10 },
            SlotRange {
                start: 100,
                end: 100,
            },
        ];
        let replica = InstanceState::new("replica".into(), "10.0.0.2:7001".into());

        let render = |snap: &InstanceState, column: &Arc<dyn Column>| {
            column.render_cell(&CellCtx::new(snap))
        };
        let sort =
            |snap: &InstanceState, column: &Arc<dyn Column>| column.sort_key(&CellCtx::new(snap));

        assert_eq!(render(&primary, total), "12");
        assert_eq!(render(&primary, ranges), "0-10,100");
        assert_eq!(sort(&primary, total), SortKey::U64(12));
        assert_eq!(sort(&primary, ranges), SortKey::U64(0));

        assert_eq!(render(&replica, total), "");
        assert_eq!(render(&replica, ranges), "");
        assert_eq!(sort(&replica, total), SortKey::Null);
        assert_eq!(sort(&replica, ranges), SortKey::Null);
    }

    #[test]
    fn merges_global_and_per_column_emphasis_style_overrides() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r#"
[view.overview.emphasis_style]
italic = true
foreground_color = "yellow"

[columns.ops.emphasis_style]
underlined = true

[columns.lat_max.emphasis_style]
foreground_color = "red"
"#,
        )
        .expect("write config");

        let registry = ColumnRegistry::load(Some(&path), false, SortMode::Address);

        assert!(!registry.overview_emphasis_style().bold);
        assert!(registry.overview_emphasis_style().italic);
        assert_eq!(
            registry.overview_emphasis_style().foreground,
            Some(UiColor::Yellow)
        );

        let ops = registry.column("ops").expect("ops column");
        let lat_max = registry.column("lat_max").expect("lat_max column");

        assert!(!ops.emphasis_style().expect("ops emphasis").bold);
        assert!(ops.emphasis_style().expect("ops emphasis").italic);
        assert!(ops.emphasis_style().expect("ops emphasis").underlined);
        assert_eq!(
            ops.emphasis_style().expect("ops emphasis").foreground,
            Some(UiColor::Yellow)
        );

        assert_eq!(
            lat_max
                .emphasis_style()
                .expect("lat max emphasis")
                .foreground,
            Some(UiColor::Red)
        );
        assert!(lat_max.emphasis_style().expect("lat max emphasis").italic);
    }

    #[test]
    fn appends_new_user_columns_after_builtin_order() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r#"
[columns.foo]
type = "calc"
header = "Foo"
calc = "$alias"

[columns.bar]
type = "calc"
header = "Bar"
calc = "$addr"
"#,
        )
        .expect("write config");

        let registry = ColumnRegistry::load(Some(&path), false, SortMode::Address);
        let columns = registry.available_overview_columns();

        assert_eq!(
            columns.get(columns.len().saturating_sub(2)),
            Some(&"foo".to_string())
        );
        assert_eq!(columns.last(), Some(&"bar".to_string()));
    }
}
