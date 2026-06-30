mod common;

use assert_cmd::Command;
use predicates::prelude::*;
use std::error::Error;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

#[test]
fn new_scaffold_creates_multi_bot_project() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
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
            .join("test_bot.py")
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

    let dockerfile =
        fs::read_to_string(project_dir.join("bots").join("echo-bot").join("Dockerfile"))?;
    assert!(dockerfile.contains("python:3.14-slim"));

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
fn new_scaffold_no_tests_skips_test_files() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
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
    ]);
    cmd.current_dir(&temp);
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
    let temp = tempfile::tempdir()?;

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
    let temp = tempfile::tempdir()?;
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
            .join("test_bot.py")
            .is_file()
    );

    Ok(())
}

#[test]
fn scaffold_adds_second_bot_to_multi_bot_project() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
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
    cmd.assert().success();

    // Add a second bot identity to the shared config.
    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.args([
        "new",
        "price-bot",
        "--backend",
        "nsec",
        "--relays",
        "ws://localhost:7000",
    ]);
    let output = cmd.assert().success();
    let snippet = std::str::from_utf8(&output.get_output().stdout)?;

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
    assert!(compose.contains("bot-only"));
    assert!(compose.contains("full"));

    let config = fs::read_to_string(&config_path)?;
    assert!(config.contains("id = \"echo-bot\""));
    assert!(config.contains("id = \"price-bot\""));

    Ok(())
}

#[test]
fn scaffold_force_overwrites_readme_but_not_config() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
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
    cmd.assert().success();

    let readme = fs::read_to_string(project_dir.join("bots").join("echo-bot").join("README.md"))?;
    assert!(!readme.contains("# custom readme"));

    let config = fs::read_to_string(project_dir.join("pacto-bot-api.toml"))?;
    assert!(config.contains("id = \"echo-bot\""));

    Ok(())
}

#[test]
fn generated_files_contain_no_real_secrets_except_config() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
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
    cmd.assert().success();

    let nsec_value = extract_nsec(&project_dir.join("pacto-bot-api.toml"))?;

    for entry in walk_files(&project_dir)? {
        if entry == project_dir.join("pacto-bot-api.toml") {
            continue;
        }
        let content = fs::read_to_string(&entry)?;
        assert!(
            !content.contains(&nsec_value),
            "{} leaked nsec value",
            entry.display()
        );
    }

    Ok(())
}

#[test]
fn new_interactive_scaffold_prompts_and_creates_project() -> Result<(), Box<dyn Error>> {
    let temp = tempfile::tempdir()?;
    let project_dir = temp.path().join("interactive-bot-project");

    let stdin = format!(
        "interactive-bot\n1\n\n\n\n\n\ny\n{}\n\necho\ny\n",
        project_dir.to_string_lossy()
    );

    let mut cmd = Command::cargo_bin("pacto-bot-admin")?;
    cmd.arg("new").write_stdin(stdin);
    cmd.assert().success().stdout(predicate::str::contains(
        "Created scaffolded project for interactive-bot",
    ));

    assert!(project_dir.join("pacto-bot-api.toml").is_file());
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
            .join("test_bot.py")
            .is_file()
    );

    let config = fs::read_to_string(project_dir.join("pacto-bot-api.toml"))?;
    assert!(config.contains("id = \"interactive-bot\""));
    assert!(config.contains("backend = \"nsec\""));
    assert!(config.contains("nsec = \"nsec1"));

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
