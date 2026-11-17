//! İçerik okumayan, sınırlı tur denetimi. Denetim önleyici bir sandbox değildir.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::events::EngineEvent;
use crate::import::mask_secrets;
use crate::policy::{Action, Decision, PolicyConfig, PolicyContext, RiskCategory, evaluate};
use crate::trash::TrashEntry;

const MAX_ENTRIES: usize = 50_000;
const MAX_VIOLATIONS: usize = 1024;

/// Koordinatörün olay/bildirim katmanına aktarılan maskeli denetim sonucu.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyViolation {
    pub command: String,
    pub category: RiskCategory,
}

/// Araç çıktısının Command ve ToolResult gibi birden fazla kopyasında sır bırakma.
pub(super) fn mask_event(event: EngineEvent) -> Result<EngineEvent> {
    fn mask(value: serde_json::Value) -> serde_json::Value {
        use serde_json::Value;
        match value {
            Value::String(text) => Value::String(mask_secrets(&text).0),
            Value::Array(items) => Value::Array(items.into_iter().map(mask).collect()),
            Value::Object(items) => Value::Object(
                items
                    .into_iter()
                    .map(|(key, value)| {
                        let probe = format!("{key}=ultron_mask_probe");
                        let value = if mask_secrets(&probe).0 != probe {
                            Value::String("[MASKED]".into())
                        } else {
                            mask(value)
                        };
                        (mask_secrets(&key).0, value)
                    })
                    .collect(),
            ),
            other => other,
        }
    }
    Ok(serde_json::from_value(mask(serde_json::to_value(event)?))?)
}

/// Ortak olay sözleşmesinin sahibine sunulan, tur sonuna ait denetim çıktısı.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditReport {
    pub chat_id: String,
    pub session_id: String,
    pub violations: Vec<PolicyViolation>,
    pub changes: Vec<(PathBuf, String)>,
    pub errors: Vec<String>,
}

pub(super) struct TurnAudit {
    root: PathBuf,
    paths: crate::Paths,
    before: Option<WorkspaceSnapshot>,
    trash_before: BTreeSet<String>,
    policy: PostTurnAudit,
    errors: Vec<String>,
}

impl TurnAudit {
    pub fn start(cwd: &Path, paths: crate::Paths, project_bound: bool) -> Result<Self> {
        let root = std::fs::canonicalize(cwd)?;
        let cfg = PolicyConfig::load(&paths)?;
        Self::start_with_budget(&root, paths, project_bound, cfg, MAX_ENTRIES)
    }

    fn start_with_budget(root: &Path, paths: crate::Paths, project_bound: bool, cfg: PolicyConfig, limit: usize) -> Result<Self> {
        let snapshot = WorkspaceSnapshot::capture_with_budget(root, limit);
        let (before, errors) = match snapshot {
            Ok(snapshot) => (Some(snapshot), Vec::new()),
            Err(error) if error.is::<SnapshotBudgetExceeded>() => (None, vec![error.to_string()]),
            Err(error) => return Err(error),
        };
        let trash_before = crate::trash::Trash::new(&paths).record_ids()?;
        let policy = PostTurnAudit::new(
            cfg,
            PolicyContext {
                project_root: project_bound.then(|| root.to_path_buf()),
                workspace_root: root.to_path_buf(),
            },
        );
        Ok(Self {
            root: root.to_path_buf(),
            paths,
            before,
            trash_before,
            policy,
            errors,
        })
    }

    pub fn observe(&mut self, event: &EngineEvent) {
        if let Err(error) = self.policy.observe(event)
            && self.errors.len() < 3
        {
            self.errors.push(mask_secrets(&error.to_string()).0);
        }
    }

