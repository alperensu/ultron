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
    // v10: proje başına "Şimdiye kadar yapılanlar" geçmiş notu için yeni tablo.
    r#"
    CREATE TABLE project_notes (
        project_id TEXT PRIMARY KEY REFERENCES projects(id),
        markdown TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        source_stamp INTEGER NOT NULL,
        claude_sessions INTEGER NOT NULL,
        codex_sessions INTEGER NOT NULL
    );
    "#,
    // v11: NOT ekleyici. Önce (Rust tarafında, `dedupe_projects`) aynı `path_key`li yinelenen
    // proje satırları birleştirilir; sonra `path_key` üzerinde benzersiz indeks kurulur ve
    // notlara özetleyici türü sütunu eklenir.
    r#"
    ALTER TABLE project_notes ADD COLUMN summarizer TEXT NOT NULL DEFAULT '';
    CREATE UNIQUE INDEX projects_path_key_unique ON projects(path_key) WHERE path_key != '';
    "#,
    // v12: Telegram kabul günlüğü ve nesil kontrollü kota beklemeleri (yalnız ekleyici).
    r#"
    ALTER TABLE approvals ADD COLUMN kind TEXT NOT NULL DEFAULT 'unknown'
        CHECK(kind IN ('unknown', 'policy_tool', 'turn_elevation'));
    CREATE TABLE telegram_bindings (
        user_id INTEGER PRIMARY KEY CHECK(user_id > 0),
        chat_id TEXT REFERENCES chats(id) ON DELETE SET NULL,
        updated_at TEXT NOT NULL
    );
    CREATE TABLE telegram_poll_state (
        bot_id INTEGER PRIMARY KEY CHECK(bot_id > 0),
        next_update_id INTEGER NOT NULL CHECK(next_update_id >= 0)
    );
    CREATE TABLE telegram_turn_origins (
        logical_turn_id TEXT PRIMARY KEY CHECK(length(logical_turn_id) BETWEEN 1 AND 128),
        chat_id TEXT NOT NULL REFERENCES chats(id),
        user_id INTEGER NOT NULL CHECK(user_id > 0),
        private_chat_id INTEGER NOT NULL CHECK(private_chat_id = user_id),
        user_event_seq INTEGER NOT NULL CHECK(user_event_seq > 0),
        start_seq INTEGER NOT NULL CHECK(start_seq >= user_event_seq),
        end_seq INTEGER CHECK(end_seq >= start_seq),
        delivery_state TEXT NOT NULL DEFAULT 'pending'
            CHECK(delivery_state IN ('pending', 'ready', 'delivered', 'needs_attention')),
        UNIQUE(chat_id, user_event_seq)
    );
    CREATE TABLE telegram_inbox (
        bot_id INTEGER NOT NULL CHECK(bot_id > 0),
        update_id INTEGER NOT NULL CHECK(update_id >= 0),
        user_id INTEGER NOT NULL CHECK(user_id > 0),
        state TEXT NOT NULL CHECK(state IN ('accepted', 'running', 'completed', 'needs_attention')),
        logical_turn_id TEXT UNIQUE REFERENCES telegram_turn_origins(logical_turn_id),
        PRIMARY KEY(bot_id, update_id)
    );
    CREATE TABLE quota_waits (
        id TEXT PRIMARY KEY CHECK(length(id) BETWEEN 1 AND 128),
        logical_turn_id TEXT NOT NULL UNIQUE CHECK(length(logical_turn_id) BETWEEN 1 AND 128),
        chat_id TEXT NOT NULL REFERENCES chats(id),
        source_session_id TEXT NOT NULL REFERENCES engine_sessions(id),
        engine TEXT NOT NULL CHECK(engine IN ('claude', 'codex', 'agy')),
        user_event_seq INTEGER NOT NULL CHECK(user_event_seq > 0),
        checkpoint_seq INTEGER NOT NULL CHECK(checkpoint_seq >= user_event_seq),
        cwd TEXT NOT NULL CHECK(length(cwd) BETWEEN 1 AND 32768),
        policy TEXT NOT NULL CHECK(policy IN ('read_only', 'workspace', 'requires_local_approval')),
        reset_at TEXT,
        state TEXT NOT NULL CHECK(state IN ('waiting', 'claimed', 'running', 'completed', 'cancelled', 'needs_attention')),
        generation INTEGER NOT NULL DEFAULT 0 CHECK(generation >= 0),
        next_engine_index INTEGER NOT NULL DEFAULT 0 CHECK(next_engine_index >= 0),
        attempt_count INTEGER NOT NULL DEFAULT 0 CHECK(attempt_count >= 0),
        next_attempt_at TEXT,
        updated_at TEXT NOT NULL
    );
    CREATE UNIQUE INDEX quota_waits_one_active_chat ON quota_waits(chat_id)
        WHERE state IN ('waiting', 'claimed', 'running', 'needs_attention');
    CREATE INDEX quota_waits_pending ON quota_waits(state, next_attempt_at);
    "#,
    // v13: bekleme başına sabit fallback zinciri ve reset toleransı (yalnız ekleyici).
    r#"
    ALTER TABLE quota_waits ADD COLUMN scheduler_json TEXT NOT NULL DEFAULT '{}';
    "#,
    // v14: Aşama 5a hafıza indeksi/kuyrukları (yalnız ekleyici). Kabul edilmiş hafızanın
    // doğruluk kaynağı Markdown kasasıdır; memory_items/memory_fts yeniden üretilebilir önbellektir.
    // Öneri, iş ve işlem tabloları kalıcı durumdur; yeniden indeksleme bunları silmez.
    // Hash kısıtları GLOB/substr ile büyük/küçük harf duyarlıdır (LIKE ASCII'de duyarsızdır).
    r#"
    CREATE TABLE memory_items (
        id TEXT PRIMARY KEY NOT NULL,
        relative_path TEXT NOT NULL UNIQUE COLLATE NOCASE CHECK(length(relative_path) BETWEEN 1 AND 260),
        memory_type TEXT NOT NULL
            CHECK(memory_type IN ('user', 'project', 'home', 'tool', 'person', 'rule', 'decision')),
        project TEXT,
        visibility TEXT NOT NULL DEFAULT 'private'
            CHECK(visibility IN ('private', 'project', 'shared')),
        remote_allowed INTEGER NOT NULL DEFAULT 0 CHECK(remote_allowed IN (0, 1)),
        sensitivity TEXT NOT NULL DEFAULT 'personal'
            CHECK(sensitivity IN ('normal', 'personal', 'finance')),
        title TEXT NOT NULL,
        summary TEXT NOT NULL DEFAULT '',
        body TEXT NOT NULL DEFAULT '',
        tags_json TEXT NOT NULL DEFAULT '[]',
        revision INTEGER NOT NULL CHECK(revision >= 1),
        content_hash TEXT NOT NULL CHECK(length(content_hash) = 71 AND substr(content_hash, 1, 7) = 'sha256:' AND NOT (substr(content_hash, 8) GLOB '*[^0-9a-f]*')),
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        review_at TEXT,
        index_state TEXT NOT NULL DEFAULT 'indexed'
            CHECK(index_state IN ('indexed', 'pending', 'stale', 'conflict', 'quarantined', 'inactive')),
        trashed_at TEXT,
        CHECK(NOT (visibility = 'private' AND remote_allowed = 1))
    );
    CREATE INDEX memory_items_project ON memory_items(project, updated_at);
    CREATE INDEX memory_items_state ON memory_items(index_state, trashed_at);

    CREATE TABLE memory_receipts (
        hash TEXT PRIMARY KEY NOT NULL CHECK(length(hash) = 71 AND substr(hash, 1, 7) = 'sha256:' AND NOT (substr(hash, 8) GLOB '*[^0-9a-f]*')),
        relative_path TEXT NOT NULL CHECK(length(relative_path) BETWEEN 1 AND 260),
        source_kind TEXT NOT NULL CHECK(source_kind IN ('event', 'file', 'import')),
        source_locator TEXT,
        chat_id TEXT,
        seq_from INTEGER,
        seq_to INTEGER,
        claim_status TEXT NOT NULL CHECK(claim_status IN ('claimed', 'verified', 'user_authored')),
        excerpt TEXT NOT NULL DEFAULT '',
        evidence_state TEXT NOT NULL DEFAULT 'present'
            CHECK(evidence_state IN ('present', 'missing', 'mismatch')),
        captured_at TEXT NOT NULL,
        CHECK(source_kind != 'event'
            OR (chat_id IS NOT NULL AND seq_from IS NOT NULL AND seq_to IS NOT NULL
                AND seq_from >= 1 AND seq_to >= seq_from)),
        CHECK(source_kind = 'event' OR source_locator IS NOT NULL)
    );

    CREATE TABLE memory_item_receipts (
        item_id TEXT NOT NULL REFERENCES memory_items(id) ON DELETE CASCADE,
        revision INTEGER NOT NULL CHECK(revision >= 1),
        receipt_hash TEXT NOT NULL REFERENCES memory_receipts(hash),
        UNIQUE(item_id, revision, receipt_hash)
    );
    CREATE INDEX memory_item_receipts_hash ON memory_item_receipts(receipt_hash);

    CREATE TABLE memory_suggestions (
        id TEXT PRIMARY KEY NOT NULL,
        chat_id TEXT NOT NULL,
        logical_turn_id TEXT NOT NULL,
        end_seq INTEGER NOT NULL CHECK(end_seq >= 1),
        candidate_json TEXT NOT NULL,
        candidate_hash TEXT NOT NULL CHECK(length(candidate_hash) = 71 AND substr(candidate_hash, 1, 7) = 'sha256:' AND NOT (substr(candidate_hash, 8) GLOB '*[^0-9a-f]*')),
        target_id TEXT,
        target_revision INTEGER CHECK(target_revision IS NULL OR target_revision >= 1),
        receipts_json TEXT NOT NULL DEFAULT '[]',
        dedupe_key TEXT NOT NULL,
        status TEXT NOT NULL DEFAULT 'pending'
            CHECK(status IN ('pending', 'accepted', 'rejected', 'conflict')),
        created_at TEXT NOT NULL,
        decided_at TEXT,
        decided_by TEXT CHECK(decided_by IN ('desktop', 'telegram', 'recovery')),
        decision_request_id TEXT,
        decision_request_hash TEXT CHECK(decision_request_hash IS NULL OR (length(decision_request_hash) = 71 AND substr(decision_request_hash, 1, 7) = 'sha256:' AND NOT (substr(decision_request_hash, 8) GLOB '*[^0-9a-f]*'))),
        accepted_item_id TEXT,
        UNIQUE(chat_id, logical_turn_id, candidate_hash),
        CHECK((target_id IS NULL) = (target_revision IS NULL)),
        CHECK((decision_request_id IS NULL) = (decision_request_hash IS NULL)),
        CHECK((status = 'pending' AND decided_at IS NULL AND decided_by IS NULL)
           OR (status != 'pending' AND decided_at IS NOT NULL AND decided_by IS NOT NULL))
    );
    CREATE INDEX memory_suggestions_status ON memory_suggestions(status, created_at);
    CREATE INDEX memory_suggestions_dedupe ON memory_suggestions(dedupe_key);

    CREATE TABLE memory_jobs (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        chat_id TEXT NOT NULL,
        logical_turn_id TEXT NOT NULL,
        end_seq INTEGER NOT NULL CHECK(end_seq >= 1),
        status TEXT NOT NULL DEFAULT 'queued'
            CHECK(status IN ('queued', 'running', 'done', 'failed', 'cancelled')),
        attempt INTEGER NOT NULL DEFAULT 0 CHECK(attempt >= 0),
        lease_until TEXT,
        generation INTEGER NOT NULL DEFAULT 0 CHECK(generation >= 0),
        error_code TEXT CHECK(error_code IS NULL OR error_code IN
            ('timeout', 'engine_unavailable', 'invalid_output', 'store_unavailable', 'cancelled', 'quota')),
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        UNIQUE(chat_id, logical_turn_id, end_seq)
    );
    CREATE INDEX memory_jobs_status ON memory_jobs(status, lease_until);

    CREATE TABLE memory_operations (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        request_id TEXT NOT NULL UNIQUE,
        request_hash TEXT NOT NULL CHECK(length(request_hash) = 71 AND substr(request_hash, 1, 7) = 'sha256:' AND NOT (substr(request_hash, 8) GLOB '*[^0-9a-f]*')),
        source TEXT NOT NULL CHECK(source IN ('desktop', 'telegram', 'engine', 'system')),
        kind TEXT NOT NULL
            CHECK(kind IN ('capture', 'update', 'delete', 'accept_suggestion', 'reconcile', 'restore')),
        target_id TEXT,
        expected_revision INTEGER CHECK(expected_revision IS NULL OR expected_revision >= 1),
        state TEXT NOT NULL DEFAULT 'pending'
            CHECK(state IN ('pending', 'file_committed', 'completed', 'failed')),
        result_revision INTEGER CHECK(result_revision IS NULL OR result_revision >= 1),
        content_hash TEXT CHECK(content_hash IS NULL OR (length(content_hash) = 71 AND substr(content_hash, 1, 7) = 'sha256:' AND NOT (substr(content_hash, 8) GLOB '*[^0-9a-f]*'))),
        receipt_hash TEXT CHECK(receipt_hash IS NULL OR (length(receipt_hash) = 71 AND substr(receipt_hash, 1, 7) = 'sha256:' AND NOT (substr(receipt_hash, 8) GLOB '*[^0-9a-f]*'))),
        error_code TEXT CHECK(error_code IS NULL
            OR (length(error_code) BETWEEN 1 AND 48 AND error_code NOT GLOB '*[^a-z0-9_]*')),
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL
    );
    CREATE INDEX memory_operations_state ON memory_operations(state, updated_at);

    CREATE TABLE memory_changes (
        cursor INTEGER PRIMARY KEY AUTOINCREMENT,
        item_id TEXT,
        suggestion_id TEXT,
        revision INTEGER CHECK(revision IS NULL OR revision >= 1),
        kind TEXT NOT NULL CHECK(kind IN ('created', 'updated', 'trashed', 'restored',
            'external_change', 'deactivated', 'suggestion_created', 'suggestion_resolved',
            'index_rebuilt')),
        created_at TEXT NOT NULL
    );

    CREATE VIRTUAL TABLE memory_fts USING fts5(
        item_id UNINDEXED,
        title,
        summary,
        body,
        tags,
        tokenize = 'unicode61 remove_diacritics 2'
    );

    CREATE TABLE approval_shadow_scores (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        approval_id TEXT NOT NULL UNIQUE REFERENCES approvals(id) ON DELETE CASCADE,
        model TEXT NOT NULL CHECK(length(model) BETWEEN 1 AND 128),
        prompt_version TEXT NOT NULL CHECK(length(prompt_version) BETWEEN 1 AND 64),
        p_safe REAL NOT NULL CHECK(p_safe >= 0.0 AND p_safe <= 1.0),
        measured_at TEXT NOT NULL,
        duration_ms INTEGER NOT NULL CHECK(duration_ms >= 0),
        action_fingerprint TEXT NOT NULL CHECK(length(action_fingerprint) = 71 AND substr(action_fingerprint, 1, 7) = 'sha256:' AND NOT (substr(action_fingerprint, 8) GLOB '*[^0-9a-f]*'))
    );
    "#,
];

