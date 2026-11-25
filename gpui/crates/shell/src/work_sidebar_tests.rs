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

pub(crate) fn branch(repo: &str, name: &str, gone: bool) -> BranchStatus {
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

pub(super) fn fixture(owned: bool) -> WorkSidebar {
    fixture_with_branch(owned, "tru-274")
}

fn fixture_with_branch(owned: bool, first_branch: &str) -> WorkSidebar {
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
                branch("/repos/api", first_branch, false),
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
            "Focus: Ship parity ▾",
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
fn ownership_summary_uses_small_muted_proportional_row_style() {
    let sidebar = fixture(true);
    let ownership = sidebar
        .rows()
        .into_iter()
        .find(|row| row.label.starts_with("owns "))
        .unwrap();
    assert!(ownership.small);
    assert!(ownership.quiet);
}

#[test]
fn repos_count_and_rows_exclude_the_docs_root() {
    let mut sidebar = fixture(false);
    sidebar.workspace.repos.push(RepoFacts {
        dir: PathBuf::from("/work/docs"),
        local: true,
        run_command: None,
    });
    let rows = sidebar.rows();
    let repos = rows.iter().find(|row| row.label == "Repos").unwrap();
    assert_eq!(repos.badges, ["4"]);
    assert!(!rows.iter().any(|row| row.label == "docs"));
}

#[gpui::test]
fn chat_title_column_never_overlaps_right_aligned_metadata(cx: &mut gpui::TestAppContext) {
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let sidebar = fixture(false);
    let chat_row = sidebar
        .rows()
        .iter()
        .position(|row| row.label == "✦ Ship parity")
        .unwrap();
    assert_eq!(chat_row, 9, "fixture chat row index changed");
    let (_, cx) = cx.add_window_view(|_, _| sidebar);
    cx.simulate_resize(gpui::size(px(260.), px(700.)));
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let title = cx.debug_bounds("work-label-9").unwrap();
    let open = cx.debug_bounds("work-badge-9-0").unwrap();
    assert!(
        title.right() <= open.left(),
        "title {title:?} overlaps open {open:?}"
    );
    assert!(
        title.size.height <= px(32.),
        "title exceeds two lines: {title:?}"
    );
}

pub(super) fn draw_sidebar(cx: &mut gpui::VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    cx.run_until_parked();
}

pub(super) fn click_sidebar(cx: &mut gpui::VisualTestContext, selector: &'static str) {
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("missing {selector}"));
    cx.simulate_click(bounds.center(), gpui::Modifiers::none());
    draw_sidebar(cx);
}

pub(super) fn subscribe_outcomes(
    sidebar: &Entity<WorkSidebar>,
    cx: &mut gpui::VisualTestContext,
) -> Rc<RefCell<Vec<WorkSidebarOutcome>>> {
    let events = Rc::new(RefCell::new(vec![]));
    let received = events.clone();
    cx.update(|_, cx| {
        cx.subscribe(sidebar, move |_, event: &WorkSidebarOutcome, _| {
            received.borrow_mut().push(event.clone());
        })
        .detach();
    });
    events
}

fn replace_entries(sidebar: &WorkSidebar, entries: Vec<ChatIndexEntry>) {
    let mut state = sidebar.state.borrow_mut();
    let (stamp, summary) = state
        .reduce(Intent::IndexChanged {
            visible: Some(7),
            wall_time: SystemTime::now(),
        })
        .into_iter()
        .find_map(|e| match e {
            Effect::BuildSummary { stamp, plan } => Some((stamp, plan.build(&entries))),
            _ => None,
        })
        .unwrap();
    state.reduce(Intent::SummaryFinished {
        stamp,
        result: Box::new(Ok(summary)),
        now: Instant::now(),
        visible: false,
    });
}

#[gpui::test]
fn w30_more_in_find_switches_host_mode_preserving_filter_without_writes_or_sessions(
    cx: &mut gpui::TestAppContext,
) {
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let root = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let fixture = fixture(false);
    replace_entries(
        &fixture,
        (0..14)
            .map(|i| entry(&format!("chat-{i}"), "Chat", SystemTime::now()))
            .collect(),
    );
    let (sidebar, cx) = cx.add_window_view(|_, _| fixture);
    let shell = cx.update(|window, cx| {
        cx.new(|cx| {
            let mut model = crate::WorkspaceStatus::default();
            model.add(root.path().to_path_buf(), Instant::now());
            let owner = cx.new(|cx| crate::WorkspaceOwner::new(model, None, cx));
            crate::Shell::with_owner(
                Rc::new(crate::data::LocalData(owner.clone())),
                owner,
                config.path(),
                true,
                window,
                cx,
            )
            .unwrap()
        })
    });
    cx.update(|window, cx| {
        shell.update(cx, |shell, cx| {
            shell
                .find_state
                .borrow_mut()
                .reduce(gitterm_core::find::state::Intent::SetFilter(
                    gitterm_core::find::state::Filter {
                        query: "keep query".into(),
                        ..Default::default()
                    },
                ));
            shell.workspaces[0].store.regions.sidebar_collapsed = true;
            cx.subscribe_in(
                &sidebar,
                window,
                |shell, _, outcome: &WorkSidebarOutcome, window, cx| {
                    shell.work_outcome(outcome, window, cx)
                },
            )
            .detach();
        })
    });
    let events = subscribe_outcomes(&sidebar, cx);
    let before = tree_snapshot(root.path());
    let config_before = tree_snapshot(config.path());
    let filter = shell.read_with(cx, |s, _| s.find_state.borrow().filter().clone());
    cx.simulate_resize(gpui::size(px(640.), px(1600.)));
    draw_sidebar(cx);
    click_row(&sidebar, cx, "… 2 more in Find");
    assert_eq!(*events.borrow(), [WorkSidebarOutcome::OpenFind]);
    shell.read_with(cx, |s, cx| {
        assert_eq!(
            s.active(cx).store.regions.sidebar_mode,
            crate::layout::SidebarMode::Find
        );
        assert!(!s.active(cx).store.regions.sidebar_collapsed);
        assert_eq!(s.find_state.borrow().filter(), &filter);
        assert!(s.workspaces.iter().all(|w| w.sessions.is_empty()));
        assert!(s.owner.read(cx).chats.is_empty());
    });
    assert_eq!(tree_snapshot(root.path()), before);
    assert_eq!(tree_snapshot(config.path()), config_before);
}

#[gpui::test]
fn w27_document_copies_suffix_tracks_fold_without_opening_a_document(
    cx: &mut gpui::TestAppContext,
) {
    use gitterm_core::work::documents::{
        CopySource, Document, DocumentCopy, ScanRequest, ScanResult,
    };
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let root = tempfile::tempdir().unwrap();
    let fixture = fixture(false);
    {
        let mut state = fixture.state.borrow_mut();
        state.reduce(Intent::SetInputs(Inputs {
            workspace: 7,
            source: Source::Folder("/work".into()),
            tasks: vec![],
            focus: Focus::Latest,
            documents: Some(ScanRequest::default()),
        }));
        let stamp = state
            .reduce(Intent::ScanDocuments(7))
            .into_iter()
            .find_map(|e| match e {
                Effect::ScanDocuments { stamp, .. } => Some(stamp),
                _ => None,
            })
            .unwrap();
        state.reduce(Intent::DocumentsFinished {
            stamp,
            result: Ok(ScanResult {
                documents: vec![Document {
                    key: "doc".into(),
                    title: "Decision".into(),
                    rel: "docs/DECISIONS.md".into(),
                    differ: true,
                    copies: ["new.md", "old.md"]
                        .into_iter()
                        .map(|name| DocumentCopy {
                            path: root.path().join(name),
                            source: CopySource::WorkDocs,
                            modified: SystemTime::now(),
                        })
                        .collect(),
                }],
                truncated: false,
            }),
            now: Instant::now(),
        });
    }
    let before = tree_snapshot(root.path());
    let (sidebar, cx) = cx.add_window_view(|_, _| fixture);
    let events = subscribe_outcomes(&sidebar, cx);
    cx.simulate_resize(gpui::size(px(640.), px(1300.)));
    draw_sidebar(cx);
    click_row(&sidebar, cx, "Documents");
    click_control(&sidebar, cx, "Decision", "2 copies differ ▸");
    assert!(sidebar.read_with(cx, |s, _| {
        s.rows()
            .iter()
            .any(|r| r.controls.iter().any(|c| c.label == "2 copies differ ▾"))
    }));
    click_control(&sidebar, cx, "Decision", "2 copies differ ▾");
    assert!(sidebar.read_with(cx, |s, _| {
        s.rows()
            .iter()
            .any(|r| r.controls.iter().any(|c| c.label == "2 copies differ ▸"))
    }));
    assert!(
        events.borrow().is_empty(),
        "folds cannot launch a process or open a document"
    );
    assert_eq!(tree_snapshot(root.path()), before);
}

#[gpui::test]
fn w03_and_w15_index_and_owned_chat_only_notice(cx: &mut gpui::TestAppContext) {
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let root = tempfile::tempdir().unwrap();
    let fixture = fixture(true);
    let mut chat = entry("recent", "Unindexed chat", SystemTime::now());
    chat.facts = None;
    replace_entries(&fixture, vec![chat]);
    let (sidebar, cx) = cx.add_window_view(|_, _| fixture);
    let events = subscribe_outcomes(&sidebar, cx);
    let before = tree_snapshot(root.path());
    cx.simulate_resize(gpui::size(px(640.), px(1300.)));
    draw_sidebar(cx);
    let label = sidebar.read_with(cx, |s, _| {
        s.rows()
            .into_iter()
            .find(|r| r.label.starts_with("No chat facts"))
            .unwrap()
            .label
    });
    click_control(&sidebar, cx, &label, "Index older chats");
    assert_eq!(
        sidebar.read_with(cx, |s, _| s.error.clone()).as_deref(),
        Some("Index older chats · not available in preview")
    );
    click_row(&sidebar, cx, "✦ Unindexed chat");
    assert_eq!(
        sidebar.read_with(cx, |s, _| s.error.clone()).as_deref(),
        Some("✦ Unindexed chat · not available in preview")
    );
    assert!(events.borrow().is_empty());
    assert_eq!(tree_snapshot(root.path()), before);
}

fn click_row(sidebar: &Entity<WorkSidebar>, cx: &mut gpui::VisualTestContext, label: &str) {
    let index = sidebar.read_with(cx, |sidebar, _| {
        sidebar
            .rows()
            .iter()
            .position(|row| row.label == label)
            .unwrap()
    });
    click_sidebar(
        cx,
        Box::leak(format!("work-label-{index}").into_boxed_str()),
    );
}

pub(super) fn click_control(
    sidebar: &Entity<WorkSidebar>,
    cx: &mut gpui::VisualTestContext,
    row_label: &str,
    label: &str,
) {
    let selector = sidebar.read_with(cx, |sidebar, _| {
        let rows = sidebar.rows();
        let (index, row) = rows
            .iter()
            .enumerate()
            .find(|(_, row)| row.label == row_label)
            .unwrap();
        if let Some(control) = row.controls.iter().position(|c| c.label == label) {
            format!("work-control-{index}-{control}")
        } else if let Some(control) = row.detail_controls.iter().position(|c| c.label == label) {
            format!("work-detail-{index}-{control}")
        } else {
            let (detail, control) = row
                .detail_rows
                .iter()
                .enumerate()
                .find_map(|(i, row)| {
                    row.controls
                        .iter()
                        .position(|c| c.label == label)
                        .map(|c| (i, c))
                })
                .unwrap();
            format!("work-inline-{index}-{detail}-{control}")
        }
    });
    click_sidebar(cx, Box::leak(selector.into_boxed_str()));
}

#[gpui::test]
fn w12_branch_body_emits_review_and_disclosure_only_folds(cx: &mut gpui::TestAppContext) {
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let root = tempfile::tempdir().unwrap();
    let before = tree_snapshot(root.path());
    let mut fixture = fixture(false);
    fixture.open.insert(Fold::Repo("/repos/api".into()));
    let (sidebar, cx) = cx.add_window_view(|_, _| fixture);
    let events = subscribe_outcomes(&sidebar, cx);
    cx.simulate_resize(gpui::size(px(640.), px(1200.)));
    draw_sidebar(cx);
    let index = sidebar.read_with(cx, |s, _| {
        s.rows().iter().position(|r| r.label == "tru-274").unwrap()
    });
    click_sidebar(cx, Box::leak(format!("work-fold-{index}").into_boxed_str()));
    assert!(
        events.borrow().is_empty(),
        "disclosure must never open review or launch"
    );
    assert!(sidebar.read_with(cx, |s, _| {
        s.open.contains(&Fold::Branch("/repos/api-tru-274".into()))
    }));
    click_row(&sidebar, cx, "tru-274");
    assert_eq!(
        *events.borrow(),
        [WorkSidebarOutcome::OpenReview {
            checkout: "/repos/api-tru-274".into(),
            vs_base: false
        }]
    );
    let s = sidebar.read_with(cx, |s, _| {
        let mut clean = branch("/repos/api", "clean", false);
        clean.status.as_mut().unwrap().files = 0;
        clean.base = Some(gitterm_core::git_facts::BaseChoice {
            reference: "main".into(),
            source: gitterm_core::git_facts::BaseSource::Local,
            considered: 1,
        });
        s.branch_row(&clean).action
    });
    assert_eq!(
        s,
        Some(Action::Emit(WorkSidebarOutcome::OpenReview {
            checkout: "/repos/api-clean".into(),
            vs_base: true
        }))
    );
    assert_eq!(tree_snapshot(root.path()), before);
}

#[gpui::test]
fn w25_focus_menu_uses_iced_suffix_choices_and_closes_on_pick(cx: &mut gpui::TestAppContext) {
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let root = tempfile::tempdir().unwrap();
    let before = tree_snapshot(root.path());
    let (sidebar, cx) = cx.add_window_view(|_, _| fixture(false));
    let events = subscribe_outcomes(&sidebar, cx);
    cx.simulate_resize(gpui::size(px(640.), px(1200.)));
    draw_sidebar(cx);
    click_row(&sidebar, cx, "Focus: Ship parity ▾");
    assert!(events.borrow().is_empty());
    assert!(sidebar.read_with(cx, |s, _| {
        s.rows().iter().any(|r| r.label == "Focus: Ship parity ▴")
    }));
    click_row(&sidebar, cx, "All recent work · 1 chats, last 7 days");
    assert_eq!(
        *events.borrow(),
        [WorkSidebarOutcome::Focus {
            workspace: 7,
            focus: Focus::AllRecent
        }]
    );
    assert!(!sidebar.read_with(cx, |s, _| s.open.contains(&Fold::Focus)));
    click_row(&sidebar, cx, "Focus: Ship parity ▾");
    click_row(&sidebar, cx, "Most recently active chat");
    assert_eq!(
        events.borrow().last(),
        Some(&WorkSidebarOutcome::Focus {
            workspace: 7,
            focus: Focus::Latest
        })
    );
    assert_eq!(tree_snapshot(root.path()), before);
}

#[gpui::test]
fn preview_write_controls_only_show_the_exact_clicked_label(cx: &mut gpui::TestAppContext) {
    use gitterm_core::{
        merge_plan::{PrKey, PrState},
        prs::{PullCore, WatchedPr},
    };
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let root = tempfile::tempdir().unwrap();
    let before = tree_snapshot(root.path());
    let mut fixture = fixture(false);
    for dir in ["home", "tools", "api"] {
        fixture
            .open
            .insert(Fold::Repo(format!("/repos/{dir}").into()));
    }
    fixture
        .open
        .insert(Fold::Branch("/repos/api-tru-274".into()));
    fixture.open.insert(Fold::Chat("recent".into()));
    let mut pr = WatchedPr::new(PrKey::new("example/gitterm", 42));
    pr.snapshot.core = Some(PullCore {
        title: "Draft change".into(),
        state: Some(PrState::Draft),
        ..Default::default()
    });
    pr.news.push(gitterm_core::prs::NewsItem {
        key: "news".into(),
        kind: gitterm_core::prs::NewsKind::ChecksFailed,
        text: "checks failed".into(),
        at: "2026-11-20T00:00:00Z".into(),
        dismissed: false,
        told: true,
        approvals_only: false,
    });
    fixture.pull_requests.push(PullRequestGroup {
        root: "root".into(),
        title: "Chat".into(),
        prs: vec![pr],
    });
    fixture.open.insert(Fold::Pr("example/gitterm#42".into()));
    let (sidebar, cx) = cx.add_window_view(|_, _| fixture);
    let events = subscribe_outcomes(&sidebar, cx);
    cx.simulate_resize(gpui::size(px(800.), px(2000.)));
    draw_sidebar(cx);
    let folds = sidebar.read_with(cx, |s, _| s.open.clone());
    for (row, label) in [
        ("Parity", "✎"),
        ("Repos", "+ Add repo…"),
        ("tools added to this workspace's repos", "Undo"),
        ("tools added to this workspace's repos", "×"),
        ("tools", "×"),
        ("home", "Open in editor"),
        ("home", "✎"),
        ("tools", "+ run command"),
        ("tru-274", "Open in editor"),
        ("✦ Ship parity", "↗"),
        ("✦ Ship parity", "⊞ Make workspace"),
        ("Draft change", "Review"),
        ("Draft change", "×"),
    ] {
        click_control(&sidebar, cx, row, label);
        sidebar.read_with(cx, |s, _| {
            assert_eq!(
                s.error.as_deref(),
                Some(format!("{label} · not available in preview").as_str())
            );
            assert_eq!(s.open, folds);
            assert!(s.editing.is_none());
            assert!(s.dismissed_auto.is_empty());
        });
        assert!(
            events.borrow().is_empty(),
            "{label} must not emit a process/write/navigation outcome"
        );
        assert_eq!(tree_snapshot(root.path()), before);
    }
    // Iced's disabled Approve and Merge do nothing: no panel, no notice.
    let notice = sidebar.read_with(cx, |s, _| s.error.clone());
    for label in ["Approve (draft)", "Merge (draft)"] {
        click_control(&sidebar, cx, "Draft change", label);
        sidebar.read_with(cx, |s, _| {
            assert!(s.confirm.is_none(), "{label} is disabled");
            assert_eq!(s.error, notice);
        });
    }
    sidebar.update(cx, |s, cx| {
        s.workspace
            .chat_owners
            .insert("recent".into(), "Other work".into());
        cx.notify();
    });
    draw_sidebar(cx);
    click_control(&sidebar, cx, "✦ Ship parity", "→ Go to Other work");
    assert!(events.borrow().is_empty());
    assert_eq!(
        sidebar.read_with(cx, |s, _| s.error.clone()).as_deref(),
        Some("→ Go to Other work · not available in preview")
    );
    assert_eq!(tree_snapshot(root.path()), before);
}

#[gpui::test]
fn w19a_w20_plan_merge_and_hold_controls_match_iced_placement_and_only_notice(
    cx: &mut gpui::TestAppContext,
) {
    use gitterm_core::{
        merge_plan::{PrKey, PrState},
        prs::{PullCore, WatchedPr},
    };
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let root = tempfile::tempdir().unwrap();
    let mut fixture = fixture(true);
    let mut own = WatchedPr::new(PrKey::new("example/gitterm", 1));
    own.snapshot.core = Some(PullCore {
        title: "Next change".into(),
        author: "fixture".into(),
        state: Some(PrState::Open),
        ..Default::default()
    });
    let mut held = WatchedPr::new(PrKey::new("example/gitterm", 2));
    held.snapshot.core = Some(PullCore {
        title: "Held change".into(),
        state: Some(PrState::Open),
        ..Default::default()
    });
    std::fs::write(
        root.path().join("github-accounts.json"),
        r#"{"EXAMPLE":" fixture "}"#,
    )
    .unwrap();
    fixture.pr_accounts = read_pr_accounts(root.path());
    fixture.pr_plans.insert("root".into(), serde_json::from_value(serde_json::json!({"root_chat_id":"root", "entries":[{"repo":"example/gitterm","number":1,"step":1},{"repo":"example/gitterm","number":2,"step":2,"hold":"wait for release"}]})).unwrap());
    fixture.pull_requests.push(PullRequestGroup {
        root: "root".into(),
        title: "Chat".into(),
        prs: vec![own, held],
    });
    for number in [1, 2] {
        fixture
            .open
            .insert(Fold::Pr(format!("example/gitterm#{number}")));
    }
    let next = fixture
        .rows()
        .into_iter()
        .find(|r| r.label == "Next change")
        .unwrap();
    let merge = next
        .controls
        .iter()
        .find(|c| c.label.starts_with("Merge"))
        .unwrap()
        .label
        .clone();
    assert_eq!(merge, "Merge");
    assert!(
        !next
            .detail_controls
            .iter()
            .any(|c| c.label.starts_with("Approve") || c.label.starts_with("Merge")),
        "own PR omits Approve; next Merge sits on the title line"
    );
    let held = fixture
        .rows()
        .into_iter()
        .find(|r| r.label == "Held change")
        .unwrap();
    assert_eq!(held.detail_rows[0].label, "hold: wait for release");
    let before = tree_snapshot(root.path());
    let (sidebar, cx) = cx.add_window_view(|_, _| fixture);
    let events = subscribe_outcomes(&sidebar, cx);
    cx.simulate_resize(gpui::size(px(800.), px(1500.)));
    draw_sidebar(cx);
    click_control(&sidebar, cx, "Held change", "clear hold");
    assert_eq!(
        sidebar.read_with(cx, |s, _| s.error.clone()).as_deref(),
        Some("clear hold · not available in preview")
    );
    // The next PR's title-line Merge opens the confirmation (Iced
    // `ActionOpen`) instead of a notice.
    click_control(&sidebar, cx, "Next change", merge.as_str());
    sidebar.read_with(cx, |s, _| {
        let confirm = s.confirm.as_ref().expect("Merge opens the panel");
        assert_eq!(confirm.kind, ActionKind::Merge);
        assert_eq!(confirm.key.number, 1);
    });
    sidebar.update(cx, |s, cx| s.confirm_close(cx));
    let notice = sidebar.read_with(cx, |s, _| s.error.clone());
    for (outcome, reason) in [
        (gitterm_core::prs::CheckOutcome::Pending, "checks running"),
        (gitterm_core::prs::CheckOutcome::Fail, "checks failed"),
    ] {
        sidebar.update(cx, |s, cx| {
            s.pull_requests[0].prs[1].snapshot.check_runs = vec![gitterm_core::prs::CheckItem {
                name: "test".into(),
                outcome,
            }];
            cx.notify();
        });
        draw_sidebar(cx);
        for action in ["Approve", "Merge"] {
            let label = format!("{action} ({reason})");
            click_control(&sidebar, cx, "Held change", &label);
            sidebar.read_with(cx, |s, _| {
                assert!(s.confirm.is_none(), "{label} is disabled");
                assert_eq!(s.error, notice);
            });
        }
    }
    assert!(
        events.borrow().is_empty(),
        "no merge/hold process or write can reach the host"
    );
    assert_eq!(
        sidebar
            .read_with(cx, |s, _| s.pr_plans["root"].entries[1].hold.clone())
            .as_deref(),
        Some("wait for release")
    );
    assert_eq!(tree_snapshot(root.path()), before);
}

#[gpui::test]
fn repo_name_and_disclosure_toggle_but_only_run_emits_command(cx: &mut gpui::TestAppContext) {
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let (sidebar, cx) = cx.add_window_view(|_, _| fixture(false));
    let events = Rc::new(RefCell::new(Vec::new()));
    let received = events.clone();
    cx.update(|_, cx| {
        cx.subscribe(&sidebar, move |_, outcome: &WorkSidebarOutcome, _| {
            received.borrow_mut().push(outcome.clone());
        })
        .detach();
    });
    cx.simulate_resize(gpui::size(px(640.), px(900.)));
    draw_sidebar(cx);
    let fold = Fold::Repo("/repos/home".into());
    assert!(!sidebar.read_with(cx, |sidebar, _| sidebar.open.contains(&fold)));
    assert_eq!(
        cx.debug_bounds("work-fold-3-▸").unwrap().size.width,
        px(24.)
    );
    assert!(cx.debug_bounds("work-control-3-0").is_none());

    // Empty header space and the full-width facts line are passive.
    let blank = cx.debug_bounds("work-space-3").unwrap();
    assert!(blank.size.width > px(20.));
    click_sidebar(cx, "work-space-3");
    click_sidebar(cx, "work-facts-3-0");
    assert!(!sidebar.read_with(cx, |sidebar, _| sidebar.open.contains(&fold)));
    assert!(events.borrow().is_empty());

    click_sidebar(cx, "work-name-3");
    assert!(sidebar.read_with(cx, |sidebar, _| sidebar.open.contains(&fold)));
    assert_eq!(
        cx.debug_bounds("work-fold-3-▾").unwrap().size.width,
        px(24.)
    );
    assert!(cx.debug_bounds("work-control-3-0").is_some());
    assert!(events.borrow().is_empty());
    click_sidebar(cx, "work-fold-3-▾");
    assert!(!sidebar.read_with(cx, |sidebar, _| sidebar.open.contains(&fold)));
    click_sidebar(cx, "work-fold-3-▸");
    assert!(sidebar.read_with(cx, |sidebar, _| sidebar.open.contains(&fold)));
    assert!(events.borrow().is_empty());

    click_sidebar(cx, "work-run-3");
    assert!(sidebar.read_with(cx, |sidebar, _| sidebar.open.contains(&fold)));
    // Run adds no banner row, so the repo row keeps its index.
    click_sidebar(cx, "work-name-3");
    assert!(!sidebar.read_with(cx, |sidebar, _| sidebar.open.contains(&fold)));
    click_sidebar(cx, "work-run-3");
    assert!(!sidebar.read_with(cx, |sidebar, _| sidebar.open.contains(&fold)));
    assert_eq!(
        *events.borrow(),
        vec![
            WorkSidebarOutcome::RunRepoCommand {
                repo: "/repos/home".into(),
            };
            2
        ]
    );
    click_sidebar(cx, "work-name-3");
    // Open in editor is still unavailable in the preview; its banner row
    // moves the repo row to index 4 from here on.
    click_sidebar(cx, "work-control-3-0");
    assert!(sidebar.read_with(cx, |sidebar, _| sidebar.open.contains(&fold)));
    assert_eq!(events.borrow().len(), 2, "editor launch is notice-only");
    assert_eq!(
        sidebar
            .read_with(cx, |sidebar, _| sidebar.error.clone())
            .as_deref(),
        Some("Open in editor · not available in preview")
    );
    click_sidebar(cx, "work-control-4-1");
    assert!(sidebar.read_with(cx, |sidebar, _| sidebar.editing.is_none()));
    assert!(sidebar.read_with(cx, |sidebar, _| sidebar.open.contains(&fold)));
    assert!(cx.debug_bounds("work-run-4").is_some());
    assert!(cx.debug_bounds("work-run-command-input").is_none());
    assert_eq!(
        sidebar
            .read_with(cx, |sidebar, _| sidebar.error.clone())
            .as_deref(),
        Some("✎ · not available in preview")
    );
    assert_eq!(events.borrow().len(), 2);
}

#[gpui::test]
fn expanded_repo_disclosure_stays_inside_default_sidebar_and_collapses(
    cx: &mut gpui::TestAppContext,
) {
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let repo = PathBuf::from("/repos/api");
    let fold = Fold::Repo(repo.clone());
    let (sidebar, cx) = cx.add_window_view(|_, _| {
        let mut sidebar = fixture(false);
        sidebar.workspace.repos[0].dir = repo.clone();
        sidebar
    });
    cx.simulate_resize(gpui::size(
        px(crate::layout::RegionLayout::default().sidebar_width),
        px(900.),
    ));
    for command in [Some("cargo run --example preview".to_string()), None] {
        let has_command = command.is_some();
        sidebar.update(cx, |sidebar, cx| {
            sidebar.workspace.repos[0].run_command = command;
            sidebar.open.insert(fold.clone());
            cx.notify();
        });
        draw_sidebar(cx);
        let index = sidebar.read_with(cx, |sidebar, _| {
            let rows = sidebar.rows();
            let index = rows.iter().position(|row| row.label == "api").unwrap();
            assert_eq!(rows[index].controls.len(), 3, "editor, run edit, remove");
            assert!(rows[index].age.is_some());
            index
        });
        let selector: &'static str = Box::leak(format!("work-fold-{index}-▾").into_boxed_str());
        let disclosure = cx.debug_bounds(selector).unwrap();
        let sidebar_bounds = cx.debug_bounds("work-sidebar").unwrap();
        assert_eq!(disclosure.size.width, px(24.));
        assert!(disclosure.left() >= sidebar_bounds.left());
        assert!(disclosure.right() <= sidebar_bounds.right());
        assert_eq!(disclosure.right(), sidebar_bounds.right() - px(12.));
        let editor = cx
            .debug_bounds(Box::leak(
                format!("work-control-{index}-0").into_boxed_str(),
            ))
            .unwrap();
        // Iced clips the actions slot at the disclosure; Open in editor stays one line.
        assert!(editor.size.height <= px(24.), "{editor:?}");
        if has_command {
            let run = cx
                .debug_bounds(Box::leak(format!("work-run-{index}").into_boxed_str()))
                .unwrap();
            assert!(
                run.right() <= disclosure.left(),
                "command truncates, never overlaps"
            );
        }
        click_sidebar(cx, selector);
        assert!(!sidebar.read_with(cx, |sidebar, _| sidebar.open.contains(&fold)));
    }
}

#[gpui::test]
fn pr_number_click_requests_the_pr_page_in_the_pr_store(cx: &mut gpui::TestAppContext) {
    use gitterm_core::{merge_plan::PrKey, prs::WatchedPr};
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let root = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let mut sidebar = fixture(true);
    sidebar.pull_requests = vec![PullRequestGroup {
        root: "root".into(),
        title: "Owning chat".into(),
        prs: vec![WatchedPr::new(PrKey::new("example/gitterm", 42))],
    }];
    sidebar.pr_accounts = BTreeMap::from([("example".into(), "Sample-Login".into())]);
    let (sidebar, cx) = cx.add_window_view(|_, _| sidebar);
    let shell = cx.update(|window, cx| {
        cx.new(|cx| {
            let mut model = crate::WorkspaceStatus::default();
            model.add(root.path().to_path_buf(), Instant::now());
            let owner = cx.new(|cx| crate::WorkspaceOwner::new(model, None, cx));
            crate::Shell::with_owner(
                Rc::new(crate::data::LocalData(owner.clone())),
                owner,
                config.path(),
                true,
                window,
                cx,
            )
            .unwrap()
        })
    });
    cx.update(|window, cx| {
        shell.update(cx, |_, cx| {
            cx.subscribe_in(
                &sidebar,
                window,
                |shell, _, outcome: &WorkSidebarOutcome, window, cx| {
                    shell.work_outcome(outcome, window, cx)
                },
            )
            .detach();
        })
    });
    cx.simulate_resize(gpui::size(px(640.), px(900.)));
    draw_sidebar(cx);
    click_sidebar(cx, "work-pr-number-2");
    assert_eq!(
        crate::page_tests::page(&shell, cx),
        Some((
            "https://github.com/example/gitterm/pull/42".into(),
            gitterm_core::page_store::PageStore::GitHub("sample-login".into())
        ))
    );
    assert!(
        cx.opened_url().is_none(),
        "the PR opens embedded, not externally"
    );
}

#[test]
fn issue_rows_with_a_url_open_the_page_in_the_workspace_store() {
    use gitterm_core::{issues::CachedIssue, page_store::PageStore};
    let mut sidebar = fixture(true);
    let issue = |url: Option<&str>, workspace: Option<&str>| CachedIssue {
        title: Some("Sample issue".into()),
        url: url.map(str::to_owned),
        workspace: workspace.map(str::to_owned),
        ..Default::default()
    };
    for (key, cached) in [
        (
            "SAMPLE-1",
            issue(
                Some("https://linear.app/sample-a/issue/SAMPLE-1/x"),
                Some("sample-b"),
            ),
        ),
        (
            "SAMPLE-2",
            issue(Some("https://linear.app/sample-a/issue/SAMPLE-2/y"), None),
        ),
        ("SAMPLE-3", issue(Some("https://example.com/issue/3"), None)),
        ("SAMPLE-4", issue(None, Some("sample-b"))),
    ] {
        sidebar.issues.issues.insert(key.into(), cached);
    }
    let keys: Vec<String> = (1..=4).map(|n| format!("SAMPLE-{n}")).collect();
    let mut rows = vec![];
    sidebar.issue_rows(&keys, &mut rows);
    let action = |key: &str| {
        rows.iter()
            .find(|row| row.label.starts_with(key))
            .unwrap_or_else(|| panic!("missing {key}"))
            .action
            .clone()
    };
    let page = |url: &str, store| {
        Some(Action::Emit(WorkSidebarOutcome::OpenPage {
            url: url.into(),
            store,
        }))
    };
    // The resolving workspace wins; older caches fall back to the URL's workspace.
    assert_eq!(
        action("SAMPLE-1"),
        page(
            "https://linear.app/sample-a/issue/SAMPLE-1/x",
            PageStore::Linear("sample-b".into())
        )
    );
    assert_eq!(
        action("SAMPLE-2"),
        page(
            "https://linear.app/sample-a/issue/SAMPLE-2/y",
            PageStore::Linear("sample-a".into())
        )
    );
    assert_eq!(
        action("SAMPLE-3"),
        page("https://example.com/issue/3", PageStore::Default)
    );
    assert_eq!(action("SAMPLE-4"), None, "unknown URL stays text");
}

#[gpui::test]
fn pr_body_and_disclosure_keep_iced_toggle_targets(cx: &mut gpui::TestAppContext) {
    use gitterm_core::{merge_plan::PrKey, prs::WatchedPr};
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let root = tempfile::tempdir().unwrap();
    let before = tree_snapshot(root.path());
    let mut sidebar = fixture(true);
    sidebar.pull_requests = vec![PullRequestGroup {
        root: "root".into(),
        title: "Owning chat".into(),
        prs: vec![WatchedPr::new(PrKey::new("example/gitterm", 42))],
    }];
    sidebar.pr_accounts = BTreeMap::from([("Example".into(), "Sample-Login".into())]);
    let (sidebar, cx) = cx.add_window_view(|_, _| sidebar);
    cx.simulate_resize(gpui::size(px(640.), px(900.)));
    draw_sidebar(cx);
    let fold = Fold::Pr("example/gitterm#42".into());
    assert!(cx.debug_bounds("work-fold-2-▸").is_some());
    let events = subscribe_outcomes(&sidebar, cx);
    click_sidebar(cx, "work-pr-number-2");
    assert!(!sidebar.read_with(cx, |sidebar, _| sidebar.open.contains(&fold)));
    assert_eq!(
        *events.borrow(),
        [WorkSidebarOutcome::OpenPage {
            url: "https://github.com/example/gitterm/pull/42".into(),
            store: gitterm_core::page_store::PageStore::GitHub("sample-login".into()),
        }]
    );
    assert!(
        cx.opened_url().is_none(),
        "PR number requests the Page viewer, never an external process"
    );
    click_sidebar(cx, "work-label-2");
    assert!(sidebar.read_with(cx, |sidebar, _| sidebar.open.contains(&fold)));
    assert!(cx.debug_bounds("work-fold-2-▾").is_some());
    click_sidebar(cx, "work-fold-2-▾");
    assert!(!sidebar.read_with(cx, |sidebar, _| sidebar.open.contains(&fold)));
    click_sidebar(cx, "work-fold-2-▸");
    assert!(sidebar.read_with(cx, |sidebar, _| sidebar.open.contains(&fold)));
    assert_eq!(
        events.borrow().len(),
        1,
        "only the separate number link emits navigation"
    );
    assert_eq!(tree_snapshot(root.path()), before);
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
        sidebar
            .rows()
            .iter()
            .any(|row| row.label.contains("added to this workspace")),
        "preview dismiss must preserve the notice"
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
        if owned {
            assert_eq!(row.action, Some(Action::Preview(row.label.clone())));
        } else {
            assert_eq!(
                sidebar.activate(row.action.unwrap()),
                Some(WorkSidebarOutcome::Focus {
                    workspace: 7,
                    focus: Focus::Coordinator("recent".into())
                })
            );
        }
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
        crate::data::IcedWorkData::read(
            &self.config,
            &crate::data::TaskSource::iced(&self.config),
            1,
            &self.workspace,
            Focus::Latest,
            None,
            |_| {},
        )
        .unwrap()
    }
}

