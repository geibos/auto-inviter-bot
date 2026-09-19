-- Привязанные группы. v1 использует одну активную, схема готова к нескольким.
CREATE TABLE chats (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    tg_chat_id INTEGER NOT NULL UNIQUE,
    title TEXT NOT NULL DEFAULT '',
    is_active INTEGER NOT NULL DEFAULT 1,
    bound_at INTEGER NOT NULL
);

-- Реестр кандидатов: позывной + опциональный username.
-- callsign_norm — lowercase-нормализация в Rust (SQLite NOCASE не работает для кириллицы).
CREATE TABLE roster (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    chat_id INTEGER NOT NULL REFERENCES chats(id) ON DELETE CASCADE,
    callsign TEXT NOT NULL,
    callsign_norm TEXT NOT NULL,
    username TEXT,
    created_by INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    joined_tg_user_id INTEGER,
    joined_username TEXT,
    joined_at INTEGER,
    UNIQUE(chat_id, callsign_norm)
);

-- История invite-ссылок. Активная = не отозвана и не использована.
CREATE TABLE invite_links (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    roster_id INTEGER NOT NULL REFERENCES roster(id) ON DELETE CASCADE,
    invite_link TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    revoked_at INTEGER,
    used_at INTEGER
);

-- Инвариант: не более одной активной ссылки на запись реестра (design.md D3/D5).
CREATE UNIQUE INDEX idx_invite_links_one_active
    ON invite_links(roster_id)
    WHERE revoked_at IS NULL AND used_at IS NULL;