type DupRow = (String, String, Option<String>, Option<String>, Option<String>, i64);

/// Aynı `path_key`li projeleri tek satırda toplar. Tutulan satır deterministiktir: en çok
/// sohbeti olan, sonra en yeni `last_active_at`, sonra en küçük kimlik. Üst veri birleştirilir
/// (boş ad/açıklama doldurulur, takma adların birleşimi, en yeni etkinlik); sohbetler taşınır;
/// tutulan satırın notu yoksa yinelenenin notu ona taşınır, aksi halde silinir.
fn dedupe_projects(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    use std::collections::BTreeMap;
    let rows: Vec<(String, DupRow)> = {
        let mut stmt = tx.prepare(
            "SELECT p.path_key, p.id, p.name, p.aliases_json, p.description, p.last_active_at,
                    (SELECT COUNT(*) FROM chats c WHERE c.project_id = p.id)
             FROM projects p WHERE p.path_key != ''",
        )?;
        stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                (r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?),
            ))
        })?
        .collect::<rusqlite::Result<_>>()?
    };
    let mut groups: BTreeMap<String, Vec<DupRow>> = BTreeMap::new();
    for (key, row) in rows {
        groups.entry(key).or_default().push(row);
    }
    for (_, mut group) in groups {
        if group.len() < 2 {
            continue;
        }
        // (kimlik, ad, takma adlar JSON, açıklama, son etkinlik, sohbet sayısı)
        group.sort_by(|a, b| {
            b.5.cmp(&a.5)
                .then_with(|| b.4.cmp(&a.4)) // Some > None, daha yeni önce
                .then_with(|| a.0.cmp(&b.0))
        });
        let (keeper, dups) = group.split_first().expect("grup en az iki satırlı");
        let mut name = keeper.1.clone();
        let mut description = keeper.3.clone().filter(|d| !d.trim().is_empty());
        let mut last = keeper.4.clone();
        let mut aliases: Vec<String> =
            serde_json::from_str(keeper.2.as_deref().unwrap_or("[]")).unwrap_or_default();
        for d in dups {
            if name.trim().is_empty() {
                name = d.1.clone();
            }
            if description.is_none() {
                description = d.3.clone().filter(|x| !x.trim().is_empty());
            }
            last = last.max(d.4.clone());
            let extra: Vec<String> =
                serde_json::from_str(d.2.as_deref().unwrap_or("[]")).unwrap_or_default();
            for a in extra {
                if !aliases.contains(&a) {
                    aliases.push(a);
                }
            }
        }
        tx.execute(
            "UPDATE projects SET name = ?2, description = ?3, last_active_at = ?4, aliases_json = ?5
             WHERE id = ?1",
            rusqlite::params![keeper.0, name, description, last, serde_json::to_string(&aliases)?],
        )?;
        for d in dups {
            tx.execute(
                "UPDATE chats SET project_id = ?2 WHERE project_id = ?1",
                rusqlite::params![d.0, keeper.0],
            )?;
            let keeper_has_note: i64 = tx.query_row(
                "SELECT COUNT(*) FROM project_notes WHERE project_id = ?1",
                [&keeper.0],
                |r| r.get(0),
            )?;
            if keeper_has_note == 0 {
                tx.execute(
                    "UPDATE project_notes SET project_id = ?2 WHERE project_id = ?1",
                    rusqlite::params![d.0, keeper.0],
                )?;
            } else {
                tx.execute("DELETE FROM project_notes WHERE project_id = ?1", [&d.0])?;
            }
            tx.execute("DELETE FROM projects WHERE id = ?1", [&d.0])?;
        }
    }
    Ok(())
}

