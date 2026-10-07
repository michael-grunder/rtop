//! File-oriented configuration commands. Never resolve environment credentials or
//! open the generated credential cache while inspecting or editing configuration.
use std::fs::{self, OpenOptions};
use std::io::{self, ErrorKind, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use crossterm::style::Stylize;
use toml::Value;
use toml_edit::{DocumentMut, Item};

use super::{
    FileConfig, GlobalConfig, default_settings, parse_protocol, parse_sort, parse_theme, parse_view,
};
use crate::target_addr::{normalize_tcp_addr, tcp_endpoint_identity};

const REDACTED: &str = "[redacted]";

pub fn run(action: &str, args: &[String], target: Option<&str>, path: Option<&Path>) -> Result<()> {
    let expected = match action {
        "list" => 0,
        "get" => 1,
        "set" => 2,
        _ => bail!("unknown config command"),
    };
    if args.len() != expected {
        bail!(
            "usage: rtop --config [get KEY | set KEY VALUE] [--target ALIAS_OR_ADDRESS] [-c PATH]"
        );
    }
    let path = path.map_or_else(
        || crate::credentials::default_path().map(|path| path.with_file_name("rtop.toml")),
        |path| Ok(path.to_path_buf()),
    )?;
    let mut stdout = io::stdout().lock();
    if action == "set" {
        set(&path, target, &args[0], &args[1])?;
        // Do not echo user-supplied values, including credentials.
        writeln!(stdout, "Configuration updated.")?;
    } else {
        let document = read(&path)?;
        let saved = values(&document)?;
        if action == "get" {
            let value = get(&saved, target, &args[0])?;
            writeln!(stdout, "{}", display_value(&value))?;
        } else {
            write_overview(
                &mut stdout,
                &path,
                &saved,
                target,
                io::stdout().is_terminal(),
            )?;
        }
    }
    Ok(())
}

fn read(path: &Path) -> Result<DocumentMut> {
    match fs::read_to_string(path) {
        Ok(content) => content.parse().map_err(|_| {
            // Parser diagnostics quote source lines, which may contain passwords.
            anyhow!("invalid TOML configuration (source omitted to protect credentials)")
        }),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(DocumentMut::new()),
        Err(error) => Err(error).context("failed to read configuration"),
    }
}

fn values(document: &DocumentMut) -> Result<Value> {
    toml::from_str(&document.to_string())
        .map_err(|_| anyhow!("invalid configuration (source omitted to protect credentials)"))
}

fn global_defaults() -> Result<Value> {
    let defaults = default_settings();
    Ok(toml::toml! {
        refresh_interval_ms = (i64::try_from(defaults.refresh_interval.as_millis())?)
        connect_timeout_ms = (i64::try_from(defaults.connect_timeout.as_millis())?)
        command_timeout_ms = (i64::try_from(defaults.command_timeout.as_millis())?)
        concurrency_limit = (i64::try_from(defaults.concurrency_limit)?)
        view_default = (defaults.default_view.as_str())
        sort_default = "address"
        still_autodiscover = true
        remember_auth = false
        leave_killed_servers = (defaults.leave_killed_servers)
        show_activity = (defaults.show_activity)
    }
    .into())
}

fn target_index(saved: &Value, selector: &str) -> Result<usize> {
    let targets = saved
        .get("targets")
        .and_then(Value::as_array)
        .context("no targets configured")?;
    let identity = normalize_tcp_addr(selector)
        .ok()
        .and_then(|addr| tcp_endpoint_identity(&addr));
    let matches: Vec<_> = targets
        .iter()
        .enumerate()
        .filter(|(_, entry)| {
            entry.get("alias").and_then(Value::as_str) == Some(selector)
                || entry
                    .get("addr")
                    .and_then(Value::as_str)
                    .is_some_and(|addr| {
                        addr == selector
                            || (entry.get("protocol").and_then(Value::as_str) != Some("unix")
                                && identity.is_some()
                                && normalize_tcp_addr(addr)
                                    .ok()
                                    .and_then(|addr| tcp_endpoint_identity(&addr))
                                    == identity)
                    })
        })
        .map(|(index, _)| index)
        .collect();
    match matches.as_slice() {
        [index] => Ok(*index),
        [] => bail!("target not found; use an existing alias or address from --config"),
        _ => bail!("target selector is ambiguous; use a unique alias or address"),
    }
}

fn get(saved: &Value, target: Option<&str>, key: &str) -> Result<Value> {
    let mut result = if let Some(selector) = target {
        let index = target_index(saved, selector)?;
        let entry = &saved["targets"][index];
        let alternate = match key {
            "user" => "username",
            "username" => "user",
            _ => key,
        };
        entry.get(key).or_else(|| entry.get(alternate)).cloned()
    } else if let Some(key) = key.strip_prefix("theme.") {
        saved.get("theme").and_then(|theme| theme.get(key)).cloned()
    } else {
        let key = key.strip_prefix("global.").unwrap_or(key);
        saved
            .get("global")
            .and_then(|global| global.get(key))
            .cloned()
            .or(global_defaults()?.get(key).cloned())
    }
    .context("setting not found; inspect available settings with --config")?;
    redact(key.rsplit('.').next().unwrap_or(key), &mut result);
    Ok(result)
}

#[derive(Clone, Copy)]
enum Kind {
    String,
    Boolean,
    PositiveInteger,
    Strings,
}

fn setting_kind(target: bool, key: &str) -> Result<Kind> {
    let kind = if target {
        match key {
            "alias" | "addr" | "protocol" | "username" | "user" | "password" | "password_env" => {
                Kind::String
            }
            "enabled" => Kind::Boolean,
            "tags" => Kind::Strings,
            _ => bail!("unknown target setting; inspect --config or consult the README"),
        }
    } else if let Some(key) = key.strip_prefix("theme.") {
        match key {
            "background_color" | "foreground_color" | "carat_color" | "caret_color"
            | "warning_color" | "critical_color" => Kind::String,
            _ => bail!("unknown theme setting; inspect --config or consult the README"),
        }
    } else {
        match key.strip_prefix("global.").unwrap_or(key) {
            "refresh_interval_ms"
            | "connect_timeout_ms"
            | "command_timeout_ms"
            | "concurrency_limit" => Kind::PositiveInteger,
            "still_autodiscover" | "remember_auth" | "leave_killed_servers" | "show_activity" => {
                Kind::Boolean
            }
            "view_default" | "sort_default" => Kind::String,
            _ => bail!("unknown global setting; inspect --config or consult the README"),
        }
    };
    Ok(kind)
}

fn parse_value(kind: Kind, raw: &str) -> Result<toml_edit::Value> {
    match kind {
        Kind::String => Ok(raw.into()),
        Kind::Boolean => raw
            .parse::<bool>()
            .map(Into::into)
            .map_err(|_| anyhow!("expected true or false")),
        Kind::PositiveInteger => raw
            .parse::<i64>()
            .ok()
            .filter(|value| *value > 0)
            .map(Into::into)
            .context("expected a positive integer"),
        Kind::Strings => {
            let value: toml_edit::Value = raw
                .parse()
                .map_err(|_| anyhow!("expected a TOML array of strings"))?;
            if !value
                .as_array()
                .is_some_and(|array| array.iter().all(toml_edit::Value::is_str))
            {
                bail!("expected a TOML array of strings");
            }
            Ok(value)
        }
    }
}

fn set(path: &Path, target: Option<&str>, key: &str, raw: &str) -> Result<()> {
    let value = parse_value(setting_kind(target.is_some(), key)?, raw)?;
    // Follow existing symlinks so an atomic replacement updates their referent.
    let path = if path.is_symlink() || path.exists() {
        fs::canonicalize(path).context("failed to resolve config path")?
    } else {
        path.to_path_buf()
    };
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).context("failed to create config directory")?;
    let mut lock_name = path.as_os_str().to_os_string();
    lock_name.push(".lock");
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let lock = options
        .open(PathBuf::from(lock_name))
        .context("failed to open config lock")?;
    lock.lock().context("failed to lock configuration")?;
    let mut document = read(&path)?;
    let saved = values(&document)?;
    let table: &mut dyn toml_edit::TableLike = if let Some(selector) = target {
        let index = target_index(&saved, selector)?;
        let targets = document
            .get_mut("targets")
            .context("no targets configured")?;
        match targets {
            Item::ArrayOfTables(array) => array.get_mut(index).context("target not found")?,
            Item::Value(toml_edit::Value::Array(array)) => array
                .get_mut(index)
                .and_then(toml_edit::Value::as_inline_table_mut)
                .context("target must be a table")?,
            _ => bail!("targets must be an array of tables"),
        }
    } else {
        let section = if key.starts_with("theme.") {
            "theme"
        } else {
            "global"
        };
        if !document.contains_key(section) {
            document[section] = Item::Table(toml_edit::Table::new());
        }
        document[section]
            .as_table_like_mut()
            .context("config section must be a table")?
    };
    let key = if target.is_some() {
        key
    } else {
        key.rsplit('.').next().unwrap_or(key)
    };
    let mut value = value;
    if let Some(previous) = table.get(key).and_then(Item::as_value) {
        *value.decor_mut() = previous.decor().clone();
    }
    table.insert(key, Item::Value(value));
    // These fields are aliases or mutually exclusive credential sources.
    if target.is_some() {
        match key {
            "password" => {
                table.remove("password_env");
            }
            "password_env" => {
                table.remove("password");
            }
            "user" => {
                table.remove("username");
            }
            "username" => {
                table.remove("user");
            }
            _ => {}
        }
    }
    validate(&document)
        .map_err(|_| anyhow!("invalid configuration value; file was not changed"))?;
    let mut temporary =
        tempfile::NamedTempFile::new_in(parent).context("failed to create temporary config")?;
    // NamedTempFile is private (0600 on Unix), including when replacing a file
    // that was previously readable by other users.
    temporary
        .write_all(document.to_string().as_bytes())
        .context("failed to write configuration")?;
    temporary
        .as_file()
        .sync_all()
        .context("failed to sync configuration")?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .context("failed to replace configuration")?;
    Ok(())
}

