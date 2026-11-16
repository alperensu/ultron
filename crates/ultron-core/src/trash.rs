//! Recoverable deletion. Trash directories and their contents must not contain links.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::Paths;

pub struct Trash {
    root: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TrashEntry {
    pub id: String,
    pub original_path: PathBuf,
    pub deleted_at: DateTime<Utc>,
    pub chat_id: Option<String>,
    pub size: u64,
    pub is_dir: bool,
}

impl Trash {
    pub fn new(paths: &Paths) -> Trash {
        Trash {
            root: paths.root.join("trash"),
        }
    }

    pub fn put(&self, path: &Path, chat_id: Option<&str>) -> Result<TrashEntry> {
        self.put_inner(path, chat_id, false)
    }

    /// Exercise the cross-volume copy/verify/delete path without a second drive.
    #[doc(hidden)]
    pub fn put_with_forced_copy(&self, path: &Path, chat_id: Option<&str>) -> Result<TrashEntry> {
        self.put_inner(path, chat_id, true)
    }

    fn put_inner(
        &self,
        path: &Path,
        chat_id: Option<&str>,
        force_copy: bool,
    ) -> Result<TrashEntry> {
        plain_ancestors(path)?;
        let source = fs::canonicalize(path).context("çöpe taşınacak yol bulunamadı")?;
        let root = self
            .checked_root(true)?
            .context("çöp kökü oluşturulamadı")?;
        self.validate_source(&source, &root)?;
        let tree = snapshot(&source)?;
        let size = tree_size(&tree)?;
        let entry = TrashEntry {
            id: Uuid::new_v4().to_string(),
            original_path: source.clone(),
            deleted_at: Utc::now(),
            chat_id: chat_id.map(str::to_owned),
            size,
            is_dir: fs::symlink_metadata(&source)?.is_dir(),
        };
        let slot = root.join(&entry.id);
        fs::create_dir(&slot)?;
        let moved = (|| -> Result<()> {
            // Persist the recovery information before moving any user data.
            let mut meta = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(slot.join("meta.json"))?;
            meta.write_all(&serde_json::to_vec_pretty(&entry)?)?;
            meta.sync_all()?;
            drop(meta);
            transfer(&source, &slot.join("item"), force_copy)
        })();
        if let Err(error) = moved {
            // Ordinary failures (e.g. an open Windows file) must not leave a
            // broken entry that prevents listing all other trash. Only discard
            // the attempted copy when the complete original is still present.
            if snapshot(&source).is_ok_and(|current| current == tree) {
                snapshot(&slot)?;
                ensure!(
                    fs::canonicalize(&slot)?.parent() == Some(root.as_path()),
                    "çöp kökü dışı temizlik reddedildi"
                );
                fs::remove_dir_all(&slot).context("başarısız taşıma kaydı temizlenemedi")?;
            }
            return Err(error.context(format!(
                "taşıma tamamlanamadı; kurtarma konumu: {}",
                slot.display()
            )));
        }
        Ok(entry)
    }

    pub fn list(&self) -> Result<Vec<TrashEntry>> {
        let Some(root) = self.checked_root(false)? else {
            return Ok(Vec::new());
        };
        let mut entries = Vec::new();
        for child in fs::read_dir(&root)? {
            let child = child?;
            let id = child.file_name();
            let id = id.to_str().context("geçersiz çöp kimliği")?;
            entries.push(self.read_entry(&root, id)?.0);
        }
        entries.sort_by(|a, b| b.deleted_at.cmp(&a.deleted_at).then(a.id.cmp(&b.id)));
        Ok(entries)
    }

