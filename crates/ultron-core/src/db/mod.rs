mod migrations;
pub mod approvals;

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::EngineId;
use crate::ipc::{ChatInfo, ProjectInfo};

/// Otomatik sohbet başlığı üst sınırı (karakter).
pub const TITLE_MAX_CHARS: usize = 60;

/// Devir paketi ve çalışma durumu türetimi için bir sohbetten okunan en fazla olay sayısı.
/// Daha eski geçmiş yalnız tam metin aramasıyla (`ultron history search`) erişilebilir.
pub const RECENT_EVENTS_WINDOW: usize = 2000;

/// Motor oturumunun durumu.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Running,
    WaitingQuota,
    Closed,
    Failed,
    Interrupted,
}

impl SessionStatus {
    fn as_str(self) -> &'static str {
        match self {
            SessionStatus::Running => "running",
            SessionStatus::WaitingQuota => "waiting_quota",
            SessionStatus::Closed => "closed",
            SessionStatus::Failed => "failed",
            SessionStatus::Interrupted => "interrupted",
        }
    }

    fn parse(s: &str) -> Result<SessionStatus> {
        Ok(match s {
            "running" => SessionStatus::Running,
            "waiting_quota" => SessionStatus::WaitingQuota,
            "closed" => SessionStatus::Closed,
            "failed" => SessionStatus::Failed,
            "interrupted" => SessionStatus::Interrupted,
            other => return Err(anyhow!("bilinmeyen oturum durumu: {other}")),
        })
    }
}

/// `engine_sessions` satırı.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRow {
    pub id: String,
    pub chat_id: String,
    pub engine: EngineId,
    pub native_session_id: Option<String>,
    pub status: SessionStatus,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
}

/// Kalıcı olay kaydı.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredEvent {
    pub id: String,
    pub chat_id: String,
    pub session_id: Option<String>,
    pub seq: i64,
    pub ts: DateTime<Utc>,
    pub kind: String,
    pub payload: Value,
}

/// Tam metin arama sonucu.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchHit {
    pub chat_id: String,
    pub event_id: String,
    pub text: String,
}

/// SQLite deposu. Bağlantı tek bir kilit arkasındadır.
pub struct Db {
    conn: Mutex<Connection>,
}

type RawEvent = (String, String, Option<String>, i64, String, String, String);

#[derive(Debug, Clone, Default)]
pub(crate) struct ImportCheckpoint {
    pub offset: u64,
    pub generation: i64,
    pub file_size: u64,
    pub first_line_hash: Option<String>,
    pub prefix_hash: Option<String>,
    pub session_cwd: Option<String>,
}

pub(crate) fn normalized_project_path(path: &str) -> String {
    path.replace('\\', "/").trim_end_matches('/').to_lowercase()
}

