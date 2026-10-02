use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use redis::{ErrorKind, Value};
use tokio::sync::{Semaphore, mpsc};

use crate::model::{InstanceState, InstanceType, RuntimeSettings, Status, Target, TargetProtocol};
use crate::parse::{ParsedInfo, collect_cluster_shard_addresses, parse_info};
use crate::poller::{
    apply_cluster_shards_to_state, apply_info_to_state, classify_error, error_details,
};
use crate::redis_connection;
use crate::target_addr::{canonical_host, is_loopback_host, strip_host, tcp_host, tcp_port};

const DISCOVERY_TAG: &str = "autodiscovered";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryTarget {
    pub host: String,
    pub username: Option<String>,
    pub password: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct CandidateCredentials {
    username: Option<String>,
    password: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct LocalProcessInventory {
    ports: BTreeSet<u16>,
    port_to_pid: HashMap<u16, u32>,
    unix_socket_to_pid: HashMap<String, u32>,
}

impl CandidateCredentials {
    fn merge_from_target(&mut self, target: &Target) {
        if self.username.is_none() {
            self.username.clone_from(&target.username);
        }
        if self.password.is_none() {
            self.password.clone_from(&target.password);
        }
    }

    fn apply_to_candidate(&self, candidate: &mut CandidateEndpoint) {
        if candidate.username.is_none() {
            candidate.username.clone_from(&self.username);
        }
        if candidate.password.is_none() {
            candidate.password.clone_from(&self.password);
        }
    }
}

impl DiscoveryTarget {
    pub fn localhost(username: Option<String>, password: Option<String>) -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            username,
            password,
        }
    }

    pub fn is_localhost(&self) -> bool {
        is_loopback_host(&self.host)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CandidateSource {
    SeedTarget,
    CuratedPorts,
    LocalListeningSockets,
    LocalProcesses,
    ClusterPeers,
    SentinelPeers,
    ReplicationPeers,
}

impl CandidateSource {
    pub const fn label(&self) -> &'static str {
        match self {
            Self::SeedTarget => "seed",
            Self::CuratedPorts => "ports",
            Self::LocalListeningSockets => "sockets",
            Self::LocalProcesses => "processes",
            Self::ClusterPeers => "cluster",
            Self::SentinelPeers => "sentinel",
            Self::ReplicationPeers => "replication",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CandidateEndpoint {
    pub host: String,
    pub port: u16,
    pub source: CandidateSource,
    pub username: Option<String>,
    pub password: Option<String>,
    pub process_id: Option<u32>,
}

impl CandidateEndpoint {
    pub fn addr(&self) -> String {
        if self.host.contains(':') && !self.host.starts_with('[') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    pub fn dedupe_key(&self) -> String {
        candidate_key(&self.host, self.port)
    }

    pub fn into_target(self) -> Target {
        Target {
            alias: None,
            addr: self.addr(),
            protocol: TargetProtocol::Tcp,
            username: self.username,
            password: self.password,
            tags: vec![DISCOVERY_TAG.to_string()],
            process_id: self.process_id,
        }
    }

    fn merge_from(&mut self, other: &Self) {
        if self.username.is_none() {
            self.username.clone_from(&other.username);
        }
        if self.password.is_none() {
            self.password.clone_from(&other.password);
        }
        if self.process_id.is_none() {
            self.process_id = other.process_id;
        }
    }
}

#[derive(Debug, Clone)]
pub struct VerifiedInstance {
    pub candidate: CandidateEndpoint,
    pub target: Target,
    pub state: InstanceState,
}

#[derive(Debug, Clone)]
pub struct VerificationFailure {
    pub candidate: CandidateEndpoint,
    pub status: Status,
    pub message: String,
    pub tls_required: bool,
}

#[derive(Debug, Clone)]
pub enum DiscoveryEvent {
    CandidateFound(CandidateEndpoint),
    CandidateSkipped(CandidateEndpoint),
    VerificationStarted(CandidateEndpoint),
    VerificationSucceeded(Box<VerifiedInstance>),
    VerificationFailed(Box<VerificationFailure>),
    TopologyExpansionAdded {
        from: CandidateEndpoint,
        count: usize,
    },
    Complete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoveryPhase {
    Idle,
    Running,
    Complete,
}

#[derive(Debug, Clone)]
pub struct DiscoveryStatus {
    pub phase: DiscoveryPhase,
    pub candidates_seen: usize,
    pub duplicates: usize,
    pub queued: usize,
    pub probing: usize,
    pub verified: usize,
    pub failed: usize,
    pub topology_candidates_added: usize,
    started_at: Option<Instant>,
}

impl Default for DiscoveryStatus {
    fn default() -> Self {
        Self {
            phase: DiscoveryPhase::Idle,
            candidates_seen: 0,
            duplicates: 0,
            queued: 0,
            probing: 0,
            verified: 0,
            failed: 0,
            topology_candidates_added: 0,
            started_at: None,
        }
    }
}

impl DiscoveryStatus {
    pub fn apply_event(&mut self, event: &DiscoveryEvent) {
        match event {
            DiscoveryEvent::CandidateFound(_) => {
                self.phase = DiscoveryPhase::Running;
                self.candidates_seen += 1;
                self.queued += 1;
                self.started_at.get_or_insert_with(Instant::now);
            }
            DiscoveryEvent::CandidateSkipped(_) => {
                self.phase = DiscoveryPhase::Running;
                self.duplicates += 1;
                self.started_at.get_or_insert_with(Instant::now);
            }
            DiscoveryEvent::VerificationStarted(_) => {
                self.phase = DiscoveryPhase::Running;
                self.queued = self.queued.saturating_sub(1);
                self.probing += 1;
                self.started_at.get_or_insert_with(Instant::now);
            }
            DiscoveryEvent::VerificationSucceeded(_) => {
                self.phase = DiscoveryPhase::Running;
                self.probing = self.probing.saturating_sub(1);
                self.verified += 1;
            }
            DiscoveryEvent::VerificationFailed(_) => {
                self.phase = DiscoveryPhase::Running;
                self.probing = self.probing.saturating_sub(1);
                self.failed += 1;
            }
            DiscoveryEvent::TopologyExpansionAdded { count, .. } => {
                self.phase = DiscoveryPhase::Running;
                self.topology_candidates_added += count;
            }
            DiscoveryEvent::Complete => {
                self.phase = DiscoveryPhase::Complete;
                self.queued = 0;
                self.probing = 0;
                self.started_at.get_or_insert_with(Instant::now);
            }
        }
    }

    pub fn footer_summary(&self) -> Option<String> {
        match self.phase {
            DiscoveryPhase::Idle | DiscoveryPhase::Complete => None,
            DiscoveryPhase::Running => Some(format!("{} Autodiscovering", self.spinner_frame())),
        }
    }

    fn spinner_frame(&self) -> char {
        const FRAMES: [char; 4] = ['|', '/', '-', '\\'];
        let Some(started_at) = self.started_at else {
            return FRAMES[0];
        };
        let ticks = started_at.elapsed().as_millis() / 120;
        let frame_count = u128::try_from(FRAMES.len()).unwrap_or(1);
        let idx = usize::try_from(ticks % frame_count).unwrap_or(0);
        FRAMES[idx]
    }
}

#[derive(Debug)]
enum ManagerMessage {
    Candidates(Vec<CandidateEndpoint>),
    SourceFinished,
    VerificationResult(Box<VerificationResult>),
}

#[derive(Debug)]
struct VerificationResult {
    verified: Option<VerifiedInstance>,
    failure: Option<VerificationFailure>,
    expanded_candidates: Vec<CandidateEndpoint>,
}

struct AdmissionContext {
    configured_credentials: HashMap<String, CandidateCredentials>,
    settings: RuntimeSettings,
    semaphore: std::sync::Arc<Semaphore>,
    event_tx: mpsc::Sender<DiscoveryEvent>,
    manager_tx: mpsc::Sender<ManagerMessage>,
}

#[derive(Default)]
struct CandidateQueue {
    seen: HashSet<String>,
    preferred: HashMap<String, CandidateEndpoint>,
    pending_verifications: usize,
}

impl CandidateQueue {
    /// Dedupes `candidates` and spawns a verifier for every endpoint we have
    /// not probed yet.
    ///
    /// Admission has to happen inline in the manager loop: routing topology
    /// expansions back through the message channel would let the loop observe
    /// `pending_verifications == 0` and finish before the expanded peers were
    /// ever queued.
    async fn admit(&mut self, candidates: Vec<CandidateEndpoint>, ctx: &AdmissionContext) {
        for mut candidate in candidates {
            if let Some(credentials) = ctx.configured_credentials.get(&candidate.dedupe_key()) {
                credentials.apply_to_candidate(&mut candidate);
            }

            let key = candidate.dedupe_key();
            if let Some(existing) = self.preferred.get_mut(&key) {
                existing.merge_from(&candidate);
                let _ = ctx
                    .event_tx
                    .send(DiscoveryEvent::CandidateSkipped(candidate))
                    .await;
                continue;
            }

            if !self.seen.insert(key.clone()) {
                let _ = ctx
                    .event_tx
                    .send(DiscoveryEvent::CandidateSkipped(candidate))
                    .await;
                continue;
            }

            self.preferred.insert(key, candidate.clone());
            let _ = ctx
                .event_tx
                .send(DiscoveryEvent::CandidateFound(candidate.clone()))
                .await;
            self.pending_verifications += 1;
            spawn_verifier(
                candidate,
                ctx.settings.clone(),
                ctx.semaphore.clone(),
                ctx.event_tx.clone(),
                ctx.manager_tx.clone(),
            );
        }
    }
}

pub fn start(
    discovery_targets: Vec<DiscoveryTarget>,
    discovery_seed_targets: Vec<Target>,
    configured_targets: Vec<Target>,
    settings: RuntimeSettings,
) -> mpsc::Receiver<DiscoveryEvent> {
    let (event_tx, event_rx) = mpsc::channel(1024);

    tokio::spawn(async move {
        let seed_candidates = discovery_seed_targets
            .into_iter()
            .filter(|target| target.protocol == TargetProtocol::Tcp)
            .filter_map(candidate_from_target)
            .collect::<Vec<_>>();

        let source_count = discovery_targets.len() + usize::from(!seed_candidates.is_empty());
        if source_count == 0 {
            return;
        }

        let (manager_tx, mut manager_rx) = mpsc::channel::<ManagerMessage>(1024);

        if !seed_candidates.is_empty() {
            let tx = manager_tx.clone();
            tokio::spawn(async move {
                let _ = tx.send(ManagerMessage::Candidates(seed_candidates)).await;
                let _ = tx.send(ManagerMessage::SourceFinished).await;
            });
        }

        for target in discovery_targets {
            let tx = manager_tx.clone();
            tokio::spawn(async move {
                let candidates = generate_candidates(target).await;
                let _ = tx.send(ManagerMessage::Candidates(candidates)).await;
                let _ = tx.send(ManagerMessage::SourceFinished).await;
            });
        }

        let concurrency_limit = settings.concurrency_limit.max(1);
        let ctx = AdmissionContext {
            configured_credentials: credential_map(&configured_targets),
            settings,
            semaphore: std::sync::Arc::new(Semaphore::new(concurrency_limit)),
            event_tx: event_tx.clone(),
            manager_tx,
        };

        let mut pending_sources = source_count;
        let mut queue = CandidateQueue::default();

        while let Some(message) = manager_rx.recv().await {
            match message {
                ManagerMessage::Candidates(candidates) => queue.admit(candidates, &ctx).await,
                ManagerMessage::SourceFinished => {
                    pending_sources = pending_sources.saturating_sub(1);
                }
                ManagerMessage::VerificationResult(result) => {
                    queue.pending_verifications = queue.pending_verifications.saturating_sub(1);
                    if let Some(verified) = result.verified {
                        let _ = event_tx
                            .send(DiscoveryEvent::VerificationSucceeded(Box::new(
                                verified.clone(),
                            )))
                            .await;
                        if !result.expanded_candidates.is_empty() {
                            let _ = event_tx
                                .send(DiscoveryEvent::TopologyExpansionAdded {
                                    from: verified.candidate.clone(),
                                    count: result.expanded_candidates.len(),
                                })
                                .await;
                            queue.admit(result.expanded_candidates, &ctx).await;
                        }
                    } else if let Some(failure) = result.failure {
                        let _ = event_tx
                            .send(DiscoveryEvent::VerificationFailed(Box::new(failure)))
                            .await;
                    }
                }
            }

            if pending_sources == 0 && queue.pending_verifications == 0 {
                let _ = event_tx.send(DiscoveryEvent::Complete).await;
                return;
            }
        }
    });

    event_rx
}

fn spawn_verifier(
    candidate: CandidateEndpoint,
    settings: RuntimeSettings,
    semaphore: std::sync::Arc<Semaphore>,
    event_tx: mpsc::Sender<DiscoveryEvent>,
    manager_tx: mpsc::Sender<ManagerMessage>,
) {
    tokio::spawn(async move {
        let _permit = semaphore.acquire_owned().await.ok();
        let _ = event_tx
            .send(DiscoveryEvent::VerificationStarted(candidate.clone()))
            .await;
        let result = verify_candidate(candidate, &settings).await;
        let _ = manager_tx
            .send(ManagerMessage::VerificationResult(Box::new(result)))
            .await;
    });
}

async fn generate_candidates(target: DiscoveryTarget) -> Vec<CandidateEndpoint> {
    let mut candidates = curated_ports()
        .into_iter()
        .map(|port| CandidateEndpoint {
            host: target.host.clone(),
            port,
            source: CandidateSource::CuratedPorts,
            username: target.username.clone(),
            password: target.password.clone(),
            process_id: None,
        })
        .collect::<Vec<_>>();

    if target.is_localhost() {
        let listening = tokio::task::spawn_blocking(local_listening_ports)
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        let processes = tokio::task::spawn_blocking(local_process_inventory)
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        candidates.extend(listening.into_iter().map(|port| CandidateEndpoint {
            host: target.host.clone(),
            port,
            source: CandidateSource::LocalListeningSockets,
            username: target.username.clone(),
            password: target.password.clone(),
            process_id: processes.port_to_pid.get(&port).copied(),
        }));
        candidates.extend(processes.ports.into_iter().map(|port| CandidateEndpoint {
            host: target.host.clone(),
            port,
            source: CandidateSource::LocalProcesses,
            username: target.username.clone(),
            password: target.password.clone(),
            process_id: processes.port_to_pid.get(&port).copied(),
        }));
    }

    dedupe_candidates(candidates)
}

#[allow(clippy::too_many_lines)]
async fn verify_candidate(
    candidate: CandidateEndpoint,
    settings: &RuntimeSettings,
) -> VerificationResult {
    let target = candidate.clone().into_target();
    let mut state = InstanceState::new(target.addr.clone(), target.addr.clone());
    state.addr = target.addr.clone();
    state.tags = target.tags.clone();
    state.detail.process_id = target.process_id;

    let discovery_command_timeout = settings.command_timeout.min(Duration::from_millis(500));
    let mut discovery_settings = settings.clone();
    discovery_settings.command_timeout = discovery_command_timeout;

    let connect_start = Instant::now();
    let mut conn = match redis_connection::connect(&target, &discovery_settings).await {
        Ok(conn) => conn,
        Err(err) => {
            let (status, details) = classify_error(&err);
            return VerificationResult {
                verified: None,
                failure: Some(VerificationFailure {
                    candidate,
                    status,
                    message: details.message.clone(),
                    tls_required: looks_like_tls_required(&details.message),
                }),
                expanded_candidates: Vec::new(),
            };
        }
    };
    let latency_ms = connect_start.elapsed().as_secs_f64() * 1000.0;

    let mut auth_required = false;
    let mut auth_error_message = None;
    let hello = redis::cmd("HELLO")
        .arg(3)
        .query_async::<Value>(&mut conn)
        .await;
    match hello {
        Ok(_) => {}
        Err(err) if is_auth_required(&err) => {
            auth_required = true;
            auth_error_message.get_or_insert_with(|| err.to_string());
        }
        Err(_) => match redis::cmd("PING").query_async::<String>(&mut conn).await {
            Ok(_) => {}
            Err(err) if is_auth_required(&err) => {
                auth_required = true;
                auth_error_message.get_or_insert_with(|| err.to_string());
            }
            Err(err) => {
                let (status, details) = classify_error(&err);
                return VerificationResult {
                    verified: None,
                    failure: Some(VerificationFailure {
                        candidate,
                        status,
                        message: details.message.clone(),
                        tls_required: looks_like_tls_required(&details.message),
                    }),
                    expanded_candidates: Vec::new(),
                };
            }
        },
    }

    state.push_latency_sample(latency_ms);
    state.last_latency_ms = Some(latency_ms);
    state.last_updated = Some(Instant::now());

    if auth_required {
        let details = auth_error_message.map_or_else(
            || error_details("authentication required".to_string()),
            error_details,
        );
        state.status = Status::Auth;
        state.last_error = Some(details.summary.clone());
        state.error_details = Some(details);
        return VerificationResult {
            verified: Some(VerifiedInstance {
                candidate: candidate.clone(),
                target,
                state,
            }),
            failure: None,
            expanded_candidates: Vec::new(),
        };
    }

    let mut is_sentinel = false;
    let info_raw = redis::cmd("INFO")
        .query_async::<String>(&mut conn)
        .await
        .ok();
    let parsed_info = info_raw.as_deref().map(parse_info);
    if let Some(info) = &info_raw {
        apply_info_to_state(&mut state, info, None);
        is_sentinel = parsed_info
            .as_ref()
            .and_then(|parsed| parsed.get("server", "redis_mode"))
            .is_some_and(|mode| mode.eq_ignore_ascii_case("sentinel"));
    }

    state.status = Status::Ok;
    state.last_error = None;
    state.error_details = None;

    let mut expanded_candidates = Vec::new();
    if is_sentinel {
        state.kind = InstanceType::Standalone;
        state.detail.role = Some("sentinel".to_string());
        state
            .info
            .insert("redis_mode".to_string(), "sentinel".to_string());
        expanded_candidates.extend(discover_sentinel_peers(&mut conn, &candidate).await);
    } else {
        if state.detail.cluster_enabled
            && let Ok(shards) = redis::cmd("CLUSTER")
                .arg("SHARDS")
                .query_async::<Value>(&mut conn)
                .await
        {
            apply_cluster_shards_to_state(&mut state, &target, &shards);
            expanded_candidates.extend(
                collect_cluster_shard_addresses(&shards)
                    .into_iter()
                    .filter_map(|addr| {
                        candidate_from_addr(&addr, CandidateSource::ClusterPeers, &candidate)
                    }),
            );
        }

        if let Some(parsed_info) = &parsed_info {
            expanded_candidates.extend(replication_candidates(parsed_info, &candidate));
        }
    }

    VerificationResult {
        verified: Some(VerifiedInstance {
            candidate,
            target,
            state: state.clone(),
        }),
        failure: None,
        expanded_candidates: dedupe_candidates(expanded_candidates),
    }
}

async fn discover_sentinel_peers(
    conn: &mut impl redis::aio::ConnectionLike,
    seed: &CandidateEndpoint,
) -> Vec<CandidateEndpoint> {
    let masters = redis::cmd("SENTINEL")
        .arg("MASTERS")
        .query_async::<Vec<HashMap<String, String>>>(conn)
        .await
        .unwrap_or_default();
    let mut out = Vec::new();

    for master in masters {
        out.extend(candidate_from_sentinel_map(
            &master,
            CandidateSource::SentinelPeers,
            seed,
        ));
        let Some(name) = master.get("name") else {
            continue;
        };
        out.extend(discover_sentinel_named_peers(conn, "REPLICAS", name, seed).await);
        out.extend(discover_sentinel_named_peers(conn, "SLAVES", name, seed).await);
        out.extend(discover_sentinel_named_peers(conn, "SENTINELS", name, seed).await);
    }

    dedupe_candidates(out)
}

async fn discover_sentinel_named_peers(
    conn: &mut impl redis::aio::ConnectionLike,
    subcommand: &str,
    name: &str,
    seed: &CandidateEndpoint,
) -> Vec<CandidateEndpoint> {
    redis::cmd("SENTINEL")
        .arg(subcommand)
        .arg(name)
        .query_async::<Vec<HashMap<String, String>>>(conn)
        .await
        .unwrap_or_default()
        .into_iter()
        .flat_map(|entry| candidate_from_sentinel_map(&entry, CandidateSource::SentinelPeers, seed))
        .collect()
}

fn candidate_from_sentinel_map(
    map: &HashMap<String, String>,
    source: CandidateSource,
    seed: &CandidateEndpoint,
) -> Vec<CandidateEndpoint> {
    map.get("ip")
        .and_then(|host| {
            map.get("port")
                .and_then(|port| port.parse::<u16>().ok())
                .map(|port| CandidateEndpoint {
                    host: host.clone(),
                    port,
                    source,
                    username: seed.username.clone(),
                    password: seed.password.clone(),
                    process_id: None,
                })
        })
        .into_iter()
        .collect()
}

fn replication_candidates(info: &ParsedInfo, seed: &CandidateEndpoint) -> Vec<CandidateEndpoint> {
    let Some(replication) = info.sections.get("replication") else {
        return Vec::new();
    };

    let mut out = Vec::new();
    if let (Some(host), Some(port)) = (
        replication.get("master_host"),
        replication
            .get("master_port")
            .and_then(|port| port.parse::<u16>().ok()),
    ) {
        out.push(CandidateEndpoint {
            host: host.clone(),
            port,
            source: CandidateSource::ReplicationPeers,
            username: seed.username.clone(),
            password: seed.password.clone(),
            process_id: None,
        });
    }

    out.extend(replication.iter().filter_map(|(key, value)| {
        key.starts_with("slave").then_some(value).and_then(|raw| {
            let mut host = None;
            let mut port = None;
            for field in raw.split(',') {
                let (field_key, field_value) = field.split_once('=')?;
                match field_key {
                    "ip" => host = Some(field_value.to_string()),
                    "port" => port = field_value.parse::<u16>().ok(),
                    _ => {}
                }
            }
            Some(CandidateEndpoint {
                host: host?,
                port: port?,
                source: CandidateSource::ReplicationPeers,
                username: seed.username.clone(),
                password: seed.password.clone(),
                process_id: None,
            })
        })
    }));

    dedupe_candidates(out)
}

fn candidate_from_target(target: Target) -> Option<CandidateEndpoint> {
    Some(CandidateEndpoint {
        host: tcp_host(&target.addr)?,
        port: strip_host(&target.addr)?.parse::<u16>().ok()?,
        source: CandidateSource::SeedTarget,
        username: target.username,
        password: target.password,
        process_id: target.process_id,
    })
}

fn candidate_from_addr(
    addr: &str,
    source: CandidateSource,
    seed: &CandidateEndpoint,
) -> Option<CandidateEndpoint> {
    Some(CandidateEndpoint {
        host: tcp_host(addr)?,
        port: strip_host(addr)?.parse::<u16>().ok()?,
        source,
        username: seed.username.clone(),
        password: seed.password.clone(),
        process_id: None,
    })
}

fn credential_map(targets: &[Target]) -> HashMap<String, CandidateCredentials> {
    let mut out = HashMap::new();
    for target in targets {
        let Some(key) = tcp_target_key(target) else {
            continue;
        };
        out.entry(key)
            .and_modify(|existing: &mut CandidateCredentials| existing.merge_from_target(target))
            .or_insert_with(|| CandidateCredentials {
                username: target.username.clone(),
                password: target.password.clone(),
            });
    }
    out
}

fn tcp_target_key(target: &Target) -> Option<String> {
    if target.protocol != TargetProtocol::Tcp {
        return None;
    }

    let host = tcp_host(&target.addr)?;
    let port = tcp_port(&target.addr)?;
    Some(candidate_key(&host, port))
}

fn candidate_key(host: &str, port: u16) -> String {
    let normalized_host = if is_loopback_host(host) {
        "127.0.0.1".to_string()
    } else {
        let addr = if host.contains(':') && !host.starts_with('[') {
            format!("[{host}]:{port}")
        } else {
            format!("{host}:{port}")
        };
        canonical_host(&addr).unwrap_or_else(|| host.to_ascii_lowercase())
    };
    format!("{normalized_host}:{port}")
}

fn curated_ports() -> BTreeSet<u16> {
    let mut ports = BTreeSet::new();
    ports.insert(6379);
    ports.insert(26379);
    ports.extend(6380..=6389);
    ports.extend(7000..=7099);
    ports.extend(26380..=26389);
    ports
}

fn dedupe_candidates(input: Vec<CandidateEndpoint>) -> Vec<CandidateEndpoint> {
    let mut by_key = HashMap::new();
    for candidate in input {
        by_key
            .entry(candidate.dedupe_key())
            .and_modify(|existing: &mut CandidateEndpoint| existing.merge_from(&candidate))
            .or_insert(candidate);
    }
    let mut out: Vec<_> = by_key.into_values().collect();
    out.sort_by_key(CandidateEndpoint::addr);
    out
}

fn local_listening_ports() -> Option<BTreeSet<u16>> {
    let mut ports = BTreeSet::new();
    ports.extend(parse_proc_net_listening_ports("/proc/net/tcp")?);
    ports.extend(parse_proc_net_listening_ports("/proc/net/tcp6").unwrap_or_default());
    Some(ports)
}

fn parse_proc_net_listening_ports(path: &str) -> Option<BTreeSet<u16>> {
    let content = fs::read_to_string(path).ok()?;
    let mut ports = BTreeSet::new();
    for line in content.lines().skip(1) {
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.len() < 4 || fields[3] != "0A" {
            continue;
        }
        let Some((host_hex, port_hex)) = fields[1].split_once(':') else {
            continue;
        };
        if !is_loopback_or_any_listener(host_hex) {
            continue;
        }
        let Ok(port) = u16::from_str_radix(port_hex, 16) else {
            continue;
        };
        ports.insert(port);
    }
    Some(ports)
}

fn is_loopback_or_any_listener(host_hex: &str) -> bool {
    matches!(
        host_hex,
        "00000000"
            | "0100007F"
            | "00000000000000000000000000000000"
            | "00000000000000000000000001000000"
    )
}

fn local_process_inventory() -> Option<LocalProcessInventory> {
    let mut inventory = LocalProcessInventory::default();
    for entry in fs::read_dir("/proc").ok()? {
        let Ok(entry) = entry else {
            continue;
        };
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        if !file_name.chars().all(|ch| ch.is_ascii_digit()) {
            continue;
        }
        let Ok(pid) = file_name.parse::<u32>() else {
            continue;
        };
        let path = entry.path().join("cmdline");
        let Some(tokens) = read_cmdline_tokens(&path) else {
            continue;
        };
        if !tokens.iter().any(|token| {
            token.contains("redis-server")
                || token.contains("valkey-server")
                || token.contains("redis-sentinel")
        }) {
            continue;
        }
        for port in extract_ports_from_cmdline(&tokens) {
            inventory.ports.insert(port);
            inventory.port_to_pid.entry(port).or_insert(pid);
        }
        for socket in extract_unix_sockets_from_cmdline(&tokens) {
            inventory.unix_socket_to_pid.entry(socket).or_insert(pid);
        }
    }
    Some(inventory)
}

fn read_cmdline_tokens(path: &Path) -> Option<Vec<String>> {
    let bytes = fs::read(path).ok()?;
    let tokens = bytes
        .split(|byte| *byte == 0)
        .filter(|segment| !segment.is_empty())
        .map(|segment| String::from_utf8_lossy(segment).to_string())
        .collect::<Vec<_>>();
    (!tokens.is_empty()).then_some(tokens)
}

fn extract_ports_from_cmdline(tokens: &[String]) -> BTreeSet<u16> {
    let mut ports = BTreeSet::new();
    let mut iter = tokens.iter().peekable();
    while let Some(token) = iter.next() {
        if matches!(token.as_str(), "--port" | "--tls-port" | "--sentinel-port")
            && let Some(next) = iter.peek()
            && let Ok(port) = next.parse::<u16>()
        {
            ports.insert(port);
            continue;
        }

        if let Ok(port) = token.parse::<u16>() {
            ports.insert(port);
            continue;
        }

        if let Some(port) = token
            .rsplit_once(':')
            .and_then(|(_, port)| port.parse::<u16>().ok())
        {
            ports.insert(port);
        }
    }
    ports
}

fn extract_unix_sockets_from_cmdline(tokens: &[String]) -> BTreeSet<String> {
    let mut sockets = BTreeSet::new();
    let mut iter = tokens.iter().peekable();
    while let Some(token) = iter.next() {
        if token == "--unixsocket"
            && let Some(next) = iter.peek()
        {
            sockets.insert((*next).clone());
            continue;
        }

        if let Some(path) = token.strip_prefix("unixsocket:") {
            sockets.insert(path.to_string());
            continue;
        }

        if token.contains('/') {
            sockets.insert(token.clone());
        }
    }
    sockets
}

pub(crate) fn local_process_id_for_tcp_port(port: u16) -> Option<u32> {
    local_process_inventory()?.port_to_pid.get(&port).copied()
}

pub(crate) fn local_process_id_for_unix_socket(path: &str) -> Option<u32> {
    local_process_inventory()?
        .unix_socket_to_pid
        .get(path)
        .copied()
}

fn is_auth_required(error: &redis::RedisError) -> bool {
    error
        .code()
        .is_some_and(|code| matches!(code, "NOAUTH" | "WRONGPASS"))
        || error.kind() == ErrorKind::AuthenticationFailed
}

fn looks_like_tls_required(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("tls") || lower.contains("ssl")
}

#[cfg(test)]
mod tests {
    use super::{
        CandidateEndpoint, CandidateSource, DiscoveryEvent, DiscoveryPhase, DiscoveryStatus,
        DiscoveryTarget, credential_map, dedupe_candidates, extract_ports_from_cmdline,
        parse_proc_net_listening_ports, replication_candidates,
    };
    use crate::model::{Target, TargetProtocol};
    use crate::parse::parse_info;

    #[test]
    fn localhost_detection_handles_names_and_ips() {
        use crate::target_addr::is_loopback_host;

        assert!(is_loopback_host("localhost"));
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("::1"));
        assert!(!is_loopback_host("192.168.1.10"));
    }

    #[test]
    fn dedupe_merges_credentials_for_same_endpoint() {
        let deduped = dedupe_candidates(vec![
            CandidateEndpoint {
                host: "127.0.0.1".to_string(),
                port: 6379,
                source: CandidateSource::CuratedPorts,
                username: None,
                password: None,
                process_id: None,
            },
            CandidateEndpoint {
                host: "127.0.0.1".to_string(),
                port: 6379,
                source: CandidateSource::SeedTarget,
                username: Some("alice".to_string()),
                password: Some("secret".to_string()),
                process_id: None,
            },
        ]);

        assert_eq!(deduped.len(), 1);
        assert_eq!(deduped[0].username.as_deref(), Some("alice"));
        assert_eq!(deduped[0].password.as_deref(), Some("secret"));
    }

    #[test]
    fn credential_map_matches_localhost_aliases_by_port() {
        let targets = vec![Target {
            alias: None,
            addr: "localhost:6380".to_string(),
            protocol: TargetProtocol::Tcp,
            username: Some("alice".to_string()),
            password: Some("secret".to_string()),
            tags: Vec::new(),
            process_id: None,
        }];

        let mapped = credential_map(&targets);
        let credentials = mapped
            .get("127.0.0.1:6380")
            .expect("localhost config should canonicalize to loopback");

        assert_eq!(credentials.username.as_deref(), Some("alice"));
        assert_eq!(credentials.password.as_deref(), Some("secret"));
    }

    #[test]
    fn cmdline_port_parser_extracts_explicit_and_addr_ports() {
        let tokens = vec![
            "redis-server".to_string(),
            "--port".to_string(),
            "6379".to_string(),
            "127.0.0.1:6380".to_string(),
        ];
        let ports = extract_ports_from_cmdline(&tokens);
        assert!(ports.contains(&6379));
        assert!(ports.contains(&6380));
    }

    #[test]
    fn replication_candidate_parser_finds_master_and_replicas() {
        let info = parse_info(
            "# Replication\nrole:master\nslave0:ip=10.0.0.2,port=6380,state=online\nmaster_host:10.0.0.1\nmaster_port:6379\n",
        );
        let seed = CandidateEndpoint {
            host: "10.0.0.1".to_string(),
            port: 6379,
            source: CandidateSource::SeedTarget,
            username: None,
            password: None,
            process_id: None,
        };

        let candidates = replication_candidates(&info, &seed);
        assert_eq!(candidates.len(), 2);
    }

    #[test]
    fn discovery_status_tracks_progress() {
        let candidate = CandidateEndpoint {
            host: "127.0.0.1".to_string(),
            port: 6379,
            source: CandidateSource::CuratedPorts,
            username: None,
            password: None,
            process_id: None,
        };
        let mut status = DiscoveryStatus::default();
        status.apply_event(&DiscoveryEvent::CandidateFound(candidate.clone()));
        status.apply_event(&DiscoveryEvent::VerificationStarted(candidate.clone()));
        status.apply_event(&DiscoveryEvent::CandidateSkipped(candidate));
        status.apply_event(&DiscoveryEvent::Complete);

        assert_eq!(status.phase, DiscoveryPhase::Complete);
        assert_eq!(status.candidates_seen, 1);
        assert_eq!(status.duplicates, 1);
        assert_eq!(status.footer_summary(), None);
    }

    #[test]
    fn discovery_status_footer_summary_only_shows_while_running() {
        let mut status = DiscoveryStatus::default();
        assert_eq!(status.footer_summary(), None);

        status.phase = DiscoveryPhase::Running;
        let summary = status
            .footer_summary()
            .expect("running discovery should show footer text");
        assert!(summary.contains("Autodiscovering"));
    }

    #[test]
    fn discovery_target_localhost_constructor_sets_loopback() {
        let target = DiscoveryTarget::localhost(None, None);
        assert!(target.is_localhost());
    }

    #[test]
    fn proc_net_parser_handles_listening_entries() {
        let path = tempfile::NamedTempFile::new().expect("tempfile should work");
        std::fs::write(
            path.path(),
            "  sl  local_address rem_address st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   0: 0100007F:18EB 00000000:0000 0A 00000000:00000000 00:00000000 00000000   100        0 0 1 0000000000000000 100 0 0 10 0\n",
        )
        .expect("write temp proc file");
        let ports =
            parse_proc_net_listening_ports(path.path().to_str().expect("temp path must be utf8"))
                .expect("parser should return ports");
        assert!(ports.contains(&6379));
    }
}
