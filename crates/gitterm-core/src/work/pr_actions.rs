//! The Work sidebar's PR row actions (TRU-274): the Approve / Merge / Ready
//! confirmation and its gating, and the row's other writes (clear a hold,
//! dismiss news, Review's unseen-comment reset and checkout choice).
//!
//! Frontends draw these and execute the effects: the `gh` run of a
//! [`RunPrAction`] (`prs::run_action`, reported back to [`finish_action`]),
//! the plan write after [`clear_hold`], the watch save after
//! [`dismiss_news`] / [`take_unseen_comments`], and the slug lookup behind
//! [`review_checkout`].

use super::summary::BranchStatus;
use crate::merge_plan::{MergeMethod, MergePlan, PrKey, PrState};
use crate::prs::{self, ChecksGate, PrAction, WatchFile, WatchedPr};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Which row action the confirmation runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    Approve,
    Merge,
    /// Mark a draft ready for review.
    Ready,
}

/// The Approve / Merge / Ready confirmation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrActionConfirm {
    pub key: PrKey,
    pub kind: ActionKind,
    pub method: MergeMethod,
    /// The plan named the method.
    pub method_from_plan: bool,
    /// The mapped gh account; `None` for gh's active account.
    pub account: Option<String>,
    pub running: bool,
    /// Why the last run failed; a run that succeeds closes the sheet.
    pub error: Option<String>,
}

/// The `gh` run a confirmation asked for. The frontend executes it with
/// `prs::run_action` and reports the result to [`finish_action`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunPrAction {
    pub action: PrAction,
    pub key: PrKey,
    pub account: Option<String>,
}

impl RunPrAction {
    /// The `gh` argv, without the binary.
    pub fn args(&self) -> Vec<String> {
        prs::action_args(self.action, &self.key)
    }

    pub fn command_line(&self) -> String {
        prs::action_command_line(self.action, &self.key, self.account.as_deref())
    }
}

impl PrActionConfirm {
    /// Opens the sheet: the method is the first plan entry's for `key`, else
    /// the default; the account is `github_accounts`' mapping for its owner.
    pub fn open<'a>(
        key: PrKey,
        kind: ActionKind,
        plans: impl IntoIterator<Item = &'a MergePlan>,
        accounts: &BTreeMap<String, String>,
    ) -> Self {
        let method = plans
            .into_iter()
            .find_map(|plan| plan.entry(&key))
            .and_then(|entry| entry.method);
        Self {
            account: prs::account_for(key.owner(), accounts),
            key,
            kind,
            method: method.unwrap_or_default(),
            method_from_plan: method.is_some(),
            running: false,
            error: None,
        }
    }

    pub fn action(&self) -> PrAction {
        match self.kind {
            ActionKind::Approve => PrAction::Approve,
            ActionKind::Merge => PrAction::Merge(self.method),
            ActionKind::Ready => PrAction::Ready,
        }
    }

    /// A mapped account must be a GitHub login; gh's active account is fine.
    pub fn account_ok(&self) -> bool {
        self.account.as_deref().is_none_or(prs::valid_login)
    }

    /// The verb button runs only while idle with a usable account.
    pub fn run_enabled(&self) -> bool {
        !self.running && self.account_ok()
    }

    /// Close and the method buttons are off while gh runs.
    pub fn can_close(&self) -> bool {
        !self.running
    }

    pub fn close_label(&self) -> &'static str {
        if self.error.is_some() {
            "Close"
        } else {
            "Cancel"
        }
    }

    pub fn method_hint(&self) -> &'static str {
        if self.method_from_plan {
            "(the plan's)"
        } else {
            "(default; the plan names none)"
        }
    }

    /// The command box: the command exactly as it runs.
    pub fn command_line(&self) -> String {
        prs::action_command_line(self.action(), &self.key, self.account.as_deref())
    }

    /// Who the command runs as. `active_login` is gh's active login when a
    /// poll has read it.
    pub fn account_line(&self, active_login: Option<&str>) -> String {
        let owner = self.key.owner();
        match &self.account {
            Some(user) if prs::valid_login(user) => {
                format!("Runs as {user} (github_accounts maps {owner} to it)")
            }
            Some(user) => format!(
                "github_accounts maps {owner} to {user:?}, which is not a GitHub login; fix github-accounts.json"
            ),
            None => match active_login {
                Some(login) => format!(
                    "Runs as gh's active account ({login}): github_accounts has no entry for {owner}"
                ),
                None => format!(
                    "Runs as gh's active account: github_accounts has no entry for {owner}"
                ),
            },
        }
    }

    /// A method button: only while idle; clears the last error.
    pub fn choose_method(&mut self, method: MergeMethod) {
        if !self.running {
            self.method = method;
            self.error = None;
        }
    }

    /// The verb button. Merge re-checks `latest` (the PR's latest read): the
    /// checks may have turned pending or failing since the sheet opened.
    /// Returns the run to execute, or `None` when running already or gated
    /// (the reason is then the error).
    pub fn run(&mut self, latest: Option<&WatchedPr>) -> Option<RunPrAction> {
        if self.running {
            return None;
        }
        let reason = if self.kind != ActionKind::Merge {
            None
        } else {
            latest
                .map(WatchedPr::checks_gate)
                .unwrap_or(ChecksGate::Running)
                .reason()
        };
        if let Some(reason) = reason {
            self.error = Some(format!("Not merged: {reason}"));
            return None;
        }
        self.running = true;
        self.error = None;
        Some(RunPrAction {
            action: self.action(),
            key: self.key.clone(),
            account: self.account.clone(),
        })
    }
}

