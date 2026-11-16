//! Markdown proje kataloğunu okur; bozuk satırlar katalog dışında bırakılır.

use anyhow::Result;

use crate::Db;
use crate::ipc::CatalogImportReport;

/// Katalogdan alınan proje; kimlik ve etkinlik bilgileri veritabanında tutulur.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectSeed {
    pub name: String,
    pub path: String,
    pub description: String,
}

fn after_separator(text: &str) -> Option<&str> {
    text.trim_start()
        .strip_prefix(['-', '—', '–'])
        .map(str::trim_start)
}

/// `- **Ad** — `yol` — açıklama` biçimini çözümler (üç tire türü desteklenir).
pub fn parse_catalog(md: &str) -> Vec<ProjectSeed> {
    md.lines()
        .filter_map(|line| {
            let line = line.trim().strip_prefix('-')?.trim_start();
            let (name, rest) = line.strip_prefix("**")?.split_once("**")?;
            let rest = after_separator(rest)?.strip_prefix('`')?;
            let (path, rest) = rest.split_once('`')?;
            let description = after_separator(rest)?.trim();
            let name = name.trim();
            let path = path.trim();
            if name.is_empty() || path.is_empty() || description.is_empty() {
                return None;
            }
            Some(ProjectSeed {
                name: name.into(),
                path: path.into(),
                description: description.into(),
            })
        })
        .collect()
}

/// Aynı yoldaki projeleri kimlik, takma ad ve etkinliklerini koruyarak günceller.
pub fn import_catalog(db: &Db, md: &str) -> Result<CatalogImportReport> {
    db.upsert_catalog_projects(&parse_catalog(md))
}
