use super::*;
use crate::{fixture, repo};
use gitterm_core::{
    git_facts::{BaseSource, DiffSource},
    review::{model::ReviewSource, state::CollectStamp, viewed::ViewedMarks},
};

fn stamp(effects: Vec<Effect>) -> CollectStamp {
    effects
        .into_iter()
        .find_map(|effect| match effect {
            Effect::Collect(stamp) => Some(stamp),
            _ => None,
        })
        .expect("stamped collection effect")
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn open(
    root: &std::path::Path,
    executor: &tokio::runtime::Runtime,
) -> (AggregateModel, CollectStamp) {
    let (target, bases) = executor
        .block_on(repo::aggregate_target(root.into()))
        .unwrap();
    assert_eq!(target.base.as_ref().unwrap().reference, "main");
    assert!(bases.iter().any(|base| base.reference == "main"));
    let mut model = AggregateModel::default();
    model.dispatch(Intent::MarksLoaded(Ok(ViewedMarks::default())));
    let stamp = stamp(model.dispatch(Intent::Open {
        target,
        source: ReviewSource::Uncommitted,
        hide_noise: false,
        focus: None,
        page: AggregateModel::page(),
    }));
    (model, stamp)
}

#[test]
fn base_change_refresh_and_stamped_result_project_the_shared_diff() {
    let dir = fixture::aggregate_repo();
    let executor = runtime();
    let (mut model, old) = open(dir.path(), &executor);
    model.dispatch(executor.block_on(repo::collect_aggregate(old)));
    assert_eq!(
        model.projection().files[0].file.hunks[0]
            .lines
            .last()
            .unwrap()
            .text,
        "fn pending() {}"
    );
    let intent = model
        .base_intent(BaseChoice {
            reference: "release".into(),
            source: BaseSource::Local,
            considered: 1,
        })
        .unwrap();
    assert!(matches!(
        intent,
        Intent::Open {
            source: ReviewSource::VsBase,
            ..
        }
    ));
    let base = stamp(model.dispatch(intent));
    assert_eq!(base.source, ReviewSource::VsBase);
    assert_eq!(base.target.base.as_ref().unwrap().reference, "release");
    let refresh = stamp(model.dispatch(Intent::Collect {
        page: AggregateModel::page(),
    }));
    assert!(refresh.generation > base.generation);
    model.dispatch(executor.block_on(repo::collect_aggregate(refresh)));
    let projection = model.projection();
    assert_eq!(projection.files.len(), 1);
    assert_eq!(projection.files[0].file.path, "hello.rs");
    assert!(
        projection.files[0].file.hunks[0]
            .lines
            .iter()
            .any(|line| line.text == "fn second() {}")
    );
    assert!(
        !projection.files[0].file.hunks[0]
            .lines
            .iter()
            .any(|line| line.text == "fn pending() {}")
    );
}

#[test]
fn stale_success_and_error_are_ignored_by_the_authority() {
    let dir = fixture::aggregate_repo();
    let executor = runtime();
    let (mut model, old) = open(dir.path(), &executor);
    let stale = executor.block_on(repo::collect_aggregate(old.clone()));
    let current = stamp(model.dispatch(model.source_intent(DiffSource::VsBase)));
    assert!(model.dispatch(stale).is_empty());
    assert!(model.projection().files.is_empty());
    model.dispatch(executor.block_on(repo::collect_aggregate(current.clone())));
    assert!(
        model
            .dispatch(Intent::CollectFinished {
                stamp: old,
                result: Box::new(Err("stale failure".into()))
            })
            .is_empty()
    );
    assert_eq!(model.state().pane().unwrap().stamp(), current);
    assert_eq!(model.projection().files.len(), 1);
    assert!(model.state().pane().unwrap().error.is_none());
}

#[test]
fn commit_clicks_emit_begin_then_extend_and_core_normalizes_the_range() {
    let dir = fixture::aggregate_repo();
    let executor = runtime();
    let (mut model, open) = open(dir.path(), &executor);
    model.dispatch(executor.block_on(repo::collect_aggregate(open)));
    let show = stamp(model.dispatch(model.commits_intent().unwrap()));
    model.dispatch(executor.block_on(repo::collect_aggregate(show)));
    assert!(model.projection().files.is_empty());
    let commits = model.state().pane().unwrap().commits.clone();
    assert_eq!(commits.len(), 2);
    let first = model.commit_intent(commits[0].sha.clone());
    assert!(
        matches!(&first, Intent::SelectCommit { pending: true, first, last, .. } if first == last)
    );
    let job = stamp(model.dispatch(first));
    model.dispatch(executor.block_on(repo::collect_aggregate(job)));
    let second = model.commit_intent(commits[1].sha.clone());
    assert!(matches!(
        &second,
        Intent::SelectCommit { pending: false, .. }
    ));
    let job = stamp(model.dispatch(second));
    model.dispatch(executor.block_on(repo::collect_aggregate(job)));
    assert!(
        matches!(&model.state().pane().unwrap().source, ReviewSource::Commits(Some(range)) if range.first == commits[1].sha && range.last == commits[0].sha && !range.pending)
    );
    assert_eq!(model.projection().files.len(), 1);
}

#[test]
fn limits_are_projected_without_fetching_unbounded_patches() {
    let dir = fixture::aggregate_repo();
    std::fs::write(dir.path().join("large.txt"), "line\n".repeat(2200)).unwrap();
    let executor = runtime();
    let (mut model, job) = open(dir.path(), &executor);
    let Intent::CollectFinished { stamp, mut result } =
        executor.block_on(repo::collect_aggregate(job))
    else {
        panic!()
    };
    let load = result.as_mut().as_mut().unwrap();
    assert!(load.diff.file("large.txt").unwrap().truncated);
    // Exercise the other budget metadata without manufacturing 2,000 fixture files.
    load.diff.unlisted = 7;
    load.diff.files[0].omitted = true;
    load.commits_truncated = true;
    model.dispatch(Intent::CollectFinished { stamp, result });
    let notices = model.projection().notices.join("\n");
    for text in [
        "7 changed files",
        "1 files omitted",
        "1 file diffs truncated",
        "newest 200 commits",
    ] {
        assert!(notices.contains(text), "{notices}");
    }
}

#[test]
fn viewed_intent_marks_loaded_and_changed_content_use_core_marks() {
    let dir = fixture::aggregate_repo();
    let executor = runtime();
    let (mut model, job) = open(dir.path(), &executor);
    model.dispatch(executor.block_on(repo::collect_aggregate(job)));
    let intent = model.viewed_intent("hello.rs").unwrap();
    assert!(matches!(&intent, Intent::ToggleViewed { path, viewed: true } if path == "hello.rs"));
    assert!(
        model
            .dispatch(intent)
            .iter()
            .any(|effect| matches!(effect, Effect::SaveMarks))
    );
    assert_eq!(model.projection().files[0].viewed, ViewedState::Viewed);
    let marks = model.state().marks().unwrap().as_ref().unwrap().clone();
    model.dispatch(Intent::MarksLoaded(Ok(ViewedMarks::default())));
    assert_eq!(model.projection().files[0].viewed, ViewedState::Unviewed);
    model.dispatch(Intent::MarksLoaded(Ok(marks)));
    assert_eq!(model.projection().files[0].viewed, ViewedState::Viewed);
    std::fs::write(dir.path().join("hello.rs"), "fn changed() {}\n").unwrap();
    let job = stamp(model.dispatch(Intent::Collect {
        page: AggregateModel::page(),
    }));
    model.dispatch(executor.block_on(repo::collect_aggregate(job)));
    assert_eq!(
        model.projection().files[0].viewed,
        ViewedState::ChangedSinceViewed
    );
    model.dispatch(Intent::MarksLoaded(Err("malformed fixture marks".into())));
    assert_eq!(
        model.projection().marks_error,
        Some("malformed fixture marks")
    );
    assert!(
        matches!(&model.dispatch(model.viewed_intent("hello.rs").unwrap())[0], Effect::Rejected(error) if error.contains("malformed fixture marks"))
    );
}