    pub fn restore(&self, id: &str, to: Option<&Path>) -> Result<PathBuf> {
        let root = self.checked_root(false)?.context("çöp kutusu boş")?;
        let (entry, slot) = self.read_entry(&root, id)?;
        let requested = std::path::absolute(to.unwrap_or(&entry.original_path))?;
        let parent = requested
            .parent()
            .context("geri yükleme hedefi kök olamaz")?;
        plain_ancestors(parent)?;
        // Resolve the existing ancestor first; missing parents are created only
        // after checking that the destination cannot point into the trash.
        let mut existing = parent;
        while !existing.try_exists()? {
            existing = existing.parent().context("hedef üst dizini bulunamadı")?;
        }
        let resolved = fs::canonicalize(existing)?;
        let remainder = parent.strip_prefix(existing)?;
        ensure!(
            !remainder
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir)),
            "hedef üst dizin geçişi içeremez"
        );
        let target_parent = resolved.join(remainder);
        ensure!(
            !target_parent.starts_with(&root),
            "çöp kutusuna geri yüklenemez"
        );
        let name = requested.file_name().context("hedef adı eksik")?;
        fs::create_dir_all(&target_parent)?;
        let desired = target_parent.join(name);
        self.validate_source(&desired, &root)?;
        for n in 0..=u32::MAX {
            let target = if n == 0 {
                desired.clone()
            } else {
                restored_name(&desired, n)
            };
            if fs::symlink_metadata(&target).is_ok() {
                continue;
            }
            match transfer(&slot.join("item"), &target, false) {
                Ok(()) => {
                    fs::remove_file(slot.join("meta.json"))?;
                    fs::remove_dir(&slot)?;
                    return Ok(target);
                }
                Err(error)
                    if error
                        .downcast_ref::<io::Error>()
                        .is_some_and(|e| e.kind() == io::ErrorKind::AlreadyExists) =>
                {
                    continue;
                }
                Err(error) => {
                    return Err(error.context("geri yükleme tamamlanamadı; çöp kaydı korundu"));
                }
            }
        }
        bail!("boş geri yükleme adı bulunamadı")
    }

    pub fn purge_expired(&self, retention_days: u32, now: DateTime<Utc>) -> Result<usize> {
        let Some(root) = self.checked_root(false)? else {
            return Ok(0);
        };
        let cutoff = now
            .checked_sub_signed(Duration::days(i64::from(retention_days)))
            .context("saklama süresi tarih aralığı dışında")?;
        let mut count = 0;
        for entry in self.list()? {
            if entry.deleted_at <= cutoff {
                let (_, slot) = self.read_entry(&root, &entry.id)?;
                // Never use original_path for deletion. Validate every descendant
                // (including Windows junctions) before recursively removing a slot.
                snapshot(&slot)?;
                ensure!(
                    fs::canonicalize(&slot)?.parent() == Some(root.as_path()),
                    "çöp kökü dışı silme reddedildi"
                );
                fs::remove_dir_all(&slot)?;
                count += 1;
            }
        }
        Ok(count)
    }

    fn checked_root(&self, create: bool) -> Result<Option<PathBuf>> {
        plain_ancestors(&self.root)?;
        if !self.root.try_exists()? {
            if !create {
                return Ok(None);
            }
            fs::create_dir_all(&self.root)?;
        }
        ensure!(
            fs::symlink_metadata(&self.root)?.is_dir(),
            "çöp kökü dizin değil"
        );
        Ok(Some(fs::canonicalize(&self.root)?))
    }

    fn validate_source(&self, source: &Path, root: &Path) -> Result<()> {
        ensure!(source.parent().is_some(), "sürücü kökü taşınamaz");
        ensure!(
            !source.starts_with(root) && !root.starts_with(source),
            "çöp kökü veya üst dizini taşınamaz"
        );
        if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))
            && let Ok(home) = fs::canonicalize(home)
        {
            ensure!(source != home, "kullanıcı profili kökü taşınamaz");
        }
        Ok(())
    }

    fn read_entry(&self, root: &Path, id: &str) -> Result<(TrashEntry, PathBuf)> {
        ensure!(
            Uuid::parse_str(id)?.to_string() == id,
            "geçersiz çöp kimliği"
        );
        let slot = root.join(id);
        plain_ancestors(&slot)?;
        ensure!(
            fs::canonicalize(&slot)?.parent() == Some(root),
            "çöp kaydı kök dışında"
        );
        let meta = slot.join("meta.json");
        ensure!(plain_metadata(&meta)?.is_file(), "meta.json dosya değil");
        let entry: TrashEntry = serde_json::from_slice(&fs::read(meta)?)?;
        ensure!(
            entry.id == id && entry.original_path.is_absolute(),
            "bozuk çöp kaydı"
        );
        let item = slot.join("item");
        ensure!(
            plain_metadata(&item)?.is_dir() == entry.is_dir,
            "çöp türü uyuşmuyor"
        );
        ensure!(
            tree_size(&snapshot(&item)?)? == entry.size,
            "çöp boyutu uyuşmuyor"
        );
        Ok((entry, slot))
    }
}