    pub fn finish(mut self, chat_id: &str, session_id: &str) -> AuditReport {
        let changes = (|| -> Result<Vec<(PathBuf, &'static str)>> {
            match &self.before {
                Some(before) => Ok(before.changes(&WorkspaceSnapshot::capture(&self.root)?)),
                None => Ok(Vec::new()),
            }
        })();
        let trash = crate::trash::Trash::new(&self.paths).records_since(&self.trash_before);
        let mut file_changes = Vec::new();
        match (changes, trash) {
            (Ok(changes), Ok(trash)) => {
                for (path, change) in changes {
                    let absolute = self.root.join(&path);
                    let trashed = trashed_in_turn(&absolute, chat_id, &self.trash_before, &trash);
                    if let Err(error) =
                        self.policy
                            .check_file_change(&absolute, change == "delete", trashed)
                        && self.errors.len() < 3
                    {
                        self.errors.push(mask_secrets(&error.to_string()).0);
                    }
                    file_changes.push((path, change.into()));
                }
            }
            (changes, trash) => {
                for error in [changes.err(), trash.err()].into_iter().flatten() {
                    self.errors.push(mask_secrets(&error.to_string()).0);
                }
            }
        }
        AuditReport {
            chat_id: chat_id.into(),
            session_id: session_id.into(),
            violations: self.policy.finish(),
            changes: file_changes,
            errors: self.errors,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Stamp {
    size: u64,
    modified: SystemTime,
}

/// Yalnız dosya metaverisi tutulur; sır dosyaları dahil hiçbir içerik okunmaz.
pub struct WorkspaceSnapshot {
    files: BTreeMap<PathBuf, Stamp>,
}

#[derive(Debug)]
struct SnapshotBudgetExceeded;

impl std::fmt::Display for SnapshotBudgetExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Denetim sınırlı: çalışma alanı tarama bütçesi aşıldı")
    }
}

impl std::error::Error for SnapshotBudgetExceeded {}

impl WorkspaceSnapshot {
    pub fn capture(root: &Path) -> Result<Self> {
        Self::capture_with_budget(root, MAX_ENTRIES)
    }

    fn capture_with_budget(root: &Path, limit: usize) -> Result<Self> {
        ensure!(
            !is_link(&std::fs::symlink_metadata(root)?),
            "denetim kökü bağlantı olamaz"
        );
        let root = std::fs::canonicalize(root).context("denetim çalışma dizini çözülemedi")?;
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        let mut files = BTreeMap::new();
        let mut stack = vec![root.clone()];
        let mut seen = 0;
        while let Some(dir) = stack.pop() {
            ensure!(
                !is_link(&std::fs::symlink_metadata(&dir)?),
                "denetim dizini bağlantıya dönüştü"
            );
            for entry in std::fs::read_dir(&dir).context("denetim dizini okunamadı")? {
                seen += 1;
                if seen > limit || Instant::now() >= deadline {
                    return Err(SnapshotBudgetExceeded.into());
                }
                let entry = entry?;
                let path = entry.path();
                let meta = std::fs::symlink_metadata(&path)?;
                if is_link(&meta) {
                    continue;
                }
                if meta.is_dir() {
                    if !matches!(
                        entry.file_name().to_str(),
                        Some(".git" | "target" | "node_modules" | ".venv" | "venv" | "vendor" | "dist" | "build" | ".next" | "__pycache__")
                    ) {
                        stack.push(path);
                    }
                } else if meta.is_file() {
                    files.insert(
                        path.strip_prefix(&root)?.to_path_buf(),
                        Stamp {
                            size: meta.len(),
                            modified: meta.modified()?,
                        },
                    );
                }
            }
        }
        Ok(Self { files })
    }

    pub fn changes(&self, after: &Self) -> Vec<(PathBuf, &'static str)> {
        let mut changes = Vec::new();
        for (path, before) in &self.files {
            match after.files.get(path) {
                None => changes.push((path.clone(), "delete")),
                Some(stamp) if stamp != before => changes.push((path.clone(), "update")),
                _ => {}
            }
        }
        changes.extend(
            after
                .files
                .keys()
                .filter(|path| !self.files.contains_key(*path))
                .map(|path| (path.clone(), "add")),
        );
        changes
    }
}

fn is_link(meta: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        meta.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        meta.file_type().is_symlink()
    }
}

