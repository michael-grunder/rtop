//! Live aggregate activity, independent of table filtering and rendering.
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::time::{Duration, Instant};

use crate::model::{InstanceState, Status};

const HISTORY_LEN: usize = 120;

#[derive(Debug)]
struct CpuSample {
    at: Instant,
    seconds: f64,
    run_id: Option<String>,
    uptime: Option<u64>,
    percent: Option<f64>,
}

#[derive(Debug, Clone, Default)]
pub struct ActivityTotals {
    pub servers: usize,
    pub available: usize,
    /// Some servers did not supply every metric (including CPU warm-up).
    pub partial: bool,
    pub cpu: Option<f64>,
    pub ops: Option<u64>,
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub memory: Option<u64>,
    pub clients: Option<u64>,
}

#[derive(Debug, Default)]
pub struct Activity {
    cpu: HashMap<String, CpuSample>,
    scope: BTreeSet<String>,
    sampled_at: Option<Instant>,
    pub current: ActivityTotals,
    pub history: VecDeque<ActivityTotals>,
}

impl Activity {
    pub fn observe(&mut self, state: &InstanceState) {
        let seconds = number(state, "used_cpu_sys")
            .zip(number(state, "used_cpu_user"))
            .map(|(sys, user)| sys + user)
            .filter(|seconds| seconds.is_finite());
        let (Some(at), Some(seconds)) = (state.last_updated, seconds) else {
            self.remove(&state.key);
            return;
        };
        if state.status != Status::Ok {
            self.remove(&state.key);
            return;
        }
        let previous = self.cpu.get(&state.key);
        // Background detail task updates reuse the INFO timestamp.
        if previous.is_some_and(|previous| at <= previous.at) {
            return;
        }
        let run_id = state.info.get("run_id").cloned();
        let uptime = state.detail.uptime_seconds;
        let percent = previous.and_then(|previous| {
            if previous.run_id != run_id || uptime < previous.uptime || seconds < previous.seconds {
                return None;
            }
            let value =
                (seconds - previous.seconds) / at.duration_since(previous.at).as_secs_f64() * 100.0;
            value.is_finite().then_some(value)
        });
        self.cpu.insert(
            state.key.clone(),
            CpuSample {
                at,
                seconds,
                run_id,
                uptime,
                percent,
            },
        );
    }

    pub fn remove(&mut self, key: &str) {
        self.cpu.remove(key);
    }

    pub fn sample<'a>(
        &mut self,
        instances: impl Iterator<Item = &'a InstanceState>,
        interval: Duration,
        now: Instant,
    ) {
        let mut scope = BTreeSet::new();
        let mut totals = ActivityTotals::default();
        for state in instances {
            scope.insert(state.key.clone());
            totals.servers += 1;
            if state.status != Status::Ok
                || state
                    .last_updated
                    .is_none_or(|at| now.saturating_duration_since(at) > interval.saturating_mul(2))
            {
                totals.partial = true;
                continue;
            }
            totals.available += 1;
            let cpu = self.cpu.get(&state.key).and_then(|sample| sample.percent);
            if let Some(cpu) = cpu {
                *totals.cpu.get_or_insert(0.0) += cpu;
            } else {
                totals.partial = true;
            }
            for (total, value) in [
                (&mut totals.ops, state.ops_per_sec),
                (&mut totals.memory, state.used_memory_bytes),
                (&mut totals.clients, state.detail.connected_clients),
                (
                    &mut totals.input,
                    network_bytes(state, "instantaneous_input_kbps"),
                ),
                (
                    &mut totals.output,
                    network_bytes(state, "instantaneous_output_kbps"),
                ),
            ] {
                if let Some(value) = value {
                    *total = Some(total.unwrap_or(0).saturating_add(value));
                } else {
                    totals.partial = true;
                }
            }
        }
        // A different membership is a different time series, even when its size is unchanged.
        if scope != self.scope {
            self.history.clear();
            self.sampled_at = None;
            self.scope = scope;
        }
        self.current = totals.clone();
        if self
            .sampled_at
            .is_none_or(|at| now.saturating_duration_since(at) >= interval)
        {
            if self.history.len() == HISTORY_LEN {
                self.history.pop_front();
            }
            self.history.push_back(totals);
            self.sampled_at = Some(now);
        }
    }
}

fn number(state: &InstanceState, key: &str) -> Option<f64> {
    state
        .info
        .get(key)?
        .parse::<f64>()
        .ok()
        .filter(|n| n.is_finite() && *n >= 0.0)
}

fn network_bytes(state: &InstanceState, key: &str) -> Option<u64> {
    number(state, key).map(|kbps| graph_value(kbps * 1024.0))
}

