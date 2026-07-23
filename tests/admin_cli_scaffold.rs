mod common;

use assert_cmd::Command;
use predicates::prelude::*;
use std::error::Error;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

fn setup_scaffold_env(cmd: &mut Command, temp: &Path) -> Result<PathBuf, Box<dyn Error>> {
    let fixture_repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("templates");
    let cache_dir = temp.join("cache");
    fs::create_dir_all(cache_dir.join("contracts"))?;
    fs::create_dir_all(cache_dir.join("sdks"))?;
    fs::write(
        cache_dir
            .join("contracts")
            .join("pacto-contract-0.1.0.json"),
        "{}",
    )?;
    fs::write(
        cache_dir.join("sdks").join("pacto-bot-sdk-0.2.0.json"),
        "{}",
    )?;
    cmd.env("PACTO_TEMPLATE_REPO", &fixture_repo);
    cmd.env("PACTO_CACHE_DIR", &cache_dir);
    Ok(fixture_repo)
}
#[test]
fn new_scaffold_lock_file_contains_expected_fields() -> Result<(), Box<dyn Error>> {
    let temp = common::tempdir()?;
    let project_dir = temp.path().join("lock-check");

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args([
        "new",
        "--scaffold",
        "lock-bot",
        "--backend",
        "nsec",
        "--relays",
        "ws://localhost:7000",
        "--commands",
        "echo",
        "--project-dir",
        &project_dir.to_string_lossy(),
    ]);
    setup_scaffold_env(&mut cmd, temp.path())?;
    cmd.assert().success();

    let lock_path = project_dir
        .join(".pacto")
        .join("bots")
        .join("lock-bot")
        .join("scaffold.lock");
    let lock_text = fs::read_to_string(&lock_path)?;

    assert!(
        lock_text.contains("lock_version = 1"),
        "lock_version missing"
    );
    assert!(lock_text.contains("[template]"), "template table missing");
    assert!(
        lock_text.contains("path = \"python-llm\""),
        "template path missing"
    );
    assert!(lock_text.contains("[contract]"), "contract table missing");
    assert!(
        lock_text.contains("name = \"pacto-contract\""),
        "contract name missing"
    );
    assert!(lock_text.contains("[sdk]"), "sdk table missing");
    assert!(
        lock_text.contains("name = \"pacto-bot-sdk\""),
        "sdk name missing"
    );
    assert!(lock_text.contains("[admin]"), "admin table missing");
    assert!(
        lock_text.contains("version = \"0.9.0\""),
        "admin version missing"
    );

    Ok(())
}