pub struct PostTurnAudit {
    cfg: PolicyConfig,
    ctx: PolicyContext,
    violations: Vec<PolicyViolation>,
}

impl PostTurnAudit {
    pub fn new(cfg: PolicyConfig, ctx: PolicyContext) -> Self {
        Self {
            cfg,
            ctx,
            violations: Vec::new(),
        }
    }

    /// Çıkış kodu olmayan teklif/engelleme bir gerçekleşme kanıtı sayılmaz.
    /// Başarısız komut da çalışmış ve kısmi yan etki bırakmış olabilir.
    pub fn observe(&mut self, event: &EngineEvent) -> Result<()> {
        if let EngineEvent::Command {
            command,
            exit_code: Some(_),
            ..
        } = event
        {
            let action = Action::Shell {
                command: command.clone(),
                cwd: self.ctx.workspace_root.clone(),
            };
            self.check(&action, command)?;
        }
        // Çağıran yalnız gerçekleşme kanıtı taşıyan ToolCall olaylarını aktarır:
        // Codex sürücüsü bunları tamamlanan MCP öğelerinden üretir.
        if let EngineEvent::ToolCall { tool, input } = event {
            self.check(&Action::ToolCall { tool: tool.clone(), input: input.clone() },
                &format!("ToolCall {tool} {input}"))?;
        }
        Ok(())
    }

    pub fn check_file_change(&mut self, path: &Path, deleted: bool, trashed: bool) -> Result<()> {
        if deleted && trashed {
            return Ok(());
        }
        let action = if deleted {
            Action::FileDelete {
                path: path.to_path_buf(),
            }
        } else {
            Action::FileWrite {
                path: path.to_path_buf(),
            }
        };
        self.check(
            &action,
            &format!(
                "{} {}",
                if deleted { "FileDelete" } else { "FileWrite" },
                path.display()
            ),
        )
    }

    fn check(&mut self, action: &Action, description: &str) -> Result<()> {
        let category = match evaluate(action, &self.cfg, &self.ctx) {
            Decision::Allow => return Ok(()),
            Decision::Ask { category, .. } => category,
            // Deny sözleşmesi kategori taşımaz; kesin retler sistem koruması olarak sunulur.
            Decision::Deny { .. } => RiskCategory::SystemConfig,
        };
        ensure!(
            self.violations.len() < MAX_VIOLATIONS,
            "tur denetimi ihlal sınırı aşıldı"
        );
        self.violations.push(PolicyViolation {
            command: crate::eventlog::excerpt(&mask_secrets(description).0, 4000),
            category,
        });
        Ok(())
    }

    pub fn finish(self) -> Vec<PolicyViolation> {
        self.violations
    }
}