/// A run for `key` finished. The open confirmation for it closes on
/// success and keeps the error on failure; returns whether it closed.
pub fn finish_action(
    open: &mut Option<PrActionConfirm>,
    key: &PrKey,
    result: Result<String, String>,
) -> bool {
    if !open.as_ref().is_some_and(|c| c.key == *key) {
        return false;
    }
    match result {
        Ok(_) => {
            *open = None;
            true
        }
        Err(error) => {
            if let Some(confirm) = open.as_mut() {
                confirm.running = false;
                confirm.error = Some(error);
            }
            false
        }
    }
}

/// Cancel / Close: refused while gh runs. Returns whether it closed.
pub fn close_action(open: &mut Option<PrActionConfirm>) -> bool {
    if open.as_ref().is_some_and(|c| c.running) {
        return false;
    }
    *open = None;
    true
}

/// A row's Approve / Merge gating from the PR's latest read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowGate {
    pub open: bool,
    pub draft: bool,
    /// Why the checks keep Approve and Merge off; a PR not read is pending.
    pub reason: Option<&'static str>,
}

impl RowGate {
    pub fn of(watched: Option<&WatchedPr>) -> Self {
        let state = watched.and_then(WatchedPr::brief).map(|brief| brief.state);
        Self {
            open: state == Some(PrState::Open),
            draft: state == Some(PrState::Draft),
            reason: watched
                .map(WatchedPr::checks_gate)
                .unwrap_or(ChecksGate::Running)
                .reason(),
        }
    }

    /// Approve and Merge open their confirmation only for an open PR whose
    /// checks allow it.
    pub fn can_act(&self) -> bool {
        self.open && self.reason.is_none()
    }

    /// The button label: a draft says so; an open PR names the checks.
    pub fn label(&self, name: &str) -> String {
        match self.reason {
            _ if self.draft => format!("{name} (draft)"),
            Some(reason) if self.open => format!("{name} ({reason})"),
            _ => name.to_string(),
        }
    }
}

/// The PR's author is the gh account Approve would run as: the mapped one,
/// else gh's `active_login`. Unknown either way: not own.
pub fn own_pr(
    key: &PrKey,
    watched: Option<&WatchedPr>,
    accounts: &BTreeMap<String, String>,
    active_login: Option<&str>,
) -> bool {
    let author = watched
        .and_then(|w| w.snapshot.core.as_ref())
        .map(|core| core.author.as_str());
    let account = prs::account_for(key.owner(), accounts);
    let login = match &account {
        Some(user) => Some(user.as_str()),
        None => active_login,
    };
    prs::is_own_pr(author, login)
}

