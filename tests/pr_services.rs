//! TRU-274 row 21: synthetic pre-extraction contracts, exercised through facades.
use gitterm::{issue_titles::IssueTitlesFile, merge_plan, pr_watch};
use serde_json::Value;

#[test]
fn persisted_bytes_and_failed_saves() {
    let input: Value =
        serde_json::from_str(include_str!("fixtures/stage45/identity/formats.input.json")).unwrap();
    for label in ["old", "current"] {
        let dir = tempfile::tempdir().unwrap();
        let case = &input[label];
        let watch: pr_watch::WatchFile = serde_json::from_value(case["watch"].clone()).unwrap();
        let issues: IssueTitlesFile = serde_json::from_value(case["issues"].clone()).unwrap();
        let plan: merge_plan::MergePlan =
            serde_json::from_value(case["merge_plan"].clone()).unwrap();
        let watch_path = pr_watch::WatchFile::path_in(dir.path());
        let issues_path = IssueTitlesFile::path_in(dir.path());
        let plans = merge_plan::plans_dir(dir.path());
        watch.save(&watch_path).unwrap();
        issues.save(&issues_path).unwrap();
        merge_plan::write_plan(&plans, &plan).unwrap();
        assert_eq!(watch_path.file_name().unwrap(), "pr-watch.json");
        assert_eq!(issues_path.file_name().unwrap(), "issue-titles.json");
        assert_eq!(plans.file_name().unwrap(), "merge-plans");
        for (name, path) in [
            ("watch", &watch_path),
            ("issues", &issues_path),
            (
                "merge-plan",
                &merge_plan::plan_path(&plans, &plan.root_chat_id).unwrap(),
            ),
        ] {
            let bytes = std::fs::read(path).unwrap();
            let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(format!(
                "tests/fixtures/stage45/pr-services/{label}.{name}.json"
            ));
            assert_eq!(bytes, std::fs::read(fixture).unwrap());
        }
        assert_eq!(pr_watch::WatchFile::load(&watch_path), watch);
        assert_eq!(IssueTitlesFile::load(&issues_path), issues);
        assert_eq!(merge_plan::load_plans(&plans)[&plan.root_chat_id], plan);
        let blocked = dir.path().join("blocked");
        std::fs::write(&blocked, b"sentinel").unwrap();
        assert!(watch.save(&blocked.join("watch.json")).is_err());
        assert!(issues.save(&blocked.join("issues.json")).is_err());
        assert!(merge_plan::write_plan(&blocked, &plan).is_err());
        assert_eq!(std::fs::read(&blocked).unwrap(), b"sentinel");
        for root in ["", "../escape", "a/b", "a\\b", ".", "root.json", "☃"] {
            let mut unsafe_plan = plan.clone();
            unsafe_plan.root_chat_id = root.into();
            assert!(merge_plan::write_plan(&plans, &unsafe_plan).is_err());
        }
        assert!(merge_plan::plan_path(&plans, "root_1-safe").is_ok());
        std::fs::write(&watch_path, b"{broken").unwrap();
        std::fs::write(&issues_path, b"{broken").unwrap();
        assert_eq!(pr_watch::WatchFile::load(&watch_path), Default::default());
        assert_eq!(IssueTitlesFile::load(&issues_path), Default::default());
        assert_eq!(std::fs::read(&watch_path).unwrap(), b"{broken");
        assert_eq!(std::fs::read(&issues_path).unwrap(), b"{broken");
    }
}

