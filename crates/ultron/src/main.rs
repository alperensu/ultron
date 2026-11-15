use clap::Parser;
use ultron::commands::{Cli, run};

#[tokio::main]
async fn main() {
    if let Err(e) = run(Cli::parse()).await {
        eprintln!("hata: {e:#}");
        std::process::exit(1);
    }
}
