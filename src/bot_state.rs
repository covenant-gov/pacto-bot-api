use crate::config::BotConfig;
use crate::errors::DaemonError;
use crate::signer::SignerBackend;

/// Runtime state for a single configured bot identity.
#[derive(Debug)]
pub struct BotState {
    pub config: BotConfig,
    pub signer: SignerBackend,
}

impl BotState {
    pub fn new(config: BotConfig) -> Result<Self, DaemonError> {
        let signer = SignerBackend::from_config(&config.signing, &config.npub)?;
        Ok(Self { config, signer })
    }
}
