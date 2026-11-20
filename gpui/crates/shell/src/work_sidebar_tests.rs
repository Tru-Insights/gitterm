use super::*;
use gitterm_core::{
    chat_facts::{ChatFacts, RepoTouch},
    chats::{ChatBackend, ChatIndexEntry},
    work::{
        state::{Effect, Inputs, Intent, RefreshMode, RefreshOutcome, Source},
        summary::{GitdirStamp, OwnedBranch, Ownership, StatusCounts, TaskLink},
    },
};
use std::time::{Duration, Instant, SystemTime};

fn entry(id: &str, title: &str, now: SystemTime) -> ChatIndexEntry {
    ChatIndexEntry {
        id: id.into(),
        backend: ChatBackend::Claude,
        path: format!("/fixture/{id}.jsonl").into(),
        cwd: "/work".into(),
        repo_root: None,
        is_worktree: false,
        branch: None,
        title: title.into(),
        latest: Some(format!("Latest from {title}")),
        mtime: now,
        size: 1,
        dead_cwd: false,
        root_id: None,
        facts: Some(ChatFacts {
            repos_touched: vec![
                RepoTouch {
                    dir: "/repos/api".into(),
                    edits: 7,
                },
                RepoTouch {
                    dir: "/repos/home".into(),
                    edits: 2,
                },
            ],
            ..ChatFacts::default()
        }),
    }
}

fn branch(repo: &str, name: &str, gone: bool) -> BranchStatus {
    BranchStatus {
        owned: OwnedBranch {
            repo: repo.into(),
            checkout: format!("{repo}-{name}").into(),
            branch: Some(name.into()),
            is_main: false,
            ownership: Ownership::Edited,
            owner: "coordinator".into(),
            last: None,
            edits: 3,
            task: Some(TaskLink {
                task_id: "task".into(),
                base: "main".into(),
                pr_number: Some(42),
            }),
            gone,
        },
        base: None,
        status: Some(StatusCounts {
            files: 3,
            ..StatusCounts::default()
        }),
        vs_base: None,
        error: None,
        stamp: GitdirStamp::default(),
        checked_at: SystemTime::now(),
    }
}

fn fixture(owned: bool) -> WorkSidebar {
    let now = Instant::now();
    let wall_time = SystemTime::now();
    let entries = vec![
        entry("recent", "Ship parity", wall_time),
        entry(
            "old",
            "Earlier work",
            wall_time - Duration::from_secs(10 * 86400),
        ),
    ];
    let mut state = State::default();
    state.reduce(Intent::SetInputs(Inputs {
        workspace: 7,
        source: if owned {
            Source::Owned(vec!["recent".into(), "old".into()])
        } else {
            Source::Folder("/work".into())
        },
        tasks: vec![],
        focus: Focus::Latest,
        documents: None,
    }));
    let effects = state.reduce(Intent::Open {
        workspace: 7,
        now,
        wall_time,
    });
    let (stamp, summary) = effects
        .into_iter()
        .find_map(|effect| match effect {
            Effect::BuildSummary { stamp, plan } => Some((stamp, plan.build(&entries))),
            _ => None,
        })
        .unwrap();
    let effects = state.reduce(Intent::SummaryFinished {
        stamp,
        result: Box::new(Ok(summary)),
        now,
        visible: true,
    });
    let stamp = effects
        .into_iter()
        .find_map(|effect| match effect {
            Effect::Refresh { stamp, .. } => Some(stamp),
            _ => None,
        })
        .unwrap();
    state.reduce(Intent::RefreshFinished {
        stamp,
        result: RefreshOutcome {
            mode: RefreshMode::Full,
            branches: vec![
                branch("/repos/api", "tru-274", false),
                branch("/repos/api", "old-branch", true),
                branch("/repos/infra", "infra-fix", false),
                branch("/repos/hidden", "hidden-fix", false),
            ],
            errors: vec![],
            git_runs: 0,
            skipped_unchanged: 0,
            took: Duration::ZERO,
        },
        now,
        visible: true,
    });
    WorkSidebar::new(
        WorkspaceFacts {
            id: 7,
            name: "Parity".into(),
            dir: "/work".into(),
            home: "/repos/home".into(),
            local: true,
            repos: vec![
                RepoFacts {
                    dir: "/repos/home".into(),
                    local: true,
                    run_command: Some("cargo run".into()),
                },
                RepoFacts {
                    dir: "/repos/tools".into(),
                    local: true,
                    run_command: None,
                },
            ],
            removed: vec!["/repos/hidden".into()],
            auto_added: vec!["/repos/tools".into()],
            open_chats: HashSet::from(["recent".into()]),
            owned_chats: if owned {
                vec!["recent".into(), "old".into()]
            } else {
                vec![]
            },
            chat_owners: HashMap::new(),
            running_by_repo: HashMap::from([("/repos/api".into(), 2)]),
            selected_checkout: None,
        },
        Rc::new(RefCell::new(state)),
    )
}

