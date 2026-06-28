use crate::bot_state::BotState;
use crate::config::DaemonConfig;
use crate::errors::DaemonError;
use crate::handlers::HandlerRegistry;
use crate::nostr::NostrClient;
use std::collections::HashMap;

/// Manages multiple bot identities and provides npub/bot_id lookups.
#[derive(Debug)]
pub struct ClientManager {
    bots: HashMap<String, BotState>,
    bot_id_map: HashMap<String, String>,
    pub nostr_client: NostrClient,
    pub handler_registry: HandlerRegistry,
}

impl ClientManager {
    pub fn new(config: DaemonConfig, nostr_client: NostrClient) -> Result<Self, DaemonError> {
        let mut bots = HashMap::new();
        let mut bot_id_map = HashMap::new();

        for bot_config in config.bots {
            let npub = bot_config.npub.clone();
            let bot_id = bot_config.id.clone();
            let bot_state = BotState::new(bot_config)?;
            bots.insert(npub.clone(), bot_state);
            bot_id_map.insert(bot_id, npub);
        }

        Ok(Self {
            bots,
            bot_id_map,
            nostr_client,
            handler_registry: HandlerRegistry::new(),
        })
    }

    pub fn get_bot(&self, npub: &str) -> Option<&BotState> {
        self.bots.get(npub)
    }

    pub fn get_bot_by_id(&self, bot_id: &str) -> Option<&BotState> {
        self.bot_id_map
            .get(bot_id)
            .and_then(|npub| self.bots.get(npub))
    }

    pub fn is_authorized(&self, handler_id: &str, bot_id: &str) -> bool {
        self.handler_registry.is_authorized(handler_id, bot_id)
    }
}
