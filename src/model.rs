use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt;
use std::time::{Duration, Instant};

use ratatui::style::Color;
use serde::{Deserialize, Serialize};

use crate::hotkeys::HotkeysMetrics;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ViewMode {
    Tree,
    Flat,
    Primary,
}

impl ViewMode {
    pub const fn cycle(self) -> Self {
        match self {
            Self::Tree => Self::Flat,
            Self::Flat => Self::Primary,
            Self::Primary => Self::Tree,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tree => "tree",
            Self::Flat => "flat",
            Self::Primary => "primary",
        }
    }

    pub const fn footer_label(self) -> &'static str {
        match self {
            Self::Tree => "Tree",
            Self::Flat => "Flat",
            Self::Primary => "Primary",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortMode {
    Alias,
    Address,
    Type,
    Cluster,
    Mem,
    Ops,
    Lat,
    LatMax,
    Status,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum SortDirection {
    #[serde(rename = "asc")]
    Asc,
    #[serde(rename = "desc")]
    Desc,
}

impl SortDirection {
    pub const fn toggle(self) -> Self {
        match self {
            Self::Asc => Self::Desc,
            Self::Desc => Self::Asc,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TargetProtocol {
    Tcp,
    Unix,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillAction {
    ShutdownSave,
    ShutdownNosave,
    Sigint,
    Sigterm,
    Sigquit,
    Sigkill,
}

impl KillAction {
    pub const ALL: [Self; 6] = [
        Self::ShutdownSave,
        Self::ShutdownNosave,
        Self::Sigint,
        Self::Sigterm,
        Self::Sigquit,
        Self::Sigkill,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::ShutdownSave => "SHUTDOWN SAVE",
            Self::ShutdownNosave => "SHUTDOWN NOSAVE",
            Self::Sigint => "SIGINT",
            Self::Sigterm => "SIGTERM",
            Self::Sigquit => "SIGQUIT",
            Self::Sigkill => "SIGKILL",
        }
    }

    pub const fn shutdown_arg(self) -> Option<&'static str> {
        match self {
            Self::ShutdownSave => Some("SAVE"),
            Self::ShutdownNosave => Some("NOSAVE"),
            _ => None,
        }
    }

    pub const fn signal_name(self) -> Option<&'static str> {
        match self {
            Self::Sigint => Some("INT"),
            Self::Sigterm => Some("TERM"),
            Self::Sigquit => Some("QUIT"),
            Self::Sigkill => Some("KILL"),
            _ => None,
        }
    }

    pub const fn is_signal(self) -> bool {
        self.signal_name().is_some()
    }
}

#[derive(Debug, Clone)]
pub struct Target {
    pub alias: Option<String>,
    pub addr: String,
    pub protocol: TargetProtocol,
    pub username: Option<String>,
    pub password: Option<String>,
    pub tags: Vec<String>,
    pub process_id: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct RuntimeSettings {
    pub credential_store: Option<std::sync::Arc<crate::credentials::CredentialStore>>,
    pub refresh_interval: Duration,
    pub connect_timeout: Duration,
    pub command_timeout: Duration,
    pub concurrency_limit: usize,
    pub leave_killed_servers: bool,
    pub show_activity: bool,
    pub default_view: ViewMode,
    pub default_sort: SortMode,
    pub ui_theme: UiTheme,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UiColor {
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

impl std::str::FromStr for UiColor {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "black" => Ok(Self::Black),
            "red" => Ok(Self::Red),
            "green" => Ok(Self::Green),
            "yellow" => Ok(Self::Yellow),
            "blue" => Ok(Self::Blue),
            "magenta" => Ok(Self::Magenta),
            "cyan" => Ok(Self::Cyan),
            "gray" | "grey" => Ok(Self::Gray),
            "white" => Ok(Self::White),
            _ => Err(format!(
                "{raw} (supported: black, red, green, yellow, blue, magenta, cyan, gray, white)"
            )),
        }
    }
}

impl UiColor {
    pub const fn to_ratatui_color(self) -> Color {
        match self {
            Self::Black => Color::Black,
            Self::Red => Color::Red,
            Self::Green => Color::Green,
            Self::Yellow => Color::Yellow,
            Self::Blue => Color::Blue,
            Self::Magenta => Color::Magenta,
            Self::Cyan => Color::Cyan,
            Self::Gray => Color::Gray,
            Self::White => Color::White,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UiTheme {
    pub background: UiColor,
    pub foreground: UiColor,
    pub carat: UiColor,
    pub warning: UiColor,
    pub critical: UiColor,
}

impl Default for UiTheme {
    fn default() -> Self {
        Self {
            background: UiColor::Black,
            foreground: UiColor::White,
            carat: UiColor::White,
            warning: UiColor::Yellow,
            critical: UiColor::Red,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstanceType {
    Standalone,
    Cluster,
    Primary,
    Replica,
}

impl InstanceType {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Standalone => "standalone",
            Self::Cluster => "cluster",
            Self::Primary => "primary",
            Self::Replica => "replica",
        }
    }

    pub const fn compact_label(self) -> &'static str {
        match self {
            Self::Standalone => "STD",
            Self::Cluster => "CLU",
            Self::Primary => "PRI",
            Self::Replica => "REP",
        }
    }
}

/// Inclusive range of cluster hash slots served by a shard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SlotRange {
    pub start: u16,
    pub end: u16,
}

impl SlotRange {
    /// Number of slots in the range, saturating on a malformed (reversed) range.
    pub fn count(self) -> u32 {
        u32::from(self.end).saturating_sub(u32::from(self.start)) + 1
    }

    pub fn total(ranges: &[Self]) -> u32 {
        ranges.iter().copied().map(Self::count).sum()
    }

    /// Renders ranges as the comma separated form Redis uses, e.g. `0-5460,9000`.
    pub fn format_ranges(ranges: &[Self]) -> String {
        ranges
            .iter()
            .map(Self::to_string)
            .collect::<Vec<_>>()
            .join(",")
    }
}

impl fmt::Display for SlotRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.start == self.end {
            write!(f, "{}", self.start)
        } else {
            write!(f, "{}-{}", self.start, self.end)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Protected,
    Auth,
    Timeout,
    Down,
    Loading,
    Error,
}

impl Status {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "OK",
            Self::Protected => "PROTECTED",
            Self::Auth => "AUTH",
            Self::Timeout => "TIMEOUT",
            Self::Down => "DOWN",
            Self::Loading => "LOADING",
            Self::Error => "ERROR",
        }
    }

    pub const fn severity(self) -> u8 {
        match self {
            Self::Down => 0,
            Self::Protected => 1,
            Self::Auth => 2,
            Self::Timeout => 3,
            Self::Loading => 4,
            Self::Error => 5,
            Self::Ok => 6,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorDetails {
    pub summary: String,
    pub message: String,
}

#[derive(Debug, Clone, Default)]
pub struct DetailMetrics {
    pub redis_version: Option<String>,
    pub process_id: Option<u32>,
    pub uptime_seconds: Option<u64>,
    pub used_memory_rss: Option<u64>,
    pub total_commands_processed: Option<u64>,
    pub connected_clients: Option<u64>,
    pub blocked_clients: Option<u64>,
    pub keyspace_hits: Option<u64>,
    pub keyspace_misses: Option<u64>,
    pub evicted_keys: Option<u64>,
    pub expired_keys: Option<u64>,
    pub role: Option<String>,
    pub master_host: Option<String>,
    pub master_port: Option<u16>,
    pub cluster_enabled: bool,
    pub commandstats: Vec<CommandStat>,
    pub bigkeys: BigkeysMetrics,
    pub hotkeys: HotkeysMetrics,
    pub raw_info: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CommandStat {
    pub command: String,
    pub calls: u64,
    pub usec: u64,
    pub usec_per_call: f64,
    /// Unrecognized metrics retain the server's representation without numeric conversion.
    pub additional_metrics: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BigkeysScanStatus {
    #[default]
    Idle,
    Running,
    Ready,
    Failed,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BigkeyEntry {
    pub key: String,
    pub key_type: String,
    pub size: Option<u64>,
    pub memory_usage: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BigkeysMetrics {
    pub status: BigkeysScanStatus,
    pub last_error: Option<String>,
    pub largest_keys: Vec<BigkeyEntry>,
    pub last_completed: Option<Instant>,
}

/// Grouping key used for instances that do not belong to a Redis Cluster.
pub const STANDALONE_CLUSTER: &str = "Standalone";

#[derive(Debug, Clone)]
pub struct InstanceState {
    pub key: String,
    pub alias: Option<String>,
    pub addr: String,
    pub kind: InstanceType,
    pub cluster_id: Option<String>,
    /// Hash slots served by this node; only populated for cluster primaries.
    pub slots: Vec<SlotRange>,
    pub parent_addr: Option<String>,
    pub tags: Vec<String>,
    pub info: HashMap<String, String>,
    pub used_memory_bytes: Option<u64>,
    pub maxmemory_bytes: Option<u64>,
    pub ops_per_sec: Option<u64>,
    pub last_latency_ms: Option<f64>,
    pub max_latency_ms: f64,
    pub avg_latency_ms: f64,
    pub status: Status,
    pub last_error: Option<String>,
    pub error_details: Option<ErrorDetails>,
    pub last_updated: Option<Instant>,
    pub latency_window: VecDeque<f64>,
    pub detail: DetailMetrics,
}

impl InstanceState {
    pub fn new(key: String, addr: String) -> Self {
        Self {
            key,
            alias: None,
            addr,
            kind: InstanceType::Standalone,
            cluster_id: None,
            slots: Vec::new(),
            parent_addr: None,
            tags: Vec::new(),
            info: HashMap::new(),
            used_memory_bytes: None,
            maxmemory_bytes: None,
            ops_per_sec: None,
            last_latency_ms: None,
            max_latency_ms: 0.0,
            avg_latency_ms: 0.0,
            status: Status::Down,
            last_error: None,
            error_details: None,
            last_updated: None,
            latency_window: VecDeque::with_capacity(120),
            detail: DetailMetrics::default(),
        }
    }

    pub fn cluster_key(&self) -> &str {
        self.cluster_id.as_deref().unwrap_or(STANDALONE_CLUSTER)
    }

    pub fn display_name(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.addr)
    }

    pub fn is_stale(&self, refresh_interval: Duration) -> bool {
        self.last_updated
            .is_none_or(|ts| ts.elapsed() > refresh_interval.saturating_mul(2))
    }

    pub fn push_latency_sample(&mut self, sample_ms: f64) {
        const MAX_SAMPLES: usize = 120;
        if self.latency_window.len() == MAX_SAMPLES {
            let _ = self.latency_window.pop_front();
        }
        self.latency_window.push_back(sample_ms);
        if sample_ms > self.max_latency_ms {
            self.max_latency_ms = sample_ms;
        }
        let total: f64 = self.latency_window.iter().sum();
        self.avg_latency_ms = total / crate::column::usize_to_f64(self.latency_window.len());
    }
}
