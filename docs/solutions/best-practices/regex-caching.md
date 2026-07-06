---
title: Cache compiled regexes in hot paths
date: 2026-07-06
category: best-practices
module: diagnostics
problem_type: best_practice
component: development_workflow
severity: medium
applies_when:
  - A function uses regex::Regex
  - The function is called repeatedly, including error paths
tags: [performance, regex, caching, lazy-lock]
---

# Cache compiled regexes in hot paths

## Context

`regex::Regex::new` compiles the pattern into a finite automaton. Compiling a regex on every call can dominate CPU under error-heavy or high-throughput workloads.

## Guidance

Cache compiled regexes with `std::sync::LazyLock` (or `OnceLock`/`once_cell`).

- Use a `LazyLock<Regex>` for a single pattern.
- Use a `LazyLock<HashMap<&'static str, Regex>>` for a fixed set of patterns keyed by strings.

## Why This Matters

`redact_secrets` is called from `record_error`. Under a load of errors, recompiling regexes on every call becomes measurable. Caching makes the function O(input) instead of O(input + regex_compile).

## When to Apply

- Any regex used in a request handler, error path, or loop.
- Any regex with a fixed pattern.

## Examples

```rust
use regex::Regex;
use std::sync::LazyLock;

// Before: recompiles on every call
fn redact_hex_secret(input: &str) -> String {
    let Ok(re) = Regex::new(r"[0-9a-fA-F]{64}") else {
        return input.to_string();
    };
    re.replace_all(input, "[REDACTED]").into_owned()
}

// After: compiled once per process
static HEX_SECRET_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[0-9a-fA-F]{64}")
        .expect("static regex pattern is valid")
});

fn redact_hex_secret(input: &str) -> String {
    HEX_SECRET_REGEX.replace_all(input, "[REDACTED]").into_owned()
}
```

## Related

- `src/diagnostics.rs` (`redact_secrets`, `redact_query_param`)
