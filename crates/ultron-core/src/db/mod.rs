mod migrations;

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::EngineId;

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

    /// Olayı ekler. Aynı `id` zaten varsa `false` döner ve hiçbir şey değişmez.
    /// Mesaj olaylarının metni aynı işlemde tam metin dizinine yazılır.
    pub fn insert_event(&self, ev: &StoredEvent) -> Result<bool> {
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO events(id, chat_id, session_id, seq, ts, kind, payload_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
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
        tx.commit()?;
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
