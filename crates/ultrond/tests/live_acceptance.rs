//! Aşama 1 canlı kabul testleri. Gerçek `claude` ve `agy` ikililerini çalıştırır ve kota harcar;
//! bu yüzden hepsi `#[ignore]` işaretlidir. Çalıştırma örneği:
//! `cargo test -p ultrond --test live_acceptance -- --ignored live_claude_to_agy_handoff --nocapture`
//!
//! Her test kendi geçici `ULTRON_HOME` ve proje klasörünü kullanır. Ham olay dökümleri
//! (repo dışında) `%LOCALAPPDATA%\ultron\fixtures-private\acceptance` altına yazılır.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;
use tempfile::TempDir;
use ultron_core::engine::{ToolPolicy, TurnOutcome};
use ultron_core::handoff::{HandoffPack, build_handoff_before};
use ultron_core::{Config, Db, EngineId, EventLog, Paths, SessionManager, StoredEvent, SwitchReason};

/// Turn 1 (Claude): karar ve sıradaki adım.
const PROMPT_DECISION: &str = "Bu projede rapor çıktısı için format seçiyoruz. KARAR: CSV kullanacağız çünkü Excel'de açılacak. Sıradaki adım: export fonksiyonunu yazmak. Sadece ANLADIM yaz.";
/// Turn 2 (devir sonrası motor): karar hatırlatma sorusu.
const PROMPT_RECALL: &str = "Hangi formatta karar verdik, neden, sıradaki adım ne? Tek satır.";
/// Araç kısıtlama ölçümü.
const PROMPT_DELETE: &str = "Bu klasördeki test.txt dosyasını sil.";
/// Claude turları için model takma adı (ucuz tur).
const CLAUDE_MODEL: &str = "haiku";
/// Codex modeli; `codex` varsayılan adı güncel masaüstü CLI ikilisine otomatik çözülür.
const CODEX_MODEL: &str = "gpt-6.1-sol";
/// Devir paketinde aynen verilen son mesaj sayısı (`session.rs` ile aynı).
const HANDOFF_RECENT_MESSAGES: usize = 6;

struct Harness {
    mgr: SessionManager,
    db: Arc<Db>,
    cfg: Config,
}

/// Geçici `ULTRON_HOME` altında gerçek motor sürücüleriyle bir `SessionManager` kurar.
fn harness(home: &Path) -> Harness {
    let paths = Paths::from_root(home.join("ultron"));
    paths.ensure().expect("ULTRON_HOME dizinleri oluşturulamadı");
    let mut cfg = Config::default();
    for engine in cfg.engines.iter_mut() {
        if engine.id == EngineId::Claude {
            engine.model = Some(CLAUDE_MODEL.to_string());
        }
        if engine.id == EngineId::Codex {
            engine.model = Some(CODEX_MODEL.to_string());
        }
    }
    let db = Arc::new(Db::open(&paths.db).expect("veritabanı açılamadı"));
    let log = Arc::new(EventLog::new(db.clone(), paths));
    let engines = ultrond::build_engines(&cfg);
    let mgr = SessionManager::new(db.clone(), log, cfg.clone(), engines);
    Harness { mgr, db, cfg }
}

/// Git deposu olan geçici proje klasörü.
fn make_project(dir: &Path) {
    fs::create_dir_all(dir).expect("proje klasörü oluşturulamadı");
    let status = std::process::Command::new("git")
        .arg("init")
        .arg("-q")
        .current_dir(dir)
        .status()
        .expect("git başlatılamadı");
    assert!(status.success(), "git init başarısız");
}

fn kind_text(ev: &StoredEvent) -> Option<&str> {
    match ev.kind.as_str() {
        "user_text" | "assistant_text" => ev.payload.get("text").and_then(Value::as_str),
        _ => None,
    }
}

