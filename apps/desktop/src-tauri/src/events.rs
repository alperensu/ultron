//! Olay köprüsü: genel `Events { follow: true }` akışı; kopunca üstel geri çekilmeyle yeniden
//! bağlanır, sohbet başına son `seq`'ten devam eder (kaçırma yok, tekrar yok).
//! `policy_violation` dahil sohbet olayları tür filtresi olmadan `ultron://event`'e aktarılır.

use std::collections::HashMap;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use ultron_core::db::StoredEvent;
use ultron_core::db::approvals::{Approval, ApprovalStatus};
use ultron_core::events::EngineEvent;
use ultron_core::ipc::{ChatInfo, Client, Request, Response};

use crate::bridge::{Bridge, BridgeError};

/// Webview'in dinlediği olay adı.
pub const EVENT_NAME: &str = "ultron://event";

/// Yeniden bağlanma geri çekilme sınırları.
#[derive(Clone, Copy)]
pub struct Backoff {
    pub initial: Duration,
    pub max: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Backoff {
            initial: Duration::from_millis(250),
            max: Duration::from_secs(8),
        }
    }
}

/// Servis bağlantı durumu; yalnız geçişlerde bildirilir.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonStatus {
    Ready,
    Unavailable(BridgeError),
}

/// Sohbet başına iletilen son sıra; tekrarları eler.
#[derive(Default)]
struct Cursors(HashMap<String, i64>);

impl Cursors {
    fn last(&self, chat: &str) -> Option<i64> {
        self.0.get(chat).copied()
    }
    /// Olay yeni ise imleci ilerletir ve `true` döner.
    fn accept(&mut self, ev: &StoredEvent) -> bool {
        let last = self.last(&ev.chat_id).unwrap_or(0);
        if ev.seq <= last {
            return false;
        }
        self.0.insert(ev.chat_id.clone(), ev.seq);
        true
    }
}

/// Köprünün bildiği durum: imleçler ve görülen sohbetler (`id -> updated_at`). İlk başarılı
/// uzlaşmada eski geçmiş geri yüklenmez; yalnız taban çizgisi alınır.
#[derive(Default)]
struct Sync {
    cursors: Cursors,
    known: HashMap<String, String>,
    initialized: bool,
    global_approvals: HashMap<String, ApprovalStatus>,
}

impl Sync {
    fn accept_global_approval(&mut self, ev: &StoredEvent) -> bool {
        let Some(id) = ev.payload["approval_id"].as_str() else {
            return false;
        };
        let status = match ev.kind.as_str() {
            "approval_requested" => ApprovalStatus::Pending,
            "approval_resolved" => {
                let Ok(status) = serde_json::from_value(ev.payload["status"].clone()) else {
                    return false;
                };
                status
            }
            _ => return false,
        };
        if self
            .global_approvals
            .get(id)
            .is_some_and(|previous| *previous == status || *previous != ApprovalStatus::Pending)
        {
            return false;
        }
        self.global_approvals.insert(id.into(), status);
        true
    }

    fn restore_global_approval<F: FnMut(StoredEvent)>(
        &mut self,
        row: Approval,
        emit: &mut F,
    ) -> anyhow::Result<()> {
        if row.chat_id.is_some() {
            return Ok(());
        }
        let event = if row.status == ApprovalStatus::Pending {
            EngineEvent::ApprovalRequested {
                approval_id: row.id.clone(),
                category: row.category,
                reason: row.reason,
                summary: ultron_core::eventlog::excerpt(&row.action_json, 4000),
            }
        } else {
            EngineEvent::ApprovalResolved {
                approval_id: row.id.clone(),
                status: row.status,
                by: row
                    .resolved_by
                    .ok_or_else(|| anyhow::anyhow!("onay çözüm kaynağı eksik"))?,
            }
        };
        let payload = serde_json::to_value(event)?;
        let ev = StoredEvent {
            id: format!(
                "approval-state-{}-{}",
                row.id,
                payload["type"].as_str().unwrap_or_default()
            ),
            chat_id: String::new(),
            session_id: None,
            // Durum özeti küresel olay imleci değildir; sıralama onay durum makinesindedir.
            seq: 0,
            ts: row.resolved_at.unwrap_or(row.created_at),
            kind: payload["type"].as_str().unwrap_or_default().into(),
            payload,
        };
        if self.accept_global_approval(&ev) {
            emit(ev);
        }
        Ok(())
    }
}