#[test]
fn new_scaffold_creates_multi_bot_project() -> Result<(), Box<dyn Error>> {
    let temp = common::tempdir()?;
    let project_dir = temp.path().join("echo-bot");

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args([
        "new",
        "--scaffold",
        "echo-bot",
        "--backend",
        "nsec",
        "--relays",
        "ws://localhost:7000",
        "--commands",
        "echo",
        "--project-dir",
        &project_dir.to_string_lossy(),
    ]);
    setup_scaffold_env(&mut cmd, temp.path())?;
    cmd.assert().success();

    assert!(project_dir.join("pacto-bot-api.toml").is_file());
    assert!(
        project_dir
            .join(".pacto")
            .join("bots")
            .join("echo-bot")
            .join("scaffold.lock")
            .is_file()
    );
    assert!(
        project_dir
            .join("bots")
            .join("echo-bot")
            .join("echo_bot.py")
            .is_file()
    );
    assert!(
        project_dir
            .join("bots")
            .join("echo-bot")
            .join("Dockerfile")
            .is_file()
    );
    assert!(project_dir.join("docker-compose.yml").is_file());
    assert!(
        project_dir
            .join("bots")
            .join("echo-bot")
            .join("README.md")
            .is_file()
    );
    assert!(
        project_dir
            .join("bots")
            .join("echo-bot")
            .join("systemd.service")
            .is_file()
    );
    assert!(
        project_dir
            .join("bots")
            .join("echo-bot")
            .join("pyproject.toml")
            .is_file()
    );
    assert!(
        project_dir
            .join("bots")
            .join("echo-bot")
            .join("tests")
            .join("test_handlers.py")
            .is_file()
    );

    let config = fs::read_to_string(project_dir.join("pacto-bot-api.toml"))?;
    assert!(config.contains("id = \"echo-bot\""));
    assert!(config.contains("backend = \"nsec\""));
    assert!(config.contains("nsec = \"nsec1"));

    let handler = fs::read_to_string(
        project_dir
            .join("bots")
            .join("echo-bot")
            .join("echo_bot.py"),
    )?;
    assert!(handler.contains("@bot.command(\"/echo\")"));
    assert!(handler.contains("@bot.default"));
    assert!(
        handler.contains("bot.log("),
        "generated handler should log entry/exit"
    );

    let dockerfile =
        fs::read_to_string(project_dir.join("bots").join("echo-bot").join("Dockerfile"))?;
    assert!(dockerfile.contains("python:3.14-slim"));

    assert!(
        project_dir.join("docker").join("README.md").is_file(),
        "project docker support README must exist"
    );
    assert!(
        project_dir
            .join("docker")
            .join("relay-config.toml")
            .is_file(),
        "project placeholder relay config must exist"
    );

    let compose = fs::read_to_string(project_dir.join("docker-compose.yml"))?;
    assert!(
        compose.contains("ghcr.io/covenant-gov/pacto-bot-api:latest"),
        "daemon image must be the published covenant-gov image"
    );
    assert!(
        compose.contains("nip46-bunker"),
        "compose must include the NIP-46 bunker service"
    );
    assert!(
        compose.contains("nostr-relay"),
        "compose must include the Nostr relay service"
    );
    assert!(
        compose.contains("./pacto-bot-api.toml:/etc/pacto/pacto-bot-api.toml:ro"),
        "daemon must mount the generated config"
    );
    assert!(
        !compose.contains("pacto/pacto-bunker"),
        "compose must not reference the non-existent bunker image"
    );

    assert!(
        compose.contains("restart: on-failure"),
        "compose must include restart: on-failure for the bot service"
    );

    let systemd = fs::read_to_string(
        project_dir
            .join("bots")
            .join("echo-bot")
            .join("systemd.service"),
    )?;
    assert!(
        systemd.contains("Restart=on-failure"),
        "systemd service must include Restart=on-failure"
    );

    let readme = fs::read_to_string(project_dir.join("bots").join("echo-bot").join("README.md"))?;
    assert!(readme.contains("echo-bot"));
    assert!(!readme.contains("nsec1"));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(project_dir.join("pacto-bot-api.toml"))?
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "config must be 0o600");
    }

    Ok(())
}

#[test]
fn new_scaffold_project_name_sets_project_directory() -> Result<(), Box<dyn Error>> {
    let temp = common::tempdir()?;
    let project_dir = temp.path().join("my-custom-project");

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args([
        "new",
        "--scaffold",
        "echo-bot",
        "--backend",
        "nsec",
        "--relays",
        "ws://localhost:7000",
        "--commands",
        "echo",
        "--project-name",
        "my-custom-project",
    ]);
    cmd.current_dir(&temp);
    setup_scaffold_env(&mut cmd, temp.path())?;
    cmd.assert().success();

    assert!(project_dir.join("pacto-bot-api.toml").is_file());
    assert!(
        project_dir
            .join("bots")
            .join("echo-bot")
            .join("echo_bot.py")
            .is_file()
    );
    assert!(
        project_dir
            .join("bots")
            .join("echo-bot")
            .join("README.md")
            .is_file()
    );

    Ok(())
}

