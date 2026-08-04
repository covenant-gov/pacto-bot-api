//! Attachment primitives shared by inbound and outbound Nostr kind:15 handling.
//!
//! `crypto` implements the AES-256-GCM layout `pacto-app` uses for encrypted
//! attachment payloads, including its nonstandard 16-byte nonce (KTD8).
//! `mime` maps between mime types and filename extensions and sniffs a mime
//! type from payload bytes. Later units add their own submodules
//! (`inbound`, `outbound`, `blossom`) alongside these.

pub mod crypto;
pub mod mime;
