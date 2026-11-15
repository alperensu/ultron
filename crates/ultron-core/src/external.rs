//! Dış değişiklik algılama: Ultron dışında (kullanıcı, başka araç) yapılan dosya değişikliklerini
//! özetler. Özet, bir sonraki turun bağlamına ve olay günlüğüne girer.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::SystemTime;

use anyhow::Result;
use chrono::{DateTime, Utc};

/// Özette listelenecek en fazla dosya sayısı.
const MAX_FILES: usize = 50;

/// Git dışı taramada atlanan dizin adları.
const SKIP_DIRS: [&str; 3] = ["target", "node_modules", ".git"];

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
        let fresh = std::fs::metadata(&abs)
            .and_then(|m| m.modified())
            .map_or(true, |m| m > since);
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

fn run_git(cwd: &Path, args: &[&str]) -> Option<String> {
    let mut cmd = Command::new("git");
    cmd.args(["-c", "core.quotepath=off"])
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let out = cmd.output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}