/// Bekleyen migrasyonları `user_version` üzerinden sırayla uygular.
pub fn apply(conn: &Connection) -> Result<()> {
    let current: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    for (idx, sql) in MIGRATIONS.iter().enumerate() {
        let version = (idx + 1) as i64;
        if version <= current {
            continue;
        }
        let tx = conn.unchecked_transaction()?;
        if version == 11 {
            dedupe_projects(&tx)?;
        }
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
    fn approval_kind_migration_defaults_to_desktop_only() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("legacy.db");
        let conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        for (idx, sql) in MIGRATIONS.iter().take(11).enumerate() {
            if idx == 10 {
                let tx = conn.unchecked_transaction().unwrap();
                dedupe_projects(&tx).unwrap();
                tx.commit().unwrap();
            }
            conn.execute_batch(sql).unwrap();
            conn.pragma_update(None, "user_version", (idx + 1) as i64)
                .unwrap();
        }
        conn.execute("INSERT INTO chats(id, title, created_at) VALUES ('kept', 'Türkçe', '2026-01-01T00:00:00Z')", []).unwrap();
        conn.execute(
            "INSERT INTO events(id, chat_id, seq, ts, kind, payload_json)
            VALUES ('event', 'kept', 1, '2026-01-01T00:00:00Z', 'user_text',
            '{\"type\":\"user_text\",\"text\":\"[MASKED]\"}')",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO approvals(id, chat_id, engine, category, reason, action_json, created_at)
            VALUES ('legacy', 'kept', 'claude', '\"SystemConfig\"', 'sentetik',
            '{\"ToolCall\":{\"tool\":\"ultron.turn.elevate\",\"input\":{}}}', '2026-01-01T00:00:00Z')", []).unwrap();
        let before: String = conn
            .query_row("SELECT payload_json FROM events", [], |r| r.get(0))
            .unwrap();
        apply(&conn).unwrap();
        apply(&conn).unwrap();
        assert_eq!(
            conn.pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))
                .unwrap(),
            MIGRATIONS.len() as i64
        );
        assert_eq!(
            conn.query_row::<String, _, _>("SELECT payload_json FROM events", [], |r| r.get(0))
                .unwrap(),
            before
        );
        assert_eq!(
            conn.query_row::<String, _, _>("SELECT title FROM chats", [], |r| r.get(0))
                .unwrap(),
            "Türkçe"
        );
        drop(conn);
        let db = super::super::Db::open(&path).unwrap();
        let approval = db.get_approval("legacy").unwrap().unwrap();
        assert_eq!(
            approval.kind,
            super::super::approvals::ApprovalKind::Unknown
        );
        assert!(!approval.is_policy_tool());
        assert_eq!(
            approval.status,
            super::super::approvals::ApprovalStatus::Pending
        );
    }

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
    fn v11_merges_duplicate_projects_deterministically_and_adds_unique_index() {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        for (idx, sql) in MIGRATIONS.iter().take(10).enumerate() {
            conn.execute_batch(sql).unwrap();
            conn.pragma_update(None, "user_version", (idx + 1) as i64).unwrap();
        }
        // Grup 1: `b` en çok sohbetli (tutulur); `a` daha küçük kimlikli ama sohbetsiz.
        conn.execute("INSERT INTO projects(id, name, path, path_key, aliases_json, description, last_active_at) VALUES ('a', 'ad-a', 'C:/x', 'c:/x', '[\"x\"]', 'aciklama-a', '2026-03-01T00:00:00+00:00')", []).unwrap();
        conn.execute("INSERT INTO projects(id, name, path, path_key, aliases_json, description, last_active_at) VALUES ('b', 'ad-b', 'C:/x', 'c:/x', '[\"y\"]', NULL, '2026-02-01T00:00:00+00:00')", []).unwrap();
        conn.execute("INSERT INTO chats(id, project_id, title, created_at) VALUES ('k', 'b', 't', '2026-01-01T00:00:00Z')", []).unwrap();
        // Yalnız yinelenen `a`nın notu var: tutulan `b`ye taşınmalı.
        conn.execute("INSERT INTO project_notes(project_id, markdown, updated_at, source_stamp, claude_sessions, codex_sessions) VALUES ('a', 'not-a', 'x', 1, 1, 0)", []).unwrap();
        // Grup 2: sohbet yok, etkinlik eşit değil -> en yeni etkinlik (`n2`) tutulur; ikisinin de notu var.
        conn.execute("INSERT INTO projects(id, name, path, path_key, last_active_at) VALUES ('n1', 'n1', 'C:/z', 'c:/z', '2026-01-01T00:00:00+00:00')", []).unwrap();
        conn.execute("INSERT INTO projects(id, name, path, path_key, last_active_at) VALUES ('n2', 'n2', 'C:/z', 'c:/z', '2026-05-01T00:00:00+00:00')", []).unwrap();
        for id in ["n1", "n2"] {
            conn.execute("INSERT INTO project_notes(project_id, markdown, updated_at, source_stamp, claude_sessions, codex_sessions) VALUES (?1, ?1, 'x', 1, 1, 0)", [id]).unwrap();
        }
        conn.execute("INSERT INTO projects(id, name, path, path_key) VALUES ('c', 'q', 'C:/y', 'c:/y')", []).unwrap();
        apply(&conn).unwrap();
        apply(&conn).unwrap();
        let ids: Vec<String> = conn
            .prepare("SELECT id FROM projects ORDER BY id").unwrap()
            .query_map([], |r| r.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
        assert_eq!(ids, ["b", "c", "n2"]);
        let owner: String = conn.query_row("SELECT project_id FROM chats WHERE id = 'k'", [], |r| r.get(0)).unwrap();
        assert_eq!(owner, "b");
        // Üst veri birleşti: açıklama dolduruldu, takma adlar birleşti, en yeni etkinlik alındı.
        let meta: (String, Option<String>, String, Option<String>) = conn.query_row(
            "SELECT name, description, aliases_json, last_active_at FROM projects WHERE id = 'b'", [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).unwrap();
        assert_eq!(meta.0, "ad-b");
        assert_eq!(meta.1.as_deref(), Some("aciklama-a"));
        assert_eq!(meta.2, "[\"y\",\"x\"]");
        assert_eq!(meta.3.as_deref(), Some("2026-03-01T00:00:00+00:00"));
        // Notlar: `a`nın notu `b`ye taşındı; `n1`in notu silindi, `n2`ninki kaldı.
        let notes: Vec<(String, String)> = conn
            .prepare("SELECT project_id, markdown FROM project_notes ORDER BY project_id").unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().collect::<rusqlite::Result<_>>().unwrap();
        assert_eq!(notes, [("b".into(), "not-a".into()), ("n2".into(), "n2".into())]);
        assert!(conn.execute("INSERT INTO projects(id, name, path, path_key) VALUES ('z', 'p', 'C:/X', 'c:/x')", []).is_err());
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
        // v11 yinelenen satırı en çok sohbetli olanda (p2) birleştirir.
        assert_eq!(keys, ["c:/work/alpha", "c:/ödeme"]);
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
