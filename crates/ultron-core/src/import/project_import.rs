//! Proje içe aktarma (bulma + geçmiş notu) ve içe aktarılmış sohbetleri kaldırma.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Result, anyhow};
use chrono::Utc;

use super::discovery::{Filters, FoundProject, discover};
use super::notes::{NoteCtx, PrevNote, build_note, collect, has_sources, one_line, parse_note, source_stamp};
use crate::db::{Db, ProjectNote, StoredEvent};
use crate::ipc::{ProjectImportReport, ProjectImportRow, ProjectInfo, RemoveChatsReport};
use crate::summarizer::{Summarizer, SummaryFuture};
use crate::workstate::WorkState;

/// Bir çalıştırmada işlenen en çok proje ve özetleyiciye yapılacak en çok çağrı.
pub const MAX_PROJECTS: usize = 300;
pub const MAX_SUMMARY_CALLS: usize = 200;
/// Art arda bu kadar özetleyici hatasından sonra kalan projeler atlanır.
const MAX_CONSECUTIVE_FAILURES: u32 = 2;

fn folder_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| path.to_string())
}

/// Çağrı sayısını sınırlayan özetleyici sarmalayıcısı.
struct Capped<'a> {
    inner: &'a dyn Summarizer,
    left: AtomicUsize,
}

impl Summarizer for Capped<'_> {
    fn refresh<'b>(&'b self, state: &'b WorkState, recent: &'b [StoredEvent]) -> SummaryFuture<'b> {
        let allowed = self
            .left
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok();
        if !allowed {
            return Box::pin(async { Err(anyhow!("özetleyici çağrı sınırına ulaşıldı")) });
        }
        self.inner.refresh(state, recent)
    }

    fn kind(&self) -> &'static str {
        self.inner.kind()
    }
}

