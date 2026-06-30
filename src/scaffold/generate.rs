//! Project generation logic for the scaffold command.

use crate::scaffold::safety::{OverwritePolicy, WriteDecision, decide_write, set_config_permissions};
use crate::scaffold::template::{Template, Value as TemplateValue};
use include_dir::{Dir, include_dir};
use pacto_bot_api::config::BotConfig;
use pacto_bot_api::errors::DaemonError;
use secrecy::ExposeSecret;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

/// Templates embedded at compile time so the binary works after `cargo install`
/// or when distributed without the source `templates/` directory.
static TEMPLATES_DIR: Dir<'static> = include_dir!("templates");

/// Lazily extracted copy of the embedded templates on disk.
///
/// Embedded templates are stored in the binary, but the existing template
/// rendering code expects a filesystem directory. A one-time extraction to a
/// temporary directory bridges the two without restructuring the renderer.
static EMBEDDED_TEMPLATES: LazyLock<Result<tempfile::TempDir, String>> =
    LazyLock::new(|| {
        let temp = tempfile::tempdir().map_err(|e| e.to_string())?;
        extract_embedded_dir(&TEMPLATES_DIR, temp.path())
            .map_err(|e| e.to_string())?;
        Ok(temp)
    });

fn extract_embedded_dir(dir: &Dir, dest: &Path) -> Result<(), String> {
    fs::create_dir_all(dest).map_err(|e| e.to_string())?;
    for file in dir.files() {
        let target = dest.join(file.path());
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        fs::write(target, file.contents()).map_err(|e| e.to_string())?;
    }
    for subdir in dir.dirs() {
        extract_embedded_dir(subdir, &dest.join(subdir.path()))?;
    }
    Ok(())
}

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
    pub commands: Vec<String>,
    pub with_tests: bool,
    pub force: bool,
    pub project_dir: PathBuf,
    pub mode: ScaffoldMode,
}

/// Template manifest describing metadata and protected files.
#[derive(Debug, Clone, Deserialize)]
struct Manifest {
    #[serde(default)]
    protected_files: Vec<String>,
}

/// Generate the project files described by `request`.
///
/// This is the entry point used by both `pacto-bot-admin new --scaffold` and
/// `pacto-bot-admin scaffold`.
pub fn run_scaffold(request: ScaffoldRequest) -> Result<(), DaemonError> {
    validate_commands(&request.commands)?;

    let template_dir = template_dir_for_language(&request.language)?;
    let manifest = load_manifest(&template_dir)?;

    let policy = OverwritePolicy {
        force: request.force,
        interactive: std::io::stdin().is_terminal(),
        skip_existing: matches!(request.mode, ScaffoldMode::ExistingProject { .. }),
    };
    let denylist = build_denylist(&request, &manifest);
    let context = build_context(&request);

    fs::create_dir_all(&request.project_dir).map_err(DaemonError::Io)?;

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
            append_config_entry(
                &request.project_dir.join("pacto-bot-api.toml"),
                bot_config,
            )?;
        }
    }

    render_templates(
        &template_dir,
        &request.project_dir,
        &request.bot_id,
        &context,
        &policy,
        &denylist,
    )?;

    append_compose_services(
        &template_dir,
        &request.project_dir,
        &request.bot_id,
        &context,
        &policy,
        &denylist,
    )?;

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

fn template_dir_for_language(language: &str) -> Result<PathBuf, DaemonError> {
    let relative = PathBuf::from("templates").join(language);

    // Try to locate templates relative to the running executable so that
    // tests and installed binaries can find them without relying on CWD.
    if let Ok(exe) = std::env::current_exe() {
        let mut dir = exe.parent();
        while let Some(d) = dir {
            let candidate = d.join(&relative);
            if candidate.is_dir() {
                return Ok(candidate);
            }
            dir = d.parent();
        }
    }

    // Fallback to the current working directory.
    if relative.is_dir() {
        return Ok(relative);
    }

    // Final fallback: templates embedded at compile time.
    let embedded = match EMBEDDED_TEMPLATES.as_ref() {
        Ok(temp) => temp,
        Err(e) => {
            return Err(DaemonError::Config(format!(
                "embedded templates unavailable: {e}"
            )));
        }
    };
    let candidate = embedded.path().join(language);
    if candidate.is_dir() {
        return Ok(candidate);
    }

    Err(DaemonError::Config(format!(
        "template directory not found: {}",
        relative.display()
    )))
}

fn load_manifest(template_dir: &Path) -> Result<Manifest, DaemonError> {
    let path = template_dir.join("manifest.toml");
    if !path.exists() {
        return Ok(Manifest {
            protected_files: Vec::new(),
        });
    }
    let raw = fs::read_to_string(&path).map_err(DaemonError::Io)?;
    toml::from_str(&raw).map_err(|e| DaemonError::Config(format!("invalid manifest.toml: {e}")))
}

