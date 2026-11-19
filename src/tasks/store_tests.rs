use super::*;

include!("test_support.rs");

#[test]
fn batch_replacements_write_once_and_match_individual_replacements() {
    let temp = tempfile::tempdir().unwrap();
    let mut batch = TaskStore::load(temp.path().join("batch.json")).unwrap();
    let mut individual = TaskStore::load(temp.path().join("individual.json")).unwrap();
    for id in ["a", "b", "c"] {
        batch.insert(sample_task(id)).unwrap();
        individual.insert(sample_task(id)).unwrap();
    }
    let changed = batch
        .tasks()
        .iter()
        .cloned()
        .map(|mut task| {
            task.title.push_str(" updated");
            task
        })
        .collect::<Vec<_>>();
    let before = batch.write_count.get();
    batch.replace_many(changed.clone()).unwrap();
    assert_eq!(batch.write_count.get() - before, 1);
    for task in changed {
        individual.replace(task).unwrap();
    }
    assert_eq!(batch.document, individual.document);
    assert_eq!(
        TaskStore::load(batch.path()).unwrap().document,
        batch.document
    );
    let before = batch.write_count.get();
    batch.replace_many(Vec::new()).unwrap();
    assert_eq!(batch.write_count.get(), before);
}

#[test]
fn batch_replacements_roll_back_memory_and_disk_on_any_error() {
    let temp = tempfile::tempdir().unwrap();
    let mut store = TaskStore::load(temp.path().join("tasks.json")).unwrap();
    store.insert(sample_task("a")).unwrap();
    store.insert(sample_task("b")).unwrap();
    let original = store.document.clone();
    let bytes = fs::read(store.path()).unwrap();
    let mut changed = store.get("a").unwrap().clone();
    changed.title.push_str(" changed");
    let mut invalid = store.get("b").unwrap().clone();
    invalid.title.clear();
    assert!(store.replace_many(vec![changed.clone(), invalid]).is_err());
    assert!(store
        .replace_many(vec![changed.clone(), sample_task("missing")])
        .is_err());
    let mut invalid_transition = store.get("b").unwrap().clone();
    invalid_transition.lifecycle = TaskLifecycle::Completed;
    assert!(!TaskLifecycle::Draft.can_transition_to(TaskLifecycle::Completed));
    assert!(store
        .replace_many(vec![changed.clone(), invalid_transition])
        .is_err());
    assert_eq!(store.document, original);
    assert_eq!(fs::read(store.path()).unwrap(), bytes);
    // A write failure must also leave the candidate unapplied.
    let blocked = temp.path().join("not-a-file");
    fs::create_dir(&blocked).unwrap();
    store.path = blocked;
    assert!(store.replace_many(vec![changed]).is_err());
    assert_eq!(store.document, original);
    assert_eq!(fs::read(temp.path().join("tasks.json")).unwrap(), bytes);
}

#[test]
fn failed_atomic_replace_keeps_the_prior_valid_file() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join(TASKS_FILE_NAME);
    let mut store = TaskStore::load(&path).unwrap();
    store.insert(sample_task("task-1")).unwrap();
    let before = fs::read(&path).unwrap();
    let mut candidate = store.document.clone();
    candidate.tasks[0].title = "Replacement title".to_string();
    let error = store
        .write_document_with(&candidate, |_temporary, _destination| {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "injected replace failure",
            ))
        })
        .unwrap_err();
    assert!(error.to_string().contains("injected replace failure"));
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(store.tasks()[0].title, "Add durable tasks");
    assert_eq!(
        TaskStore::load(path).unwrap().tasks()[0].title,
        "Add durable tasks"
    );
}

#[test]
fn rejects_running_task_without_an_active_attempt() {
    let mut task = sample_task("task-1");
    task.lifecycle = TaskLifecycle::Running;
    let error = validate_task(&task).unwrap_err();
    assert!(error.contains("without an active attempt"));
}
