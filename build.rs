use std::path::Path;
use std::process::Command;

fn main() {
    // Re-run the build script when the HEAD changes so the embedded commit
    // stays up to date. If the source tree is not a git checkout (e.g. a
    // crate tarball), fall back to "unknown" and skip the watch.
    if Path::new(".git").exists() {
        println!("cargo:rerun-if-changed=.git/HEAD");
    }

    let commit = Command::new("git")
        .args(["rev-parse", "--short=8", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=GIT_COMMIT_SHORT={commit}");
}
