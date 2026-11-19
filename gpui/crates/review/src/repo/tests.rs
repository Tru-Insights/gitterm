use super::*;
use crate::model::ReviewModel;

pub(crate) fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        let output = gitterm_core::git_status::git_command()
            .current_dir(dir.path())
            .args([
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    };
    git(&["init", "-qb", "main"]);
    std::fs::write(dir.path().join("both.rs"), "fn before() {}\n").unwrap();
    std::fs::write(dir.path().join("old.rs"), "fn renamed() {}\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "fixture"]);
    std::fs::write(dir.path().join("both.rs"), "fn staged() {}\n").unwrap();
    git(&["add", "both.rs"]);
    std::fs::write(dir.path().join("both.rs"), "fn unstaged() {}\n").unwrap();
    git(&["mv", "old.rs", "renamed.rs"]);
    std::fs::create_dir(dir.path().join("notes")).unwrap();
    std::fs::write(dir.path().join("notes/new.txt"), "real untracked\n").unwrap();
    dir
}

#[test]
fn repo_source_loads_real_files_patches_and_invalidates_viewed_by_content() {
    let dir = fixture();
    let executor = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let (snapshot, patches) = executor
        .block_on(load(executor.handle(), 42, dir.path().into()))
        .unwrap();
    assert_eq!(snapshot.tab_id, 42);
    assert_eq!(snapshot.branch_name, "main");
    let mut model = ReviewModel::new(&snapshot, &patches).unwrap();
    assert_eq!(
        model
            .files
            .iter()
            .map(|f| f.key.clone())
            .collect::<Vec<_>>(),
        [
            (Group::Staged, "both.rs".into()),
            (Group::Staged, "renamed.rs".into()),
            (Group::Unstaged, "both.rs".into()),
            (Group::Untracked, "notes/new.txt".into()),
        ]
    );
    assert!(model.files[0].patch.contains("+fn staged() {}"));
    assert!(model.files[2].patch.contains("+fn unstaged() {}"));
    assert!(model.files[3].patch.contains("@@ -0,0 +1 @@"));
    assert_eq!(model.files[1].title(), "old.rs → renamed.rs");
    for file in &mut model.files {
        file.viewed = true;
    }
    std::fs::write(dir.path().join("both.rs"), "fn changed_again() {}\n").unwrap();
    let (snapshot, patches) = executor
        .block_on(load(executor.handle(), 42, dir.path().into()))
        .unwrap();
    model.replace(&snapshot, &patches).unwrap();
    assert!(model.files[0].viewed);
    assert!(!model.files[2].viewed);
    assert!(model.files[3].viewed);
    assert!(model.files[2].patch.contains("+fn changed_again() {}"));
    let outside = tempfile::tempdir().unwrap();
    assert!(
        executor
            .block_on(load(executor.handle(), 42, outside.path().into()))
            .unwrap_err()
            .contains("not a Git repository")
    );
}

#[test]
fn viewed_marks_include_binary_identity_and_large_patch_tail() {
    let dir = fixture();
    std::fs::write(dir.path().join("binary.bin"), b"\0first").unwrap();
    let content = format!(
        "{}first tail\n",
        "line\n".repeat(crate::model::LARGE_LINES + 1)
    );
    std::fs::write(dir.path().join("large.txt"), &content).unwrap();
    let executor = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let (snapshot, patches) = executor
        .block_on(load(executor.handle(), 42, dir.path().into()))
        .unwrap();
    let mut model = ReviewModel::new(&snapshot, &patches).unwrap();
    for file in &mut model.files {
        file.viewed = true;
    }
    std::fs::write(dir.path().join("binary.bin"), b"\0second").unwrap();
    std::fs::write(
        dir.path().join("large.txt"),
        content.replace("first tail", "second tail"),
    )
    .unwrap();
    let (snapshot, patches) = executor
        .block_on(load(executor.handle(), 42, dir.path().into()))
        .unwrap();
    model.replace(&snapshot, &patches).unwrap();
    for path in ["binary.bin", "large.txt"] {
        let file = model.files.iter().find(|f| f.key.1 == path).unwrap();
        assert!(
            !file.viewed,
            "viewed mark survived changed {path}: {}",
            file.patch
        );
        assert_eq!(file.is_binary(), path == "binary.bin");
        assert_eq!(file.collapsed(), path == "large.txt");
    }
}
