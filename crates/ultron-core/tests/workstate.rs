use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::anyhow;
use chrono::Utc;
use serde_json::json;
use ultron_core::config::{EngineConfig, EngineId};
use ultron_core::db::{Db, StoredEvent};
use ultron_core::engine::{AgyEngine, Engine, FakeEngine, ToolPolicy, TurnRequest};
use ultron_core::events::{EngineEvent, SwitchReason};
use ultron_core::summarizer::{
    AgySummarizer, SUMMARY_SCHEMA, StructuredEngine, SummaryFuture, SummaryPatch, Summarizer,
};
use ultron_core::workstate::{EngineSpan, WorkState, WorkStateService};

fn stored(seq: i64, ev: EngineEvent) -> StoredEvent {
    let payload = serde_json::to_value(&ev).unwrap();
    let kind = payload["type"].as_str().unwrap().to_string();
    StoredEvent {
        id: format!("e{seq}"),
        chat_id: "chat-1".into(),
        session_id: None,
        seq,
        ts: Utc::now(),
        kind,
        payload,
    }
}

fn open_db(dir: &std::path::Path) -> Arc<Db> {
    let db = Db::open(&dir.join("ultron.db")).unwrap();
    db.insert_chat("chat-1", None, "test", Utc::now()).unwrap();
    Arc::new(db)
}

struct FailingSummarizer;

impl Summarizer for FailingSummarizer {
    fn refresh<'a>(&'a self, _state: &'a WorkState, _recent: &'a [StoredEvent]) -> SummaryFuture<'a> {
        Box::pin(async { Err(anyhow!("sahte özetleyici hatası")) })
    }
}

struct FixedSummarizer;

impl Summarizer for FixedSummarizer {
    fn refresh<'a>(&'a self, _state: &'a WorkState, _recent: &'a [StoredEvent]) -> SummaryFuture<'a> {
        Box::pin(async {
            Ok(SummaryPatch {
                next_step: Some("Testleri yaz".into()),
                done: Some(vec!["plan".into()]),
                ..SummaryPatch::default()
            })
        })
    }
}

#[test]
fn rules_collect_files_and_engine_history() {
    let mut state = WorkState::default();
    let file = |path: &str| EngineEvent::FileChanged {
        path: path.into(),
        change: "modified".into(),
    };
    state.apply_rules(&stored(1, file("src/a.rs")));
    state.apply_rules(&stored(2, file("src/a.rs")));
    state.apply_rules(&stored(3, file("src/b.rs")));
    state.apply_rules(&stored(
        5,
        EngineEvent::EngineSwitched {
            from: EngineId::Claude,
            to: EngineId::Codex,
            reason: SwitchReason::Quota,
        },
    ));
    assert_eq!(
        state.files_touched,
        BTreeSet::from(["src/a.rs".to_string(), "src/b.rs".to_string()])
    );
    assert_eq!(
        state.engine_history,
        vec![
            EngineSpan {
                engine: EngineId::Claude,
                from_seq: 0,
                to_seq: Some(5),
            },
            EngineSpan {
                engine: EngineId::Codex,
                from_seq: 5,
                to_seq: None,
            },
        ]
    );
}

#[test]
fn rules_capture_decision_markers() {
    let mut state = WorkState::default();
    state.apply_rules(&stored(
        1,
        EngineEvent::UserText {
            text: "Merhaba, bir dosya yazalım".into(),
        },
    ));
    state.apply_rules(&stored(
        2,
        EngineEvent::UserText {
            text: "KARAR: CSV kullanacağız".into(),
        },
    ));
    state.apply_rules(&stored(
        3,
        EngineEvent::UserText {
            text: "Bunu not al.\nDECISION: JSON yapalım\nnormal satır".into(),
        },
    ));
    assert_eq!(state.goal.as_deref(), Some("Merhaba, bir dosya yazalım"));
    assert_eq!(state.decisions.len(), 2);
    assert_eq!(state.decisions[0].what, "CSV kullanacağız");
    assert_eq!(state.decisions[0].source_event, "e2");
    assert_eq!(state.decisions[1].what, "JSON yapalım");
}