/// Abonesiz başlangıçta veya kopuklukta kaçan sohbet dışı kararları kalıcı durumdan tamamlar.
async fn reconcile_approvals<F: FnMut(StoredEvent)>(
    bridge: &Bridge,
    sync: &mut Sync,
    emit: &mut F,
) -> anyhow::Result<()> {
    let mut cursor = None;
    loop {
        let data = bridge
            .request(Request::ListApprovals {
                status: None,
                limit: Some(32),
                cursor: cursor.clone(),
            })
            .await?;
        let rows: Vec<Approval> = serde_json::from_value(data["approvals"].clone())?;
        for row in rows {
            sync.restore_global_approval(row, emit)?;
        }
        let next: Option<String> = serde_json::from_value(data["next_cursor"].clone())?;
        if next.is_none() {
            return Ok(());
        }
        anyhow::ensure!(next > cursor, "onay sayfalama imleci ilerlemedi");
        cursor = next;
    }
}

/// Bir sohbetin `after_seq` sonrasını kalıcı günlükten çekip sırayla iletir.
async fn catch_up<F: FnMut(StoredEvent)>(
    bridge: &Bridge,
    chat: &str,
    cursors: &mut Cursors,
    emit: &mut F,
) -> anyhow::Result<()> {
    let mut client = bridge.connect().await?;
    client
        .send(&Request::Events {
            chat_id: Some(chat.to_string()),
            follow: false,
            after_seq: cursors.last(chat),
            limit: None,
        })
        .await?;
    loop {
        match client.next().await? {
            Some(Response::Event(ev)) => {
                if cursors.accept(&ev) {
                    emit(ev);
                }
            }
            Some(Response::Ok { .. }) | None => return Ok(()),
            Some(Response::Error { message }) => anyhow::bail!(message),
        }
    }
}

/// Yeniden bağlanınca kaçanları tamamlar: imleci olan sohbetler imlecinden, kopukken oluşan veya
/// değişen sohbetler (`ListChats`) baştan (seq 0). İlk uzlaşmada yalnız taban çizgisi alınır.
async fn reconcile<F: FnMut(StoredEvent)>(
    bridge: &Bridge,
    sync: &mut Sync,
    heads: &HashMap<String, i64>,
    emit: &mut F,
) -> anyhow::Result<()> {
    reconcile_approvals(bridge, sync, emit).await?;
    let data = bridge
        .request(Request::ListChats {
            project_id: None,
            include_archived: true,
        })
        .await?;
    let chats: Vec<ChatInfo> = serde_json::from_value(data["chats"].clone())?;
    for chat in chats {
        let changed = sync
            .known
            .get(&chat.id)
            .is_none_or(|u| *u != chat.updated_at);
        let stale = sync.initialized && changed;
        if sync.cursors.last(&chat.id).is_some() || stale {
            catch_up(bridge, &chat.id, &mut sync.cursors, emit).await?;
        }
        sync.known.insert(chat.id, chat.updated_at);
    }
    // İlk bağlantıda geçmişi yeniden oynatma kuralı korunur. Sonraki bağlantılarda zaman
    // damgası değişmemiş olsa bile onaydaki başa kadar tamamla; canlı kopyaları imleç eler.
    for (chat, head) in heads {
        if (sync.initialized || sync.cursors.last(chat).is_some())
            && sync.cursors.last(chat).unwrap_or(0) < *head
        {
            catch_up(bridge, chat, &mut sync.cursors, emit).await?;
            anyhow::ensure!(
                sync.cursors.last(chat).unwrap_or(0) >= *head,
                "abonelik başına kadar olaylar tamamlanamadı"
            );
        }
    }
    sync.initialized = true;
    Ok(())
}

