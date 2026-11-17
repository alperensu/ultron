//! Proje keşfi, geçmiş notu ve içe aktarılmış sohbetleri kaldırma testleri (yalnız sentetik veri).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::Utc;
use serde_json::{Value, json};
use ultron_core::db::{Db, StoredEvent};
use ultron_core::import::discovery::{Filters, discover};
use ultron_core::import::{ImportState, import_claude_history, import_projects, remove_imported_chats};
use ultron_core::summarizer::{Summarizer, SummaryFuture, SummaryPatch};
use ultron_core::workstate::WorkState;

/// Çağrıları ve gördüğü metinleri kaydeden sahte özetleyici (gerçek motor yok).
struct Recorder {
    seen: Arc<Mutex<Vec<String>>>,
    goal: &'static str,
    done: &'static str,
    kind: &'static str,
}

impl Summarizer for Recorder {
    fn refresh<'a>(&'a self, _state: &'a WorkState, recent: &'a [StoredEvent]) -> SummaryFuture<'a> {
        let text: String = recent.iter().map(|e| e.payload.to_string()).collect();
        self.seen.lock().unwrap().push(text);
        let (goal, done) = (self.goal, self.done);
        Box::pin(async move {
            Ok(SummaryPatch {
                goal: Some(goal.into()),
                done: Some(vec![done.into()]),
                ..SummaryPatch::default()
            })
        })
    }
    fn kind(&self) -> &'static str {
        self.kind
    }
}

fn recorder() -> (Recorder, Arc<Mutex<Vec<String>>>) {
    recorder_with("SENTETIK-HEDEF", "SENTETIK-YAPILDI", "fake")
}

fn recorder_with(goal: &'static str, done: &'static str, kind: &'static str) -> (Recorder, Arc<Mutex<Vec<String>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    (Recorder { seen: seen.clone(), goal, done, kind }, seen)
}

/// Her çağrıda hata veren özetleyici (devre kesici testi).
struct Failing {
    calls: Arc<Mutex<usize>>,
}

impl Summarizer for Failing {
    fn refresh<'a>(&'a self, _s: &'a WorkState, _r: &'a [StoredEvent]) -> SummaryFuture<'a> {
        *self.calls.lock().unwrap() += 1;
        Box::pin(async { Err(anyhow::anyhow!("motor\u{7}çöktü sk-ant-api03-abcdefghijklmnop\nsahte satır")) })
    }
    fn kind(&self) -> &'static str {
        "fake"
    }
}

fn write_lines(path: &Path, lines: &[Value]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(path, body).unwrap();
}

fn claude_session(home: &Path, dir: &str, name: &str, cwd: &str, first: &str, extra: &str) -> PathBuf {
    let path = home.join(".claude/projects").join(dir).join(format!("{name}.jsonl"));
    write_lines(
        &path,
        &[
            json!({"type":"summary","summary":"x"}),
            json!({"type":"user","cwd":cwd,"timestamp":"2026-11-01T10:00:00Z","message":{"role":"user","content":first}}),
            json!({"type":"assistant","cwd":cwd,"timestamp":"2026-11-01T10:00:05Z","message":{"role":"assistant","content":[
                {"type":"text","text":format!("yanit {extra}")},
                {"type":"tool_use","name":"Bash","input":{"command":"ARAC-CIKTISI-SIZMAMALI"}}]}}),
            json!({"type":"user","cwd":cwd,"timestamp":"2026-11-01T10:00:09Z","message":{"role":"user","content":[
                {"type":"tool_result","content":"ARAC-SONUCU-SIZMAMALI"}]}}),
        ],
    );
    path
}

fn codex_session(home: &Path, rel: &str, cwd: &str, first: &str) {
    let path = home.join(".codex").join(rel);
    write_lines(
        &path,
        &[
            json!({"type":"session_meta","payload":{"id":"s1","cwd":cwd}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[
                {"type":"input_text","text":"<environment_context>enjekte</environment_context>"}]}}),
            json!({"type":"response_item","timestamp":"2026-11-02T10:00:00Z","payload":{"type":"message","role":"user","content":[
                {"type":"input_text","text":first}]}}),
            json!({"type":"response_item","timestamp":"2026-11-02T10:00:03Z","payload":{"type":"message","role":"assistant","content":[
                {"type":"output_text","text":"codex yaniti"}]}}),
            json!({"type":"response_item","payload":{"type":"function_call_output","output":"ARAC-SONUCU-SIZMAMALI"}}),
        ],
    );
}