/// `after` sıra numarasından sonraki asistan metinlerini birleştirir (artımlar boşluksuz).
fn assistant_answer(events: &[StoredEvent], after: i64) -> String {
    events
        .iter()
        .filter(|e| e.seq > after && e.kind == "assistant_text")
        .filter_map(|e| e.payload.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .concat()
}

fn print_usage(label: &str, events: &[StoredEvent], after: i64) {
    for ev in events.iter().filter(|e| e.seq > after && e.kind == "usage") {
        println!("[{label}] usage: {}", ev.payload);
    }
}

/// Ham olay dökümünü repo dışındaki özel klasöre yazar. `LOCALAPPDATA` yoksa atlanır.
fn dump_private(name: &str, events: &[StoredEvent]) {
    let Some(base) = std::env::var_os("LOCALAPPDATA") else {
        println!("LOCALAPPDATA yok; ham döküm yazılmadı");
        return;
    };
    let dir = PathBuf::from(base).join("ultron").join("fixtures-private").join("acceptance");
    if fs::create_dir_all(&dir).is_err() {
        println!("özel klasör oluşturulamadı; ham döküm yazılmadı");
        return;
    }
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let file = dir.join(format!("{name}-{stamp}.json"));
    let body = serde_json::to_string_pretty(events).expect("olaylar serileştirilemedi");
    fs::write(&file, body).expect("ham döküm yazılamadı");
    println!("ham döküm yazıldı (repo dışında): {}", file.display());
}

/// Ortak devir senaryosu: Claude turu → motor değişimi → ikinci motorda hatırlatma turu.
/// Dönüş: ikinci turun cevabı ve ölçüm verileri.
struct HandoffRun {
    answer: String,
    transcript_chars: usize,
    pack: HandoffPack,
    prompt2_chars: usize,
    events: Vec<StoredEvent>,
    turn2_start_seq: i64,
}

async fn run_handoff(second: EngineId, chat: &str) -> HandoffRun {
    let home = TempDir::new().expect("geçici klasör");
    let proj = home.path().join("proj");
    make_project(&proj);
    let h = harness(home.path());

    let t1 = h
        .mgr
        .send(chat, &proj, EngineId::Claude, PROMPT_DECISION, ToolPolicy::ReadOnly)
        .await
        .expect("tur 1 çalıştırılamadı");
    assert_eq!(t1, TurnOutcome::Completed, "tur 1 tamamlanmadı");
    h.mgr
        .switch(chat, second, SwitchReason::User)
        .await
        .expect("motor değişimi başarısız");

    // Turn 2 öncesi döküm ve paket: ölçüm, gönderilen paketle aynı anda alınır.
    let before = h.db.events_for_chat(chat, None).expect("olaylar okunamadı");
    let transcript_chars: usize = before
        .iter()
        .filter_map(kind_text)
        .map(|t| t.chars().count())
        .sum();
    let pack = build_handoff_before(
        &h.db,
        chat,
        None,
        h.cfg.context_budget_chars,
        HANDOFF_RECENT_MESSAGES,
        None,
        Some(&proj),
    )
    .expect("devir paketi kurulamadı");
    let boundary = before.last().map_or(0, |e| e.seq);

    let t2 = h
        .mgr
        .send(chat, &proj, second, PROMPT_RECALL, ToolPolicy::ReadOnly)
        .await
        .expect("tur 2 çalıştırılamadı");
    assert_eq!(t2, TurnOutcome::Completed, "tur 2 tamamlanmadı");

    let events = h.db.events_for_chat(chat, None).expect("olaylar okunamadı");
    let answer = assistant_answer(&events, boundary);
    HandoffRun {
        answer,
        transcript_chars,
        pack,
        prompt2_chars: PROMPT_RECALL.chars().count(),
        events,
        turn2_start_seq: boundary + 1,
    }
}

/// Devir sonucunu doğrular ve ölçümleri yazdırır.
fn check_handoff(label: &str, run: &HandoffRun, second: EngineId) {
    let lower = run.answer.to_lowercase();
    println!("[{label}] tur 2 cevabı: {}", run.answer);
    println!("[{label}] dökümde toplam metin karakteri (a): {}", run.transcript_chars);
    println!(
        "[{label}] devir paketi karakteri (b): {} (kırpıldı: {}, bölümler: {:?})",
        run.pack.text.chars().count(),
        run.pack.truncated,
        run.pack.sections
    );
    println!("[{label}] tur 2 istemi karakteri (paket + ayraç + soru): {}", run.pack.text.chars().count() + 2 + run.prompt2_chars);
    print_usage(label, &run.events, 0);

    let switched = run.events.iter().any(|e| {
        e.kind == "engine_switched"
            && e.payload.get("from").and_then(Value::as_str) == Some("claude")
            && e.payload.get("to").and_then(Value::as_str)
                == Some(match second {
                    EngineId::Agy => "agy",
                    EngineId::Codex => "codex",
                    EngineId::Claude => "claude",
                })
    });
    assert!(switched, "EngineSwitched olayı bulunamadı");
    assert!(lower.contains("csv"), "cevapta CSV yok: {}", run.answer);
    assert!(lower.contains("excel"), "cevapta Excel yok: {}", run.answer);
    assert!(
        lower.contains("export") || lower.contains("dışa aktar"),
        "cevapta export/dışa aktar yok: {}",
        run.answer
    );
}

/// Turn 2 olaylarının tur başlangıcına göre zamanlarını yazdırır (akış gözlemi).
fn print_timing(label: &str, run: &HandoffRun) {
    let turn: Vec<&StoredEvent> = run
        .events
        .iter()
        .filter(|e| e.seq >= run.turn2_start_seq)
        .collect();
    let Some(start) = turn.iter().find(|e| e.kind == "user_text").map(|e| e.ts) else {
        println!("[{label}] tur 2 başlangıç olayı yok");
        return;
    };
    for ev in &turn {
        let offset = (ev.ts - start).num_milliseconds();
        let len = kind_text(ev).map_or(0, |t| t.chars().count());
        println!("[{label}] +{offset:>6} ms  {:<15} metin_karakteri={len}", ev.kind);
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "canlı claude ve agy çağrısı yapar (kota harcar)"]
async fn live_claude_to_agy_handoff() {
    let chat = "accept-handoff-agy";
    let run = run_handoff(EngineId::Agy, chat).await;
    dump_private("claude-to-agy-handoff", &run.events);
    check_handoff("agy", &run, EngineId::Agy);
    print_timing("agy-akis", &run);
}

/// Codex kotası nedeniyle bu sürümde ÇALIŞTIRILMADI (koordinatör kararı). Kod, Codex
/// motoruyla aynı senaryo olarak derlenir; yalnız `--ignored` ile elle çalıştırılabilir.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "Codex kotası nedeniyle çalıştırılmadı; kota harcar"]
async fn live_claude_to_codex_handoff() {
    let chat = "accept-handoff-codex";
    let run = run_handoff(EngineId::Codex, chat).await;
    dump_private("claude-to-codex-handoff", &run.events);
    check_handoff("codex", &run, EngineId::Codex);
}

/// Her motor için ReadOnly politikasında dosya silme isteği. Dosyanın kalıp kalmadığını ve
/// motorun ürettiği araç çağrılarını yazdırır. Sonuç bir kabul kapısı değil, ölçümdür.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "canlı claude ve agy çağrısı yapar (kota harcar)"]
async fn live_tool_restriction_probe() {
    for engine in [EngineId::Claude, EngineId::Agy] {
        let label = match engine {
            EngineId::Claude => "claude",
            EngineId::Agy => "agy",
            EngineId::Codex => "codex",
        };
        let home = TempDir::new().expect("geçici klasör");
        let proj = home.path().join("proj");
        make_project(&proj);
        let target = proj.join("test.txt");
        fs::write(&target, "sentetik test verisi\n").expect("test.txt yazılamadı");
        let h = harness(home.path());
        let chat = format!("probe-{label}");

        let result = h
            .mgr
            .send(&chat, &proj, engine, PROMPT_DELETE, ToolPolicy::ReadOnly)
            .await;
        let exists = target.exists();
        println!("[{label}] send sonucu: {result:?}");
        println!("[{label}] test.txt hâlâ var mı: {exists}");

        let events = h.db.events_for_chat(&chat, None).expect("olaylar okunamadı");
        for ev in &events {
            match ev.kind.as_str() {
                "tool_call" => {
                    let tool = ev.payload.get("tool").and_then(Value::as_str).unwrap_or("?");
                    println!("[{label}] tool_call: {tool}");
                }
                "tool_result" => println!(
                    "[{label}] tool_result: is_error={}",
                    ev.payload.get("is_error").and_then(Value::as_bool).unwrap_or(false)
                ),
                "command" => println!("[{label}] command: {}", ev.payload),
                "file_changed" => println!("[{label}] file_changed: {}", ev.payload),
                "assistant_text" => println!(
                    "[{label}] assistant_text: {}",
                    kind_text(ev).unwrap_or("").chars().take(300).collect::<String>()
                ),
                "error" | "quota_exhausted" => println!("[{label}] {}: {}", ev.kind, ev.payload),
                _ => {}
            }
        }
        print_usage(label, &events, 0);
        dump_private(&format!("tool-restriction-{label}"), &events);
    }
}
