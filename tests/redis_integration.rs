use std::collections::HashSet;
use std::env;
use std::time::Duration;

use redis::AsyncCommands;
use rtop::cluster::discover_cluster_targets;
use rtop::discovery::{self, DiscoveryEvent};
use rtop::hotkeys::{HotkeysMetric, HotkeysStatus};
use rtop::model::{
    BigkeysScanStatus, RuntimeSettings, SortMode, Target, TargetProtocol, UiTheme, ViewMode,
};
use rtop::poller::{PollerRequest, PollerUpdate, start};
use rtop::target_addr::tcp_endpoint_identity;
use tokio::sync::mpsc;
use tokio::time::timeout;

fn runtime_settings() -> RuntimeSettings {
    RuntimeSettings {
        credential_store: None,
        insecure_tls: Default::default(),
        refresh_interval: Duration::from_secs(60),
        connect_timeout: Duration::from_millis(300),
        command_timeout: Duration::from_secs(2),
        concurrency_limit: 2,
        leave_killed_servers: false,
        show_activity: true,
        default_view: ViewMode::Flat,
        default_sort: SortMode::Address,
        ui_theme: UiTheme::default(),
    }
}

#[cfg(unix)]
struct Server(std::process::Child);

#[cfg(unix)]
impl Drop for Server {
    fn drop(&mut self) {
        self.0.kill().expect("stop test Redis");
        self.0.wait().expect("reap test Redis");
    }
}

/// Dedicated temporary servers: this test must never reset a user's Redis statistics.
#[cfg(unix)]
#[tokio::test]
async fn reset_stats_attempts_each_selected_node_and_refreshes_commandstats() {
    use std::process::{Command, Stdio};

    let dir = tempfile::tempdir().unwrap();
    let mut servers = Vec::new();
    let mut targets = Vec::new();
    for index in 0..3 {
        let path = dir.path().join(format!("redis-{index}.sock"));
        let mut command = Command::new("redis-server");
        command
            .args([
                "--port",
                "0",
                "--save",
                "",
                "--appendonly",
                "no",
                "--unixsocket",
            ])
            .arg(&path)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if index == 0 {
            command.args(["--rename-command", "CONFIG", ""]);
        }
        let child = match command.spawn() {
            Ok(child) => child,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                skip_unreachable(
                    "isolated reset statistics (redis-server not installed)",
                    &error.to_string(),
                );
                return;
            }
            Err(error) => panic!("start test Redis: {error}"),
        };
        servers.push(Server(child));
        let target = Target {
            addr: path.to_string_lossy().into_owned(),
            protocol: TargetProtocol::Unix,
            ..standalone_target()
        };
        let client = redis::Client::open(format!("redis+unix://{}", target.addr)).unwrap();
        let mut conn = timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(conn) = client.get_multiplexed_async_connection().await {
                    break conn;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("test Redis ready");
        redis::cmd("ECHO")
            .arg("before reset")
            .query_async::<String>(&mut conn)
            .await
            .unwrap();
        targets.push(target);
    }

    let (mut updates, requests) = start(targets.clone(), runtime_settings());
    for _ in &targets {
        let update = timeout(Duration::from_secs(5), updates.recv())
            .await
            .unwrap()
            .unwrap();
        let PollerUpdate::State(state) = update else {
            panic!("initial state");
        };
        assert!(
            state
                .detail
                .commandstats
                .iter()
                .any(|stat| stat.command == "echo")
        );
    }
    requests
        .send(PollerRequest::ResetStats {
            keys: targets[..2]
                .iter()
                .map(|target| target.addr.clone())
                .collect(),
        })
        .await
        .unwrap();
    for (index, target) in targets[..2].iter().enumerate() {
        let state = recv_state(&mut updates, &target.addr).await;
        assert_eq!(state.status, rtop::model::Status::Ok);
        assert_eq!(
            state
                .detail
                .commandstats
                .iter()
                .any(|stat| stat.command == "echo"),
            index == 0
        );
    }
    let update = timeout(Duration::from_secs(5), updates.recv())
        .await
        .unwrap()
        .unwrap();
    let PollerUpdate::ResetStatsComplete { results } = update else {
        panic!("reset results");
    };
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].0, targets[0].addr);
    assert!(results[0].1.is_err());
    assert_eq!(results[1], (targets[1].addr.clone(), Ok(())));

    // The third, unselected node retains its statistics.
    let client = redis::Client::open(format!("redis+unix://{}", targets[2].addr)).unwrap();
    let mut conn = client.get_multiplexed_async_connection().await.unwrap();
    let info = redis::cmd("INFO")
        .arg("COMMANDSTATS")
        .query_async::<String>(&mut conn)
        .await
        .unwrap();
    assert!(info.contains("cmdstat_echo:"));
    drop(requests);
}