/// Yalnız bu bağlantının geçerli onayından sonra istemciyi ve abonelik başlarını döndürür.
/// İlk bağlantı, yeniden bağlantı ve hata sonrası deneme aynı kapıdan geçer.
async fn subscribe(bridge: &Bridge) -> anyhow::Result<(Client, HashMap<String, i64>)> {
    let mut client: Client = bridge.connect().await?;
    client
        .send(&Request::Events {
            chat_id: None,
            follow: true,
            after_seq: None,
            limit: None,
        })
        .await?;
    // `send` aboneliği kanıtlamaz: sunucunun `{"subscribed":true}` onayını bekle. Bundan önce
    // uzlaşma/Ready yok; onaydan sonraki her olay akışta bekler.
    let resp = tokio::time::timeout(Duration::from_secs(30), client.next()).await??;
    let heads: HashMap<String, i64> = match resp {
        Some(Response::Ok { data }) if data["subscribed"] == true => {
            let heads: HashMap<String, i64> = serde_json::from_value(data["head_seq"].clone())?;
            anyhow::ensure!(
                heads.values().all(|seq| *seq >= 0),
                "geçersiz abonelik başı"
            );
            heads
        }
        Some(Response::Error { message }) => anyhow::bail!(message),
        None => anyhow::bail!("olay akışı abonelik onayından önce kapandı"),
        _ => anyhow::bail!("ilk akış yanıtı abonelik onayı olmalı"),
    };
    Ok((client, heads))
}

async fn follow_once<F: FnMut(StoredEvent), G: FnMut(DaemonStatus)>(
    bridge: &Bridge,
    sync: &mut Sync,
    emit: &mut F,
    on_state: &mut G,
    cancel: &CancellationToken,
    connected: &mut bool,
) -> anyhow::Result<()> {
    let (mut client, heads) = tokio::select! {
        () = cancel.cancelled() => return Ok(()),
        result = subscribe(bridge) => result?,
    };
    // Onaylanmış bu bağlantıdan sonra kaçanları tamamla; bekleyen canlı olaylar tekrarsız akar
    // (`Cursors::accept` yinelenenleri eler).
    tokio::select! {
        () = cancel.cancelled() => return Ok(()),
        result = reconcile(bridge, sync, &heads, emit) => result?,
    }
    *connected = true;
    on_state(DaemonStatus::Ready);
    loop {
        let resp = tokio::select! {
            () = cancel.cancelled() => return Ok(()),
            r = client.next() => r?,
        };
        match resp {
            Some(Response::Event(ev)) => {
                if ev.chat_id.is_empty() {
                    if sync.accept_global_approval(&ev) {
                        emit(ev);
                    }
                    continue;
                }
                // Boşluk varsa (bilinen sohbette atlanan sıra ya da ilk olayı seq > 1 olan
                // sohbet) eksik öneki günlükten tamamla; sonra bu olayı ilet.
                let last = sync.cursors.last(&ev.chat_id).unwrap_or(0);
                if ev.seq > last + 1 {
                    catch_up(bridge, &ev.chat_id, &mut sync.cursors, emit).await?;
                }
                if sync.cursors.accept(&ev) {
                    emit(ev);
                }
            }
            Some(Response::Ok { .. }) => {}
            Some(Response::Error { message }) => anyhow::bail!(message),
            None => anyhow::bail!("olay akışı kapandı"),
        }
    }
}

/// İptal edilene dek olayları `emit`'e iletir; kopunca yeniden bağlanır.
pub async fn run_follow<F: FnMut(StoredEvent)>(
    bridge: &Bridge,
    backoff: Backoff,
    cancel: CancellationToken,
    emit: F,
) {
    run_follow_with_state(bridge, backoff, cancel, emit, |_| {}).await;
}