fn control(sidebar: &WorkSidebar, row_label: &str, control_label: &str) -> Action {
    let row = sidebar
        .rows()
        .into_iter()
        .find(|row| row.label == row_label)
        .unwrap();
    row.controls
        .into_iter()
        .chain(row.detail_controls)
        .find(|control| control.label == control_label)
        .unwrap()
        .action
}

#[test]
fn fixture_row_order_labels_and_repo_facts_match_iced() {
    let sidebar = fixture(false);
    let rows = sidebar.rows();
    assert_eq!(
        rows.iter()
            .map(|row| row.label.as_str())
            .collect::<Vec<_>>(),
        [
            "Parity",
            "Repos",
            "tools added to this workspace's repos",
            "home",
            "tools",
            "api",
            "infra",
            "Focus: Ship parity",
            "Chats",
            "✦ Ship parity",
            "1 older ▸",
        ]
    );
    assert_eq!(rows[1].badges, ["4"]);
    assert_eq!(
        rows[3].details,
        ["home · 2 edits · no live checkout of this work"]
    );
    assert_eq!(rows[4].details, ["no edits"]);
    assert_eq!(rows[5].details, ["7 edits · 1 branch · 1 PR · 2 running"]);
    assert_eq!(rows[6].details, ["1 branch · 1 PR"]);
    assert_eq!(rows[8].badges, ["1 in the last 7 days"]);
    assert_eq!(rows[9].badges[0], "open");
    assert!(rows[9].selected);
    assert_eq!(rows[3].run.as_ref().unwrap().label, "▶ cargo run");
}

#[test]
fn disclosure_reveals_branches_age_gone_and_run_editor() {
    let mut sidebar = fixture(false);
    sidebar.activate(Action::Toggle(Fold::Repo("/repos/api".into())));
    let rows = sidebar.rows();
    let api = rows.iter().position(|row| row.label == "api").unwrap();
    assert_eq!(rows[api + 1].label, "tru-274");
    assert_eq!(rows[api + 1].badges, ["3 dirty"]);
    assert_eq!(rows[api + 2].label, "▸ 1 folder gone · old-branch");
    assert_eq!(rows[api].age.as_deref(), Some("now"));
    sidebar.activate(Action::Toggle(Fold::Branch("/repos/api-tru-274".into())));
    assert_eq!(
        sidebar
            .rows()
            .into_iter()
            .find(|row| row.label == "tru-274")
            .unwrap()
            .details,
        ["api-tru-274 · no base · not pushed · PR #42 · coordinator"]
    );
    sidebar.activate(Action::Toggle(Fold::Repo("/repos/tools".into())));
    assert_eq!(
        control(&sidebar, "tools", "+ run command"),
        Action::Edit("/repos/tools".into())
    );
    sidebar.activate(Action::Toggle(Fold::Repo("/repos/api".into())));
    assert!(!sidebar.rows().iter().any(|row| row.label == "tru-274"));
}

#[test]
fn home_is_never_removable_and_touched_repos_can_be_hidden() {
    let mut sidebar = fixture(false);
    for dir in ["home", "tools", "api"] {
        sidebar.activate(Action::Toggle(Fold::Repo(format!("/repos/{dir}").into())));
    }
    let home = sidebar
        .rows()
        .into_iter()
        .find(|row| row.label == "home")
        .unwrap();
    assert!(!home.controls.iter().any(|control| control.label == "×"));
    assert_eq!(
        sidebar.activate(Action::Emit(WorkSidebarOutcome::RemoveRepo {
            workspace: 7,
            repo: "/repos/home".into()
        })),
        None
    );
    for dir in ["tools", "api"] {
        let action = control(&sidebar, dir, "×");
        assert_eq!(
            sidebar.activate(action),
            Some(WorkSidebarOutcome::RemoveRepo {
                workspace: 7,
                repo: format!("/repos/{dir}").into()
            })
        );
    }
}

#[test]
fn add_undo_and_dismiss_use_actual_row_controls() {
    let mut sidebar = fixture(false);
    let action = control(&sidebar, "Repos", "+ Add repo…");
    assert_eq!(sidebar.activate(action), Some(WorkSidebarOutcome::AddRepo));
    let action = control(&sidebar, "tools added to this workspace's repos", "Undo");
    assert_eq!(
        sidebar.activate(action),
        Some(WorkSidebarOutcome::UndoAutoRepo {
            workspace: 7,
            repos: vec!["/repos/tools".into()]
        })
    );
    let action = control(&sidebar, "tools added to this workspace's repos", "×");
    assert_eq!(
        sidebar.activate(action),
        Some(WorkSidebarOutcome::DismissAutoRepos { workspace: 7 })
    );
    assert!(
        !sidebar
            .rows()
            .iter()
            .any(|row| row.label.contains("added to this workspace"))
    );
    sidebar.workspace.local = false;
    assert!(sidebar.rows()[1].controls.is_empty());
}

