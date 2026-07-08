use std::env;
use std::path::Path;
use std::process::Command;

fn main() {
    // The build prefers an explicit GIT_COMMIT_SHORT passed in by the build
    // environment (CI, Docker, packaging scripts). This works even when the
    // source tree is not a git checkout (e.g. Docker's build context does not
    // include `.git`).
    let commit = env::var("GIT_COMMIT_SHORT")
        .ok()
        .filter(|s| !s.is_empty() && s != "unknown")
        .inspect(|_| {
            println!("cargo:rerun-if-env-changed=GIT_COMMIT_SHORT");
        })
        .or_else(|| {
            // Re-run the build script when HEAD changes so the embedded commit
            // stays up to date for local builds.
            if Path::new(".git").exists() {
                println!("cargo:rerun-if-changed=.git/HEAD");
            }

            Command::new("git")
                .args(["rev-parse", "--short=8", "HEAD"])
                .output()
                .ok()
                .filter(|output| output.status.success())
                .and_then(|output| String::from_utf8(output.stdout).ok())
                .map(|s| s.trim().to_string())
        })
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=GIT_COMMIT_SHORT={commit}");
}
