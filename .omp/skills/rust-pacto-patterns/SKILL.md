---
name: rust-pacto-patterns
description: >
  Pacto-specific Rust patterns for code that survives review. Use this skill when:
  (1) writing or reviewing Rust code in the pacto-bot-api daemon,
  (2) creating files with sensitive data,
  (3) using regexes in request/error paths,
  (4) cleaning up state in hot paths,
  (5) writing tests for exact protocol values.
license: MIT
compatibility: Rust 1.96+, pacto-bot-api
metadata:
  author: covenant-gov
  version: "1.0.0"
allowed-tools: Read Write Edit Glob Grep
---

# Pacto Rust Review Patterns

Apply these patterns when writing or reviewing Rust in `pacto-bot-api`. They are drawn from recurring Copilot review feedback and documented in `docs/solutions/`.

## Search first

Before implementing in a documented area, search `docs/solutions/` for relevant patterns:

```bash
grep -R "file-permissions\|regex\|cleanup\|assertions" docs/solutions/
```

## Secure file creation

Files containing sensitive data must be created with owner-only permissions. Do not rely on umask or apply permissions after a rename.

```rust
use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;

let mut file = tokio::fs::OpenOptions::new()
    .write(true)
    .create(true)
    .mode(0o600)
    .open(&path)
    .await?;
file.write_all(data).await?;
file.flush().await?;
drop(file);
tokio::fs::rename(&tmp_path, &final_path).await?;
tokio::fs::set_permissions(&final_path, Permissions::from_mode(0o600)).await?;
```

See `docs/solutions/best-practices/secure-file-creation.md`.

## Regex caching

Do not compile `regex::Regex` inside hot or error paths. Cache compiled regexes with `std::sync::LazyLock` (or `OnceLock`/`once_cell`).

```rust
use regex::Regex;
use std::sync::LazyLock;

static HEX_SECRET_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[0-9a-fA-F]{64}").expect("static regex pattern is valid")
});

fn redact_hex_secret(input: &str) -> String {
    HEX_SECRET_REGEX.replace_all(input, "[REDACTED]").into_owned()
}
```

See `docs/solutions/best-practices/regex-caching.md`.

## Amortized cleanup

Avoid O(n) scans or full-map cleanup on every request. Gate sweeps on a size threshold or a time-based cadence tracked in the data structure.

```rust
if buckets.len() > max_buckets || now - last_sweep >= stale_after {
    buckets.retain(|_, b| !b.is_stale(now, stale_after));
    last_sweep = now;
}
```

See `docs/solutions/best-practices/opportunistic-cleanup.md`.

## Exact test assertions

When the protocol or API specifies an exact value, assert the exact value. Prefix or substring checks are only appropriate when the contract is intentionally loose.

```rust
assert_eq!(content_type, "application/json; charset=utf-8");
```

Not:

```rust
assert!(content_type.contains("application/json"));
```

See `docs/solutions/best-practices/exact-test-assertions.md`.
