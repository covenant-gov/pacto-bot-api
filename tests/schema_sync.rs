//! Ensures generated Rust types stay in sync with canonical schemas.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn generated_types_match_schemas() {
    let root = workspace_root();

    let tracked_files = [
        "src/config_generated.rs",
        "src/metrics_generated.rs",
        "src/transport/protocol_generated.rs",
    ];

    // Snapshot the committed contents before codegen overwrites them in-place.
    let committed: Vec<String> = tracked_files
        .iter()
        .map(|path| {
            let committed_path = root.join(path);
            fs::read_to_string(&committed_path)
                .unwrap_or_else(|e| panic!("failed to read {}: {}", committed_path.display(), e))
        })
        .collect();

    // Run the codegen xtask against the current working tree.
    let status = Command::new("cargo")
        .args(["xtask", "codegen"])
        .current_dir(&root)
        .status()
        .expect("failed to run cargo xtask codegen");
    assert!(status.success(), "cargo xtask codegen failed");

    // Compare freshly generated files to the committed snapshot.
    for (path, committed) in tracked_files.iter().zip(committed) {
        let generated_path = root.join(path);
        let generated = fs::read_to_string(&generated_path)
            .unwrap_or_else(|e| panic!("failed to read {}: {}", generated_path.display(), e));

        assert_eq!(
            generated,
            committed,
            "generated file {} does not match committed version; run `cargo xtask codegen`",
            generated_path.display()
        );
    }
}
