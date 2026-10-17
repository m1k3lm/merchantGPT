-- App-level settings shared by every shop (e.g. the Google Ads OAuth client).
CREATE TABLE app_settings (
    key        TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
