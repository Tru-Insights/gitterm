use super::*;

fn git_command() -> std::process::Command {
    let mut command = std::process::Command::new("git");
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_COMMON_DIR",
        "GIT_PREFIX",
    ] {
        command.env_remove(key);
    }
    command
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1");
    command
}

fn git(dir: &Path, args: &[&str]) {
    let status = git_command()
        // A commit hook's author override would defeat the fixture's user.name.
        .env_remove("GIT_AUTHOR_NAME")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .current_dir(dir)
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed");
}

fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = git_command()
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run git");
    assert!(out.status.success(), "git {args:?} failed");
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn target(checkout: &Path, base: Option<&str>) -> ReviewTarget {
    ReviewTarget {
        checkout: checkout.to_path_buf(),
        repo: "portal".into(),
        branch: "work/prj-101".into(),
        owner: "coordinator".into(),
        base: base.map(|reference| BaseChoice {
            reference: reference.to_string(),
            source: crate::git_facts::BaseSource::Local,
            considered: 1,
        }),
    }
}

fn review_fixture() -> tempfile::TempDir {
    let temp = tempfile::Builder::new()
        .prefix("tru188-")
        .tempdir()
        .unwrap();
    git(temp.path(), &["init", "-q", "-b", "main"]);
    git(temp.path(), &["commit", "--allow-empty", "-qm", "base"]);
    git(temp.path(), &["checkout", "-qb", "feature"]);
    temp
}

async fn load_review_source(
    target: ReviewTarget,
    source: ReviewSource,
) -> Result<ReviewLoad, String> {
    let temp = tempfile::tempdir().unwrap();
    let roots =
        SnapshotRoots::new(temp.path().join("snapshots"), temp.path().join("temporary")).unwrap();
    super::load_review_source(target, source, &roots).await
}

#[tokio::test]
async fn regression_review_base_cannot_inject_git_output_options() {
    let temp = review_fixture();
    let dir = temp.path();
    let base = "--output=review".to_string();
    git(dir, &["update-ref", &format!("refs/heads/{base}"), "HEAD"]);
    git(dir, &["commit", "--allow-empty", "-qm", "change"]);
    let result = branch_commits(dir, &base).await;
    assert!(
        !dir.join("review..HEAD").exists(),
        "read-only log wrote a file"
    );
    assert_eq!(result.unwrap().0.len(), 1);
    load_review_source(target(dir, Some(&base)), ReviewSource::VsBase)
        .await
        .unwrap();
    assert!(!dir.join("review...HEAD").exists());
    let invalid = "--output=invalid".to_string();
    assert!(branch_commits(dir, &invalid).await.is_err());
    assert!(!dir.join("invalid..HEAD").exists());
    let secondary = other_source_paths(
        dir,
        DiffSource::Uncommitted,
        target(dir, Some(&base)).base.as_ref(),
    )
    .await;
    assert!(secondary.is_ok());
    assert!(!dir.join("review...HEAD").exists());
    git(dir, &["update-ref", "refs/heads/--all", "HEAD"]);
    let selected = git_out(dir, &["rev-parse", "HEAD"]);
    let parent = git_out(dir, &["rev-parse", "HEAD^"]);
    let endpoints = commit_diff_endpoints(
        dir,
        &CommitRange {
            first: "--all".into(),
            last: "--all".into(),
            pending: false,
        },
    )
    .await
    .unwrap();
    assert_eq!(endpoints, (parent, selected));
    for (first, last) in [(invalid.as_str(), "HEAD"), ("HEAD", invalid.as_str())] {
        assert!(commit_diff_endpoints(
            dir,
            &CommitRange {
                first: first.into(),
                last: last.into(),
                pending: false,
            }
        )
        .await
        .is_err());
    }
}

