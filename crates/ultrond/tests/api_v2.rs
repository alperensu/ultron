mod common;

use std::path::Path;

use chrono::{Duration, Utc};
use common::*;
use serde_json::Value;
use ultron_core::ipc::{ChatInfo, PolicyArg, ProjectInfo, Request, Response};
use ultron_core::{Db, EngineId, Paths};
use ultrond::server::ServerHandle;

const LINES: [&str; 3] = [STARTED, REPLY, DONE];

fn seed_project(paths: &Paths, id: &str, path: &Path) {
    let db = Db::open(&paths.db).unwrap();
    db.upsert_project(&ProjectInfo {
        id: id.into(),
        name: "Örnek".into(),
        path: path.to_string_lossy().into_owned(),
        aliases: vec!["ornek".into()],
        description: None,
        last_active_at: None,
    })
    .unwrap();
}

async fn ok_data(h: &ServerHandle, req: Request) -> Value {
    let mut c = connect(h).await;
    c.send(&req).await.unwrap();
    match next(&mut c).await.expect("yanıt yok") {
        Response::Ok { data } => data,
        other => panic!("beklenmeyen yanıt: {other:?}"),
    }
}

async fn error_of(h: &ServerHandle, req: Request) -> String {
    let mut c = connect(h).await;
    c.send(&req).await.unwrap();
    match next(&mut c).await.expect("yanıt yok") {
        Response::Error { message } => message,
        other => panic!("hata beklenirdi: {other:?}"),
    }
}

async fn chats(h: &ServerHandle, project: Option<&str>, archived: bool) -> Vec<ChatInfo> {
    let data = ok_data(
        h,
        Request::ListChats {
            project_id: project.map(str::to_string),
            include_archived: archived,
        },
    )
    .await;
    serde_json::from_value(data["chats"].clone()).unwrap()
}

async fn run_send(h: &ServerHandle, req: Request) {
    let mut c = connect(h).await;
    c.send(&req).await.unwrap();
    loop {
        match next(&mut c).await.expect("akış erken kapandı") {
            Response::Ok { data } if data.get("done").is_some() => return,
            Response::Error { message } => panic!("{message}"),
            _ => {}
        }
    }
}

#[tokio::test]
async fn list_chats_orders_pinned_then_recent() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths_in(tmp.path());
    {
        let db = Db::open(&paths.db).unwrap();
        let now = Utc::now();
        db.insert_chat("eski", None, "eski", now - Duration::hours(3))
            .unwrap();
        db.insert_chat("orta", None, "orta", now - Duration::hours(2))
            .unwrap();
        db.insert_chat("yeni", None, "yeni", now - Duration::hours(1))
            .unwrap();
        db.insert_chat("arsiv", None, "arsiv", now).unwrap();
        db.update_chat("eski", None, Some(true), None, None).unwrap();
        db.update_chat("arsiv", None, None, Some(true), None).unwrap();
    }
    let h = start_with(&paths, &unique_pipe(), &LINES).await;

    let ids = |v: Vec<ChatInfo>| v.into_iter().map(|c| c.id).collect::<Vec<_>>();
    assert_eq!(ids(chats(&h, None, false).await), ["eski", "yeni", "orta"]);
    assert_eq!(
        ids(chats(&h, None, true).await),
        ["eski", "arsiv", "yeni", "orta"]
    );
    h.shutdown().await;
}

#[tokio::test]
async fn update_chat_rename_pin_archive() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths_in(tmp.path());
    let project_dir = abs_dir(tmp.path());
    seed_project(&paths, "p1", &project_dir);
    Db::open(&paths.db)
        .unwrap()
        .insert_chat("c1", None, "ilk", Utc::now())
        .unwrap();
    let h = start_with(&paths, &unique_pipe(), &LINES).await;

    let data = ok_data(
        &h,
        Request::UpdateChat {
            chat_id: "c1".into(),
            title: Some("  Yeni ad ".into()),
            pinned: Some(true),
            archived: Some(true),
            project_id: Some(Some("p1".into())),
        },
    )
    .await;
    let chat: ChatInfo = serde_json::from_value(data["chat"].clone()).unwrap();
    assert_eq!(chat.title, "Yeni ad");
    assert!(chat.pinned && chat.archived);
    assert_eq!(chat.project_id.as_deref(), Some("p1"));

    // Projeden ayırma (`Some(None)`) tel üzerinden de çalışır; alan yoksa dokunulmaz.
    let data = ok_data(
        &h,
        Request::UpdateChat {
            chat_id: "c1".into(),
            title: None,
            pinned: Some(false),
            archived: None,
            project_id: Some(None),
        },
    )
    .await;
    let chat: ChatInfo = serde_json::from_value(data["chat"].clone()).unwrap();
    assert_eq!(chat.title, "Yeni ad");
    assert!(!chat.pinned && chat.archived);
    assert_eq!(chat.project_id, None);

    // GetChat: meta + çalışma durumu + son motor.
    let data = ok_data(
        &h,
        Request::GetChat {
            chat_id: "c1".into(),
        },
    )
    .await;
    assert_eq!(data["chat"]["chat"]["id"], "c1");

    let msg = error_of(
        &h,
        Request::UpdateChat {
            chat_id: "yok".into(),
            title: None,
            pinned: Some(true),
            archived: None,
            project_id: None,
        },
    )
    .await;
    assert!(msg.contains("bulunamadı"), "{msg}");
    h.shutdown().await;
}

