use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use ultron_core::engine::{
    Capabilities, CommandSpec, Engine, FakeEngine, ToolPolicy, TurnOutcome, TurnRequest,
};
use ultron_core::events::{EngineEvent, SwitchReason};
use ultron_core::external;
use ultron_core::{Config, Db, EngineId, EventLog, Paths, SessionManager, SessionStatus};

/// FakeEngine'i sarar; her isteği kaydeder ve kimliğini seçilebilir kılar.
struct RecEngine {
    id: EngineId,
    inner: FakeEngine,
    reqs: Arc<Mutex<Vec<TurnRequest>>>,
}

impl Engine for RecEngine {
    fn id(&self) -> EngineId {
        self.id
    }
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
    fn command(&self, req: &TurnRequest) -> CommandSpec {
        self.reqs.lock().unwrap().push(req.clone());
        self.inner.command(req)
    }
    fn parse_line(&self, line: &str) -> Vec<EngineEvent> {
        self.inner.parse_line(line)
    }
}

type Reqs = Arc<Mutex<Vec<TurnRequest>>>;

struct Env {
    tmp: tempfile::TempDir,
    db: Arc<Db>,
    mgr: Arc<SessionManager>,
    reqs: HashMap<EngineId, Reqs>,
    cwd: PathBuf,
}

/// Her motor için `(kimlik, fixture satırları)` verilir; satırlar `EngineEvent` JSON'udur.
fn env_with(engines: &[(EngineId, Vec<&str>)]) -> Env {
    // SAFETY: testler yalnızca bu değişkeni aynı değerle yazar.
    unsafe { std::env::set_var("ULTRON_FAKE_CLI", env!("CARGO_BIN_EXE_ultron-fake-cli")) };
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::from_root(tmp.path().join("ultron"));
    paths.ensure().unwrap();
    let db = Arc::new(Db::open(&paths.db).unwrap());
    let log = Arc::new(EventLog::new(db.clone(), paths));
    let cwd = tmp.path().join("proje klasörü");
    std::fs::create_dir_all(&cwd).unwrap();
    let mut map: HashMap<EngineId, Arc<dyn Engine>> = HashMap::new();
    let mut reqs = HashMap::new();
    for (id, lines) in engines {
        let fixture = tmp.path().join(format!("{id:?}.jsonl"));
        std::fs::write(&fixture, lines.join("\n") + "\n").unwrap();
        let recorded: Reqs = Arc::default();
        reqs.insert(*id, recorded.clone());
        map.insert(
            *id,
            Arc::new(RecEngine {
                id: *id,
                inner: FakeEngine::from_fixture(&fixture),
                reqs: recorded,
            }),
        );
    }
    let mgr = Arc::new(SessionManager::new(
        db.clone(),
        log,
        Config::default(),
        map,
    ));
    Env {
        tmp,
        db,
        mgr,
        reqs,
        cwd,
    }
}

const STARTED: &str = r#"{"type":"session_started","native_session_id":"nat-1"}"#;
const REPLY: &str = r#"{"type":"assistant_text","text":"anladım"}"#;
const DONE: &str = r#"{"type":"turn_completed"}"#;

fn kinds(db: &Db, chat: &str) -> Vec<String> {
    db.events_for_chat(chat, None)
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect()
}

#[tokio::test]
async fn send_resumes_native_session_on_same_engine() {
    let env = env_with(&[(EngineId::Claude, vec![STARTED, REPLY, DONE])]);
    for text in ["ilk mesaj", "ikinci mesaj"] {
        let out = env
            .mgr
            .send("c1", &env.cwd, EngineId::Claude, text, ToolPolicy::ReadOnly)
            .await
            .unwrap();
        assert_eq!(out, TurnOutcome::Completed);
    }
    let reqs = env.reqs[&EngineId::Claude].lock().unwrap();
    assert_eq!(reqs.len(), 2);
    assert_eq!(reqs[0].resume_native_id, None);
    assert_eq!(reqs[1].resume_native_id.as_deref(), Some("nat-1"));
    // Yeni sohbette devir paketi yok; resume turunda da yalnız kullanıcı metni gider.
    assert_eq!(reqs[0].prompt, "ilk mesaj");
    assert_eq!(reqs[1].prompt, "ikinci mesaj");
    let session = env.db.latest_session("c1").unwrap().unwrap();
    assert_eq!(session.native_session_id.as_deref(), Some("nat-1"));
    assert_eq!(session.status, SessionStatus::Running);
}

