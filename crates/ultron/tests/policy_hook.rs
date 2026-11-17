//! Yalnız sentetik girdiler ve izole servis; gerçek Claude veya kullanıcı ayarı yok.
use std::collections::HashMap;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use ultron::commands::{check_hook, hook_action, hook_output};
use ultron_core::db::approvals::ApprovalStatus;
use ultron_core::ipc::{Client, Request, Response};
use ultron_core::policy::{Action, Decision};
use ultron_core::{Config, Db, Paths};
use ultrond::server::{self, ServerConfig, ServerHandle};

fn input(tool: &str, args: Value, cwd: &std::path::Path) -> Value {
    json!({"hook_event_name": "PreToolUse", "tool_name": tool,
        "tool_input": args, "session_id": "synthetic-session", "cwd": cwd})
}

fn permission(output: &Value) -> &str {
    output["hookSpecificOutput"]["permissionDecision"]
        .as_str()
        .unwrap()
}

fn assert_denied(output: &Value) {
    assert_eq!(permission(output), "deny");
    assert_eq!(output["hookSpecificOutput"]["hookEventName"], "PreToolUse");
    assert!(
        !output["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .unwrap()
            .is_empty()
    );
    assert!(output["systemMessage"].is_string());
}

#[test]
fn hook_maps_bash_to_shell_action() {
    let tmp = tempfile::tempdir().unwrap();
    assert_eq!(
        hook_action(&input("Bash", json!({"command":"git status"}), tmp.path())).unwrap(),
        Action::Shell {
            command: "git status".into(),
            cwd: tmp.path().into()
        }
    );
}

#[test]
fn hook_maps_file_tools_and_read_paths_relative_to_hook_cwd() {
    let tmp = tempfile::tempdir().unwrap();
    for (tool, args, name) in [
        ("Write", json!({"file_path":"a.txt", "content":""}), "a.txt"),
        (
            "Edit",
            json!({"file_path":"a.txt", "old_string":"x", "new_string":"y"}),
            "a.txt",
        ),
        (
            "MultiEdit",
            json!({"file_path":"a.txt", "edits":[{"old_string":"x","new_string":"y"}]}),
            "a.txt",
        ),
        (
            "NotebookEdit",
            json!({"notebook_path":"a.ipynb", "new_source":"x"}),
            "a.ipynb",
        ),
    ] {
        assert_eq!(
            hook_action(&input(tool, args, tmp.path())).unwrap(),
            Action::FileWrite {
                path: tmp.path().join(name)
            }
        );
    }
    let read = hook_action(&input("Read", json!({"file_path":"notes.txt"}), tmp.path())).unwrap();
    assert_eq!(
        read,
        Action::ToolCall {
            tool: "Read".into(),
            input: json!({"file_path":tmp.path().join("notes.txt")})
        }
    );
    for tool in ["Grep", "Glob"] {
        let Action::ToolCall { input, .. } =
            hook_action(&input(tool, json!({"pattern":"test"}), tmp.path())).unwrap()
        else {
            panic!()
        };
        assert_eq!(input["path"], json!(tmp.path()));
    }
}

#[tokio::test]
async fn malformed_and_unknown_hook_inputs_deny_without_connection() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::from_root(tmp.path().join("state"));
    let valid = input("Bash", json!({"command":"git status"}), tmp.path());
    let mut missing_session = valid.clone();
    missing_session
        .as_object_mut()
        .unwrap()
        .remove("session_id");
    let mut wrong_event = valid.clone();
    wrong_event["hook_event_name"] = json!("PostToolUse");
    let mut no_event = valid.clone();
    no_event.as_object_mut().unwrap().remove("hook_event_name");
    let mut relative = valid.clone();
    relative["cwd"] = json!("relative");
    for bad in [
        Value::Null,
        json!({}),
        missing_session,
        wrong_event,
        no_event,
        relative,
        input("Bash", json!({}), tmp.path()),
        input("Write", json!({"file_path":"x"}), tmp.path()),
        input(
            "Edit",
            json!({"file_path":"x","old_string":"x"}),
            tmp.path(),
        ),
        input("MultiEdit", json!({"file_path":"x","edits":[]}), tmp.path()),
        input("Read", json!({"file_path":null}), tmp.path()),
        input("FutureTool", json!({}), tmp.path()),
    ] {
        assert!(hook_action(&bad).is_err());
        assert_denied(&check_hook(bad, &paths, "unused", None, 0, true).await);
    }
}

