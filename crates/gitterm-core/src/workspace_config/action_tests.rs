use super::*;
use crate::{desktop_store::DesktopStore, task_worktree as trees};

// Each scenario runs in its own process: HOME and the absolute config override
// cannot race another test, and neither host can reach the real desktop state.
fn isolated(name: &str, test: impl FnOnce(&Path, &Path)) {
    if std::env::var_os("GITTERM_REPO_ACTION_CHILD").is_some() {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        let root = PathBuf::from(std::env::var_os("GITTERM_V6_CONFIG_DIR").unwrap());
        assert!(root.is_absolute() && root.starts_with(&home));
        test(&home, &root);
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().canonicalize().unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            &format!("workspace_config::repo_action_tests::{name}"),
            "--nocapture",
        ])
        .env("HOME", &home)
        .env("GITTERM_V6_CONFIG_DIR", home.join("v6"))
        .env("GITTERM_REPO_ACTION_CHILD", "1")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", home.join("empty-gitconfig"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
fn git(repo: &Path, args: &[&str]) {
    let result = crate::git_status::git_command()
        .current_dir(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
}
fn repo(home: &Path, name: &str) -> PathBuf {
    let path = home.join(name);
    std::fs::create_dir_all(&path).unwrap();
    git(&path, &["init", "-q", "-b", "main"]);
    git(
        &path,
        &[
            "-c",
            "user.name=fixture",
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
            "initial",
        ],
    );
    path
}
fn document(home: &Path) -> WorkspacesFile {
    let mut file: WorkspacesFile = serde_json::from_value(serde_json::json!({"active_workspace":0,"workspaces":[
        {"name":"Work","abbrev":"W","color":"blue","dir":home,"tabs":[],"env":{"KEEP":"yes"}},
        {"name":"Closed","abbrev":"C","color":"red","dir":home.join("closed"),"active":false,"tabs":[],"parked":{"note":"hold","parked_at":"2026-11-27T00:00:00Z"}}
    ],"collapsed_repo_groups":["agent:fixture"]})).unwrap();
    file.normalize_repos();
    file
}
fn seed(root: &Path, file: &WorkspacesFile) -> DesktopStore {
    std::fs::create_dir_all(root).unwrap();
    std::fs::write(
        root.join("workspaces.json"),
        serde_json::to_vec_pretty(file).unwrap(),
    )
    .unwrap();
    DesktopStore::open(root).unwrap()
}
fn edit(store: &DesktopStore, target: &WorkspaceTarget, edit: RepoEdit) -> WorkspacesFile {
    let target = target.clone();
    store
        .try_edit_workspaces(move |file| file.edit_repo(&target, edit))
        .wait()
        .unwrap()
}
fn read(root: &Path) -> WorkspacesFile {
    serde_json::from_slice(&std::fs::read(root.join("workspaces.json")).unwrap()).unwrap()
}
#[test]
fn rename_add_remove_undo_and_home_protection() {
    isolated(
        "rename_add_remove_undo_and_home_protection",
        |home, root| {
            let main = repo(home, "main");
            let extra = repo(home, "extra");
            let original = document(&main);
            let closed = serde_json::to_value(&original.workspaces[1]).unwrap();
            let store = seed(root, &original);
            let target = WorkspaceTarget::of(&original.workspaces[0]);
            let file = edit(&store, &target, RepoEdit::Rename("  Renamed  ".into()));
            assert_eq!(file.workspaces[0].name, "Renamed");
            let target = WorkspaceTarget::of(&file.workspaces[0]);
            let before = std::fs::read(root.join("workspaces.json")).unwrap();
            edit(&store, &target, RepoEdit::Rename("  ".into()));
            assert_eq!(std::fs::read(root.join("workspaces.json")).unwrap(), before);
            let stale = WorkspaceTarget::of(&original.workspaces[0]);
            assert!(store
                .try_edit_workspaces(move |f| f.edit_repo(&stale, RepoEdit::Rename("Wrong".into())))
                .wait()
                .is_err());
            assert_eq!(std::fs::read(root.join("workspaces.json")).unwrap(), before);
            let file = edit(
                &store,
                &target,
                RepoEdit::Add {
                    dir: extra.clone(),
                    detected_command: Some("cargo run".into()),
                },
            );
            assert_eq!(
                file.workspaces[0].repos,
                [main.to_string_lossy(), extra.to_string_lossy()]
            );
            let before = std::fs::read(root.join("workspaces.json")).unwrap();
            let refused = target.clone();
            let main_copy = main.clone();
            let error = store
                .try_edit_workspaces(move |file| {
                    file.edit_repo(&refused, RepoEdit::Remove(vec![main_copy]))
                })
                .wait()
                .unwrap_err();
            assert_eq!(error, "the home repo stays listed");
            assert_eq!(std::fs::read(root.join("workspaces.json")).unwrap(), before);
            let file = edit(&store, &target, RepoEdit::Remove(vec![extra.clone()]));
            assert_eq!(file.workspaces[0].repos, [main.to_string_lossy()]);
            assert_eq!(file.workspaces[0].repos_removed, [extra.to_string_lossy()]);
            let file = edit(
                &store,
                &target,
                RepoEdit::Add {
                    dir: extra.clone(),
                    detected_command: None,
                },
            );
            assert!(file.workspaces[0].repos_removed.is_empty());
            assert_eq!(file.workspaces[0].repos_added, [extra.to_string_lossy()]);
            // Iced's Undo removes the auto-added repos through the same remove rule.
            let file = edit(&store, &target, RepoEdit::Remove(vec![extra]));
            assert_eq!(file.workspaces[0].repos, [main.to_string_lossy()]);
            assert_eq!(serde_json::to_value(&file.workspaces[1]).unwrap(), closed);
            assert_eq!(file.collapsed_repo_groups, ["agent:fixture"]);
            assert_eq!(read(root).workspaces[0].name, "Renamed");
            assert_eq!(file.workspaces[0].env.get("KEEP").unwrap(), "yes");
        },
    );
}
#[test]
fn run_command_is_shared_and_empty_clears_it() {
    isolated("run_command_is_shared_and_empty_clears_it", |home, root| {
        let main = repo(home, "main");
        std::fs::write(main.join("Cargo.toml"), "[package]\n").unwrap();
        assert_eq!(detect_run_command(&main).as_deref(), Some("cargo run"));
        let mut file = document(&main);
        file.workspaces.push(file.workspaces[0].clone());
        file.workspaces[2].name = "Other".into();
        let target = WorkspaceTarget::of(&file.workspaces[0]);
        let store = seed(root, &file);
        let file = edit(
            &store,
            &target,
            RepoEdit::RunCommand {
                dir: main.clone(),
                draft: "  npm run dev  ".into(),
            },
        );
        assert_eq!(
            file.repo(&RepoKey::local(&main))
                .unwrap()
                .run_command
                .as_deref(),
            Some("npm run dev")
        );
        assert_eq!(
            file.workspaces[2].run_command.as_deref(),
            Some("npm run dev")
        );
        edit(
            &store,
            &target,
            RepoEdit::RunCommand {
                dir: main.clone(),
                draft: " \n ".into(),
            },
        );
        assert!(read(root)
            .repo(&RepoKey::local(&main))
            .unwrap()
            .run_command
            .is_none());
    });
}
#[test]
fn open_adopt_and_remove_disposable_worktree() {
    isolated("open_adopt_and_remove_disposable_worktree", |home, root| {
        let main = repo(home, "main");
        let path = home.join("linked");
        git(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "task/tru-274-adopt",
                path.to_str().unwrap(),
            ],
        );
        trees::check_open_worktree(&main, &path).unwrap();
        assert!(trees::check_open_worktree(&main, home).is_err());
        let file = document(&main);
        let target = WorkspaceTarget::of(&file.workspaces[0]);
        let store = seed(root, &file);
        for _ in 0..2 {
            let t = target.clone();
            let p = path.clone();
            store
                .try_edit_workspaces(move |f| {
                    assert_eq!(f.open_worktree_tab(&t, &p)?, 0);
                    Ok(())
                })
                .wait()
                .unwrap();
        }
        assert_eq!(read(root).workspaces[0].tabs.len(), 1);
        let opened = read(root);
        let tab = &opened.workspaces[0].tabs[0];
        assert_eq!(tab.repo_dir.as_deref(), path.to_str());
        uuid::Uuid::parse_str(tab.session_uid.as_deref().unwrap()).unwrap();
        let resolution = trees::resolve_worktree_adoption_blocking(&trees::AdoptWorktreeRequest {
            repository_path: main.clone(),
            worktree_path: path.clone(),
        })
        .unwrap();
        let mut tasks = crate::tasks::TaskStore::load(root.join("tasks.json")).unwrap();
        trees::persist_adopted_worktree(
            &mut tasks,
            &resolution,
            "Work".into(),
            &path,
            "task-fixture".into(),
            "2026-11-27T00:00:00Z",
        )
        .unwrap();
        let reloaded = crate::tasks::TaskStore::load(root.join("tasks.json")).unwrap();
        let task = reloaded.get("task-fixture").unwrap();
        assert_eq!(task.issue.as_ref().unwrap().key, "TRU-274");
        assert_eq!(task.worktree.path.as_deref(), Some(path.as_path()));
        assert_eq!(task.branch, "task/tru-274-adopt");
        assert_eq!(
            task.created_by.as_ref().unwrap().kind,
            crate::tasks::TaskCreatorKind::Manual
        );
        assert_eq!(
            trees::check_unmanaged_removal(Some(&main), &path, Some(&task.title)).unwrap_err(),
            format!(
                "{} belongs to task \"tru-274-adopt\" — delete it from the task instead",
                path.display()
            )
        );
        // Remove a distinct, unmanaged worktree; adoption must never delete it.
        let disposable = home.join("disposable");
        git(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "disposable",
                disposable.to_str().unwrap(),
            ],
        );
        trees::check_unmanaged_removal(Some(&main), &disposable, None).unwrap();
        trees::remove_task_worktree_blocking(&main, &disposable, false).unwrap();
        assert!(!disposable.exists());
        assert!(path.exists());
    });
}
#[test]
fn dirty_current_managed_refusals_and_explicit_force() {
    isolated(
        "dirty_current_managed_refusals_and_explicit_force",
        |home, _root| {
            let main = repo(home, "main");
            let path = home.join("dirty");
            git(
                &main,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "-b",
                    "dirty",
                    path.to_str().unwrap(),
                ],
            );
            assert_eq!(
                trees::check_unmanaged_removal(Some(&main), &main, None).unwrap_err(),
                "The main checkout cannot be removed"
            );
            assert_eq!(
                trees::check_unmanaged_removal(None, &path, None).unwrap_err(),
                "Removing a worktree needs a local workspace"
            );
            assert!(
                trees::check_unmanaged_removal(Some(&main), &path, Some("Owned"))
                    .unwrap_err()
                    .contains("delete it from the task instead")
            );
            std::fs::write(path.join("unsaved.txt"), "do not lose me").unwrap();
            let error = trees::remove_task_worktree_blocking(&main, &path, false)
                .unwrap_err()
                .to_string();
            assert!(trees::offer_force(&error), "{error}");
            assert_eq!(
                std::fs::read_to_string(path.join("unsaved.txt")).unwrap(),
                "do not lose me"
            );
            assert!(trees::removal_force(true, None).is_err());
            assert!(trees::removal_force(true, Some("not registered")).is_err());
            assert!(trees::removal_force(true, Some(&error)).unwrap());
            // The explicit Force remove path is the only caller passing true.
            trees::remove_task_worktree_blocking(&main, &path, true).unwrap();
            assert!(!path.exists());
            assert!(!trees::offer_force("path is not a registered worktree"));
            git(&main, &["show-ref", "--verify", "refs/heads/dirty"]);
        },
    );
}
