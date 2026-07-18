-- Baseline schema for pacto-bot-api agent.db
-- Mirrors the idempotent CREATE TABLE ... IF NOT EXISTS setup that existed
-- before refinery migrations were introduced. Running this baseline on a
-- fresh database creates all required tables; on an existing database it is
-- idempotent and leaves the prior schema in place.

CREATE TABLE IF NOT EXISTS cursors (
    bot_id TEXT PRIMARY KEY,
    npub TEXT NOT NULL,
    last_event_id TEXT,
    updated_at INTEGER
);

CREATE TABLE IF NOT EXISTS handlers (
    handler_id TEXT PRIMARY KEY,
    bot_ids TEXT NOT NULL,
    event_types TEXT NOT NULL,
    capabilities TEXT NOT NULL,
    reconnect_token TEXT NOT NULL,
    registered_at INTEGER
);

CREATE TABLE IF NOT EXISTS event_trace (
    bot_id TEXT NOT NULL,
    event_id TEXT NOT NULL,
    author TEXT NOT NULL,
    content_preview TEXT NOT NULL,
    action TEXT NOT NULL,
    reply_event_id TEXT,
    created_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_event_trace_bot_created
    ON event_trace (bot_id, created_at DESC);

CREATE TABLE IF NOT EXISTS mls_groups (
    bot_id TEXT NOT NULL,
    group_name TEXT NOT NULL,
    wire_id TEXT NOT NULL,
    creator_npub TEXT NOT NULL,
    relay TEXT NOT NULL,
    invited_bots TEXT NOT NULL,
    PRIMARY KEY (bot_id, group_name),
    UNIQUE (bot_id, wire_id)
);

CREATE TABLE IF NOT EXISTS mls_group_members (
    bot_id TEXT NOT NULL,
    group_name TEXT NOT NULL,
    member_npub TEXT NOT NULL,
    PRIMARY KEY (bot_id, group_name, member_npub)
);