#[tokio::test]
async fn switch_emits_event_and_next_send_uses_handoff() {
    let env = env_with(&[
        (EngineId::Claude, vec![STARTED, REPLY, DONE]),
        (EngineId::Codex, vec![REPLY, DONE]),
    ]);
    env.mgr
        .send("c1", &env.cwd, EngineId::Claude, "KARAR: CSV kullan", ToolPolicy::ReadOnly)
        .await
        .unwrap();
    env.mgr
        .switch("c1", EngineId::Codex, SwitchReason::User)
        .await
        .unwrap();

    let switched: Vec<EngineEvent> = env
        .db
        .events_for_chat("c1", None)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "engine_switched")
        .map(|e| serde_json::from_value(e.payload).unwrap())
        .collect();
    assert_eq!(
        switched,
        vec![EngineEvent::EngineSwitched {
            from: EngineId::Claude,
            to: EngineId::Codex,
            reason: SwitchReason::User,
        }]
    );
    assert_eq!(
        env.db.latest_session("c1").unwrap().unwrap().status,
        SessionStatus::Closed
    );

    env.mgr
        .send("c1", &env.cwd, EngineId::Codex, "devam et", ToolPolicy::ReadOnly)
        .await
        .unwrap();
    let reqs = env.reqs[&EngineId::Codex].lock().unwrap();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].resume_native_id, None);
    assert!(reqs[0].prompt.starts_with("[ULTRON DEVİR]"));
    assert!(reqs[0].prompt.contains("KARAR: CSV kullan"));
    assert!(reqs[0].prompt.ends_with("\n\ndevam et"));
}

#[tokio::test]
async fn send_with_different_engine_writes_switch_event() {
    let env = env_with(&[
        (EngineId::Claude, vec![STARTED, REPLY, DONE]),
        (EngineId::Codex, vec![REPLY, DONE]),
    ]);
    env.mgr
        .send("c1", &env.cwd, EngineId::Claude, "bir", ToolPolicy::ReadOnly)
        .await
        .unwrap();
    env.mgr
        .send("c1", &env.cwd, EngineId::Codex, "iki", ToolPolicy::ReadOnly)
        .await
        .unwrap();
    assert!(kinds(&env.db, "c1").contains(&"engine_switched".to_string()));
    assert!(env.reqs[&EngineId::Codex].lock().unwrap()[0]
        .prompt
        .starts_with("[ULTRON DEVİR]"));
}

#[tokio::test]
async fn quota_outcome_sets_waiting_quota() {
    let quota = r#"{"type":"quota_exhausted","message":"limit doldu","resets_at":null}"#;
    let env = env_with(&[(EngineId::Claude, vec![STARTED, quota])]);
    let out = env
        .mgr
        .send("c1", &env.cwd, EngineId::Claude, "merhaba", ToolPolicy::ReadOnly)
        .await
        .unwrap();
    assert_eq!(out, TurnOutcome::QuotaExhausted);
    assert_eq!(
        env.db.latest_session("c1").unwrap().unwrap().status,
        SessionStatus::WaitingQuota
    );
    assert!(kinds(&env.db, "c1").contains(&"quota_exhausted".to_string()));
}

