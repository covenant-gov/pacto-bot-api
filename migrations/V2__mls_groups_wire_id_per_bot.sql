-- Fix mls_groups wire_id unique constraint.
--
-- Older databases created before V1 used a global UNIQUE index on wire_id.
-- That breaks when the same Squad (same wire_id) is shared by multiple bots
-- in one daemon (e.g. bosun and captain in pacto-dev-env). This migration
-- recreates the table with the correct composite UNIQUE(bot_id, wire_id)
-- constraint and preserves existing rows.
--
-- For databases created with V1 or later, the target schema already matches,
-- so the table recreation is harmless and idempotent.

PRAGMA foreign_keys = OFF;

CREATE TABLE IF NOT EXISTS mls_groups_new (
    bot_id TEXT NOT NULL,
    group_name TEXT NOT NULL,
    wire_id TEXT NOT NULL,
    creator_npub TEXT NOT NULL,
    relay TEXT NOT NULL,
    invited_bots TEXT NOT NULL,
    PRIMARY KEY (bot_id, group_name),
    UNIQUE (bot_id, wire_id)
);

INSERT OR IGNORE INTO mls_groups_new
    (bot_id, group_name, wire_id, creator_npub, relay, invited_bots)
SELECT bot_id, group_name, wire_id, creator_npub, relay, invited_bots FROM mls_groups;

DROP TABLE IF EXISTS mls_groups;
ALTER TABLE mls_groups_new RENAME TO mls_groups;

PRAGMA foreign_keys = ON;
