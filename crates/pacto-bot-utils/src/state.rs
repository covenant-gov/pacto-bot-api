use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use thiserror::Error;

/// On-disk metadata for groups created by this tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateFile {
    pub version: u32,
    pub groups: HashMap<String, GroupState>,
}

impl Default for StateFile {
    fn default() -> Self {
        Self {
            version: CURRENT_VERSION,
            groups: HashMap::new(),
        }
    }
}

/// Per-group metadata stored in the JSON state file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupState {
    /// Hex-encoded MLS group ID (`mls_group_id`).
    pub group_id: String,
    /// Creator public key in bech32 `npub` form.
    pub creator_npub: String,
    /// Relay URL used to create or re-open the group.
    pub relay: String,
    /// Bot npubs already invited to this group.
    pub invited_bots: Vec<String>,
}

const CURRENT_VERSION: u32 = 1;

#[derive(Debug, Error)]
pub enum StateError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported state file version: {0}")]
    UnsupportedVersion(u32),
}

/// Load the state file if it exists, otherwise return an empty state.
pub fn load(path: &Path) -> Result<StateFile, StateError> {
    if !path.exists() {
        return Ok(StateFile::default());
    }
    let _lock = LockFile::new(path);
    let mut file = File::open(path)?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    let state: StateFile = serde_json::from_slice(&buf)?;
    if state.version != CURRENT_VERSION {
        return Err(StateError::UnsupportedVersion(state.version));
    }
    Ok(state)
}

/// Save the state file atomically using a temp file and rename, while holding
/// an advisory lock.
pub fn save(path: &Path, state: &StateFile) -> Result<(), StateError> {
    let _lock = LockFile::new(path);
    let json = serde_json::to_vec_pretty(state)?;

    let temp = temp_path(path);
    let mut temp_file = File::create(&temp)?;
    temp_file.write_all(&json)?;
    temp_file.flush()?;
    drop(temp_file);

    std::fs::rename(&temp, path)?;
    Ok(())
}

fn temp_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "state.tmp".to_string());
    path.with_file_name(format!(".{file_name}.tmp"))
}

struct LockFile {
    _file: File,
}

impl LockFile {
    fn new(path: &Path) -> Result<Self, StateError> {
        let lock_path = path.with_extension("lock");
        let file = File::options()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)?;
        fs2::FileExt::lock_exclusive(&file)?;
        Ok(Self { _file: file })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut state = StateFile::default();
        state.groups.insert(
            "squad".to_string(),
            GroupState {
                group_id: "deadbeef".to_string(),
                creator_npub: "npub1creator".to_string(),
                relay: "ws://relay".to_string(),
                invited_bots: vec!["npub1bot".to_string()],
            },
        );
        save(&path, &state).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.groups.len(), 1);
        let group = loaded.groups.get("squad").unwrap();
        assert_eq!(group.group_id, "deadbeef");
    }

    #[test]
    fn missing_file_returns_empty() {
        let path = Path::new("/nonexistent/path/state.json");
        let state = load(path).unwrap();
        assert!(state.groups.is_empty());
    }

    #[test]
    fn bad_version_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let raw = r#"{"version": 99, "groups": {}}"#;
        std::fs::write(&path, raw).unwrap();
        assert!(matches!(
            load(&path).unwrap_err(),
            StateError::UnsupportedVersion(99)
        ));
    }
}
