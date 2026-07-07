//! Project generation logic for the scaffold command.

use crate::scaffold::cache::Cache;
use crate::scaffold::lock::{ScaffoldLock, ScaffoldSettings, lock_path, write_lock};
use crate::scaffold::merge::{MergeContext, merge_rendered};
use crate::scaffold::render::render_template;
use crate::scaffold::resolve::{Resolver, ResolverConfig};
use crate::scaffold::safety::{
    OverwritePolicy, decide_write, is_populated_config, set_config_permissions,
    set_project_dir_permissions,
};
use pacto_bot_api::config::BotConfig;
use pacto_bot_api::errors::DaemonError;
use secrecy::ExposeSecret;
use std::fs;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

/// What kind of scaffold invocation is running.
#[derive(Debug, Clone)]
pub enum ScaffoldMode {
    /// Create a brand-new bot identity and scaffold a project around it.
    NewProject { snippet: String },
    /// Scaffold files for an existing bot identity already present in config.
    ExistingProject { bot_config: BotConfig },
}

/// Request to generate a bot handler project.
#[derive(Debug, Clone)]
pub struct ScaffoldRequest {
    pub bot_id: String,
    pub language: String,
    pub kind: String,
    pub commands: Vec<String>,
    pub with_tests: bool,
    pub http: bool,
    pub force: bool,
    pub allow_hooks: bool,
    pub project_dir: PathBuf,
    pub template_repo: String,
    pub template_ref: Option<String>,
    pub refresh: bool,
    pub mode: ScaffoldMode,
}

/// Generate the project files described by `request`.
///
/// This is the entry point used by both `pacto-bot-admin new --scaffold` and
/// `pacto-bot-admin scaffold`.
pub async fn run_scaffold(request: ScaffoldRequest) -> Result<(), DaemonError> {
    run_scaffold_with_cache(request, None).await
}

/// Generate project files with an explicit cache, used by tests.
pub(crate) async fn run_scaffold_with_cache(
    request: ScaffoldRequest,
    cache: Option<Cache>,
) -> Result<(), DaemonError> {
    validate_commands(&request.commands)?;

    let config = ResolverConfig {
        template_repo: request.template_repo.clone(),
        template_ref: request.template_ref.clone(),
        language: request.language.clone(),
        kind: request.kind.clone(),
        refresh: request.refresh,
    };

    let resolver = match cache {
        Some(c) => Resolver::with_cache(config, c)?,
        None => Resolver::new(config)?,
    };
    let bundle = resolver.resolve().await?;
    let triple = bundle.triple;
    let template_dir = bundle.template_dir;

    let rendered = render_template(&template_dir, &request, request.allow_hooks)?;

    let policy = OverwritePolicy {
        force: request.force,
        interactive: std::io::stdin().is_terminal(),
        skip_existing: matches!(request.mode, ScaffoldMode::ExistingProject { .. }),
    };

    let manifest = bundle.manifest;

    let mut denylist = Vec::new();
    let bot_dir = request.project_dir.join("bots").join(&request.bot_id);
    let bot_file_name = format!(
        "{}.py",
        crate::scaffold::merge::bot_id_snake(&request.bot_id)
    );
    for protected in &manifest.protected_files {
        let resolved = if protected == "bot.py" {
            bot_file_name.clone()
        } else {
            protected.clone()
        };
        if resolved == "pacto-bot-api.toml" {
            denylist.push(request.project_dir.join(&resolved));
        } else {
            denylist.push(bot_dir.join(&resolved));
        }
    }
    denylist.push(request.project_dir.join("pacto-bot-api.toml"));

    fs::create_dir_all(&request.project_dir).map_err(DaemonError::Io)?;
    set_project_dir_permissions(&request.project_dir)?;

    match &request.mode {
        ScaffoldMode::NewProject { snippet } => {
            write_config_snippet(
                &request.project_dir.join("pacto-bot-api.toml"),
                snippet,
                &policy,
                &denylist,
            )?;
        }
        ScaffoldMode::ExistingProject { bot_config } => {
            append_config_entry(&request.project_dir.join("pacto-bot-api.toml"), bot_config)?;
        }
    }

    let merge_context = MergeContext {
        project_dir: request.project_dir.clone(),
        bot_id: request.bot_id.clone(),
        policy,
        denylist,
        append_compose: matches!(request.mode, ScaffoldMode::ExistingProject { .. }),
    };
    merge_rendered(&rendered.dir, &merge_context)?;

    let lock = ScaffoldLock {
        lock_version: crate::scaffold::lock::LOCK_VERSION,
        triple,
        scaffold: ScaffoldSettings {
            commands: request.commands.clone(),
            with_tests: request.with_tests,
            http: request.http,
        },
    };
    let lock_file = lock_path(&request.project_dir, &request.bot_id);
    write_lock(&lock_file, &lock)?;

    Ok(())
}

