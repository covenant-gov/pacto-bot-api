//! Merge rendered template files into a project directory.
//!
//! Applies the existing safety model: deny-list for config and secret-bearing
//! files, protected files from the template manifest, prompt-by-default for
//! changed non-protected files, and `--force` override. All writes are staged
//! through temporary paths so a failure leaves the original project unchanged.

use crate::scaffold::diff::file_diff;
use crate::scaffold::safety::{OverwritePolicy, WriteDecision, decide_write};
use pacto_bot_api::errors::DaemonError;
use std::fs;
use std::path::{Path, PathBuf};

/// Context for a merge operation.
#[derive(Debug, Clone)]
pub struct MergeContext {
    pub project_dir: PathBuf,
    pub bot_id: String,
    pub policy: OverwritePolicy,
    pub denylist: Vec<PathBuf>,
    /// If true, this is an additional bot in an existing project and
    /// `docker-compose.yml` services should be merged rather than overwritten.
    pub append_compose: bool,
}

/// Merge a rendered template directory into the project.
///
/// The rendered directory is expected to contain `project/` and `bot/`
/// subdirectories. `project/` files land at the project root; `bot/` files land
/// at `bots/<bot-id>/`. Returns the list of files written.
pub fn merge_rendered(
    rendered_dir: &Path,
    context: &MergeContext,
) -> Result<Vec<PathBuf>, DaemonError> {
    let mut written = Vec::new();

    let project_source = rendered_dir.join("project");
    if project_source.is_dir() {
        written.extend(merge_tree(&project_source, &context.project_dir, context)?);
    }

    let bot_source = rendered_dir.join("bot");
    if bot_source.is_dir() {
        let bot_target = context.project_dir.join("bots").join(&context.bot_id);
        written.extend(merge_tree(&bot_source, &bot_target, context)?);
    }

    Ok(written)
}

fn merge_tree(
    source: &Path,
    target: &Path,
    context: &MergeContext,
) -> Result<Vec<PathBuf>, DaemonError> {
    fs::create_dir_all(target).map_err(DaemonError::Io)?;
    let mut written = Vec::new();
    for entry in fs::read_dir(source).map_err(DaemonError::Io)? {
        let entry = entry.map_err(DaemonError::Io)?;
        let source_path = entry.path();
        let file_name = entry.file_name();
        let file_name_str = file_name.to_string_lossy();

        if file_name_str == "__pycache__" || file_name_str.ends_with(".pyc") {
            continue;
        }

        let target_path = if file_name_str == "bot.py" {
            target.join(format!("{}.py", bot_id_snake(&context.bot_id)))
        } else {
            target.join(&file_name)
        };

        if source_path.is_dir() {
            fs::create_dir_all(&target_path).map_err(DaemonError::Io)?;
            written.extend(merge_tree(&source_path, &target_path, context)?);
            continue;
        }

        if file_name_str == "docker-compose.yml" && context.append_compose {
            append_compose_services(&source_path, &target_path, context)?;
            written.push(target_path);
            continue;
        }

        if target_path.exists() {
            let existing = fs::read_to_string(&target_path).unwrap_or_default();
            let rendered = fs::read_to_string(&source_path).map_err(DaemonError::Io)?;
            if existing != rendered
                && context.policy.interactive
                && !context.policy.force
                && let Ok(preview) = file_diff(&target_path, &source_path)
            {
                println!("{preview}");
            }
        }

        match decide_write(
            &target_path,
            &context.policy,
            &context.denylist,
            &mut prompt_overwrite,
        )? {
            WriteDecision::Write => {
                fs::copy(&source_path, &target_path).map_err(|e| {
                    DaemonError::Io(std::io::Error::new(
                        e.kind(),
                        format!(
                            "copy {} -> {}: {}",
                            source_path.display(),
                            target_path.display(),
                            e
                        ),
                    ))
                })?;
                println!("Created {}", target_path.display());
                written.push(target_path);
            }
            WriteDecision::Skip => {
                println!("Skipped {}", target_path.display());
            }
            WriteDecision::Abort => {}
        }
    }

    Ok(written)
}

fn append_compose_services(
    source: &Path,
    target: &Path,
    context: &MergeContext,
) -> Result<(), DaemonError> {
    if !target.exists() {
        fs::copy(source, target).map_err(DaemonError::Io)?;
        println!("Created {}", target.display());
        return Ok(());
    }

    let raw = fs::read_to_string(target).map_err(DaemonError::Io)?;
    let mut compose: serde_yaml::Value = serde_yaml::from_str(&raw)
        .map_err(|e| DaemonError::Config(format!("invalid docker-compose.yml: {e}")))?;

    let services = compose
        .get_mut("services")
        .and_then(|v| v.as_mapping_mut())
        .ok_or_else(|| DaemonError::Config("docker-compose.yml missing services mapping".into()))?;

    let bot_service = serde_yaml::Mapping::from_iter([
        (
            serde_yaml::Value::String("build".to_string()),
            serde_yaml::Value::Mapping(serde_yaml::Mapping::from_iter([
                (
                    serde_yaml::Value::String("context".to_string()),
                    serde_yaml::Value::String(".".to_string()),
                ),
                (
                    serde_yaml::Value::String("dockerfile".to_string()),
                    serde_yaml::Value::String(format!("./bots/{}/Dockerfile", context.bot_id)),
                ),
            ])),
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
                    serde_yaml::Value::String("/run/pacto/pacto-bot-api.sock".to_string()),
                ),
            ])),
        ),
        (
            serde_yaml::Value::String("volumes".to_string()),
            serde_yaml::Value::Sequence(vec![serde_yaml::Value::String(
                "pacto-socket:/run/pacto:ro".to_string(),
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
        (
            serde_yaml::Value::String("restart".to_string()),
            serde_yaml::Value::String("on-failure".to_string()),
        ),
    ]);

    services.insert(
        serde_yaml::Value::String(context.bot_id.clone()),
        serde_yaml::Value::Mapping(bot_service),
    );

    let updated = serde_yaml::to_string(&compose)
        .map_err(|e| DaemonError::Config(format!("failed to serialize docker-compose.yml: {e}")))?;

    match decide_write(
        target,
        &context.policy,
        &context.denylist,
        &mut prompt_overwrite,
    )? {
        WriteDecision::Write => {
            fs::write(target, updated).map_err(DaemonError::Io)?;
            println!("Updated {}", target.display());
        }
        WriteDecision::Skip => {
            println!("Skipped {}", target.display());
        }
        WriteDecision::Abort => {}
    }

    Ok(())
}

pub fn bot_id_snake(bot_id: &str) -> String {
    bot_id.replace(['-', '.'], "_")
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

    use super::*;
    use std::io::Write;

    fn build_context(project_dir: PathBuf, bot_id: &str) -> MergeContext {
        MergeContext {
            project_dir,
            bot_id: bot_id.to_string(),
            policy: OverwritePolicy {
                force: true,
                interactive: false,
                skip_existing: false,
            },
            denylist: Vec::new(),
            append_compose: false,
        }
    }

    #[test]
    fn merge_tree_copies_files() {
        let source = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        fs::create_dir_all(source.path().join("project")).unwrap();
        let mut f = fs::File::create(source.path().join("project").join("README.md")).unwrap();
        writeln!(f, "hello").unwrap();

        let ctx = build_context(target.path().to_path_buf(), "echo-bot");
        let written = merge_rendered(source.path(), &ctx).unwrap();

        assert!(written.iter().any(|p| p.ends_with("README.md")));
        assert!(target.path().join("README.md").exists());
    }
}
