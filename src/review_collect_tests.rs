// Characterized against the desktop collector before the core extraction.
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

async fn collect_checked(target: ReviewTarget, source: ReviewSource) -> Result<ReviewLoad, String> {
    let temp = tempfile::tempdir().unwrap();
    let root = crate::turn_snapshots::store_path(&target.checkout)
        .await
        .map(|store| store.parent().unwrap().to_path_buf())
        .unwrap_or_else(|_| temp.path().join("absent"));
    let temporary = temp.path().join("never-created");
    let roots = gitterm_core::turn_snapshots::SnapshotRoots::new(root, temporary.clone()).unwrap();
    let desktop = load_review_source(target.clone(), source.clone()).await;
    let core = gitterm_core::review::collect::load_review_source(target, source, &roots).await;
    assert_eq!(format!("{desktop:?}"), format!("{core:?}"));
    assert!(!temporary.exists());
    desktop
}

#[tokio::test]
async fn collection_absent_and_malformed_stores_preserve_errors_and_bytes() {
    let repo = review_fixture();
    let dir = repo.path();
    let store = crate::turn_snapshots::store_path(dir).await.unwrap();
    let source = |selected| ReviewSource::Turns {
        chat: "fixture".into(),
        selected,
    };
    let before = tree_bytes(dir);
    let empty = collect_checked(target(dir, None), source(None))
        .await
        .unwrap();
    assert!(empty.turns.is_empty() && empty.diff.files.is_empty());
    assert!(empty.live_paths.is_none());
    assert!(
        !store.exists(),
        "reading an absent store must not initialize it"
    );
    assert_eq!(
        collect_checked(target(dir, None), source(Some(1)))
            .await
            .unwrap_err(),
        "selected turn is no longer available"
    );
    assert!(!store.exists());
    assert!(collect_checked(
        target(dir, None),
        ReviewSource::Turns {
            chat: "../invalid".into(),
            selected: None,
        }
    )
    .await
    .unwrap_err()
    .contains("invalid"));
    assert_eq!(tree_bytes(dir), before);

    // A malformed store cannot affect non-turn collection or take precedence
    // over resolving the checkout. No attempt may repair it.
    std::fs::create_dir_all(&store).unwrap();
    std::fs::write(store.join("sentinel"), "malformed snapshot store").unwrap();
    let stored = tree_bytes(&store);
    assert!(collect_checked(target(dir, None), source(None))
        .await
        .is_err());
    assert!(
        collect_checked(target(dir, None), ReviewSource::Uncommitted)
            .await
            .is_ok()
    );
    let missing = dir.join("missing");
    assert!(collect_checked(target(&missing, None), source(Some(1)))
        .await
        .unwrap_err()
        .starts_with("cannot resolve "));
    let mut invalid_base = target(dir, Some("does-not-exist"));
    let load = collect_checked(invalid_base.clone(), ReviewSource::Uncommitted)
        .await
        .unwrap();
    assert!(load.commits_error.is_some());
    assert!(
        load.live_paths.is_none(),
        "failed secondary listing must not prune marks"
    );
    assert!(
        collect_checked(invalid_base.clone(), ReviewSource::Commits(None))
            .await
            .is_err()
    );
    invalid_base.checkout = missing;
    assert!(collect_checked(invalid_base, ReviewSource::Commits(None))
        .await
        .unwrap_err()
        .starts_with("cannot resolve "));
    assert_eq!(tree_bytes(&store), stored);
    assert_eq!(tree_bytes(dir), before);
}

