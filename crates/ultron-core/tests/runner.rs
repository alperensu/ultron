use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use ultron_core::EngineEvent;
use ultron_core::engine::runner::resolve_binary_in;
use ultron_core::engine::{FakeEngine, ToolPolicy, TurnOutcome, TurnRequest, run_turn};

fn engine(args: &[&str], append_prompt: bool) -> FakeEngine {
    // Cargo, entegrasyon testi derlenirken ikiliyi de üretir; yolu ortam değişkeniyle verir.
    // SAFETY: testler yalnızca bu değişkeni aynı değerle yazar.
    unsafe { std::env::set_var("ULTRON_FAKE_CLI", env!("CARGO_BIN_EXE_ultron-fake-cli")) };
    FakeEngine::with_mode(args, append_prompt)
}

fn request(cwd: &Path, prompt: &str) -> TurnRequest {
    TurnRequest {
        chat_id: "c1".into(),
        cwd: cwd.to_path_buf(),
        prompt: prompt.into(),
        resume_native_id: None,
        tool_policy: ToolPolicy::ReadOnly,
    }
}

async fn collect(mut rx: mpsc::Receiver<EngineEvent>) -> Vec<EngineEvent> {
    let mut out = Vec::new();
    while let Some(ev) = rx.recv().await {
        out.push(ev);
    }
    out
}

#[tokio::test]
async fn runner_streams_fixture_events() {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake/three_events.jsonl");
    let eng = {
        let _ = engine(&["echo"], false);
        FakeEngine::from_fixture(&fixture)
    };
    let tmp = tempfile::tempdir().unwrap();
    let (tx, rx) = mpsc::channel(16);
    let outcome = run_turn(&eng, request(tmp.path(), "x"), tx, CancellationToken::new())
        .await
        .unwrap();
    let events = collect(rx).await;
    assert_eq!(outcome, TurnOutcome::Completed);
    assert_eq!(events.len(), 3);
    assert!(matches!(events[0], EngineEvent::SessionStarted { .. }));
    assert!(matches!(events[2], EngineEvent::TurnCompleted));
}

#[tokio::test]
async fn runner_child_killed_midstream_keeps_events() {
    let eng = engine(&["partial", "2", "137"], false);
    let tmp = tempfile::tempdir().unwrap();
    let (tx, rx) = mpsc::channel(16);
    let outcome = run_turn(&eng, request(tmp.path(), "x"), tx, CancellationToken::new())
        .await
        .unwrap();
    let events = collect(rx).await;
    assert_eq!(events.len(), 2, "yarım satır atılmalı: {events:?}");
    assert!(matches!(
        outcome,
        TurnOutcome::Failed {
            exit_code: Some(137),
            ..
        }
    ));
}

#[cfg(windows)]
fn pid_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    const STILL_ACTIVE: u32 = 259;
    // SAFETY: tutacak açılır, sorgulanır ve kapatılır.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return false;
        }
        let mut code = 0u32;
        let ok = GetExitCodeProcess(h, &mut code);
        CloseHandle(h);
        ok != 0 && code == STILL_ACTIVE
    }
}