struct World {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    filters: Filters,
    good: PathBuf,
    codex_only: PathBuf,
    worker_only: PathBuf,
    main_repo: PathBuf,
    work: PathBuf,
    outside: PathBuf,
}

fn mkdir(p: &Path) -> String {
    std::fs::create_dir_all(p).unwrap();
    p.to_string_lossy().into_owned()
}

const WORKER_BRIEF: &str = "You are a scoped worker directed by an external coordinator.\n\nGoal: ISCI-GOREVI-X";

fn world() -> World {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let home = root.join("home");
    let faketemp = home.join("faketemp");
    let ultron_data = home.join("ultrondata");
    mkdir(&faketemp);
    mkdir(&ultron_data);
    let work = home.join("work");
    let good = work.join("iyi-proje");
    let codex_only = work.join("sadece-codex");
    let worker_only = work.join("isci-proje");
    let main_repo = work.join("anarepo");
    let good_s = mkdir(&good);
    let codex_s = mkdir(&codex_only);
    let worker_s = mkdir(&worker_only);
    let repo_s = mkdir(&main_repo.join(".git").join("worktrees").join("bagli"));
    let _ = repo_s;

    // İyi proje: 1 gerçek Claude oturumu + 1 işçi oturumu + yinelenen subagents/ dosyası.
    claude_session(&home, "iyi", "s1", &good_s, "merhaba proje", "bir");
    claude_session(&home, "iyi", "worker", &good_s, WORKER_BRIEF, "ISCI-RAPORU");
    write_lines(
        &home.join(".claude/projects/iyi/s1/subagents/agent-1.jsonl"),
        &[json!({"type":"user","cwd":good_s,"isSidechain":true,"message":{"role":"user","content":"alt ajan"}})],
    );
    // Yalnız işçi oturumlarıyla çalışılmış proje.
    codex_session(&home, "sessions/2026/11/03/rollout-w1.jsonl", &worker_s, WORKER_BRIEF);
    codex_session(&home, "sessions/2026/11/03/rollout-w2.jsonl", &worker_s, WORKER_BRIEF);

    // Ana checkout ve ona bağlı worktree'ler.
    let repo_s = main_repo.to_string_lossy().into_owned();
    std::fs::write(
        main_repo.join(".git/worktrees/bagli/commondir"),
        "../..\n",
    )
    .unwrap();
    let wt1 = mkdir(&main_repo.join(".claude").join("worktrees").join("w1"));
    claude_session(&home, "wt", "s1", &wt1, "worktree istegi", "x");
    let linked = work.join("bagli-worktree");
    mkdir(&linked);
    std::fs::write(
        linked.join(".git"),
        format!("gitdir: {}\n", main_repo.join(".git/worktrees/bagli").display()),
    )
    .unwrap();
    claude_session(&home, "linked", "s1", &linked.to_string_lossy(), "bagli worktree", "x");
    // Codex worktree'si diskte yok: ad (`anarepo`) ile ana klasöre eşlenir.
    let cwt = home.join(".codex/worktrees/ab12/anarepo");
    codex_session(&home, "sessions/2026/11/02/rollout-wt.jsonl", &cwt.to_string_lossy(), "codex worktree");

    // Elenecekler.
    let temp_s = mkdir(&faketemp.join("scratch"));
    claude_session(&home, "tmp", "s1", &temp_s, "gecici", "x");
    let ud_s = mkdir(&ultron_data.join("workspaces").join("c1"));
    claude_session(&home, "ud", "s1", &ud_s, "ultron verisi", "x");
    let lb_s = mkdir(&home.join(".local").join("bin"));
    claude_session(&home, "lb", "s1", &lb_s, "local bin", "x");
    let gone = work.join("silindi");
    claude_session(&home, "gone", "s1", &gone.to_string_lossy(), "yok", "x");
    // Profil dışı klasör (katalogda yok): kabul edilmez.
    let outside = mkdir(&root.join("disarida").join("proje"));
    claude_session(&home, "out", "s1", &outside, "profil disi", "x");
    let outside = PathBuf::from(outside);
    // Kapsayıcılar: ev dizini ve `work` (başka projelerin atası, git kökü değil).
    let home_s = mkdir(&home);
    claude_session(&home, "home", "s1", &home_s, "ev dizini", "x");
    claude_session(&home, "cont", "s1", &work.to_string_lossy(), "kapsayici", "x");
    // Codex: `\\?\` önekli cwd, arşiv dizini ve işçi oturumu.
    codex_session(&home, "sessions/2026/11/02/rollout-a.jsonl", &format!(r"\\?\{codex_s}"), "codex istegi");
    codex_session(&home, "archived_sessions/rollout-b.jsonl", &good_s, "arsiv istegi");
    codex_session(&home, "sessions/2026/11/02/rollout-sub.jsonl", &codex_s, WORKER_BRIEF);
    let _ = repo_s;
    let filters = Filters {
        home: home.clone(),
        temp_dirs: vec![faketemp, ultron_data],
    };
    World {
        _tmp: tmp,
        home,
        filters,
        good,
        codex_only,
        worker_only,
        main_repo,
        work,
        outside,
    }
}

