use std::collections::BTreeSet;

use anyhow::{Context, Result, bail};
use redis::Value;

use crate::model::{RuntimeSettings, Target, TargetProtocol};
use crate::parse::collect_cluster_shard_addresses;
use crate::redis_connection;

pub async fn discover_cluster_targets(
    seeds: &[Target],
    settings: &RuntimeSettings,
) -> Result<Vec<Target>> {
    if seeds.is_empty() {
        return Ok(Vec::new());
    }

    let mut discovered = Vec::new();
    let mut errors = Vec::new();

    for seed in seeds {
        match discover_from_seed(seed, settings).await {
            Ok((nodes, protocol)) => {
                if nodes.is_empty() {
                    errors.push(format!(
                        "{}: CLUSTER SHARDS returned no node addresses",
                        seed.addr
                    ));
                    continue;
                }
                discovered.extend(nodes.into_iter().map(|addr| Target {
                    alias: None,
                    addr,
                    protocol,
                    username: seed.username.clone(),
                    password: seed.password.clone(),
                    tags: Vec::new(),
                    process_id: None,
                }));
            }
            Err(err) => errors.push(format!("{}: {err}", seed.addr)),
        }
    }

    if discovered.is_empty() {
        bail!(
            "failed to discover cluster nodes from --cluster seed(s): {}",
            errors.join("; ")
        );
    }

    Ok(discovered)
}

async fn discover_from_seed(
    seed: &Target,
    settings: &RuntimeSettings,
) -> Result<(Vec<String>, TargetProtocol)> {
    if seed.protocol == TargetProtocol::Unix {
        bail!("cluster discovery only supports TCP seeds");
    }

    let mut conn = redis_connection::connect(seed, settings)
        .await
        .with_context(|| format!("failed to connect to {}", seed.addr))?;

    let shards: Value = redis::cmd("CLUSTER")
        .arg("SHARDS")
        .query_async(&mut conn)
        .await
        .with_context(|| format!("CLUSTER SHARDS failed on {}", seed.addr))?;

    let out: BTreeSet<String> =
        collect_cluster_shard_addresses(&shards, conn.protocol() == TargetProtocol::Tls);
    Ok((out.into_iter().collect(), conn.protocol()))
}
