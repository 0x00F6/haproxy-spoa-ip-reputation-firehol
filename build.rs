use std::env;
use std::process::Command;

fn main() {
    let git_tag = Command::new("git")
        .args(["describe", "--tags", "--exact-match"])
        .output()
        .ok()
        .and_then(|output| {
            if output.status.success() {
                String::from_utf8(output.stdout).ok()
            } else {
                None
            }
        });

    let git_commit = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .and_then(|output| {
            if output.status.success() {
                String::from_utf8(output.stdout).ok()
            } else {
                None
            }
        });

    let version = if let Some(tag) = git_tag {
        tag.trim().to_string()
    } else if let Some(commit) = git_commit {
        commit.trim().to_string()
    } else {
        env!("CARGO_PKG_VERSION").to_string()
    };

    println!("cargo:rustc-env=BUILD_VERSION={}", version);
    println!("cargo:rerun-if-changed=.git/HEAD");
}
