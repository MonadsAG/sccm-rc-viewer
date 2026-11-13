mod backend;
mod mcp;
mod tools;

use clap::{Parser, ValueEnum};

#[derive(Clone, Copy, ValueEnum)]
enum Mode {
    Headless,
    Live,
}

#[derive(Parser)]
#[command(
    version,
    about = "MCP server for operating a till via SCCM Remote Control"
)]
struct Args {
    #[arg(long, value_enum, default_value = "headless")]
    mode: Mode,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // stdout carries the protocol; logs go to stderr only.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();
    let args = Args::parse();
    tracing::info!(
        mode = match args.mode {
            Mode::Headless => "headless",
            Mode::Live => "live",
        },
        "sccm-rc-mcp starting"
    );
    // Placeholder for both modes until the real backends exist.
    let mut server = mcp::Server::new(Box::new(backend::Unimplemented));
    server
        .serve(
            tokio::io::BufReader::new(tokio::io::stdin()),
            tokio::io::stdout(),
        )
        .await
}
