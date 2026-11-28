//! Live effects over Iced's PR watch and row-action contract. No new saved schema.
use super::pr_actions::{ActionKind, PrActionConfirm, RunPrAction};
use crate::{
    merge_plan::PrKey,
    prs::{self, Fetched, PrSnapshot, WatchFile, WatchedPr},
};
use std::{collections::BTreeMap, path::Path, time::Duration};

/// The boundary tests replace. A credential is resolved once and used for both
/// the fresh read and the write, rather than consulting a mutable active login.
pub trait Github {
    type Credential;
    fn branch_prs(
        &mut self,
        _repo: &str,
        _branch: &str,
        _account: Option<String>,
    ) -> Result<Vec<prs::BranchPr>, String> {
        Err("Branch PR reader is not configured".into())
    }
    fn identity(&mut self, account: Option<&str>) -> Result<(Self::Credential, String), String>;
    fn fetch(
        &mut self,
        key: &PrKey,
        previous: &PrSnapshot,
        etags: &BTreeMap<String, String>,
        credential: &Self::Credential,
    ) -> Result<Fetched, String>;
    fn run(
        &mut self,
        run: &RunPrAction,
        head: &str,
        credential: &Self::Credential,
    ) -> Result<String, String>;
}

pub struct Gh(pub tokio::runtime::Runtime);
impl Gh {
    pub fn new() -> Result<Self, String> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map(Self)
            .map_err(|e| format!("PR runtime: {e}"))
    }
}
impl Github for Gh {
    type Credential = String;
    fn branch_prs(
        &mut self,
        repo: &str,
        branch: &str,
        account: Option<String>,
    ) -> Result<Vec<prs::BranchPr>, String> {
        self.0.block_on(prs::branch_prs(repo, branch, account))
    }
    fn identity(&mut self, account: Option<&str>) -> Result<(String, String), String> {
        self.0.block_on(async {
            // Pin even the active account to a token, so changing gh's active
            // account after validation cannot redirect the write.
            let user = match account {
                Some(user) => user.to_string(),
                None => prs::login_of(None).await?,
            };
            let token = prs::account_token(&user).await?;
            let login = prs::login_of(Some(&token)).await?;
            Ok((token, login))
        })
    }
    fn fetch(
        &mut self,
        key: &PrKey,
        previous: &PrSnapshot,
        etags: &BTreeMap<String, String>,
        token: &String,
    ) -> Result<Fetched, String> {
        self.0
            .block_on(prs::fetch_pr(key, previous, etags, Some(token)))
    }
    fn run(&mut self, run: &RunPrAction, head: &str, token: &String) -> Result<String, String> {
        self.0.block_on(prs::run_action_with_token(
            run.action,
            run.key.clone(),
            Some(token),
            run.account.as_deref(),
            Some(head),
        ))
    }
}

/// Ephemeral opening facts; never serialized into Iced's files.
#[derive(Clone, Debug)]
pub struct Guard {
    key: PrKey,
    snapshot: PrSnapshot,
    login: String,
}
impl Guard {
    pub fn capture(watched: Option<&WatchedPr>, login: Option<&str>) -> Result<Self, String> {
        let snapshot = watched
            .filter(|w| w.error.is_none())
            .map(|w| w.snapshot.clone())
            .ok_or("PR has not been read successfully; refresh and reopen")?;
        if snapshot.core.as_ref().is_none_or(|c| c.head_sha.is_empty()) {
            return Err("PR head is unknown; refresh and reopen".into());
        }
        let login = login
            .filter(|login| prs::valid_login(login))
            .ok_or("Account is unknown; refresh and reopen")?;
        Ok(Self {
            key: watched.expect("snapshot came from watched PR").key.clone(),
            snapshot,
            login: login.to_string(),
        })
    }

    pub fn head(&self) -> &str {
        &self
            .snapshot
            .core
            .as_ref()
            .expect("capture requires a head")
            .head_sha
    }

