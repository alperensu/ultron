//! Dış değişiklik algılama: Ultron dışında (kullanıcı, başka araç) yapılan dosya değişikliklerini
//! özetler. Özet, bir sonraki turun bağlamına ve olay günlüğüne girer.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::Result;
use chrono::{DateTime, Utc};

use crate::engine::runner::job;

/// Özette listelenecek en fazla dosya sayısı.
const MAX_FILES: usize = 50;

/// Git dışı taramada atlanan dizin adları.
const SKIP_DIRS: [&str; 3] = ["target", "node_modules", ".git"];

/// Tek bir git çağrısının en fazla süresi; aşılırsa süreç öldürülür.
const GIT_TIMEOUT: Duration = Duration::from_secs(10);

/// Tek bir git çağrısının stdout üst sınırı (bayt).
const GIT_STDOUT_CAP: u64 = 1024 * 1024;

/// `cwd` bir git deposundaysa güncel HEAD'i döner.
pub fn git_head(cwd: &Path) -> Option<String> {
    run_git(cwd, &["rev-parse", "HEAD"]).map(|s| s.trim().to_string())
}

/// `since` anından beri Ultron dışında değişenleri özetler; değişiklik yoksa `None`.
///
/// Git deposunda: `git status --porcelain` (yalnız `since` sonrası değişen ya da silinmiş
/// dosyalar) ve `last_head` ile güncel HEAD farklıysa `git diff --stat`. Git dışında:
/// `mtime > since` olan dosyalar (en fazla 50). `exclude` (Ultron'un dokunduğu dosyalar,
/// göreli ya da mutlak) listeden çıkarılır.
pub fn changes_since(
    cwd: &Path,
    since: DateTime<Utc>,
    last_head: Option<&str>,
    exclude: &BTreeSet<String>,
) -> Result<Option<String>> {
    let since: SystemTime = since.into();
    let excluded: BTreeSet<String> = exclude
        .iter()
        .map(|e| {
            let path = PathBuf::from(e);
            let abs = if path.is_absolute() {
                path
            } else {
                cwd.join(path)
            };
            norm(&abs)
        })
        .collect();

    match run_git(cwd, &["rev-parse", "--show-toplevel"]) {
        Some(top) => git_summary(cwd, Path::new(top.trim()), since, last_head, &excluded),
        None => Ok(walk_summary(cwd, since, &excluded)),
    }
}

fn git_summary(
    cwd: &Path,
    top: &Path,
    since: SystemTime,
    last_head: Option<&str>,
    excluded: &BTreeSet<String>,
) -> Result<Option<String>> {
    // Filtre sürücüleri (clean/smudge) `git status` sırasında kod çalıştırır; böyle bir depoda
    // git tabanlı algılama yapılmaz.
    if has_filter_driver(cwd) {
        tracing::warn!("depoda git filtre sürücüsü var; dış değişiklik mtime ile aranıyor");
        return Ok(walk_summary(cwd, since, excluded));
    }
    let mut sections = Vec::new();

    let status = run_git(cwd, &["status", "--porcelain", "-uall"]).unwrap_or_default();
    let mut lines = Vec::new();
    for line in status.lines() {
        let Some(rest) = line.get(3..) else { continue };
        let rel = rest.rsplit(" -> ").next().unwrap_or(rest).trim_matches('"');
        let abs = top.join(rel);
        // Windows kısa/uzun yol adları farklı olabilir; göreli sonekle de eşleştirilir.
        let rel_norm = format!("/{}", rel.replace('\\', "/").to_lowercase());
        if excluded.contains(&norm(&abs)) || excluded.iter().any(|e| e.ends_with(&rel_norm)) {
            continue;
        }
        let fresh = match std::fs::metadata(&abs).and_then(|m| m.modified()) {
            Ok(modified) => modified > since,
            // Silinmiş dosya: silme ancak üst dizin `since` sonrasında değiştiyse bu turda olmuştur;
            // aksi halde önceki turda zaten raporlanmıştır.
            Err(_) => abs
                .parent()
                .and_then(|dir| std::fs::metadata(dir).ok())
                .and_then(|m| m.modified().ok())
                .is_none_or(|m| m > since),
        };
        if fresh {
            lines.push(line.to_string());
        }
    }
    if !lines.is_empty() {
        let extra = lines.len().saturating_sub(MAX_FILES);
        lines.truncate(MAX_FILES);
        let mut block = format!("git durumu:\n{}", lines.join("\n"));
        if extra > 0 {
            block.push_str(&format!("\n… (+{extra} dosya daha)"));
        }
        sections.push(block);
    }

    if let (Some(old), Some(head)) = (last_head, git_head(cwd))
        && old != head
        && let Some(stat) = run_git(cwd, &["diff", "--stat", &format!("{old}..{head}")])
        && !stat.trim().is_empty()
    {
        sections.push(format!("git diff --stat (son görülen HEAD..HEAD):\n{}", stat.trim_end()));
    }

    Ok((!sections.is_empty()).then(|| sections.join("\n\n")))
}

