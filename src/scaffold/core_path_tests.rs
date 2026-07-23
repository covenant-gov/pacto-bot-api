#![cfg(test)]
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]
#![allow(clippy::panic)]

use crate::scaffold::cache::Cache;
use crate::scaffold::generate::{ScaffoldMode, ScaffoldRequest, run_scaffold_with_cache};
use crate::scaffold::render::{check_cargo_generate_path, render_template};
use crate::scaffold::resolve::{Resolver, ResolverConfig};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Local template repository root used by the core path tests.
fn fixture_repo_root() -> PathBuf {
    PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/templates"
    ))
}

/// Cached `python-llm` template directory inside the fixture repo.
fn fixture_template_dir() -> PathBuf {
    fixture_repo_root().join("python-llm")
}

/// Build a representative scaffold request rooted at `project_dir`.
fn test_request(project_dir: PathBuf) -> ScaffoldRequest {
    ScaffoldRequest {
        bot_id: "echo-bot".to_string(),
        language: "python".to_string(),
        kind: "llm".to_string(),
        commands: vec!["echo".to_string()],
        with_tests: true,
        http: false,
        force: false,
        allow_hooks: false,
        project_dir,
        template_repo: fixture_repo_root().to_string_lossy().to_string(),
        template_ref: None,
        refresh: false,
        mode: ScaffoldMode::NewProject {
            snippet: r#"[[bots]]
id = "echo-bot"
npub = "npub1echo"
"#
            .to_string(),
        },
    }
}

#[tokio::test]
async fn resolver_resolve_local_fixture() {
    let cache_dir = tempfile::tempdir().unwrap();
    let cache = Cache::at(cache_dir.path().to_path_buf()).unwrap();

    let config = ResolverConfig {
        template_repo: fixture_repo_root().to_string_lossy().to_string(),
        template_ref: None,
        language: "python".to_string(),
        kind: "llm".to_string(),
        refresh: false,
    };

    let resolver = Resolver::with_cache(config, cache).unwrap();
    let bundle = resolver.resolve().await.unwrap();

    assert_eq!(bundle.manifest.language, "python");
    assert_eq!(bundle.manifest.kind, "llm");
    assert!(
        bundle.template_dir.ends_with("python-llm"),
        "template dir should be python-llm, got {}",
        bundle.template_dir.display()
    );
    assert!(bundle.template_dir.is_dir());
    assert!(
        bundle
            .manifest
            .compatibility
            .daemon
            .range
            .matches(&semver::Version::parse(env!("CARGO_PKG_VERSION")).unwrap()),
        "fixture manifest should accept the current admin version"
    );
}

#[test]
fn render_template_local_fixture() {
    let template_dir = fixture_template_dir();
    let _workdir = tempfile::tempdir().unwrap();
    let request = test_request(_workdir.path().to_path_buf());

    let rendered = render_template(&template_dir, &request, false).unwrap();

    assert!(rendered.dir.is_dir());
    assert!(rendered.dir.join("bot").join("bot.py").exists());
    assert!(
        rendered
            .dir
            .join("bot")
            .join("tests")
            .join("test_handlers.py")
            .exists()
    );
}

#[tokio::test]
async fn run_scaffold_local_fixture() {
    let _project_dir = tempfile::tempdir().unwrap();
    let project_dir = _project_dir.path().to_path_buf();
    let _cache_dir = tempfile::tempdir().unwrap();
    let cache = Cache::at(_cache_dir.path().to_path_buf()).unwrap();
    let request = test_request(project_dir.clone());

    run_scaffold_with_cache(request, Some(cache)).await.unwrap();

    assert!(
        project_dir.join("pacto-bot-api.toml").exists(),
        "config should be created"
    );
    assert!(
        project_dir
            .join("bots")
            .join("echo-bot")
            .join("echo_bot.py")
            .exists(),
        "bot handler should be rendered"
    );
    assert!(
        project_dir
            .join(".pacto")
            .join("bots")
            .join("echo-bot")
            .join("scaffold.lock")
            .exists(),
        "scaffold lock should be written"
    );
}

#[test]
fn cargo_generate_missing_returns_install_error() {
    let err = check_cargo_generate_path(Path::new("/nonexistent/cargo-generate")).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("cargo-generate is not installed"),
        "missing binary should report installation error, got: {msg}"
    );
}

#[cfg(unix)]
#[test]
fn cargo_generate_outdated_returns_version_error() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let fake = dir.path().join("cargo-generate");
    {
        let mut file = fs::File::create(&fake).unwrap();
        writeln!(file, "#!/bin/sh").unwrap();
        writeln!(file, "echo 'cargo generate 0.1.0'").unwrap();
        file.flush().unwrap();
        // Explicitly close the handle before marking executable so Linux
        // does not reject execution with ETXTBSY while the file is still open.
    }
    let mut perms = fs::metadata(&fake).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&fake, perms).unwrap();

    let err = check_cargo_generate_path(&fake).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("is too old"),
        "outdated binary should report version error, got: {msg}"
    );
}