/// Yalnız bu turda eklenmiş, aynı sohbete ait çöp kayıtları silmeyi açıklar.
pub fn trashed_in_turn(
    path: &Path,
    chat_id: &str,
    before: &BTreeSet<String>,
    after: &[TrashEntry],
) -> bool {
    after.iter().any(|entry| {
        !before.contains(&entry.id)
            && entry.chat_id.as_deref() == Some(chat_id)
            && (entry.original_path == path
                || (entry.is_dir && path.starts_with(&entry.original_path)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_budget_is_a_notice_and_command_audit_still_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::Paths::from_root(tmp.path().join("ultron"));
        paths.ensure().unwrap();
        let cwd = tmp.path().join("workspace");
        std::fs::create_dir(&cwd).unwrap();
        std::fs::write(cwd.join("file.txt"), "sentetik").unwrap();
        let mut audit = TurnAudit::start_with_budget(&cwd, paths, false, PolicyConfig::default(), 0).unwrap();
        assert!(audit.before.is_none());
        let report = TurnAudit::start_with_budget(&cwd, audit.paths.clone(), false, PolicyConfig::default(), 0)
            .unwrap().finish("c1", "s1");
        assert!(report.violations.is_empty());
        assert!(report.errors[0].starts_with("Denetim sınırlı:"));
        audit.observe(&EngineEvent::Command {
            command: "rm lost.txt".into(), exit_code: Some(1), output_excerpt: String::new(),
        });
        assert_eq!(audit.finish("c1", "s1").violations.len(), 1);
    }

    #[test]
    fn snapshot_skips_generated_and_dependency_trees() {
        let tmp = tempfile::tempdir().unwrap();
        for name in [".venv", "venv", "vendor", "dist", "build", ".next", "__pycache__"] {
            std::fs::create_dir(tmp.path().join(name)).unwrap();
            std::fs::write(tmp.path().join(name).join("ignored.txt"), "sentetik").unwrap();
        }
        assert!(WorkspaceSnapshot::capture_with_budget(tmp.path(), 7).unwrap().files.is_empty());
    }

    #[test]
    fn completed_mcp_actions_are_evaluated() {
        let mut audit = PostTurnAudit::new(PolicyConfig::default(), PolicyContext {
            project_root: None, workspace_root: PathBuf::from("workspace"),
        });
        audit.observe(&EngineEvent::ToolCall { tool: "files.delete_file".into(),
            input: serde_json::json!({"path": "lost.txt"}) }).unwrap();
        assert_eq!(audit.finish()[0].category, RiskCategory::Delete);
    }

    #[test]
    fn working_tree_diff_includes_preexisting_untracked_deletions() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("taslak.txt");
        std::fs::write(&path, "sentetik").unwrap();
        let before = WorkspaceSnapshot::capture(tmp.path()).unwrap();
        std::fs::remove_file(path).unwrap();
        std::fs::write(tmp.path().join("yeni.txt"), "yeni").unwrap();
        let after = WorkspaceSnapshot::capture(tmp.path()).unwrap();
        assert_eq!(
            before.changes(&after),
            vec![("taslak.txt".into(), "delete"), ("yeni.txt".into(), "add")]
        );
    }

    #[test]
    fn only_fresh_same_chat_trash_records_explain_deletions() {
        let entry = TrashEntry {
            id: "fresh".into(),
            original_path: PathBuf::from("workspace/dir"),
            deleted_at: chrono::Utc::now(),
            chat_id: Some("c1".into()),
            size: 1,
            is_dir: true,
        };
        let path = Path::new("workspace/dir/file.txt");
        assert!(trashed_in_turn(
            path,
            "c1",
            &BTreeSet::new(),
            std::slice::from_ref(&entry)
        ));
        assert!(!trashed_in_turn(
            path,
            "c2",
            &BTreeSet::new(),
            std::slice::from_ref(&entry)
        ));
        assert!(!trashed_in_turn(
            path,
            "c1",
            &BTreeSet::from(["fresh".into()]),
            &[entry]
        ));
    }

    #[test]
    fn turn_audit_distinguishes_trash_from_unrecoverable_deletion() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::Paths::from_root(tmp.path().join("ultron"));
        paths.ensure().unwrap();
        let cwd = tmp.path().join("workspace");
        std::fs::create_dir(&cwd).unwrap();
        for name in ["trash.txt", "lost.txt"] {
            std::fs::write(cwd.join(name), "sentetik").unwrap();
        }
        let audit = TurnAudit::start(&cwd, paths.clone(), true).unwrap();
        crate::trash::Trash::new(&paths)
            .put(&cwd.join("trash.txt"), Some("c1"))
            .unwrap();
        std::fs::remove_file(cwd.join("lost.txt")).unwrap();
        let report = audit.finish("c1", "s1");
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.changes.len(), 2);
        assert_eq!(report.violations.len(), 1);
        assert!(report.violations[0].command.contains("lost.txt"));
    }

    #[test]
    fn post_turn_scan_failure_is_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::Paths::from_root(tmp.path().join("ultron"));
        paths.ensure().unwrap();
        let cwd = tmp.path().join("workspace");
        std::fs::create_dir(&cwd).unwrap();
        let audit = TurnAudit::start(&cwd, paths, false).unwrap();
        std::fs::remove_dir(cwd).unwrap();
        assert!(!audit.finish("c1", "s1").errors.is_empty());
    }
}