#[tokio::test]
async fn snapshot_input_is_lazy_and_follows_checkout_and_branch_validation() {
    let repo = review_fixture();
    let dir = repo.path();
    let never =
        || async { panic!("snapshot input polled for an early error or a non-turn source") };
    let absent = dir.join("missing");
    let turn = ReviewSource::Turns {
        chat: "fixture".into(),
        selected: Some(1),
    };
    let result =
        super::load_review_source_with_snapshots(target(&absent, None), turn.clone(), never())
            .await;
    assert!(result.unwrap_err().starts_with("cannot resolve "));
    for source in [
        ReviewSource::Uncommitted,
        ReviewSource::VsBase,
        ReviewSource::Commits(None),
    ] {
        super::load_review_source_with_snapshots(target(dir, Some("main")), source, never())
            .await
            .unwrap();
    }
    assert!(super::load_review_source_with_snapshots(
        target(dir, Some("missing-base")),
        ReviewSource::Commits(None),
        never(),
    )
    .await
    .is_err());
    let load = super::load_review_source_with_snapshots(
        target(dir, Some("missing-base")),
        ReviewSource::Turns {
            chat: "fixture".into(),
            selected: None,
        },
        async { Ok((Vec::new(), dir.to_path_buf())) },
    )
    .await
    .unwrap();
    assert!(load.commits_error.is_some());
    assert_eq!(
        super::load_review_source_with_snapshots(target(dir, None), turn, async {
            Err("snapshot read failed before pair validation".into())
        },)
        .await
        .unwrap_err(),
        "snapshot read failed before pair validation"
    );
}

fn tree_bytes(root: &Path) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, dir: &Path, out: &mut std::collections::BTreeMap<PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let key = path.strip_prefix(root).unwrap().to_path_buf();
            if path.is_dir() {
                out.insert(key, Vec::new());
                visit(root, &path, out);
            } else {
                out.insert(key, std::fs::read(path).unwrap());
            }
        }
    }
    let mut out = std::collections::BTreeMap::new();
    visit(root, root, &mut out);
    out
}

#[tokio::test]
async fn explicit_snapshot_roots_are_independent_and_collection_never_writes() {
    use crate::turn_snapshots::{save, snapshot, store_path};
    let repo = review_fixture();
    let dir = repo.path();
    let state = tempfile::tempdir().unwrap();
    let store_root = state.path().join("snapshots");
    std::fs::create_dir(state.path().join("capture-temporary")).unwrap();
    let roots =
        SnapshotRoots::new(store_root.clone(), state.path().join("capture-temporary")).unwrap();
    let start = snapshot(&roots, dir).await.unwrap().unwrap();
    save(&roots, dir, "fixture", 1, "start", &start)
        .await
        .unwrap();
    std::fs::write(dir.join("file.rs"), "turn content\n").unwrap();
    let end = snapshot(&roots, dir).await.unwrap().unwrap();
    save(&roots, dir, "fixture", 1, "end", &end).await.unwrap();
    drop((start, end));
    let temporary = state.path().join("never-created");
    let read_roots = SnapshotRoots::new(store_root, temporary.clone()).unwrap();
    let absent_root = state.path().join("other-root");
    let other_roots = SnapshotRoots::new(absent_root.clone(), temporary.clone()).unwrap();
    let before = tree_bytes(state.path());
    let source_before = tree_bytes(dir);
    let source = ReviewSource::Turns {
        chat: "fixture".into(),
        selected: Some(1),
    };
    let load = super::load_review_source(target(dir, None), source.clone(), &read_roots)
        .await
        .unwrap();
    assert_eq!(load.diff.files[0].path, "file.rs");
    assert!(load.live_paths.is_none());
    assert_eq!(
        super::load_review_source(target(dir, None), source, &other_roots)
            .await
            .unwrap_err(),
        "selected turn is no longer available"
    );
    let unselected = super::load_review_source(
        target(dir, None),
        ReviewSource::Turns {
            chat: "fixture".into(),
            selected: None,
        },
        &other_roots,
    )
    .await
    .unwrap();
    assert!(unselected.turns.is_empty() && unselected.diff.files.is_empty());
    assert!(!absent_root.exists() && !temporary.exists());
    let store = store_path(&read_roots, dir).await.unwrap();
    assert!(store.is_dir());
    assert_eq!(tree_bytes(state.path()), before);
    assert_eq!(tree_bytes(dir), source_before);
}
