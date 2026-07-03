//! Diff preview for scaffold updates.
//!
//! Provides a human-readable diff between the rendered template output and the
//! existing project file. Uses `git diff --no-index` when available, with a
//! simple fallback for environments where git is not present.

use pacto_bot_api::errors::DaemonError;
use std::fs;
use std::path::Path;

/// Return a preview string for the changes between `existing` and `rendered`.
///
/// If git is available, this is a unified diff. Otherwise it returns a short
/// description of the change with line counts.
pub fn file_diff(existing: &Path, rendered: &Path) -> Result<String, DaemonError> {
    if let Some(diff) = git_diff(existing, rendered) {
        return Ok(diff);
    }

    Ok(fallback_diff(existing, rendered))
}

fn git_diff(existing: &Path, rendered: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["diff", "--no-index", "--no-color", "--unified=3"])
        .arg(existing)
        .arg(rendered)
        .output()
        .ok()?;

    let code = output.status.code();
    if code != Some(0) && code != Some(1) {
        return None;
    }

    String::from_utf8(output.stdout).ok()
}

fn fallback_diff(existing: &Path, rendered: &Path) -> String {
    let existing_label = format!("a/{}", file_name(existing));
    let rendered_label = format!("b/{}", file_name(rendered));
    let old_lines = line_count(existing);
    let new_lines = line_count(rendered);
    format!(
        "--- {} ({} lines)\n+++ {} ({} lines)\nFile differs; run with --force to overwrite without reviewing.",
        existing_label, old_lines, rendered_label, new_lines
    )
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.display().to_string())
}

fn line_count(path: &Path) -> usize {
    fs::read_to_string(path)
        .map(|s| s.lines().count())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::io::Write;

    #[test]
    fn fallback_diff_reports_line_counts() {
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join("existing.txt");
        let rendered = dir.path().join("rendered.txt");
        let mut f = fs::File::create(&existing).unwrap();
        writeln!(f, "line1\nline2").unwrap();
        let mut f = fs::File::create(&rendered).unwrap();
        writeln!(f, "line1\nline2\nline3").unwrap();

        let preview = fallback_diff(&existing, &rendered);
        assert!(preview.contains("a/existing.txt"));
        assert!(preview.contains("b/rendered.txt"));
        assert!(preview.contains("(2 lines)"));
        assert!(preview.contains("(3 lines)"));
    }

    #[test]
    fn file_diff_contains_both_filenames() {
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join("existing.txt");
        let rendered = dir.path().join("rendered.txt");
        let mut f = fs::File::create(&existing).unwrap();
        writeln!(f, "line1\nline2").unwrap();
        let mut f = fs::File::create(&rendered).unwrap();
        writeln!(f, "line1\nline2\nline3").unwrap();

        let preview = file_diff(&existing, &rendered).unwrap();
        assert!(preview.contains("existing.txt"));
        assert!(preview.contains("rendered.txt"));
    }
}