/// Clears the user's hold on `key` in `root`'s plan, stamped `at`, unless a
/// write of that plan is pending. Returns `(previous, cleared)` for the
/// frontend to write, restoring `previous` if the write fails.
pub fn clear_hold(
    plans: &mut HashMap<String, MergePlan>,
    writing: &HashSet<String>,
    root: &str,
    key: &PrKey,
    at: &str,
) -> Option<(MergePlan, MergePlan)> {
    if writing.contains(root) {
        return None;
    }
    let plan = plans.get_mut(root)?;
    let previous = plan.clone();
    if !plan.clear_hold(key, at) {
        return None;
    }
    Some((previous, plan.clone()))
}

/// Dismisses news item `news` of PR `pr`; true when found (save the watch).
pub fn dismiss_news(watch: &mut WatchFile, pr: &str, news: &str) -> bool {
    let Some(item) = watch
        .prs
        .get_mut(pr)
        .and_then(|watched| watched.news.iter_mut().find(|item| item.key == news))
    else {
        return false;
    };
    item.dismissed = true;
    true
}

/// Review resets the row's new-comment count; true when there were some
/// (save the watch).
pub fn take_unseen_comments(watch: &mut WatchFile, key: &PrKey) -> bool {
    match watch.prs.get_mut(&key.id()) {
        Some(watched) if watched.unseen_comments > 0 => {
            watched.unseen_comments = 0;
            true
        }
        _ => false,
    }
}

/// The checkouts Review may open: `(repo, checkout)` of each present owned
/// branch named like the PR's head. Empty when the PR has no head yet.
pub fn review_candidates<'a>(
    watched: Option<&WatchedPr>,
    branches: impl IntoIterator<Item = &'a BranchStatus>,
) -> Vec<(PathBuf, PathBuf)> {
    let Some(head) = watched
        .and_then(|watched| watched.snapshot.core.as_ref())
        .map(|core| core.head_ref.as_str())
        .filter(|head| !head.is_empty())
    else {
        return Vec::new();
    };
    branches
        .into_iter()
        .filter(|row| !row.owned.gone && row.owned.branch.as_deref() == Some(head))
        .map(|row| (row.owned.repo.clone(), row.owned.checkout.clone()))
        .collect()
}