impl Db {
    /// Veritabanını açar (WAL, yabancı anahtarlar) ve migrasyonları uygular.
    pub fn open(path: &Path) -> Result<Db> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("veritabanı açılamadı: {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        migrations::apply(&conn)?;
        Ok(Db {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> Result<MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|_| anyhow!("veritabanı kilidi zehirlendi"))
    }

    /// Yeni bir sohbet satırı ekler (varsa dokunmaz).
    pub fn insert_chat(
        &self,
        id: &str,
        project_id: Option<&str>,
        title: &str,
        created_at: DateTime<Utc>,
    ) -> Result<()> {
        let conn = self.conn()?;
        conn.execute(
            "INSERT OR IGNORE INTO chats(id, project_id, title, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![id, project_id, title, created_at.to_rfc3339()],
        )?;
        Ok(())
    }

    /// Projeyi ekler veya günceller (kimliğe göre).
    pub fn upsert_project(&self, p: &ProjectInfo) -> Result<()> {
        let conn = self.conn()?;
        conn.execute(
            "INSERT INTO projects(id, name, path, aliases_json, description, last_active_at, path_key)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET
                name = excluded.name, path = excluded.path, path_key = excluded.path_key,
                aliases_json = excluded.aliases_json, description = excluded.description,
                last_active_at = COALESCE(excluded.last_active_at, projects.last_active_at)",
            params![
                p.id,
                p.name,
                p.path,
                serde_json::to_string(&p.aliases)?,
                p.description,
                p.last_active_at,
                normalized_project_path(&p.path),
            ],
        )?;
        Ok(())
    }

    fn project_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<(ProjectInfo, String)> {
        Ok((
            ProjectInfo {
                id: r.get(0)?,
                name: r.get(1)?,
                path: r.get(2)?,
                aliases: Vec::new(),
                description: r.get(4)?,
                last_active_at: r.get(5)?,
            },
            r.get(3)?,
        ))
    }

    /// Kataloğu tek işlemde uygular; mevcut projelerin diğer alanlarına dokunmaz.
    pub fn upsert_catalog_projects(
        &self,
        seeds: &[crate::projects::ProjectSeed],
    ) -> Result<crate::ipc::CatalogImportReport> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut report = crate::ipc::CatalogImportReport::default();
        for seed in seeds {
            let existing: Option<String> = tx
                .query_row(
                    "SELECT id FROM projects WHERE path_key = ?1 ORDER BY id LIMIT 1",
                    params![normalized_project_path(&seed.path)],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(id) = existing {
                tx.execute(
                    "UPDATE projects SET name = ?1, description = ?2 WHERE id = ?3",
                    params![seed.name, seed.description, id],
                )?;
                report.updated += 1;
            } else {
                tx.execute(
                    "INSERT INTO projects(id, name, path, description, path_key) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        uuid::Uuid::new_v4().simple().to_string(),
                        seed.name,
                        seed.path,
                        seed.description,
                        normalized_project_path(&seed.path),
                    ],
                )?;
                report.imported += 1;
            }
        }
        tx.commit()?;
        Ok(report)
    }

