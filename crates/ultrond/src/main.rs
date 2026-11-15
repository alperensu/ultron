//! `ultrond`: Ultron çekirdek servisi (kullanıcı oturumu bağlamında, named pipe üzerinden).

use std::fs::OpenOptions;
use std::sync::Mutex;

use anyhow::Result;
use ultron_core::{Config, Paths, ipc};
use ultrond::server::{self, ServerConfig};

#[tokio::main]
async fn main() -> Result<()> {
    let paths = Paths::resolve()?;
    paths.ensure()?;
    if let Ok(log) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.run.join("ultrond.log"))
    {
        tracing_subscriber::fmt()
            .with_writer(Mutex::new(log))
            .with_ansi(false)
            .init();
    }
    let cfg = Config::load(&paths)?;
    let engines = ultrond::build_engines(&cfg);
    let handle = server::start(ServerConfig {
        paths,
        pipe_name: ipc::pipe_name()?,
        cfg,
        engines,
    })
    .await?;
    tracing::info!(pipe = handle.pipe_name(), "ultrond hazır");
    tokio::signal::ctrl_c().await?;
    handle.shutdown().await;
    Ok(())
}
