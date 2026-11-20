//! Compatibility facade for the shared merge plan service.

pub use gitterm_core::merge_plan::*;

// The app facts scanner remains at its existing boundary.
pub use crate::chat_facts::is_gh_pr_create;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_gh_pr_create_in_chains_and_with_env() {
        assert!(is_gh_pr_create("gh pr create --draft --title x"));
        assert!(is_gh_pr_create(
            "git push -u origin HEAD && gh pr create --fill"
        ));
        assert!(is_gh_pr_create(
            "GH_TOKEN=\"$(gh auth token --user dev)\" gh pr create --base main"
        ));
        assert!(is_gh_pr_create("cd /tmp/x; /opt/homebrew/bin/gh pr create"));
        assert!(!is_gh_pr_create("gh pr view 12"));
        assert!(!is_gh_pr_create("gh pr list --state open"));
        assert!(!is_gh_pr_create("echo gh pr creates"));
    }
}