pub(super) fn tree_snapshot(
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
    let home = fixture.root.path().join("harness-home");
    indexed_transcript(
        &home,
        "root-chat",
        std::path::Path::new(&fixture.workspace.dir),
        "Fixture chat title",
        "Fixture request",
        "2026-11-20T12:00:00Z",
    );
    let load = || {
        crate::data::IcedWorkData::read_with_home(
            &fixture.config,
            Some(&home),
            1,
            &fixture.workspace,
            Focus::Latest,
        )
        .unwrap()
    };
    let before = tree_snapshot(fixture.root.path());
    let data = load();
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
            "Chats",
            "Documents",
            "Tasks",
            "Workers",
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
    load();
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
    let result = crate::data::IcedWorkData::read(
        &fixture.config,
        &crate::data::TaskSource::iced(&fixture.config),
        1,
        &fixture.workspace,
        Focus::Latest,
        None,
        |_| {},
    );
    assert!(result.err().unwrap().to_string().contains("task store"));
    assert_eq!(tree_snapshot(fixture.root.path()), before);
}

#[gpui::test]
fn write_outcomes_report_unavailable_without_emitting_events(cx: &mut gpui::TestAppContext) {
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
    assert!(
        events.borrow().is_empty(),
        "preview notices cannot reach host write subscribers"
    );
    assert!(
        WorkSidebarOutcome::Refresh { workspace: 7 }
            .preview_error()
            .is_none()
    );
    // The shell carries these out, as Iced does: no "not available" banner.
    for handled in [
        WorkSidebarOutcome::RunRepoCommand {
            repo: "/repos/tools".into(),
        },
        WorkSidebarOutcome::OpenTask {
            task: "task".into(),
        },
    ] {
        assert!(handled.preview_error().is_none(), "{handled:?}");
    }
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
    sidebar.update(cx, |sidebar, cx| {
        sidebar.load(false, cx);
        assert!(sidebar.loading);
        assert_eq!(
            sidebar.workspace.repos.len(),
            2,
            "saved repos must precede all background I/O"
        );
        assert_eq!(
            sidebar.workspace.repos[1].run_command.as_deref(),
            Some("cargo run")
        );
    });
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
    // The shell runs it, as Iced does; the sidebar must not claim otherwise.
    owner.read_with(cx, |owner, _| assert_eq!(owner.error, None));
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
    // Without transcripts or an earlier index, owned roots are omitted from Chats.
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
    assert!(state.pane(1).unwrap().summary.coordinators.is_empty());
    assert_eq!(tree_snapshot(fixture.root.path()), before);
}

