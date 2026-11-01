//! The repos a workspace lists (TRU-180), as pure rules: which repos its
//! chats bring in, and how manual adds and removes apply.
//!
//! A repo is a main checkout folder. Task worktrees are never repos: a
//! chat's touches are already folded to their main checkout by
//! `chat_facts`, and anything inside a task worktree is dropped here.

use crate::chats::ChatIndexEntry;
use std::path::{Path, PathBuf};

/// The main checkouts the chats `work` edited in, then those `workers`
/// (task repositories of workers they started) ran in, in first-touch
/// order: chats oldest first, each chat's touches in the order it made
/// them. Reads (touches without edits) never count. A dir inside one of
/// `task_worktrees` is not a repo and is left out.
pub fn auto_repo_candidates(
    work: &[&ChatIndexEntry],
    workers: &[PathBuf],
    task_worktrees: &[PathBuf],
) -> Vec<PathBuf> {
    let mut chats: Vec<&ChatIndexEntry> = work.to_vec();
    chats.sort_by_key(|entry| entry.mtime);
    let mut found: Vec<PathBuf> = Vec::new();
    let mut push = |dir: &Path| {
        let in_task = task_worktrees.iter().any(|tree| dir.starts_with(tree));
        if !in_task && !found.iter().any(|known| known == dir) {
            found.push(dir.to_path_buf());
        }
    };
    for facts in chats.iter().filter_map(|entry| entry.facts.as_ref()) {
        for repo in facts.repos_touched.iter().filter(|repo| repo.edits > 0) {
            push(&repo.dir);
        }
        for checkout in facts.checkouts.iter().filter(|c| c.edits > 0) {
            push(&checkout.repo);
        }
    }
    for dir in workers {
        push(dir);
    }
    found
}

/// A workspace's repo list after applying what joined: `home` first,
/// then `current` in its order, then manual adds and auto `candidates`
/// not yet listed. A dir in `removed` is never listed (home excepted).
/// Returns the list and the candidates that joined by this call.
pub fn derive_repo_list(
    home: &Path,
    current: &[PathBuf],
    candidates: &[PathBuf],
    added: &[PathBuf],
    removed: &[PathBuf],
) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut list = vec![home.to_path_buf()];
    let listable = |dir: &Path, list: &[PathBuf]| {
        dir != home
            && !removed.iter().any(|gone| gone == dir)
            && !list.iter().any(|known| known == dir)
    };
    for dir in current.iter().chain(added) {
        if listable(dir, &list) {
            list.push(dir.clone());
        }
    }
    let mut joined = Vec::new();
    for dir in candidates {
        if listable(dir, &list) {
            list.push(dir.clone());
            joined.push(dir.clone());
        }
    }
    (list, joined)
}

/// Records a manual add of `dir`: it is listed and no longer removed.
pub fn add_manually(dir: &Path, added: &mut Vec<PathBuf>, removed: &mut Vec<PathBuf>) {
    removed.retain(|gone| gone != dir);
    if !added.iter().any(|known| known == dir) {
        added.push(dir.to_path_buf());
    }
}

/// Records a manual remove of `dir`: it stays out even when its chats
/// touch it again.
pub fn remove_manually(dir: &Path, added: &mut Vec<PathBuf>, removed: &mut Vec<PathBuf>) {
    added.retain(|known| known != dir);
    if !removed.iter().any(|gone| gone == dir) {
        removed.push(dir.to_path_buf());
    }
}

/// The repo a path is in among `repos`: the deepest one containing it.
pub fn containing_repo<'a>(path: &Path, repos: &'a [PathBuf]) -> Option<&'a PathBuf> {
    repos
        .iter()
        .filter(|repo| path.starts_with(repo))
        .max_by_key(|repo| repo.components().count())
}