fn find<'a>(found: &'a [ultron_core::import::discovery::FoundProject], p: &Path) -> &'a ultron_core::import::discovery::FoundProject {
    found
        .iter()
        .find(|f| f.path.to_lowercase() == p.to_string_lossy().to_lowercase())
        .unwrap_or_else(|| panic!("proje bulunamadı: {}", p.display()))
}

#[test]
fn worker_sessions_count_toward_projects_and_worker_only_project_is_found() {
    let w = world();
    let d = discover(&w.home, &w.filters);
    assert_eq!(d.projects.len(), 4, "{:?}", d.projects.iter().map(|p| &p.path).collect::<Vec<_>>());
    // subagents/ üst oturumla yinelendiği için sayılmaz; işçi oturumu ayrı sayılır.
    assert_eq!(find(&d.projects, &w.good).counts(), (1, 1, 1));
    assert_eq!(find(&d.projects, &w.worker_only).counts(), (0, 0, 2));
    let codex = find(&d.projects, &w.codex_only);
    assert!(!codex.path.starts_with(r"\\?\"));
    assert_eq!(codex.counts(), (0, 1, 1));
}

#[test]
fn subagents_without_parent_session_are_treated_as_workers() {
    let w = world();
    write_lines(
        &w.home.join(".claude/projects/iyi/yetim/subagents/agent-2.jsonl"),
        &[json!({"type":"user","cwd":w.good.to_string_lossy(),"isSidechain":true,"message":{"role":"user","content":"yetim"}})],
    );
    let d = discover(&w.home, &w.filters);
    assert_eq!(find(&d.projects, &w.good).counts(), (1, 1, 2));
}

#[test]
fn worktree_sessions_map_to_main_checkout() {
    let w = world();
    let d = discover(&w.home, &w.filters);
    // .claude/worktrees önek kuralı + bağlı worktree (gitdir -> commondir) + Codex ad eşleşmesi.
    assert_eq!(find(&d.projects, &w.main_repo).counts(), (2, 1, 0));
    assert!(
        d.projects
            .iter()
            .all(|p| !p.path.to_lowercase().contains("worktrees") && !p.path.contains("bagli-worktree"))
    );
}

#[test]
fn containers_ultron_data_and_local_bin_are_excluded() {
    let w = world();
    let d = discover(&w.home, &w.filters);
    for p in &d.projects {
        let l = p.path.to_lowercase();
        assert!(!l.ends_with("ultrondata") && !l.contains("ultrondata"), "{l}");
        assert!(!l.contains(".local"), "{l}");
        assert_ne!(l, w.work.to_string_lossy().to_lowercase(), "kapsayıcı proje oldu");
        assert_ne!(l, w.home.to_string_lossy().to_lowercase());
    }
    // ev dizini + `work` kapsayıcısı oturumları atfedilmez.
    assert_eq!(d.unmatched_sessions, 2);
}

#[test]
fn catalog_slash_and_case_variants_dedupe_with_backslash_display() {
    let w = world();
    let variant = w.good.to_string_lossy().replace('\\', "/").to_uppercase();
    let extra = w.work.join("katalog-proje");
    std::fs::create_dir_all(&extra).unwrap();
    std::fs::write(
        w.home.join(".claude/projects-catalog.md"),
        format!(
            "- **Katalog** — `{variant}` — sentetik açıklama\n- **Yeni** — `{}` — başka\n",
            extra.display()
        ),
    )
    .unwrap();
    let d = discover(&w.home, &w.filters);
    assert_eq!(d.projects.len(), 5, "aynı klasör iki kez listelenmemeli");
    let good = find(&d.projects, &w.good);
    assert!(!good.path.contains('/'), "tek Windows biçimi bekleniyordu: {}", good.path);
    assert_eq!(good.catalog.as_ref().map(|c| c.0.as_str()), Some("Katalog"));
    assert_eq!(good.counts(), (1, 1, 1));
}

fn open(dir: &Path) -> Db {
    Db::open(&dir.join("ultron.db")).unwrap()
}

#[tokio::test]
async fn import_projects_creates_projects_masks_and_is_incremental() {
    let w = world();
    // Gizli veriler oturumda, hafıza notunda ve AGENTS.md içinde.
    let secret = "sk-ant-api03-abcdefghijklmnop";
    claude_session(
        &w.home,
        "iyi",
        "s2",
        &w.good.to_string_lossy(),
        &format!("anahtarım {secret} kullan"),
        "uc",
    );
    std::fs::write(w.good.join("AGENTS.md"), "# Kurallar\npassword=hunter2\n").unwrap();
    let mem = w
        .home
        .join(".claude/projects")
        .join(ultron_core::import::notes::encode_cwd(&w.good.to_string_lossy()))
        .join("memory");
    std::fs::create_dir_all(&mem).unwrap();
    std::fs::write(mem.join("karar.md"), "Karar: ghp_abcdefghijklmnopqrstuvwxyz0123456789 token").unwrap();

    // Rapor kırpma sınırında bir sır bulunur: önce maskelenmeli, sonra kısaltılmalı.
    let padded = format!("{} ghp_abcdefghijklmnopqrstuvwxyz0123456789", "a".repeat(985));
    claude_session(&w.home, "iyi", "worker2", &w.good.to_string_lossy(), WORKER_BRIEF, &padded);
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    let (sum, seen) = recorder();
    let r1 = import_projects(&db, &sum, &w.home, &w.filters, false).await.unwrap();
    assert_eq!(r1.rows.len(), 4);
    assert_eq!(r1.created, 4);
    assert_eq!(r1.notes_updated, 4);
    assert_eq!(r1.unmatched_sessions, 2);
    assert_eq!(db.list_projects().unwrap().len(), 4);

    let all_seen = seen.lock().unwrap().join("\n");
    for leak in [secret, "hunter2", "ghp_abcdefgh", "ghp_abcd", "ARAC-CIKTISI-SIZMAMALI", "ARAC-SONUCU-SIZMAMALI", "enjekte"] {
        assert!(!all_seen.contains(leak), "özetleyiciye sızdı: {leak}");
    }
    assert!(all_seen.contains("MASKED"));
    // İşçi oturumları nota girer: görev gövdesi ve son rapor var, standart brief başlığı yok.
    assert!(all_seen.contains("ISCI-GOREVI-X") && all_seen.contains("ISCI-RAPORU"));
    assert!(!all_seen.contains("directed by an external coordinator"));

    let good = db.find_project_by_path(&w.good.to_string_lossy()).unwrap().unwrap();
    let note = db.get_project_note(&good.id).unwrap().unwrap();
    for leak in [secret, "hunter2", "ghp_abcdefgh"] {
        assert!(!note.markdown.contains(leak), "nota sızdı: {leak}");
    }
    assert!(note.markdown.contains("Şimdiye kadar yapılanlar"));
    assert!(note.markdown.contains("2 Claude oturumu, 1 Codex oturumu, 2 işçi oturumu"));
    assert!(note.markdown.contains("AGENTS.md var"));
    assert!(note.markdown.contains("1 hafıza notu"));
    assert!(note.markdown.contains("SENTETIK-HEDEF") && note.markdown.contains("SENTETIK-YAPILDI"));
    assert!(note.markdown.contains("Son güncelleme: "));

    // Artımlı: değişiklik yok -> motor çağrılmaz, not güncellenmez; proje yinelenmez.
    let calls = seen.lock().unwrap().len();
    let r2 = import_projects(&db, &sum, &w.home, &w.filters, false).await.unwrap();
    assert_eq!(r2.created, 0);
    assert_eq!(r2.notes_updated, 0);
    assert!(r2.rows.iter().all(|r| r.note_status == "güncel"));
    assert_eq!(seen.lock().unwrap().len(), calls);
    assert_eq!(db.list_projects().unwrap().len(), 4);

    // Yeni oturum -> yalnız o proje güncellenir.
    claude_session(&w.home, "iyi", "s3", &w.good.to_string_lossy(), "yeni is", "dort");
    let r3 = import_projects(&db, &sum, &w.home, &w.filters, false).await.unwrap();
    assert_eq!(r3.notes_updated, 1);
    let row = r3.rows.iter().find(|r| r.note_updated).unwrap();
    assert_eq!(row.claude_sessions, 3);
}

#[tokio::test]
async fn dry_run_calls_no_engine_and_writes_nothing() {
    let w = world();
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    let (sum, seen) = recorder();
    let r = import_projects(&db, &sum, &w.home, &w.filters, true).await.unwrap();
    assert!(r.dry_run);
    assert_eq!(r.rows.len(), 4);
    assert!(r.rows.iter().all(|row| row.project_created && row.note_updated));
    assert!(seen.lock().unwrap().is_empty());
    assert!(db.list_projects().unwrap().is_empty());
}

#[test]
fn remove_chats_deletes_only_imported_and_backs_up() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ultron.db");
    let db = Db::open(&db_path).unwrap();
    // Gerçek içe aktarıcıyla sentetik Claude geçmişi.
    let root = dir.path().join("claude");
    claude_session(&root, "p", "oturum1", &dir.path().to_string_lossy(), "eski sohbet", "a");
    let report = import_claude_history(
        &db,
        &root.join(".claude/projects"),
        &mut ImportState::default(),
    )
    .unwrap();
    assert_eq!(report.chats, 1);
    // Ultron içinde açılan sohbetler: uuid kimlikli ve `claude-` önekli ama içe aktarılmamış.
    let mine = ["0123456789abcdef", "claude-benim-sohbetim"];
    for (i, id) in mine.iter().enumerate() {
        db.insert_chat(id, None, "benim", Utc::now()).unwrap();
        db.insert_event(&StoredEvent {
            id: format!("ev-{i}"),
            chat_id: (*id).into(),
            session_id: None,
            seq: 1,
            ts: Utc::now(),
            kind: "user_text".into(),
            payload: json!({"text":"benim mesajım"}),
        })
        .unwrap();
    }
    assert_eq!(db.imported_chat_ids().unwrap().len(), 1);

    // dry-run: sayar, hiçbir şey yazmaz (yedek dosyası da yok).
    let dry = remove_imported_chats(&db, &db_path, true).unwrap();
    assert!(dry.dry_run && dry.chats == 1 && dry.backup.is_none());
    assert_eq!(db.list_chats(None, true).unwrap().len(), 3);
    let backups = |dir: &Path| {
        std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .filter(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                n.contains(".backup-") && !n.ends_with("-wal") && !n.ends_with("-shm")
            })
            .count()
    };
    assert_eq!(backups(dir.path()), 0);

    let done = remove_imported_chats(&db, &db_path, false).unwrap();
    assert_eq!(done.chats, 1);
    assert!(done.events >= 1);
    let chats = db.list_chats(None, true).unwrap();
    let mut ids: Vec<&str> = chats.iter().map(|c| c.id.as_str()).collect();
    ids.sort();
    assert_eq!(ids, ["0123456789abcdef", "claude-benim-sohbetim"]);
    assert!(db.search_messages(None, "eski", 5).unwrap().is_empty());

    // Yedek: silinen sohbeti hâlâ içerir.
    let backup = PathBuf::from(done.backup.unwrap());
    assert_eq!(backups(dir.path()), 1);
    let restored = Db::open(&backup).unwrap();
    assert_eq!(restored.list_chats(None, true).unwrap().len(), 3);

    // İkinci çalıştırma: silinecek bir şey yok, yeni yedek açılmaz.
    let again = remove_imported_chats(&db, &db_path, false).unwrap();
    assert_eq!((again.chats, again.backup), (0, None));
    assert_eq!(backups(dir.path()), 1);
}

