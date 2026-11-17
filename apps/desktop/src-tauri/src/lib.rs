//! Ultron masaüstü kabuğu: `ultrond`'a ince köprü. İş mantığı yoktur.

pub mod bridge;
pub mod commands;
pub mod events;

use std::sync::Arc;

use serde_json::json;
use tauri::{Emitter, Manager};
use tokio_util::sync::CancellationToken;

use bridge::Bridge;

/// Servis durumu olayı (`ready` | `unavailable`); webview ilk açılışta hata ekranını buna göre kurar.
pub const DAEMON_EVENT: &str = "ultron://daemon";

/// `ultron://daemon` yükü (TS `DaemonState`).
pub fn daemon_payload(status: &events::DaemonStatus) -> serde_json::Value {
    match status {
        events::DaemonStatus::Ready => json!({"state": "ready"}),
        events::DaemonStatus::Unavailable(e) => json!({"state": "unavailable", "error": e}),
    }
}

pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            let bridge = Arc::new(Bridge::from_env()?);
            app.manage(bridge.clone());
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                // Takip döngüsü servisi bulur/başlatır; bağlanınca `ready`, bağlanamayınca veya
                // kopunca `unavailable` bildirir (servis sonradan gelirse de `ready` yayılır).
                let emitter = handle.clone();
                let state_emitter = handle.clone();
                events::run_follow_with_state(
                    &bridge,
                    events::Backoff::default(),
                    CancellationToken::new(),
                    move |ev| {
                        let _ = emitter.emit(events::EVENT_NAME, ev);
                    },
                    move |status| {
                        let _ = state_emitter.emit(DAEMON_EVENT, daemon_payload(&status));
                    },
                )
                .await;
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::list_projects,
            commands::list_chats,
            commands::get_chat,
            commands::update_chat,
            commands::send_message,
            commands::switch_engine,
            commands::cancel_turn,
            commands::events_page,
            commands::history_search,
            commands::get_quota,
            commands::set_reset_rights,
            commands::route_message,
            commands::import_catalog,
            commands::import_projects,
            commands::status,
        ])
        .run(tauri::generate_context!())
        .expect("Tauri uygulaması başlatılamadı");
}
