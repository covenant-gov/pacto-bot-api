use pacto_bot_api::errors::DaemonError;
use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

/// Policy controlling whether existing files may be overwritten.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverwritePolicy {
    /// Overwrite existing files without prompting.
    pub force: bool,
    /// Allow interactive prompting when not forced.
    pub interactive: bool,
    /// Skip existing files instead of aborting when not forced and not
    /// interactive. Used for additive operations such as retrofitting tests
    /// onto an existing bot project.
    pub skip_existing: bool,
}

/// Outcome of a single write decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteDecision {
    /// Write the file.
    Write,
    /// Leave the existing file in place.
    Skip,
    /// Abort the entire operation.
    #[allow(dead_code)]
    Abort,
}

/// Decide whether `path` may be written based on the policy, denylist, and
/// interactive prompt.
///
/// Returns `Write` when the file should be written, `Skip` when the operator
/// declines to overwrite, and an error when the operation must abort because
/// the file is protected or prompting is impossible.
pub fn decide_write(
    path: &Path,
    policy: &OverwritePolicy,
    denylist: &[PathBuf],
    prompt_fn: &mut dyn FnMut(&Path) -> Result<bool, DaemonError>,
) -> Result<WriteDecision, DaemonError> {
    if denylist.iter().any(|p| p.as_path() == path) {
        return Err(DaemonError::Config(format!(
            "refusing to overwrite protected file: {}",
            path.display()
        )));
    }

    if is_populated_config(path) {
        return Err(DaemonError::Config(format!(
            "refusing to overwrite protected file: {}",
            path.display()
        )));
    }

    if path.exists() && contains_secret_material(path)? {
        return Err(DaemonError::Config(format!(
            "refusing to overwrite protected file: {}",
            path.display()
        )));
    }

    if !path.exists() {
        return Ok(WriteDecision::Write);
    }

    if policy.force {
        return Ok(WriteDecision::Write);
    }

    if !policy.interactive || !is_interactive_tty() {
        if policy.skip_existing {
            return Ok(WriteDecision::Skip);
        }
        return Err(DaemonError::Config(format!(
            "refusing to overwrite existing file without interactive prompt: {}",
            path.display()
        )));
    }

    match prompt_fn(path)? {
        true => Ok(WriteDecision::Write),
        false => Ok(WriteDecision::Skip),
    }
}

/// Set restrictive permissions on a generated config file.
///
/// On Unix this is `0o600`; on other platforms it is a no-op.
pub fn set_config_permissions(path: &Path) -> Result<(), DaemonError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let permissions = fs::Permissions::from_mode(0o600);
        fs::set_permissions(path, permissions).map_err(DaemonError::Io)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

/// Returns true if `path` is a `pacto-bot-api.toml` file that exists and
/// contains a populated `[[bots]]` TOML array.
pub fn is_populated_config(path: &Path) -> bool {
    if path.file_name() != Some(std::ffi::OsStr::new("pacto-bot-api.toml")) {
        return false;
    }
    if !path.exists() {
        return false;
    }
    fs::read_to_string(path)
        .map(|content| content.contains("[[bots]]"))
        .unwrap_or(false)
}

/// Returns true if the file at `path` contains raw secret material.
///
/// This scans for the literal strings `nsec1` and `nsec =` so that files
/// holding private keys are never overwritten.
pub fn contains_secret_material(path: &Path) -> Result<bool, DaemonError> {
    let content = fs::read_to_string(path).map_err(DaemonError::Io)?;
    Ok(content.contains("nsec1") || content.contains("nsec ="))
}

fn is_interactive_tty() -> bool {
    std::io::stdin().is_terminal() || test_tty_override()
}

#[cfg(test)]
fn test_tty_override() -> bool {
    TEST_TTY.with(|flag| flag.get())
}

#[cfg(not(test))]
fn test_tty_override() -> bool {
    false
}

#[cfg(test)]
thread_local! {
    static TEST_TTY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn set_test_tty(enabled: bool) {
    TEST_TTY.with(|flag| flag.set(enabled));
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    #![allow(clippy::panic)]

    use super::*;
    use std::io::Write;

    fn with_test_tty<R>(f: impl FnOnce() -> R) -> R {
        set_test_tty(true);
        let result = f();
        set_test_tty(false);
        result
    }

    #[test]
    fn nonexistent_path_returns_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does_not_exist.txt");
        let policy = OverwritePolicy {
            force: false,
            interactive: false,
            skip_existing: false,
        };

        let decision = decide_write(&path, &policy, &[], &mut |_| Ok(true)).unwrap();

        assert_eq!(decision, WriteDecision::Write);
    }

    #[test]
    fn existing_file_force_true_returns_write() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file, "hello").unwrap();
        let policy = OverwritePolicy {
            force: true,
            interactive: false,
            skip_existing: false,
        };

        let decision = decide_write(file.path(), &policy, &[], &mut |_| Ok(true)).unwrap();

        assert_eq!(decision, WriteDecision::Write);
    }

    #[test]
    fn existing_file_interactive_yes_returns_write() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file, "hello").unwrap();
        let policy = OverwritePolicy {
            force: false,
            interactive: true,
            skip_existing: false,
        };
        let decision =
            with_test_tty(|| decide_write(file.path(), &policy, &[], &mut |_| Ok(true)).unwrap());

        assert_eq!(decision, WriteDecision::Write);
    }

    #[test]
    fn existing_file_interactive_no_returns_skip() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file, "hello").unwrap();
        let policy = OverwritePolicy {
            force: false,
            interactive: true,
            skip_existing: false,
        };

        let decision =
            with_test_tty(|| decide_write(file.path(), &policy, &[], &mut |_| Ok(false)).unwrap());

        assert_eq!(decision, WriteDecision::Skip);
    }

    #[test]
    fn denylisted_path_returns_abort() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file, "content").unwrap();
        let denylist = vec![file.path().to_path_buf()];
        let policy = OverwritePolicy {
            force: true,
            interactive: false,
            skip_existing: false,
        };

        let err = decide_write(file.path(), &policy, &denylist, &mut |_| Ok(true)).unwrap_err();

        let message = err.to_string();
        assert!(message.contains("refusing to overwrite protected file"));
        assert!(message.contains(&*file.path().to_string_lossy()));
    }

    #[test]
    fn populated_config_returns_abort() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pacto-bot-api.toml");
        let mut file = fs::File::create(&path).unwrap();
        writeln!(file, "[[bots]]").unwrap();
        let policy = OverwritePolicy {
            force: true,
            interactive: false,
            skip_existing: false,
        };

        let err = decide_write(&path, &policy, &[], &mut |_| Ok(true)).unwrap_err();

        assert!(
            err.to_string()
                .contains("refusing to overwrite protected file")
        );
    }

    #[test]
    fn secret_material_returns_abort() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file, "private_key = \"nsec1abcdef\"").unwrap();
        let policy = OverwritePolicy {
            force: true,
            interactive: false,
            skip_existing: false,
        };

        let err = decide_write(file.path(), &policy, &[], &mut |_| Ok(true)).unwrap_err();

        assert!(
            err.to_string()
                .contains("refusing to overwrite protected file")
        );
    }

    #[test]
    #[cfg(unix)]
    fn set_config_permissions_unix_0600() {
        use std::os::unix::fs::PermissionsExt;

        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file, "cfg").unwrap();

        set_config_permissions(file.path()).unwrap();

        let mode = fs::metadata(file.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