/// The first candidate whose repo's origin is `repo` (case-insensitive).
/// `slug_of` reads a repo dir's `owner/name`; its errors go to `on_error`
/// and skip that candidate.
pub fn review_checkout(
    candidates: Vec<(PathBuf, PathBuf)>,
    repo: &str,
    mut slug_of: impl FnMut(&Path) -> Result<String, String>,
    mut on_error: impl FnMut(String),
) -> Option<PathBuf> {
    candidates
        .into_iter()
        .find_map(|(repo_dir, checkout)| match slug_of(&repo_dir) {
            Ok(slug) if slug.eq_ignore_ascii_case(repo) => Some(checkout),
            Ok(_) => None,
            Err(error) => {
                on_error(error);
                None
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prs::{CheckItem, CheckOutcome, NewsItem, NewsKind, PullCore};

    const REPO: &str = "example-org/example-repo";

    fn key() -> PrKey {
        PrKey::new(REPO, 7)
    }

    fn watched(state: PrState, checks: &[CheckOutcome]) -> WatchedPr {
        let mut watched = WatchedPr::new(key());
        watched.snapshot.core = Some(PullCore {
            state: Some(state),
            head_ref: "feature/x".into(),
            ..PullCore::default()
        });
        watched.snapshot.check_runs = checks
            .iter()
            .map(|outcome| CheckItem {
                name: "test".into(),
                outcome: *outcome,
            })
            .collect();
        watched
    }

    fn confirm(kind: ActionKind, account: Option<&str>) -> PrActionConfirm {
        PrActionConfirm::open(
            key(),
            kind,
            [],
            &account
                .map(|user| BTreeMap::from([("example-org".to_string(), user.to_string())]))
                .unwrap_or_default(),
        )
    }

    #[test]
    fn the_sheet_texts_follow_the_account_method_and_error() {
        let mut sheet = confirm(ActionKind::Merge, None);
        assert_eq!(
            sheet.account_line(None),
            "Runs as gh's active account: github_accounts has no entry for example-org"
        );
        assert_eq!(
            sheet.account_line(Some("example-dev")),
            "Runs as gh's active account (example-dev): github_accounts has no entry for example-org"
        );
        assert_eq!(sheet.method_hint(), "(default; the plan names none)");
        assert_eq!(sheet.close_label(), "Cancel");
        assert_eq!(
            sheet.command_line(),
            "gh pr merge 7 --repo example-org/example-repo --merge"
        );
        sheet.method_from_plan = true;
        sheet.error = Some("x".into());
        assert_eq!(sheet.method_hint(), "(the plan's)");
        assert_eq!(sheet.close_label(), "Close");

        let mapped = confirm(ActionKind::Approve, Some("example-dev"));
        assert_eq!(
            mapped.account_line(Some("ignored")),
            "Runs as example-dev (github_accounts maps example-org to it)"
        );
        assert!(mapped.run_enabled());
        let bad = confirm(ActionKind::Approve, Some("not a login"));
        assert_eq!(
            bad.account_line(None),
            "github_accounts maps example-org to \"not a login\", which is not a GitHub login; fix github-accounts.json"
        );
        assert!(!bad.account_ok());
        assert!(!bad.run_enabled());
    }

    #[test]
    fn run_returns_the_gh_effect_once_and_close_waits_for_it() {
        let mut open = Some(confirm(ActionKind::Merge, Some("example-dev")));
        let latest = watched(PrState::Open, &[CheckOutcome::Pass]);
        let run = open.as_mut().unwrap().run(Some(&latest)).unwrap();
        assert_eq!(
            run.args(),
            ["pr", "merge", "7", "--repo", REPO, "--merge"].map(String::from)
        );
        assert_eq!(
            run.command_line(),
            "GH_TOKEN=\"$(gh auth token --user example-dev)\" gh pr merge 7 --repo example-org/example-repo --merge"
        );
        assert!(open.as_mut().unwrap().run(Some(&latest)).is_none());
        assert!(!open.as_ref().unwrap().run_enabled());
        assert!(!close_action(&mut open));
        assert!(!finish_action(
            &mut open,
            &PrKey::new(REPO, 8),
            Ok(String::new())
        ));
        assert!(!finish_action(&mut open, &key(), Err("failed".into())));
        assert!(close_action(&mut open));
        assert!(open.is_none());
    }

    #[test]
    fn row_gate_labels_and_enables_by_state_and_checks() {
        use CheckOutcome::{Fail, Pass, Pending};
        let cases = [
            // Unread: not open, so no reason on the label, and inert.
            (None, "Merge", false),
            (Some(watched(PrState::Open, &[Pass])), "Merge", true),
            (
                Some(watched(PrState::Open, &[Pending])),
                "Merge (checks running)",
                false,
            ),
            (
                Some(watched(PrState::Open, &[Fail])),
                "Merge (checks failed)",
                false,
            ),
            (
                Some(watched(PrState::Draft, &[Pass])),
                "Merge (draft)",
                false,
            ),
            (
                Some(watched(PrState::Draft, &[Fail])),
                "Merge (draft)",
                false,
            ),
            (Some(watched(PrState::Merged, &[Fail])), "Merge", false),
        ];
        for (watched, label, can_act) in cases {
            let gate = RowGate::of(watched.as_ref());
            assert_eq!(gate.label("Merge"), label);
            assert_eq!(gate.can_act(), can_act);
        }
        // Unread: not open, so the label stays bare.
        let unread = WatchedPr::new(key());
        assert_eq!(RowGate::of(Some(&unread)).label("Approve"), "Approve");
    }

    #[test]
    fn dismiss_and_unseen_reset_report_whether_to_save() {
        let mut watch = WatchFile::default();
        let mut pr = watched(PrState::Open, &[]);
        pr.unseen_comments = 2;
        pr.news.push(NewsItem {
            key: "n1".into(),
            kind: NewsKind::ChecksFailed,
            text: "checks failed".into(),
            at: "2026-11-01T00:00:00Z".into(),
            dismissed: false,
            told: false,
            approvals_only: false,
        });
        watch.prs.insert(key().id(), pr);
        assert!(!dismiss_news(&mut watch, &key().id(), "n2"));
        assert!(dismiss_news(&mut watch, &key().id(), "n1"));
        assert!(watch.prs[&key().id()].news[0].dismissed);
        assert!(take_unseen_comments(&mut watch, &key()));
        assert!(!take_unseen_comments(&mut watch, &key()));
        assert!(!take_unseen_comments(&mut watch, &PrKey::new(REPO, 9)));
    }

    #[test]
    fn review_checkout_matches_the_origin_slug_and_skips_errors() {
        let candidates = vec![
            (PathBuf::from("/r/broken"), PathBuf::from("/c/broken")),
            (PathBuf::from("/r/other"), PathBuf::from("/c/other")),
            (PathBuf::from("/r/match"), PathBuf::from("/c/match")),
            (PathBuf::from("/r/match2"), PathBuf::from("/c/match2")),
        ];
        let mut errors = Vec::new();
        let found = review_checkout(
            candidates,
            REPO,
            |dir| match dir.to_str().unwrap() {
                "/r/broken" => Err("no origin".into()),
                "/r/other" => Ok("example-org/other".into()),
                _ => Ok("Example-Org/Example-Repo".into()),
            },
            |error| errors.push(error),
        );
        assert_eq!(found, Some(PathBuf::from("/c/match")));
        assert_eq!(errors, ["no origin"]);
    }
}

/// Work's two local writes; the shared row reducers above remain Iced's rules.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocalControl {
    ClearHold { root: String, key: PrKey },
    DismissNews { pr: String, news: String },
}
impl LocalControl {
    pub fn label(&self) -> &'static str {
        match self {
            Self::ClearHold { .. } => "clear hold",
            Self::DismissNews { .. } => "×",
        }
    }

    #[cfg(feature = "runtime")]
    pub(crate) fn save(&self, root: &Path, at: &str) -> Result<(), String> {
        let path = match self {
            Self::ClearHold { root: chat, .. } => {
                crate::merge_plan::plan_path(&crate::merge_plan::plans_dir(root), chat)?
            }
            Self::DismissNews { .. } => WatchFile::path_in(root),
        };
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(format!("Read {}: {e}", path.display())),
        };
        let mut value: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|e| format!("Read {}: {e}", path.display()))?;
        if !value.is_object() {
            return Err(format!("Read {}: expected a JSON object", path.display()));
        }
        let changed = match self {
            Self::ClearHold { root, key } => {
                let mut plan: MergePlan = serde_json::from_value(value.clone())
                    .map_err(|e| format!("Read {}: {e}", path.display()))?;
                if plan.root_chat_id != *root {
                    return Err(format!("Read {}: merge plan root changed", path.display()));
                }
                let mut plans = [(root.clone(), plan.clone())].into();
                if let Some((_, next)) = clear_hold(&mut plans, &HashSet::new(), root, key, at) {
                    let index = plan
                        .entries
                        .iter()
                        .position(|e| e.key().id() == key.id() && e.hold.is_some())
                        .expect("cleared entry");
                    plan = next;
                    value
                        .get_mut("entries")
                        .and_then(serde_json::Value::as_array_mut)
                        .and_then(|entries| entries.get_mut(index))
                        .and_then(serde_json::Value::as_object_mut)
                        .ok_or_else(|| {
                            format!("Read {}: expected an object plan entry", path.display())
                        })?
                        .remove("hold");
                    value["updated_at"] = plan.updated_at.into();
                    true
                } else {
                    false
                }
            }
            Self::DismissNews { pr, news } => {
                let mut watch: WatchFile = serde_json::from_value(value.clone())
                    .map_err(|e| format!("Read {}: {e}", path.display()))?;
                if dismiss_news(&mut watch, pr, news) {
                    let index = watch.prs[pr]
                        .news
                        .iter()
                        .position(|n| n.key == *news)
                        .expect("dismissed item");
                    if value["prs"][pr]["news"][index]["dismissed"] == true {
                        return Ok(());
                    }
                    value
                        .get_mut("prs")
                        .and_then(|prs| prs.get_mut(pr))
                        .and_then(|pr| pr.get_mut("news"))
                        .and_then(serde_json::Value::as_array_mut)
                        .and_then(|news| news.get_mut(index))
                        .and_then(serde_json::Value::as_object_mut)
                        .ok_or_else(|| {
                            format!("Read {}: expected an object news item", path.display())
                        })?
                        .insert("dismissed".into(), true.into());
                    true
                } else {
                    false
                }
            }
        };
        if changed {
            let bytes = serde_json::to_vec_pretty(&value)
                .map_err(|e| format!("Encode {}: {e}", path.display()))?;
            crate::atomic_file::write_atomically(&path, &bytes)?;
        }
        Ok(())
    }
}