fn standalone_target() -> Target {
    Target {
        alias: Some("standalone".to_string()),
        addr: env::var("RTOP_TEST_REDIS_ADDR").unwrap_or_else(|_| "localhost:6379".to_string()),
        protocol: TargetProtocol::Tcp,
        username: None,
        password: None,
        tags: Vec::new(),
        process_id: None,
    }
}

fn cluster_target() -> Target {
    Target {
        alias: Some("cluster".to_string()),
        addr: env::var("RTOP_TEST_REDIS_CLUSTER_ADDR")
            .unwrap_or_else(|_| "localhost:7000".to_string()),
        protocol: TargetProtocol::Tcp,
        username: None,
        password: None,
        tags: Vec::new(),
        process_id: None,
    }
}

async fn recv_state(
    update_rx: &mut mpsc::Receiver<PollerUpdate>,
    key: &str,
) -> rtop::model::InstanceState {
    recv_state_with_timeout(update_rx, key, Duration::from_secs(5)).await
}

async fn recv_state_with_timeout(
    update_rx: &mut mpsc::Receiver<PollerUpdate>,
    key: &str,
    wait: Duration,
) -> rtop::model::InstanceState {
    loop {
        let update = timeout(wait, update_rx.recv())
            .await
            .expect("timed out waiting for poller update")
            .expect("poller update channel closed unexpectedly");
        match update {
            PollerUpdate::State(state) if state.key == key => return *state,
            PollerUpdate::State(_)
            | PollerUpdate::Remove { .. }
            | PollerUpdate::ResetStatsComplete { .. }
            | PollerUpdate::Clients { .. }
            | PollerUpdate::ClientsKilled { .. } => {}
        }
    }
}

fn skip_unreachable(label: &str, err: &str) {
    eprintln!("skipping {label} integration test: {err}");
}

fn redis_url_for_target(target: &Target) -> String {
    format!("redis://{}/", target.addr)
}

async fn is_reachable(target: &Target) -> bool {
    let Ok(client) = redis::Client::open(redis_url_for_target(target)) else {
        return false;
    };
    timeout(
        Duration::from_secs(1),
        client.get_multiplexed_async_connection(),
    )
    .await
    .is_ok_and(|conn| conn.is_ok())
}

async fn generate_hotkeys_traffic(target: &Target) -> redis::RedisResult<()> {
    let client = redis::Client::open(redis_url_for_target(target))?;
    let mut conn = client.get_multiplexed_async_connection().await?;
    for idx in 0..250 {
        let key = format!("rtop:hotkeys:{idx}");
        let value = format!("value-{idx}");
        let _: () = conn.set(&key, &value).await?;
        let _: String = conn.get(&key).await?;
    }
    Ok(())
}

#[tokio::test]
async fn standalone_poll_and_bigkeys_scan_work_against_live_redis() {
    let target = standalone_target();
    let settings = runtime_settings();
    let (mut update_rx, request_tx) = start(vec![target.clone()], settings);

    let state = recv_state(&mut update_rx, &target.addr).await;
    if state.status != rtop::model::Status::Ok {
        skip_unreachable(
            "standalone redis",
            state.last_error.as_deref().unwrap_or("poll failed"),
        );
        return;
    }

    request_tx
        .send(PollerRequest::RefreshBigkeys {
            key: target.addr.clone(),
            force: true,
        })
        .await
        .expect("failed to request bigkeys refresh");

    let running = recv_state(&mut update_rx, &target.addr).await;
    assert_eq!(running.detail.bigkeys.status, BigkeysScanStatus::Running);

    let scanned = recv_state(&mut update_rx, &target.addr).await;
    assert_eq!(scanned.detail.bigkeys.status, BigkeysScanStatus::Ready);
    assert!(scanned.detail.bigkeys.last_error.is_none());
}