fn validate_commands(commands: &[String]) -> Result<(), DaemonError> {
    for cmd in commands {
        if cmd.is_empty() {
            return Err(DaemonError::Config(
                "command names must not be empty".into(),
            ));
        }
        if !cmd.chars().all(|c| c.is_ascii_lowercase() || c == '_') {
            return Err(DaemonError::Config(format!(
                "invalid command name '{cmd}': use lowercase letters or underscores only"
            )));
        }
    }
    Ok(())
}

fn write_config_snippet(
    path: &Path,
    snippet: &str,
    policy: &OverwritePolicy,
    denylist: &[PathBuf],
) -> Result<(), DaemonError> {
    let daemon_section = r#"[daemon]
data_dir = "${PACTO_DATA_DIR:-~/.local/share/pacto-bot-api}"
socket_path = "${PACTO_SOCKET_PATH:-~/.local/share/pacto-bot-api/pacto-bot-api.sock}"

"#;
    let full_snippet = format!("{daemon_section}{snippet}");

    match decide_write(path, policy, denylist, &mut prompt_overwrite)? {
        crate::scaffold::safety::WriteDecision::Write => {
            fs::write(path, &full_snippet).map_err(DaemonError::Io)?;
            set_config_permissions(path)?;
            println!("Created {}", path.display());
            Ok(())
        }
        crate::scaffold::safety::WriteDecision::Skip => {
            println!("Skipped {}", path.display());
            Ok(())
        }
        crate::scaffold::safety::WriteDecision::Abort => unreachable!(),
    }
}

fn append_config_entry(path: &Path, bot_config: &BotConfig) -> Result<(), DaemonError> {
    let snippet = bot_config_to_snippet(bot_config)?;

    if !path.exists() {
        fs::write(path, &snippet).map_err(DaemonError::Io)?;
        set_config_permissions(path)?;
        println!("Created {}", path.display());
        return Ok(());
    }

    if is_populated_config(path) {
        let existing = parse_existing_config(path)?;
        if existing.bots.iter().any(|b| b.id == bot_config.id) {
            println!(
                "Skipped [[bots]] entry for {}: already exists in {}",
                bot_config.id,
                path.display()
            );
            return Ok(());
        }
    }

    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(path)
        .map_err(DaemonError::Io)?;
    file.write_all(b"\n").map_err(DaemonError::Io)?;
    file.write_all(snippet.as_bytes())
        .map_err(DaemonError::Io)?;
    set_config_permissions(path)?;
    println!("Appended [[bots]] entry to {}", path.display());
    Ok(())
}

fn parse_existing_config(path: &Path) -> Result<pacto_bot_api::config::DaemonConfig, DaemonError> {
    let raw = fs::read_to_string(path).map_err(DaemonError::Io)?;
    toml::from_str(&raw).map_err(|e| {
        DaemonError::Config(format!(
            "failed to parse existing config {}: {e}",
            path.display()
        ))
    })
}

