//! Alt süreç çalıştırıcı: satır satır okur, olaya çevirir, iptalde süreç ağacını öldürür.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::{Engine, TurnRequest};
use crate::eventlog::{DEFAULT_EXCERPT_CHARS, excerpt};
use crate::events::EngineEvent;

/// stderr için tutulan en fazla bayt.
const STDERR_CAP: usize = 64 * 1024;

/// Çocuk çıktıktan sonra stdout'un EOF'a ulaşması için tanınan süre.
/// Aşılırsa boruyu tutan torunlar sonlandırılır.
const EXIT_GRACE: Duration = Duration::from_secs(2);

/// Tek bir stdout satırının en fazla boyutu; aşan satır atlanır.
const MAX_LINE_BYTES: usize = 1024 * 1024;

/// Bir turun stdout'undan okunacak en fazla toplam bayt; aşılırsa süreç sonlandırılır.
const MAX_TURN_STDOUT: u64 = 64 * 1024 * 1024;

fn line_too_long() -> EngineEvent {
    EngineEvent::Error {
        message: "satır çok uzun, atlandı".to_string(),
    }
}

/// Bir turun sonucu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnOutcome {
    Completed,
    Cancelled,
    Failed {
        exit_code: Option<i32>,
        stderr_excerpt: String,
    },
    QuotaExhausted,
}

/// Motor komutunu çalıştırır; stdout'taki her tam satırı olaya çevirip `sink`'e yollar.
/// Yarım son satır (süreç ölünce ya da iptalde) atılır.
pub async fn run_turn(
    engine: &dyn Engine,
    req: TurnRequest,
    sink: mpsc::Sender<EngineEvent>,
    cancel: CancellationToken,
) -> Result<TurnOutcome> {
    let spec = engine.command(&req);
    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args)
        .current_dir(&spec.cwd)
        .stdin(if spec.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = cmd
        .spawn()
        .with_context(|| format!("süreç başlatılamadı: {}", spec.program))?;
    let job = job::Job::attach(&child);

    if let Some(text) = spec.stdin
        && let Some(mut stdin) = child.stdin.take()
    {
        // Büyük istemde çocuğun okumasını beklerken kilitlenmemek için ayrı görev.
        tokio::spawn(async move {
            let _ = stdin.write_all(text.as_bytes()).await;
            let _ = stdin.shutdown().await;
        });
    }

    let mut stdout = child.stdout.take().context("stdout alınamadı")?;
    let mut stderr = child.stderr.take().context("stderr alınamadı")?;
    let stderr_task = tokio::spawn(async move {
        let mut kept: Vec<u8> = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            match stderr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    // Kapasite dolunca okumaya devam ederek boruyu boşaltırız.
                    let room = STDERR_CAP.saturating_sub(kept.len());
                    kept.extend_from_slice(&buf[..n.min(room)]);
                }
            }
        }
        kept
    });

    let mut pending: Vec<u8> = Vec::new();
    let mut buf = [0u8; 8192];
    let mut quota = false;
    let mut cancelled = false;
    let mut status: Option<std::process::ExitStatus> = None;
    let mut grace_deadline: Option<Instant> = None;
    // Üst sınırı aşan satırın geri kalanı yutulurken `true`.
    let mut skipping = false;
    let mut total_out: u64 = 0;
    let mut over_budget = false;

    loop {
        tokio::select! {
            () = cancel.cancelled() => { cancelled = true; break; }
            s = child.wait(), if status.is_none() => {
                status = Some(s.context("süreç beklenemedi")?);
                grace_deadline = Some(Instant::now() + EXIT_GRACE);
            }
            () = tokio::time::sleep_until(grace_deadline.unwrap_or_else(Instant::now)),
                if grace_deadline.is_some() =>
            {
                // Çocuk bitti ama boruyu tutan bir torun var: ağacı sonlandır.
                job.terminate();
                break;
            }
            read = stdout.read(&mut buf) => {
                let n = match read { Ok(0) | Err(_) => break, Ok(n) => n };
                total_out += n as u64;
                if total_out > MAX_TURN_STDOUT {
                    over_budget = true;
                    break;
                }
                let mut rest = &buf[..n];
                while !rest.is_empty() {
                    let Some(pos) = rest.iter().position(|&b| b == b'\n') else {
                        // Satır henüz bitmedi; üst sınır aşılırsa satır atlanır ve sonuna dek yutulur.
                        if !skipping {
                            pending.extend_from_slice(rest);
                            if pending.len() > MAX_LINE_BYTES {
                                pending.clear();
                                skipping = true;
                                let _ = sink.send(line_too_long()).await;
                            }
                        }
                        break;
                    };
                    let head = &rest[..pos];
                    rest = &rest[pos + 1..];
                    if skipping {
                        skipping = false;
                        continue;
                    }
                    pending.extend_from_slice(head);
                    if pending.len() > MAX_LINE_BYTES {
                        pending.clear();
                        let _ = sink.send(line_too_long()).await;
                        continue;
                    }
                    let raw = std::mem::take(&mut pending);
                    let text = String::from_utf8_lossy(&raw);
                    let line = text.trim_end_matches('\r');
                    if line.trim().is_empty() {
                        continue;
                    }
                    for ev in engine.parse_line(line) {
                        if matches!(ev, EngineEvent::QuotaExhausted { .. }) {
                            quota = true;
                        }
                        // Alıcı kapandıysa tur yine de sonlanana dek okunur.
                        let _ = sink.send(ev).await;
                    }
                }
            }
        }
    }
    // Kalan `pending` yarım satırdır; bilerek atılır.
    drop(pending);

    if over_budget {
        job.terminate();
        let _ = child.kill().await;
        let _ = child.wait().await;
        stderr_task.abort();
        return Ok(TurnOutcome::Failed {
            exit_code: None,
            stderr_excerpt: "çıktı bütçesi aşıldı".to_string(),
        });
    }

    if cancelled {
        return Ok(cancel_turn(&job, &mut child, stderr_task).await);
    }

    let status = match status {
        Some(s) => s,
        None => tokio::select! {
            () = cancel.cancelled() => {
                return Ok(cancel_turn(&job, &mut child, stderr_task).await);
            }
            s = child.wait() => s.context("süreç beklenemedi")?,
        },
    };
    // Torunlar borunun ucunu tutuyorsa sonsuza dek beklememek için sınır.
    let stderr_bytes = match tokio::time::timeout(Duration::from_secs(1), stderr_task).await {
        Ok(Ok(bytes)) => bytes,
        _ => Vec::new(),
    };
    drop(job);

    if quota {
        return Ok(TurnOutcome::QuotaExhausted);
    }
    if status.success() {
        Ok(TurnOutcome::Completed)
    } else {
        Ok(TurnOutcome::Failed {
            exit_code: status.code(),
            stderr_excerpt: excerpt(
                &String::from_utf8_lossy(&stderr_bytes),
                DEFAULT_EXCERPT_CHARS,
            ),
        })
    }
}

