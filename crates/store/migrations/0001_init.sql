CREATE TABLE shops (
    id           INTEGER PRIMARY KEY,
    url          TEXT NOT NULL UNIQUE,
    name         TEXT,
    created_at   TEXT NOT NULL,
    last_used_at TEXT NOT NULL
);

CREATE TABLE connector_credentials (
    shop_id     INTEGER NOT NULL REFERENCES shops(id) ON DELETE CASCADE,
    kind        TEXT NOT NULL,
    credentials TEXT NOT NULL,
    updated_at  TEXT NOT NULL,
    PRIMARY KEY (shop_id, kind)
);

CREATE TABLE llm_settings (
    provider   TEXT PRIMARY KEY,
    api_key    TEXT NOT NULL,
    base_url   TEXT,
    model      TEXT NOT NULL,
    active     INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL
);

CREATE TABLE conversations (
    id            INTEGER PRIMARY KEY,
    shop_id       INTEGER REFERENCES shops(id) ON DELETE CASCADE,
    title         TEXT NOT NULL,
    input_tokens  INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    created_at    TEXT NOT NULL,
    updated_at    TEXT NOT NULL
);
CREATE INDEX conversations_by_shop ON conversations(shop_id, updated_at);

CREATE TABLE messages (
    id              INTEGER PRIMARY KEY,
    conversation_id INTEGER NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    seq             INTEGER NOT NULL,
    message         TEXT NOT NULL,
    created_at      TEXT NOT NULL,
    UNIQUE (conversation_id, seq)
);

CREATE TABLE memory_notes (
    id         INTEGER PRIMARY KEY,
    shop_id    INTEGER REFERENCES shops(id) ON DELETE CASCADE,
    text       TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE scheduled_tasks (
    id              INTEGER PRIMARY KEY,
    shop_id         INTEGER NOT NULL REFERENCES shops(id) ON DELETE CASCADE,
    name            TEXT NOT NULL,
    prompt          TEXT NOT NULL,
    schedule        TEXT NOT NULL,
    enabled         INTEGER NOT NULL DEFAULT 1,
    last_run_at     TEXT,
    conversation_id INTEGER REFERENCES conversations(id) ON DELETE SET NULL,
    created_at      TEXT NOT NULL
);

CREATE TABLE audit_log (
    id              INTEGER PRIMARY KEY,
    shop_id         INTEGER REFERENCES shops(id) ON DELETE SET NULL,
    conversation_id INTEGER REFERENCES conversations(id) ON DELETE SET NULL,
    tool            TEXT NOT NULL,
    arguments       TEXT NOT NULL,
    stage           TEXT NOT NULL,
    detail          TEXT,
    created_at      TEXT NOT NULL
);
CREATE INDEX audit_by_shop ON audit_log(shop_id, created_at);