#[cfg(unix)]
#[test]
fn account_commands_are_isolated() {
    use pr_watch::{PrAction, PrSnapshot};
    use std::{collections::BTreeMap, os::unix::fs::PermissionsExt, time::Duration};
    if std::env::var_os("GT_PR21_CHILD").is_none() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("gh");
        std::fs::write(&bin, include_str!("fixtures/stage45/pr-services/gh.sh")).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "account_commands_are_isolated", "--nocapture"])
            .env("GT_PR21_CHILD", dir.path())
            .env("PATH", dir.path())
            .env("HOME", dir.path())
            .env("GH_CONFIG_DIR", dir.path().join("gh-config"))
            .env("XDG_CONFIG_HOME", dir.path())
            .env("GH_TOKEN", "fixture-inherited-gh")
            .env("GITHUB_TOKEN", "fixture-inherited-github")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let dir = std::path::PathBuf::from(std::env::var_os("GT_PR21_CHILD").unwrap());
    let calls = dir.join("calls");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let accounts = BTreeMap::from([
            ("Acme".into(), "alice".into()),
            ("Globex".into(), "bob".into()),
        ]);
        let key = merge_plan::PrKey::new("acme/api", 7);
        let alice = pr_watch::account_for("ACME", &accounts);
        let bob = pr_watch::account_for("globex", &accounts);
        assert_eq!(
            pr_watch::run_action(PrAction::Approve, key.clone(), alice.clone())
                .await
                .unwrap(),
            "done"
        );
        assert_eq!(
            pr_watch::run_action(
                PrAction::Merge(merge_plan::MergeMethod::Squash),
                key.clone(),
                bob.clone()
            )
            .await
            .unwrap(),
            "done"
        );
        assert_eq!(
            pr_watch::run_action(PrAction::Ready, key.clone(), None)
                .await
                .unwrap(),
            "done"
        );
        for account in ["failed", "empty", "bad user"] {
            assert!(
                pr_watch::run_action(PrAction::Approve, key.clone(), Some(account.into()))
                    .await
                    .is_err()
            );
        }
        let items = [
            ("acme/api", alice.clone()),
            ("globex/api", bob),
            ("acme/web", alice),
            ("active/api", None),
            ("failed/api", Some("failed".into())),
        ]
        .into_iter()
        .map(|(repo, account)| pr_watch::PollItem {
            key: merge_plan::PrKey::new(repo, 7),
            account,
            snapshot: PrSnapshot::default(),
            etags: BTreeMap::new(),
        })
        .collect();
        let outcome = pr_watch::poll(pr_watch::PollRequest { items }).await;
        assert_eq!(outcome.results.len(), 5);
        assert_eq!(outcome.skipped, 0);
        assert_eq!(
            outcome.logins,
            [
                (None, "active".into()),
                (Some("alice".into()), "alice".into()),
                (Some("bob".into()), "bob".into())
            ]
        );
        assert_eq!(
            outcome
                .results
                .iter()
                .filter(|r| r.fetched.is_err())
                .count(),
            1
        );
        let (first, _) = outcome.results[0].fetched.as_ref().unwrap();
        let reread =
            pr_watch::fetch_pr(&outcome.results[0].key, &first.snapshot, &first.etags, None)
                .await
                .unwrap();
        assert_eq!(reread.snapshot, first.snapshot);
        assert_eq!(reread.etags, first.etags);
        let denied = pr_watch::fetch_pr(
            &merge_plan::PrKey::new("denied/api", 7),
            &PrSnapshot::default(),
            &BTreeMap::new(),
            None,
        )
        .await
        .unwrap_err();
        assert_eq!(
            denied,
            "gh api repos/denied/api/pulls/7: HTTP 403: fixture denied"
        );
        let child = tokio::spawn(pr_watch::run_action(
            PrAction::Ready,
            merge_plan::PrKey::new("acme/api", 99),
            None,
        ));
        tokio::time::timeout(Duration::from_secs(5), async {
            while !dir.join("pid").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("fake gh started");
        child.abort();
        assert!(child.await.unwrap_err().is_cancelled());
        let pid: i32 = std::fs::read_to_string(dir.join("pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let mut alive = true;
        for _ in 0..100 {
            // The only PID probed belongs to the synthetic sleep child above.
            alive = unsafe { libc::kill(pid, 0) } == 0;
            if !alive {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!alive, "cancelled fake gh was not reaped");
    });
    let text = std::fs::read_to_string(calls).unwrap();
    assert!(!text.contains("fixture-inherited"));
    assert!(
        text.contains("fixture-alice||pr review 7 --repo acme/api --approve"),
        "{text}"
    );
    assert!(
        text.contains("fixture-bob||pr merge 7 --repo acme/api --squash"),
        "{text}"
    );
    assert!(text.contains("||pr ready 7 --repo acme/api"), "{text}");
    assert_eq!(
        text.lines()
            .filter(|line| line.contains("pr review "))
            .count(),
        1
    );
    assert_eq!(
        text.lines()
            .filter(|line| line.ends_with("auth token --user alice"))
            .count(),
        2
    ); // one action, one grouped poll
    for (token, repo) in [
        ("fixture-alice", "acme/api"),
        ("fixture-alice", "acme/web"),
        ("fixture-bob", "globex/api"),
        ("", "active/api"),
    ] {
        assert!(
            text.contains(&format!(
                "{token}||api -i -H Accept: application/vnd.github+json repos/{repo}/pulls/7"
            )),
            "{text}"
        );
    }
    assert!(!text.contains("auth switch"));
    assert_eq!(std::env::var("GH_TOKEN").unwrap(), "fixture-inherited-gh");
    assert_eq!(
        std::env::var("GITHUB_TOKEN").unwrap(),
        "fixture-inherited-github"
    );
}
