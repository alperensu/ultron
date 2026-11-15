use std::path::PathBuf;

use serde_json::json;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use ultron_core::config::{EngineConfig, EngineId};
use ultron_core::engine::{CodexEngine, Engine, ToolPolicy, TurnOutcome, TurnRequest, run_turn};
use ultron_core::EngineEvent;

fn cfg() -> EngineConfig {
    EngineConfig {
        id: EngineId::Codex,
        enabled: true,
        binary: "codex".into(),
        model: Some("gpt-6.1-sol".into()),
        effort: Some("medium".into()),
        extra_args: Vec::new(),
    }
}

fn request(policy: ToolPolicy, resume: Option<&str>) -> TurnRequest {
    TurnRequest {
        chat_id: "c1".into(),
        cwd: PathBuf::from("C:/proj"),
        prompt: "Sadece TAMAM yaz.".into(),
        resume_native_id: resume.map(str::to_string),
        tool_policy: policy,
    }
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex").join(name)
}

fn parse_file(eng: &dyn Engine, path: &PathBuf) -> Vec<EngineEvent> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .flat_map(|l| eng.parse_line(l))
        .collect()
}

#[test]
fn codex_command_new_and_resume() {
    let eng = CodexEngine::new(cfg());

    let new = eng.command(&request(ToolPolicy::ReadOnly, None));
    assert_eq!(new.program, "codex");
    assert_eq!(new.args[0], "exec");
    assert!(new.args.contains(&"--json".to_string()));
    let m = new.args.iter().position(|a| a == "-m").unwrap();
    assert_eq!(new.args[m + 1], "gpt-6.1-sol");
    assert!(new.args.contains(&"model_reasoning_effort=medium".to_string()));
    assert!(new.args.contains(&"read-only".to_string()));
    assert!(!new.args.contains(&"resume".to_string()));
    assert_eq!(new.args.last().map(String::as_str), Some("-"));
    assert_eq!(new.stdin.as_deref(), Some("Sadece TAMAM yaz."));

    let resume = eng.command(&request(ToolPolicy::ReadOnly, Some("th-1")));
    assert_eq!(resume.args[0], "exec");
    assert_eq!(resume.args[1], "resume");
    assert_eq!(resume.args[2], "th-1");
    assert!(resume.args.contains(&"--json".to_string()));
    // Devam alt komutu -s kabul etmez; sandbox yapılandırma geçersiz kılmasıyla verilir.
    assert!(!resume.args.contains(&"-s".to_string()));
    assert!(resume.args.contains(&"sandbox_mode=\"read-only\"".to_string()));
}

#[test]
fn codex_full_policy_never_uses_danger() {
    let eng = CodexEngine::new(cfg());
    for policy in [ToolPolicy::ReadOnly, ToolPolicy::Workspace, ToolPolicy::Full] {
        for resume in [None, Some("th-1")] {
            let spec = eng.command(&request(policy, resume));
            assert!(
                !spec.args.iter().any(|a| a.contains("danger")),
                "tehlikeli sandbox kullanıldı: {:?}",
                spec.args
            );
            assert!(
                !spec.args.iter().any(|a| a.contains("bypass")),
                "onay atlama kullanıldı: {:?}",
                spec.args
            );
        }
    }
    let full = eng.command(&request(ToolPolicy::Full, None));
    assert!(full.args.contains(&"workspace-write".to_string()));
}

#[test]
fn codex_parses_session_message_command_events() {
    let eng = CodexEngine::new(cfg());
    let events = parse_file(&eng, &fixture("sample.jsonl"));
    assert_eq!(
        events,
        vec![
            EngineEvent::SessionStarted {
                native_session_id: Some("thread-0001".into()),
            },
            EngineEvent::AssistantText {
                text: "yanit-1".into(),
            },
            EngineEvent::Command {
                command: "cmd-0001".into(),
                exit_code: Some(0),
                output_excerpt: "cikti-0001".into(),
            },
            EngineEvent::FileChanged {
                path: "C:/proj/out.txt".into(),
                change: "add".into(),
            },
            EngineEvent::Usage {
                input_tokens: Some(100),
                output_tokens: Some(12),
                cached_tokens: Some(40),
            },
            EngineEvent::TurnCompleted,
        ]
    );
}