async fn cli(paths: &Paths, pipe: &str, bytes: &[u8]) -> std::process::Output {
    cli_with_policy(paths, pipe, bytes, "full").await
}

async fn cli_with_policy(
    paths: &Paths,
    pipe: &str,
    bytes: &[u8],
    policy: &str,
) -> std::process::Output {
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_ultron"))
        .args(["policy", "check"])
        .env("ULTRON_TOOL_POLICY", policy)
        .env("ULTRON_HOME", &paths.root)
        .env("ULTRON_PIPE_NAME", pipe)
        .env_remove("ULTRON_CHAT_ID")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(bytes).await.unwrap();
    tokio::time::timeout(Duration::from_secs(12), child.wait_with_output())
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn hook_fails_closed_when_daemon_unreachable() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::from_root(tmp.path().join("ultron"));
    paths.ensure().unwrap();
    // Onaylayıcı dosyası yok: hook yalnız requester sırrını kullanabilmelidir.
    std::fs::write(paths.run.join("token"), "synthetic-requester").unwrap();
    let pipe = format!(r"\\.\pipe\ultron-hook-{}", uuid::Uuid::new_v4());
    let out = cli(
        &paths,
        &pipe,
        &serde_json::to_vec(&input("Bash", json!({"command":"git status"}), tmp.path())).unwrap(),
    )
    .await;
    assert!(out.status.success());
    assert!(out.stderr.is_empty());
    assert_denied(&serde_json::from_slice::<Value>(&out.stdout).unwrap());
}