/// Float metrics are nonnegative. Saturate large values and round to graph precision.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub const fn graph_value(value: f64) -> u64 {
    value.round() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(key: &str, at: Instant, cpu: &str) -> InstanceState {
        let mut state = InstanceState::new(key.into(), key.into());
        state.status = Status::Ok;
        state.last_updated = Some(at);
        state.ops_per_sec = Some(120);
        state.used_memory_bytes = Some(1024);
        state.detail.connected_clients = Some(3);
        for (key, value) in [
            ("used_cpu_sys", "1"),
            ("used_cpu_user", cpu),
            ("instantaneous_input_kbps", "1.5"),
            ("instantaneous_output_kbps", "2"),
            ("run_id", "first"),
        ] {
            state.info.insert(key.into(), value.into());
        }
        state
    }

    #[test]
    fn aggregates_rates_and_ignores_duplicate_updates() {
        let now = Instant::now();
        let mut activity = Activity::default();
        let mut a = server("a", now, "1");
        activity.observe(&a);
        a.last_updated = Some(now + Duration::from_secs(2));
        a.info.insert("used_cpu_user".into(), "2".into());
        activity.observe(&a);
        activity.observe(&a);
        let b = server("b", a.last_updated.unwrap(), "0");
        activity.observe(&b);
        activity.sample(
            [&a, &b].into_iter(),
            Duration::from_secs(1),
            a.last_updated.unwrap(),
        );
        assert_eq!(activity.current.cpu, Some(50.0));
        assert_eq!(activity.current.ops, Some(240));
        assert_eq!(activity.current.input, Some(3072));
        assert_eq!(activity.current.output, Some(4096));
        assert_eq!(activity.current.memory, Some(2048));
        assert_eq!(activity.current.clients, Some(6));
        assert!(activity.current.partial); // b needs a second CPU sample.
    }

    #[test]
    fn cpu_restarts_failures_and_invalid_numbers_need_new_baselines() {
        let now = Instant::now();
        let mut activity = Activity::default();
        let mut a = server("a", now, "1");
        activity.observe(&a);
        for (offset, cpu, run_id) in [(1, "2", "first"), (2, "3", "second"), (3, "0", "second")] {
            a.last_updated = Some(now + Duration::from_secs(offset));
            a.info.insert("used_cpu_user".into(), cpu.into());
            a.info.insert("run_id".into(), run_id.into());
            activity.observe(&a);
            assert_eq!(
                activity.cpu["a"].percent,
                if offset == 1 { Some(100.0) } else { None }
            );
        }
        a.status = Status::Down;
        activity.observe(&a);
        assert!(!activity.cpu.contains_key("a"));
        a.status = Status::Ok;
        for value in ["NaN", "inf", "-1", "bad"] {
            a.info.insert("used_cpu_user".into(), value.into());
            activity.observe(&a);
            assert!(!activity.cpu.contains_key("a"));
        }
    }

    #[test]
    fn failed_and_stale_servers_do_not_contribute_old_values() {
        let now = Instant::now();
        let a = server("a", now, "1");
        let mut b = a.clone();
        b.key = "b".into();
        b.status = Status::Down;
        let mut activity = Activity::default();
        let interval = Duration::from_secs(1);
        activity.sample([&a, &b].into_iter(), interval, now);
        assert_eq!(
            (activity.current.available, activity.current.servers),
            (1, 2)
        );
        assert_eq!(activity.current.ops, Some(120));
        activity.sample([&a, &b].into_iter(), interval, now + Duration::from_secs(3));
        assert_eq!(activity.current.available, 0);
        assert_eq!(activity.current.ops, None);
    }

    #[test]
    fn history_is_bounded_timed_and_resets_on_membership_changes() {
        let now = Instant::now();
        let mut a = server("a", now, "1");
        let b = server("b", now, "1");
        let interval = Duration::from_secs(1);
        let mut activity = Activity::default();
        for offset in 0..130 {
            let at = now + Duration::from_secs(offset);
            a.last_updated = Some(at);
            activity.sample(std::iter::once(&a), interval, at);
            activity.sample(std::iter::once(&a), interval, at);
        }
        assert_eq!(activity.history.len(), HISTORY_LEN);
        activity.sample(
            std::iter::once(&b),
            interval,
            now + Duration::from_secs(130),
        );
        assert_eq!(activity.history.len(), 1);
        activity.sample(std::iter::empty(), interval, now + Duration::from_secs(131));
        assert_eq!(activity.current.servers, 0);
        assert_eq!(activity.current.memory, None);
    }
}
