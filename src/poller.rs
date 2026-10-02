use std::collections::HashMap;
use std::fs;
use std::future::Future;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use redis::{ErrorKind, Value};
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::MissedTickBehavior;

use crate::discovery::{local_process_id_for_tcp_port, local_process_id_for_unix_socket};
use crate::hotkeys::{HotkeysMetric, HotkeysMetrics, HotkeysStatus, parse_hotkeys_get};
use crate::model::{
    BigkeyEntry, BigkeysMetrics, BigkeysScanStatus, ErrorDetails, InstanceState, InstanceType,
    KillAction, RuntimeSettings, Status, Target, TargetProtocol,
};
use crate::parse::{ClusterShard, parse_cluster_shards, parse_commandstats, parse_info};
use crate::redis_connection;
use crate::target_addr::{
    canonical_host, is_local_addr, is_loopback_host, strip_host, tcp_host, tcp_port,
};
use crate::text::{first_line, truncate_chars};

const BIGKEYS_SCAN_COUNT: usize = 256;
const BIGKEYS_TOP_N: usize = 20;
const HOTKEYS_TOP_N: usize = 20;
pub const HOTKEYS_DURATION: Duration = Duration::from_mins(1);
const HOTKEYS_GET_POLL_INTERVAL: Duration = Duration::from_millis(250);
const HOTKEYS_GET_MAX_ATTEMPTS: usize = 256;
const TASK_ERROR_MAX_CHARS: usize = 120;

#[derive(Debug, Clone)]
pub enum PollerRequest {
    RefreshAll,
    UpsertTarget(Target),
    RefreshBigkeys {
        key: String,
        force: bool,
    },
    StartHotkeys {
        key: String,
        metric: HotkeysMetric,
        force: bool,
    },
    StopHotkeys {
        key: String,
    },
    KillTargets {
        keys: Vec<String>,
        action: KillAction,
    },
    AuthenticateTargets {
        keys: Vec<String>,
        username: Option<String>,
        password: String,
    },
}

#[derive(Debug, Clone)]
pub enum PollerUpdate {
    State(Box<InstanceState>),
    Remove { key: String },
}

/// Long-running per-instance jobs that must not block regular polling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum TaskKind {
    Bigkeys,
    Hotkeys,
}

enum TaskOutcome {
    Bigkeys(BigkeysMetrics),
    Hotkeys(HotkeysMetrics),
}

struct TaskResult {
    key: String,
    generation: u64,
    outcome: TaskOutcome,
}

struct BackgroundTask {
    generation: u64,
    /// Asks the task to finish early but still report its results.
    stop_tx: Option<oneshot::Sender<()>>,
    handle: JoinHandle<()>,
}

/// The UI side hung up; the poller should shut down.
struct Disconnected;

type Step = Result<(), Disconnected>;

struct Poller {
    settings: RuntimeSettings,
    semaphore: Arc<Semaphore>,
    targets: HashMap<String, Target>,
    known_states: HashMap<String, InstanceState>,
    tasks: HashMap<(String, TaskKind), BackgroundTask>,
    next_generation: u64,
    update_tx: mpsc::Sender<PollerUpdate>,
    task_tx: mpsc::Sender<TaskResult>,
}

pub fn start(
    targets: Vec<Target>,
    settings: RuntimeSettings,
) -> (mpsc::Receiver<PollerUpdate>, mpsc::Sender<PollerRequest>) {
    let (update_tx, update_rx) = mpsc::channel(1024);
    let (request_tx, mut request_rx) = mpsc::channel::<PollerRequest>(32);
    let (task_tx, mut task_rx) = mpsc::channel::<TaskResult>(128);

    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(settings.refresh_interval);
        // A slow refresh must not trigger a burst of catch-up refreshes.
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut poller = Poller {
            semaphore: Arc::new(Semaphore::new(settings.concurrency_limit.max(1))),
            settings,
            targets: targets
                .into_iter()
                .map(|target| (target.addr.clone(), target))
                .collect(),
            known_states: HashMap::new(),
            tasks: HashMap::new(),
            next_generation: 0,
            update_tx,
            task_tx,
        };

        loop {
            let step = tokio::select! {
                Some(result) = task_rx.recv() => poller.finish_task(result).await,
                _ = ticker.tick() => poller.handle(PollerRequest::RefreshAll).await,
                request = request_rx.recv() => match request {
                    Some(request) => poller.handle(request).await,
                    None => break,
                },
            };
            if step.is_err() {
                break;
            }
        }
        poller.cancel_all_tasks();
    });

    (update_rx, request_tx)
}

impl Poller {
    async fn handle(&mut self, request: PollerRequest) -> Step {
        match request {
            PollerRequest::RefreshAll => {
                let targets = self.targets.values().cloned().collect();
                for state in self
                    .run_concurrently(targets, |target, settings, prior| async move {
                        poll_one(&target, &settings, prior).await
                    })
                    .await
                {
                    self.publish(state).await?;
                }
            }
            PollerRequest::UpsertTarget(target) => {
                if self.targets.contains_key(&target.addr) {
                    return Ok(());
                }
                self.targets.insert(target.addr.clone(), target.clone());
                let prior = self.known_states.get(&target.addr).cloned();
                let state = self.limited(poll_one(&target, &self.settings, prior)).await;
                self.publish(state).await?;
            }
            PollerRequest::RefreshBigkeys { key, force } => self.start_bigkeys(key, force).await?,
            PollerRequest::StartHotkeys { key, metric, force } => {
                self.start_hotkeys(key, metric, force).await?;
            }
            PollerRequest::StopHotkeys { key } => {
                if let Some(stop_tx) = self
                    .tasks
                    .get_mut(&(key, TaskKind::Hotkeys))
                    .and_then(|task| task.stop_tx.take())
                {
                    let _ = stop_tx.send(());
                }
            }
            PollerRequest::KillTargets { keys, action } => {
                let targets = self.claim_targets(&keys);
                let results = self
                    .run_concurrently(targets, move |target, settings, prior| async move {
                        kill_target(&target, &settings, prior, action).await
                    })
                    .await;
                for update in results {
                    match update {
                        PollerUpdate::State(state) => self.publish(*state).await?,
                        PollerUpdate::Remove { key } => self.publish_removal(key).await?,
                    }
                }
            }
            PollerRequest::AuthenticateTargets {
                keys,
                username,
                password,
            } => {
                for key in &keys {
                    if let Some(target) = self.targets.get_mut(key) {
                        target.username.clone_from(&username);
                        target.password = Some(password.clone());
                    }
                }
                let targets = self.claim_targets(&keys);
                let results = self
                    .run_concurrently(targets, |target, settings, prior| async move {
                        poll_one(&target, &settings, prior).await
                    })
                    .await;
                for state in results {
                    self.publish(state).await?;
                }
            }
        }
        Ok(())
    }

