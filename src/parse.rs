use std::collections::{BTreeSet, HashMap};

use redis::Value;

use crate::model::{CommandStat, SlotRange};

#[derive(Debug, Clone, Default)]
pub struct ParsedInfo {
    pub sections: HashMap<String, HashMap<String, String>>,
}

impl ParsedInfo {
    pub fn get(&self, section: &str, key: &str) -> Option<&str> {
        self.sections
            .get(section)
            .and_then(|kv| kv.get(key).map(String::as_str))
    }

    pub fn get_u64(&self, section: &str, key: &str) -> Option<u64> {
        self.get(section, key)?.parse().ok()
    }

    pub fn get_bool_01(&self, section: &str, key: &str) -> bool {
        self.get(section, key).is_some_and(|value| value == "1")
    }

    pub fn flat_map(&self) -> HashMap<String, String> {
        let mut out = HashMap::new();
        for section in self.sections.values() {
            for (key, value) in section {
                out.entry(key.clone()).or_insert_with(|| value.clone());
            }
        }
        out
    }
}

pub fn parse_info(input: &str) -> ParsedInfo {
    let mut parsed = ParsedInfo::default();
    let mut current_section = String::from("default");

    for line in input.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(section) = trimmed.strip_prefix('#') {
            current_section = section.trim().to_ascii_lowercase();
            continue;
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            parsed
                .sections
                .entry(current_section.clone())
                .or_default()
                .insert(k.to_string(), v.to_string());
        }
    }

    parsed
}

pub fn parse_commandstats(info: &ParsedInfo) -> Vec<CommandStat> {
    let Some(section) = info.sections.get("commandstats") else {
        return Vec::new();
    };

    let mut stats = section
        .iter()
        .filter_map(|(key, value)| parse_commandstat_entry(key, value))
        .collect::<Vec<_>>();
    stats.sort_by(|left, right| {
        right
            .calls
            .cmp(&left.calls)
            .then_with(|| left.command.cmp(&right.command))
    });
    stats
}