/// Notun ne yapılacağı.
enum Plan {
    Skip(&'static str),
    Build { shrunk: bool },
}

fn plan(
    usable: bool,
    prev: Option<&ProjectNote>,
    stamp: i64,
    total: usize,
    kind: &str,
) -> Plan {
    if !usable {
        return Plan::Skip("kaynak yok");
    }
    let Some(prev) = prev else {
        return Plan::Build { shrunk: false };
    };
    // Özetleyici yokken özetlenmiş bir notu ham alıntılı nota düşürme.
    if kind == "null" && !prev.summarizer.is_empty() && prev.summarizer != "null" {
        // Not korunur; kaynaklar değiştiyse eski kaldığı açıkça bildirilir.
        let changed = stamp > prev.source_stamp || total != prev.claude_sessions + prev.codex_sessions;
        return Plan::Skip(if changed { "bayat (özetleyici yok)" } else { "güncel" });
    }
    let kind_changed = prev.summarizer != kind;
    let prev_total = prev.claude_sessions + prev.codex_sessions;
    if !kind_changed && stamp <= prev.source_stamp && total <= prev_total {
        return Plan::Skip("güncel");
    }
    // Eski dökümler silinmişse (sayı azaldı) eski içerik korunur, yalnız yeni bilgi eklenir.
    Plan::Build {
        shrunk: total < prev_total,
    }
}

/// Projeleri bulur, yoksa oluşturur ve yeni oturum/belge varsa geçmiş notunu günceller.
/// `dry_run`: motor çağrılmaz, hiçbir şey yazılmaz.
pub async fn import_projects(
    db: &Db,
    summarizer: &dyn Summarizer,
    home: &Path,
    filters: &Filters,
    dry_run: bool,
) -> Result<ProjectImportReport> {
    let (home_owned, filters_owned) = (home.to_path_buf(), filters.clone());
    let mut found = tokio::task::spawn_blocking(move || discover(&home_owned, &filters_owned))
        .await
        .map_err(|e| anyhow!("keşif görevi başarısız: {e}"))?;
    // En yeni çalışılan projeler önce; üst sınırdan sonra yola göre sıralı gösterilir.
    found.projects.sort_by_key(|p| {
        std::cmp::Reverse(
            p.claude
                .iter()
                .chain(&p.codex)
                .map(|f| f.mtime_ms)
                .max()
                .unwrap_or(0),
        )
    });
    found.projects.truncate(MAX_PROJECTS);
    found.projects.sort_by_key(|p| p.path.to_lowercase());

    let capped = Capped {
        inner: summarizer,
        left: AtomicUsize::new(MAX_SUMMARY_CALLS),
    };
    let mut report = ProjectImportReport {
        dry_run,
        unmatched_sessions: found.unmatched_sessions,
        summarizer: summarizer.kind().to_string(),
        ..ProjectImportReport::default()
    };
    let mut failures = 0u32;
    for item in &found.projects {
        let row = import_one(db, &capped, home, item, dry_run, &mut failures).await?;
        report.created += usize::from(row.project_created);
        report.notes_updated += usize::from(row.note_updated);
        report.rows.push(row);
    }
    Ok(report)
}

async fn import_one(
    db: &Db,
    summarizer: &dyn Summarizer,
    home: &Path,
    item: &FoundProject,
    dry_run: bool,
    failures: &mut u32,
) -> Result<ProjectImportRow> {
    let existing = db.find_project_by_path(&item.path)?;
    let name = existing
        .as_ref()
        .map(|p| p.name.clone())
        .or_else(|| item.catalog.as_ref().map(|(n, _)| n.clone()))
        .unwrap_or_else(|| folder_name(&item.path));
    let (claude, codex, workers) = item.counts();
    let mut row = ProjectImportRow {
        path: item.path.clone(),
        name: name.clone(),
        claude_sessions: claude,
        codex_sessions: codex,
        worker_sessions: workers,
        project_created: existing.is_none(),
        note_updated: false,
        note_status: String::new(),
    };

    // Dosya sistemi işleri engelleyici iş parçacığında yapılır.
    let (stamp, usable) = {
        let (h, it) = (home.to_path_buf(), item.clone());
        tokio::task::spawn_blocking(move || {
            (
                source_stamp(&h, &it.path, &it.claude, &it.codex),
                has_sources(&h, &it.path, &it.claude, &it.codex),
            )
        })
        .await
        .map_err(|e| anyhow!("kaynak taraması başarısız: {e}"))?
    };
    let prev_note = match &existing {
        Some(p) => db.get_project_note(&p.id)?,
        None => None,
    };
    let kind = summarizer.kind();
    let total = item.claude.len() + item.codex.len();
    let decision = plan(usable, prev_note.as_ref(), stamp, total, kind);
    let shrunk = match decision {
        Plan::Skip(reason) => {
            row.note_status = reason.into();
            if dry_run || !row.project_created {
                return Ok(row);
            }
            false
        }
        Plan::Build { shrunk } => {
            if dry_run {
                row.note_status = "güncellenecek".into();
                row.note_updated = true;
                return Ok(row);
            }
            shrunk
        }
    };

    // Proje kaydı: katalog girdileri katalog kuralıyla, diğerleri yalnız yoksa eklenir.
    let project_id = if let Some((cname, cdesc)) = &item.catalog {
        db.upsert_catalog_projects(&[crate::projects::ProjectSeed {
            name: cname.clone(),
            path: item.path.clone(),
            description: cdesc.clone(),
        }])?;
        db.find_project_by_path(&item.path)?.map(|p| p.id)
    } else {
        let (project, _) = db.find_or_insert_project(&ProjectInfo {
            id: uuid::Uuid::new_v4().simple().to_string(),
            name: name.clone(),
            path: item.path.clone(),
            aliases: Vec::new(),
            description: None,
            last_active_at: None,
        })?;
        Some(project.id)
    };
    let Some(project_id) = project_id else {
        row.note_status = "proje kaydı oluşturulamadı".into();
        return Ok(row);
    };
    if matches!(decision, Plan::Skip(_)) {
        return Ok(row);
    }

    if *failures >= MAX_CONSECUTIVE_FAILURES {
        row.note_status = "atlandı (özetleyici art arda hata verdi)".into();
        return Ok(row);
    }
    let data = {
        let (h, it) = (home.to_path_buf(), item.clone());
        tokio::task::spawn_blocking(move || collect(&h, &it.path, &it.claude, &it.codex))
            .await
            .map_err(|e| anyhow!("kaynak toplama başarısız: {e}"))?
    };
    let prev: Option<PrevNote> = prev_note.as_ref().map(|n| parse_note(&n.markdown));
    match build_note(
        summarizer,
        &name,
        &data,
        NoteCtx {
            claude_sessions: claude,
            codex_sessions: codex,
            worker_sessions: workers,
            prev: prev.as_ref(),
            shrunk,
            now: Utc::now(),
        },
    )
    .await
    {
        Ok(note) => {
            *failures = 0;
            db.put_project_note(
                &project_id,
                &ProjectNote {
                    markdown: note.markdown,
                    updated_at: Utc::now().to_rfc3339(),
                    source_stamp: stamp,
                    claude_sessions: item.claude.len(),
                    codex_sessions: item.codex.len(),
                    summarizer: kind.to_string(),
                },
            )?;
            row.note_updated = true;
            row.note_status = "güncellendi".into();
        }
        Err(e) => {
            *failures += 1;
            // Hata metni güvenilmeyen içerik taşıyabilir: temizle, maskele, kısalt.
            row.note_status = format!("hata: {}", one_line(&format!("{e}"), 160));
        }
    }
    Ok(row)
}

/// Yalnız içe aktarıcının oluşturduğu sohbetleri siler. Silmeden önce `ultron.db` yanına
/// SQLite yedekleme API'siyle (özel ACL, bütünlük denetimli) yedek alır; `dry_run` yalnız
/// sayar, hiçbir şey yazmaz.
pub fn remove_imported_chats(db: &Db, db_path: &Path, dry_run: bool) -> Result<RemoveChatsReport> {
    let ids = db.imported_chat_ids()?;
    let mut events = 0;
    for id in &ids {
        events += db.events_for_chat(id, None)?.len();
    }
    let mut report = RemoveChatsReport {
        dry_run,
        chats: ids.len(),
        events,
        backup: None,
    };
    if dry_run || ids.is_empty() {
        return Ok(report);
    }
    let stamp = Utc::now().format("%Y%m%dT%H%M%S%3f");
    let uniq = &uuid::Uuid::new_v4().simple().to_string()[..8];
    let name = db_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "ultron.db".into());
    let backup = db_path.with_file_name(format!("{name}.backup-{stamp}-{uniq}"));
    db.backup_to(&backup)?;
    let (chats, events) = db.delete_imported_chats(&ids)?;
    report.chats = chats;
    report.events = events;
    report.backup = Some(backup.to_string_lossy().into_owned());
    Ok(report)
}
