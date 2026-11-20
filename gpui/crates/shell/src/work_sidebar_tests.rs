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