fn walk_summary(cwd: &Path, since: SystemTime, excluded: &BTreeSet<String>) -> Option<String> {
    let mut found: Vec<String> = Vec::new();
    let mut stack = vec![cwd.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if !SKIP_DIRS.contains(&name.as_str()) {
                    stack.push(path);
                }
            } else if kind.is_file()
                && entry.metadata().and_then(|m| m.modified()).is_ok_and(|m| m > since)
                && !excluded.contains(&norm(&path))
            {
                let rel = path.strip_prefix(cwd).unwrap_or(&path);
                found.push(rel.display().to_string());
            }
        }
    }
    if found.is_empty() {
        return None;
    }
    found.sort();
    let extra = found.len().saturating_sub(MAX_FILES);
    found.truncate(MAX_FILES);
    let mut out = format!("değişen dosyalar:\n- {}", found.join("\n- "));
    if extra > 0 {
        out.push_str(&format!("\n… (+{extra} dosya daha)"));
    }
    Some(out)
}

/// Karşılaştırma için yolu `/` ayraçlı ve küçük harfli hale getirir (Windows).
fn norm(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/").to_lowercase()
}

/// Otomatik git çağrısının argümanları. Depo tanımlı komutlar (fsmonitor, hook'lar, dış diff
/// ve textconv sürücüleri) devre dışı bırakılır; depo içeriği Ultron'un yerine kod çalıştıramaz.
fn git_args(base: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = [
        "-c",
        "core.quotepath=off",
        "-c",
        "core.fsmonitor=false",
        "-c",
        "core.hooksPath=NUL",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    match base.split_first() {
        Some((&"diff", rest)) => {
            out.push("diff".into());
            out.push("--no-ext-diff".into());
            out.push("--no-textconv".into());
            out.extend(rest.iter().map(|s| s.to_string()));
        }
        _ => out.extend(base.iter().map(|s| s.to_string())),
    }
    out
}

/// Git çağrısı çalıştırır. Zaman aşımı veya stdout üst sınırının aşılması durumunda ağacı
/// öldürür, uyarı yazar ve `None` döner; çağıran dış değişiklik özetini atlar.
fn run_git(cwd: &Path, args: &[&str]) -> Option<String> {
    git_output(cwd, args).and_then(|(code, out)| (code == Some(0)).then_some(out))
}

/// Git çağrısı; çıkış kodu ve stdout. Başlatma hatası, zaman aşımı veya üst sınır aşımında `None`.
fn git_output(cwd: &Path, args: &[&str]) -> Option<(Option<i32>, String)> {
    let mut cmd = Command::new("git");
    cmd.args(git_args(args))
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    scrub_git_env(&mut cmd, std::env::vars_os().map(|(key, _)| key));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        // Süreç ana iş parçacığı askıda başlar; iş nesnesine bağlanmadan kod çalıştırmaz.
        const CREATE_SUSPENDED: u32 = 0x0000_0004;
        cmd.creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED);
    }
    let mut child = cmd.spawn().ok()?;
    let Some(stdout) = child.stdout.take() else {
        kill_child(&mut child);
        return None;
    };
    // Git'in başlattığı tüm torunlar (fsmonitor, filtre süreçleri) aynı iş nesnesinde olur.
    let job = match job::Job::attach_std(&child) {
        Ok(job) => job,
        Err(_) => {
            kill_child(&mut child);
            return None;
        }
    };
    let (tx, rx) = mpsc::channel();
    // Okuyucu üst sınırın bir fazlasını okuyunca boruyu bırakır; süreç yazmaya devam ederse
    // boru kırılır ve süreç sonlanır.
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let res = stdout.take(GIT_STDOUT_CAP + 1).read_to_end(&mut buf);
        let _ = tx.send(res.map(|_| buf));
    });

    let deadline = Instant::now() + GIT_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                job.terminate();
                kill_child(&mut child);
                tracing::warn!(?args, "git çağrısı zaman aşımına uğradı, atlandı");
                return None;
            }
        }
    };
    let remaining = deadline
        .saturating_duration_since(Instant::now())
        .max(Duration::from_millis(100));
    let buf = match rx.recv_timeout(remaining) {
        Ok(Ok(buf)) => buf,
        _ => {
            // Çocuk çıktı ama boruyu tutan bir torun kaldı ya da okuma başarısız oldu.
            job.terminate();
            kill_child(&mut child);
            return None;
        }
    };
    if buf.len() as u64 > GIT_STDOUT_CAP {
        tracing::warn!(?args, "git çıktısı üst sınırı aştı, atlandı");
        return None;
    }
    Some((status.code(), String::from_utf8_lossy(&buf).into_owned()))
}