#[tokio::test]
async fn send_with_project_binds_chat_and_cwd() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths_in(tmp.path());
    let project_dir = abs_dir(tmp.path());
    seed_project(&paths, "p1", &project_dir);
    let h = start_with(&paths, &unique_pipe(), &LINES).await;

    // İstekteki cwd var olmayan bir dizin; tur ancak projenin yolunda çalışırsa başarılı olur.
    let bogus = tmp.path().join("yok-boyle-bir-dizin");
    run_send(
        &h,
        Request::Send {
            chat_id: Some("c-proje".into()),
            cwd: bogus.to_string_lossy().into_owned(),
            engine: Some(EngineId::Claude),
            text: "merhaba".into(),
            policy: PolicyArg::ReadOnly,
            elevated: false,
            project_id: Some("p1".into()),
        },
    )
    .await;

    let in_project = chats(&h, Some("p1"), false).await;
    assert_eq!(in_project.len(), 1);
    assert_eq!(in_project[0].id, "c-proje");

    let msg = error_of(
        &h,
        Request::Send {
            chat_id: None,
            cwd: bogus.to_string_lossy().into_owned(),
            engine: None,
            text: "x".into(),
            policy: PolicyArg::ReadOnly,
            elevated: false,
            project_id: Some("yok".into()),
        },
    )
    .await;
    assert!(msg.contains("proje bulunamadı"), "{msg}");
    h.shutdown().await;
}

#[tokio::test]
async fn auto_title_from_first_message() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths_in(tmp.path());
    let cwd = abs_dir(tmp.path());
    let h = start_with(&paths, &unique_pipe(), &LINES).await;

    let long = "bu uzun bir ilk mesajdır ve başlığa sözcük sınırında kısaltılarak girmelidir kesinlikle";
    run_send(
        &h,
        send_req(&cwd, Some("c-baslik"), &format!("  {long}\nikinci satır")),
    )
    .await;
    run_send(&h, send_req(&cwd, Some("c-baslik"), "sonraki mesaj")).await;

    let list = chats(&h, None, false).await;
    let title = &list[0].title;
    assert!(title.chars().count() <= 60, "{title}");
    assert!(title.starts_with("bu uzun bir ilk mesajdır"), "{title}");
    assert!(long.starts_with(title.as_str()), "{title}");
    assert!(!title.ends_with(' '));
    // Sözcük ortasında kesilmez.
    assert!(
        long.chars().nth(title.chars().count()).is_none_or(|c| c == ' '),
        "{title}"
    );

    // Kullanıcı adlandırınca otomatik başlık bir daha değişmez.
    ok_data(
        &h,
        Request::UpdateChat {
            chat_id: "c-baslik".into(),
            title: Some("Benim adım".into()),
            pinned: None,
            archived: None,
            project_id: None,
        },
    )
    .await;
    run_send(&h, send_req(&cwd, Some("c-baslik"), "bir tur daha")).await;
    assert_eq!(chats(&h, None, false).await[0].title, "Benim adım");
    h.shutdown().await;
}

