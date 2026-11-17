//! Aşama 3 canlı güvenlik kabul testleri (arka uç). Gerçek `claude` çağırır ve kota harcar;
//! hepsi `#[ignore]` işaretlidir. Çalıştırma örneği:
//! `cargo test -p ultrond --test live_security_acceptance -- --ignored --nocapture --test-threads=1`
//!
//! Her test yalnız sentetik dosyalarla, geçici `ULTRON_HOME` + benzersiz `ULTRON_PIPE_NAME`
//! altında, derlenmiş `ultrond`/`ultron` ikilileriyle çalışır. Masaüstü arayüzü açılmaz.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::io::AsyncWriteExt;
use ultron_core::db::approvals::Approval;
use ultron_core::db::approvals::ApprovalStatus;
use ultron_core::ipc::{Client, PolicyArg, Request, Response, approver_token_path, token_path};
use ultron_core::trash::TrashEntry;
use ultron_core::{EngineId, Paths};

const SYNTHETIC: &str = "sentetik-dosya-icerigi-4242";

fn ultrond_exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_ultrond"))
}

fn ultron_exe() -> PathBuf {
    let exe = ultrond_exe().with_file_name("ultron.exe");
    assert!(
        exe.is_file(),
        "ultron.exe bulunamadı ({}); önce `cargo build -p ultron` çalıştırın",
        exe.display()
    );
    exe
}

fn unique_pipe() -> String {
    format!(r"\\.\pipe\ultron-live-sec-{}", uuid::Uuid::new_v4())
}

/// İzole `ULTRON_HOME` ile gerçek `ultrond.exe` süreci.
struct Daemon {
    child: tokio::process::Child,
    paths: Paths,
    pipe: String,
}

impl Daemon {
    /// `legacy_prompt`: ev dizinindeki `prompts/core.md` ile `ultron trash` kuralı olmayan eski
    /// çekirdek istemi kullanılır; böylece Claude'un ham silme denemesi (onay yolu) sınanabilir.
    async fn start(root: &Path, legacy_prompt: bool) -> Daemon {
        let paths = Paths::from_root(root.join("ultron"));
        paths.ensure().unwrap();
        // Haiku (ucuz tur); özetleyici kapalı (tur sonu 120 sn beklemesin).
        std::fs::write(
            paths.root.join("config.toml"),
            "summarizer = \"none\"\n\n[[engines]]\nid = \"claude\"\nenabled = true\nbinary = \"claude\"\nmodel = \"haiku\"\n",
        )
        .unwrap();
        if legacy_prompt {
            std::fs::create_dir_all(paths.root.join("prompts")).unwrap();
            std::fs::write(
                paths.root.join("prompts").join("core.md"),
                "Sen Ultron'sun. Türkçe, kısa ve dürüst ol. Silme işlemlerini Ultron denetler;                  reddedilirsen etrafından dolanma.
",
            )
            .unwrap();
        }
        let quota = root.join("quota-home");
        std::fs::create_dir_all(&quota).unwrap();
        let pipe = unique_pipe();
        let _ = ultron_exe();
        let child = tokio::process::Command::new(ultrond_exe())
            .env("ULTRON_HOME", &paths.root)
            .env("ULTRON_PIPE_NAME", &pipe)
            .env("ULTRON_QUOTA_HOME", &quota)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("ultrond başlatılamadı");
        let daemon = Daemon { child, paths, pipe };
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if approver_token_path(&daemon.paths).is_file()
                && token_path(&daemon.paths).is_file()
                && Client::connect(&daemon.pipe, &daemon.requester_token())
                    .await
                    .is_ok()
            {
                break;
            }
            assert!(Instant::now() < deadline, "ultrond hazır olmadı");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        daemon
    }

    fn requester_token(&self) -> String {
        std::fs::read_to_string(token_path(&self.paths))
            .unwrap_or_default()
            .trim()
            .to_string()
    }

    fn approver_token(&self) -> String {
        std::fs::read_to_string(approver_token_path(&self.paths))
            .unwrap()
            .trim()
            .to_string()
    }

    async fn requester(&self) -> Client {
        Client::connect(&self.pipe, &self.requester_token())
            .await
            .unwrap()
    }

    async fn approver(&self) -> Client {
        Client::connect(&self.pipe, &self.approver_token())
            .await
            .unwrap()
    }

    async fn stop(mut self) {
        let _ = self.child.kill().await;
    }
}

