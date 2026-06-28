//! Enforces that every plan requirement R1-R37 has coverage or explicit justification.

use std::path::PathBuf;
use std::process::Command;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn requirement_coverage_report_is_valid() {
    let root = workspace_root();
    let status = Command::new("python3")
        .arg(root.join("scripts/generate_requirement_coverage.py"))
        .current_dir(&root)
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .status()
        .expect("failed to run requirement coverage script");

    assert!(
        status.success(),
        "requirement coverage script failed; see requirements/report.md and requirements/report.json"
    );

    assert!(
        root.join("requirements/report.md").exists(),
        "coverage markdown report was not generated"
    );
    assert!(
        root.join("requirements/report.json").exists(),
        "coverage JSON report was not generated"
    );
}
