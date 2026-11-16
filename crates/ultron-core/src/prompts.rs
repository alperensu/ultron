//! Ultron talimatları: sabit katmanlar, yerel geçersiz kılmalar ve motor enjeksiyonu.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};

use crate::config::EngineId;
use crate::engine::{Capabilities, CommandSpec, Engine, TurnRequest, agy, claude, codex};
use crate::events::EngineEvent;
use crate::paths::Paths;

const CORE: &str = include_str!("../../../prompts/core.md");
const BRAIN: &str = include_str!("../../../prompts/role-brain.md");
const WORKER: &str = include_str!("../../../prompts/role-worker.md");
const DELEGATE: &str = include_str!("../../../prompts/role-delegate.md");
const GENERAL: &str = include_str!("../../../prompts/mode-general.md");
const REVIEWER: &str = "## ROL: İNCELEYİCİ\n\nBrief kapsamını salt okunur incele; dosya değiştirme ve alt ajan açma. Somut bulguları önem derecesi, konum, etki ve kanıtla bildir. İncelemediğin alanları belirt.";

/// Bir turdaki görev rolü.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Brain,
    Worker,
    Reviewer,
    Delegate,
}

/// Kod işi ya da genel asistan kullanımı.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkMode {
    Code,
    General,
}

/// Sabit çekirdek ve rol önce, değişken proje bağlamı en sonda gelir.
/// Yerel kartlar `ULTRON_HOME/prompts` (yoksa varsayılan Ultron kökü) altından okunur.
pub fn compose(role: Role, mode: WorkMode, project_files: &[PathBuf]) -> String {
    let home = Paths::resolve().ok().map(|paths| paths.root);
    compose_from_home(home.as_deref(), role, mode, project_files)
}

/// Açık durum kökü ile aynı bileşim; süreç ortamını değiştirmeden kullanılabilir.
pub fn compose_from_home(
    home: Option<&Path>,
    role: Role,
    mode: WorkMode,
    project_files: &[PathBuf],
) -> String {
    let card = |name: &str, embedded: &str| {
        home.and_then(|home| std::fs::read_to_string(home.join("prompts").join(name)).ok())
            .unwrap_or_else(|| embedded.to_string())
    };
    let mut sections = vec![card("core.md", CORE)];
    sections.push(match role {
        Role::Brain => card("role-brain.md", BRAIN),
        Role::Worker => card("role-worker.md", WORKER),
        Role::Reviewer => card("role-reviewer.md", REVIEWER),
        Role::Delegate => card("role-delegate.md", DELEGATE),
    });
    if mode == WorkMode::General {
        sections.push(card("mode-general.md", GENERAL));
    }
    for path in project_files {
        if let Ok(text) = std::fs::read_to_string(path) {
            // Mutlak kullanıcı yollarını oluşturulan karta taşımadan proje bağlamını ekle.
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            sections.push(format!("## PROJE BAĞLAMI: {name}\n\n{text}"));
        }
    }
    sections.join("\n\n")
}

/// Yalnız projeye bağlı ve çalışma dizininde kod bulunan sohbet kod moduna girer.
pub fn work_mode(project_bound: bool, cwd: &Path) -> WorkMode {
    if project_bound && has_code(cwd) {
        WorkMode::Code
    } else {
        WorkMode::General
    }
}

fn has_code(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.filter_map(Result::ok).any(|entry| {
        let Ok(kind) = entry.file_type() else {
            return false;
        };
        let path = entry.path();
        if kind.is_file() {
            return path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| {
                    matches!(
                        ext.to_ascii_lowercase().as_str(),
                        "rs" | "js"
                            | "jsx"
                            | "ts"
                            | "tsx"
                            | "py"
                            | "go"
                            | "java"
                            | "kt"
                            | "kts"
                            | "cs"
                            | "fs"
                            | "vb"
                            | "cpp"
                            | "c"
                            | "h"
                            | "hpp"
                            | "swift"
                            | "rb"
                            | "php"
                            | "lua"
                            | "luau"
                            | "vue"
                            | "svelte"
                            | "html"
                            | "css"
                            | "scss"
                            | "sh"
                            | "ps1"
                            | "sql"
                    )
                });
        }
        kind.is_dir()
            && !matches!(
                entry.file_name().to_str(),
                Some(
                    ".git"
                        | "node_modules"
                        | "target"
                        | "dist"
                        | "build"
                        | "vendor"
                        | ".venv"
                        | "venv"
                )
            )
            && has_code(&path)
    })
}

/// Motor sözleşmesini değiştirmeden tur talimatlarını ekler. Claude kartı süreçten
/// önce yazılır; yazma hatası motoru talimatsız çalıştırmak yerine çağırana döner.
pub fn with_instructions(
    inner: Arc<dyn Engine>,
    instructions: &str,
    run_dir: &Path,
    chat_id: &str,
) -> Result<Arc<dyn Engine>> {
    if chat_id.is_empty()
        || !chat_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("geçersiz sohbet kimliği");
    }
    let prompt_file = std::path::absolute(run_dir)
        .context("talimat çalışma dizini çözümlenemedi")?
        .join(format!("prompt-{chat_id}.md"));
    if inner.id() == EngineId::Agy && !agy::instructions_fit(instructions) {
        bail!("agy talimatları komut satırı sınırını aşıyor");
    }
    if inner.id() == EngineId::Claude {
        std::fs::create_dir_all(run_dir).context("talimat çalışma dizini oluşturulamadı")?;
        std::fs::write(&prompt_file, instructions).context("Claude talimat dosyası yazılamadı")?;
    }
    Ok(Arc::new(InstructionEngine {
        inner,
        instructions: instructions.to_string(),
        prompt_file,
    }))
}

struct InstructionEngine {
    inner: Arc<dyn Engine>,
    instructions: String,
    prompt_file: PathBuf,
}

impl Engine for InstructionEngine {
    fn id(&self) -> EngineId {
        self.inner.id()
    }

    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    fn command(&self, req: &TurnRequest) -> CommandSpec {
        let mut spec = self.inner.command(req);
        match self.id() {
            EngineId::Claude => claude::append_prompt_file(&mut spec, &self.prompt_file),
            EngineId::Codex => codex::prepend_instructions(&mut spec, &self.instructions),
            EngineId::Agy => agy::prepend_instructions(&mut spec, &self.instructions),
        }
        spec
    }

    fn parse_line(&self, line: &str) -> Vec<EngineEvent> {
        self.inner.parse_line(line)
    }
}

pub(crate) fn prefix(instructions: &str, prompt: &str) -> String {
    format!("[ULTRON TALİMATLARI]\n{instructions}\n[/ULTRON TALİMATLARI]\n\n{prompt}")
}
