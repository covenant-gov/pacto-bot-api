//! Seam for the `nostr` JSON round-trip and raw-`Keys` signing surface.
//!
//! `nostr` 0.45 removes the `JsonUtil` trait — and with it `as_json` /
//! `from_json` — plus `UnsignedEvent::sign_with_keys` and
//! `EventBuilder::sign_with_keys` from its public API. Every call site in
//! this crate and its test suites is bound to the helpers below instead of
//! naming those symbols directly, so the eventual 0.44/0.45 migration
//! touches this one file rather than every module that ever serialized an
//! event or signed with a raw [`Keys`] pair.

use nostr::{Event, EventBuilder, Filter, JsonUtil, Keys, UnsignedEvent};

/// Serialize an [`Event`] to its canonical JSON string.
pub fn event_to_json(event: &Event) -> String {
    event.as_json()
}

/// Parse an [`Event`] from a JSON string or byte slice.
pub fn event_from_json<T: AsRef<[u8]>>(json: T) -> Result<Event, nostr::event::Error> {
    Event::from_json(json)
}

/// Serialize an [`UnsignedEvent`] to its canonical JSON string.
pub fn unsigned_event_to_json(event: &UnsignedEvent) -> String {
    event.as_json()
}

/// Parse an [`UnsignedEvent`] from a JSON string or byte slice.
pub fn unsigned_event_from_json<T: AsRef<[u8]>>(
    json: T,
) -> Result<UnsignedEvent, nostr::event::Error> {
    UnsignedEvent::from_json(json)
}

/// Serialize a [`Filter`] to its canonical JSON string.
pub fn filter_to_json(filter: &Filter) -> String {
    filter.as_json()
}

/// Parse a [`Filter`] from a JSON string or byte slice.
pub fn filter_from_json<T: AsRef<[u8]>>(json: T) -> Result<Filter, serde_json::Error> {
    Filter::from_json(json)
}

/// Build, sign, and return an [`Event`] from an [`UnsignedEvent`] rumor,
/// using a raw [`Keys`] pair rather than the daemon's [`crate::signer::Signer`]
/// abstraction. Used where a real signing backend is not in play — ephemeral
/// gift-wrap keys and test fixtures.
pub fn sign_unsigned(rumor: UnsignedEvent, keys: &Keys) -> Result<Event, nostr::event::Error> {
    rumor.sign_with_keys(keys)
}

/// Build, sign, and return an [`Event`] from an [`EventBuilder`], using a raw
/// [`Keys`] pair rather than the daemon's [`crate::signer::Signer`]
/// abstraction.
pub fn sign_builder(
    builder: EventBuilder,
    keys: &Keys,
) -> Result<Event, nostr::event::builder::Error> {
    builder.sign_with_keys(keys)
}

#[cfg(test)]
mod tests {
    use nostr::{Kind, Timestamp};

    use super::*;

    fn sample_event() -> Event {
        let keys = Keys::generate();
        EventBuilder::text_note("nostr_json seam test")
            .sign_with_keys(&keys)
            .expect("sign sample event")
    }

    #[test]
    fn event_json_round_trip_matches_direct_as_json_and_from_json() {
        let event = sample_event();

        assert_eq!(event_to_json(&event), event.as_json());

        let via_seam = event_from_json(event_to_json(&event)).expect("seam parse");
        let direct = Event::from_json(event.as_json()).expect("direct parse");
        assert_eq!(via_seam, direct);
        assert_eq!(via_seam, event);
    }

    #[test]
    fn unsigned_event_json_round_trip_matches_direct_as_json_and_from_json() {
        let keys = Keys::generate();
        let unsigned = UnsignedEvent::new(
            keys.public_key(),
            Timestamp::now(),
            Kind::TextNote,
            Vec::new(),
            "unsigned seam test",
        );

        assert_eq!(unsigned_event_to_json(&unsigned), unsigned.as_json());

        let via_seam =
            unsigned_event_from_json(unsigned_event_to_json(&unsigned)).expect("seam parse");
        let direct = UnsignedEvent::from_json(unsigned.as_json()).expect("direct parse");
        assert_eq!(via_seam, direct);
        assert_eq!(via_seam, unsigned);
    }

    #[test]
    fn filter_json_round_trip_matches_direct_as_json_and_from_json() {
        let filter = Filter::new().kind(Kind::TextNote).limit(5);

        assert_eq!(filter_to_json(&filter), filter.as_json());

        let via_seam = filter_from_json(filter_to_json(&filter)).expect("seam parse");
        let direct = Filter::from_json(filter.as_json()).expect("direct parse");
        assert_eq!(via_seam, direct);
    }

    #[test]
    fn sign_unsigned_gift_wrap_verifies_like_pre_seam_sign_with_keys() {
        let keys = Keys::generate();
        let unsigned = UnsignedEvent::new(
            keys.public_key(),
            Timestamp::now(),
            Kind::GiftWrap,
            Vec::new(),
            "gift wrap content",
        );

        let via_seam = sign_unsigned(unsigned.clone(), &keys).expect("seam sign");
        assert!(via_seam.verify().is_ok());

        let direct = unsigned.sign_with_keys(&keys).expect("direct sign");
        assert!(direct.verify().is_ok());

        // Both events are built from identical unsigned content, so they
        // carry the same id and pubkey and pass the same signature check —
        // the seam changes nothing about what gets signed.
        assert_eq!(via_seam.id, direct.id);
        assert_eq!(via_seam.pubkey, direct.pubkey);
    }

    #[test]
    fn sign_builder_produces_a_verifiable_event() {
        let keys = Keys::generate();
        let event = sign_builder(EventBuilder::text_note("builder seam test"), &keys)
            .expect("seam sign builder");
        assert!(event.verify().is_ok());
        assert_eq!(event.pubkey, keys.public_key());
    }
}
