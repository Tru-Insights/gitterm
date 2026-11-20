//! `review.json` is a synthetic fixture format, not a runtime status wire contract.
use crate::model::{Group, Patches, ReviewModel};
use gitterm_core::status::{GitFileStatus, GitStatusSnapshot};
use serde::Deserialize;
use std::path::{Component, Path};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    files: Vec<Entry>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    group: Group,
    path: String,
    status: String,
    patch: String,
}

pub fn load(dir: &Path) -> Result<(GitStatusSnapshot, Patches), String> {
    let manifest_path = dir.join("review.json");
    let json = std::fs::read_to_string(&manifest_path)
        .map_err(|e| format!("read {}: {e}", manifest_path.display()))?;
    let manifest: Manifest = serde_json::from_str(&json)
        .map_err(|e| format!("parse {}: {e}", manifest_path.display()))?;
    let root = dir
        .canonicalize()
        .map_err(|e| format!("resolve {}: {e}", dir.display()))?;
    let mut snapshot = GitStatusSnapshot {
        tab_id: 0,
        repo_path: root.clone(),
        repo_name: "Synthetic review".into(),
        branch_name: "fixture".into(),
        is_git_repo: true,
        staged: Vec::new(),
        unstaged: Vec::new(),
        untracked: Vec::new(),
        error: None,
    };
    let mut patches = Patches::new();
    for entry in manifest.files {
        if entry.path.is_empty() || entry.status.is_empty() {
            return Err(format!(
                "{}: path and status must be nonempty",
                manifest_path.display()
            ));
        }
        let relative = Path::new(&entry.patch);
        if relative.as_os_str().is_empty()
            || relative
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err(format!("invalid fixture patch path: {}", entry.patch));
        }
        let path = root
            .join(relative)
            .canonicalize()
            .map_err(|e| format!("resolve fixture patch {}: {e}", entry.patch))?;
        if !path.starts_with(&root) {
            return Err(format!(
                "fixture patch escapes {}: {}",
                root.display(),
                path.display()
            ));
        }
        let patch = std::fs::read_to_string(&path)
            .map_err(|e| format!("read fixture patch {}: {e}", path.display()))?;
        if patches
            .insert((entry.group, entry.path.clone()), patch.into())
            .is_some()
        {
            return Err(format!(
                "duplicate {} patch for {}",
                entry.group.label(),
                entry.path
            ));
        }
        let statuses = match entry.group {
            Group::Staged => &mut snapshot.staged,
            Group::Unstaged => &mut snapshot.unstaged,
            Group::Untracked => &mut snapshot.untracked,
        };
        statuses.push(GitFileStatus {
            path: entry.path,
            status: entry.status,
            is_staged: entry.group == Group::Staged,
        });
    }
    Ok((snapshot, patches))
}

pub fn load_model(dir: &Path) -> Result<ReviewModel, String> {
    let (snapshot, patches) = load(dir)?;
    ReviewModel::new(&snapshot, &patches)
}

/// Disposable branch/range fixture shared by model and native view tests.
#[cfg(test)]
pub(crate) fn aggregate_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        let output = gitterm_core::git_status::git_command()
            .current_dir(dir.path())
            .args([
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    };
    git(&["init", "-qb", "main"]);
    std::fs::write(dir.path().join("hello.rs"), "fn base() {}\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "base"]);
    git(&["switch", "-qc", "feature"]);
    for text in ["fn first() {}\n", "fn second() {}\n"] {
        std::fs::write(dir.path().join("hello.rs"), text).unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "change"]);
    }
    git(&["branch", "release", "HEAD~1"]);
    std::fs::write(dir.path().join("hello.rs"), "fn pending() {}\n").unwrap();
    dir
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct FixtureDir(PathBuf);
    impl FixtureDir {
        fn new() -> Self {
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "gitterm-gpui-review-fixture-{}-{unique}",
                std::process::id()
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn manifest(&self, value: serde_json::Value) {
            std::fs::write(
                self.0.join("review.json"),
                serde_json::to_vec(&value).unwrap(),
            )
            .unwrap();
        }
    }
    impl Drop for FixtureDir {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }
    #[test]
    fn rejects_bad_manifest_duplicate_missing_and_escaping_patch_paths() {
        let dir = FixtureDir::new();
        std::fs::write(dir.0.join("one.patch"), "fixture").unwrap();
        let entry =
            serde_json::json!({"group":"staged","path":"a.rs","status":"M","patch":"one.patch"});
        dir.manifest(serde_json::json!({"files":[entry.clone(), entry.clone()]}));
        assert!(
            load(&dir.0)
                .unwrap_err()
                .contains("duplicate Staged patch for a.rs")
        );
        for patch in ["../one.patch", "/tmp/one.patch", "", "missing.patch"] {
            let mut invalid = entry.clone();
            invalid["patch"] = patch.into();
            dir.manifest(serde_json::json!({"files":[invalid]}));
            assert!(load(&dir.0).is_err(), "accepted patch path {patch:?}");
        }
        for field in ["path", "status"] {
            let mut invalid = entry.clone();
            invalid[field] = "".into();
            dir.manifest(serde_json::json!({"files":[invalid]}));
            assert!(load(&dir.0).unwrap_err().contains("must be nonempty"));
        }
        dir.manifest(serde_json::json!({"files":[], "unknown":true}));
        assert!(load(&dir.0).unwrap_err().contains("parse"));
        #[cfg(unix)]
        {
            let outside = FixtureDir::new();
            std::fs::write(outside.0.join("outside.patch"), "outside").unwrap();
            std::os::unix::fs::symlink(outside.0.join("outside.patch"), dir.0.join("link.patch"))
                .unwrap();
            let mut linked = entry;
            linked["patch"] = "link.patch".into();
            dir.manifest(serde_json::json!({"files":[linked]}));
            assert!(load(&dir.0).unwrap_err().contains("escapes"));
        }
    }
}
