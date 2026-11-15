mod common;

use std::sync::Arc;

use chrono::Utc;
use common::*;
use ultron_core::ipc::{Request, Response};
use ultron_core::{Db, EngineEvent, EngineId, EventLog, SessionRow, SessionStatus};

#[tokio::test]
async fn daemon_restart_restores_sessions() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = paths_in(tmp.path());
    let cwd = abs_dir(tmp.path());
    let pipe = unique_pipe();
    let lines = [STARTED, REPLY, DONE];

    // Bir tur tamamla (tamamlanmış oturumlara yeniden başlangıçta dokunulmaz).
    let h = start_with(&paths, &pipe, &lines).await;
    let mut c = connect(&h).await;
    c.send(&send_req(&cwd, Some("sohbet1"), "ilk"))
        .await
        .unwrap();
    while !matches!(next(&mut c).await, Some(Response::Ok { data }) if data["done"] == true) {}
    drop(c);
    h.shutdown().await;

    // "Servis tur ortasında öldü": ikinci sohbette Running oturum, son olay UserText.
    {
        let db = Arc::new(Db::open(&paths.db).unwrap());
        db.insert_chat("sohbet2", None, "", Utc::now()).unwrap();
        db.upsert_session(&SessionRow {
            id: "oturum-2".into(),
            chat_id: "sohbet2".into(),
            engine: EngineId::Claude,
            native_session_id: None,
            status: SessionStatus::Running,
            started_at: Utc::now(),
            ended_at: None,
        })
        .unwrap();
        EventLog::new(db, paths.clone())
            .append(
                "sohbet2",
                Some("oturum-2"),
                EngineEvent::UserText {
                    text: "yarım".into(),
                },
            )
            .unwrap();
    }
    let before = Db::open(&paths.db)
        .unwrap()
        .events_for_chat("sohbet1", None)
        .unwrap();

    let h = start_with(&paths, &pipe, &lines).await;
    let db = Db::open(&paths.db).unwrap();
    let s2 = db.latest_session("sohbet2").unwrap().unwrap();
    assert_eq!(s2.status, SessionStatus::Interrupted);
    let ev2 = db.events_for_chat("sohbet2", None).unwrap();
    assert!(
        ev2.iter()
            .any(|e| e.kind == "error" && e.payload["message"] == "servis yeniden başladı")
    );

    // Tamamlanmış sohbetin olayları Events ile tekrarsız ve aynen okunur.
    let mut c = connect(&h).await;
    c.send(&Request::Events {
        chat_id: Some("sohbet1".into()),
        follow: false,
        after_seq: None,
    })
    .await
    .unwrap();
    let mut seen = Vec::new();
    loop {
        match next(&mut c).await.unwrap() {
            Response::Event(e) => seen.push(e),
            Response::Ok { .. } => break,
            Response::Error { message } => panic!("{message}"),
        }
    }
    assert_eq!(seen, before);
    let mut seqs: Vec<i64> = seen.iter().map(|e| e.seq).collect();
    seqs.dedup();
    assert_eq!(seqs.len(), seen.len(), "tekrarlanan olay var");
    let s1 = db.latest_session("sohbet1").unwrap().unwrap();
    assert_ne!(s1.status, SessionStatus::Interrupted);

    // Yeniden başlayan servis sohbeti kaldığı yerden sürdürür.
    c.send(&send_req(&cwd, Some("sohbet1"), "ikinci"))
        .await
        .unwrap();
    while !matches!(next(&mut c).await, Some(Response::Ok { data }) if data["done"] == true) {}
    h.shutdown().await;
}