#[test]
fn new_scaffold_uses_project_name_default() -> Result<(), Box<dyn Error>> {
    let temp = common::tempdir()?;
    let project_dir = temp.path().join("echo-bot-project");

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args([
        "new",
        "--scaffold",
        "echo-bot",
        "--backend",
        "nsec",
        "--relays",
        "ws://localhost:7000",
        "--commands",
        "echo",
    ]);
    cmd.current_dir(&temp);
    setup_scaffold_env(&mut cmd, temp.path())?;
    cmd.assert().success();

    assert!(project_dir.join("pacto-bot-api.toml").is_file());
    assert!(
        project_dir
            .join("bots")
            .join("echo-bot")
            .join("echo_bot.py")
            .is_file()
    );

    Ok(())
}

#[test]
fn new_scaffold_project_dir_overrides_project_name() -> Result<(), Box<dyn Error>> {
    let temp = common::tempdir()?;
    let project_dir = temp.path().join("explicit-project");

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args([
        "new",
        "--scaffold",
        "echo-bot",
        "--backend",
        "nsec",
        "--relays",
        "ws://localhost:7000",
        "--commands",
        "echo",
        "--project-name",
        "ignored-name",
        "--project-dir",
        &project_dir.to_string_lossy(),
    ]);
    cmd.current_dir(&temp);
    setup_scaffold_env(&mut cmd, temp.path())?;
    cmd.assert().success();

    assert!(project_dir.join("pacto-bot-api.toml").is_file());
    assert!(
        project_dir
            .join("bots")
            .join("echo-bot")
            .join("echo_bot.py")
            .is_file()
    );

    // Ensure the project-name was ignored in favor of project-dir.
    assert!(!temp.path().join("ignored-name").exists());

    Ok(())
}

#[test]
fn new_scaffold_no_tests_skips_test_files() -> Result<(), Box<dyn Error>> {
    let temp = common::tempdir()?;
    let project_dir = temp.path().join("echo-bot-project");

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args([
        "new",
        "--scaffold",
        "--no-tests",
        "echo-bot",
        "--backend",
        "nsec",
        "--relays",
        "ws://localhost:7000",
    ]);
    cmd.current_dir(&temp);
    setup_scaffold_env(&mut cmd, temp.path())?;
    cmd.assert().success();

    assert!(
        project_dir
            .join("bots")
            .join("echo-bot")
            .join("echo_bot.py")
            .is_file()
    );
    assert!(
        !project_dir
            .join("bots")
            .join("echo-bot")
            .join("tests")
            .exists()
    );

    Ok(())
}

#[test]
fn scaffold_fails_when_bot_not_in_config() -> Result<(), Box<dyn Error>> {
    let temp = common::tempdir()?;

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args([
        "scaffold",
        "missing-bot",
        "--project-dir",
        &temp.path().to_string_lossy(),
    ]);
    cmd.assert()
        .failure()
        .stderr(predicate::str::contains("not found"));

    Ok(())
}

#[test]
fn scaffold_with_tests_adds_tests_without_overwriting_handler() -> Result<(), Box<dyn Error>> {
    let temp = common::tempdir()?;
    let project_dir = temp.path().join("echo-bot");

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args([
        "new",
        "--scaffold",
        "--no-tests",
        "echo-bot",
        "--backend",
        "nsec",
        "--relays",
        "ws://localhost:7000",
        "--commands",
        "echo",
        "--project-dir",
        &project_dir.to_string_lossy(),
    ]);
    setup_scaffold_env(&mut cmd, temp.path())?;
    cmd.assert().success();

    let handler_before = fs::read_to_string(
        project_dir
            .join("bots")
            .join("echo-bot")
            .join("echo_bot.py"),
    )?;

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args([
        "scaffold",
        "echo-bot",
        "--with-tests",
        "--project-dir",
        &project_dir.to_string_lossy(),
    ]);
    setup_scaffold_env(&mut cmd, temp.path())?;
    cmd.assert().success();

    let handler_after = fs::read_to_string(
        project_dir
            .join("bots")
            .join("echo-bot")
            .join("echo_bot.py"),
    )?;
    assert_eq!(handler_before, handler_after);
    assert!(
        project_dir
            .join("bots")
            .join("echo-bot")
            .join("tests")
            .join("test_handlers.py")
            .is_file()
    );

    Ok(())
}