#[cfg(all(test, feature = "runtime"))]
mod remaining_controls_tests {
    use super::*;
    use crate::desktop_store::DesktopStore;
    use serde_json::json;

    #[test]
    fn remaining_clear_hold_saved_projection_equals_iced_and_keeps_unknowns() {
        let dir = tempfile::tempdir().unwrap();
        let plans = crate::merge_plan::plans_dir(dir.path());
        std::fs::create_dir_all(&plans).unwrap();
        let path = crate::merge_plan::plan_path(&plans, "root").unwrap();
        let value = json!({"future":17,"root_chat_id":"root","source":"agent","updated_at":"before","entries":[{"repo":"example/repo","number":1,"hold":"wait","note":"keep","future_entry":true},{"repo":"example/repo","number":2,"hold":"other"}]});
        std::fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
        let previous: MergePlan = serde_json::from_value(value).unwrap();
        let key = PrKey::new("example/repo", 1);
        let mut iced = [("root".into(), previous)].into();
        clear_hold(&mut iced, &HashSet::new(), "root", &key, "after").unwrap();
        let untouched = dir.path().join("workspaces.json");
        std::fs::write(&untouched, "{\n  \"workspaces\": []\n}").unwrap();
        let before = std::fs::read(&untouched).unwrap();
        let store = DesktopStore::open(dir.path()).unwrap();
        let action = LocalControl::ClearHold {
            root: "root".into(),
            key,
        };
        store
            .pr_control(action.clone(), "after".into())
            .wait()
            .unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let saved: MergePlan = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(saved, iced["root"]);
        let saved: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(saved["future"], 17);
        assert_eq!(saved["entries"][0]["future_entry"], true);
        assert_eq!(std::fs::read(&untouched).unwrap(), before);
        store.pr_control(action, "later".into()).wait().unwrap();
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }

    #[test]
    fn remaining_news_dismiss_saved_projection_equals_iced_and_keeps_unknowns() {
        let dir = tempfile::tempdir().unwrap();
        let path = WatchFile::path_in(dir.path());
        let value = json!({"future":"keep","prs":{"example/repo#1":{"key":{"repo":"example/repo","number":1},"future_pr":12,"news":[{"key":"news1","kind":"checks_failed","text":"test failed","at":"2026-11-01T00:00:00Z","told":true,"future_news":true},{"key":"news2","kind":"merged","text":"merged","at":"2026-11-01T00:00:00Z"}]}}});
        let mut iced: WatchFile = serde_json::from_value(value.clone()).unwrap();
        dismiss_news(&mut iced, "example/repo#1", "news1");
        std::fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
        let store = DesktopStore::open(dir.path()).unwrap();
        let action = LocalControl::DismissNews {
            pr: "example/repo#1".into(),
            news: "news1".into(),
        };
        store
            .pr_control(action.clone(), "unused".into())
            .wait()
            .unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(serde_json::from_slice::<WatchFile>(&bytes).unwrap(), iced);
        let saved: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(saved["future"], "keep");
        assert_eq!(saved["prs"]["example/repo#1"]["future_pr"], 12);
        assert_eq!(
            saved["prs"]["example/repo#1"]["news"][0]["future_news"],
            true
        );
        store.pr_control(action, "unused".into()).wait().unwrap();
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }

    #[test]
    fn remaining_malformed_pr_files_and_unsafe_plan_ids_are_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = WatchFile::path_in(dir.path());
        std::fs::write(&path, "broken").unwrap();
        let store = DesktopStore::open(dir.path()).unwrap();
        assert!(store
            .pr_control(
                LocalControl::DismissNews {
                    pr: "example/repo#1".into(),
                    news: "n".into()
                },
                "now".into()
            )
            .wait()
            .is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "broken");
        std::fs::write(&path, "[]").unwrap();
        assert!(store
            .pr_control(
                LocalControl::DismissNews {
                    pr: "example/repo#1".into(),
                    news: "n".into()
                },
                "now".into()
            )
            .wait()
            .is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "[]");
        assert!(store
            .pr_control(
                LocalControl::ClearHold {
                    root: "../escape".into(),
                    key: PrKey::new("example/repo", 1)
                },
                "now".into()
            )
            .wait()
            .is_err());
    }
}
