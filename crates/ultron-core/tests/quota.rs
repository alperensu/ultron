use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, TimeZone, Utc};
use ultron_core::Config;
use ultron_core::quota::{
    BudgetMode, HttpClient, HttpFuture, Pool, QuotaService, QuotaSource, QuotaSources, QuotaWindow,
    Token, budget_mode,
};

const SECRET: &str = "sk-ant-oat-SYNTHETIC-SECRET-123";

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 16, 12, 0, 0).unwrap()
}

fn win(used: Option<f32>, remaining_frac: f64, secs: u64) -> QuotaWindow {
    QuotaWindow {
        label: "t".into(),
        used_pct: used,
        resets_at: Some(now() + Duration::seconds((secs as f64 * remaining_frac) as i64)),
        window_secs: secs,
        source: QuotaSource::Live,
    }
}

#[test]
fn budget_mode_thresholds() {
    let s = 18000;
    // (used, kalan oran, beklenen)
    let table = [
        (0.0, 1.0, BudgetMode::Tam),      // pace 1.0
        (10.0, 0.5, BudgetMode::Tam),     // 1.8
        (50.0, 0.5, BudgetMode::Tam),     // tam 1.0 sınırı
        (60.0, 0.5, BudgetMode::Dengeli), // 0.8
        (75.0, 0.5, BudgetMode::Dengeli), // 0.5 sınırı
        (80.0, 0.5, BudgetMode::Eko),     // 0.4
        (95.0, 1.0, BudgetMode::Eko),     // used >= 90
        (90.0, 0.99, BudgetMode::Eko),    // used >= 90
        (89.0, 0.2, BudgetMode::Eko),     // pace 0.55 -> dengeli değil: 0.11/0.2=0.55
    ];
    for (used, frac, want) in table {
        let got = budget_mode(&[win(Some(used), frac, s)], now());
        // 89/0.2 -> pace 0.55 => Dengeli; satırı açıkça düzelt
        let want = if used == 89.0 { BudgetMode::Dengeli } else { want };
        assert_eq!(got, want, "used={used} frac={frac}");
    }
    // En kötü pencere belirler.
    let mixed = [win(Some(10.0), 0.5, s), win(Some(80.0), 0.5, 604800)];
    assert_eq!(budget_mode(&mixed, now()), BudgetMode::Eko);
    // Ölçüm yok -> Dengeli.
    assert_eq!(budget_mode(&[], now()), BudgetMode::Dengeli);
    assert_eq!(budget_mode(&[win(None, 0.5, s)], now()), BudgetMode::Dengeli);
}

struct FakeHttp {
    result: Result<String, String>,
    calls: AtomicUsize,
}

impl FakeHttp {
    fn new(result: Result<String, String>) -> Arc<Self> {
        Arc::new(FakeHttp {
            result,
            calls: AtomicUsize::new(0),
        })
    }
}

impl HttpClient for FakeHttp {
    fn get<'a>(&'a self, _url: &'a str, _bearer: &'a Token) -> HttpFuture<'a> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let r = self.result.clone();
        Box::pin(async move { r })
    }
}

fn setup(dir: &Path, expires_ms: i64) -> QuotaSources {
    let creds = dir.join("credentials.json");
    std::fs::write(
        &creds,
        format!(
            r#"{{"claudeAiOauth":{{"accessToken":"{SECRET}","expiresAt":{expires_ms}}}}}"#
        ),
    )
    .unwrap();
    let reset5 = (now() + Duration::hours(2)).to_rfc3339();
    let cj = dir.join("claude.json");
    std::fs::write(
        &cj,
        format!(
            r#"{{"cachedUsageUtilization":{{"utilization":{{"five_hour":{{"utilization":42.0,"resets_at":"{reset5}"}}}}}}}}"#
        ),
    )
    .unwrap();
    QuotaSources {
        claude_credentials: creds,
        claude_json: cj,
        codex_sessions: dir.join("sessions"),
    }
}

