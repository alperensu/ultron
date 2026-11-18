#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Once};
use std::time::Duration;

use ultron_core::engine::{Engine, FakeEngine};
use ultron_core::ipc::{Client, Request, Response};
use ultron_core::{Config, EngineId, Paths};
use ultrond::server::{self, ServerConfig, ServerHandle};

pub const STARTED: &str = r#"{"type":"session_started","native_session_id":"nat-1"}"#;
pub const REPLY: &str = r#"{"type":"assistant_text","text":"merhaba dünya"}"#;
pub const DONE: &str = r#"{"type":"turn_completed"}"#;

static FAKE: Once = Once::new();

/// Sahte CLI ikilisinin hazır olduğundan emin olur ve yolunu `ULTRON_FAKE_CLI`'a yazar.
pub fn ensure_fake_cli() {
    FAKE.call_once(|| {
        // SessionManager'ın iç Paths::resolve çağrıları da kullanıcı durumundan yalıtılır.
        static TEST_HOME: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
        let home = TEST_HOME.get_or_init(|| tempfile::tempdir().unwrap());
        let isolated = Paths::from_root(home.path().join("runtime"));
        isolated.ensure().unwrap();
        // SAFETY: yalnız sentetik test ikilisinin ilk motor kurulumu bu sabit yolu yazar.
        unsafe { std::env::set_var("ULTRON_HOME", &isolated.root) };
        if std::env::var_os("ULTRON_QUOTA_HOME").is_none() {
            // SAFETY: açık test override'ı korunur, aksi halde yalnız sabit sentetik yol yazılır.
            unsafe { std::env::set_var("ULTRON_QUOTA_HOME", home.path().join("quota")) };
        }
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
            let status = std::process::Command::new(cargo)
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

pub fn unique_pipe() -> String {
    format!(r"\\.\pipe\ultron-test-{}", uuid::Uuid::new_v4())
}

/// Hem Claude hem Codex için aynı fixture satırlarını yeniden oynatan motorlar.
pub fn fake_engines(dir: &Path, lines: &[&str]) -> HashMap<EngineId, Arc<dyn Engine>> {
    ensure_fake_cli();
    std::fs::create_dir_all(dir).unwrap();
    let fixture = dir.join("fixture.jsonl");
    std::fs::write(&fixture, lines.join("\n") + "\n").unwrap();
    let mut map: HashMap<EngineId, Arc<dyn Engine>> = HashMap::new();
    for id in [EngineId::Claude, EngineId::Codex] {
        map.insert(id, Arc::new(FakeEngine::from_fixture(&fixture)));
    }
    map
}

/// Sahte motorlu testlerde gerçek agy özetleyicisi çalışmasın: her tur sonunda gerçek
/// bir Gemini çağrısı yapar (2-10 sn, ağa bağlı) ve kota harcar.
pub fn config_without_agy() -> Config {
    let mut cfg = Config { summarizer: ultron_core::config::SummarizerMode::Off,
        claude_live_quota: false, ..Config::default() };
    for engine in cfg.engines.iter_mut().filter(|e| e.id == EngineId::Agy) {
        engine.enabled = false;
    }
    cfg
}

pub fn paths_in(dir: &Path) -> Paths {
    let paths = Paths::from_root(dir.join("ultron"));
    paths.ensure().unwrap();
    paths
}

pub async fn start_with(paths: &Paths, pipe: &str, lines: &[&str]) -> ServerHandle {
    server::start(ServerConfig {
        paths: paths.clone(),
        pipe_name: pipe.to_string(),
        cfg: config_without_agy(),
        engines: fake_engines(&paths.root, lines),
    })
    .await
    .unwrap()
}

pub async fn connect(h: &ServerHandle) -> Client {
    Client::connect(h.pipe_name(), h.token()).await.unwrap()
}

pub async fn next(c: &mut Client) -> Option<Response> {
    tokio::time::timeout(Duration::from_secs(20), c.next())
        .await
        .expect("yanıt zaman aşımı")
        .unwrap()
}

pub fn send_req(cwd: &Path, chat: Option<&str>, text: &str) -> Request {
    Request::Send {
        chat_id: chat.map(str::to_string),
        cwd: cwd.to_string_lossy().into_owned(),
        engine: Some(EngineId::Claude),
        text: text.to_string(),
        policy: ultron_core::ipc::PolicyArg::ReadOnly,
        elevated: false,
        project_id: None,
    }
}

pub fn abs_dir(dir: &Path) -> PathBuf {
    let d = dir.join("proje");
    std::fs::create_dir_all(&d).unwrap();
    d
}
