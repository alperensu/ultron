//! Olay köprüsü: genel `Events { follow: true }` akışı; kopunca üstel geri çekilmeyle yeniden
//! bağlanır, sohbet başına son `seq`'ten devam eder (kaçırma yok, tekrar yok).

use std::collections::HashMap;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use ultron_core::db::StoredEvent;
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
    emit: &mut F,
) -> anyhow::Result<()> {
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
    sync.initialized = true;
    Ok(())
}

async fn follow_once<F: FnMut(StoredEvent), G: FnMut(DaemonStatus)>(
    bridge: &Bridge,
    sync: &mut Sync,
    emit: &mut F,
    on_state: &mut G,
    cancel: &CancellationToken,
    connected: &mut bool,
) -> anyhow::Result<()> {
    // Önce abone ol: bu andan sonraki olaylar akışta (boruda) bekler.
    let mut client: Client = bridge.connect().await?;
    client
        .send(&Request::Events {
            chat_id: None,
            follow: true,
            after_seq: None,
            limit: None,
        })
        .await?;
    *connected = true;
    on_state(DaemonStatus::Ready);
    // Sonra kaçanları günlükten tamamla; ardından bekleyen canlı olaylar tekrarsız akar
    // (`Cursors::accept` yinelenenleri eler).
    reconcile(bridge, sync, emit).await?;
    loop {
        let resp = tokio::select! {
            () = cancel.cancelled() => return Ok(()),
            r = client.next() => r?,
        };
        match resp {
            Some(Response::Event(ev)) => {
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
