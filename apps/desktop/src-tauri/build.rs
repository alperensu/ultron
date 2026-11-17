//! Uygulama komutları açıkça listelenir; capabilities yalnız `allow-<komut>` izinlerini verir.
fn main() {
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(
        tauri_build::AppManifest::new().commands(&[
            "list_approvals",
            "resolve_approval",
            "list_trash",
            "restore_trash",
            "purge_trash",
            "get_policy_config",
            "set_policy_config",
            "list_projects",
            "list_chats",
            "get_chat",
            "update_chat",
            "send_message",
            "switch_engine",
            "cancel_turn",
            "events_page",
            "history_search",
            "get_quota",
            "set_reset_rights",
            "route_message",
            "import_catalog",
            "import_projects",
            "status",
        ]),
    ))
    .expect("tauri derleme betiği başarısız");
}
