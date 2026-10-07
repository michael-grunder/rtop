use std::fs;
use std::process::{Command, Output};

use tempfile::TempDir;

struct ConfigCli {
    directory: TempDir,
}

impl ConfigCli {
    fn new() -> Self {
        Self {
            directory: tempfile::tempdir().unwrap(),
        }
    }

    fn path(&self) -> std::path::PathBuf {
        self.directory.path().join("config/rtop.toml")
    }

    fn write(&self, content: &str) {
        fs::create_dir_all(self.path().parent().unwrap()).unwrap();
        fs::write(self.path(), content).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_rtop"))
            .current_dir(self.directory.path())
            .env("HOME", self.directory.path())
            .env("XDG_CONFIG_HOME", self.directory.path().join("config"))
            .env("REDIS_PASSWORD", "resolved-env-secret")
            .args(args)
            .output()
            .unwrap()
    }

    fn success(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }
}

#[test]
fn overview_shows_defaults_and_creates_nothing() {
    let cli = ConfigCli::new();
    let output = cli.success(&["--config"]);
    assert!(output.contains("rtop configuration"));
    assert!(output.contains("Global"));
    assert!(output.contains("1000 [default]"));
    assert!(output.contains("not created"));
    assert!(!output.contains('\x1b'));
    assert!(!cli.path().exists());
    assert_eq!(
        cli.success(&["--config", "get", "global.refresh_interval_ms"]),
        "1000\n"
    );
}

