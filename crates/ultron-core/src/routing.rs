//! Ağ ya da model çağrısı yapmadan, açık işaretlere göre proje seçimi.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::Result;

use crate::Db;
pub use crate::ipc::RouteResult;

fn fold(text: &str) -> String {
    text.chars()
        .flat_map(|c| match c {
            'I' => 'ı'.to_lowercase(),
            'İ' => 'i'.to_lowercase(),
            _ => c.to_lowercase(),
        })
        .collect()
}

fn word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn matches(text: &str, marker: &str) -> bool {
    !marker.is_empty()
        && text.match_indices(marker).any(|(start, matched)| {
            text[..start]
                .chars()
                .next_back()
                .is_none_or(|c| !word_char(c))
                && text[start + matched.len()..]
                    .chars()
                    .next()
                    .is_none_or(|c| !word_char(c))
        })
}

/// Farklı açık işaretlerin sayısı skordur; eşitlik proje kimliğiyle çözülür.
/// Okunamayan proje klasörleri ad/takma ad eşleşmesini engellemez.
pub fn route(db: &Db, text: &str, active_project: Option<&str>) -> Result<RouteResult> {
    let projects = db.list_projects()?;
    let text = fold(text);
    let mut scored = Vec::new();
    for project in &projects {
        let mut markers = BTreeSet::new();
        markers.insert(fold(project.name.trim()));
        markers.extend(project.aliases.iter().map(|alias| fold(alias.trim())));
        if let Some(folder) = project
            .path
            .trim_end_matches(['/', '\\'])
            .rsplit(['/', '\\'])
            .next()
        {
            markers.insert(fold(folder));
        }
        if let Ok(entries) = std::fs::read_dir(Path::new(&project.path)) {
            for entry in entries.flatten() {
                if let Some(name) = entry.file_name().to_str() {
                    markers.insert(fold(name));
                }
            }
        }
        let score = markers
            .iter()
            .filter(|marker| matches(&text, marker))
            .count();
        if score > 0 {
            scored.push((score, project.id.clone()));
        }
    }
    scored.sort_by(|(a_score, a_id), (b_score, b_id)| b_score.cmp(a_score).then(a_id.cmp(b_id)));
    if let Some((_, selected)) = scored.first() {
        return Ok(RouteResult {
            project_id: Some(selected.clone()),
            confidence: 0.9,
            reason: "Proje adı, takma adı veya dosya/klasör adı eşleşti".into(),
            alternatives: scored.iter().skip(1).map(|(_, id)| id.clone()).collect(),
        });
    }
    if let Some(active) = active_project.filter(|id| projects.iter().any(|p| p.id == *id)) {
        return Ok(RouteResult {
            project_id: Some(active.into()),
            confidence: 0.5,
            reason: "Son aktif proje".into(),
            alternatives: Vec::new(),
        });
    }
    Ok(RouteResult {
        project_id: None,
        confidence: 0.0,
        reason: "Eşleşen veya son aktif proje bulunamadı".into(),
        alternatives: Vec::new(),
    })
}