    /// Known targets among `keys`, with their background tasks cancelled
    /// because the action is about to change their connection state.
    fn claim_targets(&mut self, keys: &[String]) -> Vec<Target> {
        let targets: Vec<Target> = keys
            .iter()
            .filter_map(|key| self.targets.get(key).cloned())
            .collect();
        for target in &targets {
            self.cancel_task(&target.addr, TaskKind::Hotkeys);
            self.cancel_task(&target.addr, TaskKind::Bigkeys);
        }
        targets
    }

    /// Runs `job` for every target under the concurrency limit and returns
    /// the results in input order.
    async fn run_concurrently<T, F, Fut>(&self, targets: Vec<Target>, job: F) -> Vec<T>
    where
        T: Send + 'static,
        F: Fn(Target, RuntimeSettings, Option<InstanceState>) -> Fut,
        Fut: Future<Output = T> + Send + 'static,
    {
        let jobs: Vec<Fut> = targets
            .into_iter()
            .map(|target| {
                let prior = self.known_states.get(&target.addr).cloned();
                job(target, self.settings.clone(), prior)
            })
            .collect();
        run_limited(jobs, &self.semaphore).await
    }

    async fn limited<T>(&self, job: impl Future<Output = T>) -> T {
        let _permit = self.semaphore.acquire().await.ok();
        job.await
    }

    async fn start_bigkeys(&mut self, key: String, force: bool) -> Step {
        let Some(prior) = self.known_states.get(&key).cloned() else {
            return Ok(());
        };
        let Some(target) = self.targets.get(&key).cloned() else {
            return Ok(());
        };
        let in_flight = self.tasks.contains_key(&(key.clone(), TaskKind::Bigkeys));
        if !force && (in_flight || prior.detail.bigkeys.status == BigkeysScanStatus::Ready) {
            return Ok(());
        }

        let readonly = bigkeys_requires_readonly(&prior);
        let previous = prior.detail.bigkeys.clone();
        let mut running = prior;
        running.detail.bigkeys.status = BigkeysScanStatus::Running;
        running.detail.bigkeys.last_error = None;
        self.publish(running).await?;

        let settings = self.settings.clone();
        let semaphore = Arc::clone(&self.semaphore);
        self.spawn_task(key, TaskKind::Bigkeys, None, async move {
            let _permit = semaphore.acquire_owned().await.ok();
            TaskOutcome::Bigkeys(poll_bigkeys(&target, &settings, readonly, previous).await)
        });
        Ok(())
    }

    async fn start_hotkeys(&mut self, key: String, metric: HotkeysMetric, force: bool) -> Step {
        let Some(prior) = self.known_states.get(&key).cloned() else {
            return Ok(());
        };
        let Some(target) = self.targets.get(&key).cloned() else {
            return Ok(());
        };
        if !force && prior.detail.hotkeys.status == HotkeysStatus::Running {
            return Ok(());
        }

        let mut running = prior;
        running.detail.hotkeys.start(metric, HOTKEYS_DURATION);
        let started = running.detail.hotkeys.clone();
        self.publish(running).await?;

        let (stop_tx, stop_rx) = oneshot::channel();
        let settings = self.settings.clone();
        let semaphore = Arc::clone(&self.semaphore);
        self.spawn_task(key, TaskKind::Hotkeys, Some(stop_tx), async move {
            let _permit = semaphore.acquire_owned().await.ok();
            TaskOutcome::Hotkeys(poll_hotkeys(&target, &settings, started, metric, stop_rx).await)
        });
        Ok(())
    }

    /// Replaces any task of the same kind; a replaced task's late result is
    /// discarded through its generation number.
    fn spawn_task(
        &mut self,
        key: String,
        kind: TaskKind,
        stop_tx: Option<oneshot::Sender<()>>,
        job: impl Future<Output = TaskOutcome> + Send + 'static,
    ) {
        self.cancel_task(&key, kind);
        self.next_generation += 1;
        let generation = self.next_generation;
        let task_tx = self.task_tx.clone();
        let result_key = key.clone();
        let handle = tokio::spawn(async move {
            let outcome = job.await;
            let _ = task_tx
                .send(TaskResult {
                    key: result_key,
                    generation,
                    outcome,
                })
                .await;
        });
        self.tasks.insert(
            (key, kind),
            BackgroundTask {
                generation,
                stop_tx,
                handle,
            },
        );
    }

    fn cancel_task(&mut self, key: &str, kind: TaskKind) {
        if let Some(task) = self.tasks.remove(&(key.to_string(), kind)) {
            if let Some(stop_tx) = task.stop_tx {
                let _ = stop_tx.send(());
            }
            task.handle.abort();
        }
    }

    fn cancel_all_tasks(&mut self) {
        for (_, task) in self.tasks.drain() {
            task.handle.abort();
        }
    }

    /// Merges a finished task into the newest state rather than restoring
    /// the snapshot the task started from.
    async fn finish_task(&mut self, result: TaskResult) -> Step {
        let kind = match result.outcome {
            TaskOutcome::Bigkeys(_) => TaskKind::Bigkeys,
            TaskOutcome::Hotkeys(_) => TaskKind::Hotkeys,
        };
        let task_key = (result.key, kind);
        if self
            .tasks
            .get(&task_key)
            .is_none_or(|task| task.generation != result.generation)
        {
            return Ok(());
        }
        self.tasks.remove(&task_key);
        let Some(mut state) = self.known_states.get(&task_key.0).cloned() else {
            return Ok(());
        };
        match result.outcome {
            TaskOutcome::Bigkeys(bigkeys) => state.detail.bigkeys = bigkeys,
            TaskOutcome::Hotkeys(hotkeys) => state.detail.hotkeys = hotkeys,
        }
        self.publish(state).await
    }

    async fn publish(&mut self, state: InstanceState) -> Step {
        self.known_states.insert(state.key.clone(), state.clone());
        self.update_tx
            .send(PollerUpdate::State(Box::new(state)))
            .await
            .map_err(|_| Disconnected)
    }

    async fn publish_removal(&mut self, key: String) -> Step {
        self.known_states.remove(&key);
        self.targets.remove(&key);
        self.update_tx
            .send(PollerUpdate::Remove { key })
            .await
            .map_err(|_| Disconnected)
    }
}

