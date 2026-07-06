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

Create sensitive files with owner-only permissions **before** writing data. Do not rely on umask or apply permissions after the fact. Clean up any temporary file on failure so a partially created or partially secured file is never left behind.

### Unix

- When using Tokio, open the file with `tokio::fs::OpenOptions::new().write(true).create(true).mode(0o600)`.
- When writing via `tokio::fs::write` and renaming into place, create an empty file first, set permissions, then write and rename.
- After renaming a file into place, explicitly set the final path to `0o600`.

### Windows

- Do not create the file with the default inherited ACL and then tighten it. That leaves a TOCTOU window where another user can open the file before the DACL is restricted.
- Instead, build the owner-only DACL from the current process token, wrap it in a security descriptor with `SE_DACL_PROTECTED`, and pass it via `SECURITY_ATTRIBUTES` to `CreateFileW` so the file is created with the restrictive ACL already in place.
- If the ACL cannot be applied, delete the partially created file before returning the error.

### Cleanup on failure

Any temp-then-rename workflow should remove the temporary file if creation, permission hardening, writing, or renaming fails. A leftover `.tmp` file may keep permissive permissions/ACLs and confuse later runs.

## Why This Matters

- The token temp file was created on Windows with the inherited ACL, then tightened. A window existed where another user could open the file before the owner-only DACL was applied.
- A failed ACL application could leave a `.tmp` file behind with permissive permissions.

Both issues were caught in review and fixed by creating the file with the restrictive DACL already in place and by cleaning up the temp file on any error.

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
