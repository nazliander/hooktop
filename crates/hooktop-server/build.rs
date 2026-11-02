//! Stamp the git commit into the binary so `hooktop --version` and the TUI
//! show exactly which build is running.

fn main() {
    println!(
        "cargo:rustc-env=HOOKTOP_BUILD={}",
        packaged_sha().or_else(git_sha).unwrap_or_else(|| "unknown".into())
    );
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs/heads");
}

/// A crate from crates.io isn't a git checkout, but `cargo publish` records
/// the commit it was packaged from in `.cargo_vcs_info.json`.
fn packaged_sha() -> Option<String> {
    let dir = std::env::var("CARGO_MANIFEST_DIR").ok()?;
    let info = std::fs::read_to_string(std::path::Path::new(&dir).join(".cargo_vcs_info.json")).ok()?;
    let rest = &info[info.find("\"sha1\"")? + 6..];
    let start = rest.find('"')? + 1;
    let sha = &rest[start..start + rest[start..].find('"')?];
    Some(sha.chars().take(7).collect())
}

fn git_sha() -> Option<String> {
    std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}