fn plain_metadata(path: &Path) -> Result<fs::Metadata> {
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        !meta.file_type().is_symlink(),
        "sembolik bağlantı reddedildi"
    );
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        ensure!(
            meta.file_attributes() & 0x400 == 0,
            "yeniden ayrıştırma noktası reddedildi"
        );
    }
    ensure!(meta.is_file() || meta.is_dir(), "özel dosya reddedildi");
    Ok(meta)
}

fn plain_ancestors(path: &Path) -> Result<()> {
    let absolute = std::path::absolute(path)?;
    for ancestor in absolute.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(_) => {
                plain_metadata(ancestor)?;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

// Per-file sizes plus directory names catch missing/extra files and empty dirs.
type Tree = BTreeMap<PathBuf, (bool, u64)>;

fn snapshot(path: &Path) -> Result<Tree> {
    fn visit(path: &Path, relative: &Path, tree: &mut Tree) -> Result<()> {
        let meta = plain_metadata(path)?;
        tree.insert(
            relative.to_owned(),
            (meta.is_dir(), if meta.is_dir() { 0 } else { meta.len() }),
        );
        if meta.is_dir() {
            for child in fs::read_dir(path)? {
                let child = child?;
                visit(&child.path(), &relative.join(child.file_name()), tree)?;
            }
        }
        Ok(())
    }
    let mut tree = Tree::new();
    visit(path, Path::new(""), &mut tree)?;
    Ok(tree)
}

fn tree_size(tree: &Tree) -> Result<u64> {
    tree.values().try_fold(0_u64, |total, (_, size)| {
        total.checked_add(*size).context("boyut taşması")
    })
}

fn copy_tree(source: &Path, target: &Path) -> Result<()> {
    let meta = plain_metadata(source)?;
    if meta.is_dir() {
        fs::create_dir(target)?;
        for child in fs::read_dir(source)? {
            let child = child?;
            copy_tree(&child.path(), &target.join(child.file_name()))?;
        }
    } else {
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(target)?;
        io::copy(&mut fs::File::open(source)?, &mut output)?;
        output.sync_all()?;
    }
    fs::set_permissions(target, meta.permissions())?;
    Ok(())
}

fn transfer(source: &Path, target: &Path, force_copy: bool) -> Result<()> {
    if !force_copy {
        match rename_exclusive(source, target) {
            Ok(()) => return Ok(()),
            Err(e) if is_cross_volume(&e) => {}
            Err(e) => return Err(e.into()),
        }
    }
    let before = snapshot(source)?;
    copy_tree(source, target)?;
    ensure!(
        snapshot(target)? == before && snapshot(source)? == before,
        "kopya doğrulaması başarısız; kaynak korundu"
    );
    if plain_metadata(source)?.is_dir() {
        fs::remove_dir_all(source)?;
    } else {
        fs::remove_file(source)?;
    }
    Ok(())
}

#[cfg(windows)]
fn rename_exclusive(source: &Path, target: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::MoveFileExW;
    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let target: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    if source[..source.len() - 1].contains(&0) || target[..target.len() - 1].contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "yolda NUL karakteri",
        ));
    }
    // SAFETY: both arguments are live, NUL-terminated UTF-16 buffers.
    // No REPLACE_EXISTING flag: a concurrent creator must never lose its file.
    if unsafe { MoveFileExW(source.as_ptr(), target.as_ptr(), 0) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn rename_exclusive(_source: &Path, _target: &Path) -> io::Result<()> {
    // std::fs::rename can overwrite on Unix. Prefer exclusive copy there.
    Err(io::Error::from_raw_os_error(18))
}

fn is_cross_volume(error: &io::Error) -> bool {
    error.raw_os_error() == Some(if cfg!(windows) { 17 } else { 18 })
}

fn restored_name(path: &Path, n: u32) -> PathBuf {
    let mut name = path.file_stem().unwrap_or_default().to_os_string();
    name.push(format!(" (geri yüklendi {n})"));
    if let Some(extension) = path.extension() {
        name.push(".");
        name.push(extension);
    }
    path.with_file_name(name)
}
