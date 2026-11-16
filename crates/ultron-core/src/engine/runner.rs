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

/// Motor ikilisini PATH üzerinde çözer (`binary` uzantısız ve yol içermiyorsa).
pub fn resolve_binary(binary: &str) -> Result<String> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let dirs: Vec<std::path::PathBuf> = std::env::split_paths(&path).collect();
    resolve_binary_in(binary, &dirs)
}

/// `resolve_binary`'nin PATH dizinleri verilmiş biçimi. Windows'ta uzantısız ad önce `.exe`
/// olarak aranır. Yalnız `.cmd`/`.bat` kabuğu bulunursa hata döner: istemi bu kabuklardan
/// geçirmek (tırnak ve `%` kaçışları) güvenli değildir; tam `.exe` yolu yapılandırılmalıdır.
/// Bulunamazsa ad olduğu gibi döner; süreç başlatma hatası çağırana kalır.
pub fn resolve_binary_in(binary: &str, dirs: &[std::path::PathBuf]) -> Result<String> {
    let has_ext_or_sep =
        std::path::Path::new(binary).extension().is_some() || binary.contains(['/', '\\']);
    if !cfg!(windows) || has_ext_or_sep {
        return Ok(binary.to_string());
    }
    for dir in dirs {
        let exe = dir.join(format!("{binary}.exe"));
        if exe.is_file() {
            return Ok(exe.to_string_lossy().into_owned());
        }
    }
    for dir in dirs {
        for ext in ["cmd", "bat"] {
            let shim = dir.join(format!("{binary}.{ext}"));
            if shim.is_file() {
                anyhow::bail!(
                    "'{binary}' bir kabuk betiği ({}); istem bu betikten geçirilemez. \
                     Yapılandırmada tam .exe yolunu verin.",
                    shim.display()
                );
            }
        }
    }
    Ok(binary.to_string())
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
    let program = resolve_binary(&spec.program)?;
    let mut cmd = Command::new(&program);
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
        // Süreç ana iş parçacığı askıda başlar; iş nesnesine bağlanmadan kod çalıştırmaz.
        const CREATE_SUSPENDED: u32 = 0x0000_0004;
        cmd.creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED);
    }

    let mut child = cmd
        .spawn()
        .with_context(|| format!("süreç başlatılamadı: {}", spec.program))?;
    let job = match job::Job::attach(&child) {
        Ok(job) => job,
        Err(err) => {
            let _ = child.kill().await;
            return Err(anyhow::Error::new(err).context("süreç iş nesnesine bağlanamadı"));
        }
    };

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
    let mut last_error: Option<String> = None;
    let mut cancelled = false;
    let mut status: Option<std::process::ExitStatus> = None;
    let mut grace_deadline: Option<Instant> = None;
    // Üst sınırı aşan satırın geri kalanı yutulurken `true`.
    let mut skipping = false;
    let mut total_out: u64 = 0;
    let mut over_budget = false;
    // Torun borusu açık kalıp sonlandırıldıysa (çocuk zaten çıkmıştır).
    let mut linger_killed = false;

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
                linger_killed = true;
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
                        if let EngineEvent::Error { message } = &ev {
                            last_error = Some(message.clone());
                        }
                        // Alıcı kapandıysa tur yine de sonlanana dek okunur.
                        let _ = sink.send(ev).await;
                    }
                }
            }
        }
    }
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
    // Kalan `pending` sonlandırıcısız son satırdır. Temiz çıkışta ve geçerli JSON ise işlenir;
    // çökme, iptal ve sonlandırmada yarım satır atılır.
    if !linger_killed && status.success() && !pending.is_empty() {
        let text = String::from_utf8_lossy(&pending).into_owned();
        let line = text.trim_end_matches('\r');
        if serde_json::from_str::<serde_json::Value>(line).is_ok() {
            for ev in engine.parse_line(line) {
                if matches!(ev, EngineEvent::QuotaExhausted { .. }) {
                    quota = true;
                }
                let _ = sink.send(ev).await;
            }
        }
    }
    drop(pending);

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
            stderr_excerpt: failure_excerpt(
                &String::from_utf8_lossy(&stderr_bytes),
                last_error.as_deref(),
            ),
        })
    }
}

/// Hata özeti: stderr doluysa o; boşsa (ör. Codex hatayı stdout JSON'unda bildirir) motorun
/// son `Error` olayının mesajı.
pub(crate) fn failure_excerpt(stderr: &str, last_error: Option<&str>) -> String {
    if stderr.trim().is_empty()
        && let Some(msg) = last_error
    {
        return excerpt(msg, DEFAULT_EXCERPT_CHARS);
    }
    excerpt(stderr, DEFAULT_EXCERPT_CHARS)
}

#[cfg(test)]
mod failure_excerpt_tests {
    use super::failure_excerpt;

    #[test]
    fn empty_stderr_falls_back_to_engine_error() {
        let got = failure_excerpt("", Some("model_not_found: desteklenmiyor"));
        assert!(got.contains("model_not_found"), "{got}");
    }