    /// Sohbetlerden bağımsız, proje tablosundaki son etkin proje kimliği.
    pub fn last_active_project(&self) -> Result<Option<String>> {
        let conn = self.conn()?;
        Ok(conn
            .query_row(
                "SELECT id FROM projects WHERE julianday(last_active_at) IS NOT NULL
             ORDER BY julianday(last_active_at) DESC, id ASC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Tek projeyi okur.
    pub fn get_project(&self, id: &str) -> Result<Option<ProjectInfo>> {
        let conn = self.conn()?;
        let row = conn
            .query_row(
                "SELECT id, name, path, aliases_json, description, last_active_at
                 FROM projects WHERE id = ?1",
                params![id],
                Self::project_row,
            )
            .optional()?;
        row.map(|(mut p, aliases)| {
            p.aliases = serde_json::from_str(&aliases)?;
            Ok(p)
        })
        .transpose()
    }

    /// Tüm projeler: son etkinliğe göre yeniden eskiye, sonra ada göre.
    pub fn list_projects(&self) -> Result<Vec<ProjectInfo>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, name, path, aliases_json, description, last_active_at
             FROM projects ORDER BY last_active_at IS NULL, last_active_at DESC, name ASC",
        )?;
        let rows = stmt.query_map([], Self::project_row)?;
        let mut out = Vec::new();
        for row in rows {
            let (mut p, aliases) = row?;
            p.aliases = serde_json::from_str(&aliases)?;
            out.push(p);
        }
        Ok(out)
    }

    fn chat_info_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<(ChatInfo, Option<String>)> {
        Ok((
            ChatInfo {
                id: r.get(0)?,
                title: r.get(1)?,
                project_id: r.get(2)?,
                pinned: r.get::<_, i64>(3)? != 0,
                archived: r.get::<_, i64>(4)? != 0,
                updated_at: r.get(5)?,
                engine: None,
            },
            r.get(6)?,
        ))
    }

    fn finish_chat_info((mut c, engine): (ChatInfo, Option<String>)) -> Result<ChatInfo> {
        c.engine = engine.as_deref().map(engine_parse).transpose()?;
        Ok(c)
    }

    const CHAT_COLS: &'static str = "id, title, project_id, pinned, archived,
         COALESCE(updated_at, created_at), current_engine";

    /// Sohbetler: önce sabitlenenler, sonra en son güncellenenler.
    pub fn list_chats(
        &self,
        project_id: Option<&str>,
        include_archived: bool,
    ) -> Result<Vec<ChatInfo>> {
        let conn = self.conn()?;
        let sql = format!(
            "SELECT {} FROM chats
             WHERE (?1 IS NULL OR project_id = ?1) AND (?2 = 1 OR archived = 0)
             ORDER BY pinned DESC, COALESCE(updated_at, created_at) DESC, rowid DESC",
            Self::CHAT_COLS
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(
            params![project_id, i64::from(include_archived)],
            Self::chat_info_row,
        )?;
        let mut out = Vec::new();
        for row in rows {
            out.push(Self::finish_chat_info(row?)?);
        }
        Ok(out)
    }

    /// Tek sohbetin meta verisi.
    pub fn get_chat(&self, chat_id: &str) -> Result<Option<ChatInfo>> {
        let conn = self.conn()?;
        let sql = format!("SELECT {} FROM chats WHERE id = ?1", Self::CHAT_COLS);
        let row = conn
            .query_row(&sql, params![chat_id], Self::chat_info_row)
            .optional()?;
        row.map(Self::finish_chat_info).transpose()
    }

    /// Sohbet alanlarını günceller; verilen başlık kullanıcı başlığı olur (`title_source='user'`).
    /// Sohbet yoksa `false` döner.
    pub fn update_chat(
        &self,
        chat_id: &str,
        title: Option<&str>,
        pinned: Option<bool>,
        archived: Option<bool>,
        project_id: Option<Option<&str>>,
    ) -> Result<bool> {
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM chats WHERE id = ?1)",
            params![chat_id],
            |r| r.get(0),
        )?;
        if !exists {
            return Ok(false);
        }
        if let Some(title) = title {
            tx.execute(
                "UPDATE chats SET title = ?2, title_source = 'user' WHERE id = ?1",
                params![chat_id, title],
            )?;
        }
        if let Some(pinned) = pinned {
            tx.execute(
                "UPDATE chats SET pinned = ?2 WHERE id = ?1",
                params![chat_id, i64::from(pinned)],
            )?;
        }
        if let Some(archived) = archived {
            tx.execute(
                "UPDATE chats SET archived = ?2 WHERE id = ?1",
                params![chat_id, i64::from(archived)],
            )?;
        }
        if let Some(project_id) = project_id {
            tx.execute(
                "UPDATE chats SET project_id = ?2 WHERE id = ?1",
                params![chat_id, project_id],
            )?;
        }
        tx.commit()?;
        Ok(true)
    }