#[test]
fn folder_work_includes_historical_chats_and_missing_index_omits_unknown_chats() {
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
    assert!(
        state
            .borrow()
            .pane(1)
            .unwrap()
            .summary
            .coordinators
            .is_empty()
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

// Install the production read-only projection before a headless host's first draw.
pub(crate) fn h8_install(sidebar: &mut WorkSidebar, data: crate::data::IcedWorkData) {
    data.apply(&mut sidebar.state.borrow_mut());
    sidebar.previous_data = Some(data.clone());
    sidebar.workspace = data.facts;
    sidebar.tasks = data.tasks;
    sidebar.workers = data.workers;
    sidebar.repo_recency = data.repo_recency;
    sidebar.loaded_at = Some(Instant::now());
}

#[test]
fn realistic_catalog_partial_index_publishes_repos_chats_docs_before_branch_checks() {
    use gitterm_core::{chat_facts::CheckoutTouch, tasks::*};
    let fixture = IcedFixture::new();
    let home = fixture.root.path().join("harness-home");
    let cwd = PathBuf::from(&fixture.workspace.dir);
    indexed_transcript(
        &home,
        "root-chat",
        &cwd,
        "Root title",
        "Latest request",
        "2026-11-20T12:00:00Z",
    );
    indexed_transcript(
        &home,
        "unrelated",
        &fixture.root.path().join("other"),
        "Unrelated title",
        "Elsewhere",
        "2026-11-20T13:00:00Z",
    );
    let path = fixture.config.join("workspaces.json");
    let mut file: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let selected = &mut file["workspaces"][0];
    selected["home"] = serde_json::json!(cwd);
    selected["active"] = serde_json::json!(true);
    selected["active_tab"] = serde_json::json!(2);
    selected["repos_added"] = serde_json::json!([fixture.repo]);
    selected["repos_removed"] = serde_json::json!([]);
    selected["chats"] = serde_json::json!([
        "root-chat",
        "owned-2",
        "owned-3",
        "owned-4",
        "owned-5",
        "owned-6",
        "owned-7"
    ]);
    selected["tabs"] = serde_json::json!([
        {"dir":cwd,"repo_dir":fixture.repo,"tab_kind":"agent","agent_config":{"backend":"claude","model":"default"},"chat_session_id":"owned-2","session_uid":"tab-2"},
        {"dir":cwd,"repo_dir":fixture.repo,"tab_kind":"agent","agent_config":{"backend":"claude","model":"default"},"chat_session_id":"owned-3","session_uid":"tab-3"},
        {"dir":cwd,"repo_dir":fixture.repo,"tab_kind":"agent","agent_config":{"backend":"claude","model":"default"},"chat_session_id":"root-chat","session_uid":"parent"}
    ]);
    // The selected named work is neither index 0 nor identified by its folder:
    // a second work shares that folder, and closed entries are also persisted.
    let work = selected.clone();
    let mut shared = work.clone();
    shared["name"] = serde_json::json!("Same folder, different work");
    shared["chats"] = serde_json::json!([]);
    let mut closed = shared.clone();
    closed["active"] = serde_json::json!(false);
    file["workspaces"] = serde_json::json!([shared.clone(), closed, shared.clone(), shared, work]);
    file["active_workspace"] = serde_json::json!(3);
    std::fs::write(&path, serde_json::to_vec(&file).unwrap()).unwrap();
    std::fs::write(fixture.config.join("settings.json"), br#"{"theme":"dark","pr_watch":{"auto_tell":true},"chat":{"default_model":"default"},"console_expanded":false}"#).unwrap();
    let facts_dir = gitterm_core::chat_facts::facts_dir(&fixture.config);
    let facts_path =
        gitterm_core::chat_facts::facts_path(&facts_dir, ChatBackend::Claude, "root-chat").unwrap();
    let mut facts = gitterm_core::chat_facts::load_facts(&facts_path)
        .unwrap()
        .unwrap();
    facts.checkouts.extend((0..178).map(|index| CheckoutTouch {
        repo: fixture.repo.clone(),
        dir: fixture.root.path().join(format!("gone-checkout-{index}")),
        branch: Some(format!("task/test-42-fixture-{index}")),
        edits: 1,
        last: "2026-11-20T12:00:00Z".into(),
    }));
    facts.prs.push(gitterm_core::chat_facts::PrRef {
        repo: "example/project".into(),
        number: 42,
        url: "https://github.com/example/project/pull/42".into(),
    });
    facts.issue_keys = vec!["TEST-42".into()];
    std::fs::write(facts_path, serde_json::to_vec(&facts).unwrap()).unwrap();
    std::fs::write(fixture.config.join("issue-titles.json"), br#"{"issues":{"TEST-42":{"title":"Synthetic issue","state":"In Progress","fetched_at":"2026-11-20T12:00:00Z"}}}"#).unwrap();
    let mut watch = gitterm_core::prs::WatchFile::default();
    let mut pr = gitterm_core::prs::WatchedPr::new(gitterm_core::merge_plan::PrKey::new(
        "example/project",
        42,
    ));
    pr.snapshot.core = Some(gitterm_core::prs::PullCore {
        title: "Synthetic pull request".into(),
        state: Some(gitterm_core::merge_plan::PrState::Draft),
        mergeable: Some(false),
        ..Default::default()
    });
    watch.prs.insert(pr.key.id(), pr);
    std::fs::write(
        gitterm_core::prs::WatchFile::path_in(&fixture.config),
        serde_json::to_vec(&watch).unwrap(),
    )
    .unwrap();
    // Preserve the full persisted task/delegation wire shape, not a rows-only mock.
    let store = TaskStore::load(fixture.config.join("tasks.json")).unwrap();
    assert_eq!(store.tasks().len(), 2);
    let catalog = crate::workspace_config::IcedCatalog::read(&fixture.config).unwrap();
    assert_eq!(catalog.active, 3);
    let saved = &catalog.workspaces[3];
    assert_ne!(
        crate::workspace_config::iced_layout_key(saved),
        crate::workspace_config::iced_layout_key(&catalog.workspaces[0])
    );
    let before = tree_snapshot(fixture.root.path());
    let mut publications = 0;
    let result = crate::data::IcedWorkData::read_with_progress(
        &fixture.config,
        &crate::data::TaskSource::iced(&fixture.config),
        Some(&home),
        4,
        saved,
        Focus::Latest,
        None,
        true,
        |data| {
            publications += 1;
            let state = Rc::new(RefCell::new(State::default()));
            data.apply(&mut state.borrow_mut());
            if publications == 1 {
                assert!(
                    state.borrow().pane(4).unwrap().branches.is_empty(),
                    "overview must precede branch probes"
                );
            } else {
                let state = state.borrow();
                let branches = &state.pane(4).unwrap().branches;
                assert!(
                    !branches.is_empty(),
                    "ownership must precede detailed probes"
                );
                assert!(branches.iter().all(|branch| branch.status.is_none()));
            }
            let mut sidebar = WorkSidebar::new(data.facts.clone(), state);
            sidebar.pull_requests = data.pull_requests.clone();
            sidebar.issues = data.issues.clone();
            let rows = sidebar.rows();
            let headings: Vec<_> = rows
                .iter()
                .filter(|row| row.heading)
                .map(|row| row.label.as_str())
                .collect();
            assert_eq!(
                headings,
                [
                    "Work fixture",
                    "Pull requests",
                    "Repos",
                    "Chats",
                    "Documents",
                    "Issues",
                    "In progress"
                ]
            );
            assert_eq!(
                rows.iter().find(|row| row.label == "Chats").unwrap().badges,
                ["1"]
            );
            assert_eq!(
                rows.iter()
                    .find(|row| row.label == "Pull requests")
                    .unwrap()
                    .badges,
                ["1 open"]
            );
            assert!(
                rows.iter()
                    .any(|row| row.label.contains("Synthetic pull request")
                        && row.badges.contains(&"draft".into()))
            );
            assert!(rows.iter().any(|row| row.label.contains("Synthetic issue")));
            assert!(rows.iter().any(|row| row.label == "✦ Root title"));
            assert!(rows.iter().any(|row| row.label.starts_with("owns 7 chats")));
            assert!(!rows.iter().any(|row| row.label.contains("Unrelated title")));
        },
    )
    .unwrap();
    assert_eq!(publications, 2);
    let mut state = State::default();
    result.apply(&mut state);
    assert!(!state.pane(4).unwrap().branches.is_empty());
    assert_eq!(tree_snapshot(fixture.root.path()), before);
}

#[test]
fn pr_fixture_groups_small_labels_done_counts_badges_and_errors_like_iced() {
    use gitterm_core::{
        merge_plan::{PrKey, PrState},
        prs::{PullCore, WatchedPr},
    };
    let mut sidebar = fixture(true);
    let mut unread = WatchedPr::new(PrKey::new("fake/repo", 42));
    unread.error = Some("gh api repos/fake/repo/pulls/42: HTTP 404: Not Found".into());
    let mut draft = WatchedPr::new(PrKey::new("example/gitterm", 183));
    draft.snapshot.core = Some(PullCore {
        title: "TRU-293 Default task base: fetch and start from the selected branch".into(),
        state: Some(PrState::Draft),
        mergeable: Some(false),
        ..Default::default()
    });
    let mut merged = WatchedPr::new(PrKey::new("example/gitterm", 1));
    merged.snapshot.core = Some(PullCore {
        state: Some(PrState::Merged),
        ..Default::default()
    });
    let mut old = merged.clone();
    old.key.number = 2;
    old.checked_at = Some("2000-01-01T00:00:00Z".into());
    sidebar.pull_requests = vec![
        PullRequestGroup {
            root: "first".into(),
            title: "lets keep going".into(),
            prs: vec![unread, merged.clone()],
        },
        PullRequestGroup {
            root: "second".into(),
            title: "Gemini models missing from model list".into(),
            prs: vec![draft],
        },
        PullRequestGroup {
            root: "third".into(),
            title: "Revisiting GPUI discussion".into(),
            prs: vec![merged.clone(), old.clone()],
        },
        PullRequestGroup {
            root: "stale".into(),
            title: "Only old PRs".into(),
            prs: vec![old],
        },
    ];
    let mut rows = vec![];
    sidebar.pull_request_rows(&mut rows);
    assert_eq!(rows[0].badges, ["2 open"]);
    assert_eq!(
        rows.iter().map(|r| r.label.as_str()).collect::<Vec<_>>(),
        [
            "Pull requests",
            "lets keep going",
            "fake/repo#42",
            "Gemini models missing from model list",
            "TRU-293 Default task base: fetch and start from the selected branch",
            "Revisiting GPUI discussion"
        ]
    );
    assert!(rows[1].small && rows[1].quiet);
    assert_eq!(rows[1].controls[0].label, "1 merged ▸");
    assert_eq!(rows[5].controls[0].label, "1 merged ▸");
    assert_eq!(rows[2].pr_number, Some((42, 4)));
    assert_eq!(rows[2].repo_prefix.as_deref(), Some("repo · "));
    assert_eq!(rows[2].badges, ["not read"]);
    assert_eq!(rows[4].badges, ["conflicts", "draft"]);
    assert!(
        rows.iter()
            .all(|r| r.details.is_empty() && !r.label.contains("HTTP"))
    );
    sidebar.activate(Action::Toggle(Fold::Pr("fake/repo#42".into())));
    let mut expanded = vec![];
    sidebar.pull_request_rows(&mut expanded);
    assert!(
        expanded[2]
            .details
            .iter()
            .any(|s| s.starts_with("not read: gh api"))
    );
    sidebar.pull_requests.truncate(1);
    let mut single = vec![];
    sidebar.pull_request_rows(&mut single);
    assert_eq!(single.len(), 2);
    assert_eq!(single[0].controls[0].label, "1 merged ▸");
    sidebar.activate(single[0].controls[0].action.clone());
    let mut done_open = vec![];
    sidebar.pull_request_rows(&mut done_open);
    assert_eq!(done_open[0].controls[0].label, "1 merged ▾");
    assert_eq!(done_open.len(), 3);
}

#[test]
fn repo_fixture_counts_branch_only_repos_and_iced_subtitles() {
    use gitterm_core::work::summary::RepoTouched;
    let mut sidebar = fixture(false);
    sidebar.workspace.repos = ["home", "gitterm", "design", "agent-kit", "Work"]
        .into_iter()
        .map(|name| RepoFacts {
            dir: format!("/repos/{name}").into(),
            local: true,
            run_command: None,
        })
        .collect();
    sidebar.workspace.auto_added.clear();
    sidebar.workspace.running_by_repo = [
        (PathBuf::from("/repos/home"), 1),
        (PathBuf::from("/repos/gitterm"), 1),
    ]
    .into();
    let mut pane = Pane::default();
    pane.last_full_refresh = Some(Instant::now());
    pane.summary.repos = [("gitterm", 382), ("design", 17), ("Work", 22)]
        .into_iter()
        .map(|(name, edits)| RepoTouched {
            dir: format!("/repos/{name}").into(),
            edits,
        })
        .collect();
    for (name, count) in [
        ("gitterm", 91),
        ("design", 3),
        ("agent-kit", 2),
        ("Work", 1),
        ("bebop-core", 4),
    ] {
        for index in 0..count {
            let mut row = branch(&format!("/repos/{name}"), &format!("b-{index}"), false);
            row.owned.task = None;
            pane.branches.push(row);
        }
    }
    let mut rows = vec![];
    sidebar.repo_rows(Some(&pane), &mut rows);
    assert_eq!(rows[0].badges, ["6"]);
    assert_eq!(
        rows.iter()
            .skip(1)
            .map(|r| r.label.as_str())
            .collect::<Vec<_>>(),
        [
            "home",
            "gitterm",
            "design",
            "agent-kit",
            "Work",
            "bebop-core"
        ]
    );
    assert_eq!(
        rows.iter()
            .skip(1)
            .map(|r| r.details[0].as_str())
            .collect::<Vec<_>>(),
        [
            "home · no edits · 1 running",
            "382 edits · 91 branches · 1 running",
            "17 edits · 3 branches",
            "2 branches",
            "22 edits · 1 branch",
            "4 branches"
        ]
    );
    assert!(rows.iter().skip(1).all(|r| r.fold.is_some() && !r.expanded));
}

#[test]
fn partial_index_reads_saved_codex_children_and_repo_work_without_writing() {
    use gitterm_core::chat_facts::{ChatChild, ChildKind};
    let fixture = IcedFixture::new();
    let home = fixture.root.path().join("harness-home");
    indexed_transcript(
        &home,
        "root-chat",
        std::path::Path::new(&fixture.workspace.dir),
        "Root title",
        "Latest request",
        "2026-11-20T12:00:00Z",
    );
    let facts_dir = gitterm_core::chat_facts::facts_dir(&fixture.config);
    let root_path =
        gitterm_core::chat_facts::facts_path(&facts_dir, ChatBackend::Claude, "root-chat").unwrap();
    let mut facts = gitterm_core::chat_facts::load_facts(&root_path)
        .unwrap()
        .unwrap();
    facts.children.push(ChatChild {
        session_id: "saved-worker".into(),
        kind: ChildKind::Worker,
        delegation_id: None,
        task_id: None,
        title: Some("Saved child".into()),
    });
    std::fs::write(root_path, serde_json::to_vec(&facts).unwrap()).unwrap();
    let extra = fixture.root.path().join("bebop-core");
    std::fs::create_dir(&extra).unwrap();
    let child = ChatFacts {
        repo_work: vec![extra.clone()],
        prs: vec![gitterm_core::chat_facts::PrRef {
            repo: "example/extra".into(),
            number: 42,
            url: "https://github.com/example/extra/pull/42".into(),
        }],
        ..Default::default()
    };
    std::fs::write(
        gitterm_core::chat_facts::facts_path(&facts_dir, ChatBackend::Codex, "saved-worker")
            .unwrap(),
        serde_json::to_vec(&child).unwrap(),
    )
    .unwrap();
    let before = tree_snapshot(fixture.root.path());
    let data = crate::data::IcedWorkData::read_with_home(
        &fixture.config,
        Some(&home),
        1,
        &fixture.workspace,
        Focus::Latest,
    )
    .unwrap();
    assert_eq!(data.facts.repos.len(), 3);
    assert!(data.facts.repos.iter().any(|repo| repo.dir == extra));
    assert_eq!(data.pull_requests.len(), 1);
    assert_eq!(data.pull_requests[0].prs[0].key.label(), "example/extra#42");
    assert_eq!(tree_snapshot(fixture.root.path()), before);
}

#[gpui::test]
fn long_pr_title_wraps_without_overlapping_number_or_badges(cx: &mut gpui::TestAppContext) {
    use gitterm_core::{
        merge_plan::{PrKey, PrState},
        prs::{PullCore, WatchedPr},
    };
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let mut sidebar = fixture(true);
    let mut pr = WatchedPr::new(PrKey::new("example/gitterm", 183));
    pr.snapshot.core = Some(PullCore { title: "TRU-293 Default task base: fetch and start from the selected branch with a deliberately long title".into(), state: Some(PrState::Draft), mergeable: Some(false), ..Default::default() });
    sidebar.pull_requests = vec![PullRequestGroup {
        root: "root".into(),
        title: "Owning chat".into(),
        prs: vec![pr],
    }];
    let (_, cx) = cx.add_window_view(|_, _| sidebar);
    cx.simulate_resize(gpui::size(px(360.), px(900.)));
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let number = cx.debug_bounds("work-pr-number-2").unwrap();
    let title = cx.debug_bounds("work-label-2").unwrap();
    let conflicts = cx.debug_bounds("work-badge-2-0").unwrap();
    let draft = cx.debug_bounds("work-badge-2-1").unwrap();
    assert!(number.right() <= title.left());
    assert!(title.right() <= conflicts.left());
    assert!(conflicts.right() <= draft.left());
    assert!(draft.right() <= px(360.));
    assert!(
        title.size.height > px(20.),
        "long title must wrap: {title:?}"
    );
    assert!(
        title.size.height < px(40.),
        "title is limited to two lines: {title:?}"
    );
}

#[test]
fn repo_running_subtitle_includes_live_chat_and_avoids_counted_task_twice() {
    let mut sidebar = fixture(false);
    sidebar.running_task_ids.insert("counted-task".into());
    sidebar.running_chats = vec![
        ("/repos/home".into(), None),
        ("/repos/api-tru-274".into(), None),
        ("/repos/api-tru-274".into(), Some("counted-task".into())),
        ("/elsewhere".into(), None),
    ];
    let rows = sidebar.rows();
    assert_eq!(
        rows.iter().find(|row| row.label == "home").unwrap().details,
        ["home · 2 edits · no live checkout of this work · 1 running"]
    );
    assert_eq!(
        rows.iter().find(|row| row.label == "api").unwrap().details,
        ["7 edits · 1 branch · 1 PR · 3 running"]
    );
}

#[gpui::test]
fn open_review_checkout_marks_only_its_branch_row_selected(cx: &mut gpui::TestAppContext) {
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let mut fixture = fixture(false);
    fixture.open.insert(Fold::Repo("/repos/api".into()));
    let (sidebar, cx) = cx.add_window_view(|_, _| fixture);
    cx.simulate_resize(gpui::size(px(640.), px(1200.)));
    draw_sidebar(cx);
    let index = sidebar.read_with(cx, |s, _| {
        s.rows().iter().position(|r| r.label == "tru-274").unwrap()
    });
    let selected: &'static str = Box::leak(format!("work-selected-{index}").into_boxed_str());
    assert!(cx.debug_bounds(selected).is_none());
    let count = |cx: &mut gpui::VisualTestContext| {
        (0..40)
            .filter(|i| {
                cx.debug_bounds(Box::leak(format!("work-selected-{i}").into_boxed_str()))
                    .is_some()
            })
            .count()
    };
    let others = count(cx);
    sidebar.update(cx, |s, cx| {
        s.set_review_checkout(Some("/repos/api-tru-274".into()), cx)
    });
    draw_sidebar(cx);
    assert!(cx.debug_bounds(selected).is_some());
    assert_eq!(
        count(cx),
        others + 1,
        "only the open review's branch is added"
    );
    sidebar.update(cx, |s, cx| s.set_review_checkout(None, cx));
    draw_sidebar(cx);
    assert!(cx.debug_bounds(selected).is_none());
}

#[gpui::test]
fn expanded_repo_row_name_and_run_never_shrink_but_actions_wrap(cx: &mut gpui::TestAppContext) {
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let repo = PathBuf::from("/repos/gitterm-v2");
    let fold = Fold::Repo(repo.clone());
    let (sidebar, cx) = cx.add_window_view(|_, _| {
        let mut sidebar = fixture(false);
        sidebar.workspace.repos[0].dir = repo.clone();
        sidebar.workspace.repos[0].run_command = Some("cargo run --example preview".into());
        sidebar.open.insert(fold.clone());
        sidebar
    });
    let index = sidebar.read_with(cx, |s, _| {
        s.rows()
            .iter()
            .position(|r| r.label == "gitterm-v2")
            .unwrap()
    });
    let measure = |cx: &mut gpui::VisualTestContext, width: f32| {
        cx.simulate_resize(gpui::size(px(width), px(900.)));
        draw_sidebar(cx);
        let bounds = |cx: &mut gpui::VisualTestContext, selector: String| {
            cx.debug_bounds(Box::leak(selector.into_boxed_str()))
                .unwrap_or_else(|| panic!("missing bounds at {width}pt"))
        };
        (
            bounds(cx, format!("work-name-{index}")).size,
            bounds(cx, format!("work-run-{index}")).size,
        )
    };
    let (name_wide, run_wide) = measure(cx, 2400.);
    let (name, run) = measure(cx, 300.);
    assert_eq!(name, name_wide, "repo name keeps its full width and height");
    assert_eq!(run, run_wide, "run command keeps its full width and height");
    assert!(
        name.height >= run.height,
        "Iced uses UI-small for the name and 10px for the run command"
    );
    let disclosure = cx
        .debug_bounds(Box::leak(format!("work-fold-{index}-▾").into_boxed_str()))
        .unwrap();
    let bounds = cx.debug_bounds("work-sidebar").unwrap();
    assert!(disclosure.right() <= bounds.right());
    let p = crate::theme::Palette::for_mode(true);
    let editor = Action::Emit(WorkSidebarOutcome::OpenEditor { dir: repo.clone() });
    assert_eq!(control_color(&editor, true, &p), p.muted);
    assert_eq!(control_color(&editor, false, &p), p.blue);
    sidebar.read_with(cx, |s, _| {
        let row = s
            .rows()
            .into_iter()
            .find(|r| r.label == "gitterm-v2")
            .unwrap();
        assert!(row.controls.iter().any(|c| c.label == "Open in editor"));
    });
}

#[test]
fn page_routes_carry_mapped_owners_and_cached_issue_workspaces() {
    use gitterm_core::issues::CachedIssue;
    let mut sidebar = fixture(true);
    sidebar.pr_accounts = BTreeMap::from([("example".into(), "Sample-Login".into())]);
    let issue = |url: Option<&str>, workspace: Option<&str>| CachedIssue {
        url: url.map(str::to_owned),
        workspace: workspace.map(str::to_owned),
        ..Default::default()
    };
    for (key, cached) in [
        // Iced prefers the cached workspace over the URL's.
        (
            "SAMPLE-1",
            issue(
                Some("https://linear.app/sample-a/issue/SAMPLE-1/x"),
                Some("sample-b"),
            ),
        ),
        (
            "SAMPLE-2",
            issue(Some("https://linear.app/sample-c/issue/SAMPLE-2/y"), None),
        ),
        ("SAMPLE-3", issue(Some("https://example.com/issue/3"), None)),
    ] {
        sidebar.issues.issues.insert(key.into(), cached);
    }
    let routes = sidebar.page_routes();
    assert_eq!(routes.github_accounts, sidebar.pr_accounts);
    assert_eq!(
        routes.linear_workspaces,
        ["sample-b".to_string(), "sample-c".to_string()].into()
    );
}

#[gpui::test]
fn long_branch_title_is_shortened_at_iced_limit_then_wraps(cx: &mut gpui::TestAppContext) {
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let long = "task/02bf8131-5c35-4939-b33f-bf01dae8aa5c-extra-long-name";
    let (sidebar, cx) = cx.add_window_view(|_, _| {
        let mut sidebar = fixture_with_branch(false, long);
        sidebar.open.insert(Fold::Repo("/repos/api".into()));
        sidebar
    });
    cx.simulate_resize(gpui::size(px(300.), px(900.)));
    draw_sidebar(cx);
    let (index, label) = sidebar.read_with(cx, |s, _| {
        let rows = s.rows();
        let index = rows
            .iter()
            .position(|r| r.label.starts_with("task/02bf"))
            .unwrap();
        (index, rows[index].label.clone())
    });
    assert_eq!(label.chars().count(), 40);
    assert!(label.ends_with('\u{2026}'));
    let title = cx
        .debug_bounds(Box::leak(format!("work-label-{index}").into_boxed_str()))
        .unwrap();
    assert!(title.size.height > px(20.), "branch title wraps: {title:?}");
}

#[gpui::test]
fn at_310pt_branch_title_wraps_to_two_lines_across_the_row(cx: &mut gpui::TestAppContext) {
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let long = "task/02bf8131-5c35-4939-b33f-bf01dae8aa5c-extra-long-name";
    let (sidebar, cx) = cx.add_window_view(|_, _| {
        let mut sidebar = fixture_with_branch(false, long);
        sidebar.open.insert(Fold::Repo("/repos/api".into()));
        sidebar
    });
    cx.simulate_resize(gpui::size(px(310.), px(900.)));
    draw_sidebar(cx);
    let index = sidebar.read_with(cx, |s, _| {
        s.rows()
            .iter()
            .position(|r| r.label.starts_with("task/02bf"))
            .unwrap()
    });
    let title = cx
        .debug_bounds(Box::leak(format!("work-label-{index}").into_boxed_str()))
        .unwrap();
    let line = px(14.);
    assert!(
        title.size.height > line * 1.5 && title.size.height < line * 2.6,
        "exactly two lines: {title:?}"
    );
    assert!(title.size.width > px(180.), "uses the row width: {title:?}");
}

/// A chat row with a long title and every trailing fact (open, fan-out, age).
fn long_chat_entry(id: &str, title: &str) -> ChatIndexEntry {
    use gitterm_core::chat_facts::{ChatChild, ChildKind};
    let mut entry = entry(id, title, SystemTime::now());
    let facts = entry.facts.as_mut().unwrap();
    for index in 0..38 {
        facts.children.push(ChatChild {
            session_id: format!("sub-{id}-{index}"),
            kind: ChildKind::Subagent,
            delegation_id: None,
            task_id: None,
            title: None,
        });
    }
    for kind in [ChildKind::Worker, ChildKind::Review] {
        facts.children.push(ChatChild {
            session_id: format!("{kind:?}-{id}"),
            kind,
            delegation_id: None,
            task_id: None,
            title: None,
        });
    }
    entry
}

#[gpui::test]
fn chat_titles_wrap_in_remaining_fill_width_at_310pt_and_240pt(cx: &mut gpui::TestAppContext) {
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    cx.update(|cx| crate::theme::apply(true, None, cx));
    let sidebar = fixture(false);
    // "recent" is open in the fixture, so it carries open + fan-out + age.
    replace_entries(
        &sidebar,
        vec![
            long_chat_entry(
                "recent",
                "Revisiting GPUI sidebar parity and keep going on the remaining row layouts",
            ),
            long_chat_entry(
                "second",
                "Investigate why the Chats section title wraps one letter per line when narrow",
            ),
        ],
    );
    let (sidebar, cx) = cx.add_window_view(|_, _| sidebar);
    let chats = sidebar.read_with(cx, |s, _| {
        s.rows()
            .iter()
            .enumerate()
            .filter(|(_, row)| row.chat)
            .map(|(index, row)| (index, row.badges.clone()))
            .collect::<Vec<_>>()
    });
    assert_eq!(chats.len(), 2);
    assert!(
        chats[0].1.iter().any(|badge| badge.contains("subagents")),
        "fixture row carries a fan-out fact: {:?}",
        chats[0].1
    );
    let line_height = px(14.);
    for width in [310., 240.] {
        cx.simulate_resize(gpui::size(px(width), px(900.)));
        draw_sidebar(cx);
        for (index, badges) in &chats {
            let title = cx
                .debug_bounds(Box::leak(format!("work-label-{index}").into_boxed_str()))
                .unwrap();
            let line = cx
                .debug_bounds(Box::leak(format!("work-line-{index}").into_boxed_str()))
                .unwrap();
            // Iced lays out fixed facts first, then wraps the Fill title in
            // the remaining space. There is no minimum half-row title width;
            // rich metadata can exhaust that space at 240pt.
            assert!(
                title.size.height > line_height * 1.5,
                "{width}pt chat {index} long title wraps like Iced: {title:?}"
            );
            assert!(
                title.size.height <= line_height * 16.,
                "{width}pt title wraps words, never individual glyphs: {title:?}"
            );
            assert!(
                title.size.width >= px(0.) && title.size.width < line.size.width * 0.5,
                "{width}pt chat {index} title gives way to intrinsic facts: {title:?} in {line:?}"
            );
            for badge_index in 0..badges.len() {
                let badge = cx
                    .debug_bounds(Box::leak(
                        format!("work-badge-{index}-{badge_index}").into_boxed_str(),
                    ))
                    .unwrap();
                assert!(
                    title.right() <= badge.left(),
                    "{width}pt chat {index} title overlaps fact {badge_index}: {title:?} vs {badge:?}"
                );
            }
        }
    }
}

#[gpui::test]
fn at_310pt_expanded_repo_actions_keep_one_line_and_age_wraps_by_word(
    cx: &mut gpui::TestAppContext,
) {
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let repo = PathBuf::from("/repos/api");
    let (sidebar, cx) = cx.add_window_view(|_, _| {
        let mut sidebar = fixture(false);
        sidebar.workspace.repos[0].dir = repo.clone();
        sidebar.workspace.repos[0].run_command = Some("cargo run --example preview".into());
        sidebar.open.insert(Fold::Repo(repo.clone()));
        sidebar
    });
    let index = sidebar.read_with(cx, |s, _| {
        s.rows().iter().position(|r| r.label == "api").unwrap()
    });
    let size = |cx: &mut gpui::VisualTestContext, width: f32, id: String| {
        cx.simulate_resize(gpui::size(px(width), px(900.)));
        draw_sidebar(cx);
        cx.debug_bounds(Box::leak(id.into_boxed_str()))
            .unwrap_or_else(|| panic!("missing bounds at {width}pt"))
    };
    let open = format!("work-control-{index}-0");
    let edit = format!("work-control-{index}-1");
    let wide_open = size(cx, 2400., open.clone()).size;
    let wide_edit = size(cx, 2400., edit.clone()).size;
    let narrow_open = size(cx, 300., open).size;
    let narrow_edit = size(cx, 300., edit).size;
    assert_eq!(narrow_open, wide_open, "Open in editor stays intrinsic");
    assert_eq!(narrow_edit, wide_edit, "edit stays intrinsic");
    // The age gives way first: wrapped or dropped to nothing.
    let age = size(cx, 300., format!("work-age-{index}")).size;
    assert!(age.height <= px(26.), "age at most two lines: {age:?}");
    let remove = cx
        .debug_bounds(Box::leak(
            format!("work-control-{index}-2").into_boxed_str(),
        ))
        .expect("× is always rendered");
    let disclosure = cx
        .debug_bounds(Box::leak(format!("work-fold-{index}-▾").into_boxed_str()))
        .unwrap();
    let actions = cx
        .debug_bounds(Box::leak(format!("work-actions-{index}").into_boxed_str()))
        .unwrap();
    let bounds = cx.debug_bounds("work-sidebar").unwrap();
    assert!(remove.left() >= bounds.left() && remove.right() <= bounds.right());
    assert!(disclosure.right() <= bounds.right());
    // Name and run command keep their width, so the clipped actions slot may
    // be pushed wholly past the wrapper's edge; it never overlaps ×.
    assert!(
        actions.size.width == px(0.)
            || actions.right() <= remove.left()
            || actions.left() >= remove.right(),
        "× clear of actions: {actions:?} vs {remove:?}"
    );
    assert!(remove.right() <= disclosure.left(), "× clear of disclosure");
}

#[gpui::test]
fn repo_age_text_wraps_when_the_actions_slot_is_squeezed(cx: &mut gpui::TestAppContext) {
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let repo = PathBuf::from("/repos/api");
    let (sidebar, cx) = cx.add_window_view(|_, _| {
        let mut sidebar = fixture(false);
        sidebar.workspace.repos[0].dir = repo.clone();
        sidebar.workspace.repos[0].run_command = Some("cargo run --example preview".into());
        sidebar.open.insert(Fold::Repo(repo.clone()));
        sidebar
    });
    let index = sidebar.read_with(cx, |s, _| {
        s.rows().iter().position(|r| r.label == "api").unwrap()
    });
    let age = |cx: &mut gpui::VisualTestContext, width: f32| {
        cx.simulate_resize(gpui::size(px(width), px(900.)));
        draw_sidebar(cx);
        cx.debug_bounds(Box::leak(format!("work-age-{index}").into_boxed_str()))
            .unwrap()
            .size
    };
    let wide = age(cx, 2400.);
    let narrow = age(cx, 300.);
    assert!(narrow.height >= wide.height, "age may wrap to more lines");
}

/// H5b B8b: a preview task host on a preview root under `base`, seeded from
/// the Iced stand-in at `iced_config`, installed as the app's host.
pub(crate) fn start_preview_host(
    base: &std::path::Path,
    iced_config: &std::path::Path,
    cx: &mut gpui::TestAppContext,
) -> gpui::Entity<crate::task_host::TaskHostOwner> {
    use crate::task_host::{GpuiTaskHost, HostConfig, Ingress, TaskHostOwner};
    use gitterm_core::preview::{Preview, ProtectedRoots};
    let protected =
        ProtectedRoots::new(&base.join("fixture-home"), [iced_config.to_path_buf()]).unwrap();
    let roots = std::sync::Arc::new(Preview::validate(&base.join("preview"), protected).unwrap());
    let (sender, receiver) = gitterm_core::task_commands::runtime::command_channel();
    let config = HostConfig {
        roots,
        iced_config: iced_config.to_path_buf(),
        ingress: Ingress::Channel(sender, receiver),
    };
    let owner = cx.update(|cx| TaskHostOwner::start(config, cx)).unwrap();
    cx.update(|cx| cx.set_global(GpuiTaskHost(owner.clone())));
    owner
}

/// Stops the host inside the test: ingress closes and the pump exits on the
/// test scheduler, so the servers' teardown wakes nothing on it.
pub(crate) fn stop_preview_host(
    owner: &gpui::Entity<crate::task_host::TaskHostOwner>,
    cx: &mut gpui::TestAppContext,
) {
    owner.update(cx, |owner, _| owner.host().shutdown());
    cx.run_until_parked();
    owner.update(cx, |owner, _| owner.shutdown());
}

/// What the Work pane, workspace bar and chat rosters show from one task source.
#[derive(Debug, PartialEq)]
struct PaneRows {
    work_tasks: Vec<(String, String)>,
    work_workers: Vec<String>,
    running: Vec<String>,
    attention: Vec<(crate::WorkspaceId, usize)>,
    roster_tasks: Vec<String>,
    cards: Vec<gitterm_gpui_chat::delegation::DelegationCard>,
}
impl IcedFixture {
    fn base(&self) -> PathBuf {
        self.root.path().canonicalize().unwrap()
    }
    fn rows(&self, tasks: &crate::data::TaskSource) -> PaneRows {
        let work = crate::data::IcedWorkData::read(
            &self.config,
            tasks,
            1,
            &self.workspace,
            Focus::Latest,
            None,
            |_| {},
        )
        .unwrap();
        let catalog = crate::workspace_config::IcedCatalog::read(&self.config).unwrap();
        let bar = crate::workspace_bar::SavedFacts::read_from(
            &self.config,
            &catalog.workspaces,
            &Default::default(),
            crate::workspace_bar::Colors::new(true),
            tasks,
        )
        .unwrap();
        let roster = crate::data::RosterFacts::read(&self.config, tasks).unwrap();
        let projection = crate::chat::RosterProjection::new(&roster.tasks, &roster.workers, |w| {
            roster.worker_tab(&w.delegation_id, None)
        });
        let mut running: Vec<_> = work.running_task_ids.into_iter().collect();
        running.sort();
        let mut attention: Vec<_> = bar.attention_counts.into_iter().collect();
        attention.sort();
        PaneRows {
            work_tasks: work
                .tasks
                .iter()
                .map(|t| (t.task_id.clone(), t.title.clone()))
                .collect(),
            work_workers: work
                .workers
                .iter()
                .map(|w| w.delegation_id.clone())
                .collect(),
            running,
            attention,
            roster_tasks: roster.tasks.iter().map(|t| t.title.clone()).collect(),
            cards: projection.cards,
        }
    }
    /// Replaces Iced's store with one that differs from the seeded copy: the
    /// first task is renamed and every worker is gone. A later mtime, so the
    /// host's next poll would see it.
    fn diverge_iced(&self) {
        use gitterm_core::tasks::TaskStoreDocument;
        let path = self.config.join("tasks.json");
        let mut document: TaskStoreDocument =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        document.tasks[0].title = "Renamed in Iced".into();
        document.delegations.clear();
        std::fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(SystemTime::now() + Duration::from_secs(10))
            .unwrap();
    }
}

#[gpui::test]
fn work_find_counts_match_host(cx: &mut gpui::TestAppContext) {
    let fixture = IcedFixture::new();
    let iced = crate::data::TaskSource::iced(&fixture.config);
    let before = fixture.rows(&iced);
    let owner = start_preview_host(&fixture.base(), &fixture.config, cx);
    let host = cx.update(|cx| crate::data::TaskSource::resolve(&fixture.config, cx));
    let seeded = owner.read_with(cx, |owner, _| {
        owner.host().store().as_ref().unwrap().path().to_owned()
    });
    assert_eq!(host, crate::data::TaskSource::Host(seeded));
    let rows = fixture.rows(&host);
    assert_eq!(rows, before, "same rows, order and counts as Iced's file");
    assert_eq!(
        rows.work_tasks,
        [("task-1".to_string(), "Fixture task".to_string())]
    );
    assert_eq!(rows.work_workers, ["worker-1"]);
    assert_eq!(rows.cards.len(), 1);
    stop_preview_host(&owner, cx);
}

#[gpui::test]
fn delegation_cards_from_host_store(cx: &mut gpui::TestAppContext) {
    let fixture = IcedFixture::new();
    let owner = start_preview_host(&fixture.base(), &fixture.config, cx);
    fixture.diverge_iced();
    let host = cx.update(|cx| crate::data::TaskSource::resolve(&fixture.config, cx));
    let cards = fixture.rows(&host).cards;
    let stored = owner.read_with(cx, |owner, _| {
        owner
            .host()
            .store()
            .as_ref()
            .unwrap()
            .delegations()
            .to_vec()
    });
    assert_eq!(
        cards.iter().map(|c| &c.delegation).collect::<Vec<_>>(),
        stored.iter().collect::<Vec<_>>(),
        "cards come from the host's store, not Iced's emptied file"
    );
    assert_eq!(cards[0].delegation.delegation_id, "worker-1");
    assert!(cards[0].worker.is_some(), "the worker card keeps its task");
    assert!(
        fixture
            .rows(&crate::data::TaskSource::iced(&fixture.config))
            .cards
            .is_empty()
    );
    stop_preview_host(&owner, cx);
}

#[gpui::test]
fn panes_fall_back_to_iced_file_when_host_down(cx: &mut gpui::TestAppContext) {
    use std::sync::atomic::AtomicBool;
    let fixture = IcedFixture::new();
    let iced = crate::data::TaskSource::iced(&fixture.config);
    assert_eq!(
        cx.update(|cx| crate::data::TaskSource::resolve(&fixture.config, cx)),
        iced
    );
    let logged = AtomicBool::new(false);
    let mut lines = vec![];
    for _ in 0..3 {
        let source =
            crate::data::TaskSource::choose(None, &fixture.config, &logged, |line: &str| {
                lines.push(line.to_owned())
            });
        assert_eq!(source, iced);
    }
    assert_eq!(lines.len(), 1, "one line while the host is down: {lines:?}");
    assert!(lines[0].contains("task host is not running"));
    let before = tree_snapshot(fixture.root.path());
    let rows = fixture.rows(&iced);
    assert_eq!(rows.work_workers, ["worker-1"]);
    assert_eq!(rows.cards.len(), 1);
    assert_eq!(
        tree_snapshot(fixture.root.path()),
        before,
        "the fallback only reads Iced's file"
    );
}

#[gpui::test]
fn no_shell_reader_loads_iced_tasks_json_when_host_up(cx: &mut gpui::TestAppContext) {
    let fixture = IcedFixture::new();
    let owner = start_preview_host(&fixture.base(), &fixture.config, cx);
    let seeded = fixture.rows(&crate::data::TaskSource::iced(&fixture.config));
    fixture.diverge_iced();
    let host = cx.update(|cx| crate::data::TaskSource::resolve(&fixture.config, cx));
    let shown = fixture.rows(&host);
    assert_eq!(shown, seeded, "every pane shows the seeded store");
    assert_eq!(shown.roster_tasks[0], "Fixture task");
    // Once the host re-seeds, the panes follow it.
    let reseed = owner.update(cx, |owner, cx| owner.refresh_seed(cx));
    assert_eq!(reseed, crate::task_host::Reseed::Reseeded);
    let shown = fixture.rows(&host);
    assert_eq!(shown.roster_tasks[0], "Renamed in Iced");
    assert!(shown.cards.is_empty());
    stop_preview_host(&owner, cx);
}

#[gpui::test]
fn delegation_dismiss_and_stop_go_through_service_context_without_writes(
    cx: &mut gpui::TestAppContext,
) {
    use gitterm_gpui_chat::interactions::{DismissRequest, preview_notice};
    let fixture = IcedFixture::new();
    let owner = start_preview_host(&fixture.base(), &fixture.config, cx);
    let requests = [
        ("Dismiss", false),
        ("Cancel", true),
        ("Stop tracking", true),
    ]
    .map(|(label, active)| DismissRequest {
        id: "worker-1".into(),
        label: label.into(),
        active,
    });
    let before = tree_snapshot(fixture.root.path());
    for request in &requests {
        let notice = cx.update(|cx| {
            crate::chat::dismiss_notice(
                crate::task_panel::host(cx).map(|host| host.services()),
                request,
            )
        });
        assert_eq!(notice, preview_notice(&request.label));
        assert_eq!(
            crate::chat::dismiss_notice(None, request),
            preview_notice(&request.label)
        );
    }
    assert_eq!(
        tree_snapshot(fixture.root.path()),
        before,
        "nothing written"
    );
    owner.read_with(cx, |owner, _| {
        assert_eq!(
            owner.host().store().as_ref().unwrap().delegations()[0].dismissed_at,
            None
        )
    });
    // The decision is the context's: one that allows the effects says so.
    let allowing =
        gitterm_core::preview::service::ServiceContext::production(&fixture.base(), "fixture")
            .unwrap();
    assert_eq!(
        crate::chat::dismiss_notice(Some(&allowing), &requests[0]),
        "Dismiss · not wired in the GPUI preview"
    );
    stop_preview_host(&owner, cx);
}

#[test]
fn tru274_loading_replaces_refresh_control_without_a_title_badge() {
    let mut sidebar = fixture(true);
    sidebar.loading = true;
    let rows = sidebar.rows();
    assert!(rows[0].badges.is_empty());
    let refresh = rows[0]
        .controls
        .iter()
        .find(|c| c.label == "reading…")
        .unwrap();
    assert_eq!(refresh.action, Action::Inert);
    sidebar.loading = false;
    assert_eq!(
        control(&sidebar, "Parity", "↻"),
        Action::Emit(WorkSidebarOutcome::Refresh { workspace: 7 })
    );
    *sidebar.state.borrow_mut() = State::default();
    sidebar.loading = true;
    assert!(sidebar.rows()[0].badges.is_empty());
    assert_eq!(control(&sidebar, "Parity", "↻"), Action::Inert);
}

#[gpui::test]
fn tru274_skills_link_starts_at_the_left_edge(cx: &mut gpui::TestAppContext) {
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let (_sidebar, cx) = cx.add_window_view(|_, _| {
        let mut sidebar = fixture(true);
        sidebar.agent_line = Some("skills 14 · MCPs 0 · hooks 4".into());
        sidebar
    });
    cx.simulate_resize(gpui::size(px(600.), px(1600.)));
    draw_sidebar(cx);
    let link = cx.debug_bounds("work-agent-config").unwrap();
    let body = cx.debug_bounds("work-sidebar").unwrap();
    assert_eq!(
        link.left() - body.left(),
        px(8.),
        "Iced content's 8px left padding"
    );
}

#[test]
fn tru274_chat_mark_uses_iced_mauve_in_both_themes() {
    for (dark, mauve) in [(true, 0xcba6f7), (false, 0x8839ef)] {
        let p = crate::theme::Palette::for_mode(dark);
        let mut mark = chat_mark(0, 16., &p);
        assert_eq!(mark.style().text.color, Some(rgb(mauve).into()));
        assert_eq!(
            mark.style().text.font_family.as_deref(),
            Some(".SystemUIFont")
        );
        assert_eq!(mark.style().text.font_size, Some(px(16.).into()));
    }
}

#[gpui::test]
fn tru274_pr_number_tracks_ui_small_size_and_iced_menlo_colour(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    let sidebar = cx.new(|_| fixture(true));
    cx.update(|cx| {
        crate::theme::set_font_sizes(
            crate::theme::FontSizes {
                ui: 17.,
                terminal: 14.,
            },
            cx,
        );
        sidebar.update(cx, |sidebar, cx| {
            let mut number = sidebar.pr_number_button(0, 183, 4, Action::Inert, cx);
            assert_eq!(number.style().text.font_size, Some(px(16.).into()));
            assert_eq!(number.style().text.font_family.as_deref(), Some("Menlo"));
            assert_eq!(
                number.style().text.color,
                Some(rgb(crate::theme::Palette::current(cx).muted).into())
            );
            assert_eq!(number.style().size.width, Some(px(40.).into()));
        });
    });
}

#[gpui::test]
fn tru274_add_repo_and_run_command_use_iced_control_styles(cx: &mut gpui::TestAppContext) {
    cx.update(gpui_component::init);
    let sidebar = cx.new(|_| fixture(true));
    cx.update(|cx| {
        crate::theme::set_font_sizes(
            crate::theme::FontSizes {
                ui: 17.,
                terminal: 14.,
            },
            cx,
        );
        sidebar.update(cx, |sidebar, cx| {
            let mut add = sidebar.button(
                "add".into(),
                Control {
                    label: "+ Add repo…".into(),
                    action: Action::Emit(WorkSidebarOutcome::AddRepo),
                },
                false,
                cx,
            );
            assert_eq!(add.style().text.font_size, Some(px(15.).into()));
            assert_eq!(
                add.style().text.font_family.as_deref(),
                Some(".SystemUIFont")
            );
            assert_eq!(
                add.style().text.color,
                Some(rgb(crate::theme::Palette::current(cx).text).into())
            );
            assert_eq!(add.style().border_widths.top, Some(px(1.).into()));
            let mut run = sidebar.button(
                "edit".into(),
                Control {
                    label: "+ run command".into(),
                    action: Action::Edit("/fixture".into()),
                },
                true,
                cx,
            );
            assert_eq!(run.style().text.font_family.as_deref(), Some("Menlo"));
        });
    });
}

#[gpui::test]
fn tru274_chat_fanout_keeps_iced_intrinsic_width_at_310pt(cx: &mut gpui::TestAppContext) {
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let sidebar = fixture(false);
    replace_entries(
        &sidebar,
        vec![long_chat_entry(
            "recent",
            "Revisiting GPUI sidebar parity and keep going on the remaining row layouts",
        )],
    );
    let index = sidebar.rows().iter().position(|r| r.chat).unwrap();
    let (sidebar, cx) = cx.add_window_view(|_, _| sidebar);
    let badge_index = sidebar.read_with(cx, |s, _| {
        s.rows()[index]
            .badges
            .iter()
            .position(|b| b.contains("subagents"))
            .unwrap()
    });
    let selector = Box::leak(format!("work-badge-{index}-{badge_index}").into_boxed_str());
    cx.simulate_resize(gpui::size(px(1200.), px(1600.)));
    draw_sidebar(cx);
    let wide = cx.debug_bounds(selector).unwrap();
    cx.simulate_resize(gpui::size(px(310.), px(1600.)));
    draw_sidebar(cx);
    let narrow = cx.debug_bounds(selector).unwrap();
    assert_eq!(
        wide.size.width, narrow.size.width,
        "fan-out must not gain an extra ellipsis"
    );
    let age = cx
        .debug_bounds(Box::leak(
            format!("work-badge-{index}-{}", badge_index + 1).into_boxed_str(),
        ))
        .unwrap();
    assert!(age.left() >= narrow.right(), "age follows fan-out");
}

#[test]
fn tru274_chat_order_is_shared_latest_root_or_child_then_id() {
    let sidebar = fixture(false);
    let now = SystemTime::now();
    let mut hello = entry("a", "hello", now - Duration::from_secs(3600));
    let discussion = entry(
        "z",
        "Revisiting GPUI discussion",
        now - Duration::from_secs(60),
    );
    let mut child = entry("child", "worker", now);
    child.root_id = Some(hello.id.clone());
    hello
        .facts
        .as_mut()
        .unwrap()
        .children
        .push(gitterm_core::chat_facts::ChatChild {
            session_id: child.id.clone(),
            kind: ChildKind::Worker,
            delegation_id: None,
            task_id: None,
            title: None,
        });
    replace_entries(&sidebar, vec![discussion, child, hello]);
    let titles = || {
        sidebar
            .rows()
            .into_iter()
            .filter(|r| r.chat)
            .map(|r| r.label)
            .collect::<Vec<_>>()
    };
    assert_eq!(titles(), ["✦ hello", "✦ Revisiting GPUI discussion"]);
    replace_entries(
        &sidebar,
        vec![
            entry("z", "Revisiting GPUI discussion", now),
            entry("a", "hello", now),
        ],
    );
    assert_eq!(titles(), ["✦ hello", "✦ Revisiting GPUI discussion"]);
}

#[test]
fn tru274_chat_fields_and_badge_styles_match_iced() {
    let mut sidebar = fixture(true);
    let now = SystemTime::now();
    let mut chat = summary::coordinators(
        &[&long_chat_entry("recent", "A title")],
        &[long_chat_entry("recent", "A title")],
        now,
    )
    .remove(0);
    chat.latest = Some("latest line".into());
    sidebar.open.insert(Fold::Chat("recent".into()));
    let row = sidebar.chat_row(&chat, false, true, Some(12));
    assert_eq!(
        row.badges,
        [
            "open".to_string(),
            "12 edits here".into(),
            summary::shorten(chat.fan_out.as_ref().unwrap(), 28),
            chats::format_age(chat.active)
        ]
    );
    assert_eq!(row.details[0], "↳ latest line");
    assert_eq!(row.controls[0].label, "↗");
    for dark in [true, false] {
        let p = crate::theme::Palette::for_mode(dark);
        for (badge, chat, family, color) in [
            ("open", true, ".SystemUIFont", p.green),
            ("12 edits here", true, "Menlo", p.blue),
            ("3 subagents · 317 workers · …", true, "Menlo", p.muted),
            ("now", true, "Menlo", p.muted),
            ("not read", false, "Menlo", p.red),
            ("conflicts", false, "Menlo", p.red),
            ("draft", false, "Menlo", p.yellow),
        ] {
            let mut badge = row_badge(0, 0, badge.into(), chat, None, &p, rgb(p.yellow).into());
            assert_eq!(badge.style().text.font_family.as_deref(), Some(family));
            assert_eq!(badge.style().text.font_size, Some(px(10.).into()));
            assert_eq!(badge.style().text.color, Some(rgb(color).into()));
        }
    }
}

#[test]
fn tru274_chat_latest_and_children_use_iced_detail_styles() {
    for dark in [true, false] {
        let p = crate::theme::Palette::for_mode(dark);
        for (index, detail, size, color) in [
            (0, "↳ latest line", 15., p.subtext0),
            (1, "   worker · title · now", 10., p.muted),
            (0, "   worker · no latest · now", 10., p.muted),
        ] {
            let mut text = row_detail(0, index, detail.into(), false, true, 16., &p);
            assert_eq!(
                text.style().text.font_family.as_deref(),
                Some(".SystemUIFont")
            );
            assert_eq!(text.style().text.font_size, Some(px(size).into()));
            assert_eq!(text.style().text.color, Some(rgb(color).into()));
        }
    }
}

#[test]
fn tru274_section_headings_use_iced_secondary_text_and_ui_count() {
    for dark in [true, false] {
        let p = crate::theme::Palette::for_mode(dark);
        let mut section = heading_label(div().id("section"), false, 17., &p);
        assert_eq!(section.style().text.color, Some(rgb(p.subtext0).into()));
        assert_eq!(section.style().text.font_size, Some(px(16.).into()));
        assert_eq!(
            section.style().text.font_weight,
            Some(gpui::FontWeight::BOLD)
        );
        let mut title = heading_label(div().id("workspace"), true, 17., &p);
        assert_eq!(title.style().text.color, Some(rgb(p.text).into()));
        assert_eq!(title.style().text.font_size, Some(px(17.).into()));
        let mut count = row_badge(
            0,
            0,
            "14".into(),
            false,
            Some(15.),
            &p,
            rgb(p.yellow).into(),
        );
        assert_eq!(
            count.style().text.font_family.as_deref(),
            Some(".SystemUIFont")
        );
        assert_eq!(count.style().text.font_size, Some(px(15.).into()));
        assert_eq!(count.style().text.color, Some(rgb(p.muted).into()));
    }
}

#[test]
fn tru274_repo_name_uses_iced_bold_proportional_ui_small() {
    let mut label = repo_name_label(div().id("name"), 16.);
    assert_eq!(
        label.style().text.font_family.as_deref(),
        Some(".SystemUIFont")
    );
    assert_eq!(label.style().text.font_size, Some(px(16.).into()));
    assert_eq!(label.style().text.font_weight, Some(gpui::FontWeight::BOLD));
}

#[gpui::test]
fn tru274_repo_actions_use_iced_two_pixel_spacing(cx: &mut gpui::TestAppContext) {
    cx.background_executor.allow_parking();
    cx.update(gpui_component::init);
    let (sidebar, cx) = cx.add_window_view(|_, _| {
        let mut sidebar = fixture(true);
        sidebar.open.insert(Fold::Repo("/repos/home".into()));
        sidebar
    });
    let index = sidebar.read_with(cx, |s, _| {
        s.rows().iter().position(|r| r.label == "home").unwrap()
    });
    cx.simulate_resize(gpui::size(px(1200.), px(1600.)));
    draw_sidebar(cx);
    let editor = cx
        .debug_bounds(Box::leak(
            format!("work-control-{index}-0").into_boxed_str(),
        ))
        .unwrap();
    let command = cx
        .debug_bounds(Box::leak(
            format!("work-control-{index}-1").into_boxed_str(),
        ))
        .unwrap();
    assert_eq!(command.left() - editor.right(), px(2.));
}