#[test]
fn owned_coordinators_open_and_folder_coordinators_focus_with_explicit_open() {
    for owned in [false, true] {
        let mut sidebar = fixture(owned);
        let row = sidebar
            .rows()
            .into_iter()
            .find(|row| row.label == "✦ Ship parity")
            .unwrap();
        assert_eq!(
            sidebar.activate(row.action.unwrap()),
            Some(if owned {
                WorkSidebarOutcome::OpenChat {
                    workspace: 7,
                    chat: "recent".into(),
                }
            } else {
                WorkSidebarOutcome::Focus {
                    workspace: 7,
                    focus: Focus::Coordinator("recent".into()),
                }
            })
        );
        assert!(!row.controls.iter().any(|c| c.label == "↗"));
        sidebar.activate(Action::Toggle(Fold::Chat("recent".into())));
        let action = control(&sidebar, "✦ Ship parity", "↗");
        assert_eq!(
            sidebar.activate(action),
            Some(WorkSidebarOutcome::OpenChat {
                workspace: 7,
                chat: "recent".into()
            })
        );
        let rows = sidebar.rows();
        assert_eq!(rows.iter().any(|row| row.label == "✦ Earlier work"), owned);
        assert_eq!(
            rows.iter().any(|row| row.label.starts_with("Focus:")),
            !owned
        );
    }
}

#[test]
fn focus_menu_and_older_fold_do_not_reimplement_summary_focus() {
    let mut sidebar = fixture(false);
    sidebar.activate(Action::Toggle(Fold::Focus));
    let row = sidebar
        .rows()
        .into_iter()
        .find(|row| row.label == "All recent work · 1 chats, last 7 days")
        .unwrap();
    assert_eq!(
        sidebar.activate(row.action.unwrap()),
        Some(WorkSidebarOutcome::Focus {
            workspace: 7,
            focus: Focus::AllRecent
        })
    );
    assert_eq!(
        sidebar.state.borrow().pane(7).unwrap().summary.focus,
        Focus::Latest,
        "only the host dispatches reducer changes"
    );
    sidebar.activate(Action::Toggle(Fold::Older));
    assert!(
        sidebar
            .rows()
            .iter()
            .any(|row| row.label == "✦ Earlier work")
    );
}

#[test]
fn run_command_submission_trims_and_empty_removes_command() {
    assert_eq!(
        WorkSidebar::run_command_outcome("/repos/tools".into(), "  npm run dev  "),
        WorkSidebarOutcome::EditRunCommand {
            repo: "/repos/tools".into(),
            command: Some("npm run dev".into())
        }
    );
    assert_eq!(
        WorkSidebar::run_command_outcome("/repos/tools".into(), " \n "),
        WorkSidebarOutcome::EditRunCommand {
            repo: "/repos/tools".into(),
            command: None
        }
    );
}

#[test]
fn run_review_editor_and_owner_actions_are_typed() {
    let mut sidebar = fixture(false);
    let action = sidebar
        .rows()
        .into_iter()
        .find(|row| row.label == "home")
        .unwrap()
        .run
        .unwrap()
        .action;
    assert_eq!(
        sidebar.activate(action),
        Some(WorkSidebarOutcome::RunRepoCommand {
            repo: "/repos/home".into()
        })
    );
    sidebar.activate(Action::Toggle(Fold::Repo("/repos/api".into())));
    let row = sidebar
        .rows()
        .into_iter()
        .find(|row| row.label == "tru-274")
        .unwrap();
    assert_eq!(
        sidebar.activate(row.action.unwrap()),
        Some(WorkSidebarOutcome::OpenReview {
            checkout: "/repos/api-tru-274".into(),
            vs_base: false
        })
    );
    sidebar.activate(Action::Toggle(Fold::Repo("/repos/tools".into())));
    let action = control(&sidebar, "tools", "Open in editor");
    assert_eq!(
        sidebar.activate(action),
        Some(WorkSidebarOutcome::OpenEditor {
            dir: "/repos/tools".into()
        })
    );
    let action = control(&sidebar, "✦ Ship parity", "⊞ Make workspace");
    assert_eq!(
        sidebar.activate(action),
        Some(WorkSidebarOutcome::MakeWorkspace {
            chat: "recent".into()
        })
    );
    sidebar
        .workspace
        .chat_owners
        .insert("recent".into(), "Other work".into());
    let action = control(&sidebar, "✦ Ship parity", "→ Go to Other work");
    assert_eq!(
        sidebar.activate(action),
        Some(WorkSidebarOutcome::GoToChatOwner {
            chat: "recent".into()
        })
    );
}