#[tokio::test]
async fn agy_silent_success_becomes_error() {
    let env = env_with(&[(EngineId::Agy, vec![STARTED, DONE])]);
    let out = env
        .mgr
        .send("c1", &env.cwd, EngineId::Agy, "merhaba", ToolPolicy::ReadOnly)
        .await
        .unwrap();
    assert!(matches!(out, TurnOutcome::Failed { .. }));
    assert!(kinds(&env.db, "c1").contains(&"error".to_string()));
    assert_eq!(
        env.db.latest_session("c1").unwrap().unwrap().status,
        SessionStatus::Failed
    );
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

#[tokio::test]
async fn external_changes_detects_user_edit() {
    let touched = r#"{"type":"file_changed","path":"b.txt","change":"modified"}"#;
    let env = env_with(&[(EngineId::Claude, vec![STARTED, touched, REPLY, DONE])]);
    git(&env.cwd, &["init"]);
    std::fs::write(env.cwd.join("a.txt"), "bir").unwrap();
    std::fs::write(env.cwd.join("b.txt"), "bir").unwrap();
    git(&env.cwd, &["add", "."]);
    git(&env.cwd, &["commit", "-m", "ilk"]);

    let since = Utc::now();
    tokio::time::sleep(Duration::from_millis(50)).await;
    // Kullanıcı a.txt'yi, Ultron'un dokunduğu b.txt'yi değiştirir.
    std::fs::write(env.cwd.join("a.txt"), "kullanıcı düzenledi").unwrap();
    std::fs::write(env.cwd.join("b.txt"), "ultron düzenledi").unwrap();
    let exclude = BTreeSet::from(["b.txt".to_string()]);
    let summary = external::changes_since(&env.cwd, since, None, &exclude)
        .unwrap()
        .expect("değişiklik bekleniyordu");
    assert!(summary.contains("a.txt"), "{summary}");
    assert!(!summary.contains("b.txt"), "{summary}");

    // Yönetici üzerinden: önceki turda dokunulan dosyaya kullanıcı düzenlemesi de raporlanır
    // (ayrıntı: `user_edit_to_previously_touched_file_is_reported`).
    env.mgr
        .send("c1", &env.cwd, EngineId::Claude, "başla", ToolPolicy::Workspace)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    std::fs::write(env.cwd.join("a.txt"), "ikinci kullanıcı düzenlemesi").unwrap();
    std::fs::write(env.cwd.join("b.txt"), "ikinci ultron düzenlemesi").unwrap();
    env.mgr
        .send("c1", &env.cwd, EngineId::Claude, "devam", ToolPolicy::Workspace)
        .await
        .unwrap();
    let ext = env
        .db
        .events_for_chat("c1", None)
        .unwrap()
        .into_iter()
        .find(|e| e.kind == "external_changes")
        .expect("external_changes olayı");
    let text = ext.payload["summary"].as_str().unwrap();
    assert!(text.contains("a.txt"), "{text}");
    // Resume turunda özet kullanıcı metninin önüne eklenir.
    let reqs = env.reqs[&EngineId::Claude].lock().unwrap();
    assert!(reqs[1].prompt.contains("a.txt") && reqs[1].prompt.ends_with("devam"));
    // Son görülen HEAD sohbet başına saklanır.
    assert!(env.db.chat_last_head("c1").unwrap().is_some());
    drop(env.tmp);
}

#[tokio::test]
async fn second_send_while_running_is_rejected() {
    // `hold` kipi 120 sn uyur; çalışan tur iptal edilene dek sürer.
    let mut env = env_with(&[(EngineId::Claude, vec![STARTED])]);
    let hold = FakeEngine::with_mode(&["hold"], false);
    let mut map: HashMap<EngineId, Arc<dyn Engine>> = HashMap::new();
    map.insert(EngineId::Claude, Arc::new(hold));
    let paths = Paths::from_root(env.tmp.path().join("ultron2"));
    paths.ensure().unwrap();
    let db = Arc::new(Db::open(&paths.db).unwrap());
    let log = Arc::new(EventLog::new(db.clone(), paths));
    env.mgr = Arc::new(SessionManager::new(db.clone(), log, Config::default(), map));

    let mgr = env.mgr.clone();
    let cwd = env.cwd.clone();
    let first = tokio::spawn(async move {
        mgr.send("c1", &cwd, EngineId::Claude, "uzun iş", ToolPolicy::ReadOnly)
            .await
    });
    for _ in 0..100 {
        if !db.open_sessions().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let err = env
        .mgr
        .send("c1", &env.cwd, EngineId::Claude, "ikinci", ToolPolicy::ReadOnly)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("sohbette çalışan bir tur var"), "{err}");
    // Başka sohbet etkilenmez; çalışan sohbette switch de reddedilir.
    assert!(
        env.mgr
            .switch("c1", EngineId::Codex, SwitchReason::User)
            .await
            .is_err()
    );

    env.mgr.cancel("c1").await.unwrap();
    let out = first.await.unwrap().unwrap();
    assert_eq!(out, TurnOutcome::Cancelled);
    assert_eq!(
        db.latest_session("c1").unwrap().unwrap().status,
        SessionStatus::Interrupted
    );
    // Kilit serbest kaldı.
    assert!(env.mgr.cancel("c1").await.is_err());
}

#[tokio::test]
async fn user_edit_to_previously_touched_file_is_reported() {
    let touched = r#"{"type":"file_changed","path":"b.txt","change":"modified"}"#;
    let env = env_with(&[(EngineId::Claude, vec![STARTED, touched, REPLY, DONE])]);
    git(&env.cwd, &["init"]);
    std::fs::write(env.cwd.join("b.txt"), "bir").unwrap();
    git(&env.cwd, &["add", "."]);
    git(&env.cwd, &["commit", "-m", "ilk"]);
    env.mgr
        .send("c1", &env.cwd, EngineId::Claude, "başla", ToolPolicy::Workspace)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    // b.txt önceki turda Ultron tarafından değiştirilmişti; şimdi kullanıcı yeniden düzenliyor.
    std::fs::write(env.cwd.join("b.txt"), "kullanıcı düzenledi").unwrap();
    env.mgr
        .send("c1", &env.cwd, EngineId::Claude, "devam", ToolPolicy::Workspace)
        .await
        .unwrap();
    let ext = env
        .db
        .events_for_chat("c1", None)
        .unwrap()
        .into_iter()
        .find(|e| e.kind == "external_changes")
        .expect("external_changes olayı");
    assert!(ext.payload["summary"].as_str().unwrap().contains("b.txt"));
}