/// Spawns every job, bounded by `semaphore`, and collects results in input
/// order. A job that panics is dropped from the results.
async fn run_limited<T, Fut>(
    jobs: impl IntoIterator<Item = Fut>,
    semaphore: &Arc<Semaphore>,
) -> Vec<T>
where
    T: Send + 'static,
    Fut: Future<Output = T> + Send + 'static,
{
    let mut set = JoinSet::new();
    for (index, job) in jobs.into_iter().enumerate() {
        let semaphore = Arc::clone(semaphore);
        set.spawn(async move {
            let _permit = semaphore.acquire_owned().await.ok();
            (index, job.await)
        });
    }
    let mut results = Vec::with_capacity(set.len());
    while let Some(joined) = set.join_next().await {
        if let Ok(result) = joined {
            results.push(result);
        }
    }
    results.sort_unstable_by_key(|(index, _)| *index);
    results.into_iter().map(|(_, result)| result).collect()
}

pub async fn refresh_targets_once(
    targets: Vec<Target>,
    settings: RuntimeSettings,
) -> Vec<InstanceState> {
    let semaphore = Arc::new(Semaphore::new(settings.concurrency_limit.max(1)));
    let jobs = targets.into_iter().map(|target| {
        let settings = settings.clone();
        async move { poll_one(&target, &settings, None).await }
    });
    run_limited(jobs, &semaphore).await
}

async fn poll_one(
    target: &Target,
    settings: &RuntimeSettings,
    prior: Option<InstanceState>,
) -> InstanceState {
    let mut state =
        prior.unwrap_or_else(|| InstanceState::new(target.addr.clone(), target.addr.clone()));
    state.alias = target.alias.clone();
    state.addr = target.addr.clone();
    state.tags = target.tags.clone();
    if state.detail.process_id.is_none() {
        state.detail.process_id = target.process_id;
    }

    let connect_start = Instant::now();
    let mut conn = match redis_connection::connect(target, settings).await {
        Ok(conn) => conn,
        Err(err) => {
            apply_error(&mut state, &err, connect_start);
            return state;
        }
    };

    let ping_start = Instant::now();
    if let Err(err) = redis::cmd("PING").query_async::<String>(&mut conn).await {
        apply_error(&mut state, &err, ping_start);
        return state;
    }
    let latency_ms = ping_start.elapsed().as_secs_f64() * 1000.0;

    let info_start = Instant::now();
    let info: String = match redis::cmd("INFO").query_async(&mut conn).await {
        Ok(info) => info,
        Err(err) => {
            apply_error(&mut state, &err, info_start);
            return state;
        }
    };

    let commandstats_info = redis::cmd("INFO")
        .arg("COMMANDSTATS")
        .query_async::<String>(&mut conn)
        .await
        .ok();

    apply_info_to_state(&mut state, &info, commandstats_info.as_deref());
    if state.detail.process_id.is_none() {
        state.detail.process_id = resolve_local_process_id(target, &mut conn).await;
    }

    if state.detail.cluster_enabled
        && let Ok(shards) = redis::cmd("CLUSTER")
            .arg("SHARDS")
            .query_async::<Value>(&mut conn)
            .await
    {
        apply_cluster_shards_to_state(&mut state, target, &shards);
    }

    record_latency_sample(&mut state, latency_ms);
    state.status = Status::Ok;
    state.last_error = None;
    state.error_details = None;
    state.last_updated = Some(Instant::now());

    state
}

async fn poll_bigkeys(
    target: &Target,
    settings: &RuntimeSettings,
    readonly: bool,
    previous: BigkeysMetrics,
) -> BigkeysMetrics {
    let result = async {
        let mut conn = redis_connection::connect(target, settings).await?;
        if readonly {
            redis::cmd("READONLY")
                .query_async::<String>(&mut conn)
                .await?;
        }
        scan_bigkeys(&mut conn).await
    };
    match result.await {
        Ok(bigkeys) => bigkeys,
        Err(err) => bigkeys_failure(previous, &err.to_string()),
    }
}

async fn poll_hotkeys(
    target: &Target,
    settings: &RuntimeSettings,
    started: HotkeysMetrics,
    metric: HotkeysMetric,
    stop_rx: oneshot::Receiver<()>,
) -> HotkeysMetrics {
    let mut conn = match redis_connection::connect(target, settings).await {
        Ok(conn) => conn,
        Err(err) => return hotkeys_failure(started, metric, &classify_error(&err).1.message),
    };

    let start_result = redis::cmd("HOTKEYS")
        .arg("START")
        .arg("METRICS")
        .arg(1)
        .arg(metric.redis_arg())
        .arg("COUNT")
        .arg(HOTKEYS_TOP_N)
        .arg("DURATION")
        .arg(HOTKEYS_DURATION.as_secs())
        .query_async::<String>(&mut conn)
        .await;
    if let Err(err) = start_result {
        return hotkeys_failure(started, metric, &err.to_string());
    }

    let manually_stopped = tokio::select! {
        () = tokio::time::sleep(HOTKEYS_DURATION) => false,
        result = stop_rx => result.is_ok(),
    };

    if manually_stopped
        && let Err(err) = redis::cmd("HOTKEYS")
            .arg("STOP")
            .query_async::<String>(&mut conn)
            .await
    {
        return hotkeys_failure(started, metric, &err.to_string());
    }

    for _ in 0..HOTKEYS_GET_MAX_ATTEMPTS {
        let reply = match redis::cmd("HOTKEYS")
            .arg("GET")
            .query_async::<Value>(&mut conn)
            .await
        {
            Ok(reply) => reply,
            Err(err) => return hotkeys_failure(started, metric, &err.to_string()),
        };
        let mut hotkeys = match parse_hotkeys_get(&reply, metric) {
            Ok(hotkeys) => hotkeys,
            Err(err) => return hotkeys_failure(started, metric, &err),
        };
        hotkeys.started_at = started.started_at;
        hotkeys.finishes_at = started.finishes_at;
        if !hotkeys.tracking_active {
            return hotkeys;
        }
        tokio::time::sleep(HOTKEYS_GET_POLL_INTERVAL).await;
    }

    hotkeys_failure(
        started,
        metric,
        "HOTKEYS GET did not finish after sampling duration",
    )
}

async fn kill_target(
    target: &Target,
    settings: &RuntimeSettings,
    state: Option<InstanceState>,
    action: KillAction,
) -> PollerUpdate {
    let attempt_error = match action.shutdown_arg() {
        Some(mode) => request_shutdown(target, settings, mode).await.err(),
        None => send_signal(target, state.as_ref(), action).err(),
    };

    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut updated = poll_one(target, settings, state).await;

    if updated.status == Status::Down {
        return if settings.leave_killed_servers {
            PollerUpdate::State(Box::new(updated))
        } else {
            PollerUpdate::Remove { key: updated.key }
        };
    }

    let message = attempt_error.unwrap_or_else(|| {
        format!(
            "{} is still reachable after {}",
            target.addr,
            action.label()
        )
    });
    record_control_failure(&mut updated, action, &message);
    PollerUpdate::State(Box::new(updated))
}