#[test]
fn state_persists_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let db = open_db(tmp.path());
    let mut state = WorkState {
        goal: Some("Hedef".into()),
        summary_stale: true,
        ..WorkState::default()
    };
    state.files_touched.insert("a.rs".into());
    db.put_work_state("chat-1", &serde_json::to_value(&state).unwrap())
        .unwrap();
    let value = db.get_work_state("chat-1").unwrap().unwrap();
    assert_eq!(value["summary_stale"], json!(true));
    let back: WorkState = serde_json::from_value(value).unwrap();
    assert_eq!(back, state);
}

#[tokio::test]
async fn summarizer_failure_keeps_rule_state_and_marks_stale() {
    let tmp = tempfile::tempdir().unwrap();
    let db = open_db(tmp.path());
    db.insert_event(&stored(
        1,
        EngineEvent::UserText {
            text: "Hedef: sohbeti devret".into(),
        },
    ))
    .unwrap();
    db.insert_event(&stored(
        2,
        EngineEvent::UserText {
            text: "KARAR: sade tut".into(),
        },
    ))
    .unwrap();
    let svc = WorkStateService::new(db.clone(), Box::new(FailingSummarizer));
    let state = svc.on_turn_completed("chat-1").await.unwrap();
    assert!(state.summary_stale);
    assert_eq!(state.decisions.len(), 1);
    assert_eq!(state.goal.as_deref(), Some("Hedef: sohbeti devret"));
    let stored_json = db.get_work_state("chat-1").unwrap().unwrap();
    assert_eq!(stored_json["summary_stale"], json!(true));
    assert_eq!(stored_json["decisions"][0]["what"], json!("sade tut"));
}

#[tokio::test]
async fn summarizer_patch_applied_and_clears_stale() {
    let tmp = tempfile::tempdir().unwrap();
    let db = open_db(tmp.path());
    db.insert_event(&stored(
        1,
        EngineEvent::UserText {
            text: "Merhaba".into(),
        },
    ))
    .unwrap();
    let svc = WorkStateService::new(db.clone(), Box::new(FixedSummarizer));
    let state = svc.on_turn_completed("chat-1").await.unwrap();
    assert!(!state.summary_stale);
    assert_eq!(state.next_step.as_deref(), Some("Testleri yaz"));
    assert_eq!(state.done, vec!["plan".to_string()]);
}

#[tokio::test]
async fn agy_summarizer_parses_structured_output() {
    let tmp = tempfile::tempdir().unwrap();
    let fixture = tmp.path().join("summary.jsonl");
    let answer = r#"{"goal":"Yeni hedef","next_step":"Testleri yaz","open_questions":[],"done":[]}"#;
    let line = json!({"type": "assistant_text", "text": answer}).to_string();
    std::fs::write(&fixture, format!("{line}\n")).unwrap();
    let summarizer = AgySummarizer::new(
        Box::new(FakeEngine::from_fixture(&fixture)),
        tmp.path().to_path_buf(),
    );
    let patch = summarizer
        .refresh(&WorkState::default(), &[])
        .await
        .unwrap();
    assert_eq!(patch.goal.as_deref(), Some("Yeni hedef"));
    assert_eq!(patch.next_step.as_deref(), Some("Testleri yaz"));
}

#[test]
fn summary_command_puts_schema_before_prompt() {
    let cfg = EngineConfig {
        id: EngineId::Agy,
        enabled: true,
        binary: "agy".into(),
        model: None,
        effort: None,
        extra_args: Vec::new(),
    };
    let engine = StructuredEngine::new(Box::new(AgyEngine::new(cfg)), SUMMARY_SCHEMA);
    let req = TurnRequest {
        chat_id: "summarizer".into(),
        cwd: PathBuf::from("C:/proj"),
        prompt: "ozetle".into(),
        resume_native_id: None,
        tool_policy: ToolPolicy::ReadOnly,
    };
    let spec = engine.command(&req);
    let n = spec.args.len();
    assert_eq!(spec.args[n - 3], "--json-schema");
    assert_eq!(spec.args[n - 2], SUMMARY_SCHEMA);
    assert_eq!(spec.args[n - 1], "-p=ozetle");
}
