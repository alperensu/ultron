use std::path::PathBuf;

use serde_json::json;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use ultron_core::config::{EngineConfig, EngineId};
use ultron_core::engine::agy::finalize;
use ultron_core::engine::{AgyEngine, Engine, ToolPolicy, TurnOutcome, TurnRequest, run_turn};
use ultron_core::EngineEvent;

fn cfg() -> EngineConfig {
    EngineConfig {
        id: EngineId::Agy,
        enabled: true,
        binary: "agy".into(),
        model: Some("gemini-3.8-flash-low".into()),
        effort: None,
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
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/agy").join(name)
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
fn agy_command_uses_stream_json_and_flash() {
    let eng = AgyEngine::new(cfg());
    let spec = eng.command(&request(ToolPolicy::ReadOnly, None));
    assert_eq!(spec.program, "agy");
    assert!(spec.args.contains(&"--output-format".to_string()));
    assert!(spec.args.contains(&"stream-json".to_string()));
    let m = spec.args.iter().position(|a| a == "--model").unwrap();
    assert_eq!(spec.args[m + 1], "gemini-3.8-flash-low");
    let mode = spec.args.iter().position(|a| a == "--mode").unwrap();
    assert_eq!(spec.args[mode + 1], "plan");
    // İstem -p=<istem> olarak son argümandır; stdin kullanılmaz.
    assert_eq!(
        spec.args.last().map(String::as_str),
        Some("-p=Sadece TAMAM yaz.")
    );
    assert_eq!(spec.stdin, None);

    let resume = eng.command(&request(ToolPolicy::Workspace, Some("conv-1")));
    let c = resume.args.iter().position(|a| a == "--conversation").unwrap();
    assert_eq!(resume.args[c + 1], "conv-1");
    assert!(resume.args.contains(&"--sandbox".to_string()));
    let mode = resume.args.iter().position(|a| a == "--mode").unwrap();
    assert_eq!(resume.args[mode + 1], "accept-edits");

    let full = eng.command(&request(ToolPolicy::Full, None));
    assert!(!full.args.contains(&"--sandbox".to_string()));
}

#[test]
fn agy_never_skips_permissions() {
    let eng = AgyEngine::new(cfg());
    for policy in [ToolPolicy::ReadOnly, ToolPolicy::Workspace, ToolPolicy::Full] {
        for resume in [None, Some("conv-1")] {
            let spec = eng.command(&request(policy, resume));
            assert!(
                !spec
                    .args
                    .iter()
                    .any(|a| a.contains("dangerously-skip-permissions")),
                "izin atlama kullanıldı: {:?}",
                spec.args
            );
        }
    }
}

#[test]
fn agy_parses_fixture_events() {
    let eng = AgyEngine::new(cfg());
    let events = parse_file(&eng, &fixture("sample.jsonl"));
    assert_eq!(
        events,
        vec![
            EngineEvent::SessionStarted {
                native_session_id: Some("conv-0001".into()),
            },
            EngineEvent::AssistantText {
                text: "yanit-1".into(),
            },
            EngineEvent::Usage {
                input_tokens: Some(10),
                output_tokens: Some(2),
                cached_tokens: Some(0),
            },
            EngineEvent::TurnCompleted,
        ]
    );
}

#[test]
fn agy_unknown_event_type_is_preserved() {
    let eng = AgyEngine::new(cfg());
    let events = eng.parse_line(r#"{"event":"brand_new","n":3}"#);
    assert_eq!(
        events,
        vec![EngineEvent::Unknown {
            raw: json!({ "event": "brand_new", "n": 3 }),
        }]
    );
    // Bozuk satır panik yapmaz.
    assert!(matches!(
        eng.parse_line("{yarim")[0],
        EngineEvent::Unknown { .. }
    ));
}

#[test]
fn agy_quota_error_maps_to_quota_exhausted() {
    let eng = AgyEngine::new(cfg());
    let events = parse_file(&eng, &fixture("quota.jsonl"));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, EngineEvent::QuotaExhausted { .. })),
        "QuotaExhausted yok: {events:?}"
    );
    assert!(!events.iter().any(|e| matches!(e, EngineEvent::TurnCompleted)));
}

#[test]
fn agy_success_without_output_is_error() {
    // Çıkış kodu 0, metin yok: lane.py'deki "başarılı ama yazmadı" durumu.
    let silent = vec![
        EngineEvent::SessionStarted {
            native_session_id: Some("conv-1".into()),
        },
        EngineEvent::Usage {
            input_tokens: Some(1),
            output_tokens: Some(0),
            cached_tokens: None,
        },
        EngineEvent::TurnCompleted,
    ];
    assert!(matches!(
        finalize(&silent, Some(0)),
        Some(EngineEvent::Error { .. })
    ));

    // Metin varsa hata yok.
    let with_text = vec![EngineEvent::AssistantText {
        text: "TAMAM".into(),
    }];
    assert_eq!(finalize(&with_text, Some(0)), None);

    // Sıfır olmayan çıkış ya da zaten hata/kota olayı: finalize karar vermez.
    assert_eq!(finalize(&silent, Some(1)), None);
    assert_eq!(finalize(&silent, None), None);
    let already_quota = vec![EngineEvent::QuotaExhausted {
        message: "x".into(),
        resets_at: None,
    }];
    assert_eq!(finalize(&already_quota, Some(0)), None);
}

#[tokio::test]
#[ignore = "canlı CLI; kota harcar. Yalnız onaylı: cargo test -- --ignored live_"]
async fn live_agy_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let eng = AgyEngine::new(cfg());
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
