//! Build-time version metadata.

/// Cargo package version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Short git commit hash (8 characters), or `"unknown"` when not built from a
/// git tree.
pub const GIT_COMMIT_SHORT: &str = env!("GIT_COMMIT_SHORT");
