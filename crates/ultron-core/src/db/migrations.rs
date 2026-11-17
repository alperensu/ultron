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
    // v4: Claude geçmişi içe aktarma ofsetleri (yalnız ekleyici).
    r#"
    CREATE TABLE import_state (
        path TEXT PRIMARY KEY,
        byte_offset INTEGER NOT NULL,
        updated_at TEXT NOT NULL
    );
    "#,
    // v5: dosya yeniden yazımlarını ve mesaj dışı cwd bilgisini kalıcı izle.
    r#"
    ALTER TABLE import_state ADD COLUMN generation INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE import_state ADD COLUMN file_size INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE import_state ADD COLUMN first_line_hash TEXT;
    ALTER TABLE import_state ADD COLUMN session_cwd TEXT;
    UPDATE import_state SET file_size = byte_offset;
    "#,
    // v6: gösterim yolunu ve mevcut proje/sohbet kimliklerini koruyan arama anahtarı.
    r#"
    ALTER TABLE projects ADD COLUMN path_key TEXT NOT NULL DEFAULT '';
    CREATE INDEX projects_path_key ON projects(path_key);
    "#,
    // v7: işlenmiş önekin son penceresini yeniden yazımlara karşı doğrula.
    r#"
    ALTER TABLE import_state ADD COLUMN prefix_hash TEXT;
    "#,
    // v8: kalıcı onay kuyruğu; eski tablolara ve kayıtlara dokunmaz.
    r#"
    CREATE TABLE approvals (
        id TEXT PRIMARY KEY,
        chat_id TEXT REFERENCES chats(id),
        engine TEXT NOT NULL,
        category TEXT NOT NULL,
        reason TEXT NOT NULL,
        action_json TEXT NOT NULL,
        status TEXT NOT NULL DEFAULT 'pending'
            CHECK(status IN ('pending', 'approved', 'denied', 'expired')),
        created_at TEXT NOT NULL,
        resolved_at TEXT,
        resolved_by TEXT CHECK(resolved_by IN ('desktop', 'telegram', 'timeout', 'restart')),
        CHECK((status = 'pending' AND resolved_at IS NULL AND resolved_by IS NULL)
           OR (status != 'pending' AND resolved_at IS NOT NULL AND resolved_by IS NOT NULL))
    );
    CREATE INDEX approvals_status_created ON approvals(status, created_at);
    "#,
    // v9: bağlantı sahipliği ve durumla atomik yazılan, yeniden denenebilir olay çıkışı.
    r#"
    CREATE TABLE approvals_v9 (
        id TEXT PRIMARY KEY,
        chat_id TEXT REFERENCES chats(id),
        engine TEXT NOT NULL,
        category TEXT NOT NULL,
        reason TEXT NOT NULL,
        action_json TEXT NOT NULL,
        status TEXT NOT NULL DEFAULT 'pending'
            CHECK(status IN ('pending', 'approved', 'denied', 'expired')),
        created_at TEXT NOT NULL,
        resolved_at TEXT,
        resolved_by TEXT CHECK(resolved_by IN ('desktop', 'telegram', 'approver', 'timeout', 'restart')),
        requester_id TEXT,
        CHECK((status = 'pending' AND resolved_at IS NULL AND resolved_by IS NULL)
           OR (status != 'pending' AND resolved_at IS NOT NULL AND resolved_by IS NOT NULL))
    );
    INSERT INTO approvals_v9(id, chat_id, engine, category, reason, action_json, status,
        created_at, resolved_at, resolved_by)
        SELECT id, chat_id, engine, category, reason, action_json, status,
            created_at, resolved_at, resolved_by FROM approvals;
    DROP TABLE approvals;
    ALTER TABLE approvals_v9 RENAME TO approvals;
    CREATE INDEX approvals_status_created ON approvals(status, created_at);
    CREATE INDEX approvals_requester ON approvals(requester_id, id);
    CREATE TABLE approval_outbox (
        seq INTEGER PRIMARY KEY AUTOINCREMENT,
        approval_id TEXT NOT NULL,
        event_json TEXT NOT NULL
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
        if version == 6 {
            // SQLite LOWER yalnız ASCII işler; uygulamayla aynı Unicode kuralını kullan.
            let paths = {
                let mut stmt = tx.prepare("SELECT id, path FROM projects")?;
                stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };
            for (id, path) in paths {
                tx.execute("UPDATE projects SET path_key = ?2 WHERE id = ?1",
                    rusqlite::params![id, super::normalized_project_path(&path)])?;
            }
        }
        tx.pragma_update(None, "user_version", version)?;
        tx.commit()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v9_preserves_existing_v8_approvals_and_accepts_authenticated_resolver() {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        for (idx, sql) in MIGRATIONS.iter().take(8).enumerate() {
            conn.execute_batch(sql).unwrap();
            conn.pragma_update(None, "user_version", (idx + 1) as i64).unwrap();
        }
        conn.execute("INSERT INTO approvals(id, engine, category, reason, action_json, created_at) VALUES ('pending', 'claude', 'Delete', 'test', '{}', '2026-01-01T00:00:00Z')", []).unwrap();
        conn.execute("INSERT INTO approvals(id, engine, category, reason, action_json, status, created_at, resolved_at, resolved_by) VALUES ('resolved', 'claude', 'Delete', 'test', '{}', 'approved', '2026-01-01T00:00:00Z', '2026-01-01T00:00:01Z', 'desktop')", []).unwrap();
        apply(&conn).unwrap();
        apply(&conn).unwrap();
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM approvals", [], |r| r.get(0)).unwrap();
        assert_eq!(count, 2);
        let by: String = conn.query_row("SELECT resolved_by FROM approvals WHERE id = 'resolved'", [], |r| r.get(0)).unwrap();
        assert_eq!(by, "desktop");
        conn.execute("UPDATE approvals SET status = 'approved', resolved_at = created_at, resolved_by = 'approver' WHERE id = 'pending'", []).unwrap();
    }

    #[test]
    fn approvals_migration_preserves_v7_data_and_is_repeatable() {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        for (idx, sql) in MIGRATIONS.iter().take(7).enumerate() {
            conn.execute_batch(sql).unwrap();
            conn.pragma_update(None, "user_version", (idx + 1) as i64).unwrap();
        }
        conn.execute("INSERT INTO chats(id, title, created_at) VALUES ('kept', 'kept title', '2026-01-01T00:00:00Z')", []).unwrap();
        apply(&conn).unwrap();
        conn.execute("INSERT INTO approvals(id, engine, category, reason, action_json, created_at) VALUES ('a', 'claude', 'Delete', 'test', '{}', '2026-01-01T00:00:00Z')", []).unwrap();
        apply(&conn).unwrap();
        let kept: String = conn.query_row("SELECT title FROM chats WHERE id = 'kept'", [], |r| r.get(0)).unwrap();
        assert_eq!(kept, "kept title");
        let approval: (Option<String>, String) = conn.query_row("SELECT chat_id, status FROM approvals WHERE id = 'a'", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!(approval, (None, "pending".into()));
        assert!(conn.execute("UPDATE approvals SET status = 'approved' WHERE id = 'a'", []).is_err());
    }

    #[test]
    fn path_key_migration_backfills_without_changing_existing_projects() {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        for (idx, sql) in MIGRATIONS.iter().take(4).enumerate() {
            conn.execute_batch(sql).unwrap();
            conn.pragma_update(None, "user_version", (idx + 1) as i64).unwrap();
        }
        for (id, path) in [("p1", r"C:\Work\Alpha\"), ("p2", "c:/work/alpha"), ("p3", "C:/ÖDEME/")] {
            conn.execute("INSERT INTO projects(id, name, path, aliases_json) VALUES (?1, ?1, ?2, '[\"kept\"]')", rusqlite::params![id, path]).unwrap();
        }
        conn.execute("INSERT INTO chats(id, project_id, created_at) VALUES ('chat', 'p2', '2026-01-01T00:00:00Z')", []).unwrap();
        conn.execute("INSERT INTO import_state(path, byte_offset, updated_at) VALUES ('source', 42, '2026-01-01T00:00:00Z')", []).unwrap();
        apply(&conn).unwrap();
        apply(&conn).unwrap();
        let keys: Vec<String> = conn.prepare("SELECT path_key FROM projects ORDER BY id").unwrap().query_map([], |r| r.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
        assert_eq!(keys, ["c:/work/alpha", "c:/work/alpha", "c:/ödeme"]);
        let row: (String, String, String) = conn.query_row("SELECT path, aliases_json, project_id FROM projects JOIN chats ON projects.id = chats.project_id", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
        assert_eq!(row, ("c:/work/alpha".into(), "[\"kept\"]".into(), "p2".into()));
        let state: (i64, i64, i64) = conn.query_row("SELECT byte_offset, file_size, generation FROM import_state", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
        assert_eq!(state, (42, 42, 0));
    }

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
        assert_eq!(v, MIGRATIONS.len() as i64);
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
