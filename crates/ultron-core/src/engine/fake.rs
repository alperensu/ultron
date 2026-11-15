//! Testler için sahte motor; `ultron-fake-cli` yardımcı ikilisini çalıştırır.

use std::path::{Path, PathBuf};

use super::{Capabilities, CommandSpec, Engine, RestrictionLevel, TurnRequest};
use crate::config::EngineId;
use crate::events::EngineEvent;

const FAKE_BIN: &str = "ultron-fake-cli";

/// Sahte CLI'ı sabit argümanlarla çalıştıran motor. Her satır bir `EngineEvent` JSON'udur.
pub struct FakeEngine {
    program: String,
    base_args: Vec<String>,
    append_prompt: bool,
}

impl FakeEngine {
    /// `replay <fixture>`: fixture satırlarını aynen stdout'a yazar.
    pub fn from_fixture(path: &Path) -> FakeEngine {
        FakeEngine::with_mode(&["replay", &path.to_string_lossy()], false)
    }

    /// Verilen kip argümanlarıyla (ör. `["partial","2","137"]`) sahte CLI'ı çalıştırır.
    /// `append_prompt` doğruysa istem son argüman olarak eklenir.
    pub fn with_mode(args: &[&str], append_prompt: bool) -> FakeEngine {
        FakeEngine {
            program: locate_fake_cli().to_string_lossy().into_owned(),
            base_args: args.iter().map(|s| (*s).to_string()).collect(),
            append_prompt,
        }
    }
}

/// `ULTRON_FAKE_CLI` ya da çalışan ikilinin yanındaki/üstündeki dizinde arar.
fn locate_fake_cli() -> PathBuf {
    if let Some(p) = std::env::var_os("ULTRON_FAKE_CLI") {
        return PathBuf::from(p);
    }
    let name = format!("{FAKE_BIN}{}", std::env::consts::EXE_SUFFIX);
    if let Ok(exe) = std::env::current_exe() {
        for dir in exe.ancestors().skip(1).take(3) {
            let candidate = dir.join(&name);
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    PathBuf::from(name)
}

impl Engine for FakeEngine {
    fn id(&self) -> EngineId {
        EngineId::Claude
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            streaming: true,
            resume: false,
            mid_turn_input: false,
            tool_restriction: RestrictionLevel::Unknown,
        }
    }

    fn command(&self, req: &TurnRequest) -> CommandSpec {
        let mut args = self.base_args.clone();
        if self.append_prompt {
            args.push(req.prompt.clone());
        }
        CommandSpec {
            program: self.program.clone(),
            args,
            cwd: req.cwd.clone(),
            stdin: None,
        }
    }

    fn parse_line(&self, line: &str) -> Vec<EngineEvent> {
        match serde_json::from_str::<serde_json::Value>(line) {
            Ok(raw) => match serde_json::from_value::<EngineEvent>(raw.clone()) {
                Ok(ev) => vec![ev],
                Err(_) => vec![EngineEvent::Unknown { raw }],
            },
            Err(e) => vec![EngineEvent::Error {
                message: format!("geçersiz JSON satırı: {e}"),
            }],
        }
    }
}
