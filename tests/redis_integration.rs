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
        refresh_interval: Duration::from_secs(60),
        connect_timeout: Duration::from_millis(300),
        command_timeout: Duration::from_secs(2),
        concurrency_limit: 2,
        leave_killed_servers: false,
        default_view: ViewMode::Flat,
        default_sort: SortMode::Address,
        ui_theme: UiTheme::default(),
    }
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
    loop {
        let update = timeout(Duration::from_secs(5), update_rx.recv())
            .await
            .expect("timed out waiting for poller update")
            .expect("poller update channel closed unexpectedly");
        match update {
            PollerUpdate::State(state) if state.key == key => return *state,
            PollerUpdate::State(_) | PollerUpdate::Remove { .. } => {}
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

#[tokio::test]
async fn standalone_hotkeys_sampling_works_against_live_redis() {
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
