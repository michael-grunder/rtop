use std::fs;
use std::path::PathBuf;
use std::process::Command;

const UNKNOWN: &str = "unknown";

fn main() {
    let build_date = command_stdout("date", &["+%F"]).unwrap_or_else(|| UNKNOWN.to_string());

    let git_sha = command_stdout("git", &["rev-parse", "--short=12", "HEAD"])
        .unwrap_or_else(|| UNKNOWN.to_string());
    let git_sha = match git_dirty() {
        Some(true) => format!("{git_sha}-dirty"),
        _ => git_sha,
    };

    println!("cargo:rustc-env=RTOP_BUILD_DATE={build_date}");
    println!("cargo:rustc-env=RTOP_GIT_SHA={git_sha}");

    configure_git_rerun_hints();
}

fn command_stdout(command: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(command).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }

    let value = String::from_utf8(output.stdout).ok()?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }

    Some(trimmed.to_string())
}

fn git_dirty() -> Option<bool> {
    let status = Command::new("git")
        .args(["diff", "--quiet", "--ignore-submodules", "HEAD", "--"])
        .status()
        .ok()?;
    match status.code() {
        Some(0) => Some(false),
        Some(1) => Some(true),
        _ => None,
    }
}

fn configure_git_rerun_hints() {
    let Some(git_dir) = command_stdout("git", &["rev-parse", "--git-dir"]) else {
        return;
    };

    let git_dir_path = PathBuf::from(git_dir);
    println!(
        "cargo:rerun-if-changed={}",
        git_dir_path.join("HEAD").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        git_dir_path.join("index").display()
    );

    let head_path = git_dir_path.join("HEAD");
    let Ok(head_contents) = fs::read_to_string(head_path) else {
        return;
    };
    let Some(reference) = head_contents.strip_prefix("ref: ") else {
        return;
    };

    println!(
        "cargo:rerun-if-changed={}",
        git_dir_path.join(reference.trim()).display()
    );
}
