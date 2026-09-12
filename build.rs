//! Embeds a version string in the binary as `BUILD_VERSION`.
//!
//! Precedence: the `GIT_VERSION` environment variable (set by container builds, whose context has
//! no `.git` directory), then the exact git tag, then the short commit hash, then the crate version.

use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

const VERSION_OVERRIDE: &str = "GIT_VERSION";

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

/// Tells Cargo which git files determine the version, so the build script re-runs when HEAD moves.
/// Nothing is emitted outside a checkout: a `rerun-if-changed` on a missing path would force the
/// build script, and this crate, to be rebuilt on every run.
fn track_git_head() {
    let head = Path::new(".git/HEAD");
    if !head.is_file() {
        return;
    }
    println!("cargo:rerun-if-changed=.git/HEAD");
    if let Some(reference) = fs::read_to_string(head)
        .ok()
        .and_then(|content| content.trim().strip_prefix("ref: ").map(str::to_owned))
    {
        let reference = Path::new(".git").join(reference);
        if reference.is_file() {
            println!("cargo:rerun-if-changed={}", reference.display());
        }
    }
    if Path::new(".git/packed-refs").is_file() {
        println!("cargo:rerun-if-changed=.git/packed-refs");
    }
}

fn main() {
    println!("cargo:rerun-if-env-changed={VERSION_OVERRIDE}");
    track_git_head();

    let version = env::var(VERSION_OVERRIDE)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .or_else(|| git(&["describe", "--tags", "--exact-match"]))
        .or_else(|| git(&["rev-parse", "--short", "HEAD"]))
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_owned());

    println!("cargo:rustc-env=BUILD_VERSION={version}");
}
