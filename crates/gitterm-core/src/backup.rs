//! Byte-preserving, verified Unix desktop backup. The caller holds the state lease.
use ring::digest::{Context, SHA256};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

pub const MANIFEST: &str = "manifest.json";
pub const COMPLETE: &str = "complete";
const EXPECTED: &[&str] = &[
    "workspaces.json",
    "settings.json",
    "profiles.json",
    "remotes.json",
    "remote-agents.json",
    "tokens.json",
    "tasks.json",
    "chat-meta",
    "turn-snapshots",
    "merge-plans",
    "task-briefs",
    "delegations",
    "captures",
    "pr-watch",
    "pr-watch.json",
    "review-viewed",
    "review-viewed.json",
    "plan-limits.json",
    "browser-profile",
];
#[derive(Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub root: PathBuf,
    pub entries: Vec<Entry>,
    pub worktrees: Vec<PathBuf>,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Entry {
    pub path: PathBuf,
    #[serde(flatten)]
    pub kind: Kind,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Kind {
    File { size: u64, sha256: String },
    Directory,
    Symlink { target: PathBuf },
    Absent,
    Skipped { reason: String },
}
fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}
fn private_dir(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}
fn private_file(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path)
}
fn sync_dir(path: &Path) -> io::Result<()> {
    fs::File::open(path)?.sync_all()
}
fn write_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = private_file(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}
fn hash(file: &mut fs::File) -> io::Result<(u64, String)> {
    let mut context = Context::new(&SHA256);
    let mut size = 0;
    let mut buffer = [0; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        size += count as u64;
        context.update(&buffer[..count]);
    }
    Ok((
        size,
        context
            .finish()
            .as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
    ))
}
fn read_regular(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(invalid(format!("not a regular file: {}", path.display())));
    }
    Ok(file)
}
fn excluded(name: &str) -> bool {
    // Chrome's regenerable directories use capitalized names on disk.
    let folded = name.to_ascii_lowercase();
    matches!(
        folded.as_str(),
        ".task-host.lock"
            | "worktrees"
            | "cache"
            | "caches"
            | "models"
            | "tmp"
            | "staging"
            | "backups"
            | "browser-cache"
            | "code cache"
            | "gpucache"
            | "shadercache"
            | "grshadercache"
    ) || name.starts_with("instance-")
        || name.ends_with(".sock")
        || name.ends_with(".tmp")
        || name.ends_with(".staging")
        || name.starts_with(".tasks.seed")
        || name.ends_with("-backups")
}
fn directory_identity(meta: &fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (meta.dev(), meta.ino())
}
fn scan(
    root: &Path,
    relative: &Path,
    browser_released: bool,
    worktrees: &[(u64, u64)],
    entries: &mut Vec<Entry>,
) -> io::Result<()> {
    let path = root.join(relative);
    let meta = fs::symlink_metadata(&path)?;
    let name = relative.file_name().unwrap().to_string_lossy();
    let kind = if meta.is_symlink() {
        Kind::Symlink {
            target: fs::read_link(&path)?,
        }
    } else if excluded(&name) || (meta.is_dir() && worktrees.contains(&directory_identity(&meta))) {
        Kind::Skipped {
            reason: "lock, worktree, transient state, cache or prior backup".into(),
        }
    } else if relative == Path::new("browser-profile") && !browser_released {
        Kind::Skipped {
            reason: "Chrome profile ownership has not been released".into(),
        }
    } else if meta.is_dir() {
        let mut children = fs::read_dir(&path)?
            .map(|e| e.map(|e| e.file_name()))
            .collect::<io::Result<Vec<_>>>()?;
        children.sort();
        for child in children {
            scan(
                root,
                &relative.join(child),
                browser_released,
                worktrees,
                entries,
            )?;
        }
        Kind::Directory
    } else if meta.is_file() {
        let (size, sha256) = hash(&mut read_regular(&path)?)?;
        Kind::File { size, sha256 }
    } else {
        Kind::Skipped {
            reason: "socket or other special file".into(),
        }
    };
    entries.push(Entry {
        path: relative.to_owned(),
        kind,
    });
    Ok(())
}
#[cfg(unix)]
fn available(path: &Path) -> io::Result<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path =
        std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|e| invalid(e.to_string()))?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // statvfs initializes the structure on success; path is NUL terminated.
    if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let stats = unsafe { stats.assume_init() };
    #[allow(clippy::unnecessary_cast)] // statvfs widths differ across Unix targets.
    let bytes = (stats.f_bavail as u64).saturating_mul(stats.f_frsize as u64);
    Ok(bytes)
}

