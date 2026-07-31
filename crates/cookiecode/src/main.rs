use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "cookiecode")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    Daemon,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    match Cli::parse().command {
        Some(Command::Daemon) => cookiecode_server::daemon().await,
        None => cookiecode_tui::run(),
    }
}