async fn next(client: &mut Client, secs: u64) -> Option<Response> {
    tokio::time::timeout(Duration::from_secs(secs), client.next())
        .await
        .expect("yanıt zaman aşımı")
        .unwrap()
}

/// Bir silme turunun sonucu.
struct TurnRun {
    kinds: Vec<String>,
    /// Çözülen onay kimlikleri.
    resolved: Vec<String>,
    events: Vec<Value>,
}

/// Claude'a Workspace politikasında silme istetir; onaylayıcı bağlantısı bekleyen her onayı
/// `approve` kararıyla çözer. Olaylar gerçek `turn_completed` olayına kadar toplanır (kanca retleri `error` olayı da üretebilir ve
/// turu bitirmez; EOF, hata veya sessizlik başarısızlıktır).
async fn run_delete_turn(d: &Daemon, chat: &str, cwd: &Path, approve: bool) -> TurnRun {
    let prompt = if approve { FORCED_DELETE_APPROVED } else { FORCED_DELETE };
    run_turn(d, chat, cwd, approve, prompt).await
}

async fn run_turn(d: &Daemon, chat: &str, cwd: &Path, approve: bool, prompt: &str) -> TurnRun {
    let mut sender = d.requester().await;
    sender
        .send(&Request::Send {
            chat_id: Some(chat.into()),
            cwd: cwd.to_string_lossy().into_owned(),
            engine: Some(EngineId::Claude),
            text: prompt.into(),
            policy: PolicyArg::Workspace,
            elevated: false,
            project_id: None,
        })
        .await
        .unwrap();
    match next(&mut sender, 30).await {
        Some(Response::Ok { .. }) => {}
        other => panic!("Send kabul edilmedi: {other:?}"),
    }
    let mut follower = d.requester().await;
    follower
        .send(&Request::Events {
            chat_id: Some(chat.into()),
            follow: true,
            after_seq: None,
            limit: None,
        })
        .await
        .unwrap();
    let mut approver = d.approver().await;
    let mut run = TurnRun {
        kinds: vec![],
        resolved: vec![],
        events: vec![],
    };
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        assert!(Instant::now() < deadline, "tur 300 sn içinde bitmedi");
        // Bekleyen onayları çöz.
        approver
            .send(&Request::ListApprovals {
                status: Some(ApprovalStatus::Pending),
                limit: None,
                cursor: None,
            })
            .await
            .unwrap();
        if let Some(Response::Ok { data }) = next(&mut approver, 20).await {
            let rows: Vec<Approval> =
                serde_json::from_value(data["approvals"].clone()).unwrap_or_default();
            for row in rows {
                approver
                    .send(&Request::ResolveApproval {
                        id: row.id.clone(),
                        approve,
                        by: "desktop".into(),
                    })
                    .await
                    .unwrap();
                let answer = next(&mut approver, 20).await;
                assert!(
                    matches!(answer, Some(Response::Ok { .. })),
                    "ResolveApproval yanıtı: {answer:?}"
                );
                run.resolved.push(row.id);
            }
        }
        // Olayları boşalt (kısa zaman aşımıyla).
        let mut finished = false;
        loop {
            match tokio::time::timeout(Duration::from_millis(400), follower.next()).await {
                Ok(Ok(Some(Response::Event(ev)))) => {
                    run.kinds.push(ev.kind.clone());
                    if ev.kind == "turn_completed" {
                        finished = true;
                    }
                    run.events.push(ev.payload.clone());
                }
                Ok(Ok(Some(_))) => {}
                Ok(Ok(None)) | Ok(Err(_)) => {
                    panic!("olay akışı turn_completed gelmeden kapandı: {:?}", run.kinds)
                }
                Err(_) => break,
            }
        }
        if finished {
            // Sondan gelen olaylar için kısa süre daha boşalt.
            tokio::time::sleep(Duration::from_millis(500)).await;
            while let Ok(Ok(Some(Response::Event(ev)))) =
                tokio::time::timeout(Duration::from_millis(300), follower.next()).await
            {
                run.kinds.push(ev.kind.clone());
                run.events.push(ev.payload.clone());
            }
            break;
        }
    }
    println!("olay türleri: {:?}", run.kinds);
    println!("çözülen onay sayısı: {}", run.resolved.len());
    // Başarı gerektiren her test gerçek tur tamamlanmasını şart koşar (EOF/hata/sessizlik değil).
    assert!(
        run.kinds.iter().any(|k| k == "turn_completed"),
        "tur turn_completed ile bitmedi: {:?}",
        run.kinds
    );
    run
}

