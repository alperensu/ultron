//! Tur belirteci: motor ortamına ortak başlatma yolundan aktarılır ve olaylarda maskelenir.
//! Süreç ortamını değiştirdiği için tek test fonksiyonu içeren ayrı bir ikilidir.

use std::path::Path;
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use ultron_core::config::EngineConfig;
use ultron_core::engine::{
    AgyEngine, Capabilities, ClaudeEngine, CodexEngine, CommandSpec, Engine, ToolPolicy,
    TurnRequest, run_turn,
};
use ultron_core::turn_grant;
use ultron_core::{Db, EngineEvent, EngineId, EventLog, Paths};

fn cfg(id: EngineId) -> EngineConfig {
    EngineConfig {
        id,
        enabled: true,
        binary: "synthetic-engine".into(),
        model: None,
        effort: None,
        extra_args: Vec::new(),
    }
}

/// Gerçek motorun kimliği ve yetenekleri korunur; ortak başlatma yolu (`run_turn`) denenir, yalnız çalıştırılan komut
/// ortamı yazdıran sentetik bir `cmd` çağrısıyla değiştirilir.
struct EchoEnv(Box<dyn Engine>);

impl Engine for EchoEnv {
    fn id(&self) -> EngineId {
        self.0.id()
    }
    fn capabilities(&self) -> Capabilities {
        self.0.capabilities()
    }
    fn command(&self, req: &TurnRequest) -> CommandSpec {
        CommandSpec {
            program: "cmd".into(),
            args: vec![
                "/C".into(),
                "echo %ULTRON_TURN_TOKEN%&&echo %PATH%".into(),
            ],
            cwd: req.cwd.clone(),
            stdin: None,
        }
    }
    fn parse_line(&self, line: &str) -> Vec<EngineEvent> {
        vec![EngineEvent::AssistantText {
            text: line.trim().to_owned(),
        }]
    }
}

async fn lines(engine: &dyn Engine, chat: &str, cwd: &Path) -> Vec<String> {
    let (tx, mut rx) = mpsc::channel(32);
    let req = TurnRequest {
        chat_id: chat.into(),
        cwd: cwd.to_path_buf(),
        prompt: "sentetik".into(),
        resume_native_id: None,
        tool_policy: ToolPolicy::Workspace,
    };
    run_turn(engine, req, tx, CancellationToken::new())
        .await
        .unwrap();
    let mut out = Vec::new();
    while let Some(event) = rx.recv().await {
        if let EngineEvent::AssistantText { text } = event {
            out.push(text);
        }
    }
    out
}

