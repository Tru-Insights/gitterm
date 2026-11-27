//! Compatibility facade for the shared prs service.

pub use gitterm_core::prs::*;

/// Polls stop between CLI calls; the current child retains its existing lifetime.
pub async fn poll(request: PollRequest) -> PollOutcome {
    gitterm_core::prs::with_poll_cancellation(
        crate::shutdown::periodic_cancelled,
        gitterm_core::prs::poll(request),
    )
    .await
}

pub async fn branch_prs(
    repo: &str,
    branch: &str,
    account: Option<String>,
) -> Result<Vec<BranchPr>, String> {
    gitterm_core::prs::with_poll_cancellation(
        crate::shutdown::periodic_cancelled,
        gitterm_core::prs::branch_prs(repo, branch, account),
    )
    .await
}

/// Admit only the atomic save; network polling never holds the quit barrier.
pub fn save(file: &WatchFile, path: &std::path::Path) -> Result<(), String> {
    let _write = crate::shutdown::periodic_write("PR watch atomic save")?;
    file.save(path)
}