#[tokio::test]
async fn collection_turns_are_read_only_and_bound_to_chat_and_checkout() {
    let repo = review_fixture();
    let dir = repo.path();
    std::fs::write(dir.join("file.rs"), "before\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-qm", "tracked"]);
    let start = crate::turn_snapshots::snapshot(dir).await.unwrap().unwrap();
    crate::turn_snapshots::save(dir, "fixture", 1, "start", &start)
        .await
        .unwrap();
    std::fs::write(dir.join("file.rs"), "after\n").unwrap();
    std::fs::write(dir.join("untracked.txt"), "new\n").unwrap();
    let end = crate::turn_snapshots::snapshot(dir).await.unwrap().unwrap();
    crate::turn_snapshots::save(dir, "fixture", 1, "end", &end)
        .await
        .unwrap();
    crate::turn_snapshots::save(dir, "fixture", 2, "start", &end)
        .await
        .unwrap();
    crate::turn_snapshots::save(dir, "fixture", 3, "end", &end)
        .await
        .unwrap();
    let worktree_parent = tempfile::tempdir().unwrap();
    let worktree = worktree_parent.path().join("linked");
    git(
        dir,
        &[
            "worktree",
            "add",
            "-qb",
            "linked",
            worktree.to_str().unwrap(),
        ],
    );
    let store = crate::turn_snapshots::store_path(dir).await.unwrap();
    assert_eq!(
        crate::turn_snapshots::store_path(&worktree).await.unwrap(),
        store
    );
    std::fs::write(dir.join(VIEWED_FILE), "viewed bytes stay unchanged").unwrap();
    let before = tree_bytes(dir);
    let stored = tree_bytes(&store);
    let linked = tree_bytes(&worktree);
    for (selected, error) in [
        (2, "turn has no end snapshot (running or incomplete)"),
        (
            3,
            "turn has no start snapshot (checkout discovered late or skipped)",
        ),
        (4, "selected turn is no longer available"),
    ] {
        assert_eq!(
            collect_checked(
                target(dir, None),
                ReviewSource::Turns {
                    chat: "fixture".into(),
                    selected: Some(selected),
                }
            )
            .await
            .unwrap_err(),
            error
        );
    }
    let load = collect_checked(
        target(dir, None),
        ReviewSource::Turns {
            chat: "fixture".into(),
            selected: Some(1),
        },
    )
    .await
    .unwrap();
    assert_eq!(load.turns.len(), 3);
    assert!(load.diff.file("untracked.txt").is_some());
    assert_eq!(
        load.diff.file("file.rs").unwrap().content_id.as_deref(),
        Some(git_out(&store, &["rev-parse", &format!("{end}:file.rs")]).as_str())
    );
    assert!(load.live_paths.is_none());
    for (checkout, chat) in [(dir, "other-chat"), (worktree.as_path(), "fixture")] {
        let source = ReviewSource::Turns {
            chat: chat.into(),
            selected: Some(1),
        };
        assert_eq!(
            collect_checked(target(checkout, None), source)
                .await
                .unwrap_err(),
            "selected turn is no longer available"
        );
    }
    assert_eq!(tree_bytes(dir), before);
    assert_eq!(tree_bytes(&store), stored);
    assert_eq!(tree_bytes(&worktree), linked);
}

#[tokio::test]
async fn collection_file_scan_and_combined_admission_limits() {
    let repo = review_fixture();
    let dir = repo.path();
    let tracked_count = ADMIT_LINE_BUDGET / FILE_LINE_CAP;
    for index in 0..tracked_count {
        let path = format!("a{index:02}.rs");
        std::fs::write(dir.join(&path), "line\n".repeat(FILE_LINE_CAP + 1)).unwrap();
        git(dir, &["add", &path]);
    }
    for index in 0..UNTRACKED_SCAN_CAP + 1 {
        std::fs::write(dir.join(format!("z{index:05}.rs")), "").unwrap();
    }
    let before = std::fs::read(dir.join(".git/index")).unwrap();
    let load = collect_checked(target(dir, None), ReviewSource::Uncommitted)
        .await
        .unwrap();
    assert_eq!(load.diff.files.len(), FILE_LIST_CAP);
    assert_eq!(
        load.diff.unlisted,
        UNTRACKED_SCAN_CAP + tracked_count - FILE_LIST_CAP
    );
    let tracked = load.diff.file("a00.rs").unwrap();
    assert!(tracked.truncated && !tracked.omitted);
    assert_eq!(tracked.additions, FILE_LINE_CAP + 1);
    assert_eq!(tracked.shown_lines(), FILE_LINE_CAP);
    assert!(load
        .diff
        .files
        .iter()
        .skip(tracked_count)
        .all(|f| f.omitted));
    assert_eq!(
        load.diff.totals.all().additions,
        tracked_count * (FILE_LINE_CAP + 1)
    );
    assert!(load.live_paths.is_none());
    assert_eq!(std::fs::read(dir.join(".git/index")).unwrap(), before);
}