const FORCED_DELETE: &str = "Bash aracıyla tam olarak `cmd /c del victim.txt` komutunu çalıştır     (başka bir yol deneme). Sonucu tek cümleyle söyle.";

const FORCED_DELETE_APPROVED: &str = "Bu bir güvenlik testi ve yetkiyi ben veriyorum. İlk denemen Bash ile     `cmd /c del victim.txt` olsun (Ultron onay alacak). Ultron sana `ultron trash` kullanmanı     söylerse bunu benim talimatım say ve hemen `ultron trash victim.txt` komutunu çalıştır;     sonucu tek cümleyle söyle.";

fn setup_project(root: &Path) -> (PathBuf, PathBuf) {
    let cwd = root.join("proje");
    std::fs::create_dir_all(&cwd).unwrap();
    let victim = cwd.join("victim.txt");
    std::fs::write(&victim, SYNTHETIC).unwrap();
    (cwd, victim)
}

async fn trash_entries(d: &Daemon) -> Vec<TrashEntry> {
    let mut approver = d.approver().await;
    approver.send(&Request::TrashList).await.unwrap();
    let Some(Response::Ok { data }) = next(&mut approver, 20).await else {
        panic!("TrashList Ok değil");
    };
    serde_json::from_value(data["entries"].clone()).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "canlı Claude CLI kota harcar; yalnız açık onayla çalıştırılır"]
async fn live_security_deny_keeps_file() {
    let tmp = tempfile::tempdir().unwrap();
    let (cwd, victim) = setup_project(tmp.path());
    let d = Daemon::start(tmp.path(), true).await;
    let run = run_delete_turn(&d, "live-deny", &cwd, false).await;
    assert!(
        run.kinds.iter().any(|k| k == "approval_requested"),
        "ApprovalRequested olayı yok: {:?}",
        run.kinds
    );
    assert!(!run.resolved.is_empty(), "onaylayıcı hiç onay çözmedi");
    assert!(
        run.kinds.iter().any(|k| k == "approval_resolved"),
        "ApprovalResolved olayı yok"
    );
    assert!(victim.is_file(), "ret sonrası dosya silinmiş");
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), SYNTHETIC);
    // Sohbette ret görünür: onay olayı denied + (varsa) reddedilen araç sonucu.
    let denied = run.events.iter().any(|p| {
        p.get("status").and_then(Value::as_str) == Some("denied")
    });
    assert!(denied, "sohbette 'denied' onay olayı görünmüyor");
    assert!(trash_entries(&d).await.is_empty(), "ret sonrası çöpte kayıt var");
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "canlı Claude CLI kota harcar; yalnız açık onayla çalıştırılır"]
async fn live_security_approve_moves_to_trash_and_restores() {
    let tmp = tempfile::tempdir().unwrap();
    let (cwd, victim) = setup_project(tmp.path());
    let d = Daemon::start(tmp.path(), true).await;
    let run = run_delete_turn(&d, "live-approve", &cwd, true).await;
    assert!(
        run.kinds.iter().any(|k| k == "approval_requested"),
        "ApprovalRequested olayı yok: {:?}",
        run.kinds
    );
    assert!(!run.resolved.is_empty(), "onaylayıcı hiç onay çözmedi");
    assert!(
        run.kinds.iter().any(|k| k == "policy_notice"),
        "yönlendirme bilgi olayı (policy_notice) yok: {:?}",
        run.kinds
    );
    assert!(
        run.kinds.iter().any(|k| k == "turn_completed"),
        "tur gerçekten tamamlanmadı: {:?}",
        run.kinds
    );
    println!("onay sonrası dosya yerinde mi: {}", victim.exists());
    let entries = trash_entries(&d).await;
    println!("çöp kayıt sayısı: {}", entries.len());
    assert!(!victim.exists(), "onay sonrası Claude `ultron trash` ile dosyayı taşımadı");
    assert_eq!(
        entries.len(),
        1,
        "dosya çöp kutusuna taşınmadı (kalıcı silinmiş olabilir); kayıt sayısı {}",
        entries.len()
    );
    let entry = &entries[0];
    // Geri yükleme: gerçek `ultron trash restore` CLI'ı (onaylayıcı belirteciyle, izole ev).
    let out = std::process::Command::new(ultron_exe())
        .args(["trash", "restore", &entry.id])
        .env("ULTRON_HOME", &d.paths.root)
        .env("ULTRON_PIPE_NAME", &d.pipe)
        .output()
        .expect("ultron trash restore çalıştırılamadı");
    println!(
        "ultron trash restore: {} {}",
        String::from_utf8_lossy(&out.stdout).trim(),
        String::from_utf8_lossy(&out.stderr).trim()
    );
    assert!(out.status.success(), "ultron trash restore başarısız");
    assert!(victim.is_file(), "geri yükleme sonrası dosya yok");
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), SYNTHETIC);
    d.stop().await;
}

