//! Root worker facade; ambient environment discovery remains in the host.
pub use gitterm_core::workers::*;

pub fn validate_codex_trust_home(
    home: &std::path::Path,
    extra_env: &[(String, String)],
) -> Result<(), String> {
    gitterm_core::workers::validate_codex_trust_home(home, extra_env, |key| std::env::var(key).ok())
}

pub fn validate_claude_trust_launch(
    command: &str,
    home: &std::path::Path,
    extra_env: &[(String, String)],
) -> Result<(), String> {
    gitterm_core::workers::validate_claude_trust_launch(command, home, extra_env, |key| {
        std::env::var(key).ok()
    })
}

#[cfg(test)]
mod pty_tests {
    use super::*;
    use std::path::{Path, PathBuf};
    fn repo(dir: &Path) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        git2::Repository::init(dir).unwrap();
        std::fs::canonicalize(dir).unwrap()
    }
    fn linked_repo(temp: &Path) -> (PathBuf, PathBuf) {
        let root = repo(&temp.join("main.repo space ' $quote"));
        let repository = git2::Repository::open(&root).unwrap();
        let tree = repository
            .find_tree(repository.index().unwrap().write_tree().unwrap())
            .unwrap();
        let signature = git2::Signature::now("Fixture", "fixture@example.invalid").unwrap();
        repository
            .commit(Some("HEAD"), &signature, &signature, "fixture", &tree, &[])
            .unwrap();
        let linked = temp.join("linked worker");
        repository.worktree("worker", &linked, None).unwrap();
        (root, linked)
    }
    #[cfg(unix)]
    #[test]
    #[ignore = "requires Claude Code 2.1.318 and python3; isolated PTY, synthetic auth, no model request"]
    fn claude_worker_trust_pty_smoke() {
        let temp = tempfile::tempdir().unwrap();
        let (root, linked) = linked_repo(temp.path());
        let isolated = temp.path().join("isolated-config");
        std::fs::create_dir(&isolated).unwrap();
        let config = isolated.join(".claude.json");
        for source in ["config-dir", "home"] {
            let fixture = serde_json::json!({
                "hasCompletedOnboarding":true,"lastOnboardingVersion":"2.1.318","theme":"dark",
                "customApiKeyResponses":{"approved":["gitterm-v6-fixture-not-a-real-key".chars().rev().take(20).collect::<Vec<_>>().into_iter().rev().collect::<String>()],"rejected":[]},
                "projects":{root.to_str().unwrap():{"hasTrustDialogAccepted":false}}
            });
            std::fs::write(&config, serde_json::to_vec(&fixture).unwrap()).unwrap();
            let probe = |trusted| {
                let status = std::process::Command::new("python3")
                    .arg(concat!(
                        env!("CARGO_MANIFEST_DIR"),
                        "/tests/claude_worker_trust_pty.py"
                    ))
                    .arg(&linked)
                    .arg(&config)
                    .arg(trusted)
                    .arg(source)
                    .status()
                    .unwrap();
                assert!(status.success(), "isolated Claude PTY smoke failed");
            };
            probe("false");
            assert_eq!(
                claude_task_trust(&linked, std::slice::from_ref(&root), &config).unwrap(),
                Some(root.clone())
            );
            probe("true");
        }
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "requires codex-cli 0.164.0, python3 and local Codex auth; isolated PTY, no model request"]
    fn codex_worker_trust_pty_smoke() {
        let temp = tempfile::tempdir().unwrap();
        let root = repo(&temp.path().join("sample.repo space \"quote\" ' $value"));
        let value = codex_trust_value(&root).unwrap();
        let command = with_codex_trust("codex", &value).unwrap();
        let status = std::process::Command::new("python3")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/codex_worker_trust_pty.py"
            ))
            .arg(&root)
            .arg(command)
            .status()
            .unwrap();
        assert!(status.success(), "isolated Codex PTY smoke failed");
    }
}
