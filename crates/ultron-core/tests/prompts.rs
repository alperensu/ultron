use std::path::PathBuf;
use std::sync::Arc;

use ultron_core::config::{EngineConfig, EngineId};
use ultron_core::engine::{AgyEngine, ClaudeEngine, CodexEngine, ToolPolicy, TurnRequest};
use ultron_core::prompts::{
    Role, WorkMode, compose, compose_from_home, with_instructions, work_mode,
};

fn config(id: EngineId) -> EngineConfig {
    EngineConfig {
        id,
        enabled: true,
        binary: "synthetic-engine".into(),
        model: None,
        effort: None,
        extra_args: Vec::new(),
    }
}

fn request() -> TurnRequest {
    TurnRequest {
        chat_id: "synthetic-chat".into(),
        cwd: PathBuf::from("synthetic-project"),
        prompt: "Kullanıcının özgün isteği".into(),
        resume_native_id: Some("synthetic-native".into()),
        tool_policy: ToolPolicy::ReadOnly,
    }
}

#[test]
fn compose_orders_stable_first() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("AGENTS.md");
    std::fs::write(&project, "SENTETİK PROJE TALİMATI").unwrap();
    let text = compose(Role::Brain, WorkMode::General, &[project]);
    let identity = text.find("Sen Ultron'sun").unwrap();
    let rules = text.find("DEĞİŞMEZ KURALLAR").unwrap();
    let principles = text.find("ÇALIŞMA İLKELERİ").unwrap();
    let role = text.find("ROL: ŞEF").unwrap();
    let mode = text.find("GENEL MOD").unwrap();
    let project = text.find("SENTETİK PROJE TALİMATI").unwrap();
    assert!(identity < rules && rules < principles && principles < role);
    assert!(role < mode && mode < project);
}

#[test]
fn worker_prompt_has_no_core_card_placeholder_leak() {
    let text = compose(Role::Worker, WorkMode::Code, &[]);
    assert!(text.contains("ROL: İŞÇİ"));
    assert!(text.contains("alt ajan açma"));
    for forbidden in ["{{", "}}", "CORE_CARD", "ÇEKİRDEK KART", "TODO"] {
        assert!(!text.contains(forbidden), "sızan yer tutucu: {forbidden}");
    }
    assert!(!text.contains("GENEL MOD"));
}

#[test]
fn claude_command_appends_prompt_file() {
    let tmp = tempfile::tempdir().unwrap();
    let instructions = compose(Role::Brain, WorkMode::General, &[]);
    let engine = with_instructions(
        Arc::new(ClaudeEngine::new(config(EngineId::Claude))),
        &instructions,
        tmp.path(),
        "synthetic-chat",
    )
    .unwrap();
    let req = request();
    let command = engine.command(&req);
    let index = command
        .args
        .iter()
        .position(|s| s == "--append-system-prompt-file")
        .unwrap();
    let path = PathBuf::from(&command.args[index + 1]);
    assert_eq!(path, tmp.path().join("prompt-synthetic-chat.md"));
    assert_eq!(std::fs::read_to_string(path).unwrap(), instructions);
    assert_eq!(command.stdin.as_deref(), Some(req.prompt.as_str()));
    assert!(command.args.contains(&"--resume".into()));
    assert!(!command.args.contains(&"--system-prompt".into()));
    assert!(command.args.contains(&"--settings".into()));
    assert!(command.args.contains(&"--include-hook-events".into()));
}

#[test]
fn codex_prompt_prefixed_not_replaced() {
    let tmp = tempfile::tempdir().unwrap();
    let instructions = compose(Role::Brain, WorkMode::General, &[]);
    let engine = with_instructions(
        Arc::new(CodexEngine::new(config(EngineId::Codex))),
        &instructions,
        tmp.path(),
        "synthetic-chat",
    )
    .unwrap();
    let req = request();
    let command = engine.command(&req);
    let prompt = command.stdin.unwrap();
    assert!(prompt.starts_with("[ULTRON TALİMATLARI]\n"));
    assert!(prompt.contains(&instructions));
    assert!(prompt.ends_with(&req.prompt));
    assert!(command.args.contains(&"resume".into()));
    assert!(
        !command
            .args
            .iter()
            .any(|a| a.contains("developer_instructions") || a.contains("base_instructions"))
    );
}