#[test]
fn reducer_updates_are_reflected_without_rebuilding_or_mutating_the_view() {
    let sidebar = fixture(false);
    sidebar.state.borrow_mut().reduce(Intent::Remove(7));
    assert_eq!(sidebar.rows().last().unwrap().label, "Loading…");
}

#[test]
fn undo_never_removes_home_even_if_the_host_lists_it_in_the_note() {
    let mut sidebar = fixture(false);
    sidebar
        .workspace
        .auto_added
        .insert(0, sidebar.workspace.home.clone());
    let action = control(
        &sidebar,
        "home, tools added to this workspace's repos",
        "Undo",
    );
    assert_eq!(
        sidebar.activate(action),
        Some(WorkSidebarOutcome::UndoAutoRepo {
            workspace: 7,
            repos: vec!["/repos/tools".into()],
        })
    );
}

#[test]
fn refresh_and_more_in_find_emit_from_fixture_rows() {
    let mut sidebar = fixture(false);
    let action = control(&sidebar, "Parity", "↻");
    assert_eq!(
        sidebar.activate(action),
        Some(WorkSidebarOutcome::Refresh { workspace: 7 })
    );
    let now = Instant::now();
    let wall_time = SystemTime::now();
    let entries: Vec<_> = (0..14)
        .map(|i| entry(&format!("chat-{i:02}"), "Chat", wall_time))
        .collect();
    let effects = sidebar.state.borrow_mut().reduce(Intent::IndexChanged {
        visible: Some(7),
        wall_time,
    });
    let (stamp, summary) = effects
        .into_iter()
        .find_map(|effect| match effect {
            Effect::BuildSummary { stamp, plan } => Some((stamp, plan.build(&entries))),
            _ => None,
        })
        .unwrap();
    sidebar.state.borrow_mut().reduce(Intent::SummaryFinished {
        stamp,
        result: Box::new(Ok(summary)),
        now,
        visible: false,
    });
    let rows = sidebar.rows();
    assert_eq!(rows.iter().filter(|row| row.label == "✦ Chat").count(), 12);
    let action = rows
        .into_iter()
        .find(|row| row.label == "… 2 more in Find")
        .unwrap()
        .action
        .unwrap();
    assert_eq!(sidebar.activate(action), Some(WorkSidebarOutcome::OpenFind));
}