/// `parent` must already exist, outside the source root. Nothing in the source
/// is created, normalized or rewritten. Failure leaves an incomplete backup.
pub fn create(root: &Path, parent: &Path, browser_released: bool) -> io::Result<PathBuf> {
    create_with(root, parent, browser_released, |_| Ok(()))
}
fn create_with(
    root: &Path,
    parent: &Path,
    browser_released: bool,
    before_verify: impl FnOnce(&Path) -> io::Result<()>,
) -> io::Result<PathBuf> {
    let root = fs::canonicalize(root)?;
    let parent = fs::canonicalize(parent)?;
    // The shared guard also rejects case aliases and overlapping directory
    // identities on macOS, where canonicalize can retain alternate casing.
    crate::state_lock::separate_agent_root(&parent, &root)?;
    let mut manifest = Manifest {
        root: root.clone(),
        entries: vec![],
        worktrees: vec![],
    };
    // Registered paths are metadata only, including custom worktree locations
    // inside the state root. Exclude them before walking any source contents.
    let tasks = root.join("tasks.json");
    if fs::symlink_metadata(&tasks).is_ok_and(|meta| meta.is_file() && !meta.is_symlink()) {
        let document: crate::tasks::TaskStoreDocument =
            serde_json::from_reader(read_regular(&tasks)?)?;
        manifest.worktrees.extend(
            document
                .tasks
                .iter()
                .filter_map(|task| task.worktree.path.clone()),
        );
    }
    let mut worktree_identities = Vec::new();
    for path in &manifest.worktrees {
        match fs::metadata(path) {
            Ok(meta) if meta.is_dir() => worktree_identities.push(directory_identity(&meta)),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(invalid(format!(
                    "inspect registered worktree {}: {error}",
                    path.display()
                )))
            }
        }
    }
    if worktree_identities.contains(&directory_identity(&fs::metadata(&root)?)) {
        return Err(invalid(format!(
            "registered worktree is the V6 state root {}; refusing an incomplete backup",
            root.display()
        )));
    }
    for entry in fs::read_dir(&root)? {
        scan(
            &root,
            Path::new(&entry?.file_name()),
            browser_released,
            &worktree_identities,
            &mut manifest.entries,
        )?;
    }
    for expected in EXPECTED {
        if !manifest
            .entries
            .iter()
            .any(|entry| entry.path == Path::new(expected))
        {
            manifest.entries.push(Entry {
                path: expected.into(),
                kind: Kind::Absent,
            });
        }
    }
    let bytes = serde_json::to_vec_pretty(&manifest)?;
    let needed =
        manifest
            .entries
            .iter()
            .try_fold(bytes.len() as u64 + 1024 * 1024, |sum, entry| {
                sum.checked_add(match entry.kind {
                    Kind::File { size, .. } => size,
                    _ => 0,
                })
                .ok_or_else(|| invalid("backup size overflow"))
            })?;
    if available(&parent)? < needed {
        return Err(io::Error::other(format!(
            "insufficient free space for {needed} byte verified backup"
        )));
    }
    let destination = parent.join(format!(
        "{}-{}",
        chrono::Utc::now().format("%Y%m%dT%H%M%S%.9fZ"),
        uuid::Uuid::new_v4().simple()
    ));
    private_dir(&destination)?;
    let data = destination.join("state");
    private_dir(&data)?;
    // Ancestors first, independently of the filesystem's enumeration order.
    manifest.entries.sort_by(|a, b| a.path.cmp(&b.path));
    for entry in &manifest.entries {
        let target = data.join(&entry.path);
        match entry.kind {
            Kind::Directory => private_dir(&target)?,
            Kind::File { .. } => {
                let mut source = read_regular(&root.join(&entry.path))?;
                let mut target = private_file(&target)?;
                io::copy(&mut source, &mut target)?;
                target.sync_all()?;
            }
            _ => {}
        }
    }
    write_new(
        &destination.join(MANIFEST),
        &serde_json::to_vec_pretty(&manifest)?,
    )?;
    before_verify(&destination)?;
    verify_data(&destination, &root)?;
    for entry in manifest.entries.iter().rev() {
        if matches!(entry.kind, Kind::Directory) {
            sync_dir(&data.join(&entry.path))?;
        }
    }
    sync_dir(&data)?;
    sync_dir(&destination)?;
    write_new(&destination.join(COMPLETE), b"verified\n")?;
    sync_dir(&destination)?;
    sync_dir(&parent)?;
    Ok(destination)
}
fn verify_data(destination: &Path, root: &Path) -> io::Result<Manifest> {
    let manifest: Manifest = serde_json::from_reader(read_regular(&destination.join(MANIFEST))?)?;
    if manifest.root != root {
        return Err(invalid("backup root differs from selected root"));
    }
    for entry in &manifest.entries {
        if entry.path.is_absolute()
            || entry
                .path
                .components()
                .any(|p| !matches!(p, std::path::Component::Normal(_)))
        {
            return Err(invalid("invalid backup manifest path"));
        }
        if let Kind::File { size, ref sha256 } = entry.kind {
            let actual = hash(&mut read_regular(
                &destination.join("state").join(&entry.path),
            )?)?;
            if actual != (size, sha256.clone()) {
                return Err(invalid(format!(
                    "backup hash mismatch: {}",
                    entry.path.display()
                )));
            }
        }
    }
    Ok(manifest)
}
/// A recorded path is reusable only after its durable completion and bytes verify.
pub fn verify(destination: &Path, root: &Path) -> io::Result<Manifest> {
    let root = fs::canonicalize(root)?;
    let destination = fs::canonicalize(destination)?;
    crate::state_lock::separate_agent_root(&destination, &root)?;
    let mut marker = Vec::new();
    read_regular(&destination.join(COMPLETE))?.read_to_end(&mut marker)?;
    if marker != b"verified\n" {
        return Err(invalid("backup completion marker invalid"));
    }
    verify_data(&destination, &root)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[test]
    fn original_bytes_verified_absence_failure_and_no_source_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("v6");
        let parent = temp.path().join("backups");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&parent).unwrap();
        let bytes = b"{  \"unknown\": [1, 2] }\n";
        fs::write(root.join("workspaces.json"), bytes).unwrap();
        fs::create_dir(root.join("chat-meta")).unwrap();
        fs::write(root.join("chat-meta/raw"), b"chat").unwrap();
        fs::create_dir(root.join("worktrees")).unwrap();
        fs::write(root.join("worktrees/ignored"), b"no").unwrap();
        let backup = create(&root, &parent, false).unwrap();
        let manifest = verify(&backup, &root).unwrap();
        assert_eq!(
            fs::read(backup.join("state/workspaces.json")).unwrap(),
            bytes
        );
        assert!(manifest
            .entries
            .iter()
            .any(|e| e.path == Path::new("settings.json") && matches!(e.kind, Kind::Absent)));
        assert!(!backup.join("state/worktrees").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(backup.join("state/workspaces.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(&backup).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        let error = create_with(&root, &parent, true, |path| {
            fs::write(path.join("state/workspaces.json"), b"corrupt")
        })
        .unwrap_err();
        assert!(error.to_string().contains("hash mismatch"));
        for entry in fs::read_dir(&parent).unwrap() {
            let path = entry.unwrap().path();
            if fs::canonicalize(&path).unwrap() != backup {
                assert!(!path.join(COMPLETE).exists());
            }
        }
        assert_eq!(fs::read(root.join("workspaces.json")).unwrap(), bytes);
        fs::write(backup.join("state/workspaces.json"), b"corrupt").unwrap();
        assert!(verify(&backup, &root).is_err());
    }
    #[test]
    fn custom_registered_worktrees_are_metadata_only_and_browser_requires_release() {
        use crate::tasks::*;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap().join("v6");
        let parent = temp.path().join("backups");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&parent).unwrap();
        let worktree = root.join("custom-checkout");
        fs::create_dir(&worktree).unwrap();
        fs::write(worktree.join("source"), b"do not copy").unwrap();
        let mut task = TaskRecord::new_draft(
            NewTaskRecord {
                task_id: "backup-fixture".into(),
                title: "Backup fixture".into(),
                objective: "Keep metadata only".into(),
                workspace: WorkspaceIdentity {
                    name: "fixture".into(),
                    location: WorkspaceLocationIdentity::Local {
                        directory: worktree.clone(),
                    },
                },
                repository: RepositoryIdentity {
                    common_dir: worktree.join(".git"),
                    remote_url: None,
                },
                issue: None,
                base: GitBase {
                    reference: "v6".into(),
                    commit: "fixture".into(),
                },
                branch: "task/fixture".into(),
                executor: ExecutorTarget::Local,
                harness: None,
                stopping_boundary: StoppingBoundary::ImplementUntilTestsPass,
            },
            "2026-11-27T00:00:00Z",
        );
        task.worktree = TaskWorktree {
            state: TaskWorktreeState::Ready,
            path: Some(worktree.clone()),
        };
        let mut ancestor = task.clone();
        ancestor.task_id = "ancestor-fixture".into();
        ancestor.worktree.path = Some(root.parent().unwrap().to_owned());
        let mut document = TaskStoreDocument {
            tasks: vec![task, ancestor],
            ..Default::default()
        };
        fs::write(
            root.join("tasks.json"),
            serde_json::to_vec(&document).unwrap(),
        )
        .unwrap();
        fs::write(root.join("settings.json"), b"{ \"future\": true }").unwrap();
        fs::create_dir(root.join("browser-profile")).unwrap();
        fs::write(root.join("browser-profile/Cookies"), b"private fixture").unwrap();
        fs::create_dir(root.join("browser-profile/Cache")).unwrap();
        fs::write(root.join("browser-profile/Cache/regenerable"), b"skip").unwrap();
        let skipped = create(&root, &parent, false).unwrap();
        let manifest = verify(&skipped, &root).unwrap();
        assert_eq!(
            manifest.worktrees,
            [worktree, root.parent().unwrap().to_owned()]
        );
        assert_eq!(
            fs::read(skipped.join("state/settings.json")).unwrap(),
            b"{ \"future\": true }"
        );
        assert!(!skipped.join("state/custom-checkout").exists());
        assert!(manifest
            .entries
            .iter()
            .any(|e| e.path == Path::new("browser-profile")
                && matches!(e.kind, Kind::Skipped { .. })));
        let included = create(&root, &parent, true).unwrap();
        assert!(!included.join("state/browser-profile/Cache").exists());
        assert_eq!(
            fs::read(included.join("state/browser-profile/Cookies")).unwrap(),
            b"private fixture"
        );
        use std::os::unix::fs::MetadataExt;
        assert_ne!(
            fs::metadata(root.join("browser-profile/Cookies"))
                .unwrap()
                .ino(),
            fs::metadata(included.join("state/browser-profile/Cookies"))
                .unwrap()
                .ino()
        );
        let case_alias = root.with_file_name("V6");
        if case_alias.exists() {
            assert!(create(&root, &case_alias, true).is_err());
        }
        // A self-referential registry entry must fail closed, never mark an
        // empty snapshot complete. An ancestor entry above did not omit state.
        document.tasks[1].worktree.path = Some(root.clone());
        fs::write(
            root.join("tasks.json"),
            serde_json::to_vec(&document).unwrap(),
        )
        .unwrap();
        assert!(create(&root, &parent, true)
            .unwrap_err()
            .to_string()
            .contains("state root"));
    }
    #[cfg(unix)]
    #[test]
    fn symlinks_are_manifest_only_even_when_external_or_dangling() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("v6");
        let parent = temp.path().join("backups");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&parent).unwrap();
        std::os::unix::fs::symlink(temp.path().join("secret"), root.join("tokens.json")).unwrap();
        let backup = create(&root, &parent, false).unwrap();
        let manifest = verify(&backup, &root).unwrap();
        assert!(manifest
            .entries
            .iter()
            .any(|e| e.path == Path::new("tokens.json") && matches!(e.kind, Kind::Symlink { .. })));
        assert!(!backup.join("state/tokens.json").exists());
        assert!(create(&root, &root, true).is_err());
    }
}