#[test]
fn local_overrides_replace_only_their_card_and_missing_cards_fall_back() {
    let tmp = tempfile::tempdir().unwrap();
    let cards = tmp.path().join("prompts");
    std::fs::create_dir(&cards).unwrap();
    std::fs::write(cards.join("role-worker.md"), "YEREL İŞÇİ").unwrap();
    std::fs::write(cards.join("role-reviewer.md"), "YEREL İNCELEYİCİ").unwrap();
    std::fs::write(cards.join("mode-general.md"), "YEREL GENEL").unwrap();
    let text = compose_from_home(Some(tmp.path()), Role::Worker, WorkMode::General, &[]);
    assert!(text.starts_with("Sen Ultron'sun"));
    assert!(text.contains("YEREL İŞÇİ"));
    assert!(text.ends_with("YEREL GENEL"));
    assert!(!text.contains("ROL: İŞÇİ"));
    assert!(
        compose_from_home(Some(tmp.path()), Role::Reviewer, WorkMode::Code, &[])
            .contains("YEREL İNCELEYİCİ")
    );
    // Okunamayan UTF-8 kartı da gömülü karşılığına düşer.
    std::fs::write(cards.join("core.md"), [0xff]).unwrap();
    assert!(
        compose_from_home(Some(tmp.path()), Role::Delegate, WorkMode::Code, &[])
            .contains("ROL: TEMSİLCİ")
    );
}

#[test]
fn code_mode_requires_project_binding_and_source_files() {
    let tmp = tempfile::tempdir().unwrap();
    assert_eq!(work_mode(true, tmp.path()), WorkMode::General);
    std::fs::write(tmp.path().join("notes.md"), "sentetik").unwrap();
    assert_eq!(work_mode(true, tmp.path()), WorkMode::General);
    let source = tmp.path().join("crates/example/src/deep");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("lib.rs"), "// sentetik").unwrap();
    assert_eq!(work_mode(false, tmp.path()), WorkMode::General);
    assert_eq!(work_mode(true, tmp.path()), WorkMode::Code);
}

#[test]
fn agy_preserves_instructions_and_user_tail_within_utf16_limit() {
    let tmp = tempfile::tempdir().unwrap();
    let instructions = compose(Role::Brain, WorkMode::General, &[]);
    let engine = with_instructions(
        Arc::new(AgyEngine::new(config(EngineId::Agy))),
        &instructions,
        tmp.path(),
        "synthetic-chat",
    )
    .unwrap();
    let mut req = request();
    req.prompt = format!("{}SON İSTEK", "🦀".repeat(20_000));
    let command = engine.command(&req);
    let arg = command.args.last().unwrap();
    assert!(arg.starts_with("-p=[ULTRON TALİMATLARI]\n"));
    assert!(arg.contains(&instructions));
    assert!(arg.contains("[KIRPILDI:"));
    assert!(arg.ends_with("SON İSTEK"));
    assert!(arg.encode_utf16().count() <= 30_000);
    assert!(command.stdin.is_none());
    assert!(!tmp.path().join("prompt-synthetic-chat.md").exists());
}

#[test]
fn invalid_chat_id_and_unwritable_prompt_fail_before_launch() {
    let tmp = tempfile::tempdir().unwrap();
    let engine = Arc::new(ClaudeEngine::new(config(EngineId::Claude)));
    for id in ["", "../escape", "a/b", "a\\b"] {
        assert!(with_instructions(engine.clone(), "sentetik", tmp.path(), id).is_err());
    }
    let file = tmp.path().join("file");
    std::fs::write(&file, "sentetik").unwrap();
    assert!(with_instructions(engine, "sentetik", &file, "safe").is_err());
    assert!(
        with_instructions(
            Arc::new(AgyEngine::new(config(EngineId::Agy))),
            &"x".repeat(30_000),
            tmp.path(),
            "safe",
        )
        .is_err()
    );
}