#[tokio::test]
async fn hook_cli_rejects_malformed_oversized_and_stalled_stdin() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::from_root(tmp.path().into());
    for bytes in [b"not-json".to_vec(), vec![b' '; 1024 * 1024 + 1]] {
        let out = cli(&paths, "unused", &bytes).await;
        assert!(out.status.success());
        assert!(out.stderr.is_empty());
        assert_denied(&serde_json::from_slice::<Value>(&out.stdout).unwrap());
    }
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_ultron"))
        .args(["policy", "check"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .env("ULTRON_HOME", &paths.root)
        .env("ULTRON_PIPE_NAME", "unused")
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let _open_stdin = child.stdin.take().unwrap();
    let out = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(out.status.success());
    assert_denied(&serde_json::from_slice::<Value>(&out.stdout).unwrap());
}

async fn start(paths: &Paths) -> ServerHandle {
    server::start(ServerConfig {
        paths: paths.clone(),
        pipe_name: format!(r"\\.\pipe\ultron-hook-{}", uuid::Uuid::new_v4()),
        cfg: Config::default(),
        engines: HashMap::new(),
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn requester_hook_allows_safe_and_denies_forbidden_without_approver_file() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::from_root(tmp.path().join("ultron"));
    let h = start(&paths).await;
    // Sunucu rol sırrını bellekte tutar. Hook bu dosyaya erişmemelidir.
    std::fs::remove_file(paths.run.join("approver-token")).unwrap();
    for (command, expected) in [("git status", "allow"), ("diskpart", "deny")] {
        let bytes =
            serde_json::to_vec(&input("Bash", json!({"command":command}), tmp.path())).unwrap();
        let out = cli(&paths, h.pipe_name(), &bytes).await;
        assert!(out.status.success());
        assert!(out.stderr.is_empty());
        let json: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(permission(&json), expected);
    }
    h.shutdown().await;
}

#[tokio::test]
async fn hook_waits_for_user_decision_and_times_out_closed() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::from_root(tmp.path().join("ultron"));
    let h = start(&paths).await;
    let db = Db::open(&paths.db).unwrap();
    db.insert_chat("hook-chat", None, "sentetik", chrono::Utc::now())
        .unwrap();
    for approve in [true, false] {
        let check = check_hook(
            input("Bash", json!({"command":"rm test.txt"}), tmp.path()),
            &paths,
            h.pipe_name(),
            Some("hook-chat".into()),
            10,
            true,
        );
        let resolve = async {
            let row = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if let Some(row) = db
                        .list_approvals(Some(ApprovalStatus::Pending))
                        .unwrap()
                        .into_iter()
                        .next()
                    {
                        break row;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            assert_eq!(row.chat_id.as_deref(), Some("hook-chat"));
            assert_eq!(row.engine, ultron_core::EngineId::Claude);
            // Yalnız testin insan arayüzü tarafı onaylayıcıdır; hook bu istemciyi almaz.
            let mut user = Client::connect(h.pipe_name(), h.approver_token())
                .await
                .unwrap();
            user.send(&Request::ResolveApproval {
                id: row.id,
                approve,
                by: "desktop".into(),
            })
            .await
            .unwrap();
            assert!(matches!(
                user.next().await.unwrap(),
                Some(Response::Ok { .. })
            ));
        };
        let (output, ()) = tokio::join!(check, resolve);
        // Onaylı silmeyi Ultron yapar (çöp kutusu); ham komut hiçbir koşulda allow olmaz.
        assert_eq!(permission(&output), "deny");
        assert_eq!(
            output["hookSpecificOutput"]["permissionDecisionReason"]
                .as_str()
                .unwrap()
                .contains("ultron trash"),
            approve
        );
    }
    let output = check_hook(
        input("Bash", json!({"command":"rm test.txt"}), tmp.path()),
        &paths,
        h.pipe_name(),
        None,
        0,
        true,
    )
    .await;
    assert_denied(&output);
    assert!(
        db.list_approvals(Some(ApprovalStatus::Pending))
            .unwrap()
            .is_empty()
    );
    h.shutdown().await;
}

#[tokio::test]
async fn secret_inputs_and_unresolved_decisions_never_allow_or_leak() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::from_root(tmp.path().into());
    let secret = ["synthetic", "private", "value"].join("_");
    for value in [
        input(
            "Bash",
            json!({"command":format!("rm test.txt; token={secret}")}),
            tmp.path(),
        ),
        input(
            "Write",
            json!({"file_path":"test.txt","content":format!("password={secret}")}),
            tmp.path(),
        ),
        input(
            "Read",
            json!({"file_path":"test.txt", "extra":{"api_key":secret}}),
            tmp.path(),
        ),
    ] {
        let output = check_hook(value, &paths, "unused", None, 0, true).await;
        assert_denied(&output);
        assert!(!output.to_string().contains(&secret));
    }
    let output = hook_output(
        Decision::Deny {
            reason: format!("token={secret}"),
        },
        true,
    );
    assert!(!output.to_string().contains(&secret));
    assert_denied(&hook_output(
        Decision::Ask {
            reason: "bekliyor".into(),
            category: ultron_core::policy::RiskCategory::Delete,
        },
        true,
    ));
}

#[tokio::test]
async fn invalid_and_interrupted_daemon_replies_fail_closed() {
    use tokio::io::BufReader;
    use ultron_core::ipc::{self, sys};
    for reply in [
        b"not-json\n".as_slice(),
        b"{\"type\":\"ok\",\"data\":{}}\n",
        b"{\"type\":\"ok\",\"data\":{\"decision\":\"FutureDecision\"}}\n",
        b"{\"type\":\"error\",\"message\":\"password=synthetic-private\"}\n",
        b"",
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(tmp.path().into());
        paths.ensure().unwrap();
        std::fs::write(paths.run.join("token"), "synthetic-requester").unwrap();
        let pipe = format!(r"\\.\pipe\ultron-hook-{}", uuid::Uuid::new_v4());
        let sd =
            sys::SecurityDescriptor::owner_only(&sys::current_user_sid().unwrap(), "GA").unwrap();
        let server = sys::create_server(&pipe, true, &sd).unwrap();
        let responding = async {
            server.connect().await.unwrap();
            let (read, mut write) = tokio::io::split(server);
            let mut read = BufReader::new(read);
            let hello: Request = serde_json::from_str(
                &ipc::read_frame(&mut read, ipc::MAX_LINE_BYTES)
                    .await
                    .unwrap()
                    .unwrap(),
            )
            .unwrap();
            assert!(matches!(hello, Request::Hello { token } if token == "synthetic-requester"));
            ipc::write_msg(
                &mut write,
                &Response::Ok {
                    data: json!({"role":"requester"}),
                },
            )
            .await
            .unwrap();
            let request: Request = serde_json::from_str(
                &ipc::read_frame(&mut read, ipc::MAX_LINE_BYTES)
                    .await
                    .unwrap()
                    .unwrap(),
            )
            .unwrap();
            assert!(matches!(
                request,
                Request::PolicyCheck {
                    engine: ultron_core::EngineId::Claude,
                    ..
                }
            ));
            write.write_all(reply).await.unwrap();
        };
        let (output, ()) = tokio::join!(
            check_hook(
                input("Bash", json!({"command":"git status"}), tmp.path()),
                &paths,
                &pipe,
                None,
                0,
                true
            ),
            responding
        );
        assert_denied(&output);
        assert!(!output.to_string().contains("synthetic-private"));
    }
}

#[tokio::test]
async fn stalled_daemon_handshake_times_out_closed() {
    use ultron_core::ipc::sys;
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::from_root(tmp.path().into());
    paths.ensure().unwrap();
    std::fs::write(paths.run.join("token"), "synthetic-requester").unwrap();
    let pipe = format!(r"\\.\pipe\ultron-hook-{}", uuid::Uuid::new_v4());
    let sd = sys::SecurityDescriptor::owner_only(&sys::current_user_sid().unwrap(), "GA").unwrap();
    let server = sys::create_server(&pipe, true, &sd).unwrap();
    let check = check_hook(
        input("Bash", json!({"command":"git status"}), tmp.path()),
        &paths,
        &pipe,
        None,
        0,
        true,
    );
    let accept = server.connect();
    let (output, connected) = tokio::time::timeout(Duration::from_secs(8), async {
        tokio::join!(check, accept)
    })
    .await
    .unwrap();
    connected.unwrap();
    assert_denied(&output);
}

#[test]
fn harmless_internal_tools_map_to_tool_call_and_unknown_names_are_reported() {
    let tmp = tempfile::tempdir().unwrap();
    for tool in [
        "Task", "Agent", "BashOutput", "KillShell", "LS", "NotebookRead", "ExitPlanMode",
        "AskUserQuestion", "Skill",
    ] {
        assert!(matches!(
            hook_action(&input(tool, json!({"x": 1}), tmp.path())).unwrap(),
            Action::ToolCall { tool: t, .. } if t == tool
        ));
    }
    for tool in ["mcp__srv__write", "FutureTool"] {
        let error = hook_action(&input(tool, json!({}), tmp.path())).unwrap_err();
        assert!(error.to_string().contains(tool), "{error}");
    }
}

#[test]
fn notebook_cell_delete_is_an_edit_not_a_file_delete() {
    let tmp = tempfile::tempdir().unwrap();
    let action = hook_action(&input(
        "NotebookEdit",
        json!({"notebook_path":"n.ipynb","edit_mode":"delete","cell_id":"a"}),
        tmp.path(),
    ))
    .unwrap();
    assert_eq!(action, Action::FileWrite { path: tmp.path().join("n.ipynb") });
}

#[test]
fn allow_is_emitted_only_for_full_tool_policy_and_output_is_ascii() {
    assert_eq!(permission(&hook_output(Decision::Allow, true)), "allow");
    assert_eq!(hook_output(Decision::Allow, false), json!({}));
    let denied = hook_output(Decision::Deny { reason: "işlem reddedildi".into() }, false);
    assert_denied(&denied);
    let text = ultron::commands::hook_json(&denied);
    assert!(text.is_ascii());
    assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), denied);
    assert!(
        denied["hookSpecificOutput"]["permissionDecisionReason"]
            .as_str()
            .unwrap()
            .starts_with(ultron_core::engine::claude::HOOK_REASON_PREFIX)
    );
}