/// `run_follow` + servis durumu: bağlanınca `Ready`, bağlanamayınca/kopunca `Unavailable`
/// (yalnız durum değişiminde).
pub async fn run_follow_with_state<F: FnMut(StoredEvent), G: FnMut(DaemonStatus)>(
    bridge: &Bridge,
    backoff: Backoff,
    cancel: CancellationToken,
    mut emit: F,
    mut on_state: G,
) {
    let mut sync = Sync::default();
    let mut delay = backoff.initial;
    let mut last: Option<DaemonStatus> = None;
    let mut notify = |status: DaemonStatus| {
        if last.as_ref() != Some(&status) {
            last = Some(status.clone());
            on_state(status);
        }
    };
    while !cancel.is_cancelled() {
        let mut connected = false;
        let result = follow_once(
            bridge,
            &mut sync,
            &mut emit,
            &mut notify,
            &cancel,
            &mut connected,
        )
        .await;
        if cancel.is_cancelled() {
            return;
        }
        if connected {
            delay = backoff.initial;
        }
        if let Err(e) = result {
            tracing::warn!(error = %format!("{e:#}"), "olay akışı koptu, yeniden denenecek");
            let err = match e.downcast::<BridgeError>() {
                Ok(b) => b,
                Err(e) => BridgeError::DaemonUnavailable {
                    reason: format!("{e:#}"),
                },
            };
            notify(DaemonStatus::Unavailable(err));
        }
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(delay) => {}
        }
        delay = (delay * 2).min(backoff.max);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_violation_uses_the_chat_cursor_without_losing_payload() {
        let event = EngineEvent::PolicyViolation {
            command: "rm sentetik.txt".into(),
            category: ultron_core::policy::RiskCategory::Delete,
        };
        let payload = serde_json::to_value(event).unwrap();
        let stored = StoredEvent {
            id: "sentetik-ihlal".into(), chat_id: "c1".into(), session_id: Some("s1".into()),
            seq: 1, ts: "2026-01-01T00:00:00Z".parse().unwrap(), kind: "policy_violation".into(), payload: payload.clone(),
        };
        let mut cursors = Cursors::default();
        assert!(cursors.accept(&stored));
        assert!(!cursors.accept(&stored));
        let wire = serde_json::to_value(&stored).unwrap();
        assert_eq!(wire["payload"], payload);
        assert_eq!(wire["kind"], "policy_violation");
    }

    #[test]
    fn chatless_state_reconcile_restores_expiry_and_ignores_stale_live_pending() {
        let pending: Approval = serde_json::from_value(serde_json::json!({
            "id": "synthetic-approval", "chat_id": null, "engine": "claude",
            "category": "Delete", "reason": "test", "action_json": "{}",
            "status": "pending", "created_at": "2026-01-01T00:00:00Z",
            "resolved_at": null, "resolved_by": null
        }))
        .unwrap();
        let mut expired = pending.clone();
        expired.status = ApprovalStatus::Expired;
        expired.resolved_by = Some(ultron_core::db::approvals::ResolvedBy::Restart);
        expired.resolved_at = Some("2026-01-01T00:01:00Z".parse().unwrap());
        let mut sync = Sync::default();
        let mut events = Vec::new();
        sync.restore_global_approval(pending.clone(), &mut |ev| events.push(ev))
            .unwrap();
        sync.restore_global_approval(expired.clone(), &mut |ev| events.push(ev))
            .unwrap();
        sync.restore_global_approval(expired.clone(), &mut |ev| events.push(ev))
            .unwrap();
        sync.restore_global_approval(pending, &mut |ev| events.push(ev))
            .unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].payload["status"], "expired");
        assert!(!sync.accept_global_approval(&events[0]));
        assert!(!sync.accept_global_approval(&events[1]));
        // Yeni masaüstü de abonesiz başlangıçta sonuçlanmış kaydı geri yükler.
        let mut fresh = Sync::default();
        fresh
            .restore_global_approval(expired, &mut |ev| {
                assert_eq!(ev.payload["status"], "expired")
            })
            .unwrap();
    }
}