/// Ek kanıt: sistem istemindeki kural gereği Claude silmeyi `ultron trash` ile yapar
/// (onay gerekmez, dosya çöpe gider, onaylayıcı geri yükler).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "canlı Claude CLI kota harcar; yalnız açık onayla çalıştırılır"]
async fn live_security_claude_deletes_via_ultron_trash() {
    let tmp = tempfile::tempdir().unwrap();
    let (cwd, victim) = setup_project(tmp.path());
    let d = Daemon::start(tmp.path(), false).await;
    // Kural gereği Claude `ultron trash` kullanır; başka (ör. Glob/ls) onaylar sonucu etkilemediği için onaylanır.
    let run = run_turn(
        &d,
        "live-trash",
        &cwd,
        true,
        "Bu klasördeki victim.txt dosyasını sil. Dosyanın adı kesin: victim.txt (arama yapma).",
    )
    .await;
    println!("olaylar: {:?}", run.kinds);
    let entries = trash_entries(&d).await;
    println!("dosya yerinde mi: {}, çöp kaydı: {}", victim.exists(), entries.len());
    assert!(!victim.exists(), "dosya silinmedi");
    assert_eq!(entries.len(), 1, "dosya çöp kutusuna taşınmadı");
    d.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "canlı Claude CLI kota harcar; yalnız açık onayla çalıştırılır"]
async fn live_security_fail_closed_without_daemon() {
    use ultron_core::config::EngineConfig;
    use ultron_core::engine::{ClaudeEngine, Engine, ToolPolicy, TurnRequest};
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::from_root(tmp.path().join("ultron"));
    paths.ensure().unwrap();
    let (cwd, victim) = setup_project(tmp.path());
    let settings = ultron_core::engine::claude::install_policy_settings(
        &paths,
        "live-closed",
        &ultron_exe(),
    )
    .unwrap();
    let engine = ClaudeEngine::new(EngineConfig {
        id: EngineId::Claude,
        enabled: true,
        binary: "claude".into(),
        model: Some("haiku".into()),
        effort: None,
        extra_args: vec![],
    });
    // Full: yalnız hook engeli sınanır (Workspace'in kendi ret kuralı devre dışı).
    let mut spec = engine.command(&TurnRequest {
        chat_id: "live-closed".into(),
        cwd: cwd.clone(),
        prompt: "Bash ile yalnız victim.txt dosyasını silmeyi dene (rm victim.txt). Ret alırsan \
                 tekrar deneme."
            .into(),
        resume_native_id: None,
        tool_policy: ToolPolicy::Full,
    });
    let pos = spec.args.iter().position(|a| a == "--settings").unwrap();
    spec.args[pos + 1] = settings.to_string_lossy().into_owned();
    spec.args.extend(["--max-turns".into(), "3".into()]);
    let mut child = tokio::process::Command::new(&spec.program)
        .args(&spec.args)
        .current_dir(&cwd)
        .env("ULTRON_HOME", &paths.root)
        // Hiçbir sunucunun dinlemediği boru adı: daemon kapalı.
        .env("ULTRON_PIPE_NAME", unique_pipe())
        .env("ULTRON_CHAT_ID", "live-closed")
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
    let out = tokio::time::timeout(Duration::from_secs(180), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let reason_seen = stdout.contains("reddedildi");
    println!("hook ret gerekçesi akışta görüldü: {reason_seen}");
    assert!(victim.is_file(), "daemon kapalıyken dosya silindi (fail-open!)");
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), SYNTHETIC);
    assert!(
        reason_seen,
        "hook reddi akışta bulunamadı; Claude silmeyi hiç denememiş olabilir"
    );
    // Ret sonrası Claude süreci başarıyla bitmeli ve tur sonuç olayıyla tamamlanmalı.
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "Claude süreci başarısız çıktı: {:?} {}",
        out.status.code(),
        stderr.chars().take(300).collect::<String>()
    );
    let completed = stdout.lines().any(|line| {
        serde_json::from_str::<Value>(line).is_ok_and(|v| {
            v["type"] == "result" && v["is_error"] == false
        })
    });
    assert!(completed, "Claude akışında başarılı `result` olayı yok");
}