    /// Called only by Confirm. Cancel discards this guard without any effect.
    pub fn execute(
        &self,
        confirm: &mut PrActionConfirm,
        github: &mut (impl Github + ?Sized),
    ) -> Result<String, String> {
        if confirm.key.id() != self.key.id() {
            return Err("PR confirmation target changed; reopen".into());
        }
        if !confirm.run_enabled() {
            return Err("PR action is unavailable".into());
        }
        let (credential, login) = github.identity(confirm.account.as_deref())?;
        if !login.eq_ignore_ascii_case(&self.login)
            || confirm
                .account
                .as_deref()
                .is_some_and(|expected| !login.eq_ignore_ascii_case(expected))
        {
            return Err("Signed-in account changed or is wrong; refresh and reopen".into());
        }
        let fetched = github.fetch(&confirm.key, &self.snapshot, &BTreeMap::new(), &credential)?;
        let before = self
            .snapshot
            .core
            .as_ref()
            .ok_or("Opening PR head is missing")?;
        let after = fetched
            .snapshot
            .core
            .as_ref()
            .ok_or("Fresh PR head is missing")?;
        if before.head_sha != after.head_sha {
            return Err("PR head moved; refresh and reopen".into());
        }
        let fresh = &fetched.snapshot;
        if self.snapshot.checks_sha != fresh.checks_sha
            || self.snapshot.check_runs != fresh.check_runs
            || self.snapshot.statuses != fresh.statuses
            || self.snapshot.required != fresh.required
            || self.snapshot.required_rules != fresh.required_rules
            || self.snapshot.had_checks != fresh.had_checks
        {
            return Err("PR checks changed; refresh and reopen".into());
        }
        let author = after.author.clone();
        let mut latest = WatchedPr::new(confirm.key.clone());
        latest.snapshot = fetched.snapshot;
        let gate = super::pr_actions::RowGate::of(Some(&latest));
        if match confirm.kind {
            ActionKind::Ready => !gate.draft,
            _ => !gate.can_act(),
        } {
            return Err(
                "PR state or checks no longer allow this action; refresh and reopen".into(),
            );
        }
        if confirm.kind == ActionKind::Approve && prs::is_own_pr(Some(&author), Some(&login)) {
            return Err("Cannot approve your own PR".into());
        }
        let run = confirm.run(Some(&latest)).ok_or("PR action was refused")?;
        github.run(&run, &before.head_sha, &credential)
    }
}

