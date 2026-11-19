use super::*;
use gitterm::review_pane::*;
use gitterm::workspace_overview::{BaseChoice, BaseSource};

fn target(repo: &Path, base: bool) -> ReviewTarget {
    ReviewTarget {
        checkout: repo.into(),
        repo: "fixture".into(),
        branch: "feature".into(),
        owner: "synthetic".into(),
        base: base.then(|| BaseChoice {
            reference: "main".into(),
            source: BaseSource::Local,
            considered: 1,
        }),
    }
}
fn summary(load: &ReviewLoad) -> Value {
    json!({"files":load.diff.files.iter().map(|f| json!({"path":f.path,"old_path":f.old_path,
        "status":f.status.label(),"kind":format!("{:?}",f.kind),"additions":f.additions,"deletions":f.deletions,
        "shown_lines":f.shown_lines(),"content_id":f.content_id,
        "patch_sample": f.hunks.iter().flat_map(|h| h.lines.iter()).take(4).map(|l|json!({"op":format!("{:?}",l.op),"text":l.text,"old":l.old_no,"new":l.new_no})).collect::<Vec<_>>(),
        "truncated":f.truncated,"omitted":f.omitted,"note":f.note,
        "content_id_kind":f.content_id.as_ref().map(|s| if s.starts_with("deleted:") { "deleted" } else if s.starts_with("commit:") { "commit" } else { "blob" })})).collect::<Vec<_>>(),
        "commits":load.commits.iter().map(|c|c.subject.as_str()).collect::<Vec<_>>(),
        "commits_truncated":load.commits_truncated,"commits_error":load.commits_error,"unlisted":load.diff.unlisted,
        "live_paths":load.live_paths,"merge_base":load.diff.merge_base.is_some()})
}
#[tokio::test]
async fn review_sources_bounds_turns_and_viewed() {
    if isolated("review_sources_bounds_turns_and_viewed", "review") {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    let input: Value = serde_json::from_str(&fixture("review/repository.input.json")).unwrap();
    for (name, body) in input["initial"].as_object().unwrap() {
        std::fs::write(repo.join(name), body.as_str().unwrap()).unwrap();
    }
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "root"]);
    let root = git(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["checkout", "-qb", "feature"]);
    std::fs::write(repo.join("committed.rs"), "fn committed() {}\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "feature"]);
    let first = git(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["checkout", "-qb", "side", "main"]);
    std::fs::write(repo.join("side.rs"), "fn side() {}\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "side"]);
    git(&repo, &["checkout", "-q", "feature"]);
    git(&repo, &["merge", "--no-ff", "-qm", "merge", "side"]);
    let last = git(&repo, &["rev-parse", "HEAD"]);
    for (name, body) in input["dirty"].as_object().unwrap() {
        std::fs::write(repo.join(name), body.as_str().unwrap()).unwrap();
    }
    git(&repo, &["mv", "old.rs", "renamed.rs"]);
    std::fs::remove_file(repo.join("gone.txt")).unwrap();
    std::fs::write(
        repo.join("large.md"),
        (0..FILE_LINE_CAP + 1)
            .map(|i| format!("line {i}\n"))
            .collect::<String>(),
    )
    .unwrap();
    let uncommitted = load_review_source(target(&repo, true), ReviewSource::Uncommitted)
        .await
        .unwrap();
    let base = load_review_source(target(&repo, true), ReviewSource::VsBase)
        .await
        .unwrap();
    let no_selection = load_review_source(target(&repo, true), ReviewSource::Commits(None))
        .await
        .unwrap();
    let mut selections = Vec::new();
    for pending in [true, false] {
        let range = CommitRange {
            first: first.clone(),
            last: last.clone(),
            pending,
        };
        let selected = load_review_source(
            target(&repo, true),
            ReviewSource::Commits(Some(range.clone())),
        )
        .await
        .unwrap();
        let (from, to) = commit_diff_endpoints(&repo, &range).await.unwrap();
        selections.push(json!({"pending":pending,"from_is_root":from == root,"to_is_merge":to == last,"load":summary(&selected)}));
    }
    let (empty, to) = commit_diff_endpoints(
        &repo,
        &CommitRange {
            first: root.clone(),
            last: root.clone(),
            pending: false,
        },
    )
    .await
    .unwrap();
    let mut errors = Vec::new();
    for source in [ReviewSource::VsBase, ReviewSource::Commits(None)] {
        errors.push(
            load_review_source(target(&repo, false), source)
                .await
                .unwrap_err()
                .replace(&repo.display().to_string(), "<repo>"),
        );
    }
    errors.push(
        load_review_source(
            target(&repo, true),
            ReviewSource::Commits(Some(CommitRange {
                first: last.clone(),
                last: first.clone(),
                pending: false,
            })),
        )
        .await
        .unwrap_err(),
    );
    let canonical = std::fs::canonicalize(&repo).unwrap();
    let mut marks = ViewedMarks::default();
    let large = uncommitted.diff.file("large.md").unwrap();
    assert!(marks.set(&canonical, large, true));
    marks.save(temp.path()).unwrap();
    let marks = ViewedMarks::load(temp.path()).unwrap();
    let viewed_shape = json!({"checkouts":{"<checkout>":marks.checkouts.get(&canonical.display().to_string()).unwrap()}});
    let before = marks.state(&canonical, large);
    // Change only text beyond the display cap: visible lines remain identical.
    let mut text = std::fs::read_to_string(repo.join("large.md")).unwrap();
    text.push_str("unseen tail\n");
    std::fs::write(repo.join("large.md"), text).unwrap();
    let changed = load_review_source(target(&repo, true), ReviewSource::Uncommitted)
        .await
        .unwrap();
    let after = marks.state(&canonical, changed.diff.file("large.md").unwrap());
    assert_eq!(large.hunks, changed.diff.file("large.md").unwrap().hunks);
    let mut unknown = large.clone();
    unknown.content_id = None;
    let mut no_id = ViewedMarks::default();
    let refused = !no_id.set(&canonical, &unknown, true);
    let marks_path = temp.path().join(VIEWED_FILE);
    std::fs::write(&marks_path, "{broken").unwrap();
    let read_refused = ViewedMarks::load(temp.path()).is_err();
    assert_eq!(std::fs::read_to_string(marks_path).unwrap(), "{broken");
    // Snapshot the disposable checkout only; the source refs/index stay intact.
    let start = gitterm::turn_snapshots::snapshot(&repo)
        .await
        .unwrap()
        .unwrap();
    gitterm::turn_snapshots::save(&repo, "fixture-chat", 1, "start", start.as_str())
        .await
        .unwrap();
    std::fs::write(repo.join("turn.rs"), "fn turn() {}\n").unwrap();
    let end = gitterm::turn_snapshots::snapshot(&repo)
        .await
        .unwrap()
        .unwrap();
    gitterm::turn_snapshots::save(&repo, "fixture-chat", 1, "end", end.as_str())
        .await
        .unwrap();
    gitterm::turn_snapshots::save(&repo, "fixture-chat", 2, "start", end.as_str())
        .await
        .unwrap();
    gitterm::turn_snapshots::save(&repo, "fixture-chat", 3, "end", end.as_str())
        .await
        .unwrap();
    let mut turns = Vec::new();
    for selected in [None, Some(1), Some(2), Some(3), Some(99)] {
        let result = load_review_source(
            target(&repo, true),
            ReviewSource::Turns {
                chat: "fixture-chat".into(),
                selected,
            },
        )
        .await;
        turns.push(match result {
            Ok(load) => json!({"selected":selected,"load":summary(&load)}),
            Err(error) => json!({"selected":selected,"error":error}),
        });
    }
    golden(
        "review/sources.trace.json",
        json!({"uncommitted":summary(&uncommitted),"base":summary(&base),"commits_empty":summary(&no_selection),"selections":selections,
        "root_endpoint":json!({"from_empty_tree":empty=="4b825dc642cb6eb9a060e54bf8d69288fbee4904","to_root":to==root}),"errors":errors,"turns":turns,
        "viewed_shape":viewed_shape,"viewed":{"before":format!("{before:?}"),"after":format!("{after:?}"),"no_identity_refused":refused,"malformed_load_refused":read_refused}}),
    );
}
#[test]
fn review_budget_contract() {
    let mut budget = Budget::default();
    let lines = untracked_hunks(
        "a\n".repeat(FILE_LINE_CAP + 1).as_bytes(),
        false,
        &mut budget,
    );
    let mut byte_budget = Budget::default();
    let bytes = untracked_hunks(
        "x".repeat(FILE_BYTE_CAP + 1).as_bytes(),
        false,
        &mut byte_budget,
    );
    let mut total = Budget {
        lines: TOTAL_LINE_CAP - 1,
        bytes: 0,
    };
    let tail = untracked_hunks(b"a\nb\n", false, &mut total);
    let admitted = admit(
        &[
            (FileKind::Generated, 2),
            (FileKind::Code, 3),
            (FileKind::Docs, 2),
        ],
        4,
    );
    let mut admitted = admitted.into_iter().collect::<Vec<_>>();
    admitted.sort();
    golden(
        "review/bounds.json",
        json!({"caps":[FILE_LINE_CAP,FILE_BYTE_CAP,ADMIT_LINE_BUDGET,TOTAL_LINE_CAP,TOTAL_BYTE_CAP,FILE_LIST_CAP,UNTRACKED_SCAN_CAP,COMMIT_LIST_CAP],
        "line_budget":budget.lines,"line_truncated":lines.2,"byte_budget":byte_budget.bytes,"byte_truncated":bytes.2,
        "total_lines":total.lines,"total_truncated":tail.2,"admitted":admitted}),
    );
}

#[tokio::test]
async fn review_metadata_only_submodule_and_admission() {
    if isolated("review_metadata_only_submodule_and_admission", "review") {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    let module = repo.join("module");
    std::fs::create_dir_all(&module).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&module, &["init", "-q", "-b", "main"]);
    git(&module, &["commit", "--allow-empty", "-qm", "module root"]);
    let oid = git(&module, &["rev-parse", "HEAD"]);
    std::fs::write(repo.join("mode.sh"), "echo fixture\n").unwrap();
    git(&repo, &["add", "mode.sh"]);
    git(
        &repo,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{oid},module"),
        ],
    );
    for n in 0..11 {
        std::fs::write(repo.join(format!("budget-{n:02}.rs")), "").unwrap();
    }
    git(&repo, &["add", "--all"]);
    git(&repo, &["commit", "-qm", "root"]);
    git(
        &module,
        &["commit", "--allow-empty", "-qm", "module changed"],
    );
    let unstaged = load_review_source(target(&repo, false), ReviewSource::Uncommitted)
        .await
        .unwrap();
    let unstaged_submodule_identity = unstaged.diff.file("module").unwrap().content_id.clone();
    git(&repo, &["add", "module"]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(repo.join("mode.sh"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
    }
    git(&repo, &["update-index", "--chmod=+x", "mode.sh"]);
    for n in 0..11 {
        std::fs::write(
            repo.join(format!("budget-{n:02}.rs")),
            "x\n".repeat(FILE_LINE_CAP + 1),
        )
        .unwrap();
    }
    let load = load_review_source(target(&repo, false), ReviewSource::Uncommitted)
        .await
        .unwrap();
    let files=load.diff.files.iter().map(|f|json!({"path":f.path,"status":f.status.label(),"additions":f.additions,"shown":f.shown_lines(),"truncated":f.truncated,"omitted":f.omitted,
        "identity_kind":f.content_id.as_ref().map(|s| if s.starts_with("commit:") {"commit"} else {"blob"}),
        "zero_commit_identity":f.content_id.as_ref().is_some_and(|id|id==&format!("commit:{}","0".repeat(40)))})).collect::<Vec<_>>();
    golden(
        "review/metadata-admission.trace.json",
        json!({"files":files,"total_files":load.diff.totals.all().files,"unstaged_submodule_identity":unstaged_submodule_identity}),
    );
}