/// A repo's short name: its folder name, else the whole path.
pub fn repo_name(dir: &Path) -> String {
    dir.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| dir.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat_facts::{ChatFacts, CheckoutTouch, RepoTouch};
    use crate::chats::ChatBackend;
    use std::time::{Duration, SystemTime};

    fn entry(id: &str, age_secs: u64, facts: ChatFacts) -> ChatIndexEntry {
        ChatIndexEntry {
            id: id.to_string(),
            backend: ChatBackend::Claude,
            path: PathBuf::from(format!("/tmp/{id}.jsonl")),
            cwd: PathBuf::from("/w/home"),
            repo_root: None,
            is_worktree: false,
            branch: None,
            title: id.to_string(),
            latest: None,
            mtime: SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 - age_secs),
            size: 0,
            dead_cwd: false,
            facts: Some(facts),
            root_id: None,
        }
    }

    fn touch(dir: &str, edits: u64) -> RepoTouch {
        RepoTouch {
            dir: PathBuf::from(dir),
            edits,
        }
    }

    fn checkout(repo: &str, dir: &str, edits: u64) -> CheckoutTouch {
        CheckoutTouch {
            repo: PathBuf::from(repo),
            dir: PathBuf::from(dir),
            branch: Some("feature".into()),
            edits,
            last: "2026-10-30T00:00:00Z".into(),
        }
    }

    fn paths(dirs: &[&str]) -> Vec<PathBuf> {
        dirs.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn candidates_are_edited_main_checkouts_in_first_touch_order() {
        let older = entry(
            "older",
            500,
            ChatFacts {
                repos_touched: vec![touch("/w/api", 3), touch("/w/read-only", 0)],
                // An edit in a linked worktree counts for its main checkout.
                checkouts: vec![checkout("/w/infra", "/w/infra-wt-feature", 2)],
                ..ChatFacts::default()
            },
        );
        let newer = entry(
            "newer",
            10,
            ChatFacts {
                repos_touched: vec![touch("/w/web", 1), touch("/w/api", 9)],
                ..ChatFacts::default()
            },
        );
        // Given newest first, as the index lists chats.
        let candidates = auto_repo_candidates(
            &[&newer, &older],
            &paths(&["/w/worker-repo", "/w/api"]),
            &[],
        );
        assert_eq!(
            candidates,
            paths(&["/w/api", "/w/infra", "/w/web", "/w/worker-repo"])
        );
    }

    #[test]
    fn task_worktrees_are_never_repos() {
        let chat = entry(
            "chat",
            1,
            ChatFacts {
                repos_touched: vec![touch("/tasks/api-ops-1", 4), touch("/w/api", 1)],
                checkouts: vec![checkout("/tasks/api-ops-1/sub", "/tasks/api-ops-1/sub", 1)],
                ..ChatFacts::default()
            },
        );
        let candidates = auto_repo_candidates(&[&chat], &[], &paths(&["/tasks/api-ops-1"]));
        assert_eq!(candidates, paths(&["/w/api"]));
    }

    #[test]
    fn the_list_keeps_home_first_and_join_order_and_honours_manual_edits() {
        let home = Path::new("/w/home");
        let (list, joined) = derive_repo_list(
            home,
            &paths(&["/w/home", "/w/api"]),
            &paths(&["/w/home", "/w/api", "/w/web", "/w/infra"]),
            &[],
            &paths(&["/w/infra"]),
        );
        assert_eq!(list, paths(&["/w/home", "/w/api", "/w/web"]));
        assert_eq!(joined, paths(&["/w/web"]));

        // Nothing new: nothing joins, order stays.
        let (again, joined) = derive_repo_list(home, &list, &paths(&["/w/web"]), &[], &[]);
        assert_eq!(again, list);
        assert!(joined.is_empty());

        // Remove web by hand, add infra by hand.
        let mut added = Vec::new();
        let mut removed = paths(&["/w/infra"]);
        remove_manually(Path::new("/w/web"), &mut added, &mut removed);
        add_manually(Path::new("/w/infra"), &mut added, &mut removed);
        assert_eq!(added, paths(&["/w/infra"]));
        assert_eq!(removed, paths(&["/w/web"]));
        let current: Vec<PathBuf> = list.into_iter().filter(|d| d != "/w/web").collect();
        let (list, joined) = derive_repo_list(
            home,
            &current,
            &paths(&["/w/web", "/w/api"]),
            &added,
            &removed,
        );
        assert_eq!(list, paths(&["/w/home", "/w/api", "/w/infra"]));
        assert!(joined.is_empty());
    }

    #[test]
    fn a_path_belongs_to_the_deepest_listed_repo() {
        let repos = paths(&["/w", "/w/api", "/w/api/vendor/lib"]);
        assert_eq!(
            containing_repo(Path::new("/w/api/src"), &repos),
            Some(&PathBuf::from("/w/api"))
        );
        assert_eq!(
            containing_repo(Path::new("/w/api/vendor/lib/x"), &repos),
            Some(&PathBuf::from("/w/api/vendor/lib"))
        );
        assert_eq!(containing_repo(Path::new("/elsewhere"), &repos), None);
    }
}