#[tokio::test]
async fn non_full_policy_never_emits_allow_through_the_cli() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::from_root(tmp.path().join("ultron"));
    let h = start(&paths).await;
    let bytes =
        serde_json::to_vec(&input("Bash", json!({"command":"git status"}), tmp.path())).unwrap();
    for policy in ["workspace", "readonly", ""] {
        let out = cli_with_policy(&paths, h.pipe_name(), &bytes, policy).await;
        assert!(out.status.success());
        let json: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(json, json!({}));
    }
    let denied =
        serde_json::to_vec(&input("Bash", json!({"command":"diskpart"}), tmp.path())).unwrap();
    let out = cli_with_policy(&paths, h.pipe_name(), &denied, "readonly").await;
    assert_eq!(permission(&serde_json::from_slice::<Value>(&out.stdout).unwrap()), "deny");
    h.shutdown().await;
}

#[tokio::test]
async fn secret_in_safe_input_is_decided_on_raw_action_not_denied_for_masking() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::from_root(tmp.path().join("ultron"));
    let h = start(&paths).await;
    let secret = ["synthetic", "private", "value"].join("_");
    let value = input(
        "Read",
        json!({"file_path":"test.txt", "extra":{"api_key":secret}}),
        tmp.path(),
    );
    let output = check_hook(value, &paths, h.pipe_name(), None, 0, true).await;
    assert_eq!(permission(&output), "allow");
    assert!(!output.to_string().contains(&secret));
    h.shutdown().await;
}

