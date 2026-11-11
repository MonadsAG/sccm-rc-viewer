//! Non-Windows prerequisite checks.
//!
//! Only TCP/2701 reachability can be checked from a non-Windows host. The
//! other checks query the target through Win32 remote SCM / LSA / NetAPI calls
//! and are reported as skipped so the output keeps the same shape.

use crate::CheckResult;
use std::time::Duration;

/// Check 1 — TCP/2701 reachability (shared with the Windows build).
pub use crate::reach::tcp_reachable;

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