    /// Sohbeti projeye bağlar (yalnız henüz projesi yoksa) ve projenin son etkinliğini işler.
    pub fn bind_chat_project(&self, chat_id: &str, project_id: &str) -> Result<()> {
        let conn = self.conn()?;
        conn.execute(
            "UPDATE chats SET project_id = ?2 WHERE id = ?1 AND project_id IS NULL",
            params![chat_id, project_id],
        )?;
        conn.execute(
            "UPDATE projects SET last_active_at = ?2 WHERE id = ?1",
            params![project_id, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    /// İçe aktarma ofsetlerini (dosya yolu -> bayt) okur.
    pub fn import_offsets(&self) -> Result<std::collections::HashMap<String, u64>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare("SELECT path, byte_offset FROM import_state")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        let mut out = std::collections::HashMap::new();
        for row in rows {
            let (p, o) = row?;
            out.insert(p, u64::try_from(o).unwrap_or(0));
        }
        Ok(out)
    }

    /// Bir dosyanın işlenmiş bayt ofsetini yazar.
    pub fn set_import_offset(&self, path: &str, offset: u64) -> Result<()> {
        let conn = self.conn()?;
        conn.execute(
            "INSERT INTO import_state(path, byte_offset, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(path) DO UPDATE SET
                byte_offset = excluded.byte_offset, updated_at = excluded.updated_at",
            params![path, i64::try_from(offset)?, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub(crate) fn import_checkpoints(
        &self,
    ) -> Result<std::collections::HashMap<String, ImportCheckpoint>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT path, byte_offset, generation, file_size, first_line_hash, session_cwd, prefix_hash
             FROM import_state",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get(0)?, ImportCheckpoint {
                offset: r.get::<_, i64>(1)?.max(0) as u64,
                generation: r.get(2)?,
                file_size: r.get::<_, i64>(3)?.max(0) as u64,
                first_line_hash: r.get(4)?,
                session_cwd: r.get(5)?,
                prefix_hash: r.get(6)?,
            }))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Sıra tahsisi, olay/FTS yazımı ve kontrol noktası aynı yazma kilidindedir.
    /// Yalnız kaynak olay kimliği yinelenirse ekleme atlanır.
    pub(crate) fn commit_import_record(
        &self,
        path: &str,
        checkpoint: &ImportCheckpoint,
        event: Option<&StoredEvent>,
    ) -> Result<bool> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut inserted = false;
        if let Some(ev) = event {
            inserted = Self::append_event_tx(&tx, ev)?.is_some();
            if inserted {
                tx.execute("UPDATE chats SET updated_at = ?2 WHERE id = ?1",
                    params![ev.chat_id, ev.ts.to_rfc3339()])?;
            }
        }
        tx.execute(
            "INSERT INTO import_state(path, byte_offset, updated_at, generation, file_size, first_line_hash, session_cwd, prefix_hash)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(path) DO UPDATE SET byte_offset = excluded.byte_offset,
                updated_at = excluded.updated_at, generation = excluded.generation,
                file_size = excluded.file_size, first_line_hash = excluded.first_line_hash,
                session_cwd = excluded.session_cwd, prefix_hash = excluded.prefix_hash",
            params![path, i64::try_from(checkpoint.offset)?, Utc::now().to_rfc3339(),
                checkpoint.generation, i64::try_from(checkpoint.file_size)?,
                checkpoint.first_line_hash, checkpoint.session_cwd, checkpoint.prefix_hash],
        )?;
        tx.commit()?;
        Ok(inserted)
    }

    /// Sohbetin güncellenme zamanını verilen değere ayarlar (içe aktarma için).
    pub fn set_chat_updated_at(&self, chat_id: &str, at: DateTime<Utc>) -> Result<()> {
        let conn = self.conn()?;
        conn.execute(
            "UPDATE chats SET updated_at = ?2 WHERE id = ?1",
            params![chat_id, at.to_rfc3339()],
        )?;
        Ok(())
    }

    /// Sohbetin güncellenme zamanını yazar; başlık boş ve otomatikse ilk mesajdan başlık üretir.
    pub fn touch_chat(&self, chat_id: &str, first_message: &str) -> Result<()> {
        let conn = self.conn()?;
        conn.execute(
            "UPDATE chats SET updated_at = ?2 WHERE id = ?1",
            params![chat_id, Utc::now().to_rfc3339()],
        )?;
        let title = auto_title(first_message);
        if !title.is_empty() {
            conn.execute(
                "UPDATE chats SET title = ?2 WHERE id = ?1 AND title = '' AND title_source = 'auto'",
                params![chat_id, title],
            )?;
        }
        Ok(())
    }

    /// Olayı ekler. Aynı `id` zaten varsa `false` döner ve hiçbir şey değişmez.
    /// Mesaj olaylarının metni aynı işlemde tam metin dizinine yazılır.
    pub fn insert_event(&self, ev: &StoredEvent) -> Result<bool> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let inserted = Self::insert_event_tx(&tx, ev)?;
        tx.commit()?;
        Ok(inserted)
    }