#[test]
fn codex_unknown_event_type_is_preserved() {
    let eng = CodexEngine::new(cfg());
    let events = eng.parse_line(r#"{"type":"brand_new","n":2}"#);
    assert_eq!(
        events,
        vec![EngineEvent::Unknown {
            raw: json!({ "type": "brand_new", "n": 2 }),
        }]
    );
    // Bozuk satır panik yapmaz.
    assert!(matches!(
        eng.parse_line("{yarim")[0],
        EngineEvent::Unknown { .. }
    ));
}

#[test]
fn codex_quota_error_maps_to_quota_exhausted() {
    let eng = CodexEngine::new(cfg());
    let events = parse_file(&eng, &fixture("quota.jsonl"));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, EngineEvent::QuotaExhausted { message, .. } if message.contains("usage limit"))),
        "QuotaExhausted yok: {events:?}"
    );
    assert!(!events.iter().any(|e| matches!(e, EngineEvent::Error { .. })));
}

#[test]
fn codex_model_not_found_is_error_not_quota() {
    // Gerçek yakalamadaki hata biçiminin sentetik karşılığı.
    let eng = CodexEngine::new(cfg());
    let events = parse_file(&eng, &fixture("model_error.jsonl"));
    assert!(
        !events.iter().any(|e| matches!(e, EngineEvent::QuotaExhausted { .. })),
        "model hatası kota sayıldı: {events:?}"
    );
    assert!(events.iter().any(|e| matches!(e, EngineEvent::Error { .. })));
    assert!(!events.iter().any(|e| matches!(e, EngineEvent::TurnCompleted)));
}

#[test]
fn extra_args_precede_stdin_marker() {
    let mut c = cfg();
    c.extra_args = vec!["--profile".into(), "ozel".into()];
    let eng = CodexEngine::new(c);
    let spec = eng.command(&request(ToolPolicy::Workspace, None));
    let pos = spec.args.iter().position(|a| a == "--profile").expect("ek argüman yok");
    assert_eq!(spec.args[pos + 1], "ozel");
    assert_eq!(spec.args.last().map(String::as_str), Some("-"));
}

#[test]
fn codex_item_error_is_not_quota() {
    // Öğe düzeyindeki hata tur sonu değildir; kota mesajı bile olsa Error olmalı.
    let eng = CodexEngine::new(cfg());
    let line = r#"{"type":"item.completed","item":{"id":"item-9","type":"error","message":"You have reached your usage limit"}}"#;
    assert_eq!(
        eng.parse_line(line),
        vec![EngineEvent::Error {
            message: "You have reached your usage limit".into(),
        }]
    );
}

#[test]
fn assistant_text_with_quota_word_is_not_quota() {
    let eng = CodexEngine::new(cfg());
    let line = r#"{"type":"item.completed","item":{"id":"item-3","type":"agent_message","text":"usage limit ve quota exceeded kodunu inceledim, 429 yok"}}"#;
    let events = eng.parse_line(line);
    assert!(!events.iter().any(|e| matches!(e, EngineEvent::QuotaExhausted { .. })), "{events:?}");
    assert!(matches!(events.as_slice(), [EngineEvent::AssistantText { .. }]));
}

#[tokio::test]
#[ignore = "canlı CLI; kota harcar. Yalnız onaylı ve kota <%90 iken: cargo test -- --ignored live_"]
async fn live_codex_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let mut config = cfg();
    config.model = Some("gpt-6-luna".into());
    config.effort = Some("max".into());
    let eng = CodexEngine::new(config);
    let req = TurnRequest {
        chat_id: "live".into(),
        cwd: tmp.path().to_path_buf(),
        prompt: "Sadece TAMAM yaz".into(),
        resume_native_id: None,
        tool_policy: ToolPolicy::ReadOnly,
    };
    let (tx, mut rx) = mpsc::channel(256);
    let outcome = run_turn(&eng, req, tx, CancellationToken::new()).await.unwrap();
    let mut events = Vec::new();
    while let Some(ev) = rx.recv().await {
        events.push(ev);
    }
    assert_eq!(outcome, TurnOutcome::Completed);
    assert!(events.iter().any(|e| matches!(
        e,
        EngineEvent::AssistantText { text } if text.contains("TAMAM")
    )));
    assert!(events.iter().any(|e| matches!(
        e,
        EngineEvent::SessionStarted { native_session_id: Some(_) }
    )));
}