fn validate(document: &DocumentMut) -> Result<()> {
    let parsed: FileConfig = toml::from_str(&document.to_string())?;
    let global: GlobalConfig = parsed.global.unwrap_or_default();
    parse_view(global.view_default.as_deref())?;
    parse_sort(global.sort_default.as_deref())?;
    parse_theme(parsed.theme)?;
    for target in parsed.targets.unwrap_or_default() {
        if target.password.is_some() && target.password_env.is_some() {
            bail!("conflicting credential sources");
        }
        let addr = target.addr.as_deref().context("target requires addr")?;
        if addr.trim().is_empty() {
            bail!("target requires a nonempty addr");
        }
        if parse_protocol(target.protocol.as_deref(), addr)? == crate::model::TargetProtocol::Tcp {
            normalize_tcp_addr(addr)?;
        }
    }
    Ok(())
}

fn redact(key: &str, value: &mut Value) {
    let key = key.to_ascii_lowercase();
    if key != "password_env"
        && (matches!(
            key.as_str(),
            "user" | "username" | "auth" | "token" | "secret" | "pass"
        ) || key.contains("password")
            || key.ends_with("_token")
            || key.ends_with("_secret")
            || matches!(
                key.as_str(),
                "api_key" | "private_key" | "secret_key" | "access_key"
            ))
    {
        *value = REDACTED.into();
        return;
    }
    match value {
        Value::Table(table) => {
            for (key, value) in table {
                redact(key, value);
            }
        }
        Value::Array(array) => {
            for value in array {
                redact("", value);
            }
        }
        Value::String(value) => {
            if let Some((scheme, rest)) = value.split_once("://")
                && let Some((_, endpoint)) = rest.rsplit_once('@')
            {
                *value = format!("{scheme}://{REDACTED}@{endpoint}");
            }
        }
        _ => {}
    }
}