struct IcedFixture {
    root: tempfile::TempDir,
    config: PathBuf,
    workspace: gitterm_core::workspace_config::WorkspaceConfig,
    repo: PathBuf,
}
impl IcedFixture {
    fn new() -> Self {
        use gitterm_core::tasks::*;
        let root = tempfile::tempdir_in("/private/tmp").unwrap();
        let base = root.path().canonicalize().unwrap();
        let config = base.join("iced");
        let home = base.join("work");
        let repo = base.join("api");
        for dir in [&config, &home.join("docs"), &repo] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let git = |args: &[&str]| {
            let output = gitterm_core::git_status::git_command()
                .current_dir(&repo)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-q", "-b", "main"]);
        git(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "fixture",
        ]);
        std::fs::write(repo.join("changed.txt"), "changed").unwrap();
        for (dir, name, text) in [
            (&home, "docs/note.md", "# Work note\n"),
            (&repo, ".plans/plan.md", "# Repo plan\n"),
            (&repo, "docs/DECISIONS.md", "# Decisions\n"),
            (&repo, "docs/random.md", "# Not listed\n"),
        ] {
            let path = dir.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        let file = serde_json::json!({"active_workspace":0,"workspaces":[
            {"name":"Work fixture","abbrev":"WF","dir":home,"color":"blue","location":{"kind":"local","root":repo},"tabs":[{"dir":home,"chat_session_id":"root-chat","session_uid":"parent"}],"chats":["root-chat"],"repos":[home,repo]},
            {"name":"Other","abbrev":"O","dir":base.join("other"),"color":"blue","tabs":[]}
        ],"repos":[{"dir":repo,"run_command":"cargo run"}]});
        std::fs::write(
            config.join("workspaces.json"),
            serde_json::to_vec(&file).unwrap(),
        )
        .unwrap();
        let facts_dir = gitterm_core::chat_facts::facts_dir(&config);
        std::fs::create_dir(&facts_dir).unwrap();
        let facts = ChatFacts {
            checkouts: vec![gitterm_core::chat_facts::CheckoutTouch {
                repo: repo.clone(),
                dir: repo.clone(),
                branch: Some("main".into()),
                edits: 3,
                last: "2026-01-01T00:00:00Z".into(),
            }],
            repos_touched: vec![RepoTouch {
                dir: repo.clone(),
                edits: 3,
            }],
            ..Default::default()
        };
        std::fs::write(
            gitterm_core::chat_facts::facts_path(&facts_dir, ChatBackend::Claude, "root-chat")
                .unwrap(),
            serde_json::to_vec(&facts).unwrap(),
        )
        .unwrap();
        let mut task = crate::tasks::fixture_task("task-1", "Fixture task", "task/fixture");
        task.workspace.location = WorkspaceLocationIdentity::Local {
            directory: repo.clone(),
        };
        task.repository.common_dir = repo.join(".git");
        task.lifecycle = TaskLifecycle::Interrupted;
        task.attention.reason = Some(TaskAttentionReason::Interrupted);
        task.attention.unread = true;
        task.attention.since = Some("2026-01-01T00:00:00Z".into());
        let mut other = task.clone();
        other.task_id = "other-task".into();
        other.workspace.location = WorkspaceLocationIdentity::Local {
            directory: base.join("other"),
        };
        let worker = Delegation {
            delegation_id: "worker-1".into(),
            kind: DelegationKind::Implement,
            parent: DelegationParent {
                session_uid: "parent".into(),
                chat_session_id: Some("root-chat".into()),
                workspace: "Work fixture".into(),
                cwd: home,
            },
            child: DelegationChild::TaskSession {
                task_id: task.task_id.clone(),
                task_session_id: None,
                conversation: None,
                preset_name: None,
                model: None,
                role: None,
            },
            brief: "Implement fixture".into(),
            target: None,
            previous: None,
            superseded_by: None,
            status: DelegationStatus::Running,
            result: None,
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            delivered_at: None,
            delivered: false,
            dismissed_at: None,
            options: Default::default(),
            time_limit_reached_at: None,
            time_limit_deadline_unix_ms: None,
        };
        let document = TaskStoreDocument {
            tasks: vec![task, other],
            delegations: vec![worker],
            ..Default::default()
        };
        std::fs::write(
            config.join("tasks.json"),
            serde_json::to_vec(&document).unwrap(),
        )
        .unwrap();
        let catalog = crate::workspace_config::IcedCatalog::read(&config).unwrap();
        Self {
            root,
            config,
            workspace: catalog.workspaces[0].clone(),
            repo,
        }
    }
    fn load(&self) -> crate::data::IcedWorkData {
        crate::data::IcedWorkData::read(&self.config, 1, &self.workspace, Focus::Latest).unwrap()
    }
}

fn tree_snapshot(
    root: &std::path::Path,
) -> std::collections::BTreeMap<PathBuf, (Option<Vec<u8>>, SystemTime)> {
    let mut snapshot = std::collections::BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let meta = std::fs::metadata(&path).unwrap();
        let bytes = if meta.is_dir() {
            pending.extend(std::fs::read_dir(&path).unwrap().map(|e| e.unwrap().path()));
            None
        } else {
            Some(std::fs::read(&path).unwrap())
        };
        snapshot.insert(path, (bytes, meta.modified().unwrap()));
    }
    snapshot
}

#[test]
fn iced_work_fixture_has_real_rows_in_order_and_never_changes_source_bytes_or_mtimes() {
    let fixture = IcedFixture::new();
    let before = tree_snapshot(fixture.root.path());
    let data = fixture.load();
    assert_eq!(data.tasks.len(), 1);
    assert_eq!(data.workers.len(), 1);
    assert_eq!(
        data.facts.repos.iter().map(|r| &r.dir).collect::<Vec<_>>(),
        [
            PathBuf::from(fixture.workspace.home_dir()),
            fixture.repo.clone()
        ]
        .iter()
        .collect::<Vec<_>>()
    );
    assert_eq!(
        data.facts.repos[1].run_command.as_deref(),
        Some("cargo run")
    );
    let state = Rc::new(RefCell::new(State::default()));
    data.apply(&mut state.borrow_mut());
    let mut sidebar = WorkSidebar::new(data.facts, state);
    sidebar.tasks = data.tasks;
    sidebar.workers = data.workers;
    assert_eq!(
        sidebar
            .rows()
            .iter()
            .filter(|r| r.heading)
            .map(|r| r.label.as_str())
            .collect::<Vec<_>>(),
        [
            "Work fixture",
            "Repos",
            "Tasks",
            "Workers",
            "Chats",
            "Documents",
            "Needs you"
        ]
    );
    assert!(!sidebar.rows().iter().any(|r| r.label == "Work note"));
    sidebar.open.insert(Fold::Documents);
    sidebar.open.insert(Fold::Repo(fixture.repo.clone()));
    let rows = sidebar.rows();
    for label in [
        "api",
        "Fixture task",
        "Implement fixture",
        "Work note",
        "Repo plan",
        "Decisions",
        "main",
    ] {
        assert!(rows.iter().any(|r| r.label == label), "missing {label}");
    }
    assert!(!rows.iter().any(|r| r.label == "Not listed"));
    assert!(
        rows.iter()
            .find(|r| r.label == "main")
            .unwrap()
            .badges
            .iter()
            .any(|badge| badge.contains("dirty"))
    );
    assert_eq!(
        rows.iter().find(|r| r.label == "Work note").unwrap().action,
        Some(Action::Emit(WorkSidebarOutcome::OpenDocument {
            workspace: 1,
            path: PathBuf::from(&fixture.workspace.dir).join("docs/note.md")
        }))
    );
    // A second load also must not acquire a lease, migrate a task, or write
    // status refresh metadata in the repositories or source config directory.
    fixture.load();
    assert_eq!(tree_snapshot(fixture.root.path()), before);
}

