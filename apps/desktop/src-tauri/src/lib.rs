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

pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            let bridge = Arc::new(Bridge::from_env()?);
            app.manage(bridge.clone());
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                // Başlangıç: servisi bul/başlat; başarısızsa DaemonUnavailable bildir ve dene.
                match bridge.connect().await {
                    Ok(_) => {
                        let _ = handle.emit(DAEMON_EVENT, json!({"state": "ready"}));
                    }
                    Err(e) => {
                        let _ = handle.emit(
                            DAEMON_EVENT,
                            json!({"state": "unavailable", "error": e}),
                        );
                    }
                }
                let emitter = handle.clone();
                events::run_follow(
                    &bridge,
                    events::Backoff::default(),
                    CancellationToken::new(),
                    move |ev| {
                        let _ = emitter.emit(events::EVENT_NAME, ev);
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
            commands::import_claude_history,
            commands::status,
        ])
        .run(tauri::generate_context!())
        .expect("Tauri uygulaması başlatılamadı");
}
