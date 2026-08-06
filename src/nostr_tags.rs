//! Tag-kind and reaction-event construction seam.
//!
//! `nostr::TagKind` and `EventBuilder::reaction_extended` are used across the
//! daemon to build and match `e`/`p`/`h`/`k` tags, attachment-metadata custom
//! tags, and NIP-25 reaction events. Centralizing every constructor and
//! predicate here means a nostr crate upgrade that reshapes `TagKind` or
//! `reaction_extended`'s signature only needs to touch this file, and a
//! future lint can scope `TagKind`/`reaction_extended` usage to this module
//! alone. [`Tag`] itself is re-exported unchanged since callers still need to
//! name and inspect individual tags; nothing else belongs in this module.

use nostr::{EventBuilder, EventId, Kind, PublicKey, TagKind, Tags};

pub use nostr::Tag;

/// Build an `e` tag referencing `event_id`, marked as a reply.
pub fn reply_e_tag(event_id: EventId) -> Tag {
    Tag::custom(
        TagKind::e(),
        [event_id.to_hex(), String::new(), String::from("reply")],
    )
}

/// Build a custom `ms` tag carrying a millisecond offset.
pub fn ms_tag(value: impl Into<String>) -> Tag {
    Tag::custom(TagKind::custom("ms"), [value.into()])
}

/// Build a custom kind:15 attachment-metadata tag (`file-type`, `size`,
/// `decryption-key`, `filename`, ...).
pub fn attachment_tag(name: &'static str, value: impl Into<String>) -> Tag {
    Tag::custom(TagKind::custom(name), [value.into()])
}

/// Find the first `e` tag in `tags`.
pub fn find_e_tag(tags: &Tags) -> Option<&Tag> {
    tags.find(TagKind::e())
}

/// Return the content of the last `e` tag in `tags`, if any.
pub fn last_e_tag_content(tags: &Tags) -> Option<&str> {
    tags.filter(TagKind::e()).last()?.content()
}

/// Find the first `p` tag in `tags`.
pub fn find_p_tag(tags: &Tags) -> Option<&Tag> {
    tags.find(TagKind::p())
}

/// Find the first `k` tag in `tags`.
pub fn find_k_tag(tags: &Tags) -> Option<&Tag> {
    tags.find(TagKind::k())
}

/// Find a custom-named tag (e.g. the DM rumor's `ms` millisecond-offset tag).
pub fn find_custom_tag<'a>(tags: &'a Tags, name: &str) -> Option<&'a Tag> {
    tags.find(TagKind::custom(name))
}

/// Return the content of the first `h` (MLS group) tag in `tags`, if any.
pub fn h_tag_content(tags: &Tags) -> Option<String> {
    tags.find(TagKind::h())
        .and_then(|tag| tag.content())
        .map(str::to_owned)
}

/// Look up a custom kind:15 attachment-metadata tag's content by name.
pub fn find_attachment_tag<'a>(tags: &'a Tags, name: &str) -> Option<&'a str> {
    find_custom_tag(tags, name).and_then(|tag| tag.content())
}

/// Build a NIP-25 reaction event reacting to `target` with `emoji`,
/// addressed to `recipient` and optionally scoped to `reacted_to_kind`.
pub fn reaction_event(
    target: EventId,
    recipient: PublicKey,
    reacted_to_kind: Option<Kind>,
    emoji: &str,
) -> EventBuilder {
    // `EventBuilder::reaction_extended` was removed by the time nostr
    // reached 0.44; `reaction` + `ReactionTarget` is the current NIP-25
    // constructor. `ReactionTarget`'s fields are public specifically to
    // support building one without a full target `Event` in hand -- the
    // seam only ever has the target's id, author, and kind.
    EventBuilder::reaction(
        nostr::nips::nip25::ReactionTarget {
            event_id: target,
            public_key: recipient,
            coordinate: None,
            kind: reacted_to_kind,
            relay_hint: None,
        },
        emoji,
    )
}

/// Decode a reaction rumor's target event id and emoji from its tags and
/// content, matching the layout [`reaction_event`] writes: the last `e` tag
/// names the target rumor and the content is the reaction emoji.
///
/// Returns `None` when `content` is empty or there is no target `e` tag.
pub fn decode_reaction<'a>(tags: &'a Tags, content: &'a str) -> Option<(&'a str, &'a str)> {
    if content.is_empty() {
        return None;
    }
    let target = last_e_tag_content(tags)?;
    Some((target, content))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::Keys;

    fn event_id(byte: u8) -> EventId {
        let hex = format!("{byte:02x}").repeat(32);
        EventId::from_hex(&hex).expect("valid 32-byte hex event id")
    }

    #[test]
    fn tag_built_through_seam_round_trips_through_seam_predicate() {
        let target = event_id(0x01);
        let tag = reply_e_tag(target);
        let tags = Tags::from_list(vec![tag]);

        let found = find_e_tag(&tags).expect("e tag should be found");
        assert!(found.is_reply(), "e tag should be marked as reply");
        assert_eq!(found.content(), Some(target.to_hex().as_str()));

        let ms = ms_tag("42");
        let ms_tags = Tags::from_list(vec![ms]);
        let found_ms = find_custom_tag(&ms_tags, "ms").expect("ms tag should be found");
        assert_eq!(found_ms.content(), Some("42"));
    }

    #[test]
    fn reaction_event_built_through_seam_decodes_to_same_target_and_emoji() {
        let sender = Keys::generate();
        let recipient = Keys::generate();
        let target = event_id(0x02);
        let emoji = "👍";

        let rumor = reaction_event(
            target,
            recipient.public_key(),
            Some(Kind::PrivateDirectMessage),
            emoji,
        )
        .build(sender.public_key());

        let (decoded_target, decoded_emoji) =
            decode_reaction(&rumor.tags, &rumor.content).expect("reaction should decode");
        assert_eq!(decoded_target, target.to_hex());
        assert_eq!(decoded_emoji, emoji);
    }

    #[test]
    fn pre_seam_tag_is_still_matched_by_seam_predicate() {
        // Simulates a tag built by code that has not been routed through the
        // seam yet (or an event received from a relay built by other
        // software): raw `nostr::Tag`/`nostr::TagKind` construction, not the
        // seam's own constructors.
        let target = event_id(0x03);
        let raw_tag = nostr::Tag::custom(
            nostr::TagKind::e(),
            [target.to_hex(), String::new(), String::new()],
        );
        let tags = Tags::from_list(vec![raw_tag]);

        let found = find_e_tag(&tags).expect("raw e tag should still be matched");
        assert_eq!(found.content(), Some(target.to_hex().as_str()));
    }
}
