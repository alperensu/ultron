use std::path::PathBuf;

use anyhow::{Context, Result};

/// Ultron durum kökü ve alt dizinleri.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub root: PathBuf,
    pub db: PathBuf,
    pub transcripts: PathBuf,
    pub fixtures_private: PathBuf,
    pub run: PathBuf,
}

impl Paths {
    /// Verilen kök dizinden yolları türetir (dizin oluşturmaz).
    pub fn from_root(root: PathBuf) -> Paths {
        Paths {
            db: root.join("state").join("ultron.db"),
            transcripts: root.join("transcripts"),
            fixtures_private: root.join("fixtures-private"),
            run: root.join("run"),
            root,
        }
    }

    /// `ULTRON_HOME` varsa onu, yoksa `%LOCALAPPDATA%\ultron` kullanır.
    pub fn resolve() -> Result<Paths> {
        if let Some(home) = std::env::var_os("ULTRON_HOME") {
            return Ok(Paths::from_root(PathBuf::from(home)));
        }
        let local = std::env::var_os("LOCALAPPDATA")
            .context("LOCALAPPDATA tanımlı değil ve ULTRON_HOME yok")?;
        Ok(Paths::from_root(PathBuf::from(local).join("ultron")))
    }

    /// Gerekli dizinleri oluşturur.
    pub fn ensure(&self) -> Result<()> {
        for dir in [
            self.root.clone(),
            self.root.join("state"),
            self.transcripts.clone(),
            self.fixtures_private.clone(),
            self.run.clone(),
        ] {
            std::fs::create_dir_all(&dir)
                .with_context(|| format!("dizin oluşturulamadı: {}", dir.display()))?;
        }
        Ok(())
    }
}
