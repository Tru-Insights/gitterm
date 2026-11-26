//! One kernel-owned lifetime lock per resolved state directory. The file is
//! permanent; its contents (including any old PID) never establish ownership.
use std::{
    fs::{self, File, OpenOptions},
    io,
    path::{Component, Path, PathBuf},
};

pub const LOCK_FILE: &str = ".task-host.lock";

#[derive(Debug)]
pub enum LockError {
    Busy { root: PathBuf },
    Io { root: PathBuf, source: io::Error },
}
impl std::fmt::Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy { root } => write!(
                f,
                "GitTerm state root is already in use: {}",
                root.display()
            ),
            Self::Io { root, source } => {
                write!(f, "lock GitTerm state root {}: {source}", root.display())
            }
        }
    }
}
impl std::error::Error for LockError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Busy { .. } => None,
        }
    }
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// Resolve existing components, including symlinks, without creating a missing
/// tail. Resolve `..` only after following the preceding component's symlink.
pub fn resolve_root(path: &Path) -> io::Result<PathBuf> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "state root must be absolute",
        ));
    }
    let mut resolved = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => continue,
            Component::ParentDir => {
                resolved.pop();
            }
            _ => resolved.push(part),
        }
        match fs::symlink_metadata(&resolved) {
            Ok(_) => resolved = fs::canonicalize(&resolved)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(resolved)
}

/// Agent storage must not share the desktop directory or any ancestor/child,
/// even through a symlink or a not-yet-created tail. This performs no writes.
pub fn separate_agent_root(agent: &Path, desktop: &Path) -> io::Result<PathBuf> {
    let agent = if agent.is_absolute() {
        agent.to_path_buf()
    } else {
        std::env::current_dir()?.join(agent)
    };
    let agent = resolve_root(&agent)?;
    let desktop = resolve_root(desktop)?;
    if agent.starts_with(&desktop)
        || desktop.starts_with(&agent)
        || directory_contains(&desktop, &agent)?
        || directory_contains(&agent, &desktop)?
        || component_prefix_ignore_case(&agent, &desktop)
        || component_prefix_ignore_case(&desktop, &agent)
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "agent storage {} overlaps desktop state root {}",
                agent.display(),
                desktop.display()
            ),
        ));
    }
    Ok(agent)
}

// Missing tails have no directory identity yet. Conservatively refuse case
// aliases even on case-sensitive volumes; never fold a lock path or identity.
fn component_prefix_ignore_case(path: &Path, prefix: &Path) -> bool {
    let mut parts = path.components();
    prefix.components().all(|part| {
        parts.next().is_some_and(|candidate| {
            candidate.as_os_str().to_string_lossy().to_lowercase()
                == part.as_os_str().to_string_lossy().to_lowercase()
        })
    })
}

