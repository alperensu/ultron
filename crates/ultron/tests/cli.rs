use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Once};

use clap::Parser;
use ultron::commands::{
    Cli, Cmd, EngineArg, render_event, schtasks_install_args, schtasks_uninstall_args,
};
use ultron_core::engine::{Engine, FakeEngine};
use ultron_core::{Config, EngineId, Paths, StoredEvent};
use ultrond::server::{self, ServerConfig, ServerHandle};

const STARTED: &str = r#"{"type":"session_started","native_session_id":"nat-1"}"#;
const REPLY: &str = r#"{"type":"assistant_text","text":"merhaba dünya"}"#;
const DONE: &str = r#"{"type":"turn_completed"}"#;

static FAKE: Once = Once::new();

fn ensure_fake_cli() {
    FAKE.call_once(|| {
        let name = format!("ultron-fake-cli{}", std::env::consts::EXE_SUFFIX);
        let find = || {
            let exe = std::env::current_exe().unwrap();
            exe.ancestors()
                .skip(1)
                .take(6)
                .map(|d| d.join(&name))
                .find(|p| p.is_file())
        };
        let fake = find().unwrap_or_else(|| {
            let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
            let status = Command::new(cargo)
                .args(["build", "-p", "ultron-core", "--bin", "ultron-fake-cli"])
                .status()
                .unwrap();
            assert!(status.success());
            find().expect("sahte CLI derlenemedi")
        });
        // SAFETY: testler bu değişkeni yalnız aynı değerle bir kez yazar.
        unsafe { std::env::set_var("ULTRON_FAKE_CLI", &fake) };
    });
}

struct Env {
    _tmp: tempfile::TempDir,
    paths: Paths,
    pipe: String,
    cwd: PathBuf,
    handle: ServerHandle,
}

async fn env() -> Env {
    ensure_fake_cli();
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::from_root(tmp.path().join("ultron"));
    paths.ensure().unwrap();
    let cwd = tmp.path().join("proje klasörü");
    std::fs::create_dir_all(&cwd).unwrap();
    let fixture = tmp.path().join("fixture.jsonl");
    std::fs::write(&fixture, [STARTED, REPLY, DONE].join("\n") + "\n").unwrap();
    let mut engines: HashMap<EngineId, Arc<dyn Engine>> = HashMap::new();
    for id in [EngineId::Claude, EngineId::Codex] {
        engines.insert(id, Arc::new(FakeEngine::from_fixture(&fixture)));
    }
    let pipe = format!(r"\\.\pipe\ultron-test-{}", uuid::Uuid::new_v4());
    let handle = server::start(ServerConfig {
        paths: paths.clone(),
        pipe_name: pipe.clone(),
        cfg: Config::default(),
        engines,
    })
    .await
    .unwrap();
    Env {
        _tmp: tmp,
        paths,
        pipe,
        cwd,
        handle,
    }
}

