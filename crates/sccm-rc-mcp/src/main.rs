mod backend;
mod creds;
mod headless;
#[cfg(unix)]
mod live;
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

/// The viewer's proven session feature set (graphics handshake, drawing
/// orders, session arbitration, compression), unless set in the environment.
/// Not set: SCCM_RC_TAKEOVER (the headless backend passes take-over
/// explicitly, only with force), SCCM_RC_CLIP (no clipboard sync between
/// this Mac and the till) and SCCM_RC_CURTAIN (no privacy screen).
const SESSION_ENV: [(&str, &str); 6] = [
    ("SCCM_RC_ARB", "1"),
    ("SCCM_RC_WLC", "1"),
    ("SCCM_RC_MSTSC_CAPS", "1"),
    ("SCCM_RC_ORDERS", "1"),
    ("SCCM_RC_ARB_EVENT", "1"),
    ("SCCM_RC_COMPRESS", "1"),
];

fn main() -> anyhow::Result<()> {
    // stdout carries the protocol; logs go to stderr only.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();
    let args = Args::parse();
    // Before any other thread exists, so setting the environment is race-free.
    for (k, v) in SESSION_ENV {
        if std::env::var_os(k).is_none() {
            std::env::set_var(k, v);
        }
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let res = rt.block_on(run(args.mode));
    // run() has already shut the backend down (till released or the 3 s
    // wait over). Exit now: dropping the runtime would wait for tokio's
    // blocking stdin read, which never ends while stdin stays open (signal).
    let code = match &res {
        Ok(()) => 0,
        Err(e) => {
            tracing::error!(error = %e, "sccm-rc-mcp failed");
            1
        }
    };
    std::process::exit(code)
}

#[cfg(unix)]
fn live_backend() -> Box<dyn backend::Backend + Send> {
    Box::new(live::Live::new())
}

#[cfg(not(unix))]
fn live_backend() -> Box<dyn backend::Backend + Send> {
    Box::new(backend::Unavailable)
}

async fn run(mode: Mode) -> anyhow::Result<()> {
    tracing::info!(
        mode = match mode {
            Mode::Headless => "headless",
            Mode::Live => "live",
        },
        "sccm-rc-mcp starting"
    );
    let backend: Box<dyn backend::Backend + Send> = match mode {
        Mode::Headless => Box::new(headless::Headless::new()),
        Mode::Live => live_backend(),
    };
    let mut server = mcp::Server::new(backend);
    // stdin EOF ends serve(), which shuts the backend down itself. On a
    // signal serve() is cancelled, so shut down here (idempotent).
    let res = tokio::select! {
        r = server.serve(
            tokio::io::BufReader::new(tokio::io::stdin()),
            tokio::io::stdout(),
        ) => r,
        sig = shutdown_signal() => {
            tracing::info!(signal = sig, "shutting down");
            Ok(())
        }
    };
    server.shutdown().await;
    res
}

#[cfg(unix)]
async fn shutdown_signal() -> &'static str {
    use tokio::signal::unix::{signal, SignalKind};
    match signal(SignalKind::terminate()) {
        Ok(mut term) => tokio::select! {
            _ = tokio::signal::ctrl_c() => "SIGINT",
            _ = term.recv() => "SIGTERM",
        },
        Err(e) => {
            tracing::warn!(error = %e, "cannot watch SIGTERM");
            let _ = tokio::signal::ctrl_c().await;
            "SIGINT"
        }
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() -> &'static str {
    let _ = tokio::signal::ctrl_c().await;
    "Ctrl+C"
}