fn bot_target_dir(project_dir: &Path, bot_id: &str) -> PathBuf {
    project_dir.join("bots").join(bot_id)
}

fn build_context(request: &ScaffoldRequest) -> HashMap<String, TemplateValue> {
    let mut ctx = HashMap::new();
    ctx.insert("bot_id".to_string(), TemplateValue::from(request.bot_id.clone()));
    ctx.insert(
        "bot_id_snake".to_string(),
        TemplateValue::from(bot_id_snake(&request.bot_id)),
    );
    ctx.insert(
        "commands".to_string(),
        TemplateValue::from(request.commands.clone()),
    );
    ctx.insert(
        "with_tests".to_string(),
        TemplateValue::from(request.with_tests),
    );
    ctx.insert("version".to_string(), TemplateValue::from("0.1.0"));
    ctx
}

fn bot_id_snake(bot_id: &str) -> String {
    bot_id.replace(['-', '.'], "_")
}

fn build_denylist(request: &ScaffoldRequest, manifest: &Manifest) -> Vec<PathBuf> {
    let mut denylist = Vec::new();

    let bot_dir = bot_target_dir(&request.project_dir, &request.bot_id);

    // When retrofitting an existing bot, never overwrite its populated config.
    if matches!(request.mode, ScaffoldMode::ExistingProject { .. }) {
        denylist.push(request.project_dir.join("pacto-bot-api.toml"));
    }

    // Manifest-declared protected files are relative to the bot directory.
    for protected in &manifest.protected_files {
        if protected == "pacto-bot-api.toml" {
            denylist.push(request.project_dir.join(protected));
        } else {
            denylist.push(bot_dir.join(protected));
        }
    }

    denylist
}

