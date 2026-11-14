mod backend;
mod creds;
mod headless;
#[cfg(unix)]
mod live;
mod mcp;
mod router;
mod tools;

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::OnceLock;
use std::task::{Context, Poll};

use clap::{Parser, ValueEnum};
use tokio::sync::Notify;

#[derive(Clone, Copy, ValueEnum)]
enum Mode {
    Headless,
    Live,
    /// Live and headless in one server; `connect` picks (see router.rs).
    Both,
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
    install_panic_hook();
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
            Mode::Both => "both",
        },
        "sccm-rc-mcp starting"
    );
    let backend: Box<dyn backend::Backend + Send> = match mode {
        Mode::Headless => Box::new(headless::Headless::new()),
        Mode::Live => live_backend(),
        Mode::Both => Box::new(router::Router::new(
            Box::new(headless::Headless::new()),
            live_backend(),
        )),
    };
    let mut server = mcp::Server::new(backend);
    // stdin EOF ends serve(), which shuts the backend down itself. On a
    // signal or a panic serve() is cancelled (or unwound), so shut down here
    // (idempotent): the till is released either way.
    let serve = CatchUnwind(Box::pin(server.serve(
        tokio::io::BufReader::new(tokio::io::stdin()),
        tokio::io::stdout(),
    )));
    let res = tokio::select! {
        r = serve => r.unwrap_or_else(|()| Err(anyhow::anyhow!("the server panicked"))),
        sig = shutdown_signal() => {
            tracing::info!(signal = sig, "shutting down");
            Ok(())
        }
        () = panicked().notified() => {
            tracing::error!("a task panicked; releasing the till and exiting");
            Err(anyhow::anyhow!("a task panicked"))
        }
    };
    server.shutdown().await;
    res
}

/// Signalled by the panic hook, so a panic on any thread (a spawned task,
/// the session thread) ends the server through the graceful shutdown.
fn panicked() -> &'static Notify {
    static PANICKED: OnceLock<Notify> = OnceLock::new();
    PANICKED.get_or_init(Notify::new)
}

/// Log the panic (location only: the payload may quote typed text), run the
/// default hook, then trigger the shutdown that releases the till.
fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let at = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_default();
        tracing::error!(location = %at, "panic");
        default(info);
        // A stored permit: wakes the select! even if it is not waiting yet.
        panicked().notify_one();
    }));
}

/// A future that turns a panic while polling into `Err(())`, so the caller
/// can still shut the backend down. Safe code only (`catch_unwind` per poll).
struct CatchUnwind<F>(Pin<Box<F>>);

impl<F: Future> Future for CatchUnwind<F> {
    type Output = Result<F::Output, ()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let inner = self.0.as_mut();
        match std::panic::catch_unwind(AssertUnwindSafe(|| inner.poll(cx))) {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(v)) => Poll::Ready(Ok(v)),
            Err(_) => Poll::Ready(Err(())),
        }
    }
}

/// SIGINT, SIGTERM, SIGHUP (the terminal was closed) or SIGQUIT: each ends
/// the server through the graceful shutdown that releases the till.
#[cfg(unix)]
async fn shutdown_signal() -> &'static str {
    use tokio::signal::unix::{signal, SignalKind};
    async fn on(kind: SignalKind, name: &'static str) -> &'static str {
        match signal(kind) {
            Ok(mut s) => {
                s.recv().await;
                name
            }
            Err(e) => {
                tracing::warn!(error = %e, signal = name, "cannot watch signal");
                std::future::pending().await
            }
        }
    }
    tokio::select! {
        n = on(SignalKind::interrupt(), "SIGINT") => n,
        n = on(SignalKind::terminate(), "SIGTERM") => n,
        n = on(SignalKind::hangup(), "SIGHUP") => n,
        n = on(SignalKind::quit(), "SIGQUIT") => n,
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() -> &'static str {
    let _ = tokio::signal::ctrl_c().await;
    "Ctrl+C"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn catch_unwind_turns_a_panic_into_err() {
        let ok = CatchUnwind(Box::pin(async { 7 })).await;
        assert_eq!(ok, Ok(7));
        // The panic still reaches the global hook (test output), not the caller.
        let r = CatchUnwind(Box::pin(async {
            tokio::task::yield_now().await;
            panic!("boom")
        }))
        .await;
        assert_eq!(r, Err::<(), ()>(()));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sighup_ends_the_server() {
        // Registered before raising (as at start-up); once registered, the
        // signal no longer terminates the test process.
        let wait = tokio::spawn(shutdown_signal());
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        std::process::Command::new("kill")
            .args(["-HUP", &std::process::id().to_string()])
            .status()
            .unwrap();
        let sig = tokio::time::timeout(std::time::Duration::from_secs(5), wait)
            .await
            .expect("signal seen")
            .unwrap();
        assert_eq!(sig, "SIGHUP");
    }
}
