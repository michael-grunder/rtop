use crate::column::{
    Align, CellCtx, Column, Emphasis, EmphasisLifetime, EmphasisStyle, FormatSpec, SortKey, Tone,
    WidthHint, default_label, parse_info_field, u64_to_f64,
};
use crate::model::{SlotRange, Status};
use crate::target_addr::is_local_addr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CalcKind {
    Addr,
    Alias,
    ProcessId,
    Role,
    Cluster,
    Status,
    SlotsTotal,
    SlotRanges,
    LatencyLastMs,
    LatencyMaxMs,
    MaxmemoryPercent {
        used_key: String,
        max_key: String,
    },
    HitratePercent {
        hits_key: String,
        misses_key: String,
    },
    ClientsTotal {
        key: String,
    },
    OpsPerSec {
        key: String,
    },
}

impl CalcKind {
    fn value(&self, ctx: &CellCtx<'_>) -> SortKey {
        let snap = ctx.snap;
        match self {
            Self::Addr => SortKey::Str(snap.addr.to_ascii_lowercase()),
            Self::Alias => SortKey::Str(display_label(ctx).to_ascii_lowercase()),
            Self::ProcessId => is_local_addr(&snap.addr)
                .then_some(snap.detail.process_id)
                .flatten()
                .map(u64::from)
                .into(),
            Self::Role => SortKey::Str(snap.kind.compact_label().to_string()),
            Self::Cluster => ctx.cluster_label.map(str::to_string).into(),
            Self::Status => SortKey::U64(u64::from(snap.status.severity())),
            Self::SlotsTotal => (!snap.slots.is_empty())
                .then(|| u64::from(SlotRange::total(&snap.slots)))
                .into(),
            // Ordering by the first served slot keeps shards in cluster order.
            Self::SlotRanges => snap
                .slots
                .first()
                .map(|range| u64::from(range.start))
                .into(),
            Self::LatencyLastMs => snap.last_latency_ms.into(),
            Self::LatencyMaxMs => SortKey::F64(snap.max_latency_ms),
            Self::MaxmemoryPercent { used_key, max_key } => {
                let used = parse_info_field(snap, used_key).or(snap.used_memory_bytes);
                let max = parse_info_field(snap, max_key).or(snap.maxmemory_bytes);
                match (used, max) {
                    (Some(used), Some(max)) => percent(used, max).into(),
                    _ => SortKey::Null,
                }
            }
            Self::HitratePercent {
                hits_key,
                misses_key,
            } => {
                let hits = parse_info_field(snap, hits_key)
                    .or(snap.detail.keyspace_hits)
                    .unwrap_or(0);
                let misses = parse_info_field(snap, misses_key)
                    .or(snap.detail.keyspace_misses)
                    .unwrap_or(0);
                percent(hits, hits.saturating_add(misses)).into()
            }
            Self::ClientsTotal { key } => parse_info_field(snap, key)
                .or(snap.detail.connected_clients)
                .into(),
            Self::OpsPerSec { key } => parse_info_field(snap, key).or(snap.ops_per_sec).into(),
        }
    }
}

fn display_label(ctx: &CellCtx<'_>) -> String {
    ctx.snap
        .alias
        .clone()
        .unwrap_or_else(|| default_label(&ctx.snap.addr, ctx.omit_host))
}

fn percent(part: u64, whole: u64) -> Option<f64> {
    (whole > 0).then(|| u64_to_f64(part) / u64_to_f64(whole) * 100.0)
}

pub struct CalcColumn {
    pub header: String,
    pub kind: CalcKind,
    pub format: FormatSpec,
    pub missing: String,
    pub emphasis: Option<Emphasis>,
    pub emphasis_style: Option<EmphasisStyle>,
    pub align: Align,
    pub width_hint: WidthHint,
}

impl Column for CalcColumn {
    fn header(&self) -> &str {
        &self.header
    }

    fn align(&self) -> Align {
        self.align
    }

    fn width_hint(&self) -> WidthHint {
        self.width_hint
    }

    fn render_cell(&self, ctx: &CellCtx<'_>) -> String {
        let snap = ctx.snap;
        // Most kinds render by formatting their sort value; these display
        // something richer than what they sort by.
        let text = match &self.kind {
            CalcKind::Addr => Some(snap.addr.clone()),
            CalcKind::Alias => Some(format!("{}{}", ctx.tree_prefix, display_label(ctx))),
            CalcKind::Status => Some(snap.status.as_str().to_string()),
            CalcKind::SlotRanges => {
                (!snap.slots.is_empty()).then(|| SlotRange::format_ranges(&snap.slots))
            }
            kind => self.format.apply(&kind.value(ctx)),
        };
        text.unwrap_or_else(|| self.missing.clone())
    }

    fn sort_key(&self, ctx: &CellCtx<'_>) -> SortKey {
        self.kind.value(ctx)
    }

    fn tone(&self, ctx: &CellCtx<'_>) -> Option<Tone> {
        if self.kind != CalcKind::Status {
            return None;
        }
        match ctx.snap.status {
            Status::Ok => None,
            Status::Loading | Status::Timeout | Status::Auth | Status::Protected => {
                Some(Tone::Warning)
            }
            Status::Down | Status::Error => Some(Tone::Critical),
        }
    }

    fn emphasis(&self) -> Option<Emphasis> {
        self.emphasis
    }

    fn emphasis_lifetime(&self) -> EmphasisLifetime {
        match self.kind {
            CalcKind::LatencyMaxMs => EmphasisLifetime::TransientRecord,
            _ => EmphasisLifetime::PersistentWinner,
        }
    }

    fn emphasis_style(&self) -> Option<EmphasisStyle> {
        self.emphasis_style
    }
}
