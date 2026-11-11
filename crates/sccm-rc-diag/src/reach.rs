//! Check 1 — TCP/2701 reachability. Plain tokio, so it is shared by the
//! Windows checks and the non-Windows build.

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
             1. Windows Firewall on target blocks TCP/2701 → check `Get-NetFirewallRule -DisplayGroup 'Configuration Manager Remote Control'`\n\
             2. CmRcService on target is not running → see the cmrcservice check below\n\
             3. Target is offline, the VPN/tunnel is down, or the hostname does not resolve",
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