#[test]
fn missing_and_malformed_task_stores_are_read_only_and_errors_are_visible() {
    let fixture = IcedFixture::new();
    std::fs::remove_file(fixture.config.join("tasks.json")).unwrap();
    let before = tree_snapshot(fixture.root.path());
    assert!(fixture.load().tasks.is_empty());
    assert_eq!(tree_snapshot(fixture.root.path()), before);
    std::fs::write(fixture.config.join("tasks.json"), b"{bad").unwrap();
    let before = tree_snapshot(fixture.root.path());
    let result =
        crate::data::IcedWorkData::read(&fixture.config, 1, &fixture.workspace, Focus::Latest);
    assert!(result.err().unwrap().to_string().contains("task store"));
    assert_eq!(tree_snapshot(fixture.root.path()), before);
}

#[gpui::test]
fn write_outcomes_report_unavailable_and_still_emit_typed_events(cx: &mut gpui::TestAppContext) {
    let sidebar = cx.new(|_| fixture(false));
    let events = Rc::new(RefCell::new(Vec::new()));
    let received = events.clone();
    cx.update(|cx| {
        cx.subscribe(&sidebar, move |_, outcome: &WorkSidebarOutcome, _| {
            received.borrow_mut().push(outcome.clone());
        })
        .detach();
    });
    let actions = [
        WorkSidebarOutcome::AddRepo,
        WorkSidebarOutcome::RemoveRepo {
            workspace: 7,
            repo: "/repos/tools".into(),
        },
        WorkSidebarOutcome::UndoAutoRepo {
            workspace: 7,
            repos: vec!["/repos/tools".into()],
        },
        WorkSidebarOutcome::EditRunCommand {
            repo: "/repos/tools".into(),
            command: Some("cargo run".into()),
        },
        WorkSidebarOutcome::DismissAutoRepos { workspace: 7 },
        WorkSidebarOutcome::RunRepoCommand {
            repo: "/repos/tools".into(),
        },
        WorkSidebarOutcome::OpenEditor {
            dir: "/repos/tools".into(),
        },
        WorkSidebarOutcome::MakeWorkspace {
            chat: "root".into(),
        },
    ];
    for outcome in &actions {
        sidebar.update(cx, |sidebar, cx| {
            sidebar.emit(outcome.clone(), cx);
            assert_eq!(sidebar.error, outcome.preview_error());
            assert!(
                sidebar
                    .error
                    .as_deref()
                    .unwrap()
                    .contains("not available in preview")
            );
        });
    }
    assert_eq!(events.borrow().as_slice(), actions.as_slice());
    assert!(
        WorkSidebarOutcome::Refresh { workspace: 7 }
            .preview_error()
            .is_none()
    );
}

