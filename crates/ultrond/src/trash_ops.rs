//! `TrashPut`: motorun `ultron trash` çağrısı için kısıtlı çöp kutusuna taşıma.
//! Sohbet ve oturum dizini tur belirtecinden gelir; yollar burada yeniden doğrulanır.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Result, anyhow, bail};
use ultron_core::Paths;
use ultron_core::policy::PolicyConfig;
use ultron_core::trash::{Trash, TrashEntry};

/// Çöp kutusu işlemleri tek seferde bir tane çalışır.
pub(crate) static TRASH_LOCK: Mutex<()> = Mutex::new(());

/// Bağlantı, junction veya başka bir reparse noktası mı?
fn is_link(path: &Path) -> Result<bool> {
    let meta = std::fs::symlink_metadata(path)
        .map_err(|_| anyhow!("yol bulunamadı: {}", path.display()))?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const REPARSE_POINT: u32 = 0x400;
        Ok(meta.file_attributes() & REPARSE_POINT != 0 || meta.is_symlink())
    }
    #[cfg(not(windows))]
    {
        Ok(meta.is_symlink())
    }
}

/// Özgün (kanonikleştirilmemiş) hedefi ve oturum köküne kadar tüm üst dizinlerini bağlantı
/// açısından denetler; sonra kanonik yolu döndürür.
fn checked_target(target: &Path, root: &Path) -> Result<PathBuf> {
    let text = target.to_string_lossy();
    if ultron_core::import::discovery::is_remote_or_verbatim(&text) {
        bail!("UNC ve verbatim biçimli yollar kabul edilmez");
    }
    if !target.is_absolute() {
        bail!("yol mutlak olmalı");
    }
    if target.components().any(|c| {
        matches!(
            c,
            std::path::Component::ParentDir | std::path::Component::CurDir
        )
    }) {
        bail!("yol `.` veya `..` bileşeni içeremez");
    }
    let mut reached_root = false;
    for ancestor in target.ancestors() {
        if is_link(ancestor)? {
            bail!(
                "bağlantı (symlink/junction) içeren yollar taşınamaz: {}",
                ancestor.display()
            );
        }
        if std::fs::canonicalize(ancestor).ok().as_deref() == Some(root) {
            reached_root = true;
            break;
        }
    }
    if !reached_root {
        bail!("yol oturum dizini dışında: {}", target.display());
    }
    let real = std::fs::canonicalize(target)
        .map_err(|_| anyhow!("yol bulunamadı: {}", target.display()))?;
    if real == root || !real.starts_with(root) {
        bail!("yol oturum dizini dışında: {}", target.display());
    }
    Ok(real)
}

/// Dizin ağacında herhangi bir derinlikte `.git` var mı? Sınırlı tarama; bütçe aşılırsa `true`
/// (reddet) döner.
fn subtree_has_git(dir: &Path) -> bool {
    const BUDGET: usize = 50_000;
    let mut seen = 0usize;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(read) = std::fs::read_dir(&current) else {
            return true;
        };
        for entry in read {
            let Ok(entry) = entry else {
                return true;
            };
            seen += 1;
            if seen > BUDGET || entry.file_name().eq_ignore_ascii_case(".git") {
                return true;
            }
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => stack.push(entry.path()),
                Ok(_) => {}
                Err(_) => return true,
            }
        }
    }
    false
}

fn canonical_or_self(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Korumalı/finans yolları ve `.git` kanonik hedefte reddedilir (hedef altında veya üstünde).
fn policy_check(paths: &Paths, root: &Path, real: &Path) -> Result<()> {
    let relative = real.strip_prefix(root).unwrap_or(real);
    if relative
        .components()
        .any(|c| c.as_os_str().eq_ignore_ascii_case(".git"))
        || (real.is_dir() && subtree_has_git(real))
    {
        bail!(".git ve içeriği çöp kutusuna taşınamaz: {}", real.display());
    }
    let cfg = PolicyConfig::load(paths)
        .map_err(|_| anyhow!("güvenlik kuralları okunamadı; taşıma reddedildi"))?;
    for guarded in cfg.protected_paths.iter().chain(cfg.finance_paths.iter()) {
        let guarded = if guarded.is_absolute() {
            canonical_or_self(guarded)
        } else {
            canonical_or_self(&root.join(guarded))
        };
        if real.starts_with(&guarded) || guarded.starts_with(real) {
            bail!(
                "korumalı veya finans yolu çöp kutusuna taşınamaz: {}",
                real.display()
            );
        }
    }
    Ok(())
}

/// Tam doğrulama: bağlantı, kök, Ultron ev dizini, korumalı yol ve `.git` denetimleri.
fn validate(paths: &Paths, chat_id: &str, root: &Path, target: &Path) -> Result<PathBuf> {
    let home = canonical_or_self(&paths.root);
    let scratch = canonical_or_self(&paths.root.join("workspaces").join(chat_id));
    // C6: yalnız sohbetin kendi geçici çalışma alanında ev dizini istisnası vardır.
    let own_scratch = root == scratch;
    if !own_scratch && root.starts_with(&home) {
        bail!("oturum dizini Ultron ev dizini içinde; taşıma reddedildi");
    }
    let real = checked_target(target, root)?;
    if !own_scratch && (real.starts_with(&home) || home.starts_with(&real)) {
        bail!("Ultron ev dizini veya içindeki yollar taşınamaz");
    }
    policy_check(paths, root, &real)?;
    Ok(real)
}

/// Hedefleri oturum kökü altında doğrulayıp çöpe taşır. Önce hepsi doğrulanır; taşımadan hemen
/// önce yeniden doğrulanır ve taşınan kaydın özgün yolu yine kök altında olmalıdır.
pub(crate) fn put_inside(
    paths: &Paths,
    chat_id: &str,
    root: &Path,
    targets: &[PathBuf],
) -> Result<Vec<TrashEntry>> {
    if targets.is_empty() {
        bail!("taşınacak yol verilmedi");
    }
    let root = std::fs::canonicalize(root).map_err(|_| anyhow!("oturum dizini bulunamadı"))?;
    for target in targets {
        validate(paths, chat_id, &root, target)?;
    }
    let _guard = TRASH_LOCK
        .lock()
        .map_err(|_| anyhow!("çöp kilidi zehirlendi"))?;
    let trash = Trash::new(paths);
    let mut entries: Vec<TrashEntry> = Vec::new();
    for target in targets {
        // Taşımadan hemen önce yeniden doğrula (denetim ile taşıma arasındaki pencereyi daraltır).
        let real = validate(paths, chat_id, &root, target)?;
        let entry = match trash.put(&real, Some(chat_id)) {
            Ok(entry) => entry,
            Err(error) => {
                let done: Vec<_> = entries.iter().map(|e| e.id.as_str()).collect();
                bail!("çöpe taşıma başarısız (taşınanlar: {done:?}): {error:#}");
            }
        };
        if !entry.original_path.starts_with(&root) {
            // Taşıma sırasında yol değişmiş: kaydı geri al.
            let _ = trash.restore(&entry.id, None);
            bail!("taşınan yol oturum dizini dışına çıktı; işlem geri alındı");
        }
        entries.push(entry);
    }
    Ok(entries)
}
