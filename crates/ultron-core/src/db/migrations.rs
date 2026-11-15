use anyhow::Result;
use rusqlite::Connection;

/// Sıralı şema sürümleri. Her girdi bir sürüm yükseltmesidir.
const MIGRATIONS: &[&str] = &[
    r#"
    CREATE TABLE projects (
        id TEXT PRIMARY KEY,
        name TEXT NOT NULL,
        path TEXT NOT NULL,
        aliases_json TEXT NOT NULL DEFAULT '[]'
    );
    CREATE TABLE chats (
        id TEXT PRIMARY KEY,
        project_id TEXT REFERENCES projects(id),
        title TEXT NOT NULL DEFAULT '',
        created_at TEXT NOT NULL
    );
    CREATE TABLE engine_sessions (
        id TEXT PRIMARY KEY,
        chat_id TEXT NOT NULL REFERENCES chats(id),
        engine TEXT NOT NULL,
        native_session_id TEXT,
        status TEXT NOT NULL,
        started_at TEXT NOT NULL,
        ended_at TEXT
    );
    CREATE TABLE events (
        id TEXT PRIMARY KEY,
        chat_id TEXT NOT NULL REFERENCES chats(id),
        session_id TEXT REFERENCES engine_sessions(id),
        seq INTEGER NOT NULL,
        ts TEXT NOT NULL,
        kind TEXT NOT NULL,
        payload_json TEXT NOT NULL,
        UNIQUE(chat_id, seq)
    );
    CREATE VIRTUAL TABLE messages_fts USING fts5(
        chat_id UNINDEXED,
        event_id UNINDEXED,
        text,
        tokenize = 'unicode61 remove_diacritics 2'
    );
    CREATE TABLE work_state (
        chat_id TEXT PRIMARY KEY REFERENCES chats(id),
        json TEXT NOT NULL,
        updated_at TEXT NOT NULL
    );
    "#,
];

/// Bekleyen migrasyonları `user_version` üzerinden sırayla uygular.
pub fn apply(conn: &Connection) -> Result<()> {
    let current: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    for (idx, sql) in MIGRATIONS.iter().enumerate() {
        let version = (idx + 1) as i64;
        if version <= current {
            continue;
        }
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", version)?;
        tx.commit()?;
    }
    Ok(())
}
