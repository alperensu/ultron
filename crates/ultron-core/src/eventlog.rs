use std::io::Write;
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow, bail};
use chrono::Utc;
use serde_json::{Map, Value};
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::db::{Db, StoredEvent};
use crate::events::EngineEvent;
use crate::paths::Paths;

/// Çıktı kırpma için varsayılan üst sınır (karakter).
pub const DEFAULT_EXCERPT_CHARS: usize = 4000;

const CHANNEL_CAPACITY: usize = 1024;

/// Olay günlüğü: SQLite'a yazar, `transcripts\<chat>.jsonl` dökümünü tutar, aboneleri besler.
pub struct EventLog {
    db: Arc<Db>,
    paths: Paths,
    seq_lock: Mutex<()>,
    tx: broadcast::Sender<StoredEvent>,
}

impl EventLog {
    pub fn new(db: Arc<Db>, paths: Paths) -> EventLog {
        let (tx, _) = broadcast::channel(CHANNEL_CAPACITY);
        EventLog {
            db,
            paths,
            seq_lock: Mutex::new(()),
            tx,
        }
    }

    /// Olayı sıra numarasıyla kaydeder, dökümü yazar ve aboneleri bilgilendirir.
    pub fn append(
        &self,
        chat_id: &str,
        session_id: Option<&str>,
        ev: EngineEvent,
    ) -> Result<StoredEvent> {
        validate_chat_id(chat_id)?;
        let payload = serde_json::to_value(&ev)?;
        let kind = payload
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("olay tipi üretilemedi"))?
            .to_string();

        let _guard = self
            .seq_lock
            .lock()
            .map_err(|_| anyhow!("olay sıra kilidi zehirlendi"))?;
        let stored = StoredEvent {
            id: Uuid::new_v4().to_string(),
            chat_id: chat_id.to_string(),
            session_id: session_id.map(str::to_string),
            seq: 0, // Veritabanı işlemi içinde tahsis edilir.
            ts: Utc::now(),
            kind,
            payload,
        };
        let stored = self
            .db
            .append_event(&stored)?
            .ok_or_else(|| anyhow!("olay zaten kayıtlı: {}", stored.id))?;
        self.write_transcript(&stored, false)?;
        // Dinleyici yoksa gönderim hatası beklenen bir durumdur.
        let _ = self.tx.send(stored.clone());
        Ok(stored)
    }

    /// Yeni olayları izlemek için abone olur.
    pub fn subscribe(&self) -> broadcast::Receiver<StoredEvent> {
        self.tx.subscribe()
    }

    /// Kalıcı onay olayını normal dökümle aynı yazıcı ve kilitten geçirir.
    pub fn write_persisted_transcript(&self, stored: &StoredEvent) -> Result<()> {
        validate_chat_id(&stored.chat_id)?;
        let _guard = self
            .seq_lock
            .lock()
            .map_err(|_| anyhow!("olay sıra kilidi zehirlendi"))?;
        self.write_transcript(stored, true)
    }

    fn write_transcript(&self, stored: &StoredEvent, durable: bool) -> Result<()> {
        let path = self
            .paths
            .transcripts
            .join(format!("{}.jsonl", stored.chat_id));
        let mut line = Map::new();
        line.insert("id".into(), Value::String(stored.id.clone()));
        line.insert("seq".into(), Value::from(stored.seq));
        line.insert("ts".into(), Value::String(stored.ts.to_rfc3339()));
        line.insert("chat_id".into(), Value::String(stored.chat_id.clone()));
        line.insert(
            "session_id".into(),
            stored.session_id.clone().map_or(Value::Null, Value::String),
        );
        if let Value::Object(payload) = &stored.payload {
            for (k, v) in payload {
                line.insert(k.clone(), v.clone());
            }
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        writeln!(file, "{}", Value::Object(line))?;
        if durable {
            file.sync_data()?;
        }
        Ok(())
    }
}

/// Sohbet kimliği dosya adında kullanılır; yalnız güvenli karakterlere izin verilir.
fn validate_chat_id(id: &str) -> Result<()> {
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("geçersiz sohbet kimliği: {id}");
    }
    Ok(())
}

/// Uzun metni baş %60 + kırpma işareti + son %40 olacak şekilde kısaltır.
pub fn excerpt(s: &str, max_chars: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_chars {
        return s.to_string();
    }
    let head = max_chars * 6 / 10;
    let tail = max_chars * 4 / 10;
    let removed = chars.len() - head - tail;
    let mut out: String = chars[..head].iter().collect();
    out.push_str(&format!("\n…[{removed} karakter kırpıldı]…\n"));
    out.extend(chars[chars.len() - tail..].iter());
    out
}
