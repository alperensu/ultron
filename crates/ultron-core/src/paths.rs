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
    /// Varsayılan hafıza kasası. `from_root` altında `root/vault` (test güvenli); üretim
    /// çözümlemesinde (`ULTRON_HOME` yok) `~/harness-brain`. `[memory] vault_path` ayarı bunu ezer.
    pub default_vault: PathBuf,
}

impl Paths {
    /// Verilen kök dizinden yolları türetir (dizin oluşturmaz).
    pub fn from_root(root: PathBuf) -> Paths {
        Paths {
            db: root.join("state").join("ultron.db"),
            transcripts: root.join("transcripts"),
            fixtures_private: root.join("fixtures-private"),
            run: root.join("run"),
            default_vault: root.join("vault"),
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
        let mut paths = Paths::from_root(PathBuf::from(local).join("ultron"));
        // Yalnız üretim çözümlemesi kullanıcı profilindeki kasaya bağlanır; `ULTRON_HOME`
        // (test/izole kurulum) kullanıldığında gerçek profil kasasına asla dokunulmaz.
        if let Some(profile) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
            let profile = PathBuf::from(profile);
            if profile.is_absolute() {
                paths.default_vault = profile.join("harness-brain");
            }
        }
        Ok(paths)
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
