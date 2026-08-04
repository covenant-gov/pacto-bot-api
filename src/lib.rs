pub mod bot_state;
pub mod client_manager;
pub mod nip46;

pub use bot_state::BotState;
pub use client_manager::ClientManager;

// Re-export secrecy so consumers (and tests) can construct SecretString values
// for SigningConfig without adding a separate dependency.
pub use secrecy;
pub mod attachment;
pub mod config;
pub mod config_generated;
pub mod db;
pub mod dev_env_probe;
pub mod diagnostics;
pub mod dispatch;
pub mod errors;
pub mod events;
pub mod guide;
pub mod handlers;
pub mod mls;
pub mod mls_path;
pub mod nostr;
pub mod service_compatibility_generated;
pub mod signer;
pub mod spool;
pub mod transport;
pub mod version;

#[cfg(test)]
pub mod test_support;

/// Installs the process-wide `rustls` crypto provider.
///
/// `rustls` 0.23 refuses to pick a default automatically once more than one
/// backend feature (`ring`, `aws-lc-rs`) is compiled into the dependency
/// graph — which happens here because different transitive dependencies
/// (nostr-sdk's `wss://` relay connections, `reqwest`'s `rustls-tls`) each
/// request a backend. Without an explicit install, the first `wss://` relay
/// connection panics inside `rustls::crypto::mod::CryptoProvider`. Call this
/// once, as early as possible, from every binary entry point before any
/// TLS-capable client (relay pool, HTTP client) is constructed.
///
/// Safe to call more than once: a second install is reported as an `Err`
/// (provider already set) and is intentionally ignored.
pub fn install_tls_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}