#[tokio::test]
async fn session_wires_brain_prompt_and_reselects_mode_when_project_is_bound() {
    use chrono::Utc;
    use std::collections::HashMap;
    use ultron_core::engine::{Engine, FakeEngine, TurnOutcome};
    use ultron_core::ipc::ProjectInfo;
    use ultron_core::{Config, Db, EngineId, EventLog, Paths, SessionManager};

    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::from_root(tmp.path().join("ultron"));
    paths.ensure().unwrap();
    let db = Arc::new(Db::open(&paths.db).unwrap());
    let log = Arc::new(EventLog::new(db.clone(), paths));
    let cwd = tmp.path().join("project");
    std::fs::create_dir(&cwd).unwrap();
    std::fs::write(cwd.join("main.rs"), "// sentetik").unwrap();
    std::fs::write(cwd.join("AGENTS.md"), "SENTETİK PROJE KURALI").unwrap();
    let fixture = tmp.path().join("reply.jsonl");
    std::fs::write(&fixture, "{\"type\":\"turn_completed\"}\n").unwrap();
    let engines: HashMap<EngineId, Arc<dyn Engine>> = HashMap::from([(
        EngineId::Claude,
        Arc::new(FakeEngine::from_fixture(&fixture)) as Arc<dyn Engine>,
    )]);
    let manager = SessionManager::new(db.clone(), log, Config::default(), engines);
    let chat = "synthetic-prompt-wiring";
    let prompt_file = Paths::resolve()
        .unwrap()
        .run
        .join(format!("prompt-{chat}.md"));
    assert_eq!(
        manager
            .send(
                chat,
                &cwd,
                EngineId::Claude,
                "ilk istek",
                ToolPolicy::ReadOnly
            )
            .await
            .unwrap(),
        TurnOutcome::Completed
    );
    let first = std::fs::read_to_string(&prompt_file).unwrap();
    assert!(first.contains("ROL: ŞEF"));
    assert!(first.contains("GENEL MOD"));
    assert!(!first.contains("SENTETİK PROJE KURALI"));

    db.upsert_project(&ProjectInfo {
        id: "synthetic-project".into(),
        name: "Sentetik proje".into(),
        path: cwd.to_string_lossy().into_owned(),
        aliases: Vec::new(),
        description: None,
        last_active_at: Some(Utc::now().to_rfc3339()),
    })
    .unwrap();
    db.bind_chat_project(chat, "synthetic-project").unwrap();
    assert_eq!(
        manager
            .send(
                chat,
                &cwd,
                EngineId::Claude,
                "ikinci istek",
                ToolPolicy::ReadOnly
            )
            .await
            .unwrap(),
        TurnOutcome::Completed
    );
    let second = std::fs::read_to_string(prompt_file).unwrap();
    assert!(second.contains("ROL: ŞEF"));
    assert!(!second.contains("GENEL MOD"));
    assert!(second.contains("SENTETİK PROJE KURALI"));
    let messages = db.events_for_chat(chat, None).unwrap();
    assert!(
        messages
            .iter()
            .filter(|event| event.kind == "user_text")
            .all(|event| !event.payload.to_string().contains("ULTRON TALİMATLARI"))
    );
}

#[tokio::test]
#[ignore = "canlı Claude CLI; kota harcar, yalnız açık kullanıcı onayıyla çalıştırılır"]
async fn live_output_style_probe() {
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;
    use ultron_core::engine::{Engine, TurnOutcome, run_turn};
    use ultron_core::events::EngineEvent;

    let tmp = tempfile::tempdir().unwrap();
    let styles = tmp.path().join(".claude/output-styles");
    std::fs::create_dir_all(&styles).unwrap();
    std::fs::write(styles.join("ultron-genel.md"),
        "---\nname: ultron-genel\ndescription: Ultron genel asistan sondajı\nkeep-coding-instructions: false\n---\n\nGenel asistansın. Kodlama dışı istekte test veya commit önerme. Yanıtında ULTRON_GENEL_PROBE işaretini kullan.\n"
    ).unwrap();
    let mut cfg = config(EngineId::Claude);
    cfg.binary = "claude".into();
    cfg.extra_args = vec![
        "--settings".into(),
        r#"{"outputStyle":"ultron-genel"}"#.into(),
    ];
    let engine = ClaudeEngine::new(cfg);
    let mut req = request();
    req.cwd = tmp.path().to_path_buf();
    req.resume_native_id = None;
    req.prompt = "Kısa bir çay molası öner. Test veya commit hakkında konuşma.".into();
    assert!(engine.command(&req).args.contains(&"--settings".into()));
    let (tx, mut rx) = mpsc::channel(256);
    let cancel = CancellationToken::new();
    let run = run_turn(&engine, req, tx, cancel.clone());
    let collect = async move {
        let mut text = String::new();
        while let Some(event) = rx.recv().await {
            if let EngineEvent::AssistantText { text: delta } = event {
                text.push_str(&delta);
            }
        }
        text
    };
    let operation = async { tokio::join!(run, collect) };
    tokio::pin!(operation);
    let (outcome, text) = tokio::select! {
        result = &mut operation => result,
        () = tokio::time::sleep(std::time::Duration::from_secs(60)) => {
            cancel.cancel();
            operation.await
        },
    };
    assert_eq!(outcome.unwrap(), TurnOutcome::Completed);
    let applied = text.contains("ULTRON_GENEL_PROBE");
    eprintln!(
        "Çıktı stili davranış sinyali: {applied}; kodlama talimatlarının gerçekten kaldırıldığı bu davranış sondajıyla kanıtlanamaz."
    );
    assert!(applied, "çıktı stili işareti gözlenmedi");
}