fn write_config_snippet(
    path: &Path,
    snippet: &str,
    policy: &OverwritePolicy,
    denylist: &[PathBuf],
) -> Result<(), DaemonError> {
    match decide_write(path, policy, denylist, &mut prompt_overwrite)? {
        WriteDecision::Write => {
            fs::write(path, snippet).map_err(DaemonError::Io)?;
            set_config_permissions(path)?;
            println!("Created {}", path.display());
            Ok(())
        }
        WriteDecision::Skip => {
            println!("Skipped {}", path.display());
            Ok(())
        }
        WriteDecision::Abort => unreachable!(),
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

    // Appending a new [[bots]] entry is additive, not destructive, so it does
    // not require an overwrite prompt.
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(path)
        .map_err(DaemonError::Io)?;
    file.write_all(b"\n").map_err(DaemonError::Io)?;
    file.write_all(snippet.as_bytes()).map_err(DaemonError::Io)?;
    println!("Appended [[bots]] entry to {}", path.display());
    Ok(())
}

fn bot_config_to_snippet(bot_config: &BotConfig) -> Result<String, DaemonError> {
    let mut lines = Vec::new();
    lines.push("[[bots]]".to_string());
    lines.push(format!("id = {:?}", bot_config.id));
    lines.push(format!("npub = {:?}", bot_config.npub));

    match &bot_config.signing {
        pacto_bot_api::config::SigningConfig::Nsec { nsec } => {
            let nsec = nsec.expose_secret();
            lines.push(format!("signing = {{ backend = \"nsec\", nsec = {nsec:?} }}"));
        }
        pacto_bot_api::config::SigningConfig::BunkerLocal { uri } => {
            let uri = uri.expose_secret();
            lines.push(format!("signing = {{ backend = \"bunker_local\", uri = {uri:?} }}"));
        }
        pacto_bot_api::config::SigningConfig::BunkerRemote { uri } => {
            let uri = uri.expose_secret();
            lines.push(format!("signing = {{ backend = \"bunker_remote\", uri = {uri:?} }}"));
        }
    }

    lines.push(format!("relays = {}", format_toml_array(&bot_config.relays)));
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

fn render_templates(
    template_dir: &Path,
    project_dir: &Path,
    bot_id: &str,
    context: &HashMap<String, TemplateValue>,
    policy: &OverwritePolicy,
    denylist: &[PathBuf],
) -> Result<(), DaemonError> {
    let target_dir = bot_target_dir(project_dir, bot_id);
    fs::create_dir_all(&target_dir).map_err(DaemonError::Io)?;

    render_template_tree(
        template_dir,
        &target_dir,
        template_dir,
        context,
        policy,
        denylist,
    )?;

    Ok(())
}

fn render_template_tree(
    template_dir: &Path,
    target_dir: &Path,
    current: &Path,
    context: &HashMap<String, TemplateValue>,
    policy: &OverwritePolicy,
    denylist: &[PathBuf],
) -> Result<(), DaemonError> {
    for entry in fs::read_dir(current).map_err(DaemonError::Io)? {
        let entry = entry.map_err(DaemonError::Io)?;
        let source = entry.path();
        let file_name = entry.file_name();

        if current == template_dir && file_name == "manifest.toml" {
            continue;
        }

        // docker-compose.yml is handled separately so services can be merged
        // when scaffolding additional bots into an existing project.
        if current == template_dir && file_name == "docker-compose.yml" {
            continue;
        }

        let relative = source.strip_prefix(template_dir).map_err(|e| {
            DaemonError::Config(format!("failed to compute relative template path: {e}"))
        })?;

        let relative_str = relative.to_string_lossy();
        if relative_str.starts_with("tests/") && !context["with_tests"].is_truthy() {
            continue;
        }

        // Skip the tests directory entirely when not generating tests so that
        // an empty `tests/` folder is not left behind.
        if source.is_dir()
            && current == template_dir
            && file_name == "tests"
            && !context["with_tests"].is_truthy()
        {
            continue;
        }

        let target = if file_name == "bot.py" {
            target_dir.join(format!(
                "{}.py",
                context["bot_id_snake"].as_str().unwrap_or("bot")
            ))
        } else {
            target_dir.join(relative)
        };

        if source.is_dir() {
            fs::create_dir_all(&target).map_err(DaemonError::Io)?;
            render_template_tree(template_dir, target_dir, &source, context, policy, denylist)?;
        } else {
            render_template_file(&source, &target, context, policy, denylist)?;
        }
    }

    Ok(())
}

fn render_template_file(
    source: &Path,
    target: &Path,
    context: &HashMap<String, TemplateValue>,
    policy: &OverwritePolicy,
    denylist: &[PathBuf],
) -> Result<(), DaemonError> {
    let raw = fs::read_to_string(source).map_err(DaemonError::Io)?;
    let template = Template::new(&raw);
    let rendered = template.render(context)?;

    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(DaemonError::Io)?;
    }

    match decide_write(target, policy, denylist, &mut prompt_overwrite)? {
        WriteDecision::Write => {
            fs::write(target, rendered).map_err(DaemonError::Io)?;
            println!("Created {}", target.display());
        }
        WriteDecision::Skip => {
            println!("Skipped {}", target.display());
        }
        WriteDecision::Abort => unreachable!(),
    }

    Ok(())
}

fn append_compose_services(
    template_dir: &Path,
    project_dir: &Path,
    bot_id: &str,
    context: &HashMap<String, TemplateValue>,
    policy: &OverwritePolicy,
    denylist: &[PathBuf],
) -> Result<(), DaemonError> {
    let compose_path = project_dir.join("docker-compose.yml");

    if !compose_path.exists() {
        // First bot in the project: render the template compose file as-is.
        let source = template_dir.join("docker-compose.yml");
        render_template_file(&source, &compose_path, context, policy, denylist)?;
        return Ok(());
    }

    // Additional bot: merge services into the existing compose file.
    let raw = fs::read_to_string(&compose_path).map_err(DaemonError::Io)?;
    let mut compose: serde_yaml::Value = serde_yaml::from_str(&raw)
        .map_err(|e| DaemonError::Config(format!("invalid docker-compose.yml: {e}")))?;

    let services = compose
        .get_mut("services")
        .and_then(|v| v.as_mapping_mut())
        .ok_or_else(|| DaemonError::Config("docker-compose.yml missing services mapping".into()))?;

    let bot_service = serde_yaml::Mapping::from_iter([
        (
            serde_yaml::Value::String("build".to_string()),
            serde_yaml::Value::String(format!("bots/{bot_id}")),
        ),
        (
            serde_yaml::Value::String("profiles".to_string()),
            serde_yaml::Value::Sequence(vec![serde_yaml::Value::String("bot-only".to_string())]),
        ),
        (
            serde_yaml::Value::String("environment".to_string()),
            serde_yaml::Value::Mapping(serde_yaml::Mapping::from_iter([
                (
                    serde_yaml::Value::String("PACTO_TRANSPORT".to_string()),
                    serde_yaml::Value::String("unix".to_string()),
                ),
                (
                    serde_yaml::Value::String("PACTO_SOCKET_PATH".to_string()),
                    serde_yaml::Value::String("/run/pacto-bot-api.sock".to_string()),
                ),
            ])),
        ),
        (
            serde_yaml::Value::String("volumes".to_string()),
            serde_yaml::Value::Sequence(vec![serde_yaml::Value::Mapping(
                serde_yaml::Mapping::from_iter([
                    (
                        serde_yaml::Value::String("type".to_string()),
                        serde_yaml::Value::String("bind".to_string()),
                    ),
                    (
                        serde_yaml::Value::String("source".to_string()),
                        serde_yaml::Value::String("/run/pacto-bot-api.sock".to_string()),
                    ),
                    (
                        serde_yaml::Value::String("target".to_string()),
                        serde_yaml::Value::String("/run/pacto-bot-api.sock".to_string()),
                    ),
                    (
                        serde_yaml::Value::String("read_only".to_string()),
                        serde_yaml::Value::Bool(true),
                    ),
                ]),
            )]),
        ),
    ]);

    let full_service = serde_yaml::Mapping::from_iter([
        (
            serde_yaml::Value::String("build".to_string()),
            serde_yaml::Value::String(format!("bots/{bot_id}")),
        ),
        (
            serde_yaml::Value::String("profiles".to_string()),
            serde_yaml::Value::Sequence(vec![serde_yaml::Value::String("full".to_string())]),
        ),
        (
            serde_yaml::Value::String("environment".to_string()),
            serde_yaml::Value::Mapping(serde_yaml::Mapping::from_iter([
                (
                    serde_yaml::Value::String("PACTO_TRANSPORT".to_string()),
                    serde_yaml::Value::String("unix".to_string()),
                ),
                (
                    serde_yaml::Value::String("PACTO_SOCKET_PATH".to_string()),
                    serde_yaml::Value::String("/run/pacto-bot-api.sock".to_string()),
                ),
            ])),
        ),
        (
            serde_yaml::Value::String("volumes".to_string()),
            serde_yaml::Value::Sequence(vec![serde_yaml::Value::String(
                "pacto-socket:/run/pacto-bot-api:ro".to_string(),
            )]),
        ),
        (
            serde_yaml::Value::String("depends_on".to_string()),
            serde_yaml::Value::Mapping(serde_yaml::Mapping::from_iter([(
                serde_yaml::Value::String("daemon".to_string()),
                serde_yaml::Value::Mapping(serde_yaml::Mapping::from_iter([(
                    serde_yaml::Value::String("condition".to_string()),
                    serde_yaml::Value::String("service_started".to_string()),
                )])),
            )])),
        ),
    ]);

    services.insert(
        serde_yaml::Value::String(bot_id.to_string()),
        serde_yaml::Value::Mapping(bot_service),
    );
    services.insert(
        serde_yaml::Value::String(format!("{bot_id}-full")),
        serde_yaml::Value::Mapping(full_service),
    );

    let updated = serde_yaml::to_string(&compose)
        .map_err(|e| DaemonError::Config(format!("failed to serialize docker-compose.yml: {e}")))?;

    match decide_write(&compose_path, policy, denylist, &mut prompt_overwrite)? {
        WriteDecision::Write => {
            fs::write(&compose_path, updated).map_err(DaemonError::Io)?;
            println!("Updated {}", compose_path.display());
        }
        WriteDecision::Skip => {
            println!("Skipped {}", compose_path.display());
        }
        WriteDecision::Abort => unreachable!(),
    }

    Ok(())
}

fn prompt_overwrite(path: &Path) -> Result<bool, DaemonError> {
    println!(
        "File {} already exists. Overwrite? [y/N]:",
        path.display()
    );
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
    fn bot_id_snake_replaces_hyphens() {
        assert_eq!(bot_id_snake("echo-bot"), "echo_bot");
        assert_eq!(bot_id_snake("my-cool-bot"), "my_cool_bot");
    }

    #[test]
    fn validate_commands_rejects_invalid_names() {
        assert!(validate_commands(&["echo".to_string(), "help_me".to_string()]).is_ok());
        assert!(validate_commands(&["Echo".to_string()]).is_err());
        assert!(validate_commands(&["echo!".to_string()]).is_err());
        assert!(validate_commands(&["help-me".to_string()]).is_err());
    }

    #[test]
    fn build_context_includes_bot_values() {
        let request = ScaffoldRequest {
            bot_id: "echo-bot".to_string(),
            language: "python".to_string(),
            commands: vec!["echo".to_string()],
            with_tests: true,
            force: false,
            project_dir: PathBuf::from("/tmp/echo-bot"),
            mode: ScaffoldMode::NewProject {
                snippet: "[[bots]]\n".to_string(),
            },
        };
        let ctx = build_context(&request);
        assert_eq!(ctx["bot_id"].as_str(), Some("echo-bot"));
        assert_eq!(ctx["bot_id_snake"].as_str(), Some("echo_bot"));
        assert!(ctx["with_tests"].is_truthy());
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
}
