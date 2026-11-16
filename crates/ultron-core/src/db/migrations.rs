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
    // v2: sohbet başına güncel motor ve son görülen git HEAD.
    r#"
    ALTER TABLE chats ADD COLUMN current_engine TEXT;
    ALTER TABLE chats ADD COLUMN last_head TEXT;
    "#,
    // v3: proje kataloğu alanları ile sohbet meta verisi (yalnız ekleyici).
    r#"
    ALTER TABLE projects ADD COLUMN description TEXT;
    ALTER TABLE projects ADD COLUMN last_active_at TEXT;
    ALTER TABLE chats ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE chats ADD COLUMN archived INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE chats ADD COLUMN title_source TEXT NOT NULL DEFAULT 'auto';
    ALTER TABLE chats ADD COLUMN updated_at TEXT;
    UPDATE chats SET updated_at = created_at WHERE updated_at IS NULL;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_v3_is_additive() {
        let conn = Connection::open_in_memory().unwrap();
        for (idx, sql) in MIGRATIONS.iter().take(2).enumerate() {
            conn.execute_batch(sql).unwrap();
            conn.pragma_update(None, "user_version", (idx + 1) as i64)
                .unwrap();
        }
        conn.execute(
            "INSERT INTO projects(id, name, path) VALUES ('p1', 'Ad', 'yol')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO chats(id, project_id, title, created_at, current_engine)
             VALUES ('c1', 'p1', 'eski', '2026-01-01T00:00:00+00:00', 'claude')",
            [],
        )
        .unwrap();

        apply(&conn).unwrap();

        let v: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v, 3);
        let row: (String, String, String, i64, i64, String, String) = conn
            .query_row(
                "SELECT title, project_id, current_engine, pinned, archived, title_source, updated_at
                 FROM chats WHERE id = 'c1'",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            row,
            (
                "eski".into(),
                "p1".into(),
                "claude".into(),
                0,
                0,
                "auto".into(),
                "2026-01-01T00:00:00+00:00".into()
            )
        );
        let name: String = conn
            .query_row("SELECT name FROM projects WHERE id = 'p1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(name, "Ad");
    }
}
