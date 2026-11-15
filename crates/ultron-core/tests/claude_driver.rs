use std::path::PathBuf;

use chrono::{TimeZone, Utc};
use serde_json::json;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use ultron_core::config::{EngineConfig, EngineId};
use ultron_core::engine::{ClaudeEngine, Engine, ToolPolicy, TurnOutcome, TurnRequest, run_turn};
use ultron_core::EngineEvent;

fn cfg() -> EngineConfig {
    EngineConfig {
        id: EngineId::Claude,
        enabled: true,
        binary: "claude".into(),
        model: None,
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
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude").join(name)
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
fn claude_command_includes_stream_json_and_resume() {
    let eng = ClaudeEngine::new(cfg());
    let spec = eng.command(&request(ToolPolicy::ReadOnly, Some("abc")));
    assert_eq!(spec.program, "claude");
    assert!(spec.args.contains(&"--output-format".to_string()));
    assert!(spec.args.contains(&"stream-json".to_string()));
    assert!(spec.args.contains(&"--resume".to_string()));
    assert!(spec.args.contains(&"abc".to_string()));
    assert!(spec.args.contains(&"--strict-mcp-config".to_string()));
    assert_eq!(spec.stdin.as_deref(), Some("Sadece TAMAM yaz."));
    assert_eq!(spec.cwd, PathBuf::from("C:/proj"));
}

#[test]
fn claude_readonly_policy_limits_tools() {
    let eng = ClaudeEngine::new(cfg());
    let spec = eng.command(&request(ToolPolicy::ReadOnly, None));
    let pos = spec
        .args
        .iter()
        .position(|a| a == "--tools")
        .expect("--tools bayrağı yok");
    assert_eq!(spec.args[pos + 1], "Read,Grep,Glob");
    assert!(!spec.args.iter().any(|a| a.contains("Bash")));
    assert!(!spec.args.contains(&"--resume".to_string()));
}

#[test]
fn claude_workspace_policy_denies_destructive_commands() {
    let eng = ClaudeEngine::new(cfg());
    let spec = eng.command(&request(ToolPolicy::Workspace, None));
    let pos = spec
        .args
        .iter()
        .position(|a| a == "--disallowedTools")
        .expect("--disallowedTools bayrağı yok");
    assert!(spec.args[pos + 1].contains("Bash(rm:*)"));
    assert!(spec.args[pos + 1].contains("Bash(git push:*)"));
    let mode = spec.args.iter().position(|a| a == "--permission-mode").unwrap();
    assert_eq!(spec.args[mode + 1], "acceptEdits");
}

#[test]
fn claude_parses_init_assistant_tool_result() {
    let eng = ClaudeEngine::new(cfg());
    let events = parse_file(&eng, &fixture("sample.jsonl"));
    assert_eq!(
        events,
        vec![
            EngineEvent::SessionStarted {
                native_session_id: Some("sess-0001".into()),
            },
            EngineEvent::AssistantText {
                text: "yanit-1".into(),
            },
            EngineEvent::ToolCall {
                tool: "Read".into(),
                input: json!({ "file_path": "C:/proj/notes.txt" }),
            },
            EngineEvent::ToolResult {
                tool: "toolu-0001".into(),
                output_excerpt: "yanit-2".into(),
                is_error: false,
            },
            EngineEvent::Usage {
                input_tokens: Some(20),
                output_tokens: Some(6),
                cached_tokens: Some(8),
            },
            EngineEvent::TurnCompleted,
        ]
    );
}

#[test]
fn claude_edit_tool_emits_file_changed() {
    let eng = ClaudeEngine::new(cfg());
    let edit = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu-9","name":"Edit","input":{"file_path":"C:/proj/a.txt","old_string":"x","new_string":"y"}}]}}"#;
    let write = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu-10","name":"Write","input":{"file_path":"C:/proj/b.txt","content":"z"}}]}}"#;
    let notebook = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu-11","name":"NotebookEdit","input":{"notebook_path":"C:/proj/n.ipynb","new_source":"q"}}]}}"#;
    let read = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu-12","name":"Read","input":{"file_path":"C:/proj/c.txt"}}]}}"#;

    let events = eng.parse_line(edit);
    assert_eq!(
        events,
        vec![
            EngineEvent::FileChanged {
                path: "C:/proj/a.txt".into(),
                change: "edit".into(),
            },
            EngineEvent::ToolCall {
                tool: "Edit".into(),
                input: json!({ "file_path": "C:/proj/a.txt", "old_string": "x", "new_string": "y" }),
            },
        ]
    );
    assert!(matches!(
        eng.parse_line(write).first(),
        Some(EngineEvent::FileChanged { path, change }) if path == "C:/proj/b.txt" && change == "write"
    ));
    assert!(matches!(
        eng.parse_line(notebook).first(),
        Some(EngineEvent::FileChanged { path, change }) if path == "C:/proj/n.ipynb" && change == "edit"
    ));
    assert!(
        !eng.parse_line(read).iter().any(|e| matches!(e, EngineEvent::FileChanged { .. })),
        "Read dosya değişikliği sayılmamalı"
    );
}

#[test]
fn assistant_text_with_quota_word_is_not_quota() {
    let eng = ClaudeEngine::new(cfg());
    let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"usage limit ve quota exceeded kodunu inceledim, 429 yok"}]}}"#;
    let events = eng.parse_line(line);
    assert!(!events.iter().any(|e| matches!(e, EngineEvent::QuotaExhausted { .. })), "{events:?}");
    assert!(matches!(events.as_slice(), [EngineEvent::AssistantText { .. }]));
}

#[test]
fn claude_unknown_event_type_is_preserved() {
    let eng = ClaudeEngine::new(cfg());
    let line = r#"{"type":"brand_new","detail":{"x":1}}"#;
    let events = eng.parse_line(line);
    assert_eq!(
        events,
        vec![EngineEvent::Unknown {
            raw: json!({ "type": "brand_new", "detail": { "x": 1 } }),
        }]
    );
}

#[test]
fn claude_malformed_line_is_unknown_not_panic() {
    let eng = ClaudeEngine::new(cfg());
    let events = eng.parse_line("{bozuk json");
    assert_eq!(
        events,
        vec![EngineEvent::Unknown {
            raw: json!("{bozuk json"),
        }]
    );
}

#[test]
fn claude_quota_error_maps_to_quota_exhausted() {
    let eng = ClaudeEngine::new(cfg());
    let events = parse_file(&eng, &fixture("quota.jsonl"));
    let quota = events
        .iter()
        .find(|e| matches!(e, EngineEvent::QuotaExhausted { .. }))
        .unwrap_or_else(|| panic!("QuotaExhausted yok: {events:?}"));
    let EngineEvent::QuotaExhausted { message, resets_at } = quota else {
        unreachable!()
    };
    assert!(message.to_lowercase().contains("usage limit"));
    assert_eq!(*resets_at, Utc.timestamp_opt(1_700_000_000, 0).single());
    assert!(!events.iter().any(|e| matches!(e, EngineEvent::Error { .. })));
}

#[test]
fn claude_rejected_rate_limit_event_is_quota() {
    let eng = ClaudeEngine::new(cfg());
    let line = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":1700000000}}"#;
    let events = eng.parse_line(line);
    assert!(matches!(events[0], EngineEvent::QuotaExhausted { .. }));
    let allowed = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning"}}"#;
    assert!(eng.parse_line(allowed).is_empty());
}

#[tokio::test]
#[ignore = "canlı CLI; kota harcar. Yalnız onaylı: cargo test -- --ignored live_"]
async fn live_claude_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let eng = ClaudeEngine::new(cfg());
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