/// İptal: süreç ağacını sonlandırır ve çocuğu toplar.
async fn cancel_turn(
    job: &job::Job,
    child: &mut tokio::process::Child,
    stderr_task: tokio::task::JoinHandle<Vec<u8>>,
) -> TurnOutcome {
    job.terminate();
    let _ = child.kill().await;
    let _ = child.wait().await;
    stderr_task.abort();
    TurnOutcome::Cancelled
}

#[cfg(windows)]
mod job {
    use tokio::process::Child;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject,
    };

    /// Süreç ağacı için Job Object; kapanınca içindeki tüm süreçler ölür.
    /// Oluşturulamazsa tutacak boştur ve işlemler etkisizdir (`kill_on_drop` yedektir).
    pub struct Job(HANDLE);

    // SAFETY: Job Object tutacakları iş parçacıkları arasında kullanılabilir.
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    impl Job {
        pub fn attach(child: &Child) -> Job {
            let Some(process) = child.raw_handle() else {
                return Job(std::ptr::null_mut());
            };
            // SAFETY: Win32 çağrıları geçerli tutacaklarla yapılır; başarısızlıkta tutacak kapatılır.
            unsafe {
                let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if handle.is_null() {
                    return Job(std::ptr::null_mut());
                }
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let set = SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    (&raw const info).cast(),
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                if set == 0 || AssignProcessToJobObject(handle, process as HANDLE) == 0 {
                    CloseHandle(handle);
                    return Job(std::ptr::null_mut());
                }
                Job(handle)
            }
        }

        pub fn terminate(&self) {
            if self.0.is_null() {
                return;
            }
            // SAFETY: tutacak `Job` yaşadıkça geçerlidir.
            unsafe {
                TerminateJobObject(self.0, 1);
            }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            if self.0.is_null() {
                return;
            }
            // SAFETY: tutacak yalnızca burada kapatılır.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

#[cfg(not(windows))]
mod job {
    use tokio::process::Child;

    pub struct Job;

    impl Job {
        pub fn attach(_child: &Child) -> Job {
            Job
        }
        pub fn terminate(&self) {}
    }
}
