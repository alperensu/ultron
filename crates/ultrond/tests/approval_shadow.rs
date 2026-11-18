//! Gölge onay puanı testleri. Tüm süreçler sahte; gerçek Ollaya çağrılmaz. Veriler sentetiktir.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, TimeZone, Utc};
use tokio::sync::Notify;
use ultron_core::Db;
use ultron_core::config::EngineId;
use ultron_core::db::approvals::{ApprovalStatus, ResolvedBy};
use ultron_core::memory::contracts::Sha256Hash;
use ultron_core::policy::{Action, Decision, RiskCategory};
use ultrond::approval_shadow::{
    MAX_STDOUT_BYTES, MODEL, PROMPT_VERSION, RunFuture, ScoreError, ShadowRunner, ShadowScorer,
    ShadowSnapshot, ShadowStore, parse_output, resolve_exe, safe_exe_path, sanitized_env, child_cwd,
};

const SYNTHETIC_TOKEN: &str = "sk-ant-SYNTHETICTESTTOKEN0001";
const RAW_MARKER: &str = "RAWMARKER-NOT-LOGGED";

type Call = (PathBuf, Vec<String>, String);

#[derive(Default)]
struct FakeRunner {
    reply: Option<Result<Vec<u8>, ScoreError>>,
    hang: bool,
    release: Option<Arc<Notify>>,
    calls: Mutex<Vec<Call>>,
}

impl ShadowRunner for FakeRunner {
    fn run<'a>(&'a self, exe: &'a Path, args: &'a [String], stdin: &'a str) -> RunFuture<'a> {
        Box::pin(async move {
            self.calls
                .lock()
                .unwrap()
                .push((exe.to_path_buf(), args.to_vec(), stdin.to_string()));
            if self.hang {
                std::future::pending::<()>().await;
            }
            if let Some(gate) = &self.release {
                gate.notified().await;
            }
            self.reply.clone().unwrap_or(Err(ScoreError::Spawn))
        })
    }
}

fn setup() -> (tempfile::TempDir, Arc<Db>) {
    let tmp = tempfile::tempdir().unwrap();
    let db = Arc::new(Db::open(&tmp.path().join("shadow.db")).unwrap());
    (tmp, db)
}

fn ask(command: &str) -> (Decision, Action) {
    (
        Decision::Ask {
            reason: "sentetik".into(),
            category: RiskCategory::Delete,
        },
        Action::Shell {
            command: command.into(),
            cwd: PathBuf::from("C:/synthetic/workspace"),
        },
    )
}

fn make_approval(db: &Db, action: &Action) -> String {
    db.create_approval(None, EngineId::Claude, RiskCategory::Delete, "sentetik", action)
        .unwrap()
        .id
}

fn snapshot_for(action: &Action) -> ShadowSnapshot {
    let (decision, _) = ask("unused");
    ShadowSnapshot::from_ask(&decision, action, Path::new("C:/synthetic/workspace")).unwrap()
}

