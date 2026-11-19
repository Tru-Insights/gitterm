#[test]
fn failed_checkout_rolls_back_the_created_branch_and_worktree() {
    let repository = TestRepository::new();
    let base = repository.commit_file(
        ".gitattributes",
        "README.md filter=gitterm-fail-checkout\n",
        "Require checkout filter",
    );
    git_ok(
        &repository.path,
        &["config", "filter.gitterm-fail-checkout.clean", "cat"],
    );
    git_ok(
        &repository.path,
        &["config", "filter.gitterm-fail-checkout.smudge", "false"],
    );
    git_ok(
        &repository.path,
        &["config", "filter.gitterm-fail-checkout.required", "true"],
    );
    let request = repository.request("checkout-failure", None, "Failed checkout", "main");
    let proposal = resolve_task_preparation_blocking(&request)
        .unwrap()
        .proposal;

    let error = prepare_task_worktree_blocking(&request).unwrap_err();

    assert_eq!(error.operation(), "run git worktree add");
    assert!(error.to_string().contains("smudge filter"));
    assert!(!error.to_string().contains("rollback requires attention"));
    assert!(!proposal.path.exists());
    assert!(!registered_worktrees(&repository.path)
        .unwrap()
        .iter()
        .any(|path| paths_equal(path, &proposal.path)));
    assert!(!git_output(
        &repository.path,
        &[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{}", proposal.branch)
        ]
    )
    .status
    .success());
    assert_eq!(git_stdout(&repository.path, &["rev-parse", "HEAD"]), base);
    assert_eq!(
        fs::read_to_string(repository.path.join("README.md")).unwrap(),
        "initial\n"
    );
    assert!(git_stdout(&repository.path, &["status", "--porcelain"]).is_empty());
}

#[test]
fn rollback_preserves_changes_created_after_checkout() {
    let repository = TestRepository::new();
    let request = repository.request("changed-rollback", None, "Preserve changes", "main");
    let proposal = resolve_task_preparation_blocking(&request)
        .unwrap()
        .proposal;

    let error = prepare_task_worktree_with(&request, || {
        fs::write(proposal.path.join("keep.txt"), "keep my work\n").unwrap();
        Err("injected verification failure".to_string())
    })
    .unwrap_err();

    assert!(error.to_string().contains("rollback requires attention"));
    assert_eq!(
        fs::read_to_string(proposal.path.join("keep.txt")).unwrap(),
        "keep my work\n"
    );
    assert!(registered_worktrees(&repository.path)
        .unwrap()
        .iter()
        .any(|path| paths_equal(path, &proposal.path)));
    assert_eq!(
        git_stdout(&repository.path, &["rev-parse", &proposal.branch]),
        repository.initial_commit
    );
}

#[test]
fn removal_refuses_unregistered_foreign_and_main_checkout_paths_even_with_force() {
    let repository = TestRepository::new();
    let foreign = TestRepository::new();
    let foreign_request = foreign.request("foreign", None, "Foreign worktree", "main");
    let foreign_tree = prepare_task_worktree_blocking(&foreign_request).unwrap();
    let outside = repository._temp.path().join("unregistered");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("keep.txt"), "keep\n").unwrap();
    let missing = repository._temp.path().join("missing");

    for path in [
        &outside,
        &missing,
        &foreign_tree.path,
        &repository.path,
        &repository.path.join(".git"),
    ] {
        for force in [false, true] {
            let error = remove_task_worktree_blocking(&repository.path, path, force).unwrap_err();
            assert_eq!(error.operation(), "remove worktree");
        }
    }

    assert_eq!(
        fs::read_to_string(outside.join("keep.txt")).unwrap(),
        "keep\n"
    );
    assert_eq!(
        fs::read_to_string(repository.path.join("README.md")).unwrap(),
        "initial\n"
    );
    assert_eq!(
        fs::read_to_string(foreign_tree.path.join("README.md")).unwrap(),
        "initial\n"
    );
    assert_eq!(
        git_stdout(&repository.path, &["rev-parse", "HEAD"]),
        repository.initial_commit
    );
    assert_eq!(
        git_stdout(&foreign_tree.path, &["rev-parse", "HEAD"]),
        foreign.initial_commit
    );
    assert!(registered_worktrees(&foreign.path)
        .unwrap()
        .iter()
        .any(|path| paths_equal(path, &foreign_tree.path)));
    assert!(!missing.exists());
}

