use std::collections::HashMap;
use std::net::IpAddr;
use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{ArgAction, Parser, ValueEnum};

use crate::config;
use crate::discovery::DiscoveryTarget;
use crate::model::{RuntimeSettings, SortMode, Target, TargetProtocol, ViewMode};
use crate::target_addr::{normalize_tcp_addr, tcp_endpoint_identity, tcp_port};

const VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " [",
    env!("REDITOP_BUILD_DATE"),
    "] (",
    env!("REDITOP_GIT_SHA"),
    ")"
);

#[derive(Debug, Clone)]
pub struct LaunchConfig {
    pub settings: RuntimeSettings,
    pub targets: Vec<Target>,
    pub discovery_targets: Vec<DiscoveryTarget>,
    pub discovery_seed_targets: Vec<Target>,
    pub output_mode: OutputMode,
    pub once: bool,
    pub verbose: bool,
    pub config_path: Option<PathBuf>,
    pub no_default_config: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    Tui,
    Json,
}

#[derive(Debug, Parser)]
#[command(
    name = "reditop",
    version = VERSION,
    about = "htop-like TUI for Redis/Valkey"
)]
struct Cli {
    #[arg(value_name = "TARGETS")]
    targets: Vec<String>,

    #[arg(long = "unix", value_name = "PATH")]
    unix_targets: Vec<String>,

    #[arg(long = "tcp", value_name = "HOST:PORT")]
    tcp_targets: Vec<String>,

    #[arg(long = "cluster", value_name = "HOST:PORT")]
    cluster_targets: Vec<String>,

