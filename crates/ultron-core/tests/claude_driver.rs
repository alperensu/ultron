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
fn claude_command_includes_policy_settings() {
    use ultron_core::engine::claude::{install_policy_settings, policy_settings_path};
    let eng = ClaudeEngine::new(cfg());
    for policy in [
        ToolPolicy::ReadOnly,
        ToolPolicy::Workspace,
        ToolPolicy::Full,
    ] {
        let req = request(policy, None);
        let spec = eng.command(&req);
        let pos = spec
            .args
            .iter()
            .position(|arg| arg == "--settings")
            .unwrap();
        let expected =
            policy_settings_path(&ultron_core::Paths::resolve().unwrap(), &req.chat_id).unwrap();
        assert_eq!(PathBuf::from(&spec.args[pos + 1]), expected);
        assert!(spec.args.contains(&"--include-hook-events".into()));
    }
    let tmp = tempfile::tempdir().unwrap();
    let paths = ultron_core::Paths::from_root(tmp.path().join("durum klasörü"));
    let executable = std::env::current_exe().unwrap();
    let path = install_policy_settings(&paths, "sentetik-sohbet", &executable).unwrap();
    // Aynı sohbetin sonraki turunda dosya atomik olarak yenilenebilir.
    assert_eq!(
        install_policy_settings(&paths, "sentetik-sohbet", &executable).unwrap(),
        path
    );
    assert_eq!(path, paths.run.join("claude-settings-sentetik-sohbet.json"));
    let settings: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(settings["disableAllHooks"], false);
    let group = &settings["hooks"]["PreToolUse"][0];
    assert_eq!(group["matcher"], "*");
    let hook = &group["hooks"][0];
    assert_eq!(hook["command"], json!(executable));
    assert_eq!(hook["args"], json!(["policy", "check"]));
    assert_eq!(hook["onFailure"], "block");
    assert_eq!(hook["timeout"], 320);
    assert!(hook.get("async").is_none());
    assert!(
        std::fs::read_dir(&paths.run).unwrap().all(|entry| entry
            .unwrap()
            .path()
            .extension()
            .unwrap()
            == "json")
    );
    for invalid in ["", "../escape", "a/b", "a\\b", "a:stream"] {
        assert!(policy_settings_path(&paths, invalid).is_err());
    }
    assert!(install_policy_settings(&paths, "c1", &tmp.path().join("missing.exe")).is_err());
}

#[test]
fn claude_hook_preparation_rejects_settings_override_and_invalid_chat() {
    let mut c = cfg();
    c.extra_args = vec!["--settings={}".into()];
    let eng = ClaudeEngine::new(c);
    let mut cmd = tokio::process::Command::new("never-started");
    assert!(
        eng.prepare_command(&request(ToolPolicy::Full, None), &mut cmd)
            .is_err()
    );
    let eng = ClaudeEngine::new(cfg());
    let mut req = request(ToolPolicy::Full, None);
    req.chat_id = "../outside".into();
    assert!(eng.prepare_command(&req, &mut cmd).is_err());
}

// Gerçek `claude --include-hook-events` çıktısından alınmış satır (içerik sentetik: silme denemesi).
const REAL_HOOK_DENY: &str = r#"{"type":"system","subtype":"hook_response","hook_id":"95a6ca2a-6b29-4eed-a8bb-0d46e31e9bb2","hook_name":"PreToolUse:Bash","hook_event":"PreToolUse","output":"{\"hookSpecificOutput\":{\"hookEventName\":\"PreToolUse\",\"permissionDecision\":\"deny\",\"permissionDecisionReason\":\"[Ultron] i\\u015flem reddedildi veya onay s\\u00fcresi doldu\"},\"systemMessage\":\"[Ultron] i\\u015flem reddedildi veya onay s\\u00fcresi doldu\"}\n","stdout":"{\"hookSpecificOutput\":{\"hookEventName\":\"PreToolUse\",\"permissionDecision\":\"deny\",\"permissionDecisionReason\":\"[Ultron] i\\u015flem reddedildi veya onay s\\u00fcresi doldu\"},\"systemMessage\":\"[Ultron] i\\u015flem reddedildi veya onay s\\u00fcresi doldu\"}\n","stderr":"","exit_code":0,"outcome":"success","uuid":"d22117d5-b9a8-494f-9665-07d8f5e3cbf5","session_id":"23317c08-8a07-483e-af2f-3c6ddafa22e9"}"#;