#[test]
fn global_settings_are_typed_and_persisted_in_the_user_config() {
    let cli = ConfigCli::new();
    assert_eq!(cli.success(&["--config", "get", "show_activity"]), "true\n");
    cli.success(&["--config", "set", "show_activity", "false"]);
    assert_eq!(
        cli.success(&["--config", "get", "global.show_activity"]),
        "false\n"
    );
    assert!(
        !cli.run(&["--config", "set", "show_activity", "invalid"])
            .status
            .success()
    );
    cli.success(&["--config", "set", "global.refresh_interval_ms", "2500"]);
    cli.success(&["--config", "set", "remember_auth", "true"]);
    cli.success(&["--config", "set", "view_default", "primary"]);
    cli.success(&["--config", "set", "theme.foreground_color", "cyan"]);
    assert_eq!(
        cli.success(&["--config", "get", "refresh_interval_ms"]),
        "2500\n"
    );
    assert_eq!(
        cli.success(&["--config", "get", "global.remember_auth"]),
        "true\n"
    );
    assert_eq!(
        cli.success(&["--config", "get", "view_default"]),
        "primary\n"
    );
    assert_eq!(
        cli.success(&["--config", "get", "theme.foreground_color"]),
        "cyan\n"
    );
    let data: toml::Value = toml::from_str(&fs::read_to_string(cli.path()).unwrap()).unwrap();
    assert_eq!(
        data["global"]["refresh_interval_ms"].as_integer(),
        Some(2500)
    );
    assert_eq!(data["global"]["remember_auth"].as_bool(), Some(true));
    assert_eq!(data["global"]["show_activity"].as_bool(), Some(false));
    assert!(!cli.path().with_file_name("rtop-auth.toml").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(cli.path()).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

const TARGET_CONFIG: &str = r#"# Preserve this comment (and never print its comment-secret)
[global]
refresh_interval_ms = 1500 # inline comment

[[targets]]
alias = "local"
addr = ":6379"
user = "private-user"
password = "original-secret" # password comment
enabled = false

[[targets]]
alias = "remote"
addr = "redis.example:6380"
password_env = "REDIS_PASSWORD"

[columns.used_mem]
info_key = "used_memory"
header = "Mem"

[custom]
api_key = "api-secret"
url = "redis://url-user:url-secret@localhost:6379"
"#;

#[test]
fn overview_and_get_mask_credentials_without_resolving_environment() {
    let cli = ConfigCli::new();
    cli.write(TARGET_CONFIG);
    let output = cli.success(&["--config"]);
    for secret in [
        "private-user",
        "original-secret",
        "resolved-env-secret",
        "api-secret",
        "url-secret",
        "url-user",
        "comment-secret",
    ] {
        assert!(!output.contains(secret), "leaked {secret}");
    }
    for visible in [
        "local",
        ":6379",
        "false",
        "REDIS_PASSWORD",
        "[redacted]",
        "used_memory",
    ] {
        assert!(output.contains(visible), "missing {visible}");
    }
    assert_eq!(
        cli.success(&["--config", "get", "password", "--target", "local"]),
        "[redacted]\n"
    );
    assert_eq!(
        cli.success(&["--config", "get", "username", "--target", "local"]),
        "[redacted]\n"
    );
    assert_eq!(
        cli.success(&["--config", "get", "password_env", "--target", "remote"]),
        "REDIS_PASSWORD\n"
    );
    let target = cli.success(&["--config", "--target", "local"]);
    assert!(target.contains("local"));
    assert!(!target.contains("remote"));
}

#[test]
fn updates_preserve_comments_other_targets_and_unrelated_settings() {
    let cli = ConfigCli::new();
    cli.write(TARGET_CONFIG);
    cli.success(&["--config", "set", "refresh_interval_ms", "2000"]);
    cli.success(&[
        "--config",
        "set",
        "tags",
        "[\"dev\", \"local\"]",
        "--target",
        "localhost:6379",
    ]);
    cli.success(&["--config", "set", "enabled", "true", "--target", "6379"]);
    let password = "false\n\"new-secret\"\\end";
    let output = cli.success(&["--config", "set", "password", password, "--target", "local"]);
    assert!(!output.contains("new-secret"));
    let content = fs::read_to_string(cli.path()).unwrap();
    for retained in [
        "# Preserve this comment",
        "# inline comment",
        "# password comment",
        "[columns.used_mem]",
        "api-secret",
    ] {
        assert!(content.contains(retained));
    }
    let data: toml::Value = toml::from_str(&content).unwrap();
    assert_eq!(data["targets"][0]["password"].as_str(), Some(password));
    assert_eq!(data["targets"][0]["tags"].as_array().unwrap().len(), 2);
    assert_eq!(data["targets"][0]["enabled"].as_bool(), Some(true));
    assert_eq!(
        data["targets"][1]["password_env"].as_str(),
        Some("REDIS_PASSWORD")
    );
}

#[test]
fn switching_credential_sources_and_username_aliases_keeps_config_valid() {
    let cli = ConfigCli::new();
    cli.write(TARGET_CONFIG);
    cli.success(&[
        "--config",
        "set",
        "password_env",
        "NEW_REDIS_PASSWORD",
        "--target",
        "local",
    ]);
    cli.success(&[
        "--config", "set", "username", "new-user", "--target", "local",
    ]);
    let content = fs::read_to_string(cli.path()).unwrap();
    let data: toml::Value = toml::from_str(&content).unwrap();
    assert!(data["targets"][0].get("password").is_none());
    assert!(data["targets"][0].get("user").is_none());
    cli.success(&[
        "--config",
        "set",
        "password",
        "new-secret",
        "--target",
        "local",
    ]);
    let data: toml::Value = toml::from_str(&fs::read_to_string(cli.path()).unwrap()).unwrap();
    assert!(data["targets"][0].get("password_env").is_none());
}

#[test]
fn invalid_updates_fail_without_changing_the_file_or_echoing_values() {
    let cli = ConfigCli::new();
    cli.write(TARGET_CONFIG);
    for args in [
        vec!["--config", "set", "remember_auth", "invalid-secret"],
        vec!["--config", "set", "view_default", "invalid-secret"],
        vec!["--config", "set", "refresh_interval_ms", "-1"],
        vec!["--config", "set", "concurrency_limit", "0"],
        vec![
            "--config",
            "set",
            "concurrency_limit",
            "9999999999999999999999",
        ],
        vec!["--config", "set", "unknown", "invalid-secret"],
        vec!["--config", "set", "global.theme.foreground_color", "cyan"],
        vec!["--config", "set", "tags", "[1]", "--target", "local"],
        vec![
            "--config",
            "set",
            "protocol",
            "invalid-secret",
            "--target",
            "local",
        ],
        vec!["--config", "set", "enabled", "true", "--target", "absent"],
        vec![
            "--config",
            "set",
            "theme.foreground_color",
            "invalid-secret",
        ],
        vec!["--config", "set", "global.remember_auth"],
        vec!["--config", "get", "absent"],
        vec!["--config", "--no-config"],
        vec!["--config", "--tcp", "6379"],
    ] {
        let output = cli.run(&args);
        assert!(!output.status.success(), "unexpected success: {args:?}");
        assert!(!String::from_utf8_lossy(&output.stderr).contains("invalid-secret"));
        assert_eq!(fs::read_to_string(cli.path()).unwrap(), TARGET_CONFIG);
    }
}

#[test]
fn malformed_toml_errors_do_not_quote_secrets() {
    let cli = ConfigCli::new();
    cli.write("[[targets]]\npassword = \"malformed-secret\" invalid\n");
    for args in [
        vec!["--config"],
        vec!["--config", "set", "remember_auth", "true"],
    ] {
        let output = cli.run(&args);
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("malformed-secret"));
    }
}

#[test]
fn ambiguous_selectors_fail_and_explicit_file_overrides_user_config() {
    let cli = ConfigCli::new();
    cli.write("[[targets]]\nalias = 'same'\naddr = ':6379'\n[[targets]]\nalias = 'same'\naddr = ':6380'\n");
    let output = cli.run(&["--config", "get", "addr", "--target", "same"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("ambiguous"));
    let explicit = cli.directory.path().join("custom.toml");
    cli.success(&[
        "--config",
        "set",
        "remember_auth",
        "true",
        "--config-file",
        explicit.to_str().unwrap(),
    ]);
    assert_eq!(
        cli.success(&[
            "--config",
            "get",
            "remember_auth",
            "-c",
            explicit.to_str().unwrap()
        ]),
        "true\n"
    );
    assert_eq!(
        cli.success(&["--config", "get", "remember_auth"]),
        "false\n"
    );
}

#[test]
fn home_fallback_ignores_local_config_and_relative_xdg() {
    let cli = ConfigCli::new();
    fs::write(
        cli.directory.path().join("rtop.toml"),
        "[global]\nremember_auth = true\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_rtop"))
        .current_dir(cli.directory.path())
        .env("HOME", cli.directory.path())
        .env("XDG_CONFIG_HOME", "relative")
        .args(["--config", "set", "remember_auth", "false"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(cli.directory.path().join(".config/rtop.toml").exists());
    assert!(
        fs::read_to_string(cli.directory.path().join("rtop.toml"))
            .unwrap()
            .contains("true")
    );
}

#[test]
fn inline_tables_and_dash_prefixed_string_values_round_trip() {
    let cli = ConfigCli::new();
    cli.write("targets = [{ alias = 'local', addr = ':6379', password = 'secret' }]\nglobal = { remember_auth = false }\n");
    cli.success(&["--config", "set", "remember_auth", "true"]);
    cli.success(&[
        "--config",
        "set",
        "--target",
        "local",
        "--",
        "password",
        "-new-secret",
    ]);
    let content = fs::read_to_string(cli.path()).unwrap();
    let data: toml::Value = toml::from_str(&content).unwrap();
    assert_eq!(data["global"]["remember_auth"].as_bool(), Some(true));
    assert_eq!(data["targets"][0]["password"].as_str(), Some("-new-secret"));
    assert!(!cli.success(&["--config"]).contains("-new-secret"));
}

#[cfg(unix)]
#[test]
fn editing_a_symlink_preserves_the_link() {
    let cli = ConfigCli::new();
    cli.write(TARGET_CONFIG);
    let link = cli.directory.path().join("linked.toml");
    std::os::unix::fs::symlink(cli.path(), &link).unwrap();
    cli.success(&[
        "--config",
        "set",
        "remember_auth",
        "true",
        "-c",
        link.to_str().unwrap(),
    ]);
    assert!(link.is_symlink());
    assert_eq!(cli.success(&["--config", "get", "remember_auth"]), "true\n");
}