fn bot_config_to_snippet(bot_config: &BotConfig) -> Result<String, DaemonError> {
    let mut lines = Vec::new();
    lines.push("[[bots]]".to_string());
    lines.push(format!("id = {:?}", bot_config.id));
    lines.push(format!("npub = {:?}", bot_config.npub));

    match &bot_config.signing {
        pacto_bot_api::config::SigningConfig::Nsec { nsec } => {
            let nsec = nsec.expose_secret();
            lines.push(format!(
                "signing = {{ backend = \"nsec\", nsec = {nsec:?} }}"
            ));
        }
        pacto_bot_api::config::SigningConfig::BunkerLocal { uri } => {
            let uri = uri.expose_secret();
            lines.push(format!(
                "signing = {{ backend = \"bunker_local\", uri = \"${{PACTO_BUNKER_URI:-{uri}}}\" }}"
            ));
        }
        pacto_bot_api::config::SigningConfig::BunkerRemote { uri } => {
            let uri = uri.expose_secret();
            lines.push(format!(
                "signing = {{ backend = \"bunker_remote\", uri = \"${{PACTO_BUNKER_URI:-{uri}}}\" }}"
            ));
        }
    }

    match bot_config.relays.len() {
        0 => lines.push("relays = [\"${PACTO_RELAY_URL:-ws://localhost:7000}\"]".to_string()),
        1 => lines.push(format!(
            "relays = [\"${{PACTO_RELAY_URL:-{}}}\"]",
            bot_config.relays[0]
        )),
        _ => lines.push(format!(
            "relays = {}",
            format_toml_array(&bot_config.relays)
        )),
    }
    lines.push(format!(
        "capabilities = {}",
        format_toml_array(&bot_config.capabilities)
    ));

    if let Some(display_name) = &bot_config.display_name {
        lines.push(format!("display_name = {display_name:?}"));
    }
    if let Some(about) = &bot_config.about {
        lines.push(format!("about = {about:?}"));
    }
    if let Some(picture) = &bot_config.picture {
        lines.push(format!("picture = {picture:?}"));
    }

    Ok(lines.join("\n") + "\n")
}