/// `ultron` ikilisini test servisine yönlendirilmiş ortamla çalıştırır; `(başarılı, stdout)`.
async fn ultron(env: &Env, args: &[&str]) -> (bool, String) {
    let args: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
    let home = env.paths.root.clone();
    let pipe = env.pipe.clone();
    tokio::task::spawn_blocking(move || {
        let out = Command::new(env!("CARGO_BIN_EXE_ultron"))
            .args(&args)
            .env("ULTRON_HOME", home)
            .env("ULTRON_PIPE_NAME", pipe)
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.success(), text)
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_prints_assistant_text() {
    let env = env().await;
    let cwd = env.cwd.to_string_lossy().into_owned();
    let (ok, out) = ultron(
        &env,
        &["chat", "--chat", "c1", "--engine", "claude", "--cwd", &cwd, "selam"],
    )
    .await;
    assert!(ok, "{out}");
    assert!(out.contains("merhaba dünya"), "{out}");
    env.handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn switch_prints_engine_switched_warning() {
    let env = env().await;
    let cwd = env.cwd.to_string_lossy().into_owned();
    let (ok, out) = ultron(&env, &["chat", "--chat", "c1", "--engine", "claude", "--cwd", &cwd, "bir"]).await;
    assert!(ok, "{out}");

    let (ok, out) = ultron(&env, &["switch", "--chat", "c1", "--to", "codex"]).await;
    assert!(ok, "{out}");
    assert!(out.contains("MOTOR DEĞİŞTİ") && out.contains("Codex"), "{out}");

    let (ok, out) = ultron(&env, &["events", "--chat", "c1"]).await;
    assert!(ok, "{out}");
    assert!(out.contains("!! MOTOR DEĞİŞTİ: claude -> codex"), "{out}");

    // Motor değişimi sohbet akışında da belirgin uyarı satırıyla görünür.
    let (ok, out) = ultron(&env, &["chat", "--chat", "c1", "--engine", "claude", "--cwd", &cwd, "iki"]).await;
    assert!(ok, "{out}");
    assert!(out.contains("!! MOTOR DEĞİŞTİ: codex -> claude"), "{out}");
    env.handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn history_search_finds_previous_message() {
    let env = env().await;
    let cwd = env.cwd.to_string_lossy().into_owned();
    let (ok, out) = ultron(
        &env,
        &["chat", "--chat", "c1", "--engine", "claude", "--cwd", &cwd, "kahvaltıda simit yedik"],
    )
    .await;
    assert!(ok, "{out}");
    let (ok, out) = ultron(&env, &["history", "search", "simit", "--chat", "c1", "--limit", "3"]).await;
    assert!(ok, "{out}");
    assert!(out.contains("simit") && out.contains("c1"), "{out}");
    let (ok, out) = ultron(&env, &["history", "search", "olmayankelime"]).await;
    assert!(ok && out.contains("sonuç yok"), "{out}");
    env.handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_reports_running_service() {
    let env = env().await;
    let (ok, out) = ultron(&env, &["status"]).await;
    assert!(ok, "{out}");
    assert!(out.contains("ultrond çalışıyor"), "{out}");
    env.handle.shutdown().await;
}

#[test]
fn schtasks_args_are_pure_and_quote_spaced_paths() {
    let args = schtasks_install_args(Path::new(r"C:\Program Files\Ultron\ultrond.exe"));
    assert_eq!(
        args,
        [
            "/Create", "/SC", "ONLOGON", "/RL", "LIMITED", "/TN", r"Ultron\ultrond", "/TR",
            r#""C:\Program Files\Ultron\ultrond.exe""#, "/F"
        ]
    );
    assert_eq!(
        schtasks_uninstall_args(),
        ["/Delete", "/TN", r"Ultron\ultrond", "/F"]
    );
}

#[test]
fn clap_parses_all_commands() {
    let cli = Cli::try_parse_from([
        "ultron", "chat", "--chat", "x", "--engine", "agy", "--policy", "readonly", "mesaj",
    ])
    .unwrap();
    assert!(matches!(cli.command, Cmd::Chat { engine: Some(EngineArg::Agy), .. }));
    for args in [
        vec!["ultron", "switch", "--chat", "x", "--to", "codex"],
        vec!["ultron", "events", "--chat", "x", "--follow"],
        vec!["ultron", "history", "search", "q", "--limit", "5"],
        vec!["ultron", "status"],
        vec!["ultron", "service", "install"],
        vec!["ultron", "service", "uninstall"],
    ] {
        Cli::try_parse_from(&args).unwrap_or_else(|e| panic!("{args:?}: {e}"));
    }
    assert!(Cli::try_parse_from(["ultron", "chat", "--engine", "gpt", "m"]).is_err());
}

#[test]
fn render_event_formats_lines() {
    let ev = |kind: &str, payload: serde_json::Value| StoredEvent {
        id: "e".into(),
        chat_id: "c".into(),
        session_id: None,
        seq: 1,
        ts: chrono::Utc::now(),
        kind: kind.into(),
        payload,
    };
    let text = render_event(&ev("assistant_text", serde_json::json!({"type":"assistant_text","text":"selam"})), false);
    assert_eq!(text.as_deref(), Some("selam"));
    let cmd = render_event(&ev("command", serde_json::json!({"command":"ls  -la","exit_code":0,"output_excerpt":""})), false);
    assert_eq!(cmd.as_deref(), Some("$ ls -la (çıkış 0)"));
    assert!(render_event(&ev("user_text", serde_json::json!({"text":"x"})), false).is_none());
    assert!(render_event(&ev("turn_completed", serde_json::json!({})), false).is_none());
}

/// Servis yokken istemci ultrond'u kendisi başlatır (ultrond.exe derlenmişse).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_autostarts_daemon_when_pipe_absent() {
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_ultron"));
    if !exe.with_file_name("ultrond.exe").is_file() {
        eprintln!("ultrond.exe derlenmemiş; otomatik başlatma testi atlandı");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("ultron");
    let pipe = format!(r"\\.\pipe\ultron-test-{}", uuid::Uuid::new_v4());
    let (home2, pipe2) = (home.clone(), pipe.clone());
    let out = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_ultron"))
            .arg("status")
            .env("ULTRON_HOME", home2)
            .env("ULTRON_PIPE_NAME", pipe2)
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    // Başlatılan servisi her durumda durdur.
    if let Some(pid) = text.split("pid ").nth(1).and_then(|s| s.split(',').next()) {
        let _ = Command::new("taskkill").args(["/F", "/PID", pid.trim()]).output();
    }
    assert!(out.status.success(), "{text}{}", String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("ultrond çalışıyor"), "{text}");
    std::thread::sleep(std::time::Duration::from_millis(500));
}

#[test]
fn import_commands_parse_and_render_reports() {
    use serde_json::json;
    use ultron::commands::{ImportCmd, render_project_report, render_remove_report};
    let cli = Cli::try_parse_from(["ultron", "import", "projects", "--dry-run"]).unwrap();
    assert!(matches!(cli.command, Cmd::Import { action: ImportCmd::Projects { dry_run: true } }));
    let cli = Cli::try_parse_from(["ultron", "import", "remove-chats"]).unwrap();
    assert!(matches!(cli.command, Cmd::Import { action: ImportCmd::RemoveChats { dry_run: false } }));
    let table = render_project_report(&json!({"dry_run": true, "created": 1, "notes_updated": 1, "summarizer": "null", "unmatched_sessions": 3,
        "rows": [{"path": "/sentetik/proje", "claude_sessions": 2, "codex_sessions": 1, "worker_sessions": 7, "note_updated": true, "note_status": "güncellenecek"}]}));
    assert!(table.contains("/sentetik/proje") && table.contains("evet") && table.contains("dry-run"));
    assert!(table.contains("ISCI") && table.contains("3 eşleşmeyen oturum") && table.contains(" 7 "));
    let out = render_remove_report(&json!({"dry_run": false, "chats": 3, "events": 9, "backup": "x.backup"}));
    assert!(out.contains("3 içe aktarılmış sohbet") && out.contains("Yedek: x.backup"));
}