/// Yeni bir `Request` varyantı eklenince bu eşleme derlenmez; varyant testte ele alınmalı.
fn variant_name(r: &Request) -> &'static str {
    match r {
        Request::Hello { .. } => "hello",
        Request::Send { .. } => "send",
        Request::Switch { .. } => "switch",
        Request::Cancel { .. } => "cancel",
        Request::Events { .. } => "events",
        Request::HistorySearch { .. } => "history_search",
        Request::Status => "status",
        Request::TelegramStatus => "telegram_status",
        Request::TelegramSetToken { .. } => "telegram_set_token",
        Request::TelegramTest => "telegram_test",
        Request::GetTelegramSettings => "get_telegram_settings",
        Request::SetTelegramSettings { .. } => "set_telegram_settings",
        Request::DesktopHeartbeat { .. } => "desktop_heartbeat",
        Request::ListProjects => "list_projects",
        Request::ListChats { .. } => "list_chats",
        Request::GetChat { .. } => "get_chat",
        Request::UpdateChat { .. } => "update_chat",
        Request::GetQuota => "get_quota",
        Request::ImportCatalog { .. } => "import_catalog",
        Request::ImportClaudeHistory { .. } => "import_claude_history",
        Request::ImportProjects { .. } => "import_projects",
        Request::RemoveImportedChats { .. } => "remove_imported_chats",
        Request::Route { .. } => "route",
        Request::SetResetRights { .. } => "set_reset_rights",
        Request::PolicyCheck { .. } => "policy_check",
        Request::ListApprovals { .. } => "list_approvals",
        Request::ResolveApproval { .. } => "resolve_approval",
        Request::TrashList => "trash_list",
        Request::TrashPut { .. } => "trash_put",
        Request::TrashRestore { .. } => "trash_restore",
        Request::TrashPurge { .. } => "trash_purge",
        Request::GetPolicyConfig => "get_policy_config",
        Request::SetPolicyConfig { .. } => "set_policy_config",
        Request::MemoryList { .. } => "memory_list",
        Request::MemoryGet { .. } => "memory_get",
        Request::MemorySearch { .. } => "memory_search",
        Request::MemoryCapture(_) => "memory_capture",
        Request::MemoryUpdate(_) => "memory_update",
        Request::MemoryDelete { .. } => "memory_delete",
        Request::MemorySuggestions { .. } => "memory_suggestions",
        Request::MemoryResolveSuggestion { .. } => "memory_resolve_suggestion",
        Request::MemoryRefresh { .. } => "memory_refresh",
        Request::ApprovalsShadowReport { .. } => "approvals_shadow_report",
    }
}