    #[arg(
        long = "autodiscover",
        visible_alias = "host",
        value_name = "HOST",
        num_args = 0..=1,
        default_missing_value = "127.0.0.1",
        action = ArgAction::Append
    )]
    autodiscover_hosts: Vec<String>,

    #[arg(short = 'c', long = "config", value_name = "PATH")]
    config: Option<PathBuf>,

    #[arg(long = "refresh", value_name = "DURATION")]
    refresh: Option<String>,

    #[arg(long = "connect-timeout", value_name = "DURATION")]
    connect_timeout: Option<String>,

    #[arg(long = "command-timeout", value_name = "DURATION")]
    command_timeout: Option<String>,

    #[arg(short = 'n', long = "concurrency", value_name = "N")]
    concurrency: Option<usize>,

    #[arg(long = "view", value_enum)]
    view: Option<CliViewMode>,

    #[arg(long = "sort", value_enum)]
    sort: Option<CliSortMode>,

    #[arg(long = "output", value_enum, default_value_t = CliOutputMode::Tui)]
    output: CliOutputMode,

    #[arg(long = "no-config")]
    no_config: bool,

    #[arg(long = "once")]
    once: bool,

    #[arg(short = 'a', long = "auth", value_name = "PASSWORD")]
    auth: Option<String>,

    #[arg(long = "user", value_name = "USERNAME")]
    user: Option<String>,

    #[arg(short = 'v', long = "verbose")]
    verbose: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliViewMode {
    Tree,
    Flat,
    Primary,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliSortMode {
    Alias,
    Address,
    Type,
    Cluster,
    Memory,
    Mem,
    Ops,
    Lat,
    Latmax,
    Status,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum CliOutputMode {
    Tui,
    Json,
}

#[allow(clippy::too_many_lines)]
pub fn build_launch_config() -> Result<LaunchConfig> {
    build_launch_config_from(Cli::parse())
}

#[allow(clippy::too_many_lines)]
fn build_launch_config_from(args: Cli) -> Result<LaunchConfig> {
    let base_settings = config::default_settings();
    let loaded_config = config::load_config(args.config.as_deref(), args.no_config)?;
    let config_target_count = loaded_config.targets.len();

    let mut settings = config::apply_overrides(base_settings, &loaded_config.overrides);

    if let Some(refresh) = args.refresh.as_deref() {
        settings.refresh_interval = humantime::parse_duration(refresh)?;
    }
    if let Some(timeout) = args.connect_timeout.as_deref() {
        settings.connect_timeout = humantime::parse_duration(timeout)?;
    }
    if let Some(timeout) = args.command_timeout.as_deref() {
        settings.command_timeout = humantime::parse_duration(timeout)?;
    }
    if let Some(limit) = args.concurrency {
        settings.concurrency_limit = limit.max(1);
    }
    if let Some(view) = args.view {
        settings.default_view = match view {
            CliViewMode::Tree => ViewMode::Tree,
            CliViewMode::Flat => ViewMode::Flat,
            CliViewMode::Primary => ViewMode::Primary,
        };
    }
    if let Some(sort) = args.sort {
        settings.default_sort = match sort {
            CliSortMode::Alias => SortMode::Alias,
            CliSortMode::Address => SortMode::Address,
            CliSortMode::Type => SortMode::Type,
            CliSortMode::Cluster => SortMode::Cluster,
            CliSortMode::Memory | CliSortMode::Mem => SortMode::Mem,
            CliSortMode::Ops => SortMode::Ops,
            CliSortMode::Lat => SortMode::Lat,
            CliSortMode::Latmax => SortMode::LatMax,
            CliSortMode::Status => SortMode::Status,
        };
    }

    let mut cli_targets = Vec::new();
    let mut positional_discovery_hosts = Vec::new();
    for raw in &args.targets {
        match parse_target_input(raw)? {
            ParsedInput::Target(target) => cli_targets.push(target),
            ParsedInput::DiscoveryHost(host) => positional_discovery_hosts.push(host),
        }
    }
    cli_targets.extend(args.unix_targets.into_iter().map(|raw| Target {
        alias: None,
        addr: raw,
        protocol: TargetProtocol::Unix,
        username: args.user.clone(),
        password: args.auth.clone(),
        tags: Vec::new(),
        process_id: None,
    }));
    cli_targets.extend(
        args.tcp_targets
            .into_iter()
            .map(|raw| {
                let addr = normalize_tcp_addr(&raw)?;
                Ok(Target {
                    alias: None,
                    addr,
                    protocol: TargetProtocol::Tcp,
                    username: args.user.clone(),
                    password: args.auth.clone(),
                    tags: Vec::new(),
                    process_id: None,
                })
            })
            .collect::<Result<Vec<_>>>()?,
    );
    let mut parsed_cluster_seeds: Vec<Target> = args
        .cluster_targets
        .into_iter()
        .map(|raw| {
            let addr = normalize_tcp_addr(&raw)?;
            Ok(Target {
                alias: None,
                addr,
                protocol: TargetProtocol::Tcp,
                username: args.user.clone(),
                password: args.auth.clone(),
                tags: Vec::new(),
                process_id: None,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let cli_has_explicit_targets = !cli_targets.is_empty() || !parsed_cluster_seeds.is_empty();
    let mut merged_targets = if cli_has_explicit_targets {
        Vec::new()
    } else {
        loaded_config.targets.clone()
    };

    if !cli_has_explicit_targets {
        for item in &mut merged_targets {
            if item.username.is_none() {
                item.username.clone_from(&args.user);
            }
            if item.password.is_none() {
                item.password.clone_from(&args.auth);
            }
        }
    }

    let discovery_seed_targets = dedupe_targets(parsed_cluster_seeds.clone());

    for mut parsed in cli_targets {
        if parsed.username.is_none() {
            parsed.username.clone_from(&args.user);
        }
        if parsed.password.is_none() {
            parsed.password.clone_from(&args.auth);
        }
        if let Some(config_target) = find_matching_target(&loaded_config.targets, &parsed) {
            merge_target_context(&mut parsed, config_target);
        }
        merged_targets.push(parsed);
    }

    for seed in &mut parsed_cluster_seeds {
        if seed.username.is_none() {
            seed.username.clone_from(&args.user);
        }
        if seed.password.is_none() {
            seed.password.clone_from(&args.auth);
        }
        if let Some(config_target) = find_matching_target(&loaded_config.targets, seed) {
            merge_target_context(seed, config_target);
        }
    }

    merged_targets.extend(parsed_cluster_seeds);

    merged_targets = dedupe_targets(merged_targets);
    let default_mode = if cli_has_explicit_targets {
        DiscoveryDefaultMode::Disabled
    } else if merged_targets.is_empty()
        || config_target_count == 0
        || loaded_config.still_autodiscover
    {
        DiscoveryDefaultMode::Localhost
    } else {
        DiscoveryDefaultMode::Disabled
    };
    let discovery_targets = dedupe_discovery_targets(build_discovery_targets(&DiscoveryPlan {
        default_mode,
        positional_discovery_hosts: &positional_discovery_hosts,
        autodiscover_hosts: &args.autodiscover_hosts,
        username: args.user.clone(),
        password: args.auth.clone(),
    }));

    Ok(LaunchConfig {
        settings,
        targets: merged_targets,
        discovery_targets,
        discovery_seed_targets,
        output_mode: match args.output {
            CliOutputMode::Tui => OutputMode::Tui,
            CliOutputMode::Json => OutputMode::Json,
        },
        once: args.once,
        verbose: args.verbose,
        config_path: args.config,
        no_default_config: args.no_config,
    })
}

fn parse_target_string(raw: &str) -> Result<Target> {
    let (protocol, addr) = if let Some(path) = raw.strip_prefix("unix:") {
        (TargetProtocol::Unix, path.to_string())
    } else if raw.contains('/') {
        (TargetProtocol::Unix, raw.to_string())
    } else {
        (TargetProtocol::Tcp, normalize_tcp_addr(raw)?)
    };

    Ok(Target {
        alias: None,
        addr,
        protocol,
        username: None,
        password: None,
        tags: Vec::new(),
        process_id: None,
    })
}

#[derive(Debug, Clone)]
enum ParsedInput {
    Target(Target),
    DiscoveryHost(String),
}

fn parse_target_input(raw: &str) -> Result<ParsedInput> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("target must not be empty");
    }

    if let Some(host) = parse_discovery_host(trimmed)? {
        return Ok(ParsedInput::DiscoveryHost(host));
    }

    Ok(ParsedInput::Target(parse_target_string(trimmed)?))
}

fn parse_discovery_host(raw: &str) -> Result<Option<String>> {
    if raw.strip_prefix("unix:").is_some() || raw.contains('/') {
        return Ok(None);
    }

    if raw.chars().all(|ch| ch.is_ascii_digit()) {
        return Ok(None);
    }

    if raw
        .strip_prefix(':')
        .is_some_and(|port| !port.is_empty() && port.chars().all(|ch| ch.is_ascii_digit()))
    {
        return Ok(None);
    }

    if tcp_port(raw).is_some() {
        return Ok(None);
    }

    if let Some(inner) = raw
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
    {
        if inner.is_empty() {
            bail!("autodiscovery host must not be empty");
        }
        return Ok(Some(inner.to_string()));
    }

    if raw.parse::<IpAddr>().is_ok() || !raw.contains(':') {
        return Ok(Some(raw.to_string()));
    }

    bail!("autodiscovery host must not include a port: {raw}");
}

fn dedupe_targets(input: Vec<Target>) -> Vec<Target> {
    let mut by_key: HashMap<(String, TargetProtocol), Target> = HashMap::new();
    for target in input {
        by_key
            .entry((target.addr.clone(), target.protocol))
            .and_modify(|existing| merge_target_context(existing, &target))
            .or_insert(target);
    }
    let mut out: Vec<Target> = by_key.into_values().collect();
    out.sort_by(|a, b| a.addr.cmp(&b.addr));
    out
}

fn find_matching_target<'a>(targets: &'a [Target], candidate: &Target) -> Option<&'a Target> {
    targets
        .iter()
        .find(|target| same_target_endpoint(target, candidate))
}

fn same_target_endpoint(left: &Target, right: &Target) -> bool {
    if left.protocol != right.protocol {
        return false;
    }

    match left.protocol {
        TargetProtocol::Unix => left.addr == right.addr,
        TargetProtocol::Tcp => {
            tcp_endpoint_identity(&left.addr) == tcp_endpoint_identity(&right.addr)
        }
    }
}

fn merge_target_context(target: &mut Target, known: &Target) {
    if target.alias.is_none() {
        target.alias.clone_from(&known.alias);
    }
    if target.username.is_none() {
        target.username.clone_from(&known.username);
    }
    if target.password.is_none() {
        target.password.clone_from(&known.password);
    }
    if target.tags.is_empty() && !known.tags.is_empty() {
        target.tags.clone_from(&known.tags);
    }
    if target.process_id.is_none() {
        target.process_id = known.process_id;
    }
}

#[derive(Debug, Clone)]
enum DiscoveryDefaultMode {
    Disabled,
    Localhost,
}

#[derive(Debug, Clone)]
struct DiscoveryPlan<'a> {
    default_mode: DiscoveryDefaultMode,
    positional_discovery_hosts: &'a [String],
    autodiscover_hosts: &'a [String],
    username: Option<String>,
    password: Option<String>,
}

fn build_discovery_targets(plan: &DiscoveryPlan<'_>) -> Vec<DiscoveryTarget> {
    let mut out = plan
        .positional_discovery_hosts
        .iter()
        .chain(plan.autodiscover_hosts.iter())
        .map(|host| DiscoveryTarget {
            host: host.trim().to_string(),
            username: plan.username.clone(),
            password: plan.password.clone(),
        })
        .filter(|target| !target.host.is_empty())
        .collect::<Vec<_>>();

    if out.is_empty() && matches!(plan.default_mode, DiscoveryDefaultMode::Localhost) {
        out.push(DiscoveryTarget::localhost(
            plan.username.clone(),
            plan.password.clone(),
        ));
    }

    out
}

fn dedupe_discovery_targets(input: Vec<DiscoveryTarget>) -> Vec<DiscoveryTarget> {
    let mut by_host: HashMap<String, DiscoveryTarget> = HashMap::new();
    for target in input {
        let localhost_probe = DiscoveryTarget {
            host: target.host.clone(),
            username: None,
            password: None,
        };
        let key = if localhost_probe.is_localhost() {
            "127.0.0.1".to_string()
        } else {
            target.host.to_ascii_lowercase()
        };
        by_host
            .entry(key)
            .and_modify(|existing| {
                if existing.username.is_none() {
                    existing.username.clone_from(&target.username);
                }
                if existing.password.is_none() {
                    existing.password.clone_from(&target.password);
                }
            })
            .or_insert(target);
    }
    let mut out: Vec<_> = by_host.into_values().collect();
    out.sort_by(|left, right| left.host.cmp(&right.host));
    out
}

#[cfg(test)]
mod tests {
    use std::fs;

    use clap::Parser;
    use tempfile::tempdir;

    use crate::model::ViewMode;

    use super::{
        DiscoveryDefaultMode, DiscoveryPlan, DiscoveryTarget, OutputMode, VERSION,
        build_discovery_targets, dedupe_discovery_targets,
    };
    #[test]
    fn version_string_contains_build_metadata() {
        assert!(VERSION.starts_with(env!("CARGO_PKG_VERSION")));
        assert!(VERSION.contains(" ["));
        assert!(VERSION.contains("] ("));
        assert!(VERSION.ends_with(')'));
    }

    #[test]
    fn discovery_defaults_to_localhost_when_only_config_targets_are_present() {
        let discovered = build_discovery_targets(&DiscoveryPlan {
            default_mode: DiscoveryDefaultMode::Localhost,
            positional_discovery_hosts: &[],
            autodiscover_hosts: &[],
            username: None,
            password: None,
        });

        assert_eq!(discovered.len(), 1);
        assert_eq!(discovered[0], DiscoveryTarget::localhost(None, None));
    }

    #[test]
    fn discovery_can_be_disabled_when_only_config_targets_are_present() {
        let discovered = dedupe_discovery_targets(build_discovery_targets(&DiscoveryPlan {
            default_mode: DiscoveryDefaultMode::Disabled,
            positional_discovery_hosts: &[],
            autodiscover_hosts: &[],
            username: None,
            password: None,
        }));

        assert_eq!(discovered, Vec::new());
    }

    #[test]
    fn explicit_cli_targets_still_disable_default_discovery() {
        let discovered = dedupe_discovery_targets(build_discovery_targets(&DiscoveryPlan {
            default_mode: DiscoveryDefaultMode::Disabled,
            positional_discovery_hosts: &[],
            autodiscover_hosts: &[],
            username: None,
            password: None,
        }));

        assert_eq!(discovered, Vec::new());
    }

    #[test]
    fn config_target_credentials_do_not_leak_to_hostwide_discovery() {
        let discovered = dedupe_discovery_targets(build_discovery_targets(&DiscoveryPlan {
            default_mode: DiscoveryDefaultMode::Localhost,
            positional_discovery_hosts: &[],
            autodiscover_hosts: &[],
            username: None,
            password: None,
        }));

        assert_eq!(discovered, vec![DiscoveryTarget::localhost(None, None)]);
    }

    #[test]
    fn positional_host_only_input_becomes_discovery_host() {
        let cli = super::Cli::parse_from(["reditop", "--no-config", "192.168.0.174"]);
        let launch = super::build_launch_config_from(cli).expect("launch config should parse");

        assert!(launch.targets.is_empty());
        assert_eq!(
            launch.discovery_targets,
            vec![DiscoveryTarget {
                host: "192.168.0.174".to_string(),
                username: None,
                password: None,
            }]
        );
        assert!(launch.discovery_seed_targets.is_empty());
    }

    #[test]
    fn explicit_targets_disable_autodiscovery_by_default() {
        let cli = super::Cli::parse_from(["reditop", "--no-config", "6379", "redis:9999"]);
        let launch = super::build_launch_config_from(cli).expect("launch config should parse");

        assert_eq!(launch.targets.len(), 2);
        assert_eq!(launch.discovery_targets, Vec::new());
        assert!(launch.discovery_seed_targets.is_empty());
    }

    #[test]
    fn autodiscover_without_value_reenables_localhost_discovery() {
        let cli = super::Cli::parse_from(["reditop", "--no-config", "6379", "--autodiscover"]);
        let launch = super::build_launch_config_from(cli).expect("launch config should parse");

        assert_eq!(launch.targets.len(), 1);
        assert_eq!(
            launch.discovery_targets,
            vec![DiscoveryTarget::localhost(None, None)]
        );
    }

    #[test]
    fn autodiscover_with_value_uses_requested_host() {
        let cli = super::Cli::parse_from([
            "reditop",
            "--no-config",
            "6379",
            "--autodiscover",
            "192.168.1.1",
        ]);
        let launch = super::build_launch_config_from(cli).expect("launch config should parse");

        assert_eq!(launch.targets.len(), 1);
        assert_eq!(
            launch.discovery_targets,
            vec![DiscoveryTarget {
                host: "192.168.1.1".to_string(),
                username: None,
                password: None,
            }]
        );
    }

    #[test]
    fn cluster_seeds_still_feed_discovery() {
        let cli = super::Cli::parse_from(["reditop", "--no-config", "--cluster", "7000"]);
        let launch = super::build_launch_config_from(cli).expect("launch config should parse");

        assert_eq!(launch.targets.len(), 1);
        assert_eq!(launch.discovery_seed_targets.len(), 1);
        assert_eq!(launch.discovery_seed_targets[0].addr, "127.0.0.1:7000");
    }

    #[test]
    fn once_flag_enables_non_interactive_launch() {
        let cli = super::Cli::parse_from(["reditop", "--no-config", "--once"]);
        let launch = super::build_launch_config_from(cli).expect("launch config should parse");

        assert!(launch.once);
    }

    #[test]
    fn output_json_selects_json_stream_mode() {
        let cli = super::Cli::parse_from(["reditop", "--no-config", "--output", "json"]);
        let launch = super::build_launch_config_from(cli).expect("launch config should parse");

        assert_eq!(launch.output_mode, OutputMode::Json);
    }

    #[test]
    fn cli_view_primary_sets_primary_default_view() {
        let cli = super::Cli::parse_from(["reditop", "--no-config", "--view", "primary"]);
        let launch = super::build_launch_config_from(cli).expect("launch config should parse");

        assert_eq!(launch.settings.default_view, ViewMode::Primary);
    }

    #[test]
    fn explicit_cli_targets_do_not_load_unmatched_config_targets() {
        let dir = tempdir().expect("tempdir should work");
        let config_path = dir.path().join("redis-top.toml");
        fs::write(
            &config_path,
            r#"
[[targets]]
alias = "local-6379"
addr = "127.0.0.1:6379"

[[targets]]
alias = "local-6380"
addr = "127.0.0.1:6380"
password = "secret"
"#,
        )
        .expect("config should write");

        let cli = super::Cli::parse_from([
            "reditop",
            "--config",
            config_path.to_str().expect("config path should be utf8"),
            "6379",
        ]);
        let launch = super::build_launch_config_from(cli).expect("launch config should parse");

        assert_eq!(launch.targets.len(), 1);
        assert_eq!(launch.targets[0].addr, "127.0.0.1:6379");
        assert_eq!(launch.targets[0].alias.as_deref(), Some("local-6379"));
        assert_eq!(launch.discovery_targets, Vec::new());
    }

    #[test]
    fn explicit_cli_targets_reuse_matching_config_credentials() {
        let dir = tempdir().expect("tempdir should work");
        let config_path = dir.path().join("redis-top.toml");
        fs::write(
            &config_path,
            r#"
[[targets]]
alias = "loopback"
addr = "localhost:6380"
user = "default"
password = "secret"
tags = ["known"]
"#,
        )
        .expect("config should write");

        let cli = super::Cli::parse_from([
            "reditop",
            "--config",
            config_path.to_str().expect("config path should be utf8"),
            "6380",
        ]);
        let launch = super::build_launch_config_from(cli).expect("launch config should parse");

        assert_eq!(launch.targets.len(), 1);
        assert_eq!(launch.targets[0].addr, "127.0.0.1:6380");
        assert_eq!(launch.targets[0].alias.as_deref(), Some("loopback"));
        assert_eq!(launch.targets[0].username.as_deref(), Some("default"));
        assert_eq!(launch.targets[0].password.as_deref(), Some("secret"));
        assert_eq!(launch.targets[0].tags, vec!["known".to_string()]);
    }
}
