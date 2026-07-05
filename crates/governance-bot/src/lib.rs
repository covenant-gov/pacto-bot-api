//! Example on-chain governance reader for the Pacto snapshot bot.
//!
//! This crate is intentionally small and self-contained: it reads public
//! governance and treasury state from the `pacto-gov` contracts and exposes
//! a stable [`SnapshotData`](crate::evm::snapshot::SnapshotData) aggregate that
//! the snapshot formatter (U9) can consume.

pub mod evm;
pub mod snapshot;