#[gpui::test]
fn owner_catalog_supplies_mounted_sidebar_read_authority_without_a_gui(
    cx: &mut gpui::TestAppContext,
) {
    cx.background_executor.allow_parking();
    let fixture = IcedFixture::new();
    let before = tree_snapshot(fixture.root.path());
    let catalog = crate::workspace_config::IcedCatalog::read(&fixture.config).unwrap();
    let model = crate::workspace_status::WorkspaceStatus::from_iced(&catalog);
    let mut store =
        crate::workspace_config::WorkspaceStore::new(&fixture.root.path().join("preview"));
    store.iced = Some(catalog);
    let owner = cx.new(|cx| crate::WorkspaceOwner::new(model, Some(store), cx));
    let mut facts = fixture.load().facts;
    facts.repos.clear();
    let sidebar = cx.new(|_| WorkSidebar::new(facts, Default::default()));
    sidebar.update(cx, |sidebar, cx| sidebar.load(false, cx));
    cx.run_until_parked();
    sidebar.read_with(cx, |sidebar, _| {
        assert!(!sidebar.loading);
        assert!(sidebar.error.is_none(), "{:?}", sidebar.error);
        assert_eq!(sidebar.workspace.repos.len(), 2);
        assert_eq!(sidebar.tasks.len(), 1);
    });
    let files = Rc::new(RefCell::new(Vec::new()));
    let received = files.clone();
    cx.update(|cx| {
        cx.subscribe(&owner, move |_, event: &crate::FileOpen, _| {
            received.borrow_mut().push(event.path.clone())
        })
        .detach();
    });
    let path = PathBuf::from(&fixture.workspace.dir).join("docs/note.md");
    sidebar.update(cx, |sidebar, cx| {
        sidebar.emit(
            WorkSidebarOutcome::OpenDocument {
                workspace: 1,
                path: path.clone(),
            },
            cx,
        )
    });
    assert_eq!(files.borrow().as_slice(), &[path]);
    sidebar.update(cx, |sidebar, cx| {
        sidebar.emit(
            WorkSidebarOutcome::RunRepoCommand {
                repo: fixture.repo.clone(),
            },
            cx,
        )
    });
    owner.read_with(cx, |owner, _| {
        assert_eq!(
            owner.error.as_deref(),
            Some("Run command · not available in preview")
        )
    });
    assert_eq!(tree_snapshot(fixture.root.path()), before);
}

fn indexed_transcript(
    home: &std::path::Path,
    id: &str,
    cwd: &std::path::Path,
    title: &str,
    latest: &str,
    timestamp: &str,
) {
    let path = home
        .join(".claude/projects/fixture")
        .join(format!("{id}.jsonl"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let lines = [
        serde_json::json!({"type":"user", "cwd":cwd, "gitBranch":"main",
            "timestamp":"2026-11-19T00:00:00Z", "message":{"content":"Opening request"}}),
        serde_json::json!({"type":"ai-title", "aiTitle": title}),
        serde_json::json!({"type":"user", "cwd":cwd, "timestamp":timestamp,
            "message":{"content": latest}}),
        // Reopening a chat changes the file, but must not change activity time.
        serde_json::json!({"type":"file-history-snapshot", "timestamp":"2030-01-01T00:00:00Z"}),
    ];
    std::fs::write(
        &path,
        lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .unwrap();
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_000))
        .unwrap();
}

#[test]
fn indexed_work_uses_iced_titles_latest_and_child_activity_in_iced_order_read_only() {
    use gitterm_core::tasks::*;
    let fixture = IcedFixture::new();
    let home = fixture.root.path().join("harness-home");
    let cwd = PathBuf::from(&fixture.workspace.dir);
    indexed_transcript(
        &home,
        "root-chat",
        &cwd,
        "Real root title",
        "Latest root request",
        "2026-11-20T12:00:00Z",
    );
    indexed_transcript(
        &home,
        "second",
        &cwd,
        "Second title",
        "Latest second request",
        "2026-11-20T13:00:00Z",
    );
    indexed_transcript(
        &home,
        "worker",
        &fixture.repo,
        "Worker title",
        "Latest worker request",
        "2026-11-20T14:00:00Z",
    );
    indexed_transcript(
        &home,
        "dead",
        &home.join("missing-cwd"),
        "Dead title",
        "Gone",
        "2026-11-20T15:00:00Z",
    );
    let mut file: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixture.config.join("workspaces.json")).unwrap())
            .unwrap();
    file["workspaces"][0]["chats"] = serde_json::json!(["second", "root-chat"]);
    std::fs::write(
        fixture.config.join("workspaces.json"),
        serde_json::to_vec(&file).unwrap(),
    )
    .unwrap();
    let task_path = fixture.config.join("tasks.json");
    let mut store: TaskStoreDocument =
        serde_json::from_slice(&std::fs::read(&task_path).unwrap()).unwrap();
    if let DelegationChild::TaskSession { conversation, .. } = &mut store.delegations[0].child {
        *conversation = Some(HarnessConversationRef {
            backend: HarnessConversationBackend::Claude,
            session_id: "worker".into(),
        });
    } else {
        panic!("fixture task child");
    }
    std::fs::write(task_path, serde_json::to_vec(&store).unwrap()).unwrap();
    let before = tree_snapshot(fixture.root.path());
    let data = crate::data::IcedWorkData::read_with_home(
        &fixture.config,
        Some(&home),
        1,
        &fixture.workspace,
        Focus::Latest,
    )
    .unwrap();
    let state = Rc::new(RefCell::new(State::default()));
    data.apply(&mut state.borrow_mut());
    let summary = state.borrow().pane(1).unwrap().summary.clone();
    let noon = SystemTime::UNIX_EPOCH + Duration::from_secs(1_795_176_000);
    assert_eq!(
        summary
            .coordinators
            .iter()
            .map(|c| (c.id.as_str(), c.title.as_str()))
            .collect::<Vec<_>>(),
        [("root-chat", "Real root title"), ("second", "Second title")]
    );
    let root = &summary.coordinators[0];
    assert_eq!(root.latest.as_deref(), Some("Latest root request"));
    assert_eq!(root.mtime, noon);
    assert_eq!(root.active, noon + Duration::from_secs(7200));
    assert_eq!(
        root.children
            .iter()
            .map(|c| c.title.as_str())
            .collect::<Vec<_>>(),
        ["Worker title"]
    );
    let mut sidebar = WorkSidebar::new(data.facts, state);
    sidebar.repo_recency = data.repo_recency;
    sidebar.open.insert(Fold::Chat("root-chat".into()));
    let rows = sidebar.rows();
    assert_eq!(
        rows.iter()
            .filter(|r| r.label.starts_with("✦ "))
            .map(|r| r.label.as_str())
            .collect::<Vec<_>>(),
        ["✦ Real root title", "✦ Second title"]
    );
    assert!(
        rows.iter()
            .find(|r| r.label == "✦ Real root title")
            .unwrap()
            .details
            .iter()
            .any(|line| line == "↳ Latest root request")
    );
    assert_eq!(
        sidebar.repo_recency(),
        &[
            (fixture.repo.clone(), noon + Duration::from_secs(7200)),
            (cwd, noon + Duration::from_secs(3600))
        ]
    );
    // Without transcripts, task links must not change the previous fallback rows.
    let fallback = crate::data::IcedWorkData::read_with_home(
        &fixture.config,
        Some(&home.join("absent-index")),
        1,
        &fixture.workspace,
        Focus::Latest,
    )
    .unwrap();
    let mut state = State::default();
    fallback.apply(&mut state);
    assert!(
        state
            .pane(1)
            .unwrap()
            .summary
            .coordinators
            .iter()
            .all(|c| c.fan_out.is_none())
    );
    assert_eq!(tree_snapshot(fixture.root.path()), before);
}