fn future_ms() -> i64 {
    (now() + Duration::hours(1)).timestamp_millis()
}

fn live_body() -> String {
    let r5 = (now() + Duration::hours(1)).to_rfc3339();
    let r7 = (now() + Duration::days(3)).to_rfc3339();
    format!(
        r#"{{"five_hour":{{"utilization":20.0,"resets_at":"{r5}"}},"seven_day":{{"utilization":30.5,"resets_at":"{r7}"}}}}"#
    )
}

#[tokio::test]
async fn live_claude_parsed_and_cached() {
    let dir = tempfile::tempdir().unwrap();
    let http = FakeHttp::new(Ok(live_body()));
    let svc = QuotaService::new(setup(dir.path(), future_ms()), http.clone());
    let cfg = Config::default();
    let r = svc.report(&cfg, now()).await;
    let c = &r.pools[0];
    assert_eq!(c.pool, Pool::Claude);
    assert_eq!(c.windows.len(), 2);
    assert!(c.windows.iter().all(|w| w.source == QuotaSource::Live));
    assert_eq!(c.windows[1].used_pct, Some(30.5));
    svc.report(&cfg, now()).await;
    assert_eq!(http.calls.load(Ordering::SeqCst), 1, "60 sn önbellek");
    // Agy: Unknown -> Dengeli
    let agy = &r.pools[2];
    assert_eq!(agy.pool, Pool::Agy);
    assert_eq!(agy.mode, BudgetMode::Dengeli);
    assert_eq!(agy.windows[0].source, QuotaSource::Unknown);
}

#[tokio::test]
async fn quota_falls_back_when_endpoint_fails() {
    let dir = tempfile::tempdir().unwrap();
    let http = FakeHttp::new(Err("HTTP 500".into()));
    let svc = QuotaService::new(setup(dir.path(), future_ms()), http);
    let r = svc.report(&Config::default(), now()).await;
    let c = &r.pools[0];
    assert_eq!(c.windows.len(), 1);
    assert_eq!(c.windows[0].source, QuotaSource::Estimated);
    assert_eq!(c.windows[0].used_pct, Some(42.0));
    assert!(c.note.as_deref().unwrap().contains("tahmini"));

    // Hiç kaynak yoksa da çökmez.
    let empty = tempfile::tempdir().unwrap();
    let svc = QuotaService::new(
        QuotaSources {
            claude_credentials: empty.path().join("yok.json"),
            claude_json: empty.path().join("yok2.json"),
            codex_sessions: empty.path().join("yok3"),
        },
        FakeHttp::new(Err("x".into())),
    );
    let r = svc.report(&Config::default(), now()).await;
    assert!(r.pools[0].windows.is_empty());
    assert_eq!(r.pools[0].mode, BudgetMode::Dengeli);
}

#[tokio::test]
async fn expired_token_means_no_live_call() {
    let dir = tempfile::tempdir().unwrap();
    let http = FakeHttp::new(Ok(live_body()));
    let past = (now() - Duration::hours(1)).timestamp_millis();
    let svc = QuotaService::new(setup(dir.path(), past), http.clone());
    let r = svc.report(&Config::default(), now()).await;
    assert_eq!(http.calls.load(Ordering::SeqCst), 0);
    assert_eq!(r.pools[0].windows[0].source, QuotaSource::Estimated);
}

#[tokio::test]
async fn live_quota_disabled_by_config() {
    let dir = tempfile::tempdir().unwrap();
    let http = FakeHttp::new(Ok(live_body()));
    let svc = QuotaService::new(setup(dir.path(), future_ms()), http.clone());
    let cfg = Config {
        claude_live_quota: false,
        ..Config::default()
    };
    let r = svc.report(&cfg, now()).await;
    assert_eq!(http.calls.load(Ordering::SeqCst), 0);
    assert!(r.pools[0].windows.iter().all(|w| w.source != QuotaSource::Live));
}

