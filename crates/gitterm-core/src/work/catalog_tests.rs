use super::*;
use crate::work::summary::{assign_chat, owning_workspace, resume_target, ResumeTarget};
use crate::workspace::{bar_layout, visible_order, BarChip};
use std::path::{Path, PathBuf};

#[test]
fn name_and_optional_repos_preserve_iced_validation() {
    let mut sheet = NewWorkSheet {
        decisions_repo: Some("/api".into()),
        ..Default::default()
    };
    assert_eq!(
        sheet.validate().unwrap_err(),
        "Name the work, for example \"Cloud migration\"."
    );
    sheet.name = " Cloud  migration ".into();
    assert_eq!(
        sheet.validate().unwrap_err(),
        "The decisions repo /api is not one of the starting repos."
    );
    sheet.use_picked_folder("/api".into());
    let (name, start) = sheet.validate().unwrap();
    assert_eq!(name, "Cloud migration");
    assert_eq!(start.decisions_repo, Some("/api".into()));
    assert_eq!(
        validate_work_name("\t Cloud \n migration  ").unwrap(),
        "Cloud migration"
    );
    assert_eq!(
        validate_work_name("\u{2003} \n").unwrap_err(),
        "Name the work, for example \"Cloud migration\"."
    );
    // Iced imposes no length, punctuation, existence, or uniqueness checks here.
    assert_eq!(validate_work_name(" / .. ").unwrap(), "/ ..");
    assert_eq!(WorkStart::new(vec![], None).unwrap(), WorkStart::default());
    let repos = vec![
        PathBuf::from("/web"),
        PathBuf::from("/api"),
        PathBuf::from("/web"),
    ];
    let start = WorkStart::new(repos.clone(), Some("/api".into())).unwrap();
    assert_eq!(start.repos, repos);
    assert_eq!(start.decisions_repo, Some("/api".into()));
    assert_eq!(
        WorkStart::new(vec![], Some("/api".into())).unwrap_err(),
        "The decisions repo /api is not one of the starting repos."
    );
}

#[test]
fn picked_repos_clear_errors_and_decisions_and_limit_shortcuts() {
    let mut sheet = NewWorkSheet {
        name: "Keep typed name".into(),
        error: Some("stale".into()),
        recent: (0..8).map(|i| PathBuf::from(format!("/r{i}"))).collect(),
        ..Default::default()
    };
    sheet.use_picked_folder("/r1".into());
    sheet.use_picked_folder("/r1".into());
    sheet.use_picked_folder("/r2".into());
    assert_eq!(sheet.repos, [PathBuf::from("/r1"), PathBuf::from("/r2")]);
    assert_eq!(sheet.name, "Keep typed name");
    assert_eq!(sheet.error, None);
    assert_eq!(
        sheet
            .recent_shortcuts()
            .into_iter()
            .cloned()
            .collect::<Vec<_>>(),
        ["/r0", "/r3", "/r4", "/r5", "/r6"].map(PathBuf::from)
    );
    sheet.decisions_repo = Some("/r1".into());
    sheet.remove_repo(Path::new("/r1"));
    assert_eq!(sheet.decisions_repo, None);
    assert_eq!(
        new_work_recent_repos(
            [
                "/api",
                "/api",
                "/a/.worktrees/task",
                "/a/.claude/worktrees/task",
                "/folder",
                "/web"
            ]
            .map(PathBuf::from),
            |p| p != Path::new("/folder")
        ),
        [PathBuf::from("/api"), PathBuf::from("/web")]
    );
}

#[test]
fn catalog_preserves_host_order_running_park_and_timestamp_rules() {
    use ExistingWorkStatus::*;
    let chats = vec!["chat".into()];
    let open = [
        OpenWork {
            uid: 1,
            name: "running",
            dir: "/run",
            chats: &chats,
            claims_new_chats: false,
            running_park: true,
        },
        OpenWork {
            uid: 2,
            name: "fresh",
            dir: "/fresh",
            chats: &[],
            claims_new_chats: true,
            running_park: false,
        },
        OpenWork {
            uid: 3,
            name: "folder",
            dir: "/folder",
            chats: &[],
            claims_new_chats: false,
            running_park: false,
        },
    ];
    let closed = [
        ClosedWork {
            name: "old",
            dir: "/old",
            chats: &chats,
            active: false,
            parked_at: None,
            done_at: None,
        },
        ClosedWork {
            name: "new",
            dir: "/new",
            chats: &chats,
            active: false,
            parked_at: Some("2026-11-01T00:00:00Z"),
            done_at: None,
        },
        ClosedWork {
            name: "same",
            dir: "/same",
            chats: &chats,
            active: false,
            parked_at: Some("2026-11-01T01:00:00+01:00"),
            done_at: None,
        },
        ClosedWork {
            name: "invalid",
            dir: "/invalid",
            chats: &chats,
            active: false,
            parked_at: Some("invalid"),
            done_at: None,
        },
        ClosedWork {
            name: "done invalid",
            dir: "/di",
            chats: &chats,
            active: false,
            parked_at: None,
            done_at: Some("bad"),
        },
        ClosedWork {
            name: "done new",
            dir: "/dn",
            chats: &chats,
            active: false,
            parked_at: None,
            done_at: Some("2026-11-02T00:00:00Z"),
        },
        ClosedWork {
            name: "empty",
            dir: "/empty",
            chats: &[],
            active: false,
            parked_at: None,
            done_at: None,
        },
    ];
    assert_eq!(parked_order(&closed), [1, 2, 0, 3, 6]);
    let work = existing_work(&open, &closed);
    assert_eq!(
        work.iter()
            .map(|w| (w.name.as_str(), w.status))
            .collect::<Vec<_>>(),
        [
            ("running", Parked),
            ("fresh", Open),
            ("new", Parked),
            ("same", Parked),
            ("old", Parked),
            ("invalid", Parked),
            ("done new", Done),
            ("done invalid", Done)
        ]
    );
    assert_eq!(work[1].action, Some(ExistingWorkAction::Switch(2)));
    assert!(matches!(&work[0].action, Some(ExistingWorkAction::Resume(key)) if key.dir == "/run"));
    assert!(matches!(&work[6].action, Some(ExistingWorkAction::Reopen(key)) if key.dir == "/dn"));
    assert_eq!(
        existing_work_matches(&work, " ")
            .iter()
            .map(|w| w.name.as_str())
            .collect::<Vec<_>>(),
        ["fresh", "running", "new", "same", "old"]
    );
    assert_eq!(existing_work_matches(&work, "DONE").len(), 2);
}

