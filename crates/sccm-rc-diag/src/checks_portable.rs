//! Non-Windows prerequisite checks.
//!
//! Only TCP/2701 reachability can be checked from a non-Windows host. The
//! other checks query the target through Win32 remote SCM / LSA / NetAPI calls
//! and are reported as skipped so the output keeps the same shape.

use crate::CheckResult;
use sccm_rc_protocol::transport::SCCM_RC_PORT;
use std::time::{Duration, Instant};
use tokio::net::TcpStream;

/// Check 1 — TCP/2701 reachability.
pub async fn tcp_reachable(target: &str) -> CheckResult {
    let start = Instant::now();
    let addr = format!("{target}:{SCCM_RC_PORT}");
    let connect = tokio::time::timeout(Duration::from_secs(3), TcpStream::connect(&addr)).await;
    let elapsed = start.elapsed();

    match connect {
        Ok(Ok(_)) => CheckResult::ok(
            "tcp_2701",
            format!("Target reachable at {addr} ({elapsed:?})"),
            elapsed,
        ),
        Ok(Err(e)) => CheckResult::blocker(
            "tcp_2701",
            format!("Cannot connect to {addr}: {e}"),
            "Causes (in order of likelihood):\n\
             1. Windows Firewall on target blocks TCP/2701\n\
             2. CmRcService on target is not running\n\
             3. Target is offline, the VPN/tunnel is down, or the host does not resolve",
            elapsed,
        ),
        Err(_) => CheckResult::blocker(
            "tcp_2701",
            format!("Connect to {addr} timed out after 3s"),
            "Likely a firewall silently dropping the SYN.",
            elapsed,
        ),
    }
}

fn skipped(name: &'static str) -> CheckResult {
    CheckResult::warning(
        name,
        "skipped: needs the Windows remote SCM/LSA/NetAPI APIs",
        "Run sccm-rc-diag on a Windows machine to check this prerequisite.",
        Duration::ZERO,
    )
}

/// Check 2 — `CmRcService` running state (Windows only).
pub async fn cmrcservice_state(_target: &str) -> CheckResult {
    skipped("cmrcservice")
}

/// Check 3 — "Access this computer from the network" right (Windows only).
pub async fn network_logon_right(_target: &str, _viewer_user: &str) -> CheckResult {
    skipped("network_logon_right")
}

/// Check 4 — Permitted Viewers group membership (Windows only).
pub async fn permitted_viewers(_target: &str, _viewer_user: &str) -> CheckResult {
    skipped("permitted_viewers")
}

/// Run all checks concurrently and return them in fixed order.
pub async fn run_all(target: &str, viewer_user: &str) -> Vec<CheckResult> {
    let (a, b, c, d) = tokio::join!(
        tcp_reachable(target),
        cmrcservice_state(target),
        network_logon_right(target, viewer_user),
        permitted_viewers(target, viewer_user),
    );
    vec![a, b, c, d]
}
