---
title: Assert exact values for exact protocol contracts
date: 2026-07-06
category: best-practices
module: transport
problem_type: best_practice
component: testing_framework
severity: low
applies_when:
  - A test helper checks a protocol or API value
  - The contract specifies an exact value
tags: [testing, assertions, http, content-type]
---

# Assert exact values for exact protocol contracts

## Context

The HTTP transport tests had a helper `assert_json_content_type` that only checked for the substring `application/json`. This allowed regressions where the `charset=utf-8` parameter was accidentally dropped.

## Guidance

When the contract specifies an exact value, assert the exact value. Use prefix/substring checks only when the contract is intentionally loose.

## Why This Matters

Tests should catch regressions, not just broad categories. A substring assertion that passes after a regression gives false confidence.

## When to Apply

- HTTP headers, JSON-RPC fields, or any protocol value with a defined format.
- Test helpers that wrap assertions.

## Examples

```rust
// Before: passes even if charset is dropped
fn assert_json_content_type(response: &str) {
    assert!(response.contains("application/json"));
}

// After: exact match
fn assert_json_content_type(response: &str) {
    let content_type = response
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.trim().eq_ignore_ascii_case("content-type") {
                Some(value.trim().to_string())
            } else {
                None
            }
        })
        .expect("Content-Type header present");
    assert_eq!(content_type, "application/json; charset=utf-8");
}
```

## Related

- `tests/transport_http.rs` (`assert_json_content_type`)