/// Iced's due/interval/absorb/failure rules, with its exact WatchFile encoding.
/// The host serializes calls and supplies open/parked workspace intervals.
pub fn poll_and_save(
    watch: &mut WatchFile,
    wanted: impl IntoIterator<Item = (PrKey, Duration)>,
    accounts: &BTreeMap<String, String>,
    path: &Path,
    github: &mut (impl Github + ?Sized),
) -> Result<BTreeMap<Option<String>, String>, String> {
    let now = chrono::Utc::now();
    let started = std::time::Instant::now();
    let mut logins = BTreeMap::new();
    let mut seen = std::collections::HashSet::new();
    let mut count = 0;
    let mut identities = BTreeMap::new();
    for (key, interval) in wanted {
        if !seen.insert(key.id()) {
            continue;
        }
        let watched = watch
            .prs
            .entry(key.id())
            .or_insert_with(|| WatchedPr::new(key.clone()));
        if !watched.due(watched.interval_for(interval), now) {
            continue;
        }
        if count >= 25 || started.elapsed() >= prs::POLL_DEADLINE {
            break;
        }
        count += 1;
        let account = prs::account_for(key.owner(), accounts);
        let identity = identities.entry(account.clone()).or_insert_with(|| {
            let (credential, login) = github.identity(account.as_deref())?;
            if account
                .as_deref()
                .is_some_and(|expected| !expected.eq_ignore_ascii_case(&login))
            {
                return Err("Signed-in account does not match github_accounts".to_string());
            }
            Ok((credential, login))
        });
        let result = match identity {
            Ok((credential, login)) => {
                logins.insert(account, login.clone());
                github
                    .fetch(&key, &watched.snapshot, &watched.etags, credential)
                    .map(|fetched| (fetched, login.clone()))
            }
            Err(error) => Err(error.clone()),
        };
        let at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        match result {
            Ok((fetched, login)) => {
                watched.absorb(fetched, &login, &at);
            }
            Err(error) => watched.failed(error, &at),
        }
    }
    if count > 0 {
        watch.save(path)?;
    }
    Ok(logins)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        merge_plan::{MergePlan, PrState},
        prs::{CheckItem, CheckOutcome, PullCore},
    };
    #[derive(Clone)]
    struct Fake {
        login: String,
        snapshot: PrSnapshot,
        writes: Vec<RunPrAction>,
        reads: usize,
    }
    impl Github for Fake {
        type Credential = String;
        fn identity(&mut self, _: Option<&str>) -> Result<(String, String), String> {
            Ok(("fixture-credential".into(), self.login.clone()))
        }
        fn fetch(
            &mut self,
            _: &PrKey,
            _: &PrSnapshot,
            _: &BTreeMap<String, String>,
            credential: &String,
        ) -> Result<Fetched, String> {
            assert_eq!(credential, "fixture-credential");
            self.reads += 1;
            Ok(Fetched {
                snapshot: self.snapshot.clone(),
                etags: Default::default(),
            })
        }
        fn run(
            &mut self,
            run: &RunPrAction,
            head: &str,
            credential: &String,
        ) -> Result<String, String> {
            assert_eq!(head, "head-one");
            assert_eq!(credential, "fixture-credential");
            self.writes.push(run.clone());
            Ok("done".into())
        }
    }
    fn fixture(kind: ActionKind) -> (WatchedPr, PrActionConfirm, Fake) {
        let mut watched = WatchedPr::new(PrKey::new("owner/repo", 53));
        watched.snapshot = PrSnapshot {
            core: Some(PullCore {
                title: "Example change".into(),
                head_sha: "head-one".into(),
                author: "author".into(),
                state: Some(if kind == ActionKind::Ready {
                    PrState::Draft
                } else {
                    PrState::Open
                }),
                ..Default::default()
            }),
            checks_sha: "head-one".into(),
            check_runs: vec![CheckItem {
                name: "build".into(),
                outcome: CheckOutcome::Pass,
            }],
            ..Default::default()
        };
        let confirm = PrActionConfirm::open(
            watched.key.clone(),
            kind,
            [],
            &[("owner".into(), "reviewer".into())].into(),
        );
        let fake = Fake {
            login: "reviewer".into(),
            snapshot: watched.snapshot.clone(),
            writes: Vec::new(),
            reads: 0,
        };
        (watched, confirm, fake)
    }
    #[test]
    fn live_pr_wrong_account_refuses_all_writes() {
        for kind in [ActionKind::Approve, ActionKind::Merge, ActionKind::Ready] {
            let (watched, mut confirm, mut fake) = fixture(kind);
            let guard = Guard::capture(Some(&watched), Some("reviewer")).unwrap();
            fake.login = "other".into();
            assert!(guard
                .execute(&mut confirm, &mut fake)
                .unwrap_err()
                .contains("account"));
            assert!(fake.writes.is_empty());
            assert_eq!(fake.reads, 0);
        }
    }
    #[test]
    fn live_pr_head_moved_refuses_all_writes() {
        for kind in [ActionKind::Approve, ActionKind::Merge, ActionKind::Ready] {
            let (watched, mut confirm, mut fake) = fixture(kind);
            let guard = Guard::capture(Some(&watched), Some("reviewer")).unwrap();
            fake.snapshot.core.as_mut().unwrap().head_sha = "head-two".into();
            assert!(guard
                .execute(&mut confirm, &mut fake)
                .unwrap_err()
                .contains("head moved"));
            assert!(fake.writes.is_empty());
        }
    }
    #[test]
    fn live_pr_checks_changed_refuses_even_when_still_passing() {
        for kind in [ActionKind::Approve, ActionKind::Merge, ActionKind::Ready] {
            let (watched, mut confirm, mut fake) = fixture(kind);
            let guard = Guard::capture(Some(&watched), Some("reviewer")).unwrap();
            fake.snapshot.required.push("build".into());
            assert!(guard
                .execute(&mut confirm, &mut fake)
                .unwrap_err()
                .contains("checks changed"));
            assert!(fake.writes.is_empty());
        }
    }
    #[test]
    fn live_pr_confirm_dispatches_each_shared_action_once() {
        for kind in [ActionKind::Approve, ActionKind::Merge, ActionKind::Ready] {
            let (watched, mut confirm, mut fake) = fixture(kind);
            let guard = Guard::capture(Some(&watched), Some("reviewer")).unwrap();
            assert_eq!(guard.execute(&mut confirm, &mut fake).unwrap(), "done");
            assert_eq!(fake.writes[0].action, confirm.action());
            assert!(guard.execute(&mut confirm, &mut fake).is_err());
            assert_eq!(fake.writes.len(), 1);
        }
    }
    #[test]
    fn live_pr_poll_survives_iced_reload_with_identical_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = WatchFile::path_in(dir.path());
        let (watched, _, mut fake) = fixture(ActionKind::Merge);
        let mut watch = WatchFile::default();
        poll_and_save(
            &mut watch,
            [(watched.key.clone(), prs::OPEN_INTERVAL)],
            &BTreeMap::new(),
            &path,
            &mut fake,
        )
        .unwrap();
        let first = std::fs::read(&path).unwrap();
        let mut reloaded = WatchFile::load(&path);
        assert_eq!(reloaded, watch);
        assert_eq!(reloaded.prs[&watched.key.id()].snapshot, fake.snapshot);
        assert!(reloaded.prs[&watched.key.id()].checked_at.is_some());
        poll_and_save(
            &mut reloaded,
            [(watched.key, prs::OPEN_INTERVAL)],
            &BTreeMap::new(),
            &path,
            &mut fake,
        )
        .unwrap();
        assert_eq!(fake.reads, 1);
        assert_eq!(std::fs::read(path).unwrap(), first);
    }
    #[test]
    fn live_pr_merge_plan_contains_the_complete_iced_contract() {
        let (watched, _, _) = fixture(ActionKind::Merge);
        let plan: MergePlan = serde_json::from_value(serde_json::json!({"root_chat_id":"root", "updated_at":"2026-11-01T00:00:00Z", "source":"agent", "entries":[{"repo":"owner/repo", "number":53, "step":1, "method":"squash", "note":"example"}]})).unwrap();
        let watch = WatchFile {
            prs: [(watched.key.id(), watched.clone())].into(),
        };
        let value = prs::merge_plan_value(
            "root",
            Some(&plan),
            &watch,
            vec![crate::merge_plan::PrRef {
                repo: "owner/repo".into(),
                number: 54,
                url: "https://github.com/owner/repo/pull/54".into(),
            }],
        );
        assert_eq!(value["root_chat_id"], "root");
        assert_eq!(value["next"], "owner/repo#53");
        for field in ["updated_at", "source", "entries", "unplanned_prs"] {
            assert!(!value[field].is_null(), "{field}");
        }
        let entry = &value["entries"][0];
        for field in [
            "position",
            "unordered_reason",
            "pr",
            "repo",
            "number",
            "url",
            "step",
            "after",
            "stacked_on",
            "hold",
            "method",
            "note",
            "status",
            "waits_for",
            "live",
        ] {
            assert!(entry.get(field).is_some(), "{field}");
        }
        for field in [
            "title",
            "state",
            "head",
            "base",
            "checks",
            "failing_checks",
            "required_checks_passed",
            "review",
            "conflicting",
            "checked_at",
            "error",
        ] {
            assert!(entry["live"].get(field).is_some(), "{field}");
        }
        assert_eq!(value["unplanned_prs"][0]["pr"], "owner/repo#54");
    }
}
