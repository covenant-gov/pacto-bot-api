//! Render a template through `cargo-generate`.
//!
//! The bespoke template engine is kept as a private pre-processor: it
//! evaluates the multi-line values and conditionals (`{% if %}`, `{% unless %}`)
//! that `cargo-generate`'s key-value values file cannot represent. After
//! pre-rendering, the template is passed to `cargo-generate` for the final
//! directory creation and copy step.

use crate::scaffold::generate::ScaffoldRequest;
use crate::scaffold::template::{Template, Value};
use pacto_bot_api::errors::DaemonError;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Minimum `cargo-generate` version required by the CLI.
const MIN_CARGO_GENERATE_VERSION: &str = "0.23.0";

/// Rendered project output from `cargo-generate`.
#[derive(Debug, Clone)]
pub struct RenderedTemplate {
    /// Temporary directory containing the rendered project tree. The caller is
    /// responsible for cleaning this up once the merge is complete.
    pub dir: PathBuf,
}

/// Render a template into a temporary directory.
///
/// `template_dir` is the cached template path (e.g. the `python-llm/` directory
/// inside the cloned template repository). `allow_hooks` controls whether
/// `cargo-generate` may execute pre/post-generation hooks.
pub fn render_template(
    template_dir: &Path,
    request: &ScaffoldRequest,
    allow_hooks: bool,
) -> Result<RenderedTemplate, DaemonError> {
    check_cargo_generate()?;

    let context = build_context(request);
    let pre_rendered = tempfile::tempdir().map_err(DaemonError::Io)?;
    let pre_rendered_dir = pre_rendered.path().join("template");
    copy_dir_all(template_dir, &pre_rendered_dir)?;
    pre_render_files(&pre_rendered_dir, &context)?;

    if !request.with_tests {
        let _ = fs::remove_dir_all(pre_rendered_dir.join("bot").join("tests"));
    }

    // Remove the cargo-generate and manifest metadata so the pre-rendered copy
    // can be consumed by `cargo-generate` without prompting for placeholders.
    let _ = fs::remove_file(pre_rendered_dir.join("cargo-generate.toml"));
    let _ = fs::remove_file(pre_rendered_dir.join("manifest.toml"));

    let work_dir = tempfile::tempdir().map_err(DaemonError::Io)?;
    let temp_project_name = format!("pacto-render-{}-{}-temp", request.bot_id, request.language);
    let rendered_dir = work_dir.path().join(&temp_project_name);

    let mut cmd = Command::new("cargo-generate");
    cmd.arg("generate")
        .arg("--path")
        .arg(&pre_rendered_dir)
        .arg("--name")
        .arg(&temp_project_name)
        .arg("--silent")
        .current_dir(work_dir.path());

    if allow_hooks {
        cmd.arg("--allow-commands");
    }

    let output = cmd.output().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            DaemonError::Config(format!(
                "cargo-generate is not installed. Install it with: cargo install cargo-generate --version {}",
                MIN_CARGO_GENERATE_VERSION
            ))
        } else {
            DaemonError::Io(e)
        }
    })?;

    if !output.status.success() {
        return Err(DaemonError::Config(format!(
            "cargo-generate failed for template {}: {}",
            template_dir.display(),
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    if !rendered_dir.is_dir() {
        return Err(DaemonError::Config(format!(
            "cargo-generate did not produce expected output directory {}",
            rendered_dir.display()
        )));
    }

    // Detach the temp directory from the TempDir wrapper so it survives the
    // function return; the caller cleans it up after the merge.
    let _work_dir = work_dir.keep();

    Ok(RenderedTemplate { dir: rendered_dir })
}

fn check_cargo_generate() -> Result<(), DaemonError> {
    let output = Command::new("cargo-generate")
        .arg("--version")
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                DaemonError::Config(format!(
                    "cargo-generate is not installed. Install it with: cargo install cargo-generate --version {}",
                    MIN_CARGO_GENERATE_VERSION
                ))
            } else {
                DaemonError::Io(e)
            }
        })?;

    if !output.status.success() {
        return Err(DaemonError::Config(format!(
            "cargo-generate --version failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    let version_text = String::from_utf8_lossy(&output.stdout);
    let version = parse_cargo_generate_version(&version_text).ok_or_else(|| {
        DaemonError::Config(format!(
            "could not parse cargo-generate version from: {version_text}"
        ))
    })?;

    let min = semver::Version::parse(MIN_CARGO_GENERATE_VERSION)
        .map_err(|e| DaemonError::Config(format!("invalid minimum cargo-generate version: {e}")))?;
    if version < min {
        return Err(DaemonError::Config(format!(
            "cargo-generate version {version} is too old; minimum required is {min}. Update with: cargo install cargo-generate --version {}",
            MIN_CARGO_GENERATE_VERSION
        )));
    }

    Ok(())
}

fn parse_cargo_generate_version(text: &str) -> Option<semver::Version> {
    // Output looks like "cargo generate 0.23.12\n" or "0.23.12".
    let text = text.trim();
    let last = text.split_whitespace().last()?;
    semver::Version::parse(last).ok()
}

fn build_context(request: &ScaffoldRequest) -> HashMap<String, Value> {
    let mut ctx = HashMap::new();
    ctx.insert("bot_id".to_string(), request.bot_id.clone().into());
    ctx.insert(
        "bot_id_snake".to_string(),
        bot_id_snake(&request.bot_id).into(),
    );
    ctx.insert(
        "command_handlers".to_string(),
        build_command_handlers(request).into(),
    );
    ctx.insert(
        "test_command_handlers".to_string(),
        build_test_command_handlers(request).into(),
    );
    ctx.insert(
        "command_list".to_string(),
        build_command_list(request).into(),
    );
    ctx.insert("commands".to_string(), request.commands.join(", ").into());
    ctx.insert(
        "first_command".to_string(),
        request.commands.first().cloned().unwrap_or_default().into(),
    );
    ctx.insert(
        "project_dir_name".to_string(),
        request
            .project_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&request.bot_id)
            .to_string()
            .into(),
    );
    ctx.insert("http".to_string(), request.http.into());
    ctx.insert("no_http".to_string(), (!request.http).into());
    ctx.insert("with_tests".to_string(), request.with_tests.into());
    ctx.insert(
        "manifest_contract_pieces".to_string(),
        build_manifest_contract_pieces(request).into(),
    );
    ctx.insert("version".to_string(), "0.1.0".to_string().into());
    ctx
}

fn bot_id_snake(bot_id: &str) -> String {
    bot_id.replace(['-', '.'], "_")
}

fn build_command_handlers(request: &ScaffoldRequest) -> String {
    request
        .commands
        .iter()
        .map(|command| {
            format!(
                r#"@bot.command("/{command}")
async def {command}_handler(event, bot):
    bot.log(f"received /{command}: event_id={{event.event_id}}")
    response = {{
        "event_id": event.event_id,
        "action": "reply",
        "content": "{command} placeholder response",
    }}
    bot.log(f"handled /{command}: action={{response['action']}}")
    return response"#,
                command = command
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn build_test_command_handlers(request: &ScaffoldRequest) -> String {
    request
        .commands
        .iter()
        .map(|command| {
            format!(
                r#"def test_{command}_command():
    """Smoke test for /{command}."""
    assert True"#,
                command = command
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn build_command_list(request: &ScaffoldRequest) -> String {
    request
        .commands
        .iter()
        .map(|command| format!("- /{command} — TODO: implement /{command}."))
        .collect::<Vec<_>>()
        .join("\n")
}

fn build_manifest_contract_pieces(request: &ScaffoldRequest) -> String {
    let pieces: Vec<String> = request
        .commands
        .iter()
        .map(|command| {
            format!(
                r#"    {{
      "name": "{command}_reply",
      "type": "event_response",
      "timeout_seconds": 5,
      "inject_event": {{
        "bot_id": "{bot_id}",
        "event_id": "{command}-0001",
        "type": "dm_received",
        "chat_id": null,
        "content": "/{command}",
        "rumor_id": "rumor-{command}-0001",
        "author": "npub1sender",
        "timestamp": 1700000000000
      }},
      "expect_response": {{
        "event_id": "{command}-0001",
        "action": "reply"
      }}
    }}"#,
                command = command,
                bot_id = request.bot_id
            )
        })
        .collect();
    pieces.join(",\n")
}

fn copy_dir_all(src: &Path, dst: &Path) -> Result<(), DaemonError> {
    fs::create_dir_all(dst).map_err(|e| {
        DaemonError::Io(std::io::Error::new(
            e.kind(),
            format!("create_dir_all {}: {}", dst.display(), e),
        ))
    })?;
    for entry in fs::read_dir(src).map_err(|e| {
        DaemonError::Io(std::io::Error::new(
            e.kind(),
            format!("read_dir {}: {}", src.display(), e),
        ))
    })? {
        let entry = entry.map_err(DaemonError::Io)?;
        let src_path = entry.path();
        let file_name = entry.file_name();
        let dst_path = dst.join(&file_name);
        if src_path.is_dir() {
            copy_dir_all(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path).map_err(|e| {
                DaemonError::Io(std::io::Error::new(
                    e.kind(),
                    format!(
                        "copy {} -> {}: {}",
                        src_path.display(),
                        dst_path.display(),
                        e
                    ),
                ))
            })?;
        }
    }
    Ok(())
}

fn pre_render_files(dir: &Path, ctx: &HashMap<String, Value>) -> Result<(), DaemonError> {
    for entry in walk_dir(dir) {
        let path = entry.path();
        if path.is_file()
            && let Ok(content) = fs::read_to_string(&path)
        {
            let template = Template::new(&content);
            let rendered = template
                .render(ctx)
                .map_err(|e| DaemonError::Config(format!("render {}: {}", path.display(), e)))?;
            fs::write(&path, rendered).map_err(|e| {
                DaemonError::Io(std::io::Error::new(
                    e.kind(),
                    format!("write {}: {}", path.display(), e),
                ))
            })?;
        }
    }
    Ok(())
}

fn walk_dir(dir: &Path) -> Vec<fs::DirEntry> {
    let mut entries = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        if let Ok(read_dir) = fs::read_dir(&current) {
            for entry in read_dir.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                }
                entries.push(entry);
            }
        }
    }
    entries
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::scaffold::generate::ScaffoldMode;
    use std::path::PathBuf;

    fn test_request() -> ScaffoldRequest {
        ScaffoldRequest {
            bot_id: "echo-bot".to_string(),
            language: "python".to_string(),
            kind: "llm".to_string(),
            commands: vec!["echo".to_string()],
            with_tests: true,
            http: false,
            force: false,
            allow_hooks: false,
            project_dir: PathBuf::from("/tmp/echo-bot-project"),
            template_repo: "https://github.com/covenant-gov/pacto-bot-templates".to_string(),
            template_ref: None,
            refresh: false,
            mode: ScaffoldMode::NewProject {
                snippet: String::new(),
            },
        }
    }

    #[test]
    fn build_context_includes_required_values() {
        let ctx = build_context(&test_request());
        assert_eq!(ctx.get("bot_id").and_then(Value::as_str), Some("echo-bot"));
        assert_eq!(
            ctx.get("bot_id_snake").and_then(Value::as_str),
            Some("echo_bot")
        );
        assert_eq!(ctx.get("http"), Some(&Value::Bool(false)));
    }

    #[test]
    fn parse_cargo_generate_version_extracts_semver() {
        assert_eq!(
            parse_cargo_generate_version("cargo generate 0.23.12\n"),
            Some(semver::Version::new(0, 23, 12))
        );
        assert_eq!(
            parse_cargo_generate_version("0.23.12"),
            Some(semver::Version::new(0, 23, 12))
        );
    }
}