// canonicalize/realpath can preserve alternate casing on macOS. Compare
// directory identities along both ancestry chains instead of folding names.
fn directory_contains(root: &Path, path: &Path) -> io::Result<bool> {
    let root_identity = match fs::metadata(root) {
        Ok(meta) if meta.is_dir() => directory_identity(root)?,
        Ok(_) => return Ok(false),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    for ancestor in path.ancestors() {
        match fs::metadata(ancestor) {
            Ok(meta) if meta.is_dir() => {
                if directory_identity(ancestor)? == root_identity {
                    return Ok(true);
                }
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}

#[derive(Debug)]
pub struct StateLock {
    file: File,
    root: PathBuf,
    identity: (u64, u64),
}
impl StateLock {
    pub fn acquire(root: &Path) -> Result<Self, LockError> {
        Self::acquire_with(root, || {}, || {})
    }

    fn acquire_with(
        root: &Path,
        after_open: impl FnOnce(),
        after_lock: impl FnOnce(),
    ) -> Result<Self, LockError> {
        let root = resolve_root(root).map_err(|source| LockError::Io {
            root: root.into(),
            source,
        })?;
        let attempt = || -> io::Result<Self> {
            fs::create_dir_all(&root)?;
            let identity = directory_identity(&root)?;
            let path = root.join(LOCK_FILE);
            // Refuse links and special files before opening them (a FIFO can
            // block). O_NOFOLLOW also closes the symlink-swap window on Unix.
            match fs::symlink_metadata(&path) {
                Ok(meta) => check_regular(&meta)?,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            let mut options = OpenOptions::new();
            options.read(true).write(true).create(true).truncate(false);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options
                    .mode(0o600)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::OpenOptionsExt;
                options.custom_flags(0x00200000); // FILE_FLAG_OPEN_REPARSE_POINT
            }
            let file = options.open(&path)?;
            after_open();
            check_identity(&file, &path)?;
            match file.try_lock() {
                Ok(()) => {}
                Err(fs::TryLockError::WouldBlock) => {
                    return Err(io::Error::from(io::ErrorKind::WouldBlock))
                }
                Err(fs::TryLockError::Error(error)) => return Err(error),
            }
            after_lock();
            // Check again after locking: a replacement must never leave two
            // owners holding locks on different inodes at the same path.
            let lock = Self {
                file,
                root: root.clone(),
                identity,
            };
            check_identity(&lock.file, &path)?;
            if directory_identity(&root)? != identity {
                return Err(invalid("state root was replaced"));
            }
            Ok(lock)
        };
        attempt().map_err(|source| {
            if source.kind() == io::ErrorKind::WouldBlock {
                LockError::Busy { root }
            } else {
                LockError::Io { root, source }
            }
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
    /// Filesystem identity, not a path-spelling hash (device and inode on Unix).
    pub fn identity(&self) -> (u64, u64) {
        self.identity
    }
}
impl Drop for StateLock {
    fn drop(&mut self) {
        // A forked child may still hold the inherited descriptor until exec.
        if let Err(error) = self.file.unlock() {
            eprintln!("Unlock state root {}: {error}", self.root.display());
        }
    }
}
fn check_regular(meta: &fs::Metadata) -> io::Result<()> {
    if !meta.is_file() || meta.file_type().is_symlink() {
        return Err(invalid("state lock is not a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.nlink() != 1 {
            return Err(invalid("state lock is hard-linked"));
        }
    }
    Ok(())
}
fn check_identity(file: &File, path: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    check_regular(&meta)?;
    check_regular(&file.metadata()?)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x00200000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    let actual = options.open(path)?;
    check_regular(&actual.metadata()?)?;
    let (opened, links) = file_identity(file)?;
    if links != 1 {
        return Err(invalid("state lock is hard-linked"));
    }
    if file_identity(&actual)?.0 != opened {
        return Err(invalid("state lock was replaced"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if (meta.dev(), meta.ino()) != opened {
            return Err(invalid("state lock was replaced"));
        }
    }
    Ok(())
}
fn directory_identity(path: &Path) -> io::Result<(u64, u64)> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x02000000); // FILE_FLAG_BACKUP_SEMANTICS (directory)
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_dir() {
        return Err(invalid("state root is not a directory"));
    }
    Ok(file_identity(&file)?.0)
}
#[cfg(unix)]
fn file_identity(file: &File) -> io::Result<((u64, u64), u64)> {
    use std::os::unix::fs::MetadataExt;
    let meta = file.metadata()?;
    Ok(((meta.dev(), meta.ino()), meta.nlink()))
}
#[cfg(windows)]
fn file_identity(file: &File) -> io::Result<((u64, u64), u64)> {
    use std::os::windows::io::AsRawHandle;
    #[repr(C)]
    #[derive(Default)]
    struct FileInfo {
        attributes: u32,
        created: [u32; 2],
        accessed: [u32; 2],
        written: [u32; 2],
        volume: u32,
        size_high: u32,
        size_low: u32,
        links: u32,
        index_high: u32,
        index_low: u32,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GetFileInformationByHandle(handle: *mut std::ffi::c_void, info: *mut FileInfo) -> i32;
    }
    let mut info = FileInfo::default();
    // SAFETY: the live File supplies a valid handle and info is writable.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((
        (
            info.volume as u64,
            (u64::from(info.index_high) << 32) | u64::from(info.index_low),
        ),
        info.links as u64,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, Write};
    use std::process::{Command, Stdio};

    const CHILD: &str = "GITTERM_STATE_LOCK_TEST";
    fn child(root: &Path, mode: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "state_lock::tests::lock_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD, mode)
            .env(crate::config_paths::CONFIG_DIR_ENV, root);
        command
    }
    #[test]
    fn lock_child() {
        let Ok(mode) = std::env::var(CHILD) else {
            return;
        };
        let root = PathBuf::from(std::env::var_os(crate::config_paths::CONFIG_DIR_ENV).unwrap());
        if mode == "busy" {
            match StateLock::acquire(&root) {
                Err(LockError::Busy { root: actual }) => {
                    assert_eq!(actual, fs::canonicalize(root).unwrap())
                }
                other => panic!("expected typed Busy, got {other:?}"),
            }
        } else {
            let _lock = StateLock::acquire(&root).unwrap();
            if mode == "hold" {
                println!("STATE-LOCK-HELD");
                io::stdout().flush().unwrap();
                let mut input = String::new();
                io::stdin().read_line(&mut input).unwrap();
            }
        }
    }

    fn snapshot(root: &Path) -> Vec<(PathBuf, Vec<u8>, u32)> {
        fn walk(root: &Path, path: &Path, out: &mut Vec<(PathBuf, Vec<u8>, u32)>) {
            let meta = fs::metadata(path).unwrap();
            #[cfg(unix)]
            let mode = {
                use std::os::unix::fs::PermissionsExt;
                meta.permissions().mode()
            };
            #[cfg(not(unix))]
            let mode = u32::from(meta.permissions().readonly());
            out.push((
                path.strip_prefix(root).unwrap().into(),
                if meta.is_file() {
                    fs::read(path).unwrap()
                } else {
                    vec![]
                },
                mode,
            ));
            if meta.is_dir() {
                for entry in fs::read_dir(path).unwrap() {
                    walk(root, &entry.unwrap().path(), out);
                }
            }
        }
        let mut out = vec![];
        walk(root, root, &mut out);
        out.sort();
        out
    }
    #[test]
    fn two_processes_contend_without_writes_and_distinct_roots_succeed() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("state");
        let _lock = StateLock::acquire(&root).unwrap();
        fs::write(root.join("settings.json"), b"unchanged").unwrap();
        fs::write(root.join(LOCK_FILE), b"untrusted old PID: 123").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(root.join(LOCK_FILE), fs::Permissions::from_mode(0o640)).unwrap();
        }
        let before = snapshot(&root);
        let output = child(&root, "busy").output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert_eq!(snapshot(&root), before);
        assert!(child(&temp.path().join("other"), "free")
            .status()
            .unwrap()
            .success());
    }
    #[test]
    fn crash_releases_kernel_lock_without_removing_file() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("state");
        let mut process = child(&root, "hold")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdout = io::BufReader::new(process.stdout.take().unwrap());
        let mut line = String::new();
        loop {
            line.clear();
            assert_ne!(
                stdout.read_line(&mut line).unwrap(),
                0,
                "child exited early"
            );
            if line.contains("STATE-LOCK-HELD") {
                break;
            }
        }
        assert!(matches!(
            StateLock::acquire(&root),
            Err(LockError::Busy { .. })
        ));
        let identity = directory_identity(&root).unwrap();
        process.kill().unwrap();
        process.wait().unwrap();
        let next = StateLock::acquire(&root).unwrap();
        assert_eq!(next.identity(), identity);
        assert!(root.join(LOCK_FILE).is_file());
    }
    #[test]
    fn inherited_descriptor_does_not_extend_host_ownership() {
        let temp = tempfile::tempdir().unwrap();
        let lock = StateLock::acquire(temp.path()).unwrap();
        let inherited = lock.file.try_clone().unwrap();
        drop(lock);
        let next = StateLock::acquire(temp.path()).unwrap();
        drop(inherited);
        assert!(StateLock::acquire(temp.path()).is_err());
        drop(next);
        StateLock::acquire(temp.path()).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn symlinked_home_and_root_share_identity() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        fs::create_dir(&home).unwrap();
        symlink(&home, temp.path().join("home-link")).unwrap();
        let root = home.join(".config/gitterm-v6");
        let lock = StateLock::acquire(&root).unwrap();
        symlink(&root, temp.path().join("root-link")).unwrap();
        for alias in [
            temp.path().join("home-link/.config/gitterm-v6"),
            temp.path().join("root-link"),
        ] {
            assert_eq!(resolve_root(&alias).unwrap(), lock.root());
            assert!(child(&alias, "busy").status().unwrap().success());
        }
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn tmp_and_private_tmp_are_one_root() {
        let temp = tempfile::tempdir_in("/private/tmp").unwrap();
        let alias = Path::new("/tmp").join(temp.path().file_name().unwrap());
        let lock = StateLock::acquire(&alias).unwrap();
        assert_eq!(lock.root(), fs::canonicalize(temp.path()).unwrap());
        assert!(child(temp.path(), "busy").status().unwrap().success());
    }
    #[cfg(unix)]
    #[test]
    fn symlink_hardlink_and_replaced_lock_are_rejected() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let outside = temp.path().join("outside");
        fs::write(&outside, b"never truncate").unwrap();
        let root = temp.path().join("state");
        fs::create_dir(&root).unwrap();
        let path = root.join(LOCK_FILE);
        symlink(&outside, &path).unwrap();
        assert!(matches!(
            StateLock::acquire(&root),
            Err(LockError::Io { .. })
        ));
        fs::remove_file(&path).unwrap();
        fs::hard_link(&outside, &path).unwrap();
        assert!(StateLock::acquire(&root)
            .unwrap_err()
            .to_string()
            .contains("hard-linked"));
        fs::remove_file(&path).unwrap();
        fs::write(&path, b"original").unwrap();
        let replace = || {
            fs::rename(&path, root.join("old-lock")).unwrap();
            fs::write(&path, b"replacement").unwrap();
        };
        assert!(StateLock::acquire_with(&root, replace, || {})
            .unwrap_err()
            .to_string()
            .contains("replaced"));
        fs::remove_file(root.join("old-lock")).unwrap();
        assert!(StateLock::acquire_with(&root, || {}, replace)
            .unwrap_err()
            .to_string()
            .contains("replaced"));
        assert_eq!(fs::read(outside).unwrap(), b"never truncate");
    }
    #[cfg(unix)]
    #[test]
    fn replaced_root_is_rejected_even_if_lock_inode_is_moved_with_it() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("root");
        let old = temp.path().join("old");
        let error = StateLock::acquire_with(
            &root,
            || {},
            || {
                fs::rename(&root, &old).unwrap();
                fs::create_dir(&root).unwrap();
                fs::rename(old.join(LOCK_FILE), root.join(LOCK_FILE)).unwrap();
            },
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("state root was replaced"),
            "{error}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn case_aliases_use_directory_identity_for_locks_and_agent_overlap() {
        let temp = tempfile::tempdir().unwrap();
        let desktop = temp.path().join("Desktop");
        let alias = temp.path().join("DESKTOP");
        fs::create_dir(&desktop).unwrap();
        // Case-sensitive volumes have no alternate-casing alias to exercise.
        if !alias.exists() {
            return;
        }
        let lock = StateLock::acquire(&desktop).unwrap();
        assert!(matches!(
            StateLock::acquire(&alias),
            Err(LockError::Busy { .. })
        ));
        assert!(separate_agent_root(&alias, &desktop).is_err());
        assert!(separate_agent_root(&alias.join("phones"), &desktop).is_err());
        assert!(separate_agent_root(&alias, &desktop.join("missing")).is_err());
        assert_eq!(lock.identity(), directory_identity(&alias).unwrap());
    }

    #[test]
    fn missing_case_aliased_agent_storage_overlap_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let desktop = temp.path().join("Desktop");
        let alias = temp.path().join("DESKTOP");
        for agent in [&alias, &alias.join("phones")] {
            assert_eq!(
                separate_agent_root(agent, &desktop).unwrap_err().kind(),
                io::ErrorKind::PermissionDenied
            );
            assert_eq!(
                separate_agent_root(&desktop, agent).unwrap_err().kind(),
                io::ErrorKind::PermissionDenied
            );
        }
        assert!(!desktop.exists());
        assert!(!alias.exists());
        let separate = temp.path().join("DESKTOP-agent");
        assert_eq!(
            separate_agent_root(&separate, &desktop).unwrap(),
            resolve_root(&separate).unwrap()
        );
    }

    #[test]
    fn agent_storage_overlap_is_refused_without_creating_roots() {
        let temp = tempfile::tempdir().unwrap();
        let desktop = temp.path().join("desktop");
        for agent in [&desktop, &desktop.join("phones"), temp.path()] {
            assert!(separate_agent_root(agent, &desktop)
                .unwrap_err()
                .to_string()
                .contains("overlaps"));
        }
        assert!(!desktop.exists());
        let separate = temp.path().join("desktop-agent");
        assert_eq!(
            separate_agent_root(&separate, &desktop).unwrap(),
            resolve_root(&separate).unwrap()
        );
        #[cfg(unix)]
        {
            fs::create_dir(&desktop).unwrap();
            let alias = temp.path().join("alias");
            std::os::unix::fs::symlink(&desktop, &alias).unwrap();
            assert!(separate_agent_root(&alias.join("phones"), &desktop).is_err());
            assert!(separate_agent_root(&desktop, &alias.join("missing")).is_err());
        }
    }
}
