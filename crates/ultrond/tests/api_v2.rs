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

#[tokio::test]
async fn later_task_requests_report_not_implemented() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths_in(tmp.path());
    let h = start_with(&paths, &unique_pipe(), &LINES).await;
    let msg = error_of(&h, Request::Route { text: "x".into() }).await;
    assert!(msg.contains("henüz uygulanmadı"), "{msg}");
    h.shutdown().await;
}