fn format_toml_array(items: &[String]) -> String {
    if items.is_empty() {
        "[]".to_string()
    } else {
        format!(
            "[{}]",
            items
                .iter()
                .map(|s| format!("{s:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

fn prompt_overwrite(path: &Path) -> Result<bool, DaemonError> {
    println!("File {} already exists. Overwrite? [y/N]:", path.display());
    let mut buf = String::new();
    std::io::stdin()
        .read_line(&mut buf)
        .map_err(DaemonError::Io)?;
    Ok(buf.trim().eq_ignore_ascii_case("y"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    #![allow(clippy::panic)]

    use super::*;
    use pacto_bot_api::config::{BotConfig, SigningConfig};
    use secrecy::SecretString;

    #[test]
    fn validate_commands_rejects_invalid_names() {
        assert!(validate_commands(&["echo".to_string(), "help_me".to_string()]).is_ok());
        assert!(validate_commands(&["Echo".to_string()]).is_err());
        assert!(validate_commands(&["echo!".to_string()]).is_err());
        assert!(validate_commands(&["help-me".to_string()]).is_err());
    }

    #[test]
    fn bot_config_to_snippet_preserves_nsec() {
        let bot = BotConfig {
            id: "echo-bot".to_string(),
            npub: "npub1echo".to_string(),
            signing: SigningConfig::Nsec {
                nsec: SecretString::new("nsec1secret".into()),
            },
            relays: vec!["ws://localhost:7000".to_string()],
            capabilities: vec!["ReadMessages".to_string(), "SendMessages".to_string()],
            ..Default::default()
        };
        let snippet = bot_config_to_snippet(&bot).unwrap();
        assert!(snippet.contains("id = \"echo-bot\""));
        assert!(snippet.contains("nsec = \"nsec1secret\""));
        assert!(snippet.contains("backend = \"nsec\""));
    }

    #[test]
    fn format_toml_array_handles_empty_and_items() {
        assert_eq!(format_toml_array(&[]), "[]");
        assert_eq!(
            format_toml_array(&["a".into(), "b c".into()]),
            "[\"a\", \"b c\"]"
        );
    }

    #[test]
    fn append_config_entry_creates_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pacto-bot-api.toml");
        let bot = BotConfig {
            id: "echo-bot".to_string(),
            npub: "npub1echo".to_string(),
            signing: SigningConfig::Nsec {
                nsec: SecretString::new("nsec1secret".into()),
            },
            ..Default::default()
        };
        append_config_entry(&path, &bot).unwrap();
        assert!(path.exists());
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("[[bots]]"));
        assert!(content.contains("id = \"echo-bot\""));
    }

    #[test]
    fn append_config_entry_appends_different_bot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pacto-bot-api.toml");
        let bot1 = BotConfig {
            id: "echo-bot".to_string(),
            npub: "npub1echo".to_string(),
            signing: SigningConfig::Nsec {
                nsec: SecretString::new("nsec1secret".into()),
            },
            ..Default::default()
        };
        let bot2 = BotConfig {
            id: "help-bot".to_string(),
            npub: "npub1help".to_string(),
            signing: SigningConfig::Nsec {
                nsec: SecretString::new("nsec1other".into()),
            },
            ..Default::default()
        };
        append_config_entry(&path, &bot1).unwrap();
        append_config_entry(&path, &bot2).unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(content.matches("[[bots]]").count(), 2);
        assert!(content.contains("id = \"echo-bot\""));
        assert!(content.contains("id = \"help-bot\""));
    }

    #[test]
    fn append_config_entry_skips_duplicate_bot_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pacto-bot-api.toml");
        let bot1 = BotConfig {
            id: "echo-bot".to_string(),
            npub: "npub1echo".to_string(),
            signing: SigningConfig::Nsec {
                nsec: SecretString::new("nsec1secret".into()),
            },
            ..Default::default()
        };
        let bot2 = BotConfig {
            id: "echo-bot".to_string(),
            npub: "npub1different".to_string(),
            signing: SigningConfig::Nsec {
                nsec: SecretString::new("nsec1different".into()),
            },
            ..Default::default()
        };
        append_config_entry(&path, &bot1).unwrap();
        append_config_entry(&path, &bot2).unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(content.matches("[[bots]]").count(), 1);
        assert!(content.contains("npub1echo"));
        assert!(!content.contains("npub1different"));
    }

    #[test]
    fn append_config_entry_appends_to_non_populated_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pacto-bot-api.toml");
        fs::write(&path, "[daemon]\ndata_dir = \"data\"\n").unwrap();
        let bot = BotConfig {
            id: "echo-bot".to_string(),
            npub: "npub1echo".to_string(),
            signing: SigningConfig::Nsec {
                nsec: SecretString::new("nsec1secret".into()),
            },
            ..Default::default()
        };
        append_config_entry(&path, &bot).unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(content.matches("[[bots]]").count(), 1);
        assert!(content.contains("[daemon]"));
        assert!(content.contains("id = \"echo-bot\""));
    }

    #[test]
    #[cfg(unix)]
    fn append_config_entry_tightens_lax_permissions_to_0o600() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pacto-bot-api.toml");
        fs::write(&path, "[daemon]\ndata_dir = \"data\"\n").unwrap();
        {
            let mut perms = fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o644);
            fs::set_permissions(&path, perms).unwrap();
        }

        let bot = BotConfig {
            id: "echo-bot".to_string(),
            npub: "npub1echo".to_string(),
            signing: SigningConfig::Nsec {
                nsec: SecretString::new("nsec1secret".into()),
            },
            ..Default::default()
        };
        append_config_entry(&path, &bot).unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "config file should be tightened to 0o600 after append"
        );
    }
}
