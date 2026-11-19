use super::*;
use serde::{de::DeserializeOwned, Serialize};

fn shape<T: DeserializeOwned + Serialize>(input: &Value) -> Value {
    let dto: T = serde_json::from_value(input.clone()).unwrap();
    let saved = serde_json::to_value(dto).unwrap();
    let again: T = serde_json::from_value(saved.clone()).unwrap();
    assert_eq!(serde_json::to_value(again).unwrap(), saved);
    saved
}
#[test]
fn persisted_old_current_shapes_defaults_and_omissions() {
    if isolated(
        "persisted_old_current_shapes_defaults_and_omissions",
        "stores",
    ) {
        return;
    }
    let input: Value = serde_json::from_str(&fixture("identity/formats.input.json")).unwrap();
    let mut result = serde_json::Map::new();
    for (label, case) in input.as_object().unwrap() {
        let mut out = serde_json::Map::new();
        macro_rules! capture {
            ($name:literal, $type:ty) => {
                out.insert($name.into(), shape::<$type>(&case[$name]));
            };
        }
        capture!("settings", Config);
        capture!("workspaces", WorkspacesFile);
        capture!("remote_agents", RemoteAgentsFile);
        capture!("remote_sessions", RemoteSessionsFile);
        capture!("limits", gitterm::plan_limits::PlanLimitsFile);
        capture!("focus", gitterm::workspace_overview::FocusPicks);
        capture!("facts", gitterm::chat_facts::ChatFacts);
        capture!("watch", gitterm::pr_watch::WatchFile);
        capture!("issues", gitterm::issue_titles::IssueTitlesFile);
        capture!("merge_plan", gitterm::merge_plan::MergePlan);
        capture!("guidelines", gitterm::guidelines::GuidelinesSettings);
        capture!("agent", gitterm::agentd::config::AgentConfigFile);
        out.insert("phones".into(),json!({"phones":case["phones"]["phones"].as_array().unwrap().iter().map(shape::<gitterm::agentd::phones::PhoneRecord>).collect::<Vec<_>>()}));
        result.insert(label.clone(), out.into());
    }
    golden("identity/formats.saved.json", result.into());
}

