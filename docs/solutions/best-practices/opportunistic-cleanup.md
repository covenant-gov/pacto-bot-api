---
title: Amortize cleanup in hot paths
date: 2026-07-06
category: best-practices
module: dispatch
problem_type: best_practice
component: service_object
severity: medium
applies_when:
  - A data structure accumulates entries that may become stale
  - Cleanup involves an O(n) scan
  - Cleanup is currently triggered on every request
tags: [performance, rate-limiting, cleanup, amortization]
---

# Amortize cleanup in hot paths

## Context

The `RateLimiter` in `src/dispatch.rs` tracks token buckets per handler and per bot. A `sweep()` function removes stale buckets via `HashMap::retain`, which is O(n) over the full map. Calling it on every `check()` turned every rate-limit decision into a full-map scan.

## Guidance

Do not run O(n) cleanup on every request. Instead, amortize the work by gating it on:

- A size threshold (e.g., `map.len() > max_buckets`), and/or
- A time cadence (e.g., `now - last_sweep >= stale_after`).

Track the last sweep timestamp inside the data structure.

## Why This Matters

When many handlers/bots are active, the map can hold thousands of entries. A full scan on every request adds latency to the dispatch hot path. Opportunistic cleanup keeps the hot path bounded while still preventing unbounded growth.

## When to Apply

- Any per-request path that currently scans or compacts a collection.
- Any map/cache with TTLs or stale entries.

## Examples

```rust
use std::collections::HashMap;
use std::time::{Duration, Instant};

struct Bucket {
    // ...
}

struct BucketMap {
    map: HashMap<String, Bucket>,
    last_sweep: Instant,
}

impl BucketMap {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
            last_sweep: Instant::now(),
        }
    }

    fn needs_sweep(&self, now: Instant, max_buckets: usize, stale_after: Duration) -> bool {
        self.map.len() > max_buckets || now.duration_since(self.last_sweep) >= stale_after
    }
}
```

## Related

- `src/dispatch.rs` (`RateLimiter`, `BucketMap`)