#[tokio::test]
async fn token_and_path_reach_every_engine_and_events_are_masked() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("ultron");
    // SAFETY: bu ikilide tek test var; ortam yalnız burada yazılır.
    unsafe { std::env::set_var("ULTRON_HOME", &home) };
    let paths = Paths::from_root(home);
    paths.ensure().unwrap();
    let ultron_dir = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .to_string_lossy()
        .to_lowercase();
    let engines: Vec<(&str, Box<dyn Engine>)> = vec![
        ("claude", Box::new(ClaudeEngine::new(cfg(EngineId::Claude)))),
        ("codex", Box::new(CodexEngine::new(cfg(EngineId::Codex)))),
        ("agy", Box::new(AgyEngine::new(cfg(EngineId::Agy)))),
    ];
    for (name, engine) in engines {
        let chat = format!("env-{name}");
        let wrapped = EchoEnv(engine);
        let grant = turn_grant::issue(&chat, tmp.path());
        let out = lines(&wrapped, &chat, tmp.path()).await;
        assert_eq!(out.first().map(String::as_str), Some(grant.token()), "{name}: belirteç");
        assert!(
            out.get(1).is_some_and(|p| p.to_lowercase().starts_with(&ultron_dir)),
            "{name}: PATH başına ultron dizini eklenmeli: {out:?}"
        );
        drop(grant);
        // Tur dışında belirteç verilmez (cmd, tanımsız değişkeni olduğu gibi yazar).
        let out = lines(&wrapped, &chat, tmp.path()).await;
        assert_eq!(out.first().map(String::as_str), Some("%ULTRON_TURN_TOKEN%"), "{name}");
    }

    // N01: etkin belirteç hiçbir olay alanında saklanmaz.
    let db = Arc::new(Db::open(&paths.db).unwrap());
    db.insert_chat("maske", None, "s", chrono::Utc::now()).unwrap();
    let log = EventLog::new(db.clone(), paths);
    let grant = turn_grant::issue("maske", tmp.path());
    let token = grant.token().to_owned();
    for event in [
        EngineEvent::ToolResult {
            tool: "Bash".into(),
            output_excerpt: format!("ULTRON_TURN_TOKEN={token}"),
            is_error: false,
        },
        EngineEvent::AssistantText {
            text: format!("belirteç {token}"),
        },
        EngineEvent::ToolCall {
            tool: "Bash".into(),
            input: serde_json::json!({"command": format!("echo {token}"), "nested": [token.clone()]}),
        },
    ] {
        log.append("maske", None, event).unwrap();
    }
    let stored = db.events_for_chat("maske", None).unwrap();
    assert_eq!(stored.len(), 3);
    for event in &stored {
        assert!(!event.payload.to_string().contains(&token), "{:?}", event.kind);
        assert!(event.payload.to_string().contains("[MASKED]"));
    }
    let transcript = std::fs::read_to_string(
        std::path::Path::new(&std::env::var("ULTRON_HOME").unwrap())
            .join("transcripts")
            .join("maske.jsonl"),
    )
    .unwrap();
    assert!(!transcript.contains(&token));
}

#[tokio::test]
async fn split_token_across_events_is_never_stored_or_broadcast_in_clear() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::from_root(tmp.path().join("ultron"));
    paths.ensure().unwrap();
    let db = Arc::new(Db::open(&paths.db).unwrap());
    db.insert_chat("parca", None, "s", chrono::Utc::now()).unwrap();
    let log = EventLog::new(db.clone(), paths.clone());
    let mut live = log.subscribe();
    let grant = turn_grant::issue("parca", tmp.path());
    let token = grant.token().to_owned();
    let text = format!("baş {token} son");
    // 2 parça ve 3 parça (ikinci sohbet) halinde akıtılır; araya başka olay girer.
    let cuts = [vec![21, text.len()], vec![5, 40, text.len()]];
    for (chat, cut) in ["parca", "parca2"].into_iter().zip(cuts) {
        db.insert_chat(chat, None, "s", chrono::Utc::now()).unwrap();
        let mut start = 0;
        for end in cut {
            log.append(chat, None, EngineEvent::AssistantText { text: text[start..end].to_owned() })
                .unwrap();
            start = end;
        }
        log.append(chat, None, EngineEvent::TurnCompleted).unwrap();
    }
    for chat in ["parca", "parca2"] {
        let stored = db.events_for_chat(chat, None).unwrap();
        let joined: String = stored
            .iter()
            .filter(|e| e.kind == "assistant_text")
            .map(|e| e.payload["text"].as_str().unwrap().to_owned())
            .collect();
        assert!(!joined.contains(&token[..12]), "{chat}: {joined}");
        assert!(joined.starts_with("baş ") && joined.ends_with(" son") && joined.contains("[MASKED]"), "{joined}");
        assert_eq!(stored.last().unwrap().kind, "turn_completed");
        let transcript = std::fs::read_to_string(paths.transcripts.join(format!("{chat}.jsonl"))).unwrap();
        assert!(!transcript.contains(&token[..12]), "{chat}: döküm");
    }
    let mut broadcast = String::new();
    while let Ok(event) = live.try_recv() {
        broadcast.push_str(&event.payload.to_string());
    }
    assert!(!broadcast.contains(&token[..12]), "yayın: {broadcast}");
    assert!(broadcast.contains("[MASKED]"));
}
