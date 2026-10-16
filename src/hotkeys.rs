use redis::Value;

pub(crate) const fn parse_hotkeys_support(reply: &Value) -> Option<bool> {
    let Value::Array(commands) = reply else {
        return None;
    };
    match commands.as_slice() {
        [Value::Nil] => Some(false),
        [Value::Array(info)] if !info.is_empty() => Some(true),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HotkeysMetric {
    Cpu,
    Net,
}

impl HotkeysMetric {
    pub const fn redis_arg(self) -> &'static str {
        match self {
            Self::Cpu => "CPU",
            Self::Net => "NET",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Cpu => "CPU",
            Self::Net => "NET",
        }
    }

    pub const fn value_header(self) -> &'static str {
        match self {
            Self::Cpu => "CPU us",
            Self::Net => "Bytes",
        }
    }

    pub const fn total_field(self) -> &'static str {
        match self {
            Self::Cpu => "all-commands-all-slots-us",
            Self::Net => "net-bytes-all-commands-all-slots",
        }
    }

    pub const fn entries_field(self) -> &'static str {
        match self {
            Self::Cpu => "by-cpu-time-us",
            Self::Net => "by-net-bytes",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HotkeysStatus {
    #[default]
    Idle,
    Running,
    Ready,
    Failed,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HotkeyEntry {
    pub key: String,
    pub value: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HotkeysMetrics {
    pub status: HotkeysStatus,
    pub last_error: Option<String>,
    pub selected_metric: Option<HotkeysMetric>,
    pub sample_ratio: Option<u64>,
    pub collection_duration_ms: Option<u64>,
    pub total_value: Option<u64>,
    pub tracking_active: bool,
    pub entries: Vec<HotkeyEntry>,
    pub started_at: Option<std::time::Instant>,
    pub finishes_at: Option<std::time::Instant>,
    pub last_completed: Option<std::time::Instant>,
}

impl HotkeysMetrics {
    pub fn start(&mut self, metric: HotkeysMetric, duration: std::time::Duration) {
        let now = std::time::Instant::now();
        self.status = HotkeysStatus::Running;
        self.last_error = None;
        self.selected_metric = Some(metric);
        self.sample_ratio = None;
        self.collection_duration_ms = None;
        self.total_value = None;
        self.tracking_active = true;
        self.entries.clear();
        self.started_at = Some(now);
        self.finishes_at = Some(now + duration);
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn remaining_seconds(&self) -> Option<u64> {
        let remaining = self
            .finishes_at?
            .saturating_duration_since(std::time::Instant::now());
        Some(remaining.as_secs() + u64::from(remaining.subsec_nanos() > 0))
    }
}

pub(crate) fn parse_hotkeys_get(
    value: &Value,
    metric: HotkeysMetric,
) -> Result<HotkeysMetrics, String> {
    let items = unwrap_single_top_level_array(value)?;
    let (pairs, remainder) = items.as_chunks::<2>();
    if !remainder.is_empty() {
        return Err("HOTKEYS GET reply was not a key/value list".to_string());
    }

    let mut tracking_active = false;
    let mut sample_ratio = None;
    let mut collection_duration_ms = None;
    let mut total_value = None;
    let mut entries = Vec::new();

    for [field_value, value] in pairs {
        let field = value_string(field_value).ok_or_else(|| "invalid HOTKEYS field".to_string())?;
        match field.as_str() {
            "tracking-active" => {
                tracking_active =
                    value_u64(value).ok_or_else(|| "invalid tracking-active".to_string())? > 0;
            }
            "sample-ratio" => {
                sample_ratio = value_u64(value);
            }
            "collection-duration-ms" => {
                collection_duration_ms = value_u64(value);
            }
            _ if field == metric.total_field() => {
                total_value = value_u64(value);
            }
            _ if field == metric.entries_field() => {
                entries = parse_hotkey_entries(value)?;
            }
            _ => {}
        }
    }

    entries.sort_by(|left, right| {
        right
            .value
            .cmp(&left.value)
            .then_with(|| left.key.cmp(&right.key))
    });

    Ok(HotkeysMetrics {
        status: if tracking_active {
            HotkeysStatus::Running
        } else {
            HotkeysStatus::Ready
        },
        last_error: None,
        selected_metric: Some(metric),
        sample_ratio,
        collection_duration_ms,
        total_value,
        tracking_active,
        entries,
        started_at: None,
        finishes_at: None,
        last_completed: Some(std::time::Instant::now()),
    })
}

fn parse_hotkey_entries(value: &Value) -> Result<Vec<HotkeyEntry>, String> {
    let items = value_array(value).ok_or_else(|| "invalid HOTKEYS entries".to_string())?;
    let (pairs, remainder) = items.as_chunks::<2>();
    if !remainder.is_empty() {
        return Err("HOTKEYS entries were not a key/value list".to_string());
    }

    let mut entries = Vec::with_capacity(pairs.len());
    for [key_value, entry_value] in pairs {
        let key = value_string(key_value).ok_or_else(|| "invalid HOTKEYS key".to_string())?;
        let value = value_u64(entry_value).ok_or_else(|| "invalid HOTKEYS value".to_string())?;
        entries.push(HotkeyEntry { key, value });
    }
    Ok(entries)
}

fn unwrap_single_top_level_array(value: &Value) -> Result<&[Value], String> {
    let items = value_array(value).ok_or_else(|| "invalid HOTKEYS GET reply".to_string())?;
    if items.len() == 1
        && let Some(inner) = value_array(&items[0])
    {
        return Ok(inner);
    }
    Ok(items)
}

fn value_array(value: &Value) -> Option<&[Value]> {
    match value {
        Value::Array(items) => Some(items),
        _ => None,
    }
}

fn value_string(value: &Value) -> Option<String> {
    match value {
        Value::BulkString(bytes) => std::str::from_utf8(bytes).ok().map(ToOwned::to_owned),
        Value::SimpleString(text) | Value::VerbatimString { text, .. } => Some(text.clone()),
        _ => None,
    }
}

fn value_u64(value: &Value) -> Option<u64> {
    match value {
        Value::Int(number) => u64::try_from(*number).ok(),
        Value::BulkString(bytes) => std::str::from_utf8(bytes).ok()?.parse().ok(),
        Value::SimpleString(text) => text.parse().ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use redis::Value;

    use super::{
        HotkeyEntry, HotkeysMetric, HotkeysStatus, parse_hotkeys_get, parse_hotkeys_support,
    };

    #[test]
    fn command_info_distinguishes_missing_present_and_unrecognized_replies() {
        assert_eq!(
            parse_hotkeys_support(&Value::Array(vec![Value::Nil])),
            Some(false)
        );
        assert_eq!(
            parse_hotkeys_support(&Value::Array(vec![Value::Array(vec![
                Value::BulkString(b"hotkeys".to_vec()),
                Value::Int(-2),
            ])])),
            Some(true)
        );
        for reply in [
            Value::Nil,
            Value::Okay,
            Value::Array(vec![]),
            Value::Array(vec![Value::Array(vec![])]),
            Value::Array(vec![Value::Nil, Value::Nil]),
        ] {
            assert_eq!(parse_hotkeys_support(&reply), None);
        }
    }

    #[test]
    fn parse_hotkeys_get_extracts_cpu_results() {
        let response = Value::Array(vec![Value::Array(vec![
            Value::BulkString(b"tracking-active".to_vec()),
            Value::Int(0),
            Value::BulkString(b"sample-ratio".to_vec()),
            Value::Int(1),
            Value::BulkString(b"collection-duration-ms".to_vec()),
            Value::Int(48_768),
            Value::BulkString(b"all-commands-all-slots-us".to_vec()),
            Value::Int(6_139_039),
            Value::BulkString(b"by-cpu-time-us".to_vec()),
            Value::Array(vec![
                Value::BulkString(b"mylist".to_vec()),
                Value::Int(5_613_174),
                Value::BulkString(b"mystream".to_vec()),
                Value::Int(89_740),
            ]),
        ])]);

        let parsed = parse_hotkeys_get(&response, HotkeysMetric::Cpu).expect("parsed hotkeys");

        assert_eq!(parsed.status, HotkeysStatus::Ready);
        assert_eq!(parsed.selected_metric, Some(HotkeysMetric::Cpu));
        assert_eq!(parsed.sample_ratio, Some(1));
        assert_eq!(parsed.collection_duration_ms, Some(48_768));
        assert_eq!(parsed.total_value, Some(6_139_039));
        assert_eq!(
            parsed.entries,
            vec![
                HotkeyEntry {
                    key: "mylist".to_string(),
                    value: 5_613_174,
                },
                HotkeyEntry {
                    key: "mystream".to_string(),
                    value: 89_740,
                },
            ]
        );
    }

    #[test]
    fn parse_hotkeys_get_extracts_net_results() {
        let response = Value::Array(vec![Value::Array(vec![
            Value::BulkString(b"tracking-active".to_vec()),
            Value::Int(0),
            Value::BulkString(b"net-bytes-all-commands-all-slots".to_vec()),
            Value::Int(1024),
            Value::BulkString(b"by-net-bytes".to_vec()),
            Value::Array(vec![
                Value::BulkString(b"beta".to_vec()),
                Value::Int(8),
                Value::BulkString(b"alpha".to_vec()),
                Value::Int(16),
            ]),
        ])]);

        let parsed = parse_hotkeys_get(&response, HotkeysMetric::Net).expect("parsed hotkeys");

        assert_eq!(parsed.status, HotkeysStatus::Ready);
        assert_eq!(parsed.total_value, Some(1024));
        assert_eq!(
            parsed.entries,
            vec![
                HotkeyEntry {
                    key: "alpha".to_string(),
                    value: 16,
                },
                HotkeyEntry {
                    key: "beta".to_string(),
                    value: 8,
                },
            ]
        );
    }

    #[test]
    fn parse_hotkeys_get_marks_active_tracking_as_running() {
        let response = Value::Array(vec![Value::Array(vec![
            Value::BulkString(b"tracking-active".to_vec()),
            Value::Int(1),
        ])]);

        let parsed = parse_hotkeys_get(&response, HotkeysMetric::Cpu).expect("parsed hotkeys");

        assert_eq!(parsed.status, HotkeysStatus::Running);
        assert!(parsed.tracking_active);
    }
}