#[test]
fn folder_work_includes_historical_chats_and_missing_index_preserves_fallback() {
    let fixture = IcedFixture::new();
    let home = fixture.root.path().join("harness-home");
    let missing = fixture.root.path().join("absent-home");
    let before = tree_snapshot(fixture.root.path());
    let fallback = crate::data::IcedWorkData::read_with_home(
        &fixture.config,
        Some(&missing),
        1,
        &fixture.workspace,
        Focus::Latest,
    )
    .unwrap();
    let state = Rc::new(RefCell::new(State::default()));
    fallback.apply(&mut state.borrow_mut());
    assert_eq!(
        state.borrow().pane(1).unwrap().summary.coordinators[0].title,
        "root-chat"
    );
    assert!(fallback.repo_recency.is_empty());
    assert_eq!(tree_snapshot(fixture.root.path()), before);
    let mut file: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixture.config.join("workspaces.json")).unwrap())
            .unwrap();
    file["workspaces"][0]["chats"] = serde_json::json!([]);
    std::fs::write(
        fixture.config.join("workspaces.json"),
        serde_json::to_vec(&file).unwrap(),
    )
    .unwrap();
    let cwd = std::path::Path::new(&fixture.workspace.dir);
    indexed_transcript(
        &home,
        "historical",
        cwd,
        "Historical folder chat",
        "Old request",
        "2026-11-20T11:00:00Z",
    );
    indexed_transcript(
        &home,
        "root-chat",
        cwd,
        "Open chat title",
        "New request",
        "2026-11-20T12:00:00Z",
    );
    indexed_transcript(
        &home,
        "elsewhere",
        &fixture.repo,
        "Other workspace",
        "Other",
        "2026-11-20T13:00:00Z",
    );
    let before = tree_snapshot(fixture.root.path());
    for _ in 0..2 {
        let data = crate::data::IcedWorkData::read_with_home(
            &fixture.config,
            Some(&home),
            1,
            &fixture.workspace,
            Focus::Latest,
        )
        .unwrap();
        let state = Rc::new(RefCell::new(State::default()));
        data.apply(&mut state.borrow_mut());
        let mut sidebar = WorkSidebar::new(data.facts, state);
        // Fixed timestamp fixtures remain visible after the recent window expires.
        sidebar.open.insert(Fold::Older);
        assert_eq!(
            sidebar
                .rows()
                .iter()
                .filter(|r| r.label.starts_with("✦ "))
                .map(|r| r.label.as_str())
                .collect::<Vec<_>>(),
            ["✦ Open chat title", "✦ Historical folder chat"]
        );
    }
    assert_eq!(tree_snapshot(fixture.root.path()), before);
}