async fn request_shutdown(
    target: &Target,
    settings: &RuntimeSettings,
    mode: &str,
) -> Result<(), String> {
    let mut conn = redis_connection::connect(target, settings)
        .await
        .map_err(|err| err.to_string())?;

    let result = redis::cmd("SHUTDOWN")
        .arg(mode)
        .query_async::<Value>(&mut conn)
        .await;
    if let Err(err) = result {
        let message = err.to_string();
        let normalized = message.to_ascii_lowercase();
        if normalized.contains("connection reset")
            || normalized.contains("broken pipe")
            || normalized.contains("closed")
            || normalized.contains("unexpected eof")
        {
            return Ok(());
        }
        return Err(message);
    }

    Ok(())
}

fn send_signal(
    target: &Target,
    state: Option<&InstanceState>,
    action: KillAction,
) -> Result<(), String> {
    let Some(signal) = action.signal_name() else {
        return Err(format!("{} is not a signal action", action.label()));
    };
    if !target_supports_local_signal(target) {
        return Err(format!(
            "{} only works for local TCP or Unix socket targets",
            action.label()
        ));
    }
    let Some(process_id) = target
        .process_id
        .or_else(|| state.and_then(|state| state.detail.process_id))
    else {
        return Err(format!("{} requires a local process_id", action.label()));
    };

    let output = Command::new("kill")
        .arg(format!("-{signal}"))
        .arg(process_id.to_string())
        .output()
        .map_err(|err| err.to_string())?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if stderr.is_empty() {
        Err(format!(
            "kill -{signal} {process_id} exited with {}",
            output.status
        ))
    } else {
        Err(stderr)
    }
}

fn target_supports_local_signal(target: &Target) -> bool {
    match target.protocol {
        TargetProtocol::Unix => true,
        TargetProtocol::Tcp => is_local_addr(&target.addr),
    }
}

async fn resolve_local_process_id(
    target: &Target,
    conn: &mut impl redis::aio::ConnectionLike,
) -> Option<u32> {
    match target.protocol {
        TargetProtocol::Unix => local_process_id_for_unix_socket(&target.addr),
        TargetProtocol::Tcp => {
            if !is_local_addr(&target.addr) {
                return None;
            }

            if let Some(process_id) = tcp_port(&target.addr).and_then(local_process_id_for_tcp_port)
            {
                return Some(process_id);
            }

            if should_try_pidfile_lookup(target) {
                return pid_from_config_get(conn).await;
            }

            None
        }
    }
}

fn should_try_pidfile_lookup(target: &Target) -> bool {
    target.protocol == TargetProtocol::Tcp
        && tcp_host(&target.addr).is_some_and(|host| is_loopback_host(&host))
}

async fn pid_from_config_get(conn: &mut impl redis::aio::ConnectionLike) -> Option<u32> {
    let reply = redis::cmd("CONFIG")
        .arg("GET")
        .arg("pidfile")
        .query_async::<Value>(conn)
        .await
        .ok()?;
    let pidfile = parse_pidfile_from_config_get(&reply)?;
    let raw = fs::read_to_string(pidfile).ok()?;
    raw.trim().parse::<u32>().ok()
}

fn parse_pidfile_from_config_get(reply: &Value) -> Option<&str> {
    match reply {
        Value::Array(values) => values.windows(2).find_map(|pair| match pair {
            [Value::BulkString(key), Value::BulkString(value)] if key.as_slice() == b"pidfile" => {
                std::str::from_utf8(value).ok()
            }
            _ => None,
        }),
        _ => None,
    }
}

fn record_control_failure(state: &mut InstanceState, action: KillAction, message: &str) {
    let details = error_details(format!("{} failed: {message}", action.label()));
    state.last_error = Some(details.summary.clone());
    state.error_details = Some(details);
}

pub(crate) fn apply_info_to_state(
    state: &mut InstanceState,
    info_raw: &str,
    commandstats_raw: Option<&str>,
) {
    let info = parse_info(info_raw);
    state.info = info.flat_map();

    state.used_memory_bytes = info.get_u64("memory", "used_memory");
    state.maxmemory_bytes = info.get_u64("memory", "maxmemory");
    state.ops_per_sec = info.get_u64("stats", "instantaneous_ops_per_sec");

    state.detail.redis_version = info.get("server", "redis_version").map(str::to_string);
    state.detail.process_id = info
        .get("server", "process_id")
        .and_then(|value| value.parse::<u32>().ok());
    state.detail.uptime_seconds = info.get_u64("server", "uptime_in_seconds");
    state.detail.used_memory_rss = info.get_u64("memory", "used_memory_rss");
    state.detail.total_commands_processed = info.get_u64("stats", "total_commands_processed");
    state.detail.connected_clients = info.get_u64("clients", "connected_clients");
    state.detail.blocked_clients = info.get_u64("clients", "blocked_clients");
    state.detail.keyspace_hits = info.get_u64("stats", "keyspace_hits");
    state.detail.keyspace_misses = info.get_u64("stats", "keyspace_misses");
    state.detail.evicted_keys = info.get_u64("stats", "evicted_keys");
    state.detail.expired_keys = info.get_u64("stats", "expired_keys");
    state.detail.role = info.get("replication", "role").map(str::to_string);
    state.detail.master_host = info.get("replication", "master_host").map(str::to_string);
    state.detail.master_port = info
        .get("replication", "master_port")
        .and_then(|v| v.parse::<u16>().ok());
    state.detail.cluster_enabled = info.get_bool_01("cluster", "cluster_enabled");
    state.detail.commandstats = commandstats_raw
        .map(parse_info)
        .map(|parsed| parse_commandstats(&parsed))
        .filter(|stats| !stats.is_empty())
        .unwrap_or_else(|| parse_commandstats(&info));
    state.detail.raw_info = Some(info_raw.to_string());

    // Repopulated from CLUSTER SHARDS below for cluster primaries only.
    state.slots.clear();

    if state.detail.cluster_enabled {
        state.kind = InstanceType::Cluster;
    } else {
        match state.detail.role.as_deref() {
            Some("master") => {
                state.kind = InstanceType::Primary;
                state.parent_addr = None;
            }
            Some("slave" | "replica") => {
                state.kind = InstanceType::Replica;
                state.parent_addr = match (&state.detail.master_host, state.detail.master_port) {
                    (Some(host), Some(port)) => Some(format!("{host}:{port}")),
                    _ => None,
                };
            }
            _ => {
                state.kind = InstanceType::Standalone;
                state.parent_addr = None;
            }
        }
    }
}

fn apply_failure(state: &mut InstanceState, status: Status, details: ErrorDetails) {
    state.status = status;
    state.last_error = Some(details.summary.clone());
    state.error_details = Some(details);
}

