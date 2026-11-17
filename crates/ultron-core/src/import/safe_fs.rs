//! Güvenilmeyen konumlardan (kullanıcı profili, proje klasörleri) güvenli okuma yardımcıları.
//!
//! Sembolik bağlantılar, junction/reparse noktaları ve aygıt adları izlenmez; okumalar
//! `take(N)` ile sınırlıdır ve (isteniyorsa) kanonik yol beklenen klasörün içinde kalmalıdır.

use std::fs::{File, Metadata};
use std::io::Read;
use std::os::windows::fs::MetadataExt;
use std::path::Path;

use crate::db::normalized_project_path;

const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

/// Belge/hafıza/katalog okuma üst sınırları.
pub const DOC_MAX_BYTES: u64 = 64 * 1024;
pub const CATALOG_MAX_BYTES: u64 = 256 * 1024;
pub const GIT_FILE_MAX_BYTES: u64 = 4 * 1024;

fn is_reparse(meta: &Metadata) -> bool {
    meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

/// `CON`, `NUL`, `COM1`... gibi Windows aygıt adı mı?
fn is_device_name(path: &Path) -> bool {
    let Some(stem) = path.file_stem().map(|s| s.to_string_lossy().to_ascii_uppercase()) else {
        return false;
    };
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit())
}

/// Düz (sembolik bağlantı/reparse olmayan, aygıt olmayan) dosya mı?
pub fn is_plain_file(path: &Path) -> bool {
    !is_device_name(path)
        && std::fs::symlink_metadata(path)
            .is_ok_and(|m| m.file_type().is_file() && !is_reparse(&m))
}

/// Düz (reparse olmayan) klasör mü?
pub fn is_plain_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_dir() && !is_reparse(&m))
}

fn canonical_key(path: &Path) -> Option<String> {
    let canon = std::fs::canonicalize(path).ok()?;
    let text = canon.to_string_lossy();
    let text = text.strip_prefix(r"\\?\").unwrap_or(&text);
    Some(normalized_project_path(text))
}

/// Kanonik yol `base` klasörünün içinde mi?
pub fn stays_inside(path: &Path, base: &Path) -> bool {
    match (canonical_key(path), canonical_key(base)) {
        (Some(p), Some(b)) => p.starts_with(&format!("{b}/")),
        _ => false,
    }
}

/// En çok `max` bayt okur. Düz dosya değilse, aygıt adıysa veya (verildiyse) `base` dışına
/// çıkıyorsa `None`.
pub fn read_text(path: &Path, base: Option<&Path>, max: u64) -> Option<String> {
    if !is_plain_file(path) {
        return None;
    }
    if let Some(base) = base {
        // Kanonik yol içeride kalmalı ve `base` ile dosya arasındaki her klasör düz olmalı
        // (junction/sembolik bağlantı yok).
        if !stays_inside(path, base) {
            return None;
        }
        let mut cur = path.parent();
        loop {
            let d = cur?;
            if d == base {
                break;
            }
            if !is_plain_dir(d) {
                return None;
            }
            cur = d.parent();
        }
    }
    let mut buf = Vec::new();
    File::open(path).ok()?.take(max).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_are_bounded_and_device_names_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("AGENTS.md");
        std::fs::write(&f, "a".repeat(200 * 1024)).unwrap();
        assert_eq!(read_text(&f, Some(dir.path()), DOC_MAX_BYTES).unwrap().len(), DOC_MAX_BYTES as usize);
        assert!(!is_plain_file(Path::new("NUL")) && !is_plain_file(Path::new("C:/x/CON.md")));
        // Beklenen klasörün dışındaki dosya reddedilir.
        let other = tempfile::tempdir().unwrap();
        assert!(read_text(&f, Some(other.path()), 10).is_none());
    }

    #[test]
    fn junctions_and_symlinks_are_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let secret_dir = dir.path().join("gizli");
        std::fs::create_dir_all(&secret_dir).unwrap();
        std::fs::write(secret_dir.join("x.md"), "GIZLI").unwrap();
        let link = dir.path().join("baglanti");
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&secret_dir)
            .output()
            .is_ok_and(|o| o.status.success());
        if made {
            assert!(!is_plain_dir(&link), "junction düz klasör sayılmamalı");
            assert!(read_text(&link.join("x.md"), Some(dir.path()), 100).is_none());
        }
        let file_link = dir.path().join("dosya-baglantisi.md");
        if std::os::windows::fs::symlink_file(secret_dir.join("x.md"), &file_link).is_ok() {
            assert!(!is_plain_file(&file_link));
            assert!(read_text(&file_link, None, 100).is_none());
        }
    }
}