#[test]
fn profile_outside_projects_are_rejected_unless_listed_in_catalog() {
    let w = world();
    let d = discover(&w.home, &w.filters);
    assert_eq!(d.projects.len(), 4);
    assert!(d.projects.iter().all(|p| !p.path.to_lowercase().contains("disarida")));
    std::fs::write(
        w.home.join(".claude/projects-catalog.md"),
        format!("- **Dis** — `{}` — katalogdaki profil dışı proje\n", w.outside.display()),
    )
    .unwrap();
    let d = discover(&w.home, &w.filters);
    assert_eq!(d.projects.len(), 5);
    assert_eq!(find(&d.projects, &w.outside).counts(), (1, 0, 0));
}

#[test]
fn subfolder_cwd_maps_to_git_root_and_submodule_is_its_own_project() {
    let w = world();
    let sub_dir = w.main_repo.join("crates").join("x");
    std::fs::create_dir_all(&sub_dir).unwrap();
    claude_session(&w.home, "alt", "s1", &sub_dir.to_string_lossy(), "alt klasor", "x");
    let submodule = w.main_repo.join("vendor").join("sub");
    std::fs::create_dir_all(submodule.join("src")).unwrap();
    std::fs::write(submodule.join(".git"), "gitdir: ../../.git/modules/sub\n").unwrap();
    claude_session(&w.home, "sm", "s1", &submodule.join("src").to_string_lossy(), "submodule", "x");
    let d = discover(&w.home, &w.filters);
    assert_eq!(d.projects.len(), 5, "{:?}", d.projects.iter().map(|p| &p.path).collect::<Vec<_>>());
    // Alt klasör oturumu depo köküne iner (2 worktree/bağlı + 1 alt klasör Claude, 1 Codex).
    assert_eq!(find(&d.projects, &w.main_repo).counts(), (3, 1, 0));
    assert_eq!(find(&d.projects, &submodule).counts(), (1, 0, 0));
}

