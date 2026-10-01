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
}
