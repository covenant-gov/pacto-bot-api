//! Per-bot scaffold lock file.
//!
//! Records the resolved contract/SDK/template triple so each generated project
//! is reproducible and refreshable. The lock file is written to
//! `.pacto/bots/<bot-id>/scaffold.lock` after `new --scaffold` succeeds and is
//! updated by `pacto-bot-admin update`.

use pacto_bot_api::errors::DaemonError;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// Current lock file schema version.
pub const LOCK_VERSION: u32 = 1;

/// Resolved triple recorded in a per-bot lock file.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ScaffoldLock {
    pub lock_version: u32,

    #[serde(flatten)]
    pub triple: ResolvedTriple,
}

/// The resolved contract/SDK/template triple.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ResolvedTriple {
    #[serde(rename = "template")]
    pub template: TemplateLock,

    #[serde(rename = "contract")]
    pub contract: ArtifactLock,

    #[serde(rename = "sdk")]
    pub sdk: ArtifactLock,

    #[serde(rename = "admin")]
    pub admin: AdminLock,
}

/// Template selection recorded in the lock.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct TemplateLock {
    /// Template path inside the repository, e.g. `python-llm`.
    pub path: String,

    /// Requested git ref (tag, branch, or commit-ish).
    pub r#ref: String,

    /// Resolved commit hash. Tags may move; this makes the lock reproducible.
    pub resolved_commit: String,
}

/// Named artifact version recorded in the lock.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ArtifactLock {
    pub name: String,
    pub version: String,
}

/// Admin CLI version used to generate the lock.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct AdminLock {
    pub version: String,
}

/// Return the default lock file path for a bot inside a project.
pub fn lock_path(project_dir: &Path, bot_id: &str) -> PathBuf {
    project_dir
        .join(".pacto")
        .join("bots")
        .join(bot_id)
        .join("scaffold.lock")
}

/// Read and parse a lock file, returning a typed error if it is missing or
/// unparseable.
pub fn read_lock(path: &Path) -> Result<ScaffoldLock, DaemonError> {
    let raw = fs::read_to_string(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            DaemonError::Config(format!(
                "scaffold lock file not found: {}. Pre-lock projects are not supported by update; see the migration skill at .agents/skills/pacto-bot-migration/SKILL.md, .claude/skills/pacto-bot-migration/SKILL.md, or .omp/skills/pacto-bot-migration/SKILL.md for guidance.",
                path.display()
            ))
        } else {
            DaemonError::Io(e)
        }
    })?;

    let lock: ScaffoldLock = toml::from_str(&raw).map_err(|e| {
        DaemonError::Config(format!(
            "invalid scaffold lock file {}: {e}",
            path.display()
        ))
    })?;

    if lock.lock_version != LOCK_VERSION {
        return Err(DaemonError::Config(format!(
            "unsupported scaffold lock version {} in {} (expected {LOCK_VERSION})",
            lock.lock_version,
            path.display()
        )));
    }

    Ok(lock)
}

/// Write a lock file atomically, creating parent directories as needed.
pub fn write_lock(path: &Path, lock: &ScaffoldLock) -> Result<(), DaemonError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(DaemonError::Io)?;
    }

    let raw = toml::to_string_pretty(lock)
        .map_err(|e| DaemonError::Config(format!("failed to serialize scaffold lock: {e}")))?;

    // Write to a sibling temporary file and rename it into place so readers
    // never observe a partially-written lock file.
    let tmp_path = path.with_extension("tmp");
    fs::write(&tmp_path, raw).map_err(DaemonError::Io)?;
    fs::rename(&tmp_path, path).map_err(DaemonError::Io)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn sample_lock() -> ScaffoldLock {
        ScaffoldLock {
            lock_version: LOCK_VERSION,
            triple: ResolvedTriple {
                template: TemplateLock {
                    path: "python-llm".to_string(),
                    r#ref: "v0.1.0".to_string(),
                    resolved_commit: "abc123".to_string(),
                },
                contract: ArtifactLock {
                    name: "pacto-contract".to_string(),
                    version: "0.1.0".to_string(),
                },
                sdk: ArtifactLock {
                    name: "pacto-bot-sdk".to_string(),
                    version: "0.2.0".to_string(),
                },
                admin: AdminLock {
                    version: "0.4.1".to_string(),
                },
            },
        }
    }

    #[test]
    fn round_trip_lock_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scaffold.lock");
        let lock = sample_lock();

        write_lock(&path, &lock).unwrap();
        let read = read_lock(&path).unwrap();

        assert_eq!(read, lock);
    }

    #[test]
    fn missing_lock_file_references_migration_skill() {
        let path = PathBuf::from("/nonexistent/scaffold.lock");
        let err = read_lock(&path).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("scaffold lock file not found"));
        assert!(msg.contains(".claude/skills/pacto-bot-migration/SKILL.md"));
    }

    #[test]
    fn unsupported_lock_version_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scaffold.lock");
        let mut lock = sample_lock();
        lock.lock_version = 99;
        write_lock(&path, &lock).unwrap();

        let err = read_lock(&path).unwrap_err();
        assert!(
            err.to_string()
                .contains("unsupported scaffold lock version")
        );
    }
}