#[test]
fn folder_root_selection_excludes_owners_and_remotes_and_uses_last_depth_tie() {
    let owned = vec!["root".into()];
    let facts = [
        FolderWorkspace {
            dir: Path::new("/repo"),
            local: true,
            chats: &[],
        },
        FolderWorkspace {
            dir: Path::new("/repo/api"),
            local: true,
            chats: &[],
        },
        FolderWorkspace {
            dir: Path::new("/repo/api/sub"),
            local: true,
            chats: &owned,
        },
        FolderWorkspace {
            dir: Path::new("/repo/api/sub"),
            local: false,
            chats: &[],
        },
        FolderWorkspace {
            dir: Path::new("/repo/api"),
            local: true,
            chats: &[],
        },
    ];
    assert_eq!(
        local_workspace_index(facts, Path::new("/repo/api/sub/task"), Path::new("/other")),
        Some(4)
    );
    assert_eq!(
        local_workspace_index(facts, Path::new("/worktree"), Path::new("/repo/api")),
        Some(4)
    );
    assert_eq!(
        local_workspace_index(facts, Path::new("/repo-other"), Path::new("/other")),
        None
    );
    assert_eq!(home_dir(None, "/legacy"), "/legacy");
    assert_eq!(home_dir(Some(""), "/legacy"), "");
}

#[test]
fn runtime_claims_do_not_steal_task_or_already_owned_conversations() {
    let owned = vec!["root".into()];
    assert!(!owns_chats(&[]));
    assert!(owns_chats(&owned));
    for (chats, claims) in [
        (&[][..], false),
        (&[][..], true),
        (owned.as_slice(), false),
        (owned.as_slice(), true),
    ] {
        assert_eq!(is_work(chats, claims), !chats.is_empty() || claims);
        for task in [None, Some("task")] {
            for already_owned in [false, true] {
                assert_eq!(
                    should_claim_chat(chats, claims, task, already_owned),
                    (!chats.is_empty() || claims) && task.is_none() && !already_owned
                );
            }
        }
    }
    let open = [vec![], owned.clone()];
    let closed = [owned.clone()];
    assert_eq!(
        owning_workspace(open.iter().map(Vec::as_slice), "child", Some("root")),
        Some(1)
    );
    assert_eq!(
        resume_target(
            open.iter().map(Vec::as_slice),
            closed.iter().map(Vec::as_slice),
            "child",
            Some("root"),
            Some(0)
        ),
        ResumeTarget::OwnerOpen(1)
    );
    let mut lists = [
        vec!["root".into(), "other".into()],
        vec![],
        vec!["root".into()],
    ];
    assert!(assign_chat(
        lists.iter_mut().enumerate().map(|(i, v)| (i == 1, v)),
        "root"
    ));
    assert_eq!(
        lists,
        [vec!["other".to_string()], vec!["root".into()], vec![]]
    );
}

#[test]
fn bar_order_reuses_core_with_menu_sections_and_active_remote_pin() {
    let chips = [
        ("local", false, false),
        ("agent:r", true, false),
        ("local", true, false),
        ("local", true, true),
        ("local", false, false),
    ]
    .map(|(group, work, parked)| BarChip {
        group: group.into(),
        work,
        parked,
    });
    let shown = bar_layout(&chips, 1, true);
    assert_eq!(shown.work, [2]);
    assert_eq!(shown.after, [0, 1, 4]);
    assert_eq!(visible_order(&shown), [2, 0, 1, 4]);
    assert_eq!(shown.sections[0].workspaces, [0, 4]);
    assert_eq!(shown.sections[1].workspaces, [1]);
    assert_eq!(visible_order(&bar_layout(&chips, 1, false)), [2, 1]);
}