#[tokio::test]
async fn cluster_discovery_and_bigkeys_scan_work_against_live_cluster() {
    let seed = cluster_target();
    let settings = runtime_settings();

    let discovered = match discover_cluster_targets(std::slice::from_ref(&seed), &settings).await {
        Ok(discovered) => discovered,
        Err(err) => {
            skip_unreachable("redis cluster discovery", &err.to_string());
            return;
        }
    };
    assert!(!discovered.is_empty());

    let (mut update_rx, request_tx) = start(vec![seed.clone()], settings);
    let state = recv_state(&mut update_rx, &seed.addr).await;
    if state.status != rtop::model::Status::Ok {
        skip_unreachable(
            "redis cluster poll",
            state.last_error.as_deref().unwrap_or("poll failed"),
        );
        return;
    }

    request_tx
        .send(PollerRequest::RefreshBigkeys {
            key: seed.addr.clone(),
            force: true,
        })
        .await
        .expect("failed to request cluster bigkeys refresh");

    let running = recv_state(&mut update_rx, &seed.addr).await;
    assert_eq!(running.detail.bigkeys.status, BigkeysScanStatus::Running);

    let scanned = recv_state(&mut update_rx, &seed.addr).await;
    assert_eq!(
        scanned.detail.bigkeys.status,
        BigkeysScanStatus::Ready,
        "cluster bigkeys scan failed: {:?}",
        scanned.detail.bigkeys.last_error
    );
    assert!(scanned.detail.bigkeys.last_error.is_none());
}

// Redis permits only one tracking session per server, even across connections.
static HOTKEYS_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
async fn standalone_hotkeys_sampling_works_against_live_redis() {
    let _guard = HOTKEYS_TEST_LOCK.lock().await;
    let target = standalone_target();
    let settings = runtime_settings();
    let (mut update_rx, request_tx) = start(vec![target.clone()], settings);

    let state = recv_state(&mut update_rx, &target.addr).await;
    if state.status != rtop::model::Status::Ok {
        skip_unreachable(
            "standalone redis",
            state.last_error.as_deref().unwrap_or("poll failed"),
        );
        return;
    }

    if state.detail.hotkeys_supported == Some(false) {
        skip_unreachable("standalone hotkeys", "server does not provide HOTKEYS");
        return;
    }

    for metric in [HotkeysMetric::Cpu, HotkeysMetric::Net] {
        request_tx
            .send(PollerRequest::StartHotkeys {
                key: target.addr.clone(),
                metric,
                force: true,
            })
            .await
            .expect("failed to request hotkeys sampling");

        let running = recv_state(&mut update_rx, &target.addr).await;
        assert_eq!(running.detail.hotkeys.status, HotkeysStatus::Running);
        assert_eq!(running.detail.hotkeys.selected_metric, Some(metric));

        if let Err(err) = generate_hotkeys_traffic(&target).await {
            skip_unreachable("hotkeys traffic generation", &err.to_string());
            return;
        }

        let wait = rtop::poller::HOTKEYS_DURATION + Duration::from_secs(10);
        let sampled = timeout(wait, async {
            loop {
                let state = recv_state_with_timeout(&mut update_rx, &target.addr, wait).await;
                // Regular polling can publish an intermediate Running state at the same tick.
                if state.detail.hotkeys.status != HotkeysStatus::Running {
                    break state;
                }
            }
        })
        .await
        .expect("hotkeys sampling completes");
        if sampled.detail.hotkeys.status == HotkeysStatus::Failed {
            let error = sampled
                .detail
                .hotkeys
                .last_error
                .clone()
                .unwrap_or_else(|| "unknown hotkeys error".to_string());
            if error.to_ascii_lowercase().contains("unknown command")
                || error.to_ascii_lowercase().contains("hotkeys")
            {
                skip_unreachable("standalone hotkeys", &error);
                return;
            }
            panic!("hotkeys sampling failed: {error}");
        }

        assert_eq!(sampled.detail.hotkeys.status, HotkeysStatus::Ready);
        assert_eq!(sampled.detail.hotkeys.selected_metric, Some(metric));
        assert!(!sampled.detail.hotkeys.tracking_active);
        assert!(sampled.detail.hotkeys.last_error.is_none());
    }
}