/// Depoda (veya genel/sistem yapılandırmasında) bir `filter.<sürücü>` tanımlı mı? Yalnız
/// yapılandırma okunur; hiçbir komut çalıştırılmaz. Belirsizlikte `true` döner (güvenli yol).
fn has_filter_driver(cwd: &Path) -> bool {
    match git_output(cwd, &["config", "--name-only", "--get-regexp", r"^filter\."]) {
        Some((Some(0), out)) => !out.trim().is_empty(),
        // Eşleşme yoksa git 1 ile çıkar.
        Some((Some(1), _)) => false,
        _ => true,
    }
}

/// Git sürecinin ortamını temizler: tüm `GIT_*` değişkenleri (GIT_DIR, GIT_INDEX_FILE, GIT_CONFIG_*
/// vb.) kaldırılır; sistem yapılandırması ve kimlik istemi kapatılır.
fn scrub_git_env(cmd: &mut Command, vars: impl IntoIterator<Item = OsString>) {
    for key in vars {
        if key.to_string_lossy().to_ascii_uppercase().starts_with("GIT_") {
            cmd.env_remove(key);
        }
    }
    cmd.env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_TERMINAL_PROMPT", "0");
}

fn kill_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_calls_disable_fsmonitor_and_ext_diff() {
        let status = git_args(&["status", "--porcelain"]);
        for flag in ["core.fsmonitor=false", "core.hooksPath=NUL", "core.quotepath=off"] {
            assert!(status.iter().any(|a| a == flag), "eksik: {flag}");
        }
        assert!(!status.iter().any(|a| a == "--no-ext-diff"));

        let diff = git_args(&["diff", "--stat", "a..b"]);
        assert!(diff.iter().any(|a| a == "core.fsmonitor=false"));
        assert!(diff.iter().any(|a| a == "--no-ext-diff"));
        assert!(diff.iter().any(|a| a == "--no-textconv"));
        assert_eq!(diff.last().map(String::as_str), Some("a..b"));
    }

    #[test]
    fn fsmonitor_hook_is_not_executed() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let init = Command::new("git").arg("init").arg("-q").current_dir(repo).status();
        if !init.is_ok_and(|s| s.success()) {
            return; // git yoksa test atlanır
        }
        std::fs::write(repo.join("a.txt"), "x").unwrap();
        let marker = repo.join("fsmonitor-marker");
        let marker_str = marker.to_string_lossy().replace('\\', "/");
        let hook = format!("touch '{marker_str}'");
        let set = Command::new("git")
            .args(["config", "core.fsmonitor", &hook])
            .current_dir(repo)
            .status()
            .unwrap();
        assert!(set.success());

        let since = chrono::Utc::now() - chrono::Duration::hours(1);
        let _ = changes_since(repo, since, None, &BTreeSet::new()).unwrap();
        assert!(!marker.exists(), "core.fsmonitor kanca olarak çalıştırıldı");
    }

    #[test]
    fn git_env_is_scrubbed() {
        let mut cmd = Command::new("git");
        scrub_git_env(
            &mut cmd,
            [
                OsString::from("GIT_DIR"),
                OsString::from("PATH"),
                OsString::from("GIT_INDEX_FILE"),
            ],
        );
        let envs: Vec<(OsString, Option<OsString>)> = cmd
            .get_envs()
            .map(|(k, v)| (k.to_os_string(), v.map(|v| v.to_os_string())))
            .collect();
        let has = |key: &str, val: Option<&str>| {
            envs.iter()
                .any(|(k, v)| k == key && v.as_deref() == val.map(std::ffi::OsStr::new))
        };
        assert!(has("GIT_DIR", None), "GIT_DIR kaldırılmadı: {envs:?}");
        assert!(has("GIT_INDEX_FILE", None), "GIT_INDEX_FILE kaldırılmadı");
        assert!(!envs.iter().any(|(k, _)| k == "PATH"), "PATH kaldırıldı");
        assert!(has("GIT_CONFIG_NOSYSTEM", Some("1")));
        assert!(has("GIT_TERMINAL_PROMPT", Some("0")));
    }

    fn git_ok(repo: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(repo)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} başarısız");
    }

    #[test]
    fn filter_driver_forces_mtime_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        if !Command::new("git").arg("init").arg("-q").current_dir(repo).status().is_ok_and(|s| s.success()) {
            return; // git yoksa test atlanır
        }
        std::fs::write(repo.join(".gitattributes"), "* filter=x\n").unwrap();
        std::fs::write(repo.join("a.txt"), "bir").unwrap();
        git_ok(repo, &["add", "."]);
        // Filtre tanımlanmadan commit edilir; böylece kurulum sırasında sürücü çalışmaz.
        git_ok(
            repo,
            &[
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.com",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-qm",
                "ilk",
            ],
        );
        let marker = repo.join("filter-marker");
        let marker_str = marker.to_string_lossy().replace('\\', "/");
        git_ok(
            repo,
            &["config", "filter.x.clean", &format!("touch '{marker_str}' && cat")],
        );

        let since = chrono::Utc::now() - chrono::Duration::hours(1);
        std::fs::write(repo.join("a.txt"), "iki").unwrap();
        // Denetim: doğrudan git status sürücüyü çalıştırmalı (sürücü gerçekten etkin).
        let _ = Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(repo)
            .status();
        assert!(marker.exists(), "denetim: filtre sürücüsü çalışmadı");
        std::fs::remove_file(&marker).unwrap();

        let summary = changes_since(repo, since, None, &BTreeSet::new())
            .unwrap()
            .expect("değişiklik bekleniyordu");
        assert!(summary.contains("a.txt"), "{summary}");
        assert!(!marker.exists(), "filtre sürücüsü Ultron tarafından çalıştırıldı");
    }
}
