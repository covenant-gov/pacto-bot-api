---
title: JSON-RPC custom error code allocation
date: 2026-07-06
category: best-practices
module: errors
problem_type: best_practice
component: protocol
severity: high
applies_when:
  - Adding a new JSON-RPC error response to a transport or handler-facing method
  - Changing an existing JSON-RPC error code
  - Mapping a DaemonError variant to a JSON-RPC code
tags: [protocol, json-rpc, error-codes, plan-traceability]
---

# JSON-RPC custom error code allocation

## Context

The daemon reserves the server-error range `-32000` to `-32099` for Pacto-specific error conditions. The primary implementation plan (`docs/plans/2026-06-24-001-feat-pacto-bot-api-daemon-plan.md`) documents the initial code table, and `src/errors.rs` maps `DaemonError` variants to those codes. Reusing or colliding with an existing code makes client-side error handling impossible.

## Guidance

1. **Before picking a new code, read both sources:**
   - `docs/plans/2026-06-24-001-feat-pacto-bot-api-daemon-plan.md` ("Error Codes" table)
   - `src/errors.rs` (`DaemonError::to_json_rpc_code`)

2. **Pick the next unused code in the `-32000` to `-32099` range.** Do not reuse an existing code because the message string differs; clients rely on the code.

3. **Prefer routing through `DaemonError` when possible.** Transport and dispatch code should use `DaemonError::to_json_rpc_code()` or `Into<JsonRpcError>` rather than hard-coding a magic number. If a transport-level error has no corresponding `DaemonError` variant (e.g., HTTP payload-too-large, which is caught before the request reaches the dispatch layer), document the chosen code in the plan and add a named constant or explanatory comment.

4. **Update the plan when adding a new custom code.** Add a row to the plan's error-code table so the next contributor does not pick the same number.

## Why This Matters

`-32003` was originally used for Bunker errors, but an earlier PR also used it for HTTP payload-too-large. Clients could not distinguish a failed bunker signing operation from an oversized request. Copilot flagged this in review.

## When to Apply

- Any change that adds or modifies a JSON-RPC error code returned to handlers.
- Any transport-level error that does not map to an existing `DaemonError` variant.

## Examples

```rust
// Avoid: magic number that may collide with an existing code
JsonRpcError::new(-32003, "payload too large")

// Prefer: a named constant, next unused code, and a comment referencing the plan
const HTTP_PAYLOAD_TOO_LARGE: i32 = -32012; // next unused Pacto-specific code
JsonRpcError::new(HTTP_PAYLOAD_TOO_LARGE, "payload too large")
```

## Related

- `src/errors.rs` (`DaemonError::to_json_rpc_code`)
- `docs/plans/2026-06-24-001-feat-pacto-bot-api-daemon-plan.md` (Error Codes table)
- `src/transport/http.rs`