#[test]
fn stores_load_action_save_and_refusals() {
    if isolated("stores_load_action_save_and_refusals", "stores") {
        return;
    }
    let dir = config::global_config_dir();
    let input: Value = serde_json::from_str(&fixture("identity/formats.input.json")).unwrap();
    let mut rows = Vec::new();
    for label in ["old", "current"] {
        let case = &input[label];
        std::fs::write(
            dir.join(config::SETTINGS_FILE),
            serde_json::to_vec(&case["settings"]).unwrap(),
        )
        .unwrap();
        let loaded = Config::load();
        assert!(loaded.refused.is_none());
        loaded.config.save().unwrap();
        let settings = read_json(&dir.join(config::SETTINGS_FILE));
        assert_eq!(settings, shape::<Config>(&case["settings"]));
        let ws: WorkspacesFile = serde_json::from_value(case["workspaces"].clone()).unwrap();
        ws.save();
        let ws = WorkspacesFile::load().unwrap();
        ws.save();
        assert_eq!(
            read_json(&WorkspacesFile::file_path()),
            shape::<WorkspacesFile>(&case["workspaces"])
        );
        let agents: RemoteAgentsFile =
            serde_json::from_value(case["remote_agents"].clone()).unwrap();
        agents.save();
        RemoteAgentsFile::load().unwrap().save();
        assert_eq!(
            read_json(&RemoteAgentsFile::file_path()),
            shape::<RemoteAgentsFile>(&case["remote_agents"])
        );
        let limits: gitterm::plan_limits::PlanLimitsFile =
            serde_json::from_value(case["limits"].clone()).unwrap();
        let path = gitterm::plan_limits::PlanLimitsFile::path_in(&dir);
        limits.save(&path).unwrap();
        assert_eq!(gitterm::plan_limits::PlanLimitsFile::load(&path), limits);
        let focus: gitterm::workspace_overview::FocusPicks =
            serde_json::from_value(case["focus"].clone()).unwrap();
        let path = gitterm::workspace_overview::FocusPicks::path_in(&dir);
        focus.save(&path).unwrap();
        assert_eq!(gitterm::workspace_overview::FocusPicks::load(&path), focus);
        let facts: gitterm::chat_facts::ChatFacts =
            serde_json::from_value(case["facts"].clone()).unwrap();
        let path = dir.join("chat-meta/claude-fixture-chat.json");
        gitterm::chat_facts::write_facts(&path, &facts).unwrap();
        assert_eq!(gitterm::chat_facts::load_facts(&path).unwrap(), Some(facts));
        let watch: gitterm::pr_watch::WatchFile =
            serde_json::from_value(case["watch"].clone()).unwrap();
        let path = gitterm::pr_watch::WatchFile::path_in(&dir);
        watch.save(&path).unwrap();
        assert_eq!(gitterm::pr_watch::WatchFile::load(&path), watch);
        let issues: gitterm::issue_titles::IssueTitlesFile =
            serde_json::from_value(case["issues"].clone()).unwrap();
        let path = gitterm::issue_titles::IssueTitlesFile::path_in(&dir);
        issues.save(&path).unwrap();
        assert_eq!(gitterm::issue_titles::IssueTitlesFile::load(&path), issues);
        let plan: gitterm::merge_plan::MergePlan =
            serde_json::from_value(case["merge_plan"].clone()).unwrap();
        let path = gitterm::merge_plan::plans_dir(&dir);
        gitterm::merge_plan::write_plan(&path, &plan).unwrap();
        assert_eq!(
            gitterm::merge_plan::load_plans(&path).get(&plan.root_chat_id),
            Some(&plan)
        );
        let guidelines: gitterm::guidelines::GuidelinesSettings =
            serde_json::from_value(case["guidelines"].clone()).unwrap();
        gitterm::guidelines::save_settings(&dir, &guidelines).unwrap();
        assert_eq!(gitterm::guidelines::load_settings(&dir), guidelines);
        let phone_dir = dir.join(format!("agent-{label}"));
        std::fs::create_dir(&phone_dir).unwrap();
        std::fs::write(
            phone_dir.join("phones.json"),
            serde_json::to_vec(&case["phones"]).unwrap(),
        )
        .unwrap();
        let registry = gitterm::agentd::phones::PhoneRegistry::load(&phone_dir).unwrap();
        let phones = registry.list();
        assert_eq!(phones[0].id, "fixture-phone");
        assert!(registry.revoke("fixture-phone").unwrap());
        assert!(gitterm::agentd::phones::PhoneRegistry::load(&phone_dir)
            .unwrap()
            .list()
            .is_empty());
        let mut picks = focus.clone();
        picks.set(
            Path::new("/fixture/work"),
            &gitterm::workspace_overview::Focus::Latest,
        );
        picks
            .save(&gitterm::workspace_overview::FocusPicks::path_in(&dir))
            .unwrap();
        rows.push(json!({"format":label,"settings_saved":true,"workspaces_saved":true,"phone_last_seen":phones[0].last_seen_unix_secs,
            "phone_after_revoke":read_json(&phone_dir.join("phones.json")),"focus_after_latest":serde_json::to_value(picks).unwrap()}));
    }
    std::fs::write(dir.join("settings.json"), "{broken").unwrap();
    let loaded = Config::load();
    let refused = loaded.refused.is_some();
    let mut app = headless(&dir);
    app.settings_refused = loaded.refused;
    app.save_config();
    rows.push(json!({"format":"malformed settings","load_refused":refused,"app_save_preserves_bytes":std::fs::read_to_string(dir.join("settings.json")).unwrap()=="{broken"}));
    for name in [
        "plan-limits.json",
        "overview-focus.json",
        "pr-watch.json",
        "issue-titles.json",
        "agent-guidelines.json",
    ] {
        std::fs::write(dir.join(name), "{broken").unwrap();
    }
    rows.push(json!({"format":"lossy malformed loads",
        "limits":serde_json::to_value(gitterm::plan_limits::PlanLimitsFile::load(&dir.join("plan-limits.json"))).unwrap(),
        "focus":serde_json::to_value(gitterm::workspace_overview::FocusPicks::load(&dir.join("overview-focus.json"))).unwrap(),
        "watch":serde_json::to_value(gitterm::pr_watch::WatchFile::load(&dir.join("pr-watch.json"))).unwrap(),
        "issues":serde_json::to_value(gitterm::issue_titles::IssueTitlesFile::load(&dir.join("issue-titles.json"))).unwrap(),
        "guidelines":serde_json::to_value(gitterm::guidelines::load_settings(&dir)).unwrap()}));
    let profile = fixture("identity/profiles.json");
    std::fs::write(dir.join("profiles.json"), &profile).unwrap();
    rows.push(json!({"format":"profiles read","loaded":App::load_profiles(),"bytes_unchanged":std::fs::read_to_string(dir.join("profiles.json")).unwrap()==profile}));
    golden("identity/stores.trace.json", json!(rows));
}