#[tokio::test]
async fn standalone_hotkeys_sampling_can_stop_early() {
    let _guard = HOTKEYS_TEST_LOCK.lock().await;
    let target = standalone_target();
    let settings = runtime_settings();
    let (mut update_rx, request_tx) = start(vec![target.clone()], settings);

    let state = recv_state(&mut update_rx, &target.addr).await;
    if state.status != rtop::model::Status::Ok {
        skip_unreachable(
            "standalone redis",
            state.last_error.as_deref().unwrap_or("poll failed"),
        );
        return;
    }

    if state.detail.hotkeys_supported == Some(false) {
        skip_unreachable("standalone hotkeys stop", "server does not provide HOTKEYS");
        return;
    }

    request_tx
        .send(PollerRequest::StartHotkeys {
            key: target.addr.clone(),
            metric: HotkeysMetric::Cpu,
            force: true,
        })
        .await
        .expect("failed to request hotkeys sampling");

    let running = recv_state(&mut update_rx, &target.addr).await;
    assert_eq!(running.detail.hotkeys.status, HotkeysStatus::Running);

    if let Err(err) = generate_hotkeys_traffic(&target).await {
        skip_unreachable("hotkeys traffic generation", &err.to_string());
        return;
    }

    request_tx
        .send(PollerRequest::StopHotkeys {
            key: target.addr.clone(),
        })
        .await
        .expect("failed to request hotkeys stop");

    let sampled = recv_state(&mut update_rx, &target.addr).await;
    if sampled.detail.hotkeys.status == HotkeysStatus::Failed {
        let error = sampled
            .detail
            .hotkeys
            .last_error
            .clone()
            .unwrap_or_else(|| "unknown hotkeys error".to_string());
        if error.to_ascii_lowercase().contains("unknown command")
            || error.to_ascii_lowercase().contains("hotkeys")
        {
            skip_unreachable("standalone hotkeys stop", &error);
            return;
        }
        panic!("hotkeys stop failed: {error}");
    }

    assert_eq!(sampled.detail.hotkeys.status, HotkeysStatus::Ready);
    assert_eq!(
        sampled.detail.hotkeys.selected_metric,
        Some(HotkeysMetric::Cpu)
    );
    assert!(!sampled.detail.hotkeys.tracking_active);
    assert!(sampled.detail.hotkeys.last_error.is_none());
}

#[tokio::test]
async fn cluster_seed_discovery_expands_to_every_cluster_node() {
    let seed = cluster_target();
    let settings = runtime_settings();

    let expected = match discover_cluster_targets(std::slice::from_ref(&seed), &settings).await {
        Ok(discovered) => discovered,
        Err(err) => {
            skip_unreachable("redis cluster seed discovery", &err.to_string());
            return;
        }
    };
    let mut reachable = Vec::new();
    for node in expected {
        if is_reachable(&node).await {
            reachable.push(node);
        }
    }
    assert!(
        reachable.len() > 1,
        "cluster must have more than one reachable node"
    );

    let mut discovery_rx =
        discovery::start(Vec::new(), vec![seed.clone()], vec![seed.clone()], settings);

    let mut verified = HashSet::new();
    loop {
        let event = timeout(Duration::from_secs(30), discovery_rx.recv())
            .await
            .expect("timed out waiting for discovery event")
            .expect("discovery channel closed before completing");
        match event {
            DiscoveryEvent::VerificationSucceeded(instance) => {
                verified.extend(tcp_endpoint_identity(&instance.target.addr));
            }
            DiscoveryEvent::Complete => break,
            _ => {}
        }
    }

    for node in &reachable {
        let identity =
            tcp_endpoint_identity(&node.addr).expect("cluster node must be a tcp endpoint");
        assert!(
            verified.contains(&identity),
            "cluster seed discovery missed {identity}; verified {verified:?}"
        );
    }
}

#[cfg(unix)]
async fn client_test_server(
    path: &std::path::Path,
) -> Option<(
    Server,
    Target,
    redis::Client,
    redis::aio::MultiplexedConnection,
)> {
    use std::process::{Command, Stdio};
    let child = match Command::new("redis-server")
        .args([
            "--port",
            "0",
            "--save",
            "",
            "--appendonly",
            "no",
            "--requirepass",
            "test-secret",
            "--unixsocket",
        ])
        .arg(path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            skip_unreachable(
                "isolated client tests (redis-server not installed)",
                &error.to_string(),
            );
            return None;
        }
        Err(error) => panic!("start test Redis: {error}"),
    };
    let server = Server(child);
    let target = Target {
        addr: path.to_string_lossy().into_owned(),
        protocol: TargetProtocol::Unix,
        password: Some("test-secret".into()),
        username: Some("default".into()),
        ..standalone_target()
    };
    let client =
        redis::Client::open(format!("redis+unix://{}?pass=test-secret", target.addr)).unwrap();
    let admin = timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(conn) = client.get_multiplexed_async_connection().await {
                break conn;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("private Redis ready");
    Some((server, target, client, admin))
}