#[test]
fn policy_panic_guard_exits_with_blocking_code_two() {
    if std::env::var_os("ULTRON_PANIC_CHILD").is_some() {
        ultron::commands::install_policy_panic_guard();
        panic!("sentetik panik");
    }
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "policy_panic_guard_exits_with_blocking_code_two", "--nocapture"])
        .env("ULTRON_PANIC_CHILD", "1")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("reddedildi"));
}

#[test]
fn settings_are_exec_form_and_work_with_spaced_executable_path() {
    use ultron_core::engine::claude::{cleanup_policy_settings, install_policy_settings};
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::from_root(tmp.path().join("ultron state"));
    paths.ensure().unwrap();
    let exe = tmp.path().join("program files").join("ultron.exe");
    std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
    std::fs::write(&exe, b"x").unwrap();
    let path = install_policy_settings(&paths, "chat-1", &exe).unwrap();
    let again = install_policy_settings(&paths, "chat-1", &exe).unwrap();
    assert_eq!(path, again);
    let settings: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let hook = &settings["hooks"]["PreToolUse"][0]["hooks"][0];
    assert_eq!(hook["command"], json!(exe));
    assert_eq!(hook["args"], json!(["policy", "check"]));
    assert_eq!(hook["onFailure"], "block");
    assert!(
        hook["timeout"].as_u64().unwrap()
            > u64::from(ultron::commands::HOOK_APPROVAL_WAIT_SECS) + 10
    );
    cleanup_policy_settings(&paths);
    assert!(!path.exists());
}

#[tokio::test]
#[ignore = "canlı Claude CLI kota harcar; yalnız açık kullanıcı onayıyla çalıştırılır"]
async fn live_claude_allowed_read_passes_hook() {
    use ultron_core::config::EngineConfig;
    use ultron_core::engine::{ClaudeEngine, Engine, ToolPolicy, TurnRequest};
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::from_root(tmp.path().join("ultron"));
    let h = start(&paths).await;
    Db::open(&paths.db)
        .unwrap()
        .insert_chat("live-read", None, "sentetik", chrono::Utc::now())
        .unwrap();
    let cwd = tmp.path().join("workspace");
    std::fs::create_dir(&cwd).unwrap();
    std::fs::write(cwd.join("test.txt"), "sentetik-isaret-4242").unwrap();
    let settings = ultron_core::engine::claude::install_policy_settings(
        &paths,
        "live-read",
        std::path::Path::new(env!("CARGO_BIN_EXE_ultron")),
    )
    .unwrap();
    let engine = ClaudeEngine::new(EngineConfig {
        id: ultron_core::EngineId::Claude,
        enabled: true,
        binary: "claude".into(),
        model: None,
        effort: None,
        extra_args: vec![],
    });
    let mut spec = engine.command(&TurnRequest {
        chat_id: "live-read".into(),
        cwd: cwd.clone(),
        prompt: "Read aracıyla test.txt dosyasını oku ve içeriğini aynen yaz.".into(),
        resume_native_id: None,
        tool_policy: ToolPolicy::Full,
    });
    let pos = spec.args.iter().position(|arg| arg == "--settings").unwrap();
    spec.args[pos + 1] = settings.to_string_lossy().into_owned();
    spec.args.extend(["--max-turns".into(), "3".into()]);
    let mut child = tokio::process::Command::new(&spec.program)
        .args(&spec.args)
        .current_dir(&cwd)
        .env("ULTRON_HOME", &paths.root)
        .env("ULTRON_PIPE_NAME", h.pipe_name())
        .env("ULTRON_CHAT_ID", "live-read")
        .env("ULTRON_TOOL_POLICY", "full")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(spec.stdin.unwrap().as_bytes())
        .await
        .unwrap();
    let out = tokio::time::timeout(Duration::from_secs(120), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    if let Some(dump) = std::env::var_os("ULTRON_LIVE_DUMP_READ") {
        let _ = std::fs::write(dump, &out.stdout);
    }
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("sentetik-isaret-4242"), "Read hook'tan geçmedi");
    assert!(
        !engine
            .parse_line(text.lines().last().unwrap_or(""))
            .iter()
            .any(|ev| matches!(ev, ultron_core::EngineEvent::Error { .. }))
    );
    h.shutdown().await;
}