fn valid_bytes(p_safe: f64) -> Vec<u8> {
    format!(r#"{{"answers":{{"q1":{{"noul":{p_safe}}}}},"total_duration":1500000000}}"#).into_bytes()
}

fn scorer_with(runner: FakeRunner, db: Arc<Db>) -> (ShadowScorer, Arc<FakeRunner>) {
    let runner = Arc::new(runner);
    let scorer = ShadowScorer::new(
        Some(PathBuf::from("C:/synthetic/ollaya.exe")),
        runner.clone(),
        db as Arc<dyn ShadowStore>,
    );
    (scorer, runner)
}

fn replying(reply: Result<Vec<u8>, ScoreError>) -> FakeRunner {
    FakeRunner {
        reply: Some(reply),
        ..FakeRunner::default()
    }
}

fn wide() -> (DateTime<Utc>, DateTime<Utc>) {
    (
        Utc.with_ymd_and_hms(2000, 1, 1, 0, 0, 0).unwrap(),
        Utc.with_ymd_and_hms(2100, 1, 1, 0, 0, 0).unwrap(),
    )
}

fn score_count(db: &Db) -> usize {
    db.shadow_ask_rows(wide().0, wide().1)
        .unwrap()
        .iter()
        .filter(|row| row.score.is_some())
        .count()
}

#[tokio::test]
async fn valid_score_is_stored_and_first_write_wins() {
    let (_tmp, db) = setup();
    let (decision, action) = ask("cargo test -p synthetic");
    assert!(matches!(decision, Decision::Ask { .. }));
    let id = make_approval(&db, &action);
    let (scorer, runner) = scorer_with(replying(Ok(valid_bytes(0.875))), db.clone());

    let outcome = scorer.score(&id, &snapshot_for(&action)).await.unwrap();
    assert_eq!(outcome.p_safe, 0.875);
    assert_eq!(outcome.duration_ms, 1500);

    let stored = db.shadow_score_for(&id).unwrap().unwrap();
    assert_eq!(stored.p_safe, 0.875);
    assert_eq!(stored.model, MODEL);
    assert_eq!(stored.prompt_version, PROMPT_VERSION);
    assert_eq!(runner.calls.lock().unwrap().len(), 1);

    // İkinci yazım kabul edilmez; ilk puan korunur.
    let later = ultron_core::memory::contracts::ApprovalShadowScore {
        approval_id: id.clone(),
        model: MODEL.into(),
        prompt_version: PROMPT_VERSION.into(),
        p_safe: 0.1,
        measured_at: chrono::Utc::now(),
        duration_ms: 1,
        action_fingerprint: Sha256Hash::of(b"other"),
    };
    assert!(!db.insert_shadow_score(&later).unwrap());
    assert_eq!(db.shadow_score_for(&id).unwrap().unwrap().p_safe, 0.875);
}

#[tokio::test]
async fn invalid_outputs_store_nothing() {
    let cases: Vec<(&str, Vec<u8>, ScoreError)> = vec![
        ("bozuk json", b"{not json".to_vec(), ScoreError::InvalidOutput),
        (
            "nan metni",
            br#"{"answers":{"q1":{"noul":"NaN"}},"total_duration":1}"#.to_vec(),
            ScoreError::InvalidOutput,
        ),
        (
            "1 ustu",
            br#"{"answers":{"q1":{"noul":1.5}},"total_duration":1}"#.to_vec(),
            ScoreError::InvalidOutput,
        ),
        (
            "0 altı",
            br#"{"answers":{"q1":{"noul":-0.1}},"total_duration":1}"#.to_vec(),
            ScoreError::InvalidOutput,
        ),
        (
            "süre yok",
            br#"{"answers":{"q1":{"noul":0.5}}}"#.to_vec(),
            ScoreError::InvalidOutput,
        ),
        ("alan yok", br#"{"answers":{}, "total_duration":1}"#.to_vec(), ScoreError::InvalidOutput),
        ("oversize", vec![b' '; MAX_STDOUT_BYTES + 1], ScoreError::OutputTooLarge),
    ];
    for (label, bytes, expected) in cases {
        let (_tmp, db) = setup();
        let (_decision, action) = ask("cargo check");
        let id = make_approval(&db, &action);
        let (scorer, _runner) = scorer_with(replying(Ok(bytes)), db.clone());
        let err = scorer.score(&id, &snapshot_for(&action)).await.unwrap_err();
        assert_eq!(err, expected, "durum: {label}");
        assert!(!format!("{err}").contains("noul"), "durum: {label}");
        assert_eq!(score_count(&db), 0, "durum: {label}");
    }
}

#[tokio::test]
async fn parse_output_accepts_only_bounded_probability() {
    assert_eq!(parse_output(&valid_bytes(0.0)).unwrap().p_safe, 0.0);
    assert_eq!(parse_output(&valid_bytes(1.0)).unwrap().p_safe, 1.0);
    assert_eq!(parse_output(&valid_bytes(0.25)).unwrap().duration_ms, 1500);
}

#[tokio::test]
async fn runner_failures_store_nothing_and_leak_no_output() {
    let failures = [
        ScoreError::Spawn,
        ScoreError::NonZeroExit,
        ScoreError::OutputTooLarge,
        ScoreError::InvalidOutput,
    ];
    for failure in failures {
        let (_tmp, db) = setup();
        let (_decision, action) = ask("cargo check");
        let id = make_approval(&db, &action);
        let (scorer, _runner) = scorer_with(replying(Err(failure)), db.clone());
        let err = scorer.score(&id, &snapshot_for(&action)).await.unwrap_err();
        assert_eq!(err, failure);
        assert!(!format!("{err:?} {err}").contains(RAW_MARKER));
        assert_eq!(score_count(&db), 0);
    }
}

#[tokio::test(start_paused = true)]
async fn timeout_stores_nothing() {
    let (_tmp, db) = setup();
    let (_decision, action) = ask("cargo check");
    let id = make_approval(&db, &action);
    let runner = FakeRunner {
        hang: true,
        ..FakeRunner::default()
    };
    let (scorer, runner) = scorer_with(runner, db.clone());
    let err = scorer.score(&id, &snapshot_for(&action)).await.unwrap_err();
    assert_eq!(err, ScoreError::Timeout);
    assert_eq!(runner.calls.lock().unwrap().len(), 1);
    assert_eq!(score_count(&db), 0);
}

#[tokio::test]
async fn missing_exe_disables_feature_without_spawning() {
    let (_tmp, db) = setup();
    let (_decision, action) = ask("cargo check");
    let id = make_approval(&db, &action);
    let runner = Arc::new(replying(Ok(valid_bytes(0.9))));
    let scorer = ShadowScorer::new(None, runner.clone(), db.clone() as Arc<dyn ShadowStore>);
    let err = scorer.score(&id, &snapshot_for(&action)).await.unwrap_err();
    assert_eq!(err, ScoreError::Disabled);
    assert!(runner.calls.lock().unwrap().is_empty());
    assert_eq!(score_count(&db), 0);
}

#[test]
fn resolve_exe_is_none_for_missing_configured_path() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("yok.exe");
    assert!(resolve_exe(Some(missing.as_path())).is_none());
    let present = tmp.path().join("ollaya.exe");
    std::fs::write(&present, b"synthetic").unwrap();
    assert_eq!(resolve_exe(Some(present.as_path())), Some(safe_exe_path(&present).unwrap()));
    assert!(safe_exe_path(&present).unwrap().is_absolute());
}

#[tokio::test]
async fn request_uses_fixed_argv_and_masked_stdin() {
    let (_tmp, db) = setup();
    let action = Action::Shell {
        command: format!("curl --token {SYNTHETIC_TOKEN} example"),
        cwd: PathBuf::from("C:/synthetic/workspace"),
    };
    let id = make_approval(&db, &action);
    let (scorer, runner) = scorer_with(replying(Ok(valid_bytes(0.5))), db.clone());
    let snapshot = snapshot_for(&action);
    scorer.score(&id, &snapshot).await.unwrap();

    let calls = runner.calls.lock().unwrap();
    let (exe, args, stdin) = &calls[0];
    assert_eq!(exe, Path::new("C:/synthetic/ollaya.exe"));
    assert_eq!(args.len(), 8);
    assert_eq!(args[0], "run");
    assert_eq!(args[1], MODEL);
    assert_eq!(args[2], "--ask");
    assert_eq!(args[4], "--format");
    assert_eq!(args[5], "json");
    assert_eq!(args[6], "--keepalive");
    assert_eq!(args[7], "30m");
    assert!(!stdin.contains(SYNTHETIC_TOKEN));
    assert!(stdin.contains("[MASKED]"));
    assert!(stdin.contains("Workspace: C:/synthetic/workspace"));
    assert_eq!(snapshot.fingerprint(), &Sha256Hash::of(stdin.as_bytes()));
}

#[test]
fn snapshot_is_built_only_for_ask_decisions() {
    let action = Action::Shell {
        command: "cargo test".into(),
        cwd: PathBuf::from("C:/synthetic/workspace"),
    };
    let ws = Path::new("C:/synthetic/workspace");
    assert!(ShadowSnapshot::from_ask(&Decision::Allow, &action, ws).is_none());
    assert!(
        ShadowSnapshot::from_ask(
            &Decision::Deny {
                reason: "x".into()
            },
            &action,
            ws
        )
        .is_none()
    );
}

#[tokio::test]
async fn second_concurrent_call_is_busy_and_never_queues() {
    let (_tmp, db) = setup();
    let (_decision, action) = ask("cargo check");
    let first_id = make_approval(&db, &action);
    let second_id = make_approval(&db, &action);
    let release = Arc::new(Notify::new());
    let runner = FakeRunner {
        reply: Some(Ok(valid_bytes(0.9))),
        release: Some(release.clone()),
        ..FakeRunner::default()
    };
    let (scorer, runner) = scorer_with(runner, db.clone());
    let scorer = Arc::new(scorer);
    let snapshot = snapshot_for(&action);

    let background = {
        let scorer = scorer.clone();
        let snapshot = snapshot.clone();
        tokio::spawn(async move { scorer.score(&first_id, &snapshot).await })
    };
    while runner.calls.lock().unwrap().is_empty() {
        tokio::task::yield_now().await;
    }
    let busy = scorer.score(&second_id, &snapshot).await.unwrap_err();
    assert_eq!(busy, ScoreError::Busy);
    release.notify_one();
    background.await.unwrap().unwrap();
    assert_eq!(runner.calls.lock().unwrap().len(), 1);
    assert_eq!(score_count(&db), 1);
}

#[tokio::test]
async fn missing_approval_is_a_store_error_not_a_decision() {
    let (_tmp, db) = setup();
    let (_decision, action) = ask("cargo check");
    let (scorer, _runner) = scorer_with(replying(Ok(valid_bytes(0.9))), db.clone());
    let err = scorer
        .score("yok-onay", &snapshot_for(&action))
        .await
        .unwrap_err();
    assert_eq!(err, ScoreError::Store);
    assert_eq!(score_count(&db), 0);
}

#[tokio::test]
async fn hundred_percent_safe_score_never_resolves_or_changes_execution() {
    let (_tmp, db) = setup();
    let (_decision, action) = ask("cargo check");
    let id = make_approval(&db, &action);
    let (scorer, _runner) = scorer_with(replying(Ok(valid_bytes(1.0))), db.clone());
    scorer.score(&id, &snapshot_for(&action)).await.unwrap();

    let approval = db.get_approval(&id).unwrap().unwrap();
    assert_eq!(approval.status, ApprovalStatus::Pending);
    assert_eq!(approval.resolved_by, None);
    assert_eq!(approval.resolved_at, None);

    // İnsan kararı sonradan gelirse yalnız o karar yazılır; puan karar alanına dönüşmez.
    db.resolve_approval(&id, false, ResolvedBy::Desktop).unwrap();
    let approval = db.get_approval(&id).unwrap().unwrap();
    assert_eq!(approval.status, ApprovalStatus::Denied);
    assert_eq!(approval.resolved_by, Some(ResolvedBy::Desktop));
    assert_eq!(db.shadow_score_for(&id).unwrap().unwrap().p_safe, 1.0);
}

#[tokio::test]
async fn late_score_after_resolution_is_historical_only() {
    let (_tmp, db) = setup();
    let (_decision, action) = ask("cargo check");
    let id = make_approval(&db, &action);
    db.resolve_approval(&id, true, ResolvedBy::Telegram).unwrap();
    let (scorer, _runner) = scorer_with(replying(Ok(valid_bytes(0.2))), db.clone());
    scorer.score(&id, &snapshot_for(&action)).await.unwrap();

    let approval = db.get_approval(&id).unwrap().unwrap();
    assert_eq!(approval.status, ApprovalStatus::Approved);
    assert_eq!(approval.resolved_by, Some(ResolvedBy::Telegram));
    let rows = db.shadow_ask_rows(wide().0, wide().1).unwrap();
    let row = rows.iter().find(|r| r.approval_id == id).unwrap();
    assert!(row.score.as_ref().unwrap().measured_at >= row.resolved_at.unwrap());
}

#[test]
fn float_total_duration_is_accepted_and_bad_floats_rejected() {
    let ok = parse_output(br#"{"answers":{"q1":{"noul":0.5}},"total_duration":1.5e9}"#).unwrap();
    assert_eq!(ok.duration_ms, 1500);
    for bad in ["-1.0", "\"x\"", "null"] {
        let body = format!(r#"{{"answers":{{"q1":{{"noul":0.5}}}},"total_duration":{bad}}}"#);
        assert_eq!(parse_output(body.as_bytes()).unwrap_err(), ScoreError::InvalidOutput, "{bad}");
    }
}

#[test]
fn exe_path_must_be_absolute_local_and_canonical() {
    assert!(safe_exe_path(Path::new("ollaya.exe")).is_none());
    assert!(safe_exe_path(Path::new("bin/ollaya.exe")).is_none());
    assert!(safe_exe_path(Path::new(r"\\server\share\ollaya.exe")).is_none());
    assert!(safe_exe_path(Path::new(r"\\.\C:\ollaya.exe")).is_none());
    assert!(safe_exe_path(Path::new(r"\\?\UNC\server\share\ollaya.exe")).is_none());
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("sub")).unwrap();
    std::fs::write(tmp.path().join("ollaya.exe"), b"x").unwrap();
    let exe = tmp.path().join("sub").join("..").join("ollaya.exe");
    let resolved = safe_exe_path(&exe).unwrap();
    assert!(!resolved.to_string_lossy().contains(".."));
}

#[test]
fn child_environment_is_allowlisted() {
    let env = sanitized_env(|k| match k {
        "SystemRoot" => Some(r"C:\Windows".into()),
        "TEMP" => Some(r"C:\Temp".into()),
        "ULTRON_SECRET" | "ANTHROPIC_API_KEY" | "PATH" => Some("gizli".into()),
        _ => None,
    });
    let keys: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(keys, ["SystemRoot", "TEMP", "PATH"]);
    let path = &env.iter().find(|(k, _)| k == "PATH").unwrap().1;
    assert_eq!(Path::new(path), Path::new(r"C:\Windows").join("System32"));
}

#[test]
fn huge_action_text_is_cropped_before_masking_and_then_to_2000_chars() {
    let huge = format!("echo {SYNTHETIC_TOKEN} {}", "x".repeat(300_000));
    let (_, action) = ask(&huge);
    let snap = snapshot_for(&action);
    assert!(!snap.state_text().contains(SYNTHETIC_TOKEN));
    let action_line = snap.state_text().split("Action: ").nth(1).unwrap();
    assert!(action_line.chars().count() <= 2000);
}

#[test]
fn child_env_includes_appdata_programdata_and_cwd_is_system32() {
    let get = |k: &str| match k {
        "SystemRoot" => Some(r"C:\Windows".into()),
        "APPDATA" => Some(r"C:\Users\x\AppData\Roaming".into()),
        "ProgramData" => Some(r"C:\ProgramData".into()),
        _ => None,
    };
    let keys: Vec<String> = sanitized_env(get).into_iter().map(|(k, _)| k).collect();
    assert!(keys.contains(&"APPDATA".to_string()) && keys.contains(&"ProgramData".to_string()), "{keys:?}");
    assert_eq!(child_cwd(get).unwrap(), Path::new(r"C:\Windows").join("System32"));
    assert_eq!(child_cwd(|_| None), None);
}