#[test]
fn tasks_old_load_action_save_stable_ids() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tasks.json");
    std::fs::write(&path, fixture("tasks/old.tasks.json")).unwrap();
    let mut store = TaskStore::load(&path).unwrap();
    let original = store.get("fixture-task").unwrap().clone();
    assert!(original.sessions.is_empty());
    store
        .begin_worktree_preparation("fixture-task", "2000-01-01T00:01:00Z")
        .unwrap();
    store
        .complete_worktree_preparation(
            "fixture-task",
            gitterm::tasks::CompletedWorktreePreparation {
                repository: original.repository.clone(),
                base: original.base.clone(),
                branch: original.branch.clone(),
                path: PathBuf::from("/fixture/worktree"),
            },
            "2000-01-01T00:02:00Z",
        )
        .unwrap();
    let session = serde_json::from_str(&fixture("tasks/session.json")).unwrap();
    store
        .upsert_session("fixture-task", session, "2000-01-01T00:03:00Z")
        .unwrap();
    let attempt: gitterm::tasks::TaskExecutionAttempt =
        serde_json::from_str(&fixture("tasks/attempt.json")).unwrap();
    store
        .begin_attempt(
            "fixture-task",
            attempt,
            TaskLifecycle::Running,
            "2000-01-01T00:03:30Z",
        )
        .unwrap();
    store
        .record_lifecycle_signal(
            "fixture-task",
            TaskLifecycle::Stopped,
            None,
            "2000-01-01T00:03:50Z",
        )
        .unwrap();
    store
        .update_handoff(
            "fixture-task",
            TaskHandoff {
                summary: "Synthetic handoff".into(),
                decisions: vec![],
                next_steps: vec![],
                blockers: vec![],
                updated_by_session_id: Some("fixture-session".into()),
                updated_at: "2000-01-01T00:04:00Z".into(),
            },
            "2000-01-01T00:04:00Z",
        )
        .unwrap();
    let loaded = TaskStore::load(&path).unwrap();
    assert_eq!(loaded.get("fixture-task"), store.get("fixture-task"));
    golden("tasks/current.tasks.json", read_json(&path));
    let bytes = std::fs::read(&path).unwrap();
    let task = store.get("fixture-task").unwrap().clone();
    store.replace(task).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    std::fs::write(&path, "{broken").unwrap();
    assert!(TaskStore::load(&path).is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "{broken");
}

#[test]
fn delegation_result_receipt_failure_keeps_memory_until_retry() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tasks.json");
    std::fs::write(&path, fixture("tasks/old.tasks.json")).unwrap();
    let mut store = TaskStore::load(&path).unwrap();
    let delegation: gitterm::tasks::Delegation =
        serde_json::from_str(&fixture("tasks/delegation.json")).unwrap();
    store.insert_delegation(delegation).unwrap();
    let handoff = TaskHandoff {
        summary: "Synthetic result".into(),
        decisions: vec![],
        next_steps: vec![],
        blockers: vec![],
        updated_by_session_id: Some("fixture-session".into()),
        updated_at: "2000-01-01T00:05:00Z".into(),
    };
    store
        .complete_delegation(
            "fixture-delegation",
            gitterm::delegations::worker_result(handoff),
            "2000-01-01T00:05:00Z",
        )
        .unwrap();
    let bytes = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    let error = store
        .mark_delegation_delivered("fixture-delegation", "2000-01-01T00:06:00Z")
        .unwrap_err();
    let pending = store
        .delegation("fixture-delegation")
        .unwrap()
        .is_delivered();
    std::fs::remove_dir(&path).unwrap();
    std::fs::write(&path, bytes).unwrap();
    let before = TaskStore::load(&path)
        .unwrap()
        .delegation("fixture-delegation")
        .unwrap()
        .is_delivered();
    store.retry_delivery_acknowledgements().unwrap();
    let loaded = TaskStore::load(&path).unwrap();
    golden(
        "tasks/delivery.trace.json",
        json!({"failure_operation":error.operation(),"pending_in_memory":pending,"disk_receipt_before_retry":before,
        "saved_delegation":serde_json::to_value(loaded.delegation("fixture-delegation").unwrap()).unwrap()}),
    );
}