    /// Sıra numarasını ve olayı aynı SQLite yazma işlemi içinde tahsis eder.
    pub fn append_event(&self, ev: &StoredEvent) -> Result<Option<StoredEvent>> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let stored = Self::append_event_tx(&tx, ev)?;
        tx.commit()?;
        Ok(stored)
    }

    fn append_event_tx(
        tx: &rusqlite::Transaction<'_>,
        ev: &StoredEvent,
    ) -> Result<Option<StoredEvent>> {
        let mut stored = ev.clone();
        stored.seq = tx.query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM events WHERE chat_id = ?1",
            params![ev.chat_id],
            |r| r.get(0),
        )?;
        Ok(Self::insert_event_tx(tx, &stored)?.then_some(stored))
    }

    fn insert_event_tx(tx: &rusqlite::Transaction<'_>, ev: &StoredEvent) -> Result<bool> {
        let inserted = tx.execute(
            "INSERT INTO events(id, chat_id, session_id, seq, ts, kind, payload_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) ON CONFLICT(id) DO NOTHING",
            params![
                ev.id,
                ev.chat_id,
                ev.session_id,
                ev.seq,
                ev.ts.to_rfc3339(),
                ev.kind,
                ev.payload.to_string(),
            ],
        )?;
        if inserted == 0 {
            return Ok(false);
        }
        if let Some(text) = indexable_text(&ev.kind, &ev.payload) {
            tx.execute(
                "INSERT INTO messages_fts(chat_id, event_id, text) VALUES (?1, ?2, ?3)",
                params![ev.chat_id, ev.id, text],
            )?;
        }
        Ok(true)
    }

    /// Bir sohbetin olaylarını `seq` sırasıyla döner. `after_seq` verilirse ondan sonrakileri döner.
    pub fn events_for_chat(
        &self,
        chat_id: &str,
        after_seq: Option<i64>,
    ) -> Result<Vec<StoredEvent>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, chat_id, session_id, seq, ts, kind, payload_json
             FROM events WHERE chat_id = ?1 AND seq > ?2 ORDER BY seq ASC",
        )?;
        let rows = stmt.query_map(params![chat_id, after_seq.unwrap_or(0)], raw_event_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(raw_to_event(row?)?);
        }
        Ok(out)
    }

    /// `events_for_chat`'in sayfalı hali: en fazla `limit` olay, `seq` sırasıyla.
    /// Sonraki sayfa için son dönen olayın `seq` değeri `after_seq` olarak verilir.
    pub fn events_for_chat_page(
        &self,
        chat_id: &str,
        after_seq: Option<i64>,
        limit: usize,
    ) -> Result<Vec<StoredEvent>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, chat_id, session_id, seq, ts, kind, payload_json
             FROM events WHERE chat_id = ?1 AND seq > ?2 ORDER BY seq ASC LIMIT ?3",
        )?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = stmt.query_map(
            params![chat_id, after_seq.unwrap_or(0), limit],
            raw_event_row,
        )?;
        let mut out = Vec::new();
        for row in rows {
            out.push(raw_to_event(row?)?);
        }
        Ok(out)
    }

    /// Sohbetin en yüksek `seq` değerine sahip olayı (yoksa `None`). Tüm geçmişi okumaz.
    pub fn last_event(&self, chat_id: &str) -> Result<Option<StoredEvent>> {
        let conn = self.conn()?;
        let raw = conn
            .query_row(
                "SELECT id, chat_id, session_id, seq, ts, kind, payload_json
                 FROM events WHERE chat_id = ?1 ORDER BY seq DESC LIMIT 1",
                params![chat_id],
                raw_event_row,
            )
            .optional()?;
        raw.map(raw_to_event).transpose()
    }

    /// Sohbetin en az bir olayı var mı?
    pub fn has_events(&self, chat_id: &str) -> Result<bool> {
        let conn = self.conn()?;
        let found: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM events WHERE chat_id = ?1)",
            params![chat_id],
            |r| r.get(0),
        )?;
        Ok(found)
    }

    /// Sohbetin son `limit` olayını `seq` sırasıyla (eskiden yeniye) döner.
    pub fn events_tail(&self, chat_id: &str, limit: usize) -> Result<Vec<StoredEvent>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, chat_id, session_id, seq, ts, kind, payload_json
             FROM events WHERE chat_id = ?1 ORDER BY seq DESC LIMIT ?2",
        )?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = stmt.query_map(params![chat_id, limit], raw_event_row)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(raw_to_event(row?)?);
        }
        out.reverse();
        Ok(out)
    }

    /// Sohbetin bir sonraki sıra numarası (ilk olay için 1).
    pub fn next_seq(&self, chat_id: &str) -> Result<i64> {
        let conn = self.conn()?;
        let next: i64 = conn.query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM events WHERE chat_id = ?1",
            params![chat_id],
            |r| r.get(0),
        )?;
        Ok(next)
    }

    /// Oturum satırını ekler veya günceller.
    pub fn upsert_session(&self, s: &SessionRow) -> Result<()> {
        let conn = self.conn()?;
        conn.execute(
            "INSERT INTO engine_sessions(id, chat_id, engine, native_session_id, status, started_at, ended_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET
                native_session_id = excluded.native_session_id,
                status = excluded.status,
                ended_at = excluded.ended_at",
            params![
                s.id,
                s.chat_id,
                engine_str(s.engine),
                s.native_session_id,
                s.status.as_str(),
                s.started_at.to_rfc3339(),
                s.ended_at.map(|t| t.to_rfc3339()),
            ],
        )?;
        Ok(())
    }

    /// Açık (`running` ya da `waiting_quota`) oturumları döner.
    pub fn open_sessions(&self) -> Result<Vec<SessionRow>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, chat_id, engine, native_session_id, status, started_at, ended_at
             FROM engine_sessions WHERE status IN ('running', 'waiting_quota')
             ORDER BY started_at ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, Option<String>>(6)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, chat_id, engine, native, status, started, ended) = row?;
            out.push(SessionRow {
                id,
                chat_id,
                engine: engine_parse(&engine)?,
                native_session_id: native,
                status: SessionStatus::parse(&status)?,
                started_at: parse_ts(&started)?,
                ended_at: ended.as_deref().map(parse_ts).transpose()?,
            });
        }
        Ok(out)
    }

    /// Sohbetin en son başlayan oturumu (durumdan bağımsız).
    pub fn latest_session(&self, chat_id: &str) -> Result<Option<SessionRow>> {
        let conn = self.conn()?;
        let row = conn
            .query_row(
                "SELECT id, chat_id, engine, native_session_id, status, started_at, ended_at
                 FROM engine_sessions WHERE chat_id = ?1
                 ORDER BY started_at DESC, rowid DESC LIMIT 1",
                params![chat_id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, String>(5)?,
                        r.get::<_, Option<String>>(6)?,
                    ))
                },
            )
            .optional()?;
        row.map(|(id, chat_id, engine, native, status, started, ended)| {
            Ok(SessionRow {
                id,
                chat_id,
                engine: engine_parse(&engine)?,
                native_session_id: native,
                status: SessionStatus::parse(&status)?,
                started_at: parse_ts(&started)?,
                ended_at: ended.as_deref().map(parse_ts).transpose()?,
            })
        })
        .transpose()
    }

    /// Sohbetin güncel motoru (henüz hiç tur yoksa `None`).
    pub fn chat_engine(&self, chat_id: &str) -> Result<Option<EngineId>> {
        let conn = self.conn()?;
        let text: Option<Option<String>> = conn
            .query_row(
                "SELECT current_engine FROM chats WHERE id = ?1",
                params![chat_id],
                |r| r.get(0),
            )
            .optional()?;
        text.flatten().as_deref().map(engine_parse).transpose()
    }

    /// Sohbetin güncel motorunu yazar.
    pub fn set_chat_engine(&self, chat_id: &str, engine: EngineId) -> Result<()> {
        let conn = self.conn()?;
        conn.execute(
            "UPDATE chats SET current_engine = ?2 WHERE id = ?1",
            params![chat_id, engine_str(engine)],
        )?;
        Ok(())
    }

    /// Sohbette son görülen git HEAD'i okur.
    pub fn chat_last_head(&self, chat_id: &str) -> Result<Option<String>> {
        let conn = self.conn()?;
        let head: Option<Option<String>> = conn
            .query_row(
                "SELECT last_head FROM chats WHERE id = ?1",
                params![chat_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(head.flatten())
    }

    /// Sohbette son görülen git HEAD'i yazar.
    pub fn set_chat_last_head(&self, chat_id: &str, head: &str) -> Result<()> {
        let conn = self.conn()?;
        conn.execute(
            "UPDATE chats SET last_head = ?2 WHERE id = ?1",
            params![chat_id, head],
        )?;
        Ok(())
    }

    /// Sohbetin çalışma durumunu yazar (üzerine yazar).
    pub fn put_work_state(&self, chat_id: &str, json: &Value) -> Result<()> {
        let conn = self.conn()?;
        conn.execute(
            "INSERT INTO work_state(chat_id, json, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(chat_id) DO UPDATE SET json = excluded.json, updated_at = excluded.updated_at",
            params![chat_id, json.to_string(), Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    /// Sohbetin çalışma durumunu okur.
    pub fn get_work_state(&self, chat_id: &str) -> Result<Option<Value>> {
        let conn = self.conn()?;
        let text: Option<String> = conn
            .query_row(
                "SELECT json FROM work_state WHERE chat_id = ?1",
                params![chat_id],
                |r| r.get(0),
            )
            .optional()?;
        text.map(|t| serde_json::from_str(&t).map_err(anyhow::Error::from))
            .transpose()
    }

    /// Mesajlarda tam metin araması yapar. Sorgu sözcükleri ayrı ayrı tırnaklanır (AND).
    pub fn search_messages(
        &self,
        chat_id: Option<&str>,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchHit>> {
        let expr = fts_match_expr(query);
        if expr.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.conn()?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let map = |r: &rusqlite::Row<'_>| {
            Ok(SearchHit {
                chat_id: r.get(0)?,
                event_id: r.get(1)?,
                text: r.get(2)?,
            })
        };
        let hits = match chat_id {
            Some(chat) => {
                let mut stmt = conn.prepare(
                    "SELECT chat_id, event_id, text FROM messages_fts
                     WHERE messages_fts MATCH ?1 AND chat_id = ?2
                     ORDER BY rank LIMIT ?3",
                )?;
                stmt.query_map(params![expr, chat, limit], map)?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            }
            None => {
                let mut stmt = conn.prepare(
                    "SELECT chat_id, event_id, text FROM messages_fts
                     WHERE messages_fts MATCH ?1 ORDER BY rank LIMIT ?2",
                )?;
                stmt.query_map(params![expr, limit], map)?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            }
        };
        Ok(hits)
    }
}

/// En fazla [`TITLE_MAX_CHARS`] karakterlik, sözcük sınırında kesilmiş tek satır başlık.
pub fn auto_title(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= TITLE_MAX_CHARS {
        return flat;
    }
    let cut: String = flat.chars().take(TITLE_MAX_CHARS).collect();
    // Kesim bir sözcüğün ortasına düştüyse son boşluğa geri çekilir.
    let at_boundary = flat.chars().nth(TITLE_MAX_CHARS) == Some(' ');
    match cut.rfind(' ') {
        Some(i) if !at_boundary && i > 0 => cut[..i].to_string(),
        _ => cut.trim_end().to_string(),
    }
}

/// Tam metin dizinine girecek metin (yalnız mesaj olayları).
fn indexable_text<'a>(kind: &str, payload: &'a Value) -> Option<&'a str> {
    match kind {
        "assistant_text" | "user_text" => payload.get("text").and_then(Value::as_str),
        _ => None,
    }
}

/// Sözcükleri FTS5 çift tırnağına alır; tırnak içindeki `"` karakterleri kaçışlanır.
fn fts_match_expr(query: &str) -> String {
    query
        .split_whitespace()
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

fn raw_event_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<RawEvent> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
    ))
}

fn raw_to_event(raw: RawEvent) -> Result<StoredEvent> {
    let (id, chat_id, session_id, seq, ts, kind, payload) = raw;
    Ok(StoredEvent {
        id,
        chat_id,
        session_id,
        seq,
        ts: parse_ts(&ts)?,
        kind,
        payload: serde_json::from_str(&payload)?,
    })
}

fn parse_ts(s: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(s)?.with_timezone(&Utc))
}

fn engine_str(e: EngineId) -> &'static str {
    match e {
        EngineId::Claude => "claude",
        EngineId::Codex => "codex",
        EngineId::Agy => "agy",
    }
}

fn engine_parse(s: &str) -> Result<EngineId> {
    Ok(match s {
        "claude" => EngineId::Claude,
        "codex" => EngineId::Codex,
        "agy" => EngineId::Agy,
        other => return Err(anyhow!("bilinmeyen motor: {other}")),
    })
}

#[cfg(test)]
mod writer_regression_tests;