fn record_latency_sample(state: &mut InstanceState, latency_ms: f64) {
    state.push_latency_sample(latency_ms);
    state.last_latency_ms = Some(latency_ms);
}

fn apply_error(state: &mut InstanceState, error: &redis::RedisError, start: Instant) {
    let (status, details) = classify_error(error);
    apply_timed_failure(state, status, details, start);
}

fn apply_timed_failure(
    state: &mut InstanceState,
    status: Status,
    details: ErrorDetails,
    start: Instant,
) {
    if status == Status::Timeout {
        record_latency_sample(state, start.elapsed().as_secs_f64() * 1000.0);
    }
    apply_failure(state, status, details);
}

/// Keeps the previous results visible alongside the error.
fn bigkeys_failure(previous: BigkeysMetrics, message: &str) -> BigkeysMetrics {
    BigkeysMetrics {
        status: BigkeysScanStatus::Failed,
        last_error: Some(truncate_chars(message, TASK_ERROR_MAX_CHARS, "")),
        last_completed: Some(Instant::now()),
        ..previous
    }
}

fn hotkeys_failure(
    started: HotkeysMetrics,
    metric: HotkeysMetric,
    message: &str,
) -> HotkeysMetrics {
    HotkeysMetrics {
        status: HotkeysStatus::Failed,
        last_error: Some(truncate_chars(message, TASK_ERROR_MAX_CHARS, "")),
        selected_metric: Some(metric),
        tracking_active: false,
        started_at: None,
        finishes_at: None,
        last_completed: Some(Instant::now()),
        ..started
    }
}

fn bigkeys_requires_readonly(state: &InstanceState) -> bool {
    state.detail.cluster_enabled
        && matches!(state.detail.role.as_deref(), Some("slave" | "replica"))
}

pub(crate) fn error_details(message: String) -> ErrorDetails {
    ErrorDetails {
        summary: first_line(&message, 80),
        message,
    }
}

pub(crate) fn classify_error(error: &redis::RedisError) -> (Status, ErrorDetails) {
    let msg = error.to_string();
    let status = classify_error_status(error.code(), &msg, error.kind(), error.is_timeout());

    if status == Status::Protected {
        return (
            status,
            ErrorDetails {
                summary: "Redis protected mode denies remote connections".to_string(),
                message: msg,
            },
        );
    }

    (status, error_details(msg))
}

pub(crate) fn classify_error_status(
    code: Option<&str>,
    message: &str,
    kind: ErrorKind,
    is_timeout: bool,
) -> Status {
    let msg_lower = message.to_ascii_lowercase();

    if let Some(code) = code {
        if code == "DENIED" && msg_lower.contains("protected mode") {
            return Status::Protected;
        }
        if code == "NOAUTH" || code == "WRONGPASS" {
            return Status::Auth;
        }
        if code == "LOADING" {
            return Status::Loading;
        }
    }

    match kind {
        ErrorKind::AuthenticationFailed => Status::Auth,
        ErrorKind::Io if is_timeout => Status::Timeout,
        ErrorKind::Io => Status::Down,
        _ => Status::Error,
    }
}

async fn scan_bigkeys(
    conn: &mut impl redis::aio::ConnectionLike,
) -> redis::RedisResult<BigkeysMetrics> {
    let memory_usage_supported = detect_memory_usage_support(conn).await?;
    let mut cursor = 0u64;
    let mut largest_keys = Vec::new();

    loop {
        let (next_cursor, keys): (u64, Vec<String>) = redis::cmd("SCAN")
            .cursor_arg(cursor)
            .arg("COUNT")
            .arg(BIGKEYS_SCAN_COUNT)
            .query_async(conn)
            .await?;

        if !keys.is_empty() {
            let entries = fetch_bigkey_entries(conn, &keys, memory_usage_supported).await?;
            for entry in entries {
                insert_bigkey_entry(&mut largest_keys, entry);
            }
        }

        if next_cursor == 0 {
            break;
        }
        cursor = next_cursor;
    }

    Ok(BigkeysMetrics {
        status: BigkeysScanStatus::Ready,
        last_error: None,
        largest_keys,
        last_completed: Some(Instant::now()),
    })
}

async fn detect_memory_usage_support(
    conn: &mut impl redis::aio::ConnectionLike,
) -> redis::RedisResult<bool> {
    match redis::cmd("MEMORY")
        .arg("HELP")
        .query_async::<Value>(conn)
        .await
    {
        Ok(_) => Ok(true),
        Err(err) if is_unknown_command(&err) => Ok(false),
        Err(err) => Err(err),
    }
}

async fn fetch_bigkey_entries(
    conn: &mut impl redis::aio::ConnectionLike,
    keys: &[String],
    memory_usage_supported: bool,
) -> redis::RedisResult<Vec<BigkeyEntry>> {
    let mut type_pipe = redis::pipe();
    for key in keys {
        type_pipe.cmd("TYPE").arg(key);
    }
    let types: Vec<String> = type_pipe.query_async(conn).await?;

    let mut size_pipe = redis::pipe();
    size_pipe.ignore_errors();
    let mut size_indexes = Vec::new();
    for (idx, (key, key_type)) in keys.iter().zip(types.iter()).enumerate() {
        if let Some(command) = key_type_size_command(key_type) {
            size_pipe.cmd(command).arg(key);
            size_indexes.push(idx);
        }
    }
    let size_results = if size_indexes.is_empty() {
        Vec::new()
    } else {
        size_pipe
            .query_async::<Vec<redis::RedisResult<u64>>>(conn)
            .await?
    };

    let memory_results = if memory_usage_supported {
        let mut memory_pipe = redis::pipe();
        for key in keys {
            memory_pipe.cmd("MEMORY").arg("USAGE").arg(key);
        }
        memory_pipe.query_async::<Vec<Option<u64>>>(conn).await?
    } else {
        Vec::new()
    };

    let mut sizes = vec![None; keys.len()];
    for (result_idx, key_idx) in size_indexes.into_iter().enumerate() {
        sizes[key_idx] = size_results
            .get(result_idx)
            .and_then(|result| result.as_ref().ok().copied());
    }

    let mut entries = Vec::with_capacity(keys.len());
    for (idx, key) in keys.iter().enumerate() {
        let key_type = types
            .get(idx)
            .cloned()
            .unwrap_or_else(|| "unknown".to_string());
        if key_type == "none" {
            continue;
        }
        entries.push(BigkeyEntry {
            key: key.clone(),
            key_type,
            size: sizes[idx],
            memory_usage: memory_results.get(idx).copied().flatten(),
        });
    }
    Ok(entries)
}

