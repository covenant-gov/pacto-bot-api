//! Build-time version metadata.

/// Cargo package version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Short git commit hash (8 characters), or `"unknown"` when not built from a
/// git tree.
pub const GIT_COMMIT_SHORT: &str = env!("GIT_COMMIT_SHORT");

/// `mdk-core`/`mdk-sqlite-storage`/`mdk-storage-traits` version pinned in
/// `Cargo.toml` and resolved in `Cargo.lock` (KTD1: no `git =` dependency
/// on any MDK repository). Reported at runtime (R6, R41) so a future MDK
/// advisory can be matched against a running deployment. Update alongside
/// an `mdk-*` version bump in `Cargo.toml`.
pub const MDK_VERSION: &str = "0.8.0";

/// Identifier for the MLS wire encoding generation this daemon speaks:
/// base64 KeyPackage/Welcome content with a mandatory `encoding` tag
/// (`mdk-core` 0.8.0 / `openmls` 0.8.1), the generation shipped
/// `pacto-app` builds already require. Distinct from the pre-upgrade
/// generation (`mdk-core` 0.5.2 / `openmls` 0.7.4), which published hex
/// content with no `encoding` tag and cannot interoperate with a peer on
/// this generation.
pub const MLS_WIRE_GENERATION: &str = "mdk-0.8-base64-encoding-tag";

/// Vendored OpenSSL release statically linked via `rusqlite`'s
/// `bundled-sqlcipher-vendored-openssl` feature (KD6), from `openssl-src`'s
/// pinned upstream version in `Cargo.lock`. Update alongside an
/// `openssl-src` version bump.
pub const VENDORED_OPENSSL_VERSION: &str = "3.6.3";

/// Vendored SQLCipher release statically linked via `libsqlite3-sys`'s
/// `bundled-sqlcipher*` features, from `libsqlite3-sys`'s pinned upstream
/// SQLCipher release (`upgrade_sqlcipher.sh`'s `SQLCIPHER_VERSION`).
/// Update alongside a `libsqlite3-sys` version bump.
pub const VENDORED_SQLCIPHER_VERSION: &str = "4.6.1";