#[test]
fn codex_rate_limits_parsed_from_session_fixture() {
    let dir = tempfile::tempdir().unwrap();
    let day = dir.path().join("sessions").join("2026").join("11").join("16");
    std::fs::create_dir_all(&day).unwrap();
    let old_reset = (now() + Duration::hours(1)).timestamp();
    let new_reset = (now() + Duration::hours(4)).timestamp();
    let week_reset = (now() + Duration::days(5)).timestamp();
    let mut f = std::fs::File::create(day.join("rollout-a.jsonl")).unwrap();
    writeln!(f, "bozuk satır {{").unwrap();
    writeln!(
        f,
        r#"{{"timestamp":"2026-11-16T10:00:00Z","type":"event_msg","payload":{{"type":"token_count","rate_limits":{{"limit_id":"codex","primary":{{"used_percent":11.0,"window_minutes":300,"resets_at":{old_reset}}}}}}}}}"#
    )
    .unwrap();
    writeln!(
        f,
        r#"{{"timestamp":"2026-11-16T11:30:00Z","type":"event_msg","payload":{{"type":"token_count","rate_limits":{{"limit_id":"codex","primary":{{"used_percent":37.5,"window_minutes":300,"resets_at":{new_reset}}},"secondary":{{"used_percent":8.0,"window_minutes":10080,"resets_at":{week_reset}}}}}}}}}"#
    )
    .unwrap();
    writeln!(
        f,
        r#"{{"timestamp":"2026-11-16T11:59:00Z","type":"event_msg","payload":{{"type":"token_count","rate_limits":{{"limit_id":"other","primary":{{"used_percent":99.0,"window_minutes":300,"resets_at":{new_reset}}}}}}}}}"#
    )
    .unwrap();
    drop(f);
    let w = ultron_core::quota::codex::read_codex_windows(&dir.path().join("sessions"), now());
    assert_eq!(w.len(), 2);
    assert_eq!(w[0].used_pct, Some(37.5));
    assert_eq!(w[0].window_secs, 18000);
    assert_eq!(w[0].resets_at.unwrap().timestamp(), new_reset);
    assert_eq!(w[1].used_pct, Some(8.0));
    assert_eq!(w[1].window_secs, 604800);
    assert!(
        ultron_core::quota::codex::read_codex_windows(&dir.path().join("yok"), now()).is_empty()
    );
}

#[derive(Clone, Default)]
struct Buf(Arc<Mutex<Vec<u8>>>);

impl Write for Buf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Buf {
    type Writer = Buf;
    fn make_writer(&'a self) -> Buf {
        self.clone()
    }
}

#[tokio::test]
async fn token_never_logged() {
    let buf = Buf::default();
    let sub = tracing_subscriber::fmt()
        .with_writer(buf.clone())
        .with_max_level(tracing::Level::TRACE)
        .finish();
    let _g = tracing::subscriber::set_default(sub);
    let dir = tempfile::tempdir().unwrap();
    // Sahte istemci hata metnine token'ı sızdırmaya çalışır.
    let http = FakeHttp::new(Err(format!("bağlantı hatası Bearer {SECRET}")));
    let svc = QuotaService::new(setup(dir.path(), future_ms()), http);
    let r = svc.report(&Config::default(), now()).await;
    let logs = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    assert!(!logs.contains(SECRET), "log: {logs}");
    assert!(!format!("{r:?}").contains(SECRET));
    assert!(!serde_json::to_string(&r).unwrap().contains(SECRET));
    assert!(!format!("{:?}", Token::new(SECRET)).contains(SECRET));
}

#[tokio::test]
#[ignore = "canlı ağ çağrısı; yalnızca elle"]
async fn live_claude_quota_probe() {
    let svc = QuotaService::from_env();
    let r = svc.report(&Config::default(), Utc::now()).await;
    for w in &r.pools[0].windows {
        println!("{} used={:?} resets={:?} src={:?}", w.label, w.used_pct, w.resets_at, w.source);
    }
    println!("note={:?} mode={:?}", r.pools[0].note, r.pools[0].mode);
}