/// All destructive commands in this test target a private, password-protected server.
#[cfg(unix)]
#[tokio::test]
async fn client_kills_and_lazy_reconnect_preserve_authentication() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("clients.sock");
    let Some((_server, target, client, mut admin)) = client_test_server(&path).await else {
        return;
    };
    let mut victims = Vec::new();
    let mut victim_ids = Vec::new();
    for _ in 0..2 {
        let mut conn = client.get_multiplexed_async_connection().await.unwrap();
        victim_ids.push(
            redis::cmd("CLIENT")
                .arg("ID")
                .query_async::<u64>(&mut conn)
                .await
                .unwrap(),
        );
        victims.push(conn);
    }
    let (mut updates, requests) = start(vec![target.clone()], runtime_settings());
    recv_state(&mut updates, &target.addr).await;
    requests
        .send(PollerRequest::WatchClients {
            keys: vec![target.addr.clone()],
        })
        .await
        .unwrap();
    let first = recv_clients(&mut updates).await.unwrap();
    for id in &victim_ids {
        assert!(first.rows.iter().any(|row| row.id == *id));
    }
    assert!(first.rows.iter().any(|row| row.id == first.own_id));

    // Include self first in the request to exercise sorting our own kill last.
    let mut ids = vec![first.own_id];
    ids.extend(&victim_ids);
    requests
        .send(PollerRequest::KillClients {
            targets: std::collections::BTreeMap::from([(target.addr.clone(), ids)]),
        })
        .await
        .unwrap();
    let results = recv_client_kills(&mut updates).await;
    assert_eq!(results.len(), 3);
    assert!(
        results.iter().all(|(_, result)| result == &Ok(1)),
        "{results:?}"
    );
    assert_eq!(results.last().unwrap().0, first.own_id);
    let second = recv_clients(&mut updates).await.unwrap();
    assert_ne!(first.own_id, second.own_id);
    assert!(second.rows.iter().all(|row| !victim_ids.contains(&row.id)));
    assert_eq!(
        redis::cmd("PING")
            .query_async::<String>(&mut admin)
            .await
            .unwrap(),
        "PONG"
    );

    // An external kill is first discovered by a read pipeline. The next refresh reconnects.
    redis::cmd("CLIENT")
        .arg("KILL")
        .arg("ID")
        .arg(second.own_id)
        .query_async::<u64>(&mut admin)
        .await
        .unwrap();
    requests.send(PollerRequest::RefreshAll).await.unwrap();
    assert!(recv_clients(&mut updates).await.is_err());
    requests.send(PollerRequest::RefreshAll).await.unwrap();
    let third = recv_clients(&mut updates).await.unwrap();
    assert_ne!(second.own_id, third.own_id);

    // A single command discovering a dead socket must not replay a destructive action.
    redis::cmd("CLIENT")
        .arg("KILL")
        .arg("ID")
        .arg(third.own_id)
        .query_async::<u64>(&mut admin)
        .await
        .unwrap();
    let admin_id = redis::cmd("CLIENT")
        .arg("ID")
        .query_async::<u64>(&mut admin)
        .await
        .unwrap();
    requests
        .send(PollerRequest::KillClients {
            targets: std::collections::BTreeMap::from([(target.addr.clone(), vec![admin_id])]),
        })
        .await
        .unwrap();
    assert!(recv_client_kills(&mut updates).await[0].1.is_err());
    let fourth = recv_clients(&mut updates).await.unwrap();
    assert_ne!(third.own_id, fourth.own_id);
    assert_eq!(
        redis::cmd("PING")
            .query_async::<String>(&mut admin)
            .await
            .unwrap(),
        "PONG"
    );
    requests
        .send(PollerRequest::WatchClients { keys: vec![] })
        .await
        .unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn aggregate_client_kills_route_colliding_ids_and_continue_after_errors() {
    use std::collections::BTreeMap;
    let dir = tempfile::tempdir().unwrap();
    let Some((_a, a, client_a, mut admin_a)) = client_test_server(&dir.path().join("a.sock")).await
    else {
        return;
    };
    let Some((_b, b, client_b, mut admin_b)) = client_test_server(&dir.path().join("b.sock")).await
    else {
        return;
    };
    let mut victim_a = client_a.get_multiplexed_async_connection().await.unwrap();
    let mut victim_b = client_b.get_multiplexed_async_connection().await.unwrap();
    let id_a = redis::cmd("CLIENT")
        .arg("ID")
        .query_async::<u64>(&mut victim_a)
        .await
        .unwrap();
    let id_b = redis::cmd("CLIENT")
        .arg("ID")
        .query_async::<u64>(&mut victim_b)
        .await
        .unwrap();
    assert_eq!(
        id_a, id_b,
        "private servers deliberately have colliding client IDs"
    );
    let admin_a_id = redis::cmd("CLIENT")
        .arg("ID")
        .query_async::<u64>(&mut admin_a)
        .await
        .unwrap();
    let (mut updates, requests) = start(vec![a.clone(), b.clone()], runtime_settings());
    // The initial overview states may arrive in either order.
    for _ in 0..2 {
        assert!(matches!(
            timeout(Duration::from_secs(5), updates.recv())
                .await
                .unwrap()
                .unwrap(),
            PollerUpdate::State(_)
        ));
    }
    requests
        .send(PollerRequest::WatchClients {
            keys: vec![a.addr.clone(), b.addr.clone()],
        })
        .await
        .unwrap();
    let mut snapshots = BTreeMap::new();
    for _ in 0..2 {
        let snapshot = recv_clients(&mut updates).await.unwrap();
        let server = snapshot.rows[0].server.clone();
        assert!(snapshot.rows.iter().all(|row| row.server == server));
        snapshots.insert(server, snapshot);
    }
    assert_eq!(snapshots.len(), 2);
    requests
        .send(PollerRequest::KillClients {
            targets: BTreeMap::from([(a.addr.clone(), vec![id_a])]),
        })
        .await
        .unwrap();
    assert_eq!(recv_client_kills(&mut updates).await, vec![(id_a, Ok(1))]);
    assert!(
        redis::cmd("PING")
            .query_async::<String>(&mut victim_a)
            .await
            .is_err()
    );
    assert_eq!(
        redis::cmd("PING")
            .query_async::<String>(&mut victim_b)
            .await
            .unwrap(),
        "PONG"
    );
    for _ in 0..2 {
        recv_clients(&mut updates).await.unwrap();
    }
    requests
        .send(PollerRequest::KillClients {
            targets: BTreeMap::from([
                ("!missing".into(), vec![id_a]),
                (a.addr.clone(), vec![admin_a_id]),
                (b.addr.clone(), vec![id_b]),
            ]),
        })
        .await
        .unwrap();
    assert!(recv_client_kills(&mut updates).await[0].1.is_err());
    assert_eq!(
        recv_client_kills(&mut updates).await,
        vec![(admin_a_id, Ok(1))]
    );
    assert_eq!(recv_client_kills(&mut updates).await, vec![(id_b, Ok(1))]);
    assert!(
        redis::cmd("PING")
            .query_async::<String>(&mut admin_a)
            .await
            .is_err()
    );
    assert!(
        redis::cmd("PING")
            .query_async::<String>(&mut victim_b)
            .await
            .is_err()
    );
    assert_eq!(
        redis::cmd("PING")
            .query_async::<String>(&mut admin_b)
            .await
            .unwrap(),
        "PONG"
    );
    for _ in 0..2 {
        recv_clients(&mut updates).await.unwrap();
    }
}

#[cfg(unix)]
async fn recv_clients(
    updates: &mut tokio::sync::mpsc::Receiver<PollerUpdate>,
) -> Result<rtop::clients::ClientSnapshot, String> {
    timeout(Duration::from_secs(5), async {
        loop {
            if let PollerUpdate::Clients { snapshot, .. } = updates.recv().await.unwrap() {
                return snapshot;
            }
        }
    })
    .await
    .expect("client snapshot")
}

#[cfg(unix)]
async fn recv_client_kills(
    updates: &mut tokio::sync::mpsc::Receiver<PollerUpdate>,
) -> Vec<(u64, Result<u64, String>)> {
    timeout(Duration::from_secs(5), async {
        loop {
            if let PollerUpdate::ClientsKilled { results, .. } = updates.recv().await.unwrap() {
                return results;
            }
        }
    })
    .await
    .expect("kill results")
}
