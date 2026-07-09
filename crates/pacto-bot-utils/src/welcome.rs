//! Welcome gift-wrap logic for `create-mls-group`.
//!
//! Implements the NIP-59 gift-wrap used to deliver a kind:444 MLS welcome
//! rumor to the invited bot.

use nostr::nips::nip44;
use nostr::{Event, JsonUtil, Keys, Kind, PublicKey, Tag, UnsignedEvent};
use thiserror::Error;

/// Errors that can occur while gift-wrapping a welcome rumor.
#[derive(Debug, Error)]
pub enum WelcomeError {
    /// Failed to sign the welcome rumor.
    #[error("failed to sign welcome rumor: {0}")]
    SignRumor(#[source] nostr::event::Error),

    /// Failed to encrypt the NIP-59 seal.
    #[error("failed to encrypt NIP-59 seal: {0}")]
    EncryptSeal(#[source] nip44::Error),

    /// Failed to sign the NIP-59 seal.
    #[error("failed to sign NIP-59 seal: {0}")]
    SignSeal(#[source] nostr::event::Error),

    /// Failed to encrypt the NIP-59 gift-wrap.
    #[error("failed to encrypt NIP-59 gift-wrap: {0}")]
    EncryptGiftWrap(#[source] nip44::Error),

    /// Failed to sign the gift-wrap event.
    #[error("failed to sign gift-wrap: {0}")]
    SignGiftWrap(#[source] nostr::event::Error),
}

/// Gift-wrap a welcome rumor addressed to a recipient.
///
/// Returns a `kind:1059` gift-wrap event containing the signed welcome rumor.
/// The returned event is encrypted for the recipient and wrapped in a fresh
/// ephemeral key pair, following NIP-59.
pub async fn gift_wrap_welcome(
    sender_keys: &Keys,
    recipient: &PublicKey,
    welcome_rumor: UnsignedEvent,
) -> Result<Event, WelcomeError> {
    // Sign the welcome rumor with the sender's identity key.
    let rumor_event = welcome_rumor
        .sign(sender_keys)
        .await
        .map_err(WelcomeError::SignRumor)?;

    // Encrypt the signed rumor to the recipient, forming the NIP-59 seal.
    let seal_content = nip44::encrypt(
        sender_keys.secret_key(),
        recipient,
        rumor_event.as_json(),
        nip44::Version::default(),
    )
    .map_err(WelcomeError::EncryptSeal)?;

    let seal = UnsignedEvent::new(
        sender_keys.public_key(),
        nostr::Timestamp::now(),
        Kind::Seal,
        Vec::new(),
        seal_content,
    )
    .sign(sender_keys)
    .await
    .map_err(WelcomeError::SignSeal)?;

    // Build a fresh ephemeral key pair for the gift-wrap wrapper.
    let ephemeral = Keys::generate();

    let gift_content = nip44::encrypt(
        ephemeral.secret_key(),
        recipient,
        seal.as_json(),
        nip44::Version::default(),
    )
    .map_err(WelcomeError::EncryptGiftWrap)?;

    let gift = UnsignedEvent::new(
        ephemeral.public_key(),
        nostr::Timestamp::now(),
        Kind::GiftWrap,
        [Tag::public_key(*recipient)],
        gift_content,
    );

    gift.sign_with_keys(&ephemeral)
        .map_err(WelcomeError::SignGiftWrap)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn gift_wrap_welcome_is_kind_1059_and_addressed_to_recipient() -> Result<(), WelcomeError>
    {
        let sender = Keys::generate();
        let recipient = Keys::generate();

        let welcome_rumor = UnsignedEvent::new(
            sender.public_key(),
            nostr::Timestamp::now(),
            Kind::MlsWelcome,
            Vec::new(),
            "welcome",
        );

        let event = gift_wrap_welcome(&sender, &recipient.public_key(), welcome_rumor).await?;

        assert_eq!(event.kind, Kind::GiftWrap);
        assert!(
            event
                .tags
                .public_keys()
                .any(|pk| pk == &recipient.public_key()),
            "gift-wrap should be addressed to the recipient public key"
        );

        Ok(())
    }
}
