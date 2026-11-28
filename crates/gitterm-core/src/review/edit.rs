//! Iced's chat edit-card route: owning checkout, target facts and diff source.
use super::model::{edit_review_source, EditReview, ReviewTarget};
use crate::{
    git_facts::{
        base_candidates, choose_nearest, git_text, measure_bases, run_git_lines, BaseChoice,
        GIT_TIMEOUT,
    },
    work::summary::StatusCounts,
};
use std::path::{Path, PathBuf};

/// A checkout no overview row names, read as a row reads it: its main
/// repo, branch and nearest base (TRU-179, a chat's edit card opening the
/// review pane).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutFacts {
    /// The main checkout (a linked worktree's repo), for its name.
    pub repo: PathBuf,
    /// `None` when detached.
    pub branch: Option<String>,
    pub base: Option<BaseChoice>,
}

/// Reads [`CheckoutFacts`] for `checkout` (its top level): the common git
/// dir, `status --branch` headers, then the base candidates measured as
/// Iced's `read_checkout` measures them.
pub async fn checkout_facts(checkout: &Path) -> Result<CheckoutFacts, String> {
    let common = git_text(
        checkout,
        &[
            "--no-optional-locks",
            "rev-parse",
            "--path-format=absolute",
            "--git-common-dir",
        ],
        GIT_TIMEOUT,
    )
    .await?;
    let common = PathBuf::from(common.trim());
    let repo = match common.file_name() {
        Some(name) if name == ".git" => common
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| checkout.to_path_buf()),
        // A bare repo's worktree: the repo is the git dir itself.
        _ => common,
    };
    let mut counts = StatusCounts::default();
    // Headers come first; the first entry line ends the read.
    run_git_lines(
        checkout,
        &[
            "--no-optional-locks",
            "status",
            "--porcelain=v2",
            "--branch",
        ],
        GIT_TIMEOUT,
        |line| counts.observe(line, 1),
    )
    .await?;
    let mut candidates = base_candidates(None, counts.head.as_deref(), counts.upstream.as_deref());
    let mut git_runs = 0;
    let measured = measure_bases(checkout, &mut candidates, &mut git_runs).await?;
    Ok(CheckoutFacts {
        repo,
        branch: counts.head,
        base: choose_nearest(&candidates, &measured).map(|(base, _)| base),
    })
}

/// The checkout `file` belongs to: its nearest ancestor holding a `.git`
/// entry (the directory of a main checkout, or the file a linked worktree
/// or submodule has). A worktree is its own checkout.
pub fn checkout_containing(file: &Path) -> Option<PathBuf> {
    file.ancestors()
        .skip(1)
        .find(|dir| dir.join(".git").exists())
        .map(Path::to_path_buf)
}

/// `file` relative to `checkout`, `/`-separated as git lists it.
pub fn checkout_relative(checkout: &Path, file: &Path) -> Result<String, String> {
    let rel = file
        .strip_prefix(checkout)
        .map_err(|_| format!("{} is not inside {}", file.display(), checkout.display()))?;
    let parts: Vec<String> = rel
        .components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect();
    if parts.is_empty() {
        return Err(format!("{} is the checkout itself", file.display()));
    }
    Ok(parts.join("/"))
}

/// Resolves an `open_review` for `file` in `checkout`. `known` is the
/// overview's row for the checkout when there is one (its owner and base);
/// otherwise the checkout is read as a row would be. Never call on the UI
/// thread.
pub async fn resolve_edit_review(
    checkout: PathBuf,
    file: PathBuf,
    known: Option<ReviewTarget>,
) -> Result<EditReview, String> {
    let focus = checkout_relative(&checkout, &file)?;
    let target = match known {
        Some(target) => target,
        None => {
            let facts = checkout_facts(&checkout).await?;
            ReviewTarget {
                repo: crate::work::summary::folder_name(&facts.repo),
                branch: facts.branch.unwrap_or_else(|| "(detached)".to_string()),
                owner: "opened from a chat".to_string(),
                base: facts.base,
                checkout,
            }
        }
    };
    let pathspec = format!(":(literal){focus}");
    let status = git_text(
        &target.checkout,
        &[
            "--no-optional-locks",
            "status",
            "--porcelain",
            "--untracked-files=all",
            "--",
            &pathspec,
        ],
        GIT_TIMEOUT,
    )
    .await?;
    let source = edit_review_source(!status.trim().is_empty(), target.base.is_some());
    Ok(EditReview {
        target,
        source,
        focus,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{git_facts::DiffSource, review::collect::load_review_source_with_snapshots};

    fn git(root: &Path, args: &[&str]) {
        let output = crate::git_status::git_command()
            .current_dir(root)
            .args([
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.test",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }

    #[tokio::test]
    async fn edit_cards_resolve_changed_and_committed_files_in_their_own_worktree() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().canonicalize().unwrap();
        git(&repo, &["init", "-qb", "main"]);
        std::fs::write(repo.join("a.txt"), "base\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "base"]);
        let checkout = repo.join("linked");
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-qb",
                "feature",
                checkout.to_str().unwrap(),
            ],
        );
        std::fs::create_dir(checkout.join("src")).unwrap();
        std::fs::write(checkout.join("src/done.rs"), "fn done() {}\n").unwrap();
        git(&checkout, &["add", "."]);
        git(&checkout, &["commit", "-qm", "done"]);
        std::fs::write(checkout.join("src/[wip].rs"), "fn wip() {}\n").unwrap();
        for (path, expected) in [
            ("src/[wip].rs", DiffSource::Uncommitted),
            ("src/done.rs", DiffSource::VsBase),
        ] {
            let file = checkout.join(path);
            let owner = checkout_containing(&file).unwrap();
            assert_eq!(owner, checkout);
            let open = resolve_edit_review(owner, file, None).await.unwrap();
            assert_eq!(open.focus, path);
            assert_eq!(open.source, expected);
            assert_eq!(open.target.checkout, checkout);
            assert_eq!(open.target.repo, crate::work::summary::folder_name(&repo));
            assert_eq!(open.target.base.as_ref().unwrap().reference, "main");
            let load = load_review_source_with_snapshots(open.target, open.source.into(), async {
                unreachable!()
            })
            .await
            .unwrap();
            assert!(load.diff.file(path).is_some());
        }
    }
}
