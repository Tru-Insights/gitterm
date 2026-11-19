use super::*;
use crate::fixture;
use std::path::Path;

pub(crate) fn fixture_dir() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/showcase"))
}
#[test]
fn status_groups_letters_and_same_path_versions() {
    let (snapshot, patches) = fixture::load(fixture_dir()).unwrap();
    let model = ReviewModel::new(&snapshot, &patches).unwrap();
    assert_eq!(model.files.len(), 7);
    assert_eq!(
        model
            .files
            .iter()
            .map(|f| (f.key.0, f.status.as_str()))
            .collect::<Vec<_>>(),
        [
            (Group::Staged, "M"),
            (Group::Staged, "R"),
            (Group::Unstaged, "M"),
            (Group::Unstaged, "M"),
            (Group::Unstaged, "D"),
            (Group::Untracked, "?"),
            (Group::Untracked, "?")
        ]
    );
    assert_eq!(model.files[0].key.1, model.files[2].key.1);
    assert_ne!(model.files[0].patch, model.files[2].patch);
}
#[test]
fn edge_cases_and_explicit_large_expansion() {
    let mut model = fixture::load_model(fixture_dir()).unwrap();
    assert_eq!(model.files[1].title(), "src/old.rs → src/new.rs");
    assert!(model.files[3].is_binary());
    assert!(!model.files[3].collapsed());
    assert!(model.files[4].patch.contains("+++ /dev/null"));
    assert!(model.files[5].patch.contains("--- /dev/null"));
    assert!(model.files[6].collapsed());
    model.files[6].expanded = true;
    assert!(!model.files[6].collapsed());
    let mut byte_large = model.files.remove(0);
    byte_large.patch = "x".repeat(LARGE_BYTES + 1).into();
    assert!(byte_large.collapsed());
}
#[test]
fn viewed_marks_follow_exact_patch_and_group_across_reorder() {
    let (mut snapshot, mut patches) = fixture::load(fixture_dir()).unwrap();
    let mut model = ReviewModel::new(&snapshot, &patches).unwrap();
    for file in &mut model.files {
        file.viewed = true;
        file.expanded = true;
    }
    let selected = model.files[0].key.clone();
    snapshot.staged.reverse();
    model.replace(&snapshot, &patches).unwrap();
    assert!(model.files.iter().all(|f| f.viewed));
    assert_eq!(model.files[model.selected.unwrap()].key, selected);
    patches.insert(selected.clone(), format!("{}\n", patches[&selected]).into());
    model.replace(&snapshot, &patches).unwrap();
    let changed = &model.files[model.selected.unwrap()];
    assert!(!changed.viewed && !changed.expanded);
    assert!(
        model
            .files
            .iter()
            .filter(|f| f.key != selected)
            .all(|f| f.viewed)
    );
    snapshot.staged.clear();
    model.replace(&snapshot, &patches).unwrap();
    assert_eq!(model.selected, Some(0));
}
#[test]
fn empty_navigation_and_snapshot_errors() {
    let (mut snapshot, patches) = fixture::load(fixture_dir()).unwrap();
    let mut model = ReviewModel::new(&snapshot, &patches).unwrap();
    model.navigate(true);
    assert_eq!(model.selected, Some(0));
    for _ in 0..20 {
        model.navigate(false);
    }
    assert_eq!(model.selected, Some(6));
    for _ in 0..20 {
        model.navigate(true);
    }
    assert_eq!(model.selected, Some(0));
    let mut missing = patches.clone();
    missing.remove(&model.files[0].key);
    assert!(
        model
            .replace(&snapshot, &missing)
            .unwrap_err()
            .contains("missing Staged patch")
    );
    assert_eq!(model.files.len(), 7);
    snapshot.staged.clear();
    snapshot.unstaged.clear();
    snapshot.untracked.clear();
    snapshot.error = Some("fixture source unavailable".into());
    model.replace(&snapshot, &patches).unwrap();
    model.navigate(false);
    model.navigate(true);
    assert!(model.selected.is_none());
    assert_eq!(model.error.as_deref(), Some("fixture source unavailable"));
}
#[test]
fn fixture_errors_include_operation_and_path() {
    let missing = fixture_dir().join("nonexistent");
    let error = fixture::load(&missing).unwrap_err();
    assert!(error.contains("read") && error.contains("nonexistent/review.json"));
}