#[cfg(windows)]
#[tokio::test]
async fn runner_cancel_terminates_tree() {
    let tmp = tempfile::tempdir().unwrap();
    let pidfile = tmp.path().join("pids.txt");
    let eng = engine(&["sleep", &pidfile.to_string_lossy()], false);
    let (tx, rx) = mpsc::channel(16);
    let cancel = CancellationToken::new();
    let cwd = tmp.path().to_path_buf();
    let c2 = cancel.clone();
    let task = tokio::spawn(async move { run_turn(&eng, request(&cwd, "x"), tx, c2).await });

    let started = Instant::now();
    let pids = loop {
        if let Ok(text) = std::fs::read_to_string(&pidfile) {
            let v: Vec<u32> = text.lines().filter_map(|l| l.trim().parse().ok()).collect();
            if v.len() == 2 {
                break v;
            }
        }
        assert!(started.elapsed() < Duration::from_secs(10), "pid dosyası yazılmadı");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert!(pids.iter().all(|p| pid_alive(*p)));

    cancel.cancel();
    let outcome = task.await.unwrap().unwrap();
    assert_eq!(outcome, TurnOutcome::Cancelled);
    drop(rx);

    let deadline = Instant::now() + Duration::from_secs(2);
    while pids.iter().any(|p| pid_alive(*p)) {
        assert!(Instant::now() < deadline, "süreç ağacı 2 sn içinde ölmedi");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[test]
fn resolve_binary_leaves_explicit_paths_and_extensions() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("tool.exe"), "").unwrap();
    let dirs = vec![tmp.path().to_path_buf()];
    // Uzantılı ya da yol içeren ad olduğu gibi kalır.
    assert_eq!(resolve_binary_in("tool.exe", &dirs).unwrap(), "tool.exe");
    assert_eq!(resolve_binary_in("C:/x/tool", &dirs).unwrap(), "C:/x/tool");
    // Bulunamayan ad olduğu gibi döner; hata süreç başlatmada çıkar.
    assert_eq!(resolve_binary_in("yok-boyle-bir-arac", &dirs).unwrap(), "yok-boyle-bir-arac");
}

#[cfg(windows)]
#[test]
fn resolve_binary_prefers_exe_and_rejects_cmd_shims() {
    let exe_dir = tempfile::tempdir().unwrap();
    std::fs::write(exe_dir.path().join("fakeeng.exe"), "").unwrap();
    let dirs = vec![exe_dir.path().to_path_buf()];
    let resolved = resolve_binary_in("fakeeng", &dirs).unwrap();
    assert!(resolved.ends_with("fakeeng.exe"), "{resolved}");

    let shim_dir = tempfile::tempdir().unwrap();
    std::fs::write(shim_dir.path().join("shimeng.cmd"), "@echo off").unwrap();
    let dirs = vec![shim_dir.path().to_path_buf()];
    let err = resolve_binary_in("shimeng", &dirs).unwrap_err();
    assert!(err.to_string().contains(".exe"), "{err}");

    // Aynı PATH'te .exe varsa .cmd'den önce gelir.
    std::fs::write(shim_dir.path().join("both.exe"), "").unwrap();
    std::fs::write(shim_dir.path().join("both.cmd"), "@echo off").unwrap();
    let resolved = resolve_binary_in("both", &dirs).unwrap();
    assert!(resolved.ends_with("both.exe"), "{resolved}");
}

#[tokio::test]
async fn runner_clean_exit_processes_unterminated_final_json() {
    let eng = engine(&["tailjson", "0"], false);
    let tmp = tempfile::tempdir().unwrap();
    let (tx, rx) = mpsc::channel(16);
    let outcome = run_turn(&eng, request(tmp.path(), "x"), tx, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(outcome, TurnOutcome::Completed);
    let events = collect(rx).await;
    assert_eq!(
        events,
        vec![
            EngineEvent::AssistantText { text: "satır 0".into() },
            EngineEvent::AssistantText { text: "son".into() },
        ]
    );
}

#[tokio::test]
async fn runner_crash_discards_unterminated_final_json() {
    let eng = engine(&["tailjson", "1"], false);
    let tmp = tempfile::tempdir().unwrap();
    let (tx, rx) = mpsc::channel(16);
    let outcome = run_turn(&eng, request(tmp.path(), "x"), tx, CancellationToken::new())
        .await
        .unwrap();
    assert!(matches!(outcome, TurnOutcome::Failed { exit_code: Some(1), .. }));
    let events = collect(rx).await;
    assert_eq!(
        events,
        vec![EngineEvent::AssistantText { text: "satır 0".into() }],
        "çökmede son satır atılmalı"
    );
}

#[tokio::test]
async fn runner_child_exit_with_lingering_grandchild_returns() {
    let eng = engine(&["linger"], false);
    let tmp = tempfile::tempdir().unwrap();
    let (tx, rx) = mpsc::channel(16);
    let started = Instant::now();
    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        run_turn(&eng, request(tmp.path(), "x"), tx, CancellationToken::new()),
    )
    .await
    .expect("torun boru yüzünden 5 sn'den uzun bekledi")
    .unwrap();
    assert_eq!(outcome, TurnOutcome::Completed);
    assert!(started.elapsed() < Duration::from_secs(5));
    let events = collect(rx).await;
    assert_eq!(events.len(), 1, "olaylar: {events:?}");
}

#[tokio::test]
async fn runner_passes_unicode_and_spaced_cwd() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("roblox 2");
    std::fs::create_dir(&cwd).unwrap();
    let prompt = "Ödeme ekranı çalışsın mı?";
    let eng = engine(&["echo"], true);
    let (tx, rx) = mpsc::channel(16);
    let outcome = run_turn(&eng, request(&cwd, prompt), tx, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(outcome, TurnOutcome::Completed);
    let events = collect(rx).await;
    let EngineEvent::AssistantText { text } = &events[0] else {
        panic!("beklenmeyen olay: {events:?}");
    };
    let echoed: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(echoed["args"], serde_json::json!([prompt]));
    let echoed_cwd = PathBuf::from(echoed["cwd"].as_str().unwrap());
    assert_eq!(
        std::fs::canonicalize(echoed_cwd).unwrap(),
        std::fs::canonicalize(&cwd).unwrap()
    );
}

#[tokio::test]
async fn runner_overlong_line_is_skipped() {
    let eng = engine(&["longline"], false);
    let tmp = tempfile::tempdir().unwrap();
    let (tx, rx) = mpsc::channel(16);
    let outcome = run_turn(&eng, request(tmp.path(), "x"), tx, CancellationToken::new())
        .await
        .unwrap();
    let events = collect(rx).await;
    assert_eq!(outcome, TurnOutcome::Completed);
    let errors = events
        .iter()
        .filter(|e| matches!(e, EngineEvent::Error { message } if message == "satır çok uzun, atlandı"))
        .count();
    assert_eq!(errors, 1, "olaylar: {events:?}");
    assert_eq!(events.len(), 2, "olaylar: {events:?}");
    assert!(events.iter().any(|e| matches!(e, EngineEvent::AssistantText { text } if text == "tamam")));
}

#[tokio::test]
async fn runner_output_budget_fails_turn() {
    let eng = engine(&["flood"], false);
    let tmp = tempfile::tempdir().unwrap();
    let (tx, rx) = mpsc::channel(64);
    // Alıcı aynı anda tüketmeli; yoksa gönderici kanal dolunca bekler.
    let drain = tokio::spawn(collect(rx));
    let outcome = tokio::time::timeout(
        Duration::from_secs(20),
        run_turn(&eng, request(tmp.path(), "x"), tx, CancellationToken::new()),
    )
    .await
    .expect("çıktı bütçesi aşıldıktan sonra tur sonlanmadı")
    .unwrap();
    drain.await.unwrap();
    match outcome {
        TurnOutcome::Failed { exit_code: None, stderr_excerpt } => {
            assert_eq!(stderr_excerpt, "çıktı bütçesi aşıldı");
        }
        other => panic!("beklenmeyen sonuç: {other:?}"),
    }
}