#[test]
fn claude_pretooluse_denials_are_attributed_to_ultron_only() {
    let eng = ClaudeEngine::new(cfg());
    // Gerçek satır: Ultron öneki taşıyan ret, güvenli bir hata olayına dönüşür.
    let events = eng.parse_line(REAL_HOOK_DENY);
    match events.as_slice() {
        [EngineEvent::Error { message }] => {
            assert!(message.contains("reddedildi"), "{message}");
            assert!(!message.starts_with("[Ultron]"));
        }
        other => panic!("beklenmeyen olaylar: {other:?}"),
    }
    // Ön eksiz (başka hook'a ait) ret ve hata ham içerik sızdırmadan yok sayılır.
    for event in [
        json!({"type":"system","subtype":"hook_response","hook_event":"PreToolUse","outcome":"error","exit_code":1,"stderr":"password=synthetic-private"}),
        json!({"type":"system","subtype":"hook_response","hook_event":"PreToolUse","outcome":"cancelled"}),
        json!({"type":"system","subtype":"hook_response","hook_event":"PreToolUse","outcome":"success","exit_code":0,
            "stdout":json!({"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"baska hook password=synthetic-private"}}).to_string()}),
    ] {
        assert!(eng.parse_line(&event.to_string()).is_empty());
    }
    // Ultron ön ekli ret maskelenir.
    let masked = json!({"type":"system","subtype":"hook_response","hook_event":"PreToolUse","outcome":"success","exit_code":0,
        "stdout":json!({"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"[Ultron] password=synthetic-private"}}).to_string()});
    let events = eng.parse_line(&masked.to_string());
    assert!(matches!(events.as_slice(), [EngineEvent::Error { .. }]));
    assert!(!format!("{events:?}").contains("synthetic-private"));
    let allow = json!({"type":"system","subtype":"hook_response","hook_event":"PreToolUse","outcome":"success","exit_code":0,
        "stdout":json!({"hookSpecificOutput":{"permissionDecision":"allow"}}).to_string()});
    assert!(eng.parse_line(&allow.to_string()).is_empty());
}

#[test]
fn structured_engine_forwards_prepare_command_errors() {
    let mut c = cfg();
    c.extra_args = vec!["--bare".into()];
    let engine = ultron_core::summarizer::StructuredEngine::new(
        Box::new(ClaudeEngine::new(c)),
        "{}",
    );
    let mut cmd = tokio::process::Command::new("never-started");
    assert!(
        engine
            .prepare_command(&request(ToolPolicy::Full, None), &mut cmd)
            .is_err()
    );
}

#[test]
fn claude_sets_setting_sources_user_and_rejects_security_flag_overrides() {
    let eng = ClaudeEngine::new(cfg());
    let args = eng.command(&request(ToolPolicy::Full, None)).args;
    let pos = args.iter().position(|a| a == "--setting-sources").unwrap();
    assert_eq!(args[pos + 1], "user");
    let mut cmd = tokio::process::Command::new("never-started");
    for flag in [
        "--settings",
        "--setting-sources",
        "--bare",
        "--permission-mode",
        "--dangerously-skip-permissions",
        "--setting-sources=user,project",
        "--permission-mode=bypassPermissions",
    ] {
        let mut c = cfg();
        c.extra_args = vec![flag.into()];
        assert!(
            ClaudeEngine::new(c)
                .prepare_command(&request(ToolPolicy::Full, None), &mut cmd)
                .is_err(),
            "{flag} reddedilmeli"
        );
    }
}

#[tokio::test]
async fn instruction_wrapper_preserves_preparation_and_fail_closed_launch() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use ultron_core::engine::{Capabilities, CommandSpec, RestrictionLevel};
    struct PreparationProbe(Arc<AtomicBool>);
    impl Engine for PreparationProbe {
        fn id(&self) -> EngineId {
            EngineId::Claude
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                streaming: true,
                resume: false,
                mid_turn_input: false,
                tool_restriction: RestrictionLevel::Partial,
            }
        }
        fn command(&self, req: &TurnRequest) -> CommandSpec {
            CommandSpec {
                program: "must-never-start.exe".into(),
                args: vec![],
                cwd: req.cwd.clone(),
                stdin: None,
            }
        }
        fn prepare_command(
            &self,
            req: &TurnRequest,
            cmd: &mut tokio::process::Command,
        ) -> anyhow::Result<()> {
            cmd.env("ULTRON_CHAT_ID", &req.chat_id);
            self.0.store(true, Ordering::SeqCst);
            anyhow::bail!("Sentetik hook hazırlık hatası")
        }
        fn parse_line(&self, _: &str) -> Vec<EngineEvent> {
            vec![]
        }
    }
    let tmp = tempfile::tempdir().unwrap();
    let prepared = Arc::new(AtomicBool::new(false));
    let engine = ultron_core::prompts::with_instructions(
        Arc::new(PreparationProbe(prepared.clone())),
        "sentetik",
        tmp.path(),
        "c1",
    )
    .unwrap();
    let mut req = request(ToolPolicy::ReadOnly, None);
    req.cwd = tmp.path().into();
    let (tx, _rx) = mpsc::channel(8);
    let error = run_turn(engine.as_ref(), req, tx, CancellationToken::new())
        .await
        .unwrap_err();
    assert!(prepared.load(Ordering::SeqCst));
    assert_eq!(error.to_string(), "Sentetik hook hazırlık hatası");
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
    let allow = spec
        .args
        .iter()
        .position(|a| a == "--allowedTools")
        .expect("--allowedTools bayrağı yok");
    assert_eq!(spec.args[allow + 1], "Bash(ultron trash:*)");
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

fn result_line(id: &str, is_error: bool) -> String {
    json!({
        "type": "user",
        "message": { "content": [{
            "type": "tool_result",
            "tool_use_id": id,
            "content": "sonuc",
            "is_error": is_error
        }] }
    })
    .to_string()
}

#[test]
fn claude_successful_edit_emits_file_changed_after_result() {
    let eng = ClaudeEngine::new(cfg());
    let edit = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu-9","name":"Edit","input":{"file_path":"C:/proj/a.txt","old_string":"x","new_string":"y"}}]}}"#;
    let write = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu-10","name":"Write","input":{"file_path":"C:/proj/b.txt","content":"z"}}]}}"#;
    let notebook = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu-11","name":"NotebookEdit","input":{"notebook_path":"C:/proj/n.ipynb","new_source":"q"}}]}}"#;
    let read = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu-12","name":"Read","input":{"file_path":"C:/proj/c.txt"}}]}}"#;

    // tool_use anında yalnız ToolCall; FileChanged henüz yok.
    assert_eq!(
        eng.parse_line(edit),
        vec![EngineEvent::ToolCall {
            tool: "Edit".into(),
            input: json!({ "file_path": "C:/proj/a.txt", "old_string": "x", "new_string": "y" }),
        }]
    );
    assert!(!eng.parse_line(write).iter().any(|e| matches!(e, EngineEvent::FileChanged { .. })));
    eng.parse_line(notebook);
    assert!(!eng.parse_line(read).iter().any(|e| matches!(e, EngineEvent::FileChanged { .. })));

    // Başarılı sonuçta ToolResult ardından FileChanged.
    assert_eq!(
        eng.parse_line(&result_line("toolu-9", false)),
        vec![
            EngineEvent::ToolResult {
                tool: "toolu-9".into(),
                output_excerpt: "sonuc".into(),
                is_error: false,
            },
            EngineEvent::FileChanged {
                path: "C:/proj/a.txt".into(),
                change: "edit".into(),
            },
        ]
    );
    assert_eq!(
        eng.parse_line(&result_line("toolu-10", false))
            .into_iter()
            .find(|e| matches!(e, EngineEvent::FileChanged { .. })),
        Some(EngineEvent::FileChanged {
            path: "C:/proj/b.txt".into(),
            change: "write".into(),
        })
    );
    assert!(matches!(
        eng.parse_line(&result_line("toolu-11", false)).last(),
        Some(EngineEvent::FileChanged { path, change }) if path == "C:/proj/n.ipynb" && change == "edit"
    ));
    // Aynı sonuç ikinci kez gelirse değişiklik tekrarlanmaz.
    assert!(!eng.parse_line(&result_line("toolu-9", false)).iter().any(|e| matches!(e, EngineEvent::FileChanged { .. })));
}

#[test]
fn claude_rejected_write_emits_no_file_changed() {
    let eng = ClaudeEngine::new(cfg());
    let write = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu-20","name":"Write","input":{"file_path":"C:/proj/b.txt","content":"z"}}]}}"#;
    eng.parse_line(write);
    let events = eng.parse_line(&result_line("toolu-20", true));
    assert_eq!(
        events,
        vec![EngineEvent::ToolResult {
            tool: "toolu-20".into(),
            output_excerpt: "sonuc".into(),
            is_error: true,
        }]
    );
}

#[test]
fn extra_args_are_passed_to_claude() {
    let mut c = cfg();
    c.extra_args = vec!["--add-dir".into(), "C:/extra".into()];
    let eng = ClaudeEngine::new(c);
    let spec = eng.command(&request(ToolPolicy::Full, None));
    let pos = spec.args.iter().position(|a| a == "--add-dir").expect("ek argüman yok");
    assert_eq!(spec.args[pos + 1], "C:/extra");
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
