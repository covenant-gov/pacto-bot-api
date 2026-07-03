//! `pacto-bot-admin update` implementation.
//!
//! Re-renders an existing bot project from its lock file while protecting user
//! edits and updating the lock file with the newly resolved versions.

use crate::scaffold::generate::{ScaffoldMode, ScaffoldRequest};
use crate::scaffold::lock::{ScaffoldLock, lock_path, read_lock, write_lock};
use crate::scaffold::merge::{MergeContext, bot_id_snake, merge_rendered};
use crate::scaffold::render::render_template;
use crate::scaffold::resolve::{Resolver, ResolverConfig};
use crate::scaffold::safety::OverwritePolicy;
use pacto_bot_api::errors::DaemonError;
use std::io::IsTerminal;
use std::path::Path;

/// Run `pacto-bot-admin update` for a single bot.
///
/// `project_dir` is the outer project directory. `bot_id` selects the bot.
/// `force` overrides protected-file prompts; `refresh` forces re-fetching.
/// `allow_hooks` controls whether `cargo-generate` may execute hooks.
pub async fn run_update(
    project_dir: &Path,
    bot_id: &str,
    force: bool,
    refresh: bool,
    allow_hooks: bool,
) -> Result<(), DaemonError> {
    let lock_file = lock_path(project_dir, bot_id);
    let lock = read_lock(&lock_file)?;

    let template_ref = resolve_template_ref_for_update(&lock);
    let config = ResolverConfig {
        template_repo: std::env::var("PACTO_TEMPLATE_REPO")
            .unwrap_or_else(|_| "https://github.com/covenant-gov/pacto-bot-templates".into()),
        template_ref,
        language: language_from_template_path(&lock.triple.template.path),
        kind: kind_from_template_path(&lock.triple.template.path),
        refresh,
    };

    let resolver = Resolver::new(config)?;
    let bundle = resolver.resolve().await?;
    let triple = bundle.triple;
    let manifest = bundle.manifest;
    let template_dir = bundle.template_dir;

    let request = ScaffoldRequest {
        bot_id: bot_id.to_string(),
        language: language_from_template_path(&lock.triple.template.path),
        kind: kind_from_template_path(&lock.triple.template.path),
        commands: Vec::new(), // Commands are not tracked in the lock; user edits persist.
        with_tests: true,
        http: false,
        force,
        allow_hooks,
        project_dir: project_dir.to_path_buf(),
        template_repo: std::env::var("PACTO_TEMPLATE_REPO")
            .unwrap_or_else(|_| "https://github.com/covenant-gov/pacto-bot-templates".into()),
        template_ref: None,
        refresh,
        mode: ScaffoldMode::ExistingProject {
            bot_config: Default::default(),
        },
    };

    let rendered = render_template(&template_dir, &request, allow_hooks)?;

    let policy = OverwritePolicy {
        force,
        interactive: std::io::stdin().is_terminal(),
        skip_existing: false,
    };

    let mut denylist = Vec::new();
    let bot_dir = project_dir.join("bots").join(bot_id);
    let bot_file_name = format!("{}.py", bot_id_snake(bot_id));
    for protected in &manifest.protected_files {
        let resolved = if protected == "bot.py" {
            bot_file_name.clone()
        } else {
            protected.clone()
        };
        if resolved == "pacto-bot-api.toml" {
            denylist.push(project_dir.join(&resolved));
        } else {
            denylist.push(bot_dir.join(&resolved));
        }
    }
    denylist.push(project_dir.join("pacto-bot-api.toml"));

    let context = MergeContext {
        project_dir: project_dir.to_path_buf(),
        bot_id: bot_id.to_string(),
        policy,
        denylist,
        append_compose: true,
    };

    merge_rendered(&rendered.dir, &context)?;

    let new_lock = ScaffoldLock {
        lock_version: lock.lock_version,
        triple,
    };
    write_lock(&lock_file, &new_lock)?;

    Ok(())
}

fn resolve_template_ref_for_update(lock: &ScaffoldLock) -> Option<String> {
    let r#ref = &lock.triple.template.r#ref;
    // Semver tags are allowed to float to a newer compatible version.
    if r#ref.starts_with('v') && semver::Version::parse(r#ref.trim_start_matches('v')).is_ok() {
        None
    } else {
        Some(lock.triple.template.resolved_commit.clone())
    }
}

fn language_from_template_path(path: &str) -> String {
    path.split('-').next().unwrap_or("python").to_string()
}

fn kind_from_template_path(path: &str) -> String {
    path.split('-').nth(1).unwrap_or("llm").to_string()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn semver_tag_ref_floats() {
        let lock = ScaffoldLock {
            lock_version: 1,
            triple: crate::scaffold::lock::ResolvedTriple {
                template: crate::scaffold::lock::TemplateLock {
                    path: "python-llm".to_string(),
                    r#ref: "v0.1.0".to_string(),
                    resolved_commit: "abc".to_string(),
                },
                contract: crate::scaffold::lock::ArtifactLock {
                    name: "pacto-contract".to_string(),
                    version: "0.1.0".to_string(),
                },
                sdk: crate::scaffold::lock::ArtifactLock {
                    name: "pacto-bot-sdk".to_string(),
                    version: "0.2.0".to_string(),
                },
                admin: crate::scaffold::lock::AdminLock {
                    version: "0.5.0".to_string(),
                },
            },
        };
        assert_eq!(resolve_template_ref_for_update(&lock), None);
    }

    #[test]
    fn branch_ref_pins_to_commit() {
        let lock = ScaffoldLock {
            lock_version: 1,
            triple: crate::scaffold::lock::ResolvedTriple {
                template: crate::scaffold::lock::TemplateLock {
                    path: "python-llm".to_string(),
                    r#ref: "main".to_string(),
                    resolved_commit: "abc".to_string(),
                },
                contract: crate::scaffold::lock::ArtifactLock {
                    name: "pacto-contract".to_string(),
                    version: "0.1.0".to_string(),
                },
                sdk: crate::scaffold::lock::ArtifactLock {
                    name: "pacto-bot-sdk".to_string(),
                    version: "0.2.0".to_string(),
                },
                admin: crate::scaffold::lock::AdminLock {
                    version: "0.5.0".to_string(),
                },
            },
        };
        assert_eq!(
            resolve_template_ref_for_update(&lock),
            Some("abc".to_string())
        );
    }
}
