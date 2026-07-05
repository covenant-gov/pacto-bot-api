pub mod bot_state;
pub mod client_manager;
pub mod nip46;

pub use bot_state::BotState;
pub use client_manager::ClientManager;

// Re-export secrecy so consumers (and tests) can construct SecretString values
// for SigningConfig without adding a separate dependency.
pub use secrecy;
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
pub mod metrics_generated;
pub mod mls;
pub mod nostr;
pub mod service_compatibility_generated;
pub mod signer;
pub mod transport;
pub mod version;