    #[test]
    fn stderr_wins_when_present() {
        let got = failure_excerpt("stderr metni", Some("motor hatası"));
        assert!(got.contains("stderr metni") && !got.contains("motor hatası"), "{got}");
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
pub(crate) mod job {
    use std::io::{Error, ErrorKind, Result};
    use std::os::windows::io::{AsRawHandle, RawHandle};

    use tokio::process::Child;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject,
    };
    use windows_sys::Win32::System::Threading::{
        OpenThread, ResumeThread, THREAD_SUSPEND_RESUME,
    };

    /// Test kancası: `true` iken `attach` her zaman başarısız olur.
    #[cfg(test)]
    pub static FORCE_ATTACH_FAILURE: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);

    /// Süreç ağacı için Job Object; kapanınca içindeki tüm süreçler ölür.
    pub struct Job(HANDLE);

    // SAFETY: Job Object tutacakları iş parçacıkları arasında kullanılabilir.
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    impl Job {
        /// Askıda başlatılmış süreci yeni bir Job Object'e bağlar, sonra ana iş parçacığını
        /// devam ettirir. Herhangi bir adım başarısızsa hata döner; süreci öldürmek çağıranın
        /// işidir (süreç askıdayken çalışmaz, `kill` güvenlidir).
        pub fn attach(child: &Child) -> Result<Job> {
            let process = child
                .raw_handle()
                .ok_or_else(|| Error::other("süreç tutacağı yok"))?;
            let pid = child
                .id()
                .ok_or_else(|| Error::other("süreç kimliği yok"))?;
            attach_handle(process, pid)
        }

        /// `std::process::Child` için aynı bağlama (askıda başlatılmış süreç varsayılır).
        pub fn attach_std(child: &std::process::Child) -> Result<Job> {
            attach_handle(child.as_raw_handle(), child.id())
        }

        pub fn terminate(&self) {
            // SAFETY: tutacak `Job` yaşadıkça geçerlidir.
            unsafe {
                TerminateJobObject(self.0, 1);
            }
        }
    }

    fn attach_handle(process: RawHandle, pid: u32) -> Result<Job> {
        #[cfg(test)]
        if FORCE_ATTACH_FAILURE.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(Error::other("test: iş nesnesi bağlanamadı"));
        }
        // SAFETY: Win32 çağrıları geçerli tutacaklarla yapılır. `job` yalnızca bu
        // fonksiyonda yaratılır; hata yolunda `Drop` tutacağı kapatır.
        unsafe {
            let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if handle.is_null() {
                return Err(Error::last_os_error());
            }
            let job = Job(handle);
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let set = SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                (&raw const info).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if set == 0 {
                return Err(Error::last_os_error());
            }
            if AssignProcessToJobObject(handle, process as HANDLE) == 0 {
                return Err(Error::last_os_error());
            }
            resume_main_thread(pid)?;
            Ok(job)
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: tutacak yalnızca burada kapatılır.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    /// `pid`'in ana iş parçacığını (askıda başlatılmış süreçte tek iş parçacığı vardır)
    /// Toolhelp anlık görüntüsüyle bulup devam ettirir.
    ///
    /// SAFETY: çağıran geçerli bir süreç kimliği verir; tüm tutacaklar bu fonksiyonda
    /// açılıp kapatılır.
    unsafe fn resume_main_thread(pid: u32) -> Result<()> {
        // SAFETY: bkz. fonksiyon belgesi.
        unsafe {
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
            if snapshot == INVALID_HANDLE_VALUE {
                return Err(Error::last_os_error());
            }
            let mut entry: THREADENTRY32 = std::mem::zeroed();
            entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
            let mut tid: Option<u32> = None;
            let mut more = Thread32First(snapshot, &mut entry) != 0;
            while more {
                if entry.th32OwnerProcessID == pid {
                    tid = Some(entry.th32ThreadID);
                    break;
                }
                more = Thread32Next(snapshot, &mut entry) != 0;
            }
            CloseHandle(snapshot);
            let tid = tid.ok_or_else(|| Error::new(ErrorKind::NotFound, "ana iş parçacığı yok"))?;

            let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, tid);
            if thread.is_null() {
                return Err(Error::last_os_error());
            }
            let previous = ResumeThread(thread);
            CloseHandle(thread);
            if previous == u32::MAX {
                return Err(Error::last_os_error());
            }
            Ok(())
        }
    }

    #[cfg(test)]
    mod tests {
        use std::sync::atomic::Ordering;

        use super::{FORCE_ATTACH_FAILURE, Job};

        #[tokio::test]
        async fn job_attach_failure_is_an_error() {
            let mut child = tokio::process::Command::new("cmd")
                .args(["/C", "timeout /T 30 /NOBREAK >NUL"])
                .kill_on_drop(true)
                .spawn()
                .expect("test süreci başlatılamadı");
            FORCE_ATTACH_FAILURE.store(true, Ordering::SeqCst);
            let result = Job::attach(&child);
            FORCE_ATTACH_FAILURE.store(false, Ordering::SeqCst);
            assert!(result.is_err(), "bağlama hatası Ok olarak dönmemeli");
            let _ = child.kill().await;
        }
    }
}

#[cfg(not(windows))]
pub(crate) mod job {
    use std::io::Result;

    use tokio::process::Child;

    pub struct Job;

    impl Job {
        pub fn attach(_child: &Child) -> Result<Job> {
            Ok(Job)
        }
        pub fn attach_std(_child: &std::process::Child) -> Result<Job> {
            Ok(Job)
        }
        pub fn terminate(&self) {}
    }
}
