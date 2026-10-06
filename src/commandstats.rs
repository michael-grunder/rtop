use std::collections::BTreeMap;

use crate::column::u64_to_f64;
use crate::model::CommandStat;

/// Combine counters by command. Counter overflow saturates; unsupported extra
/// metrics display as unavailable rather than presenting a misleading total.
pub fn aggregate_commandstats<'a>(
    stats: impl IntoIterator<Item = &'a CommandStat>,
) -> Vec<CommandStat> {
    let mut totals = BTreeMap::new();
    for stat in stats {
        let total = totals.entry(&stat.command).or_insert_with(|| CommandStat {
            command: stat.command.clone(),
            calls: 0,
            usec: 0,
            usec_per_call: 0.0,
            additional_metrics: BTreeMap::new(),
        });
        total.calls = total.calls.saturating_add(stat.calls);
        total.usec = total.usec.saturating_add(stat.usec);
        for (name, value) in &stat.additional_metrics {
            let sum = total
                .additional_metrics
                .entry(name.clone())
                .or_insert_with(|| "0".into());
            *sum = match (sum.parse::<u64>(), value.parse::<u64>()) {
                (Ok(left), Ok(right)) => left.saturating_add(right).to_string(),
                _ => "-".into(),
            };
        }
    }
    totals
        .into_values()
        .map(|mut stat| {
            stat.usec_per_call = if stat.calls == 0 {
                0.0
            } else {
                u64_to_f64(stat.usec) / u64_to_f64(stat.calls)
            };
            stat
        })
        .collect()
}

/// Default columns and additional metrics discovered in INFO COMMANDSTATS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandstatsColumn {
    Command,
    Calls,
    Usec,
    UsecPerCall,
    Metric(String),
}

impl CommandstatsColumn {
    pub const DEFAULT: [Self; 4] = [Self::Command, Self::Calls, Self::Usec, Self::UsecPerCall];

    pub fn header(&self) -> &str {
        match self {
            Self::Command => "Command",
            Self::Calls => "Calls",
            Self::Usec => "Usec",
            Self::UsecPerCall => "Usec/Call",
            Self::Metric(name) => name,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::aggregate_commandstats;
    use crate::parse::{parse_commandstats, parse_info};

    #[test]
    fn aggregates_commands_counters_and_weighted_time() {
        let first = parse_commandstats(&parse_info(concat!(
            "# Commandstats\n",
            "cmdstat_get:calls=2,usec=10,usec_per_call=5,failed_calls=1,label=unknown\n",
            "cmdstat_set:calls=0,usec=0,usec_per_call=0\n",
        )));
        let second = parse_commandstats(&parse_info(concat!(
            "# Commandstats\n",
            "cmdstat_get:calls=8,usec=10,usec_per_call=1.25,failed_calls=3,rejected_calls=2,label=7\n",
            "cmdstat_ping:calls=1,usec=3,usec_per_call=3\n",
        )));
        let totals = aggregate_commandstats(first.iter().chain(&second));
        assert_eq!(totals.len(), 3);
        let get = &totals[0];
        assert_eq!(get.command, "get");
        assert_eq!((get.calls, get.usec), (10, 20));
        assert!((get.usec_per_call - 2.0).abs() < f64::EPSILON);
        assert_eq!(get.additional_metrics["failed_calls"], "4");
        assert_eq!(get.additional_metrics["rejected_calls"], "2");
        assert_eq!(get.additional_metrics["label"], "-");
        assert_eq!(totals[1], second[1]);
        assert_eq!(totals[2], first[1]);
        assert_eq!(
            aggregate_commandstats([]),
            Vec::<crate::model::CommandStat>::new()
        );

        let mut huge = first[0].clone();
        huge.calls = u64::MAX;
        huge.usec = u64::MAX;
        huge.additional_metrics
            .insert("failed_calls".into(), u64::MAX.to_string());
        let totals = aggregate_commandstats([&huge, &first[0]]);
        assert_eq!((totals[0].calls, totals[0].usec), (u64::MAX, u64::MAX));
        assert_eq!(
            totals[0].additional_metrics["failed_calls"],
            u64::MAX.to_string()
        );
    }
}
