//! Ortak güvenli servis bağlantısı ve ayrık servis başlatma.
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command as Process, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use windows_sys::Win32::Foundation::{
    GetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation,
};

use crate::Paths;
use crate::ipc::{self, Client};

/// Servis yokken ultrond'un hazır olması için beklenen en uzun süre.
const START_WAIT: Duration = Duration::from_secs(5);

/// Önce aynı klasördeki servisi, yoksa açık ULTROND_PATH yolunu bulur.
pub fn locate_ultrond(current_exe: &Path) -> Result<PathBuf> {
    let sibling = current_exe.with_file_name("ultrond.exe");
    if sibling.is_file() {
        return Ok(sibling);
    }
    if let Some(path) = std::env::var_os("ULTROND_PATH").map(PathBuf::from) {
        if path.is_file() {
            return Ok(path);
        }
        bail!("ultrond bulunamadı: {}", path.display());
    }
    bail!("ultrond bulunamadı: {}", sibling.display());
}

fn is_not_running(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<io::Error>()
            .is_some_and(|e| e.kind() == io::ErrorKind::NotFound)
    })
}

async fn try_connect(paths: &Paths, pipe: &str) -> Result<Client> {
    let token = ipc::read_token(paths)?;
    Client::connect(pipe, &token).await
}

/// Verilen tutamaçların miras bırakılabilirlik bayrağını kapatır; önceki bayrakları döner.
fn disable_inherit(handles: &[HANDLE]) -> Vec<(HANDLE, u32)> {
    let mut saved = Vec::new();
    for &h in handles {
        let mut flags = 0u32;
        // SAFETY: geçersiz tutamaç yalnız hata döndürür.
        if unsafe { GetHandleInformation(h, &mut flags) } != 0 {
            // SAFETY: yukarıdaki gerekçe.
            unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0) };
            saved.push((h, flags & HANDLE_FLAG_INHERIT));
        }
    }
    saved
}

fn spawn_ultrond(path: &Path) -> Result<()> {
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    // Bu sürecin standart tutamaçları (ör. `ultron ... | tee` borusu) servise miras kalmamalı;
    // yoksa servis çalıştığı sürece boru kapanmaz.
    let saved = disable_inherit(&[
        io::stdin().as_raw_handle() as HANDLE,
        io::stdout().as_raw_handle() as HANDLE,
        io::stderr().as_raw_handle() as HANDLE,
    ]);
    let spawned = Process::new(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .spawn();
    for (h, flag) in saved {
        // SAFETY: tutamaç hâlâ bu sürecin standart tutamacı.
        unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT, flag) };
    }
    spawned.context("ultrond başlatılamadı")?;
    Ok(())
}

/// Servise bağlanır; servis çalışmıyorsa `ultrond`'u ayrık süreç olarak başlatıp 5 sn bekler.
pub async fn connect_or_start(paths: &Paths, ultrond_exe: &Path) -> Result<Client> {
    let pipe = ipc::pipe_name()?;
    let first = match try_connect(paths, &pipe).await {
        Ok(client) => return Ok(client),
        Err(e) => e,
    };
    if !is_not_running(&first) {
        return Err(first);
    }
    spawn_ultrond(ultrond_exe)?;
    let deadline = Instant::now() + START_WAIT;
    let mut last = first;
    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
        match try_connect(paths, &pipe).await {
            Ok(client) => return Ok(client),
            Err(e) => last = e,
        }
    }
    Err(last.context("ultrond 5 saniye içinde hazır olmadı"))
}