#[tokio::test]
async fn every_request_variant_is_implemented_or_explicitly_not_yet_wired() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths_in(tmp.path());
    let cwd = abs_dir(tmp.path());
    let empty = tmp.path().join("bos");
    std::fs::create_dir_all(&empty).unwrap();
    // Gerçek ev dizini okunmasın.
    // SAFETY: bu test ikilisinde ortam yalnız burada yazılır.
    unsafe { std::env::set_var("ULTRON_QUOTA_HOME", &empty) };
    let h = start_with(&paths, &unique_pipe(), &LINES).await;
    let requests = vec![
        Request::Hello { token: "x".into() },
        send_req(&cwd, Some("kapsam"), "merhaba"),
        Request::Switch { chat_id: "kapsam".into(), to: EngineId::Claude },
        Request::Cancel { chat_id: "kapsam".into() },
        Request::Events { chat_id: Some("kapsam".into()), follow: false, after_seq: None, limit: None },
        Request::HistorySearch { chat_id: None, query: "merhaba".into(), limit: 5 },
        Request::Status,
        Request::ListProjects,
        Request::ListChats { project_id: None, include_archived: false },
        Request::GetChat { chat_id: "kapsam".into() },
        Request::UpdateChat {
            chat_id: "kapsam".into(),
            title: Some("Ad".into()),
            pinned: None,
            archived: None,
            project_id: None,
        },
        Request::GetQuota,
        Request::ImportCatalog { path: tmp.path().join("yok.md").to_string_lossy().into_owned() },
        Request::ImportClaudeHistory { root: Some(empty.to_string_lossy().into_owned()) },
        Request::ImportProjects { dry_run: true, home: Some(empty.to_string_lossy().into_owned()) },
        Request::RemoveImportedChats { dry_run: true },
        Request::Route { text: "devam".into() },
        Request::SetResetRights { pool: "claude".into(), count: 1 },
        Request::PolicyCheck {
            action: ultron_core::policy::Action::Shell { command: "git status".into(), cwd: cwd.clone() },
            chat_id: None, engine: EngineId::Claude, wait_secs: 0,
        },
        Request::ListApprovals { status: None, limit: None, cursor: None },
        Request::ResolveApproval { id: "olmayan-onay".into(), approve: true, by: "desktop".into() },
        Request::TrashList,
        Request::TrashPut { paths: vec![], token: String::new() },
        Request::TrashRestore { id: "olmayan-cop".into() },
        Request::TrashPurge { id: "olmayan-cop".into() },
        Request::GetPolicyConfig,
        Request::SetPolicyConfig {
            settings: ultron_core::ipc::PolicySettings {
                config: ultron_core::policy::PolicyConfig::default(),
                trash_retention_days: 30,
            },
        },
    ];
    let mut seen = std::collections::BTreeSet::new();
    for req in requests {
        seen.insert(variant_name(&req));
        let mut c = connect(&h).await;
        c.send(&req).await.unwrap();
        // Yanıt Ok/Event ya da tipli bir doğrulama hatası olabilir; "uygulanmadı" olamaz.
        if let Response::Error { message } = next(&mut c).await.expect("yanıt yok") {
            assert!(!message.contains("henüz uygulanmadı"), "{req:?}: {message}");
            assert_ne!(message, "Telegram ağ geçidi henüz bağlanmadı", "{req:?}");
        }
    }
    // Salt okunur DTO'lar Response::Ok.data içinde doğrudan döner.
    seen.insert(variant_name(&Request::TelegramStatus));
    assert_eq!(ok_data(&h, Request::TelegramStatus).await["state"], "missing_allowed_users");
    seen.insert(variant_name(&Request::GetTelegramSettings));
    assert_eq!(ok_data(&h, Request::GetTelegramSettings).await["auto_continue_wait_minutes"], 45);
    let admin = [
        Request::TelegramSetToken { token: "synthetic-token".into() },
        Request::TelegramTest,
        Request::SetTelegramSettings {
            notify: ultron_core::config::NotifyMode::Away,
            auto_continue_enabled: false,
            auto_continue_wait_minutes: 45,
            fallback_order: vec![],
        },
        Request::DesktopHeartbeat { instance_id: "synthetic-instance".into() },
        // Aşama 5a: hafıza istekleri onaylayıcı ister; servis Task 8'de bağlanana kadar
        // onaylayıcıya da açık "henüz bağlanmadı" hatası döner (başarı simüle edilmez).
        Request::MemoryList { cursor: None, limit: None, project: None, memory_type: None },
        Request::MemoryGet { id: ultron_core::memory::MemoryId::new() },
        Request::MemorySearch { query: "sentetik".into(), project: None, limit: None },
        Request::MemoryCapture(ultron_core::memory::MemoryCaptureRequest {
            request_id: "sentetik-istek".into(), text: "sentetik not".into(),
            title: None, project: None, tags: vec![], chat_ref: None,
        }),
        Request::MemoryUpdate(ultron_core::memory::MemoryUpdateRequest {
            request_id: "sentetik-istek".into(), id: ultron_core::memory::MemoryId::new(),
            expected_revision: ultron_core::memory::Revision::FIRST,
            patch: ultron_core::memory::MemoryPatch { title: Some("Sentetik".into()), ..Default::default() },
        }),
        Request::MemoryDelete {
            request_id: "sentetik-istek".into(), id: ultron_core::memory::MemoryId::new(),
            expected_revision: ultron_core::memory::Revision::FIRST,
        },
        Request::MemorySuggestions { status: None, cursor: None, limit: None },
        Request::MemoryResolveSuggestion {
            request_id: "sentetik-istek".into(), id: ultron_core::memory::MemoryId::new(), accept: false,
            expected_candidate_hash: ultron_core::memory::Sha256Hash::of(b"sentetik"),
        },
        Request::MemoryRefresh { request_id: "sentetik-istek".into() },
        Request::ApprovalsShadowReport { since_days: None },
    ];
    for req in admin {
        seen.insert(variant_name(&req));
        assert_eq!(error_of(&h, req).await, "yetkisiz: onaylayıcı rolü gerekli");
    }
    assert_eq!(seen.len(), 43,"her varyant denenmeli: {seen:?}");
    h.shutdown().await;
}

#[tokio::test]
async fn set_reset_rights_does_not_expose_malformed_config_over_ipc() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths_in(tmp.path());
    let h = start_with(&paths, &unique_pipe(), &LINES).await;
    let source = "[telegram]\nallowed_user_ids = [101,,202]\n";
    let file = paths.root.join("config.toml");
    std::fs::write(&file, source).unwrap();
    let message = error_of(&h, Request::SetResetRights { pool: "claude".into(), count: 1 }).await;
    assert_eq!(message, "config.toml çözümlenemedi");
    for private in ["101", "202", "allowed_user_ids", "[telegram]"] {
        assert!(!message.contains(private), "{message}");
    }
    assert_eq!(std::fs::read_to_string(file).unwrap(), source);
    h.shutdown().await;
}
