//! Olay köprüsü: genel `Events { follow: true }` akışı; kopunca üstel geri çekilmeyle yeniden
//! bağlanır, sohbet başına son `seq`'ten devam eder (kaçırma yok, tekrar yok).

use std::collections::HashMap;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use ultron_core::db::StoredEvent;
use ultron_core::ipc::{Client, Request, Response};

use crate::bridge::Bridge;

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

async fn follow_once<F: FnMut(StoredEvent)>(
    bridge: &Bridge,
    cursors: &mut Cursors,
    emit: &mut F,
    cancel: &CancellationToken,
    connected: &mut bool,
) -> anyhow::Result<()> {
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
    // Kopukken kaçan olayları bilinen sohbetler için tamamla.
    let known: Vec<String> = cursors.0.keys().cloned().collect();
    for chat in known {
        catch_up(bridge, &chat, cursors, emit).await?;
    }
    loop {
        let resp = tokio::select! {
            () = cancel.cancelled() => return Ok(()),
            r = client.next() => r?,
        };
        match resp {
            Some(Response::Event(ev)) => {
                // Boşluk (kaçan sıra) varsa günlükten tamamla; sonra bu olayı ilet.
                if let Some(last) = cursors.last(&ev.chat_id)
                    && ev.seq > last + 1
                {
                    catch_up(bridge, &ev.chat_id, cursors, emit).await?;
                }
                if cursors.accept(&ev) {
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
    mut emit: F,
) {
    let mut cursors = Cursors::default();
    let mut delay = backoff.initial;
    while !cancel.is_cancelled() {
        let mut connected = false;
        let result = follow_once(bridge, &mut cursors, &mut emit, &cancel, &mut connected).await;
        if cancel.is_cancelled() {
            return;
        }
        if connected {
            delay = backoff.initial;
        }
        if let Err(e) = result {
            tracing::warn!(error = %format!("{e:#}"), "olay akışı koptu, yeniden denenecek");
        }
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(delay) => {}
        }
        delay = (delay * 2).min(backoff.max);
    }
}
