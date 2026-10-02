use std::process::Command;

#[test]
fn version_reports_rtop() {
    let output = Command::new(env!("CARGO_BIN_EXE_rtop"))
        .arg("--version")
        .output()
        .expect("rtop should launch");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("version should be UTF-8");
    assert!(stdout.starts_with(concat!("rtop ", env!("CARGO_PKG_VERSION"), " [")));
}

#[test]
fn help_reports_rtop_usage() {
    let output = Command::new(env!("CARGO_BIN_EXE_rtop"))
        .arg("--help")
        .output()
        .expect("rtop should launch");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("help should be UTF-8");
    assert!(stdout.contains("Usage: rtop"));
    assert!(stdout.contains("-r, --refresh-rate <DURATION>"));
    assert!(stdout.contains("[alias: --refresh]"));
}

#[test]
fn config_commands_reject_refresh_rate_options() {
    for option in ["-r", "--refresh-rate", "--refresh"] {
        let output = Command::new(env!("CARGO_BIN_EXE_rtop"))
            .args(["--config", option, "2s"])
            .output()
            .expect("rtop should launch");
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("--config commands cannot be combined with monitoring options")
        );
    }
}