fn display_value(value: &Value) -> String {
    match value {
        Value::String(value) => escape(value),
        _ => value.to_string(),
    }
}

fn escape(text: &str) -> String {
    text.chars()
        .flat_map(|ch| {
            if ch.is_control() {
                ch.escape_default().collect::<Vec<_>>()
            } else {
                vec![ch]
            }
        })
        .collect()
}

fn write_overview(
    out: &mut impl Write,
    path: &Path,
    saved: &Value,
    target: Option<&str>,
    color: bool,
) -> Result<()> {
    let title = "rtop configuration";
    writeln!(
        out,
        "{}",
        if color {
            title.bold().cyan().to_string()
        } else {
            title.to_owned()
        }
    )?;
    writeln!(
        out,
        "File: {}{}",
        escape(&path.display().to_string()),
        if path.exists() {
            ""
        } else {
            " (not created; defaults shown)"
        }
    )?;
    writeln!(
        out,
        "Credentials are redacted. [default] marks unsaved global defaults."
    )?;
    let mut safe = saved.clone();
    redact("", &mut safe);
    if let Some(selector) = target {
        let index = target_index(saved, selector)?;
        write_section(
            out,
            &format!("Target #{}", index + 1),
            &safe["targets"][index],
            None,
            color,
        )?;
        return Ok(());
    }
    let mut globals = global_defaults()?;
    if let Some(table) = safe.get("global").and_then(Value::as_table) {
        globals
            .as_table_mut()
            .context("invalid defaults")?
            .extend(table.clone());
    }
    write_section(
        out,
        "Global",
        &globals,
        Some(saved.get("global").unwrap_or(&Value::Boolean(false))),
        color,
    )?;
    if let Some(table) = safe.as_table() {
        for (key, value) in table {
            match key.as_str() {
                "global" => {}
                "targets" => {
                    if let Some(targets) = value.as_array() {
                        for (index, target) in targets.iter().enumerate() {
                            write_section(
                                out,
                                &format!("Target #{}", index + 1),
                                target,
                                None,
                                color,
                            )?;
                        }
                    }
                }
                _ => write_section(out, key, value, None, color)?,
            }
        }
    }
    Ok(())
}

fn write_section(
    out: &mut impl Write,
    title: &str,
    value: &Value,
    saved: Option<&Value>,
    color: bool,
) -> Result<()> {
    let title = escape(title);
    writeln!(
        out,
        "\n{}",
        if color {
            title.bold().to_string()
        } else {
            title
        }
    )?;
    if let Some(table) = value.as_table() {
        let width = table.keys().map(|key| escape(key).len()).max().unwrap_or(0);
        for (key, value) in table {
            let origin = if saved.is_some_and(|saved| saved.get(key).is_none()) {
                " [default]"
            } else {
                ""
            };
            writeln!(
                out,
                "  {:width$}  {}{origin}",
                escape(key),
                display_value(value)
            )?;
        }
    } else {
        writeln!(out, "  {}", display_value(value))?;
    }
    Ok(())
}
