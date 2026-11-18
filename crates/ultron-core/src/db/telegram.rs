//! Telegram içeriği yalnız normal kullanıcı olayında tutulur. Kabulden sonra çökme
//! otomatik replay izni değildir; uzlaştırma kaydı dikkat bekler durumuna taşır.

use anyhow::{Result, anyhow, bail, ensure};
use chrono::Utc;
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};

use super::{Db, StoredEvent};
use crate::events::EngineEvent;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboxState {
    Accepted,
    Running,
    Completed,
    NeedsAttention,
}

impl InboxState {
    fn parse(value: &str) -> rusqlite::Result<Self> {
        match value {
            "accepted" => Ok(Self::Accepted),
            "running" => Ok(Self::Running),
            "completed" => Ok(Self::Completed),
            "needs_attention" => Ok(Self::NeedsAttention),
            _ => Err(rusqlite::Error::InvalidQuery),
        }
    }
}

pub struct InboxEntry {
    pub bot_id: i64,
    pub update_id: i64,
    pub user_id: i64,
    pub state: InboxState,
    pub logical_turn_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryState {
    Pending,
    Ready,
    Delivered,
    NeedsAttention,
}

impl DeliveryState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Ready => "ready",
            Self::Delivered => "delivered",
            Self::NeedsAttention => "needs_attention",
        }
    }

    fn parse(value: &str) -> rusqlite::Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "ready" => Ok(Self::Ready),
            "delivered" => Ok(Self::Delivered),
            "needs_attention" => Ok(Self::NeedsAttention),
            _ => Err(rusqlite::Error::InvalidQuery),
        }
    }
}

/// Debug/serde yoktur: alıcı kimliği genel durum/log verisi değildir.
pub struct TelegramTurnOrigin {
    pub logical_turn_id: String,
    pub chat_id: String,
    pub user_id: i64,
    pub private_chat_id: i64,
    pub user_event_seq: i64,
    pub start_seq: i64,
    pub end_seq: Option<i64>,
    pub delivery_state: DeliveryState,
}

/// Kimlik filtresi çağıranda uygulanır; depo ayrıca pozitif özel sohbet eşitliğini doğrular.
pub struct TelegramTurnAcceptance<'a> {
    pub bot_id: i64,
    pub update_id: i64,
    pub user_id: i64,
    pub private_chat_id: i64,
    pub logical_turn_id: &'a str,
    pub event: &'a StoredEvent,
}

pub enum TelegramAcceptance {
    Accepted(StoredEvent),
    Duplicate(InboxEntry),
}

fn inbox_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<InboxEntry> {
    Ok(InboxEntry {
        bot_id: row.get(0)?,
        update_id: row.get(1)?,
        user_id: row.get(2)?,
        state: InboxState::parse(&row.get::<_, String>(3)?)?,
        logical_turn_id: row.get(4)?,
    })
}

fn checkpoint_tx(tx: &Transaction<'_>, bot_id: i64, next_update_id: i64) -> Result<()> {
    tx.execute(
        "INSERT INTO telegram_poll_state(bot_id, next_update_id) VALUES (?1, ?2)
         ON CONFLICT(bot_id) DO UPDATE SET next_update_id = MAX(next_update_id, excluded.next_update_id)",
        params![bot_id, next_update_id],
    )?;
    Ok(())
}

/// Kapanışın seq sınırı ve inbox durumu aynı transaction'da değişir.
pub(super) fn interrupt_telegram_chat_tx(tx: &Transaction<'_>, chat_id: &str, quota_only: bool) -> Result<()> {
    tx.execute(
        "UPDATE telegram_inbox SET state = 'needs_attention'
         WHERE state IN ('accepted', 'running') AND logical_turn_id IN (
             SELECT o.logical_turn_id FROM telegram_turn_origins o
             WHERE o.chat_id = ?1 AND o.end_seq IS NULL
             AND (NOT ?2 OR EXISTS(SELECT 1 FROM quota_waits q
                 WHERE q.logical_turn_id = o.logical_turn_id
                 AND q.state IN ('waiting', 'claimed', 'running', 'needs_attention'))))",
        params![chat_id, quota_only],
    )?;
    tx.execute(
        "UPDATE telegram_turn_origins AS o SET delivery_state = 'needs_attention', end_seq =
             MAX(o.start_seq, COALESCE((SELECT MAX(e.seq) FROM events e WHERE e.chat_id = o.chat_id), o.start_seq))
         WHERE o.chat_id = ?1 AND o.end_seq IS NULL
         AND (NOT ?2 OR EXISTS(SELECT 1 FROM quota_waits q
             WHERE q.logical_turn_id = o.logical_turn_id
             AND q.state IN ('waiting', 'claimed', 'running', 'needs_attention')))",
        params![chat_id, quota_only],
    )?;
    Ok(())
}

