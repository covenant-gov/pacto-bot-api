use crate::config::SigningConfig;
use crate::errors::DaemonError;

/// Abstract signing backend used by the daemon.
pub trait Signer: Send + Sync {
    /// Return the public key for this signer.
    fn public_key(&self) -> String;
}

/// Concrete signing backend selected from config.
#[derive(Debug)]
pub enum SignerBackend {
    /// Dev-only local nsec key.
    LocalKey { public_key: String },
    /// Local NIP-46 bunker connection.
    BunkerLocal { uri: String },
    /// Production NIP-46 bunker connection.
    BunkerRemote { uri: String },
}

impl SignerBackend {
    pub fn from_config(config: &SigningConfig) -> Result<Self, DaemonError> {
        match config {
            SigningConfig::Nsec { nsec } => {
                if nsec.is_empty() {
                    return Err(DaemonError::Config(
                        "nsec backend requires a non-empty key".into(),
                    ));
                }
                Ok(SignerBackend::LocalKey {
                    public_key: String::new(),
                })
            }
            SigningConfig::BunkerLocal { uri } => Ok(SignerBackend::BunkerLocal {
                uri: uri.clone(),
            }),
            SigningConfig::BunkerRemote { uri } => Ok(SignerBackend::BunkerRemote {
                uri: uri.clone(),
            }),
        }
    }
}

impl Signer for SignerBackend {
    fn public_key(&self) -> String {
        match self {
            SignerBackend::LocalKey { public_key } => public_key.clone(),
            SignerBackend::BunkerLocal { .. } | SignerBackend::BunkerRemote { .. } => {
                String::new()
            }
        }
    }
}