#[test]
fn scaffold_adds_second_bot_to_multi_bot_project() -> Result<(), Box<dyn Error>> {
    let temp = common::tempdir()?;
    let project_dir = temp.path().join("multi-bot");

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args([
        "new",
        "--scaffold",
        "echo-bot",
        "--backend",
        "nsec",
        "--relays",
        "ws://localhost:7000",
        "--commands",
        "echo",
        "--project-dir",
        &project_dir.to_string_lossy(),
    ]);
    setup_scaffold_env(&mut cmd, temp.path())?;
    cmd.assert().success();

    // Add a second bot identity to the shared config.
    let snippet_path = temp.path().join("price-bot.toml");
    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args([
        "new",
        "price-bot",
        "--backend",
        "nsec",
        "--relays",
        "ws://localhost:7000",
        "--output",
        &snippet_path.to_string_lossy(),
    ]);
    setup_scaffold_env(&mut cmd, temp.path())?;
    cmd.assert().success();

    let snippet = fs::read_to_string(&snippet_path)?;

    let config_path = project_dir.join("pacto-bot-api.toml");
    fs::OpenOptions::new()
        .append(true)
        .open(&config_path)?
        .write_all(snippet.as_bytes())?;

    // Scaffold the second bot, forcing compose merge.
    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args([
        "scaffold",
        "price-bot",
        "--commands",
        "price",
        "--force",
        "--project-dir",
        &project_dir.to_string_lossy(),
    ]);
    setup_scaffold_env(&mut cmd, temp.path())?;
    cmd.assert().success();

    assert!(
        project_dir
            .join("bots")
            .join("price-bot")
            .join("price_bot.py")
            .is_file()
    );

    let compose = fs::read_to_string(project_dir.join("docker-compose.yml"))?;
    assert!(compose.contains("price-bot:"));
    assert!(compose.contains("pacto-socket:/run/pacto:ro"));
    assert!(compose.contains("depends_on:"));

    let config = fs::read_to_string(&config_path)?;
    assert!(config.contains("id = \"echo-bot\""));
    assert!(config.contains("id = \"price-bot\""));

    Ok(())
}

#[test]
fn scaffold_force_overwrites_readme_but_not_config() -> Result<(), Box<dyn Error>> {
    let temp = common::tempdir()?;
    let project_dir = temp.path().join("echo-bot");

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args([
        "new",
        "--scaffold",
        "echo-bot",
        "--backend",
        "nsec",
        "--relays",
        "ws://localhost:7000",
        "--commands",
        "echo",
        "--project-dir",
        &project_dir.to_string_lossy(),
    ]);
    setup_scaffold_env(&mut cmd, temp.path())?;
    cmd.assert().success();

    fs::write(
        project_dir.join("bots").join("echo-bot").join("README.md"),
        "# custom readme\n",
    )?;

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args([
        "scaffold",
        "echo-bot",
        "--force",
        "--project-dir",
        &project_dir.to_string_lossy(),
    ]);
    setup_scaffold_env(&mut cmd, temp.path())?;
    cmd.assert().success();

    let readme = fs::read_to_string(project_dir.join("bots").join("echo-bot").join("README.md"))?;
    assert!(!readme.contains("# custom readme"));

    let config = fs::read_to_string(project_dir.join("pacto-bot-api.toml"))?;
    assert!(config.contains("id = \"echo-bot\""));

    Ok(())
}