pub(super) fn validated_user_event(event: &StoredEvent) -> Result<StoredEvent> {
    ensure!(
        event.kind == "user_text",
        "kabul için kullanıcı olayı gerekli"
    );
    let EngineEvent::UserText { text } = serde_json::from_value(event.payload.clone())
        .map_err(|_| anyhow!("geçersiz kullanıcı olayı"))?
    else {
        bail!("kabul için kullanıcı olayı gerekli");
    };
    ensure!(!text.trim().is_empty(), "kullanıcı iletisi boş");
    let mut event = event.clone();
    // Ek JSON alanları ve canlı tur belirteçleri kabul kaydına taşınmaz.
    event.payload = serde_json::to_value(EngineEvent::UserText { text })?;
    crate::turn_grant::mask_live_value(&mut event.payload);
    Ok(event)
}

impl Db {
    pub fn telegram_binding(&self, user_id: i64) -> Result<Option<String>> {
        ensure!(user_id > 0, "geçersiz Telegram kullanıcısı");
        let row: Option<Option<String>> = self
            .conn()?
            .query_row(
                "SELECT chat_id FROM telegram_bindings WHERE user_id = ?1",
                [user_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(row.flatten())
    }

    pub fn set_telegram_binding(&self, user_id: i64, chat_id: Option<&str>) -> Result<()> {
        ensure!(user_id > 0, "geçersiz Telegram kullanıcısı");
        self.conn()?.execute(
            "INSERT INTO telegram_bindings(user_id, chat_id, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(user_id) DO UPDATE SET chat_id = excluded.chat_id, updated_at = excluded.updated_at",
            params![user_id, chat_id, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn telegram_next_update_id(&self, bot_id: i64) -> Result<Option<i64>> {
        ensure!(bot_id > 0, "geçersiz bot kimliği");
        Ok(self
            .conn()?
            .query_row(
                "SELECT next_update_id FROM telegram_poll_state WHERE bot_id = ?1",
                [bot_id],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Reddedilen güncelleme için yalnız ofset ilerler; kullanıcı/içerik saklanmaz.
    pub fn checkpoint_telegram_poll(&self, bot_id: i64, next_update_id: i64) -> Result<()> {
        ensure!(
            bot_id > 0 && next_update_id >= 0,
            "geçersiz Telegram ofseti"
        );
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        checkpoint_tx(&tx, bot_id, next_update_id)?;
        tx.commit()?;
        Ok(())
    }

    pub fn telegram_inbox_entry(&self, bot_id: i64, update_id: i64) -> Result<Option<InboxEntry>> {
        Ok(self
            .conn()?
            .query_row(
                "SELECT bot_id, update_id, user_id, state, logical_turn_id FROM telegram_inbox
             WHERE bot_id = ?1 AND update_id = ?2",
                params![bot_id, update_id],
                inbox_row,
            )
            .optional()?)
    }

    /// Salt komut/callback kabulü. true yalnız ilk kabulde döner; false yürütme izni değildir.
    /// Dış yan etkilerle atomiklik yoktur: crash sonrasında bu kayıt tekrar yürütülmez.
    pub fn accept_telegram_update(
        &self,
        bot_id: i64,
        update_id: i64,
        user_id: i64,
    ) -> Result<bool> {
        ensure!(
            bot_id > 0 && user_id > 0 && update_id >= 0,
            "geçersiz Telegram kabulü"
        );
        let next = update_id
            .checked_add(1)
            .ok_or_else(|| anyhow!("Telegram ofseti taştı"))?;
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let inserted = tx.execute(
            "INSERT INTO telegram_inbox(bot_id, update_id, user_id, state)
             SELECT ?1, ?2, ?3, 'accepted' WHERE NOT EXISTS
             (SELECT 1 FROM telegram_poll_state WHERE bot_id = ?1 AND next_update_id > ?2)
             ON CONFLICT(bot_id, update_id) DO NOTHING",
            params![bot_id, update_id, user_id],
        )?;
        checkpoint_tx(&tx, bot_id, next)?;
        tx.commit()?;
        Ok(inserted == 1)
    }

    /// Inbox, kullanıcı olayı/FTS, özgün alıcı, bağlama, eski bekleme iptali ve ofset
    /// tek IMMEDIATE transaction'dır. Dönen olay EventLog'un kalıcı yayın yoluna verilir.
    pub fn accept_telegram_turn(
        &self,
        request: &TelegramTurnAcceptance<'_>,
    ) -> Result<TelegramAcceptance> {
        ensure!(
            request.bot_id > 0
                && request.user_id > 0
                && request.private_chat_id == request.user_id
                && request.update_id >= 0,
            "geçersiz özel Telegram kabulü"
        );
        ensure!(
            !request.logical_turn_id.is_empty() && request.logical_turn_id.len() <= 128,
            "geçersiz mantıksal tur kimliği"
        );
        let next = request
            .update_id
            .checked_add(1)
            .ok_or_else(|| anyhow!("Telegram ofseti taştı"))?;
        let event = validated_user_event(request.event)?;
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing = tx
            .query_row(
                "SELECT bot_id, update_id, user_id, state, logical_turn_id FROM telegram_inbox
             WHERE bot_id = ?1 AND update_id = ?2",
                params![request.bot_id, request.update_id],
                inbox_row,
            )
            .optional()?;
        if let Some(existing) = existing {
            ensure!(
                existing.user_id == request.user_id,
                "Telegram kabul kimliği uyuşmuyor"
            );
            checkpoint_tx(&tx, request.bot_id, next)?;
            tx.commit()?;
            return Ok(TelegramAcceptance::Duplicate(existing));
        }
        let checkpoint: Option<i64> = tx
            .query_row(
                "SELECT next_update_id FROM telegram_poll_state WHERE bot_id = ?1",
                [request.bot_id],
                |r| r.get(0),
            )
            .optional()?;
        ensure!(
            checkpoint.is_none_or(|offset| request.update_id >= offset),
            "Telegram güncellemesi ofsetten eski"
        );
        interrupt_telegram_chat_tx(&tx, &event.chat_id, false)?;
        let stored = Self::append_event_tx(&tx, &event)?
            .ok_or_else(|| anyhow!("kullanıcı olayı zaten var"))?;
        super::quota_wait::invalidate_chat_tx(&tx, &stored.chat_id, Utc::now())?;
        tx.execute(
            "INSERT INTO telegram_turn_origins(logical_turn_id, chat_id, user_id, private_chat_id,
             user_event_seq, start_seq) VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
            params![
                request.logical_turn_id,
                stored.chat_id,
                request.user_id,
                request.private_chat_id,
                stored.seq
            ],
        )?;
        tx.execute(
            "INSERT INTO telegram_inbox(bot_id, update_id, user_id, state, logical_turn_id)
             VALUES (?1, ?2, ?3, 'accepted', ?4)",
            params![
                request.bot_id,
                request.update_id,
                request.user_id,
                request.logical_turn_id
            ],
        )?;
        tx.execute(
            "INSERT INTO telegram_bindings(user_id, chat_id, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(user_id) DO UPDATE SET chat_id = excluded.chat_id, updated_at = excluded.updated_at",
            params![request.user_id, stored.chat_id, stored.ts.to_rfc3339()],
        )?;
        tx.execute(
            "UPDATE chats SET updated_at = ?2 WHERE id = ?1",
            params![stored.chat_id, stored.ts.to_rfc3339()],
        )?;
        checkpoint_tx(&tx, request.bot_id, next)?;
        tx.commit()?;
        Ok(TelegramAcceptance::Accepted(stored))
    }

    /// Yerel yeni mesaj da bekleme neslini olayla aynı transaction'da geçersiz kılar.
    pub fn append_user_event_invalidating_quota(&self, event: &StoredEvent) -> Result<StoredEvent> {
        let event = validated_user_event(event)?;
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        interrupt_telegram_chat_tx(&tx, &event.chat_id, false)?;
        let stored = Self::append_event_tx(&tx, &event)?
            .ok_or_else(|| anyhow!("kullanıcı olayı zaten var"))?;
        super::quota_wait::invalidate_chat_tx(&tx, &stored.chat_id, Utc::now())?;
        tx.commit()?;
        Ok(stored)
    }

    pub fn telegram_turn_origin(
        &self,
        logical_turn_id: &str,
    ) -> Result<Option<TelegramTurnOrigin>> {
        Ok(self.conn()?.query_row(
            "SELECT logical_turn_id, chat_id, user_id, private_chat_id, user_event_seq,
             start_seq, end_seq, delivery_state FROM telegram_turn_origins WHERE logical_turn_id = ?1",
            [logical_turn_id], |r| Ok(TelegramTurnOrigin {
                logical_turn_id: r.get(0)?, chat_id: r.get(1)?, user_id: r.get(2)?, private_chat_id: r.get(3)?,
                user_event_seq: r.get(4)?, start_seq: r.get(5)?, end_seq: r.get(6)?,
                delivery_state: DeliveryState::parse(&r.get::<_, String>(7)?)?,
            }),
        ).optional()?)
    }

    /// Kabul edilmiş komutu/turu bir kez running yapar; tekrar deneme replay izni vermez.
    pub fn start_telegram_inbox(&self, bot_id: i64, update_id: i64) -> Result<bool> {
        Ok(self.conn()?.execute(
            "UPDATE telegram_inbox SET state = 'running' WHERE bot_id = ?1 AND update_id = ?2 AND state = 'accepted'",
            params![bot_id, update_id],
        )? == 1)
    }

    pub fn complete_telegram_update(&self, bot_id: i64, update_id: i64) -> Result<bool> {
        Ok(self.conn()?.execute(
            "UPDATE telegram_inbox SET state = 'completed' WHERE bot_id = ?1 AND update_id = ?2
             AND logical_turn_id IS NULL AND state IN ('accepted', 'running')",
            params![bot_id, update_id],
        )? == 1)
    }

    /// Final sınırı ve inbox kararı atomiktir; alıcı güncel binding'den yeniden seçilmez.
    pub fn complete_telegram_turn(&self, logical_turn_id: &str, end_seq: i64) -> Result<bool> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE telegram_turn_origins SET end_seq = ?2, delivery_state = 'ready'
             WHERE logical_turn_id = ?1 AND delivery_state = 'pending' AND start_seq <= ?2
             AND EXISTS(SELECT 1 FROM events e WHERE e.chat_id = telegram_turn_origins.chat_id AND e.seq = ?2)
             AND EXISTS(SELECT 1 FROM telegram_inbox i WHERE i.logical_turn_id = ?1 AND i.state IN ('accepted', 'running'))",
            params![logical_turn_id, end_seq],
        )?;
        if changed == 1 {
            tx.execute(
                "UPDATE telegram_inbox SET state = 'completed' WHERE logical_turn_id = ?1",
                [logical_turn_id],
            )?;
        }
        tx.commit()?;
        Ok(changed == 1)
    }

    /// Henüz kota kaydı açılmadan iptal edilen denemenin sonucunu kapatır.
    pub fn interrupt_telegram_turn(&self, logical_turn_id: &str, end_seq: i64) -> Result<()> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE telegram_turn_origins SET end_seq = ?2, delivery_state = 'needs_attention'
             WHERE logical_turn_id = ?1 AND end_seq IS NULL AND start_seq <= ?2",
            params![logical_turn_id, end_seq],
        )?;
        if changed == 1 {
            tx.execute(
                "UPDATE telegram_inbox SET state = 'needs_attention'
                 WHERE logical_turn_id = ?1 AND state IN ('accepted', 'running')",
                [logical_turn_id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Belirsiz ağ teslimi delivered diye işaretlenmez; otomatik yeniden gönderim yoktur.
    pub fn finish_telegram_delivery(&self, logical_turn_id: &str, delivered: bool) -> Result<bool> {
        let state = if delivered {
            DeliveryState::Delivered
        } else {
            DeliveryState::NeedsAttention
        };
        Ok(self.conn()?.execute(
            "UPDATE telegram_turn_origins SET delivery_state = ?2 WHERE logical_turn_id = ?1 AND delivery_state = 'ready'",
            params![logical_turn_id, state.as_str()],
        )? == 1)
    }

    /// Restart belirsiz kabulü replay etmez. Doğrulanmış kalıcı waiting kaydı olan
    /// mantıksal tur scheduler'a bırakılır; özgün alıcısı/final teslimi korunur.
    /// Kota uzlaştırması aynı transaction'da yapılır; çağrı sırasına bağımlı değildir.
    pub fn recover_telegram_inbox(&self) -> Result<usize> {
        let mut conn = self.conn()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        super::quota_wait::recover_waits_tx(&tx, Utc::now())?;
        tx.execute(
            "UPDATE telegram_turn_origins AS o SET delivery_state = 'needs_attention', end_seq = MAX(o.start_seq, COALESCE(
                 (SELECT MIN(e.seq) - 1 FROM events e WHERE e.chat_id = o.chat_id AND e.seq > o.start_seq AND e.kind = 'user_text'),
                 (SELECT MAX(e.seq) FROM events e WHERE e.chat_id = o.chat_id), o.start_seq))
             WHERE o.end_seq IS NULL AND NOT EXISTS(
                 SELECT 1 FROM quota_waits q JOIN telegram_inbox i ON i.logical_turn_id = q.logical_turn_id
                 WHERE q.logical_turn_id = o.logical_turn_id AND q.state = 'waiting'
                 AND i.state IN ('accepted', 'running'))",
            [],
        )?;
        let count = tx.execute("UPDATE telegram_inbox SET state = 'needs_attention' WHERE state IN ('accepted', 'running')
            AND NOT EXISTS(SELECT 1 FROM quota_waits q
                WHERE q.logical_turn_id = telegram_inbox.logical_turn_id AND q.state = 'waiting')", [])?;
        tx.commit()?;
        Ok(count)
    }
}