#[tokio::test]
#[ignore = "canlı Claude CLI kota harcar; yalnız açık kullanıcı onayıyla çalıştırılır"]
async fn live_claude_hook_blocks_delete() {
    use ultron_core::config::EngineConfig;
    use ultron_core::engine::{ClaudeEngine, Engine, ToolPolicy, TurnRequest};
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::from_root(tmp.path().join("ultron"));
    let h = start(&paths).await;
    Db::open(&paths.db)
        .unwrap()
        .insert_chat("live-hook", None, "sentetik", chrono::Utc::now())
        .unwrap();
    let cwd = tmp.path().join("workspace");
    std::fs::create_dir(&cwd).unwrap();
    std::fs::write(cwd.join("test.txt"), "sentetik dosya").unwrap();
    let settings = ultron_core::engine::claude::install_policy_settings(
        &paths,
        "live-hook",
        std::path::Path::new(env!("CARGO_BIN_EXE_ultron")),
    )
    .unwrap();
    let engine = ClaudeEngine::new(EngineConfig {
        id: ultron_core::EngineId::Claude,
        enabled: true,
        binary: "claude".into(),
        model: None,
        effort: None,
        extra_args: vec![],
    });
    // Ayrık çocuk ortamı: kullanıcı ayarları ve bu test sürecinin ortamı değişmez.
    let mut spec = engine.command(&TurnRequest {
        chat_id: "live-hook".into(),
        cwd: cwd.clone(),
        prompt: "Bash ile yalnız test.txt dosyasını silmeyi dene. Ret alırsan tekrar deneme."
            .into(),
        resume_native_id: None,
        tool_policy: ToolPolicy::Full,
    });
    let pos = spec
        .args
        .iter()
        .position(|arg| arg == "--settings")
        .unwrap();
    spec.args[pos + 1] = settings.to_string_lossy().into_owned();
    spec.args.extend(["--max-turns".into(), "3".into()]);
    let mut child = tokio::process::Command::new(&spec.program)
        .args(&spec.args)
        .current_dir(&cwd)
        .env("ULTRON_HOME", &paths.root)
        .env("ULTRON_PIPE_NAME", h.pipe_name())
        .env("ULTRON_CHAT_ID", "live-hook")
        .env("ULTRON_TOOL_POLICY", "full")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(spec.stdin.unwrap().as_bytes())
        .await
        .unwrap();
    let db = Db::open(&paths.db).unwrap();
    let waiting = async {
        let row = tokio::time::timeout(Duration::from_secs(90), async {
            loop {
                if let Some(row) = db
                    .list_approvals(Some(ApprovalStatus::Pending))
                    .unwrap()
                    .into_iter()
                    .next()
                {
                    break row;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("Claude silme onayı oluşturmadı");
        let mut user = Client::connect(h.pipe_name(), h.approver_token())
            .await
            .unwrap();
        user.send(&Request::ResolveApproval {
            id: row.id,
            approve: false,
            by: "desktop".into(),
        })
        .await
        .unwrap();
        assert!(matches!(
            user.next().await.unwrap(),
            Some(Response::Ok { .. })
        ));
    };
    let (out, ()) = tokio::join!(
        tokio::time::timeout(Duration::from_secs(120), child.wait_with_output()),
        waiting
    );
    let out = out.unwrap().unwrap();
    if let Some(dump) = std::env::var_os("ULTRON_LIVE_DUMP") {
        let _ = std::fs::write(dump, &out.stdout);
    }
    assert!(out.status.success());
    assert!(cwd.join("test.txt").is_file());
    assert!(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .flat_map(|line| engine.parse_line(line))
            .any(|ev| matches!(ev, ultron_core::EngineEvent::Error { .. }))
    );
    h.shutdown().await;
}