#[tokio::test]
async fn circuit_breaker_skips_after_two_failures_and_status_is_sanitised() {
    let w = world();
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    let calls = Arc::new(Mutex::new(0));
    let sum = Failing { calls: calls.clone() };
    let r = import_projects(&db, &sum, &w.home, &w.filters, false).await.unwrap();
    assert_eq!(*calls.lock().unwrap(), 2, "iki hatadan sonra çağrı yapılmamalı");
    let statuses: Vec<&str> = r.rows.iter().map(|row| row.note_status.as_str()).collect();
    assert_eq!(statuses.iter().filter(|s| s.starts_with("hata:")).count(), 2);
    assert_eq!(statuses.iter().filter(|s| s.starts_with("atlandı")).count(), 2);
    for s in statuses.iter().filter(|s| s.starts_with("hata:")) {
        assert!(!s.contains("sk-ant-api03") && !s.contains('\n') && !s.contains('\u{7}'), "{s}");
    }
    assert!(r.rows.iter().all(|row| !row.note_updated));
    assert_eq!(db.list_projects().unwrap().len(), 4, "projeler yine de oluşturulur");
}

#[tokio::test]
async fn note_keeps_history_when_sessions_shrink_and_refreshes_on_summarizer_change() {
    use ultron_core::summarizer::NullSummarizer;
    let w = world();
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    let good_s = w.good.to_string_lossy().into_owned();
    let note_of = |db: &Db| {
        let p = db.find_project_by_path(&good_s).unwrap().unwrap();
        db.get_project_note(&p.id).unwrap().unwrap()
    };
    let (s1, _) = recorder_with("HEDEF-1", "YAPILDI-1", "fake");
    import_projects(&db, &s1, &w.home, &w.filters, false).await.unwrap();
    assert!(note_of(&db).markdown.contains("merhaba proje"));

    // Claude eski dökümleri sildi (sayı azaldı) ve yeni bir oturum geldi.
    let proj = w.home.join(".claude/projects/iyi");
    std::fs::remove_file(proj.join("s1.jsonl")).unwrap();
    std::fs::remove_file(proj.join("worker.jsonl")).unwrap();
    std::fs::remove_dir_all(proj.join("s1")).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(40));
    claude_session(&w.home, "iyi", "s9", &good_s, "yeni oturum istegi", "dokuz");
    let (s2, _) = recorder_with("HEDEF-2", "YAPILDI-2", "fake");
    let r = import_projects(&db, &s2, &w.home, &w.filters, false).await.unwrap();
    assert!(r.rows.iter().any(|row| row.path.to_lowercase() == good_s.to_lowercase() && row.note_updated));
    let md = note_of(&db).markdown;
    assert!(md.contains("HEDEF-1"), "eski hedef korunmalı: {md}");
    assert!(md.contains("YAPILDI-1") && md.contains("YAPILDI-2"), "{md}");
    assert!(md.contains("merhaba proje") && md.contains("yeni oturum istegi"), "{md}");

    // Özetleyici türü değişince (dosya değişmese de) yenilenir ve tür saklanır.
    let (s3, seen3) = recorder_with("HEDEF-3", "YAPILDI-3", "fake3");
    import_projects(&db, &s3, &w.home, &w.filters, false).await.unwrap();
    assert!(!seen3.lock().unwrap().is_empty());
    let n = note_of(&db);
    assert_eq!(n.summarizer, "fake3");
    assert!(n.markdown.contains("YAPILDI-1") && n.markdown.contains("YAPILDI-3"));

    // Özetleyici yokken (null) özetlenmiş not ham alıntıya düşürülmez.
    let before = note_of(&db).markdown;
    let r = import_projects(&db, &NullSummarizer, &w.home, &w.filters, false).await.unwrap();
    assert!(r.rows.iter().all(|row| !row.note_updated));
    assert_eq!(note_of(&db).markdown, before);
    assert!(r.rows.iter().all(|row| row.note_status != "bayat (özetleyici yok)"));

    // Kaynaklar değişti ama özetleyici yok: not korunur, bayat olduğu bildirilir.
    std::thread::sleep(std::time::Duration::from_millis(40));
    claude_session(&w.home, "iyi", "s10", &good_s, "sonradan gelen is", "on");
    let r = import_projects(&db, &NullSummarizer, &w.home, &w.filters, false).await.unwrap();
    let row = r.rows.iter().find(|row| row.path.to_lowercase() == good_s.to_lowercase()).unwrap();
    assert_eq!(row.note_status, "bayat (özetleyici yok)");
    assert!(!row.note_updated);
    assert_eq!(note_of(&db).markdown, before);
}