fn parse_commandstat_entry(key: &str, value: &str) -> Option<CommandStat> {
    let command = key.strip_prefix("cmdstat_")?.to_string();
    let mut calls = None;
    let mut usec = None;
    let mut usec_per_call = None;
    let mut additional_metrics = std::collections::BTreeMap::new();

    for field in value.split(',') {
        let (field_key, field_value) = field.split_once('=')?;
        match field_key {
            "calls" => calls = field_value.parse::<u64>().ok(),
            "usec" => usec = field_value.parse::<u64>().ok(),
            "usec_per_call" => usec_per_call = field_value.parse::<f64>().ok(),
            _ => {
                additional_metrics.insert(field_key.to_string(), field_value.to_string());
            }
        }
    }

    Some(CommandStat {
        command,
        calls: calls?,
        usec: usec?,
        usec_per_call: usec_per_call?,
        additional_metrics,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterShard {
    pub slots: Vec<SlotRange>,
    pub nodes: Vec<ClusterShardNode>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterShardNode {
    pub node_id: Option<String>,
    pub addr: String,
    pub role: ClusterShardRole,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClusterShardRole {
    Primary,
    Replica,
    Unknown,
}

impl ClusterShardRole {
    fn from_value(raw: Option<String>) -> Self {
        let Some(raw) = raw else {
            return Self::Unknown;
        };
        match raw.to_ascii_lowercase().as_str() {
            "master" | "primary" => Self::Primary,
            "slave" | "replica" => Self::Replica,
            _ => Self::Unknown,
        }
    }
}

impl ClusterShardNode {
    pub fn is_primary(&self) -> bool {
        self.role == ClusterShardRole::Primary
    }

    pub fn is_replica(&self) -> bool {
        self.role == ClusterShardRole::Replica
    }
}

pub fn parse_cluster_shards(value: &Value) -> Vec<ClusterShard> {
    let mut shards = Vec::new();
    collect_cluster_shards(value, &mut shards);
    shards
}

pub fn collect_cluster_shard_addresses(value: &Value) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for shard in parse_cluster_shards(value) {
        for node in shard.nodes {
            out.insert(node.addr);
        }
    }
    out
}

fn collect_cluster_shards(value: &Value, out: &mut Vec<ClusterShard>) {
    if let Some(shard) = extract_cluster_shard(value) {
        out.push(shard);
    }

    match value {
        Value::Array(items) | Value::Set(items) => {
            for item in items {
                collect_cluster_shards(item, out);
            }
        }
        Value::Map(entries) => {
            for (_, value) in entries {
                collect_cluster_shards(value, out);
            }
        }
        Value::Attribute { data, attributes } => {
            collect_cluster_shards(data, out);
            for (_, value) in attributes {
                collect_cluster_shards(value, out);
            }
        }
        Value::Push { data, .. } => {
            for value in data {
                collect_cluster_shards(value, out);
            }
        }
        _ => {}
    }
}

fn extract_cluster_shard(value: &Value) -> Option<ClusterShard> {
    let mut slots = Vec::new();
    let mut nodes_value = None;

    for (k, v) in kv_pairs(value)? {
        let Some(key) = value_to_string(k) else {
            continue;
        };
        match key.to_ascii_lowercase().as_str() {
            "slots" => slots = parse_slot_ranges(v),
            "nodes" => nodes_value = Some(v),
            _ => {}
        }
    }

    let mut nodes = Vec::new();
    collect_cluster_shard_nodes(nodes_value?, &mut nodes);
    if nodes.is_empty() {
        return None;
    }

    nodes.sort_by(|a, b| a.addr.cmp(&b.addr));
    nodes.dedup_by(|a, b| a.addr == b.addr);
    slots.sort_unstable();
    slots.dedup();
    Some(ClusterShard { slots, nodes })
}

/// `CLUSTER SHARDS` reports slots as a flat `[start, end, start, end, ...]` list.
fn parse_slot_ranges(value: &Value) -> Vec<SlotRange> {
    let (Value::Array(items) | Value::Set(items)) = value else {
        return Vec::new();
    };

    let (pairs, _remainder) = items.as_chunks::<2>();
    pairs
        .iter()
        .filter_map(|[start, end]| {
            let start = value_to_u16(start)?;
            let end = value_to_u16(end)?;
            (start <= end).then_some(SlotRange { start, end })
        })
        .collect()
}

fn collect_cluster_shard_nodes(value: &Value, out: &mut Vec<ClusterShardNode>) {
    if let Some(node) = extract_cluster_shard_node(value) {
        out.push(node);
    }
    match value {
        Value::Array(items) | Value::Set(items) => {
            for item in items {
                collect_cluster_shard_nodes(item, out);
            }
        }
        Value::Map(entries) => {
            for (_, item) in entries {
                collect_cluster_shard_nodes(item, out);
            }
        }
        Value::Attribute { data, attributes } => {
            collect_cluster_shard_nodes(data, out);
            for (_, item) in attributes {
                collect_cluster_shard_nodes(item, out);
            }
        }
        Value::Push { data, .. } => {
            for item in data {
                collect_cluster_shard_nodes(item, out);
            }
        }
        _ => {}
    }
}

fn extract_cluster_shard_node(value: &Value) -> Option<ClusterShardNode> {
    let kv = kv_pairs(value)?;
    let mut node_id = None;
    let mut endpoint = None;
    let mut hostname = None;
    let mut ip = None;
    let mut host = None;
    let mut port = None;
    let mut role = None;

    for (k, v) in kv {
        let key = value_to_string(k)?.to_ascii_lowercase();
        match key.as_str() {
            "id" => node_id = value_to_string(v),
            "endpoint" => endpoint = value_to_string(v),
            "hostname" => hostname = value_to_string(v),
            "ip" => ip = value_to_string(v),
            "host" => host = value_to_string(v),
            "port" => port = value_to_u16(v),
            "role" => role = value_to_string(v),
            _ => {}
        }
    }

    let host = [endpoint, hostname, ip, host]
        .into_iter()
        .flatten()
        .find(|value| !value.is_empty() && value != "?" && value != "-")?;
    let port = port?;

    Some(ClusterShardNode {
        node_id,
        addr: compose_addr(&host, port)?,
        role: ClusterShardRole::from_value(role),
    })
}

fn kv_pairs(value: &Value) -> Option<Vec<(&Value, &Value)>> {
    match value {
        Value::Map(entries) => Some(entries.iter().map(|(k, v)| (k, v)).collect()),
        Value::Array(items) => {
            let (pairs, remainder) = items.as_chunks::<2>();
            if !remainder.is_empty() {
                return None;
            }
            Some(pairs.iter().map(|[key, value]| (key, value)).collect())
        }
        _ => None,
    }
}

fn value_to_string(value: &Value) -> Option<String> {
    match value {
        Value::BulkString(bytes) => Some(String::from_utf8_lossy(bytes).to_string()),
        Value::SimpleString(text) | Value::VerbatimString { text, .. } => Some(text.clone()),
        Value::Int(num) => Some(num.to_string()),
        Value::Double(num) => Some(num.to_string()),
        Value::BigNumber(num) => Some(num.to_string()),
        _ => None,
    }
}

fn value_to_u16(value: &Value) -> Option<u16> {
    match value {
        Value::Int(num) => (*num).try_into().ok(),
        _ => value_to_string(value)?.parse::<u16>().ok(),
    }
}

fn compose_addr(host: &str, port: u16) -> Option<String> {
    let mut trimmed = host.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(without_port) = strip_port_suffix(trimmed) {
        trimmed = without_port;
    }
    if let Some(inner) = trimmed
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
    {
        return Some(format!("[{inner}]:{port}"));
    }
    if trimmed.contains(':') {
        return Some(format!("[{trimmed}]:{port}"));
    }
    Some(format!("{trimmed}:{port}"))
}

fn strip_port_suffix(host: &str) -> Option<&str> {
    if let Some(inner) = host.strip_prefix('[') {
        let (addr, suffix) = inner.split_once(']')?;
        if suffix
            .strip_prefix(':')
            .is_some_and(|port| !port.is_empty() && port.chars().all(|ch| ch.is_ascii_digit()))
        {
            return Some(addr);
        }
        return None;
    }

    let (prefix, suffix) = host.rsplit_once(':')?;
    if !prefix.contains(':') && !suffix.is_empty() && suffix.chars().all(|ch| ch.is_ascii_digit()) {
        return Some(prefix);
    }

    None
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use redis::Value;

    use super::{
        collect_cluster_shard_addresses, parse_cluster_shards, parse_commandstats, parse_info,
    };
    use crate::model::SlotRange;

    #[test]
    fn parses_info_sections_and_values() {
        let info = "# Server\nredis_version:7.2.4\nuptime_in_seconds:123\n# Memory\nused_memory:4096\nmaxmemory:0\n# Cluster\ncluster_enabled:1\n";
        let parsed = parse_info(info);

        assert_eq!(parsed.get("server", "redis_version"), Some("7.2.4"));
        assert_eq!(parsed.get_u64("server", "uptime_in_seconds"), Some(123));
        assert_eq!(parsed.get_u64("memory", "used_memory"), Some(4096));
        assert!(parsed.get_bool_01("cluster", "cluster_enabled"));
    }

    #[test]
    fn parses_info_keys_with_hyphens() {
        let info =
            "# Replication\nrole:replica\nmaster_host:127.0.0.1\nreplication-offset:123456\n";
        let parsed = parse_info(info);

        assert_eq!(
            parsed.get("replication", "replication-offset"),
            Some("123456")
        );
        assert_eq!(
            parsed
                .flat_map()
                .get("replication-offset")
                .map(String::as_str),
            Some("123456")
        );
    }

    #[test]
    fn parses_and_sorts_commandstats_by_calls_descending() {
        let info = "# Commandstats\ncmdstat_get:calls=100007,usec=173592,usec_per_call=1.74,rejected_calls=0,failed_calls=0\ncmdstat_echo:calls=2057,usec=49361425,usec_per_call=23996.80,rejected_calls=0,failed_calls=0\ncmdstat_lrange:calls=400000,usec=6420146,usec_per_call=16.05,rejected_calls=0,failed_calls=0\n";
        let parsed = parse_info(info);
        let stats = parse_commandstats(&parsed);

        assert_eq!(stats.len(), 3);
        assert_eq!(stats[0].command, "lrange");
        assert_eq!(stats[0].calls, 400_000);
        assert_eq!(stats[1].command, "get");
        assert_eq!(stats[1].usec, 173_592);
        assert!((stats[2].usec_per_call - 23_996.80).abs() < f64::EPSILON);
    }

    #[test]
    fn commandstats_preserve_additional_metrics_without_loss_of_precision() {
        let parsed = parse_info(concat!(
            "# Commandstats\n",
            "cmdstat_hscan:calls=3,usec=39,usec_per_call=13.00,",
            "rejected_calls=0,failed_calls=1,",
            "future_counter=18446744073709551616,future_ratio=0.1234567890123456789,",
            "future_signed=-42,future_exponent=1.25e+30\n",
            "cmdstat_get:calls=1,usec=2,usec_per_call=2.00\n",
        ));
        let stats = parse_commandstats(&parsed);
        assert_eq!(stats.len(), 2);
        let metrics = &stats[0].additional_metrics;
        assert_eq!(metrics.len(), 6);
        assert_eq!(metrics["rejected_calls"], "0");
        assert_eq!(metrics["failed_calls"], "1");
        assert_eq!(metrics["future_counter"], "18446744073709551616");
        assert_eq!(metrics["future_ratio"], "0.1234567890123456789");
        assert_eq!(metrics["future_signed"], "-42");
        assert_eq!(metrics["future_exponent"], "1.25e+30");
        assert!(stats[1].additional_metrics.is_empty());
    }

    #[test]
    fn parses_commandstats_for_subcommands_with_pipe_delimiters() {
        let info = "# Commandstats\ncmdstat_cluster|set-config-epoch:calls=1,usec=72,usec_per_call=72.00,rejected_calls=0,failed_calls=0\ncmdstat_cluster|shards:calls=863,usec=89825,usec_per_call=104.08,rejected_calls=0,failed_calls=0\n";
        let parsed = parse_info(info);
        let stats = parse_commandstats(&parsed);

        assert_eq!(stats.len(), 2);
        assert_eq!(stats[0].command, "cluster|shards");
        assert_eq!(stats[0].calls, 863);
        assert_eq!(stats[1].command, "cluster|set-config-epoch");
        assert_eq!(stats[1].usec, 72);
    }

    #[test]
    fn parses_cluster_shards_from_resp2_array_shape() {
        let response = Value::Array(vec![Value::Array(vec![
            Value::BulkString(b"slots".to_vec()),
            Value::Array(vec![Value::Int(0), Value::Int(5460)]),
            Value::BulkString(b"nodes".to_vec()),
            Value::Array(vec![
                Value::Array(vec![
                    Value::BulkString(b"id".to_vec()),
                    Value::BulkString(b"node-a".to_vec()),
                    Value::BulkString(b"endpoint".to_vec()),
                    Value::BulkString(b"10.0.0.11".to_vec()),
                    Value::BulkString(b"port".to_vec()),
                    Value::Int(7000),
                    Value::BulkString(b"role".to_vec()),
                    Value::BulkString(b"master".to_vec()),
                ]),
                Value::Array(vec![
                    Value::BulkString(b"id".to_vec()),
                    Value::BulkString(b"node-b".to_vec()),
                    Value::BulkString(b"ip".to_vec()),
                    Value::BulkString(b"10.0.0.12".to_vec()),
                    Value::BulkString(b"port".to_vec()),
                    Value::BulkString(b"7001".to_vec()),
                    Value::BulkString(b"role".to_vec()),
                    Value::BulkString(b"replica".to_vec()),
                ]),
            ]),
        ])]);

        let shards = parse_cluster_shards(&response);
        assert_eq!(shards.len(), 1);
        assert_eq!(shards[0].nodes.len(), 2);
        assert!(
            shards[0]
                .nodes
                .iter()
                .any(super::ClusterShardNode::is_primary)
        );
        assert!(
            shards[0]
                .nodes
                .iter()
                .any(super::ClusterShardNode::is_replica)
        );
    }

    #[test]
    fn parses_slot_ranges_for_each_shard() {
        let response = Value::Array(vec![Value::Map(vec![
            (
                Value::BulkString(b"slots".to_vec()),
                Value::Array(vec![
                    Value::Int(10_923),
                    Value::Int(16_383),
                    Value::Int(0),
                    Value::Int(0),
                ]),
            ),
            (
                Value::BulkString(b"nodes".to_vec()),
                Value::Array(vec![Value::Map(vec![
                    (
                        Value::BulkString(b"endpoint".to_vec()),
                        Value::BulkString(b"10.0.0.11".to_vec()),
                    ),
                    (Value::BulkString(b"port".to_vec()), Value::Int(7000)),
                    (
                        Value::BulkString(b"role".to_vec()),
                        Value::BulkString(b"master".to_vec()),
                    ),
                ])]),
            ),
        ])]);

        let shards = parse_cluster_shards(&response);
        assert_eq!(shards.len(), 1);
        assert_eq!(
            shards[0].slots,
            vec![
                SlotRange { start: 0, end: 0 },
                SlotRange {
                    start: 10_923,
                    end: 16_383
                },
            ]
        );
        assert_eq!(SlotRange::total(&shards[0].slots), 5462);
        assert_eq!(SlotRange::format_ranges(&shards[0].slots), "0,10923-16383");
    }

    #[test]
    fn ignores_malformed_slot_entries() {
        let response = Value::Array(vec![Value::Map(vec![
            (
                Value::BulkString(b"slots".to_vec()),
                Value::Array(vec![
                    Value::Int(100),
                    Value::Int(50),
                    Value::Int(200),
                    Value::Int(300),
                    Value::Int(400),
                ]),
            ),
            (
                Value::BulkString(b"nodes".to_vec()),
                Value::Array(vec![Value::Map(vec![
                    (
                        Value::BulkString(b"endpoint".to_vec()),
                        Value::BulkString(b"10.0.0.11".to_vec()),
                    ),
                    (Value::BulkString(b"port".to_vec()), Value::Int(7000)),
                ])]),
            ),
        ])]);

        let shards = parse_cluster_shards(&response);
        assert_eq!(
            shards[0].slots,
            vec![SlotRange {
                start: 200,
                end: 300
            }]
        );
    }

    #[test]
    fn parses_cluster_shards_from_resp3_map_shape() {
        let response = Value::Array(vec![Value::Map(vec![
            (
                Value::BulkString(b"slots".to_vec()),
                Value::Array(vec![Value::Int(0), Value::Int(5460)]),
            ),
            (
                Value::BulkString(b"nodes".to_vec()),
                Value::Array(vec![
                    Value::Map(vec![
                        (
                            Value::BulkString(b"endpoint".to_vec()),
                            Value::BulkString(b"2001:db8::1".to_vec()),
                        ),
                        (Value::BulkString(b"port".to_vec()), Value::Int(7000)),
                        (
                            Value::BulkString(b"role".to_vec()),
                            Value::BulkString(b"master".to_vec()),
                        ),
                    ]),
                    Value::Map(vec![
                        (
                            Value::BulkString(b"endpoint".to_vec()),
                            Value::BulkString(b"?".to_vec()),
                        ),
                        (
                            Value::BulkString(b"ip".to_vec()),
                            Value::BulkString(b"10.0.0.20".to_vec()),
                        ),
                        (Value::BulkString(b"port".to_vec()), Value::Int(7002)),
                        (
                            Value::BulkString(b"role".to_vec()),
                            Value::BulkString(b"replica".to_vec()),
                        ),
                    ]),
                ]),
            ),
        ])]);

        let mut found = BTreeSet::new();
        for shard in parse_cluster_shards(&response) {
            for node in shard.nodes {
                found.insert(node.addr);
            }
        }

        assert_eq!(
            found.into_iter().collect::<Vec<_>>(),
            vec![
                "10.0.0.20:7002".to_string(),
                "[2001:db8::1]:7000".to_string()
            ]
        );
    }

    #[test]
    fn extracts_cluster_shard_addresses_without_double_port() {
        let response = Value::Array(vec![Value::Map(vec![(
            Value::BulkString(b"nodes".to_vec()),
            Value::Array(vec![Value::Map(vec![
                (
                    Value::BulkString(b"endpoint".to_vec()),
                    Value::BulkString(b"10.0.0.30:7005".to_vec()),
                ),
                (Value::BulkString(b"port".to_vec()), Value::Int(7005)),
            ])]),
        )])]);

        assert_eq!(
            collect_cluster_shard_addresses(&response)
                .into_iter()
                .collect::<Vec<_>>(),
            vec!["10.0.0.30:7005".to_string()]
        );
    }
}
