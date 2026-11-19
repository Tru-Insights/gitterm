//! Host facade for the core document scan. Git discovery stays in the host.

pub use gitterm_core::work::documents::{
    heading_in, is_well_known_name, CopySource, Document, DocumentCopy, ScanRequest, ScanResult,
    MAX_DEPTH, MAX_ENTRIES, MAX_FILES,
};

use gitterm_core::work::{
    documents::{self, GitDiscovery},
    summary::WorktreeInfo,
};
use std::path::{Path, PathBuf};

struct HostGit;

impl GitDiscovery for HostGit {
    fn list_worktrees(&mut self, repo: &Path) -> Result<Vec<WorktreeInfo>, String> {
        crate::worktree_follow::list_worktrees(repo)
    }

    fn git_common_dir(&mut self, root: &Path) -> Result<PathBuf, String> {
        crate::workspace_repos::git_common_dir(root)
    }
}

/// Gathers and groups the workspace's documents. Blocking.
pub fn scan(request: &ScanRequest) -> ScanResult {
    documents::scan(request, &mut HostGit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    fn git(dir: &Path, args: &[&str]) {
        let status = crate::agentd::git::git_command()
            .args(args)
            .current_dir(dir)
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed in {}", dir.display());
    }

    fn init_repo(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        git(dir, &["init", "-q", "-b", "main"]);
        git(
            dir,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "init",
            ],
        );
    }

    fn write(path: &Path, text: &str, age_secs: u64) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
        let at = SystemTime::now() - Duration::from_secs(age_secs);
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(at)
            .unwrap();
    }

    #[test]
    fn well_known_names_under_docs() {
        for name in [
            "DECISIONS.md",
            "decisions.md",
            "Plan.markdown",
            "PLANS.md",
            "design.MD",
            "STATUS.md",
            "ADR-0001-storage.md",
            "adr.md",
        ] {
            assert!(is_well_known_name(name), "{name}");
        }
        for name in [
            "README.md",
            "notes.md",
            "DECISIONS.txt",
            "DECISIONS",
            "my-plan.md",
            "HANDOVER.md",
        ] {
            assert!(!is_well_known_name(name), "{name}");
        }
    }

    #[test]
    fn headings_give_titles() {
        assert_eq!(
            heading_in("intro\n## Storage plan ##\n# Later"),
            Some("Storage plan".into())
        );
        assert_eq!(heading_in("#hashtag\n#\n# Real\n"), Some("Real".into()));
        assert_eq!(heading_in("no heading"), None);
    }

    #[test]
    fn copies_across_worktrees_are_one_document_with_the_newest_and_the_differ_flag() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let api = root.join("org").join("api");
        init_repo(&api);
        // An agent-made worktree beside the repo, and a second one.
        let agent_made = root.join("org").join(".worktrees").join("api-feature");
        git(
            &api,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                agent_made.to_str().unwrap(),
            ],
        );
        let other = root.join("org").join("api-other");
        git(
            &api,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "other",
                other.to_str().unwrap(),
            ],
        );

        // DECISIONS.md: three copies, the worktree's newest, contents differ.
        write(
            &api.join("docs/topic/DECISIONS.md"),
            "# Decisions\nold\n",
            300,
        );
        write(
            &agent_made.join("docs/topic/DECISIONS.md"),
            "# Decisions\nnew one\n",
            10,
        );
        write(
            &other.join("docs/topic/DECISIONS.md"),
            "# Decisions\nold\n",
            200,
        );
        // PLAN.md: two identical copies.
        write(&api.join(".plans/rollout.md"), "# Rollout\n", 100);
        write(&other.join(".plans/rollout.md"), "# Rollout\n", 50);
        // Not listed: a docs file without a well-known name, unless a chat
        // wrote it.
        write(&api.join("docs/notes.md"), "# Notes\n", 20);
        write(&api.join("docs/guide.md"), "# Guide\n", 30);
        // The work's own docs folder, outside git.
        let home = root.join("work").join("example");
        write(&home.join("docs/status.md"), "Status, no heading\n", 5);
        // A chat wrote a file outside every repo, and one since deleted.
        write(&root.join("elsewhere/scratch.md"), "# Scratch\n", 400);

        let result = scan(&ScanRequest {
            docs_dir: home.join("docs"),
            written: vec![
                api.join("docs/guide.md"),
                root.join("elsewhere/scratch.md"),
                root.join("gone/missing.md"),
                api.join("src/main.rs"),
            ],
            repos: vec![home.clone(), api.clone()],
            task_worktrees: Vec::new(),
        });
        assert!(!result.truncated);
        let rels: Vec<&str> = result.documents.iter().map(|d| d.rel.as_str()).collect();
        assert_eq!(
            rels,
            vec![
                "docs/status.md",
                "docs/topic/DECISIONS.md",
                "docs/guide.md",
                ".plans/rollout.md",
                "scratch.md",
            ]
        );

        let status = &result.documents[0];
        assert_eq!(status.title, "status.md");
        assert_eq!(status.newest().source, CopySource::WorkDocs);
        assert_eq!(status.copies.len(), 1);
        assert!(!status.differ);

        let decisions = &result.documents[1];
        assert_eq!(decisions.title, "Decisions");
        // The "other" worktree's copy equals the main checkout's: hidden.
        assert_eq!(decisions.copies.len(), 2);
        assert!(decisions.differ);
        assert_eq!(
            decisions.newest().path,
            agent_made.join("docs/topic/DECISIONS.md")
        );
        assert_eq!(
            decisions.newest().source,
            CopySource::Repo {
                repo: "api".into(),
                worktree: Some("feature".into())
            }
        );
        assert_eq!(decisions.newest().source.label(), "api \u{00B7} feature");
        assert_eq!(
            decisions.copies[1].path,
            api.join("docs/topic/DECISIONS.md")
        );
        assert_eq!(decisions.copies[1].source.label(), "api");

        // The identical copy in another worktree is hidden: no copies line.
        let rollout = &result.documents[3];
        assert_eq!(rollout.copies.len(), 1);
        assert!(!rollout.differ);
        assert_eq!(rollout.newest().path, api.join(".plans/rollout.md"));

        let scratch = &result.documents[4];
        assert_eq!(
            scratch.newest().source,
            CopySource::Elsewhere {
                folder: root.join("elsewhere")
            }
        );

        // Found from a worktree alone (a chat wrote there; the repo is not
        // listed), the copy still joins its document's identity.
        let only_written = scan(&ScanRequest {
            docs_dir: home.join("docs"),
            written: vec![
                agent_made.join("docs/topic/DECISIONS.md"),
                api.join("docs/topic/DECISIONS.md"),
            ],
            repos: Vec::new(),
            task_worktrees: Vec::new(),
        });
        let decisions: Vec<&Document> = only_written
            .documents
            .iter()
            .filter(|d| d.rel == "docs/topic/DECISIONS.md")
            .collect();
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].copies.len(), 2);
        assert_eq!(
            decisions[0].newest().source,
            CopySource::Repo {
                repo: "api".into(),
                worktree: Some("api-feature".into())
            }
        );
    }

    #[test]
    fn agent_scratch_and_task_worktrees_and_identical_copies_do_not_count() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let api = root.join("api");
        init_repo(&api);
        let add = |path: &Path, branch: &str| {
            git(
                &api,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "-b",
                    branch,
                    path.to_str().unwrap(),
                ],
            );
        };
        let scratch = api.join(".claude/worktrees/agent-1");
        let task = root.join("tasks/api-ops-1");
        let same = root.join("api-same");
        let changed = root.join("api-changed");
        add(&scratch, "scratch");
        add(&task, "task");
        add(&same, "same");
        add(&changed, "changed");
        for (dir, text, age) in [
            (&api, "# Plan\n", 100),
            (&scratch, "# Plan\nscratch edit\n", 1),
            (&task, "# Plan\ntask edit\n", 2),
            (&same, "# Plan\n", 50),
            (&changed, "# Plan\nreal edit\n", 20),
        ] {
            write(&dir.join(".plans/plan.md"), text, age);
        }
        write(&api.join(".plans/solo.md"), "# Solo\n", 90);
        write(&same.join(".plans/solo.md"), "# Solo\n", 40);

        let result = scan(&ScanRequest {
            docs_dir: root.join("none/docs"),
            written: vec![scratch.join(".plans/plan.md")],
            repos: vec![api.clone()],
            task_worktrees: vec![task.clone()],
        });
        let plan = result
            .documents
            .iter()
            .find(|d| d.rel == ".plans/plan.md")
            .unwrap();
        // Main plus the one real difference; scratch, task and the
        // identical copy are gone.
        let paths: Vec<&Path> = plan.copies.iter().map(|c| c.path.as_path()).collect();
        assert_eq!(
            paths,
            vec![
                changed.join(".plans/plan.md").as_path(),
                api.join(".plans/plan.md").as_path()
            ]
        );
        assert!(plan.differ);

        // Every copy identical: one copy, no differ flag.
        let solo = result
            .documents
            .iter()
            .find(|d| d.rel == ".plans/solo.md")
            .unwrap();
        assert_eq!(solo.copies.len(), 1);
        assert!(!solo.differ);
    }
}
