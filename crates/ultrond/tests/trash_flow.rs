//! Onaylı silme yönlendirmesi ve `TrashPut` yetki/yol kısıtları; sentetik dosyalar, gerçek motor yok.
mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{connect, next, paths_in, start_with, unique_pipe};
use ultron_core::EngineId;
use ultron_core::db::approvals::{Approval, ApprovalStatus};
use ultron_core::ipc::{Client, Request, Response};
use ultron_core::policy::{APPROVED_DELETE_REDIRECT, Action, Decision};
use ultron_core::trash::TrashEntry;
use ultron_core::turn_grant;
use ultrond::server::ServerHandle;

const LINES: [&str; 3] = [common::STARTED, common::REPLY, common::DONE];

async fn start(tmp: &Path) -> (ServerHandle, ultron_core::Paths) {
    let paths = paths_in(tmp);
    let h = start_with(&paths, &unique_pipe(), &LINES).await;
    (h, paths)
}

fn session_dir(tmp: &Path, name: &str) -> PathBuf {
    let dir = tmp.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    plain(&std::fs::canonicalize(dir).unwrap())
}

/// Win32 verbatim önekini atar (CLI'ın gönderdiği düz yol biçimi).
fn plain(path: &Path) -> PathBuf {
    PathBuf::from(path.to_string_lossy().trim_start_matches(r"\\?\"))
}

async fn check_and_resolve(h: &ServerHandle, chat: &str, action: Action, approve: bool) -> Decision {
    let mut checker = connect(h).await;
    checker
        .send(&Request::PolicyCheck {
            action,
            chat_id: Some(chat.into()),
            engine: EngineId::Claude,
            wait_secs: 30,
        })
        .await
        .unwrap();
    let mut approver = Client::connect(h.pipe_name(), h.approver_token())
        .await
        .unwrap();
    let row = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            approver
                .send(&Request::ListApprovals {
                    status: Some(ApprovalStatus::Pending),
                    limit: None,
                    cursor: None,
                })
                .await
                .unwrap();
            if let Some(Response::Ok { data }) = next(&mut approver).await {
                let rows: Vec<Approval> =
                    serde_json::from_value(data["approvals"].clone()).unwrap();
                if let Some(row) = rows.into_iter().next() {
                    return row;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("bekleyen onay yok");
    approver
        .send(&Request::ResolveApproval {
            id: row.id,
            approve,
            by: "desktop".into(),
        })
        .await
        .unwrap();
    assert!(matches!(next(&mut approver).await, Some(Response::Ok { .. })));
    let Some(Response::Ok { data }) = next(&mut checker).await else {
        panic!("karar yok")
    };
    serde_json::from_value(data["decision"].clone()).unwrap()
}

async fn trash_list(h: &ServerHandle) -> Vec<TrashEntry> {
    let mut a = Client::connect(h.pipe_name(), h.approver_token())
        .await
        .unwrap();
    a.send(&Request::TrashList).await.unwrap();
    let Some(Response::Ok { data }) = next(&mut a).await else {
        panic!("liste yok")
    };
    serde_json::from_value(data["entries"].clone()).unwrap()
}

/// Tek istek gönderip yanıtın hata olup olmadığını döndürür.
async fn put_fails(h: &ServerHandle, paths: Vec<String>, token: &str) -> bool {
    let mut c = connect(h).await;
    c.send(&Request::TrashPut {
        paths,
        token: token.into(),
    })
    .await
    .unwrap();
    let reply = next(&mut c).await;
    if let Some(Response::Error { message }) = &reply {
        println!("TrashPut reddi: {message}");
    }
    matches!(reply, Some(Response::Error { .. }))
}

fn s(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

#[tokio::test]
async fn approved_delete_is_redirected_never_allowed_or_moved() {
    let tmp = tempfile::tempdir().unwrap();
    let (h, paths) = start(tmp.path()).await;
    ultron_core::Db::open(&paths.db).unwrap().insert_chat("sil", None, "s", chrono::Utc::now()).unwrap();
    let cwd = session_dir(tmp.path(), "proje");
    let file = cwd.join("a.txt");
    std::fs::write(&file, "sentetik").unwrap();
    for command in [
        "rm a.txt".to_string(),
        "rm \"$TARGET\"".to_string(),
        "Remove-Item a.txt,b.txt".to_string(),
        "cmd /c del a.txt".to_string(),
        "powershell -Command 'Remove-`Item (Join-Path . a.txt)'".to_string(),
        "TARGET=a.txt; r''m \"$TARGET\"".to_string(),
        "powershell -NoProfile -EncodedCommand UgBlAG0AbwB2AGUALQBJAHQAZQBtACAAYQAuAHQAeAB0AA==".to_string(),
    ] {
        let action = Action::Shell { command: command.clone(), cwd: cwd.clone() };
        let decision = check_and_resolve(&h, "sil", action, true).await;
        assert_eq!(
            decision,
            Decision::Deny { reason: APPROVED_DELETE_REDIRECT.into() },
            "{command}"
        );
    }
    assert!(file.exists(), "Ultron kendisi taşımamalı");
    assert!(trash_list(&h).await.is_empty());
    h.shutdown().await;
}

#[tokio::test]
async fn denied_delete_keeps_file() {
    let tmp = tempfile::tempdir().unwrap();
    let (h, paths) = start(tmp.path()).await;
    ultron_core::Db::open(&paths.db).unwrap().insert_chat("ret", None, "s", chrono::Utc::now()).unwrap();
    let cwd = session_dir(tmp.path(), "proje");
    let file = cwd.join("c.txt");
    std::fs::write(&file, "sentetik").unwrap();
    let action = Action::Shell { command: "rm c.txt".into(), cwd };
    assert!(matches!(
        check_and_resolve(&h, "ret", action, false).await,
        Decision::Deny { .. }
    ));
    assert!(file.exists());
    h.shutdown().await;
}

#[tokio::test]
async fn trash_put_needs_a_live_turn_token_and_limits_to_its_session() {
    let tmp = tempfile::tempdir().unwrap();
    let (h, paths) = start(tmp.path()).await;
    let cwd_a = session_dir(tmp.path(), "a");
    let cwd_b = session_dir(tmp.path(), "b");
    let inside_a = cwd_a.join("d.txt");
    let inside_b = cwd_b.join("f.txt");
    let outside = tmp.path().join("e.txt");
    for f in [&inside_a, &inside_b, &outside] {
        std::fs::write(f, "sentetik").unwrap();
    }
    let grant_a = turn_grant::issue("sohbet-a", &cwd_a);
    let _grant_b = turn_grant::issue("sohbet-b", &cwd_b);
    let token = grant_a.token().to_string();
    // Başka sohbetin dizini, dışarısı, Ultron ev dizini, belirteç dosyası, kökün kendisi.
    for (paths_arg, label) in [
        (vec![s(&inside_b)], "başka sohbetin dizini"),
        (vec![s(&outside)], "oturum dışı"),
        (vec![s(&paths.root.join("state"))], "Ultron ev dizini"),
        (vec![s(&paths.run.join("approver-token"))], "onaylayıcı belirteci"),
        (vec![s(&cwd_a)], "oturum kökü"),
        (vec![format!(r"\\?\{}", inside_a.display())], "verbatim"),
        (vec!["d.txt".into()], "göreli"),
        (vec![format!("{}\\..\\b\\f.txt", cwd_a.display())], "nokta nokta"),
    ] {
        assert!(put_fails(&h, paths_arg, &token).await, "{label} reddedilmeli");
    }
    // Belirteçsiz, uydurma ve (başka sohbetin id'si gibi) istemci iddiası yetki vermez.
    assert!(put_fails(&h, vec![s(&inside_a)], "").await);
    assert!(put_fails(&h, vec![s(&inside_a)], "sohbet-a").await);
    assert!(inside_a.exists() && inside_b.exists() && outside.exists());
    // Geçerli belirteç + oturum içi yol.
    assert!(!put_fails(&h, vec![s(&inside_a)], &token).await);
    assert!(!inside_a.exists());
    // Tur bitince (belirteç düşünce) aynı belirteç geçersizdir.
    drop(grant_a);
    std::fs::write(&inside_a, "sentetik").unwrap();
    assert!(put_fails(&h, vec![s(&inside_a)], &token).await, "süresi dolmuş belirteç");
    assert!(inside_a.exists());
    // Geri yükleme yalnız onaylayıcıdır.
    let id = trash_list(&h).await[0].id.clone();
    let mut c = connect(&h).await;
    c.send(&Request::TrashRestore { id: id.clone() }).await.unwrap();
    assert!(matches!(next(&mut c).await, Some(Response::Error { .. })));
    h.shutdown().await;
}

#[tokio::test]
async fn trash_put_rejects_links_protected_git_and_allows_own_scratch() {
    let tmp = tempfile::tempdir().unwrap();
    let (h, paths) = start(tmp.path()).await;
    let cwd = session_dir(tmp.path(), "proje");
    let real_in = cwd.join("onemli");
    std::fs::create_dir_all(&real_in).unwrap();
    std::fs::write(real_in.join("x.txt"), "sentetik").unwrap();
    let outside_dir = session_dir(tmp.path(), "disarisi");
    std::fs::write(outside_dir.join("y.txt"), "sentetik").unwrap();
    let junction = |link: &Path, target: &Path| {
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J", &s(link), &s(target)])
            .output()
            .unwrap();
        assert!(status.status.success(), "mklink /J başarısız");
    };
    let link_in = cwd.join("alias-ici");
    let link_out = cwd.join("alias-disi");
    junction(&link_in, &real_in);
    junction(&link_out, &outside_dir);
    let git_dir = cwd.join(".git");
    std::fs::create_dir_all(&git_dir).unwrap();
    std::fs::write(git_dir.join("config"), "sentetik").unwrap();
    let guarded = cwd.join("finans");
    std::fs::create_dir_all(&guarded).unwrap();
    std::fs::write(guarded.join("k.txt"), "sentetik").unwrap();
    std::fs::write(
        paths.root.join("policy.toml"),
        format!("protected_paths = ['{}']\n", guarded.display().to_string().replace('\\', "/")),
    )
    .unwrap();
    let grant = turn_grant::issue("sohbet-l", &cwd);
    let token = grant.token().to_string();
    for (arg, label) in [
        (s(&link_in), "içeri bağlayan junction"),
        (s(&link_out), "dışarı bağlayan junction"),
        (s(&link_in.join("x.txt")), "junction üzerinden alt yol"),
        (s(&link_out.join("y.txt")), "dışarıya junction altı"),
        (s(&git_dir), ".git"),
        (s(&git_dir.join("config")), ".git içeriği"),
        (s(&guarded), "korumalı dizin"),
        (s(&guarded.join("k.txt")), "korumalı dizin içeriği"),
    ] {
        assert!(put_fails(&h, vec![arg], &token).await, "{label} reddedilmeli");
    }
    assert!(real_in.join("x.txt").exists() && outside_dir.join("y.txt").exists());
    assert!(git_dir.join("config").exists() && guarded.join("k.txt").exists());
    // Sohbetin kendi geçici çalışma alanı: ev dizini içinde ama izinli (yalnız kendi sohbeti).
    let scratch = paths.root.join("workspaces").join("sohbet-s");
    std::fs::create_dir_all(&scratch).unwrap();
    let scratch = plain(&std::fs::canonicalize(scratch).unwrap());
    std::fs::write(scratch.join("z.txt"), "sentetik").unwrap();
    let own = turn_grant::issue("sohbet-s", &scratch);
    assert!(!put_fails(&h, vec![s(&scratch.join("z.txt"))], own.token()).await);
    assert!(!scratch.join("z.txt").exists());
    // Başka sohbetin scratch'ı bu sohbetin belirteciyle (kökü başka) reddedilir; ev dizininin
    // başka yerleri de (runtime) bu scratch kökünden reddedilir.
    let other_scratch = paths.root.join("workspaces").join("baska");
    std::fs::create_dir_all(&other_scratch).unwrap();
    std::fs::write(other_scratch.join("w.txt"), "sentetik").unwrap();
    assert!(put_fails(&h, vec![s(&other_scratch.join("w.txt"))], own.token()).await);
    assert!(put_fails(&h, vec![s(&paths.root.join("state"))], own.token()).await);
    // Oturum dizini ev dizini içindeyse ama kendi scratch'ı değilse reddedilir.
    let fake = turn_grant::issue("sohbet-x", &paths.root.join("workspaces").join("baska"));
    assert!(put_fails(&h, vec![s(&other_scratch.join("w.txt"))], fake.token()).await);
    drop(grant);
    h.shutdown().await;
}

#[tokio::test]
async fn trash_put_rejects_directories_containing_nested_git() {
    let tmp = tempfile::tempdir().unwrap();
    let (h, _paths) = start(tmp.path()).await;
    let cwd = session_dir(tmp.path(), "proje");
    let nested = cwd.join("paket").join("repo").join(".git");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(nested.join("config"), "sentetik").unwrap();
    let clean = cwd.join("temiz");
    std::fs::create_dir_all(clean.join("alt")).unwrap();
    std::fs::write(clean.join("alt").join("a.txt"), "sentetik").unwrap();
    let grant = turn_grant::issue("sohbet-g", &cwd);
    assert!(put_fails(&h, vec![s(&cwd.join("paket"))], grant.token()).await);
    assert!(nested.join("config").exists());
    // `.git` içermeyen dizin taşınır.
    assert!(!put_fails(&h, vec![s(&clean)], grant.token()).await);
    assert!(!clean.exists());
    h.shutdown().await;
}
