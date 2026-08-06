-- MLS store reset marker and harvested admin sets.
--
-- U10 classifies every MLS store before MDK opens it (presence, then
-- encryption state, then legacy refinery version) and resets the ones that
-- need it. Recovery must not depend on an archive -- the default
-- configuration creates none -- so a reset-in-progress marker is committed
-- here BEFORE the destructive step: "marker present, no store at the live
-- path" is the recoverable interrupted state at every crash point.

CREATE TABLE IF NOT EXISTS mls_store_resets (
    bot_id TEXT PRIMARY KEY,
    marked_at INTEGER NOT NULL,
    reset_at INTEGER,
    archive_path TEXT
);

-- Admin pubkeys harvested from a legacy (pre-0.8.0) store's `groups` table
-- before it is moved out of the way, keyed by (bot_id, wire_id) so the
-- harvest write is a natural upsert -- re-running classification against an
-- interrupted reset does not duplicate rows. `admin_npub` is bech32, matching
-- every other pubkey column in this database; the legacy store holds hex.
CREATE TABLE IF NOT EXISTS mls_store_reset_admins (
    bot_id TEXT NOT NULL,
    wire_id TEXT NOT NULL,
    admin_npub TEXT NOT NULL,
    PRIMARY KEY (bot_id, wire_id, admin_npub)
);

-- Set on every `mls_groups` row for a bot whose MLS engine state a reset
-- destroyed (U11 gates this on `mls_store_resets`, not on a bare diff, so a
-- legitimately-evicted bot is not mismarked). NULL means the bot's engine
-- state, if any, is believed current.
ALTER TABLE mls_groups ADD COLUMN state_lost_at INTEGER;
