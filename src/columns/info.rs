use crate::column::{
    Align, CellCtx, Column, Emphasis, EmphasisStyle, FormatSpec, SortKey, ValueType, WidthHint,
    parse_bool, parse_info_field,
};
use crate::model::InstanceState;

pub struct RedisInfoFieldColumn {
    pub header: String,
    pub info_key: String,
    pub value_type: ValueType,
    pub format: FormatSpec,
    pub missing: String,
    pub emphasis: Option<Emphasis>,
    pub emphasis_style: Option<EmphasisStyle>,
    pub align: Align,
    pub width_hint: WidthHint,
}

impl RedisInfoFieldColumn {
    fn value(&self, snap: &InstanceState) -> SortKey {
        let key = self.info_key.as_str();
        match self.value_type {
            ValueType::String => snap.info.get(key).cloned().into(),
            ValueType::U64 | ValueType::Bytes => parse_info_field::<u64>(snap, key).into(),
            ValueType::I64 => parse_info_field(snap, key).map_or(SortKey::Null, SortKey::I64),
            ValueType::F64 | ValueType::Percent => parse_info_field::<f64>(snap, key).into(),
            ValueType::Bool => parse_bool(snap, key).map_or(SortKey::Null, SortKey::Bool),
        }
    }
}

impl Column for RedisInfoFieldColumn {
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
        self.format
            .apply(&self.value(ctx.snap))
            .unwrap_or_else(|| self.missing.clone())
    }

    fn sort_key(&self, ctx: &CellCtx<'_>) -> SortKey {
        match self.value(ctx.snap) {
            SortKey::Str(text) => SortKey::Str(text.to_ascii_lowercase()),
            other => other,
        }
    }

    fn emphasis(&self) -> Option<Emphasis> {
        self.emphasis
    }

    fn emphasis_style(&self) -> Option<EmphasisStyle> {
        self.emphasis_style
    }
}
