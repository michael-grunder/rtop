use std::cmp::Ordering;

use serde::Serialize;

use crate::model::{InstanceState, UiColor};
use crate::target_addr::strip_host;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Align {
    Left,
    Right,
    Center,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WidthHint {
    pub min: u16,
    pub ideal: u16,
    pub max: Option<u16>,
    pub fixed: Option<u16>,
}

/// A typed cell value. Columns compute one of these per row; it drives sorting
/// and emphasis directly and rendering through the column's [`FormatSpec`].
#[derive(Debug, Clone, PartialEq)]
pub enum SortKey {
    Null,
    Bool(bool),
    I64(i64),
    U64(u64),
    F64(f64),
    Str(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Emphasis {
    Max,
    Min,
}

impl Emphasis {
    /// Whether `candidate` beats the current `best` under this rule.
    pub fn prefers(self, candidate: &SortKey, best: &SortKey) -> bool {
        match self {
            Self::Max => candidate > best,
            Self::Min => candidate < best,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EmphasisLifetime {
    #[default]
    PersistentWinner,
    TransientRecord,
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct EmphasisStyle {
    pub bold: bool,
    pub italic: bool,
    pub underlined: bool,
    pub dim: bool,
    pub reversed: bool,
    #[serde(rename = "foreground_color")]
    pub foreground: Option<UiColor>,
}

impl EmphasisStyle {
    pub const fn default_overview() -> Self {
        Self {
            bold: true,
            italic: false,
            underlined: false,
            dim: false,
            reversed: false,
            foreground: None,
        }
    }
}

/// Severity hint a column can attach to a cell so renderers can color it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Tone {
    Warning,
    Critical,
}

impl SortKey {
    pub const fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    const fn as_f64(&self) -> Option<f64> {
        match self {
            Self::I64(value) => Some(i64_to_f64(*value)),
            Self::U64(value) => Some(u64_to_f64(*value)),
            Self::F64(value) => Some(*value),
            Self::Null | Self::Bool(_) | Self::Str(_) => None,
        }
    }

    const fn variant_rank(&self) -> u8 {
        match self {
            Self::Null => 0,
            Self::Bool(_) => 1,
            Self::I64(_) => 2,
            Self::U64(_) => 3,
            Self::F64(_) => 4,
            Self::Str(_) => 5,
        }
    }
}

impl Eq for SortKey {}

impl PartialOrd for SortKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Total order where missing values sort after every present value.
impl Ord for SortKey {
    fn cmp(&self, other: &Self) -> Ordering {
        use SortKey::{Bool, F64, I64, Null, Str, U64};

        match (self, other) {
            (Null, Null) => Ordering::Equal,
            (Null, _) => Ordering::Greater,
            (_, Null) => Ordering::Less,
            (Bool(a), Bool(b)) => a.cmp(b),
            (I64(a), I64(b)) => a.cmp(b),
            (U64(a), U64(b)) => a.cmp(b),
            (F64(a), F64(b)) => a.total_cmp(b),
            (Str(a), Str(b)) => a.cmp(b),
            (a, b) => a.variant_rank().cmp(&b.variant_rank()),
        }
    }
}

impl From<Option<u64>> for SortKey {
    fn from(value: Option<u64>) -> Self {
        value.map_or(Self::Null, Self::U64)
    }
}

impl From<Option<f64>> for SortKey {
    fn from(value: Option<f64>) -> Self {
        value.map_or(Self::Null, Self::F64)
    }
}

impl From<Option<String>> for SortKey {
    fn from(value: Option<String>) -> Self {
        value.map_or(Self::Null, Self::Str)
    }
}

/// Everything a column may need to compute a cell for one instance.
pub struct CellCtx<'a> {
    pub snap: &'a InstanceState,
    pub omit_host: bool,
    /// Tree branch drawing; empty when sorting or outside tree view.
    pub tree_prefix: &'a str,
    pub cluster_label: Option<&'a str>,
}

impl<'a> CellCtx<'a> {
    pub const fn new(snap: &'a InstanceState) -> Self {
        Self {
            snap,
            omit_host: false,
            tree_prefix: "",
            cluster_label: None,
        }
    }
}

pub trait Column: Send + Sync {
    fn header(&self) -> &str;
    fn align(&self) -> Align;
    fn width_hint(&self) -> WidthHint;
    fn render_cell(&self, ctx: &CellCtx<'_>) -> String;
    fn sort_key(&self, ctx: &CellCtx<'_>) -> SortKey;
    fn tone(&self, _ctx: &CellCtx<'_>) -> Option<Tone> {
        None
    }
    fn emphasis(&self) -> Option<Emphasis> {
        None
    }
    fn emphasis_lifetime(&self) -> EmphasisLifetime {
        EmphasisLifetime::PersistentWinner
    }
    fn emphasis_style(&self) -> Option<EmphasisStyle> {
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueType {
    String,
    I64,
    U64,
    F64,
    Bytes,
    Percent,
    Bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormatSpec {
    Raw,
    BytesHuman,
    Fixed(u8),
    Percent(u8),
    Millis(u8),
}

impl FormatSpec {
    /// Formats a typed value, returning `None` for missing values so callers
    /// can substitute the column's placeholder.
    pub fn apply(&self, value: &SortKey) -> Option<String> {
        let text = match (self, value) {
            (_, SortKey::Null) => return None,
            (_, SortKey::Str(text)) => text.clone(),
            (_, SortKey::Bool(flag)) => flag.to_string(),
            (Self::Raw, SortKey::I64(number)) => number.to_string(),
            (Self::Raw, SortKey::U64(number)) => number.to_string(),
            (Self::Raw, SortKey::F64(number)) => number.to_string(),
            (Self::BytesHuman, SortKey::U64(bytes)) => format_bytes(*bytes),
            (format, number) => {
                let number = number.as_f64()?;
                match format {
                    Self::BytesHuman => format_bytes(nonnegative_f64_to_u64(number)),
                    Self::Fixed(decimals) | Self::Millis(decimals) => {
                        format!("{number:.*}", usize::from(*decimals))
                    }
                    Self::Percent(decimals) => format_percent(number, *decimals),
                    Self::Raw => number.to_string(),
                }
            }
        };
        Some(text)
    }
}

pub fn parse_info_field<T: std::str::FromStr>(snap: &InstanceState, key: &str) -> Option<T> {
    snap.info.get(key)?.parse().ok()
}

pub fn parse_bool(snap: &InstanceState, key: &str) -> Option<bool> {
    let value = snap.info.get(key)?;
    match value.as_str() {
        "1" | "true" | "yes" => Some(true),
        "0" | "false" | "no" => Some(false),
        _ => None,
    }
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = u64_to_f64(bytes);
    let mut idx = 0;
    while value >= 1024.0 && idx + 1 < UNITS.len() {
        value /= 1024.0;
        idx += 1;
    }
    if idx == 0 {
        format!("{bytes} {}", UNITS[idx])
    } else if value.fract() == 0.0 {
        format!("{value:.0} {}", UNITS[idx])
    } else {
        format!("{value:.1} {}", UNITS[idx])
    }
}

pub fn format_percent(value: f64, decimals: u8) -> String {
    format!("{value:.*}%", usize::from(decimals))
}

#[allow(clippy::cast_precision_loss)]
pub const fn u64_to_f64(value: u64) -> f64 {
    value as f64
}

#[allow(clippy::cast_precision_loss)]
pub const fn i64_to_f64(value: i64) -> f64 {
    value as f64
}

#[allow(clippy::cast_precision_loss)]
pub const fn usize_to_f64(value: usize) -> f64 {
    value as f64
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub const fn nonnegative_f64_to_u64(value: f64) -> u64 {
    value.max(0.0) as u64
}

pub fn default_label(addr: &str, omit_host: bool) -> String {
    if omit_host && let Some(without_host) = strip_host(addr) {
        return without_host;
    }
    addr.rsplit('/').next().unwrap_or(addr).to_string()
}