fn key_type_size_command(key_type: &str) -> Option<&'static str> {
    match key_type {
        "string" => Some("STRLEN"),
        "list" => Some("LLEN"),
        "set" => Some("SCARD"),
        "zset" => Some("ZCARD"),
        "hash" => Some("HLEN"),
        "stream" => Some("XLEN"),
        _ => None,
    }
}

/// Keeps `entries` sorted and capped at the top N without re-sorting.
fn insert_bigkey_entry(entries: &mut Vec<BigkeyEntry>, entry: BigkeyEntry) {
    let position = entries.partition_point(|existing| bigkey_entry_cmp(existing, &entry).is_le());
    if position < BIGKEYS_TOP_N {
        entries.insert(position, entry);
        entries.truncate(BIGKEYS_TOP_N);
    }
}

fn bigkey_entry_cmp(left: &BigkeyEntry, right: &BigkeyEntry) -> std::cmp::Ordering {
    right
        .size
        .unwrap_or(0)
        .cmp(&left.size.unwrap_or(0))
        .then_with(|| {
            right
                .memory_usage
                .unwrap_or(0)
                .cmp(&left.memory_usage.unwrap_or(0))
        })
        .then_with(|| left.key_type.cmp(&right.key_type))
        .then_with(|| left.key.cmp(&right.key))
}

fn is_unknown_command(error: &redis::RedisError) -> bool {
    error
        .code()
        .is_some_and(|code| code.eq_ignore_ascii_case("ERR"))
        && error
            .to_string()
            .to_ascii_lowercase()
            .contains("unknown command")
}

pub(crate) fn apply_cluster_shards_to_state(
    state: &mut InstanceState,
    target: &Target,
    value: &Value,
) {
    let shards = parse_cluster_shards(value);
    if shards.is_empty() {
        return;
    }

    state.cluster_id = cluster_signature(&shards);

    let myself = shards.iter().find_map(|shard| {
        shard
            .nodes
            .iter()
            .find(|node| addresses_match(&node.addr, &target.addr))
            .map(|node| (shard, node))
    });

    let Some((shard, myself)) = myself else {
        return;
    };

    if myself.is_replica() {
        state.kind = InstanceType::Replica;
        state.parent_addr = shard
            .nodes
            .iter()
            .find(|node| node.is_primary())
            .map(|node| node.addr.clone());
    } else if myself.is_primary() {
        state.kind = InstanceType::Primary;
        state.parent_addr = None;
        state.slots.clone_from(&shard.slots);
    } else {
        state.kind = InstanceType::Cluster;
    }
}

fn cluster_signature(shards: &[ClusterShard]) -> Option<String> {
    let mut ids: Vec<&str> = shards
        .iter()
        .flat_map(|shard| shard.nodes.iter())
        .filter_map(|node| node.node_id.as_deref())
        .collect();
    ids.sort_unstable();
    ids.dedup();
    if let Some(id) = ids.first() {
        return Some((*id).to_string());
    }

    let mut addrs: Vec<&str> = shards
        .iter()
        .flat_map(|shard| shard.nodes.iter())
        .map(|node| node.addr.as_str())
        .collect();
    addrs.sort_unstable();
    addrs.dedup();
    addrs.first().map(|addr| (*addr).to_string())
}