#[test]
fn removal_refuses_dirty_work_and_clean_removal_keeps_branch_history() {
    let repository = TestRepository::new();
    let request = repository.request("delete", None, "Safe removal", "main");
    let prepared = prepare_task_worktree_blocking(&request).unwrap();
    let changed = prepared.path.join("keep.txt");
    fs::write(&changed, "uncommitted\n").unwrap();

    let error = remove_task_worktree_blocking(&repository.path, &prepared.path, false).unwrap_err();

    assert_eq!(error.operation(), "remove worktree");
    assert_eq!(fs::read_to_string(&changed).unwrap(), "uncommitted\n");
    assert!(registered_worktrees(&repository.path)
        .unwrap()
        .iter()
        .any(|path| paths_equal(path, &prepared.path)));
    git_ok(&prepared.path, &["add", "keep.txt"]);
    git_ok(&prepared.path, &["commit", "-m", "Keep task history"]);
    let task_head = git_stdout(&prepared.path, &["rev-parse", "HEAD"]);

    remove_task_worktree_blocking(&repository.path, &prepared.path, false).unwrap();

    assert!(!prepared.path.exists());
    assert!(!registered_worktrees(&repository.path)
        .unwrap()
        .iter()
        .any(|path| paths_equal(path, &prepared.path)));
    assert_eq!(
        git_stdout(&repository.path, &["rev-parse", &prepared.branch]),
        task_head
    );
    assert_eq!(
        git_stdout(&repository.path, &["rev-parse", "HEAD"]),
        repository.initial_commit
    );
}

#[test]
fn existing_branch_collision_preserves_its_commit_and_source_checkout() {
    let repository = TestRepository::new();
    let request = repository.request("existing", None, "Existing branch", "main");
    let proposal = resolve_task_preparation_blocking(&request)
        .unwrap()
        .proposal;
    git_ok(&repository.path, &["branch", &proposal.branch]);
    let main_head = repository.commit_file("main.txt", "advance main\n", "Advance main");

    let error = prepare_task_worktree_blocking(&request).unwrap_err();

    assert_eq!(error.operation(), "check collisions");
    assert!(!proposal.path.exists());
    assert_eq!(
        git_stdout(&repository.path, &["rev-parse", &proposal.branch]),
        repository.initial_commit
    );
    assert_eq!(
        git_stdout(&repository.path, &["rev-parse", "HEAD"]),
        main_head
    );
    assert!(git_stdout(&repository.path, &["status", "--porcelain"]).is_empty());
}

#[test]
fn adopts_registered_existing_branch_at_its_fork_point_without_modifying_it() {
    let repository = TestRepository::new();
    let worktree = repository._temp.path().join("existing worktree");
    git_ok(
        &repository.path,
        &[
            "worktree",
            "add",
            "-b",
            "existing-feature",
            worktree.to_str().unwrap(),
            "main",
        ],
    );
    fs::write(worktree.join("feature.txt"), "feature\n").unwrap();
    git_ok(&worktree, &["add", "feature.txt"]);
    git_ok(&worktree, &["commit", "-m", "Existing feature"]);
    let feature_head = git_stdout(&worktree, &["rev-parse", "HEAD"]);
    let main_head = repository.commit_file("main.txt", "main\n", "Advance main");

    let adopted = resolve_worktree_adoption_blocking(&AdoptWorktreeRequest {
        repository_path: repository.path.clone(),
        worktree_path: worktree.clone(),
    })
    .unwrap();

    assert_eq!(adopted.branch, "existing-feature");
    assert_eq!(adopted.base.reference, "main");
    assert_eq!(adopted.base.commit, repository.initial_commit);
    assert_eq!(git_stdout(&worktree, &["rev-parse", "HEAD"]), feature_head);
    assert_eq!(
        git_stdout(&repository.path, &["rev-parse", "HEAD"]),
        main_head
    );
    assert!(git_stdout(&worktree, &["status", "--porcelain"]).is_empty());
}

#[test]
fn adoption_refuses_foreign_unregistered_and_detached_worktrees() {
    let repository = TestRepository::new();
    let foreign = TestRepository::new();
    let detached = repository._temp.path().join("detached");
    git_ok(
        &repository.path,
        &[
            "worktree",
            "add",
            "--detach",
            detached.to_str().unwrap(),
            "HEAD",
        ],
    );
    let unregistered = repository._temp.path().join("unregistered");
    fs::create_dir(&unregistered).unwrap();

    for path in [&foreign.path, &unregistered, &detached] {
        let error = resolve_worktree_adoption_blocking(&AdoptWorktreeRequest {
            repository_path: repository.path.clone(),
            worktree_path: path.clone(),
        })
        .unwrap_err();
        assert_eq!(error.operation(), "adopt worktree");
        assert!(path.is_dir());
    }
    assert_eq!(
        git_stdout(&detached, &["rev-parse", "HEAD"]),
        repository.initial_commit
    );
    assert_eq!(
        git_stdout(&foreign.path, &["rev-parse", "HEAD"]),
        foreign.initial_commit
    );
}