#[test]
fn generated_files_contain_no_real_secrets_except_config() -> Result<(), Box<dyn Error>> {
    let temp = common::tempdir()?;
    let project_dir = temp.path().join("echo-bot");

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args([
        "new",
        "--scaffold",
        "echo-bot",
        "--backend",
        "nsec",
        "--relays",
        "ws://localhost:7000",
        "--commands",
        "echo",
        "--project-dir",
        &project_dir.to_string_lossy(),
    ]);
    setup_scaffold_env(&mut cmd, temp.path())?;
    cmd.assert().success();

    let nsec_value = extract_nsec(&project_dir.join("pacto-bot-api.toml"))?;

    for entry in walk_files(&project_dir)? {
        if entry == project_dir.join("pacto-bot-api.toml") {
            continue;
        }
        let content = fs::read(&entry)?;
        assert!(
            !content
                .windows(nsec_value.len())
                .any(|w| w == nsec_value.as_bytes()),
            "{} leaked nsec value",
            entry.display()
        );
    }

    Ok(())
}

#[test]
fn new_interactive_scaffold_prompts_and_creates_project() -> Result<(), Box<dyn Error>> {
    let temp = common::tempdir()?;
    let project_dir = temp.path().join("interactive-bot-project");

    let stdin = format!(
        "interactive-bot\n1\n\n\n\n\n\ny\nn\n{}\n\necho\ny\n",
        project_dir.to_string_lossy()
    );

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.arg("new").write_stdin(stdin);
    setup_scaffold_env(&mut cmd, temp.path())?;
    cmd.assert().success().stdout(predicate::str::contains(
        "Created scaffolded project for interactive-bot",
    ));

    assert!(project_dir.join("pacto-bot-api.toml").is_file());
    assert!(
        project_dir
            .join(".pacto")
            .join("bots")
            .join("interactive-bot")
            .join("scaffold.lock")
            .is_file()
    );
    assert!(
        project_dir
            .join("bots")
            .join("interactive-bot")
            .join("interactive_bot.py")
            .is_file()
    );
    assert!(
        project_dir
            .join("bots")
            .join("interactive-bot")
            .join("tests")
            .join("test_handlers.py")
            .is_file()
    );

    let config = fs::read_to_string(project_dir.join("pacto-bot-api.toml"))?;
    assert!(config.contains("id = \"interactive-bot\""));
    assert!(config.contains("backend = \"nsec\""));
    assert!(config.contains("nsec = \"nsec1"));

    Ok(())
}

#[test]
fn new_scaffold_with_http_adds_http_dependencies_and_tests() -> Result<(), Box<dyn Error>> {
    let temp = common::tempdir()?;
    let project_dir = temp.path().join("price-bot");

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args([
        "new",
        "--scaffold",
        "price-bot",
        "--backend",
        "nsec",
        "--relays",
        "ws://localhost:7000",
        "--commands",
        "price",
        "--http",
        "--project-dir",
        &project_dir.to_string_lossy(),
    ]);
    setup_scaffold_env(&mut cmd, temp.path())?;
    cmd.assert().success();

    let pyproject = fs::read_to_string(
        project_dir
            .join("bots")
            .join("price-bot")
            .join("pyproject.toml"),
    )?;
    assert!(pyproject.contains("httpx>=0.27"));
    assert!(pyproject.contains("respx>=0.22"));

    let http_test = fs::read_to_string(
        project_dir
            .join("bots")
            .join("price-bot")
            .join("tests")
            .join("test_http.py"),
    )?;
    assert!(http_test.contains("import httpx"));
    assert!(http_test.contains("import respx"));

    Ok(())
}

fn extract_nsec(path: &Path) -> Result<String, Box<dyn Error>> {
    let content = fs::read_to_string(path)?;
    let start = content
        .find("nsec = \"")
        .ok_or("nsec not found in config")?;
    let start = start + "nsec = \"".len();
    let end = content[start..].find('"').ok_or("unterminated nsec")?;
    Ok(content[start..start + end].to_string())
}

fn walk_files(dir: &Path) -> Result<Vec<PathBuf>, Box<dyn Error>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            files.extend(walk_files(&path)?);
        } else {
            files.push(path);
        }
    }
    Ok(files)
}
