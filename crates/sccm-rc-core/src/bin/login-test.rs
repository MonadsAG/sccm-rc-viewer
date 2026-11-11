//! Minimal live logon check: connect, authenticate, read the access grant,
//! prove sealing works in both directions with one X.224 Connection Request,
//! then disconnect gracefully so the host is released.
//!
//! Sends no keyboard or mouse input and does not start the RDP desktop.
//! Makes exactly one logon attempt (no retries), so a wrong password cannot
//! lock the account.
//!
//! Credentials (non-Windows): `SCCM_USER` (`DOMAIN\user`) and `SCCM_PASS`.
//! Windows uses single sign-on.

use clap::Parser;
use sccm_rc_core::SccmSession;
use std::time::Duration;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "login-test", about = "One logon attempt + sealed round trip + clean disconnect")]
struct Cli {
    target: String,
    /// Seconds to wait for the whole bring-up, including a possible
    /// "allow remote control?" prompt on the target.
    #[arg(long, default_value_t = 60)]
    timeout: u64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,sccm_rc_core=debug")),
        )
        .init();
    let cli = Cli::parse();

    info!(target = %cli.target, "connecting + authenticating (single attempt)");
    let mut session = match tokio::time::timeout(
        Duration::from_secs(cli.timeout),
        SccmSession::connect(&cli.target),
    )
    .await
    {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            error!(error = %e, "logon failed");
            return Err(e.into());
        }
        Err(_) => anyhow::bail!("timed out after {}s", cli.timeout),
    };
    let (encrypted, verified, package) = session.security();
    info!(
        grant = ?session.grant(),
        encrypted,
        server_verified = verified,
        package = ?package,
        "✅ LOGON OK — access granted (server→client unseal works)"
    );

    // Client→server direction: a sealed X.224 Connection Request must come back
    // as a sealed X.224 Connection Confirm (TPKT 03 00 .. / CC code 0xD0).
    let x224_cr: &[u8] = &[
        0x03, 0x00, 0x00, 0x13, 0x0e, 0xe0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x08, 0x00,
        0x00, 0x00, 0x00, 0x00,
    ];
    session.send_rdp(x224_cr).await?;
    match tokio::time::timeout(Duration::from_secs(10), session.recv_rdp()).await {
        Ok(Ok(Some(reply))) => {
            let cc = reply.len() >= 6 && reply[0] == 0x03 && (reply[5] & 0xf0) == 0xd0;
            let head: Vec<String> = reply.iter().take(19).map(|b| format!("{b:02x}")).collect();
            if cc {
                info!(reply = %head.join(" "), "✅ X.224 Connection Confirm — client→server seal works");
            } else {
                error!(reply = %head.join(" "), "unexpected reply to X.224 Connection Request");
            }
        }
        Ok(Ok(None)) => error!("server closed the connection after the sealed request"),
        Ok(Err(e)) => error!(error = %e, "receiving the reply failed"),
        Err(_) => error!("no reply to the sealed X.224 request within 10s"),
    }

    session.disconnect().await;
    info!("disconnected");
    Ok(())
}