#[test]
fn linked_worktree_with_main_outside_profile_is_unmatched_not_a_project() {
    let w = world();
    // Ana checkout profil dışında (katalogda yok); worktree profil içinde.
    let outside_repo = w.outside.parent().unwrap().join("dis-repo");
    std::fs::create_dir_all(outside_repo.join(".git/worktrees/x")).unwrap();
    std::fs::write(outside_repo.join(".git/worktrees/x/commondir"), "../..
").unwrap();
    let wt = w.work.join("dis-worktree");
    std::fs::create_dir_all(&wt).unwrap();
    std::fs::write(wt.join(".git"), format!("gitdir: {}
", outside_repo.join(".git/worktrees/x").display())).unwrap();
    claude_session(&w.home, "dw", "s1", &wt.to_string_lossy(), "worktree istegi", "x");
    let d = discover(&w.home, &w.filters);
    assert_eq!(d.projects.len(), 4);
    assert!(d.projects.iter().all(|p| !p.path.to_lowercase().contains("dis-repo") && !p.path.contains("dis-worktree")));
    assert_eq!(d.unmatched_sessions, 3);
}

#[test]
fn delete_rechecks_imported_predicate_per_chat() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    db.insert_chat("claude-benim", None, "benim", Utc::now()).unwrap();
    db.insert_event(&StoredEvent {
        id: "ev-1".into(),
        chat_id: "claude-benim".into(),
        session_id: None,
        seq: 1,
        ts: Utc::now(),
        kind: "user_text".into(),
        payload: json!({"text": "benim mesajım"}),
    })
    .unwrap();
    // Çağıran yanlış kimlik verse bile koşul işlem içinde yeniden doğrulanır.
    assert_eq!(db.delete_imported_chats(&["claude-benim".into()]).unwrap(), (0, 0));
    assert!(db.get_chat("claude-benim").unwrap().is_some());
}