fn addresses_match(left: &str, right: &str) -> bool {
    if left == right {
        return true;
    }

    let left_host = canonical_host(left);
    let right_host = canonical_host(right);
    let left_port = strip_host(left);
    let right_port = strip_host(right);
    left_host.is_some() && left_host == right_host && left_port == right_port
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use redis::{ErrorKind, Value};

    use super::{
        BIGKEYS_TOP_N, BigkeyEntry, apply_cluster_shards_to_state, apply_info_to_state,
        apply_timed_failure, bigkeys_requires_readonly, classify_error_status, cluster_signature,
        error_details, insert_bigkey_entry, key_type_size_command, target_supports_local_signal,
    };
    use crate::model::{
        DetailMetrics, InstanceState, InstanceType, SlotRange, Status, Target, TargetProtocol,
    };
    use crate::parse::{ClusterShard, ClusterShardNode, ClusterShardRole, parse_cluster_shards};

    fn idle_poller() -> (
        super::Poller,
        tokio::sync::mpsc::Receiver<super::PollerUpdate>,
        tokio::sync::mpsc::Receiver<super::TaskResult>,
    ) {
        let settings = crate::config::default_settings();
        let (update_tx, update_rx) = tokio::sync::mpsc::channel(16);
        let (task_tx, task_rx) = tokio::sync::mpsc::channel(16);
        let poller = super::Poller {
            semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
            settings,
            targets: std::collections::HashMap::new(),
            known_states: std::collections::HashMap::new(),
            tasks: std::collections::HashMap::new(),
            next_generation: 0,
            update_tx,
            task_tx,
        };
        (poller, update_rx, task_rx)
    }

    fn published_state(update: Option<super::PollerUpdate>) -> InstanceState {
        match update {
            Some(super::PollerUpdate::State(state)) => *state,
            _ => panic!("expected a published state"),
        }
    }

    #[tokio::test]
    async fn task_results_merge_into_the_newest_state() {
        use crate::model::{BigkeysMetrics, BigkeysScanStatus};

        let (mut poller, mut updates, mut task_results) = idle_poller();
        let mut state = InstanceState::new("a".into(), "127.0.0.1:6379".into());
        state.ops_per_sec = Some(1);
        assert!(poller.publish(state.clone()).await.is_ok());
        let _ = updates.recv().await;

        let scanned = BigkeysMetrics {
            status: BigkeysScanStatus::Ready,
            ..BigkeysMetrics::default()
        };
        let outcome = super::TaskOutcome::Bigkeys(scanned.clone());
        poller.spawn_task("a".into(), super::TaskKind::Bigkeys, None, async {
            outcome
        });

        // A refresh lands while the scan is running.
        state.ops_per_sec = Some(99);
        assert!(poller.publish(state).await.is_ok());
        let _ = updates.recv().await;

        let result = task_results.recv().await.expect("task reports back");
        assert!(poller.finish_task(result).await.is_ok());
        let merged = published_state(updates.recv().await);
        assert_eq!(merged.ops_per_sec, Some(99));
        assert_eq!(merged.detail.bigkeys, scanned);
        assert!(poller.tasks.is_empty());
    }

    #[tokio::test]
    async fn results_from_replaced_tasks_are_discarded() {
        use crate::model::BigkeysMetrics;

        let (mut poller, mut updates, mut task_results) = idle_poller();
        let state = InstanceState::new("a".into(), "127.0.0.1:6379".into());
        assert!(poller.publish(state).await.is_ok());
        let _ = updates.recv().await;

        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        poller.spawn_task("a".into(), super::TaskKind::Bigkeys, None, async {
            let _ = release_rx.await;
            super::TaskOutcome::Bigkeys(BigkeysMetrics::default())
        });
        let superseded = super::TaskResult {
            key: "a".into(),
            generation: poller.next_generation,
            outcome: super::TaskOutcome::Bigkeys(BigkeysMetrics::default()),
        };
        // Replacing the task aborts the first one and bumps the generation.
        poller.spawn_task("a".into(), super::TaskKind::Bigkeys, None, async {
            super::TaskOutcome::Bigkeys(BigkeysMetrics::default())
        });
        drop(release_tx);

        assert!(poller.finish_task(superseded).await.is_ok());
        assert!(updates.try_recv().is_err());
        let current = task_results.recv().await.expect("replacement reports back");
        assert!(poller.finish_task(current).await.is_ok());
        assert!(updates.recv().await.is_some());
    }

    #[tokio::test]
    async fn batch_actions_attempt_each_target_after_missing_targets_and_connection_errors() {
        let directory = tempfile::tempdir().expect("temporary socket directory");
        let targets: Vec<_> = ["first.sock", "second.sock"]
            .into_iter()
            .map(|name| Target {
                alias: None,
                addr: directory.path().join(name).to_string_lossy().into_owned(),
                protocol: TargetProtocol::Unix,
                username: None,
                password: None,
                tags: Vec::new(),
                process_id: None,
            })
            .collect();
        let keys: Vec<_> = targets.iter().map(|target| target.addr.clone()).collect();
        let mut settings = crate::config::default_settings();
        settings.refresh_interval = Duration::from_secs(3600);
        let (mut updates, requests) = super::start(targets, settings);

        // Drain the initial poll before checking updates produced by each action.
        for _ in &keys {
            let update = tokio::time::timeout(Duration::from_secs(2), updates.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(update, super::PollerUpdate::State(_)));
        }
        let mut batch = vec!["missing target".to_string()];
        batch.extend(keys.clone());
        requests
            .send(super::PollerRequest::AuthenticateTargets {
                keys: batch.clone(),
                username: Some("default".to_string()),
                password: "secret".to_string(),
            })
            .await
            .unwrap();
        for key in &keys {
            let update = tokio::time::timeout(Duration::from_secs(2), updates.recv())
                .await
                .unwrap()
                .unwrap();
            let super::PollerUpdate::State(state) = update else {
                panic!("authentication should report a state for each existing target");
            };
            assert_eq!(&state.key, key);
            assert_eq!(state.status, Status::Down);
        }
        requests
            .send(super::PollerRequest::KillTargets {
                keys: batch,
                action: crate::model::KillAction::ShutdownNosave,
            })
            .await
            .unwrap();
        for expected in keys {
            let update = tokio::time::timeout(Duration::from_secs(2), updates.recv())
                .await
                .unwrap()
                .unwrap();
            let super::PollerUpdate::Remove { key } = update else {
                panic!("stopped servers should be removed even after a prior target failed");
            };
            assert_eq!(key, expected);
        }
    }

    #[test]
    fn cluster_signature_is_stable_for_same_membership() {
        let shards_a = vec![ClusterShard {
            slots: Vec::new(),
            nodes: vec![
                ClusterShardNode {
                    node_id: Some("bbbb".to_string()),
                    addr: "127.0.0.1:6380".to_string(),
                    role: ClusterShardRole::Primary,
                },
                ClusterShardNode {
                    node_id: Some("aaaa".to_string()),
                    addr: "127.0.0.1:6379".to_string(),
                    role: ClusterShardRole::Primary,
                },
            ],
        }];
        let shards_b = vec![ClusterShard {
            slots: Vec::new(),
            nodes: vec![
                ClusterShardNode {
                    node_id: Some("aaaa".to_string()),
                    addr: "127.0.0.1:6379".to_string(),
                    role: ClusterShardRole::Primary,
                },
                ClusterShardNode {
                    node_id: Some("bbbb".to_string()),
                    addr: "127.0.0.1:6380".to_string(),
                    role: ClusterShardRole::Primary,
                },
            ],
        }];

        assert_eq!(cluster_signature(&shards_a), Some("aaaa".to_string()));
        assert_eq!(cluster_signature(&shards_a), cluster_signature(&shards_b));
    }

    #[test]
    fn cluster_signature_falls_back_to_addr_without_node_ids() {
        let response = Value::Array(vec![Value::Map(vec![(
            Value::BulkString(b"nodes".to_vec()),
            Value::Array(vec![
                Value::Map(vec![
                    (
                        Value::BulkString(b"endpoint".to_vec()),
                        Value::BulkString(b"10.0.0.2".to_vec()),
                    ),
                    (Value::BulkString(b"port".to_vec()), Value::Int(7001)),
                ]),
                Value::Map(vec![
                    (
                        Value::BulkString(b"endpoint".to_vec()),
                        Value::BulkString(b"10.0.0.1".to_vec()),
                    ),
                    (Value::BulkString(b"port".to_vec()), Value::Int(7000)),
                ]),
            ]),
        )])]);

        let shards = parse_cluster_shards(&response);
        assert_eq!(
            cluster_signature(&shards),
            Some("10.0.0.1:7000".to_string())
        );
    }

    fn two_shard_cluster_response() -> Value {
        let shard = |slots: Vec<i64>, primary: &str, replica: &str| {
            let node = |addr: &str, role: &str| {
                let (host, port) = addr.rsplit_once(':').expect("host:port");
                Value::Map(vec![
                    (
                        Value::BulkString(b"endpoint".to_vec()),
                        Value::BulkString(host.as_bytes().to_vec()),
                    ),
                    (
                        Value::BulkString(b"port".to_vec()),
                        Value::Int(port.parse().expect("port")),
                    ),
                    (
                        Value::BulkString(b"role".to_vec()),
                        Value::BulkString(role.as_bytes().to_vec()),
                    ),
                ])
            };
            Value::Map(vec![
                (
                    Value::BulkString(b"slots".to_vec()),
                    Value::Array(slots.into_iter().map(Value::Int).collect()),
                ),
                (
                    Value::BulkString(b"nodes".to_vec()),
                    Value::Array(vec![node(primary, "master"), node(replica, "replica")]),
                ),
            ])
        };

        Value::Array(vec![
            shard(vec![0, 8191], "10.0.0.1:7000", "10.0.0.3:7002"),
            shard(vec![8192, 16383], "10.0.0.2:7001", "10.0.0.4:7003"),
        ])
    }

    fn cluster_target(addr: &str) -> Target {
        Target {
            alias: None,
            addr: addr.to_string(),
            protocol: TargetProtocol::Tcp,
            username: None,
            password: None,
            tags: Vec::new(),
            process_id: None,
        }
    }

    #[test]
    fn cluster_primary_records_its_own_slot_ranges() {
        let response = two_shard_cluster_response();
        let target = cluster_target("10.0.0.2:7001");
        let mut state = InstanceState::new("node".into(), target.addr.clone());

        apply_cluster_shards_to_state(&mut state, &target, &response);

        assert_eq!(state.kind, InstanceType::Primary);
        assert_eq!(
            state.slots,
            vec![SlotRange {
                start: 8192,
                end: 16_383
            }]
        );
    }

    #[test]
    fn cluster_replica_leaves_slot_ranges_empty() {
        let response = two_shard_cluster_response();
        let target = cluster_target("10.0.0.3:7002");
        let mut state = InstanceState::new("node".into(), target.addr.clone());

        apply_cluster_shards_to_state(&mut state, &target, &response);

        assert_eq!(state.kind, InstanceType::Replica);
        assert_eq!(state.parent_addr.as_deref(), Some("10.0.0.1:7000"));
        assert_eq!(state.slots, Vec::new());
    }

    #[test]
    fn info_refresh_clears_stale_slot_ranges() {
        let mut state = InstanceState::new("node".into(), "10.0.0.2:7001".into());
        state.slots = vec![SlotRange {
            start: 0,
            end: 8191,
        }];

        apply_info_to_state(&mut state, "# Replication\r\nrole:master\r\n", None);

        assert_eq!(state.slots, Vec::new());
    }

    #[test]
    fn key_type_size_command_maps_supported_types() {
        assert_eq!(key_type_size_command("string"), Some("STRLEN"));
        assert_eq!(key_type_size_command("list"), Some("LLEN"));
        assert_eq!(key_type_size_command("set"), Some("SCARD"));
        assert_eq!(key_type_size_command("zset"), Some("ZCARD"));
        assert_eq!(key_type_size_command("hash"), Some("HLEN"));
        assert_eq!(key_type_size_command("stream"), Some("XLEN"));
        assert_eq!(key_type_size_command("module"), None);
    }

    #[test]
    fn bigkey_entry_list_keeps_largest_keys_only() {
        let mut entries = Vec::new();
        for idx in 0..=BIGKEYS_TOP_N {
            insert_bigkey_entry(
                &mut entries,
                BigkeyEntry {
                    key: format!("key{idx}"),
                    key_type: "string".into(),
                    size: Some(u64::try_from(idx).expect("non-negative")),
                    memory_usage: None,
                },
            );
        }

        assert_eq!(entries.len(), BIGKEYS_TOP_N);
        assert_eq!(
            entries.first().and_then(|entry| entry.size),
            Some(BIGKEYS_TOP_N as u64)
        );
        assert_eq!(entries.last().and_then(|entry| entry.size), Some(1));
    }

    #[test]
    fn cluster_replica_bigkeys_scan_enables_readonly() {
        let mut state = InstanceState::new("node".into(), "127.0.0.1:6379".into());
        state.detail = DetailMetrics {
            cluster_enabled: true,
            role: Some("replica".into()),
            ..DetailMetrics::default()
        };
        assert!(bigkeys_requires_readonly(&state));

        state.detail.role = Some("master".into());
        assert!(!bigkeys_requires_readonly(&state));

        state.detail.cluster_enabled = false;
        state.detail.role = Some("replica".into());
        assert!(!bigkeys_requires_readonly(&state));
    }

    #[test]
    fn classify_error_status_detects_protected_mode() {
        let status = classify_error_status(
            Some("DENIED"),
            "DENIED Redis is running in protected mode because protected mode is enabled.",
            ErrorKind::Server(redis::ServerErrorKind::ResponseError),
            false,
        );
        assert_eq!(status, Status::Protected);
    }

    #[test]
    fn classify_error_status_detects_auth_codes() {
        assert_eq!(
            classify_error_status(
                Some("NOAUTH"),
                "NOAUTH Authentication required.",
                ErrorKind::Server(redis::ServerErrorKind::ResponseError),
                false,
            ),
            Status::Auth
        );
        assert_eq!(
            classify_error_status(
                Some("WRONGPASS"),
                "WRONGPASS invalid password",
                ErrorKind::Io,
                false
            ),
            Status::Auth
        );
    }

    #[test]
    fn timed_out_failures_record_elapsed_latency() {
        let mut state = InstanceState::new("node".into(), "127.0.0.1:6379".into());
        state.max_latency_ms = 10.0;

        let start = Instant::now()
            .checked_sub(Duration::from_millis(125))
            .expect("start instant");
        apply_timed_failure(
            &mut state,
            Status::Timeout,
            error_details("timed out".into()),
            start,
        );

        assert_eq!(state.status, Status::Timeout);
        assert!(state.last_latency_ms.is_some());
        assert!(state.last_latency_ms.expect("timeout latency") >= 100.0);
        assert!(state.max_latency_ms >= 100.0);
    }

    #[test]
    fn non_timeout_failures_do_not_record_latency() {
        let mut state = InstanceState::new("node".into(), "127.0.0.1:6379".into());
        let start = Instant::now()
            .checked_sub(Duration::from_millis(125))
            .expect("start instant");

        apply_timed_failure(
            &mut state,
            Status::Down,
            error_details("connection refused".into()),
            start,
        );

        assert_eq!(state.status, Status::Down);
        assert_eq!(state.last_latency_ms, None);
        assert!(state.max_latency_ms.abs() < f64::EPSILON);
    }

    #[test]
    fn apply_info_to_state_parses_process_id() {
        let mut state = InstanceState::new("node".into(), "127.0.0.1:6379".into());

        super::apply_info_to_state(
            &mut state,
            "# Server\r\nredis_version:8.0.0\r\nprocess_id:4242\r\nuptime_in_seconds:12\r\n",
            None,
        );

        assert_eq!(state.detail.process_id, Some(4242));
    }

    #[test]
    fn local_signal_support_requires_local_targets() {
        assert!(target_supports_local_signal(&Target {
            alias: None,
            addr: "127.0.0.1:6379".into(),
            protocol: TargetProtocol::Tcp,
            username: None,
            password: None,
            tags: Vec::new(),
            process_id: None,
        }));
        assert!(target_supports_local_signal(&Target {
            alias: None,
            addr: "/tmp/redis.sock".into(),
            protocol: TargetProtocol::Unix,
            username: None,
            password: None,
            tags: Vec::new(),
            process_id: None,
        }));
        assert!(!target_supports_local_signal(&Target {
            alias: None,
            addr: "redis.example:6379".into(),
            protocol: TargetProtocol::Tcp,
            username: None,
            password: None,
            tags: Vec::new(),
            process_id: None,
        }));
    }
}
