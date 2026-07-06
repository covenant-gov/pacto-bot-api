---
title: Secure file creation for sensitive runtime artifacts
date: 2026-07-06
category: best-practices
module: diagnostics
problem_type: best_practice
component: development_workflow
severity: high
applies_when:
  - Creating files that contain secrets, diagnostics, or other sensitive data
  - Writing a file and later renaming it into place
tags: [security, file-permissions, umask, diagnostics]
---

# Secure file creation for sensitive runtime artifacts

## Context

The daemon writes diagnostic reports and other sensitive runtime files to disk. On Unix, the default file mode is influenced by the process umask. A permissive umask (e.g., `0o022` or `0o000`) can leave files group- or world-readable during the window between creation and an explicit `set_permissions` call.

## Guidance

Create sensitive files with owner-only permissions **before** writing data. Do not rely on umask or apply permissions after the fact.

- When using Tokio, open the file with `tokio::fs::OpenOptions::new().write(true).create(true).mode(0o600)`.
- When writing via `tokio::fs::write` and renaming into place, create an empty file first, set permissions, then write and rename.
- After renaming a file into place, explicitly set the final path to `0o600`.

## Why This Matters

A permissive umask is common in development containers and CI. Relying on it creates a race condition where sensitive data can be read by other users. Explicit mode at creation removes the window.

## When to Apply

- Any file that may contain secrets, PII, or operational diagnostics.
- Any file written via temp-then-rename.

## Examples

```rust
// Before: permissive umask may expose the file
tokio::fs::write(&tmp_path, json).await?;

// After: explicit owner-only mode before writing
use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;

let mut file = tokio::fs::OpenOptions::new()
    .write(true)
    .create(true)
    .mode(0o600)
    .open(&tmp_path)
    .await?;
file.write_all(json.as_bytes()).await?;
file.flush().await?;
drop(file);
tokio::fs::rename(&tmp_path, &final_path).await?;
tokio::fs::set_permissions(&final_path, Permissions::from_mode(0o600)).await?;
```

## Related

- `src/diagnostics.rs` (`flush_report`)
- `docs/security-overview.md`
