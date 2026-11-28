//! Byte-preserving, verified Unix desktop backup. The caller holds the state lease.
use ring::digest::{Context, SHA256};
use serde::{Deserialize, Serialize};
use std::{
    ffi::{CStr, CString},
    fs,
    io::{self, Read, Write},
    ops::ControlFlow,
    os::{
        fd::{AsRawFd, FromRawFd, IntoRawFd},
        unix::{ffi::OsStrExt, fs::OpenOptionsExt},
    },
    path::{Path, PathBuf},
    time::{Duration, Instant},
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
    sync_file(&fs::File::open(path)?)?;
    #[cfg(test)]
    trace_durability("directory-sync", path);
    Ok(())
}
fn full_sync(path: &Path) -> io::Result<()> {
    read_regular(path)?.sync_all()?;
    #[cfg(test)]
    trace_durability("full-sync", path);
    Ok(())
}
#[cfg(test)]
thread_local! {
    static DURABILITY_TRACE: std::cell::RefCell<Option<Vec<(&'static str, PathBuf)>>> = const { std::cell::RefCell::new(None) };
}
#[cfg(test)]
fn trace_durability(operation: &'static str, path: &Path) {
    DURABILITY_TRACE.with_borrow_mut(|trace| {
        if let Some(trace) = trace {
            trace.push((operation, path.to_owned()));
        }
    });
}
fn sync_file(file: &fs::File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    // Rust's sync_all uses F_FULLFSYNC on macOS. Flush the drive cache once,
    // after verification, rather than once for every snapshot and directory.
    if unsafe { libc::fsync(file.as_raw_fd()) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
fn write_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = private_file(path)?;
    file.write_all(bytes)?;
    #[cfg(test)]
    trace_durability("write", path);
    sync_file(&file)?;
    #[cfg(test)]
    trace_durability("file-sync", path);
    Ok(())
}
fn transfer(
    file: &mut fs::File,
    mut target: Option<&mut fs::File>,
    mut chunk: impl FnMut(u64) -> io::Result<()>,
) -> io::Result<(u64, String)> {
    let mut context = Context::new(&SHA256);
    let mut size = 0;
    let mut buffer = [0; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        if let Some(target) = target.as_mut() {
            target.write_all(&buffer[..count])?;
        }
        size += count as u64;
        context.update(&buffer[..count]);
        chunk(count as u64)?;
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
// Keep the original parent open across backup creation and admission. Cleanup
// may inspect paths to fail closed, but every deletion is relative to these fds.
#[derive(Debug)]
pub struct CreatedBackup {
    destination: PathBuf,
    parent: fs::File,
    // Pin the inode so removing/replacing the directory cannot recycle its id.
    _directory: fs::File,
    identity: (u64, u64),
}
impl CreatedBackup {
    pub fn path(&self) -> &Path {
        &self.destination
    }

    pub fn remove(self) -> io::Result<()> {
        let parent_path = self.destination.parent().unwrap();
        let parent_meta = fs::symlink_metadata(parent_path)?;
        if !parent_meta.is_dir()
            || directory_identity(&parent_meta) != directory_identity(&self.parent.metadata()?)
        {
            return Err(invalid(
                "backup parent identity changed; leaving backup in place",
            ));
        }
        let name = CString::new(self.destination.file_name().unwrap().as_bytes())?;
        remove_directory_at(&self.parent, &name, self.identity)
    }
}
fn open_directory_at(parent: &fs::File, name: &CStr) -> io::Result<fs::File> {
    // name is one NUL-terminated component, and the returned fd is owned here.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_DIRECTORY,
        )
    };
    if fd == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { fs::File::from_raw_fd(fd) })
}
fn stat_at(parent: &fs::File, name: &CStr) -> io::Result<libc::stat> {
    let mut stat = std::mem::MaybeUninit::uninit();
    // Do not follow a replacement symlink, even at the final component.
    if unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } == -1
    {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { stat.assume_init() })
}
fn stat_identity(stat: &libc::stat) -> (u64, u64) {
    #[allow(clippy::unnecessary_cast)] // Unix stat widths differ.
    (stat.st_dev as u64, stat.st_ino as u64)
}
fn check_identity(parent: &fs::File, name: &CStr, identity: (u64, u64)) -> io::Result<libc::stat> {
    let stat = stat_at(parent, name)?;
    if stat_identity(&stat) != identity {
        return Err(invalid(
            "backup entry identity changed; leaving it in place",
        ));
    }
    Ok(stat)
}
fn directory_names(directory: fs::File) -> io::Result<Vec<CString>> {
    struct Stream(*mut libc::DIR);
    impl Drop for Stream {
        fn drop(&mut self) {
            unsafe {
                libc::closedir(self.0);
            }
        }
    }
    let fd = directory.into_raw_fd();
    // fdopendir takes ownership only on success; Stream closes it on all exits.
    let stream = unsafe { libc::fdopendir(fd) };
    if stream.is_null() {
        let error = io::Error::last_os_error();
        drop(unsafe { fs::File::from_raw_fd(fd) });
        return Err(error);
    }
    let stream = Stream(stream);
    let mut names = vec![];
    loop {
        // readdir reports end-of-stream with a null pointer and unchanged errno.
        #[cfg(any(target_vendor = "apple", target_os = "freebsd"))]
        unsafe {
            *libc::__error() = 0;
        }
        #[cfg(any(
            target_os = "linux",
            target_os = "hurd",
            target_os = "emscripten",
            target_os = "dragonfly"
        ))]
        unsafe {
            *libc::__errno_location() = 0;
        }
        #[cfg(any(target_os = "android", target_os = "netbsd", target_os = "openbsd"))]
        unsafe {
            *libc::__errno() = 0;
        }
        #[cfg(any(target_os = "solaris", target_os = "illumos"))]
        unsafe {
            *libc::___errno() = 0;
        }
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(0) {
                return Err(error);
            }
            break;
        }
        // Copy the name before the next readdir invalidates its storage.
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if name.to_bytes() != b"." && name.to_bytes() != b".." {
            names.push(name.to_owned());
        }
    }
    Ok(names)
}
fn remove_directory_at(parent: &fs::File, name: &CStr, identity: (u64, u64)) -> io::Result<()> {
    check_identity(parent, name, identity)?;
    let directory = open_directory_at(parent, name)?;
    if directory_identity(&directory.metadata()?) != identity {
        return Err(invalid(
            "backup directory identity changed; leaving it in place",
        ));
    }
    // Open "." rather than dup: the enumerator must have its own directory offset.
    let names = directory_names(open_directory_at(&directory, c".")?)?;
    for child in names {
        let stat = stat_at(&directory, &child)?;
        let identity = stat_identity(&stat);
        if stat.st_mode & libc::S_IFMT == libc::S_IFDIR {
            remove_directory_at(&directory, &child, identity)?;
        } else {
            check_identity(&directory, &child, identity)?;
            if unsafe { libc::unlinkat(directory.as_raw_fd(), child.as_ptr(), 0) } == -1 {
                return Err(io::Error::last_os_error());
            }
        }
    }
    check_identity(parent, name, identity)?;
    if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn scan(
    root: &Path,
    relative: &Path,
    browser_released: bool,
    worktrees: &[(u64, u64)],
    entries: &mut Vec<Entry>,
    progress: &mut Progress<impl FnMut(u64, u64, u64, u64) -> ControlFlow<()>>,
) -> io::Result<()> {
    progress.report(false)?;
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
                progress,
            )?;
        }
        Kind::Directory
    } else if meta.is_file() {
        Kind::File {
            size: meta.len(),
            sha256: String::new(),
        }
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
/// is created, normalized or rewritten. Failed and cancelled copies are removed.
pub fn create(root: &Path, parent: &Path, browser_released: bool) -> io::Result<PathBuf> {
    create_with_progress(root, parent, browser_released, |_, _, _, _| {
        ControlFlow::Continue(())
    })
    .map(|backup| backup.destination)
}

/// Report copied regular files and bytes, at most every 100 ms apart from the
/// initial/plan/final reports. Return Break to cancel. Totals come from metadata and
/// are adjusted if a source changes size while it is copied. The returned handle
/// retains the original parent and identity for safe cancellation cleanup.
pub fn create_with_progress(
    root: &Path,
    parent: &Path,
    browser_released: bool,
    progress: impl FnMut(u64, u64, u64, u64) -> ControlFlow<()>,
) -> io::Result<CreatedBackup> {
    create_with(root, parent, browser_released, progress, |_| Ok(()))
}

struct Progress<F> {
    callback: F,
    last: Option<Instant>,
    files_done: u64,
    files_total: u64,
    bytes_done: u64,
    bytes_total: u64,
}
impl<F: FnMut(u64, u64, u64, u64) -> ControlFlow<()>> Progress<F> {
    fn report(&mut self, force: bool) -> io::Result<()> {
        if !force
            && self
                .last
                .is_some_and(|last| last.elapsed() < Duration::from_millis(100))
        {
            return Ok(());
        }
        self.last = Some(Instant::now());
        if (self.callback)(
            self.files_done,
            self.files_total,
            self.bytes_done,
            self.bytes_total,
        )
        .is_break()
        {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "V6 backup cancelled",
            ));
        }
        Ok(())
    }
}
fn create_with(
    root: &Path,
    parent: &Path,
    browser_released: bool,
    callback: impl FnMut(u64, u64, u64, u64) -> ControlFlow<()>,
    before_verify: impl FnOnce(&Path) -> io::Result<()>,
) -> io::Result<CreatedBackup> {
    let mut progress = Progress {
        callback,
        last: None,
        files_done: 0,
        files_total: 0,
        bytes_done: 0,
        bytes_total: 0,
    };
    progress.report(true)?;
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
            &mut progress,
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
    for entry in &manifest.entries {
        if let Kind::File { size, .. } = entry.kind {
            progress.files_total += 1;
            progress.bytes_total = progress
                .bytes_total
                .checked_add(size)
                .ok_or_else(|| invalid("backup size overflow"))?;
        }
    }
    progress.report(true)?;
    let bytes = serde_json::to_vec_pretty(&manifest)?;
    // Planned hashes are empty; reserve their 64 hex characters per file.
    let needed =
        manifest
            .entries
            .iter()
            .try_fold(bytes.len() as u64 + 1024 * 1024, |sum, entry| {
                sum.checked_add(match entry.kind {
                    Kind::File { size, .. } => size
                        .checked_add(64)
                        .ok_or_else(|| invalid("backup size overflow"))?,
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
    let parent_fd = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(&parent)?;
    let name = CString::new(destination.file_name().unwrap().as_bytes())?;
    if unsafe { libc::mkdirat(parent_fd.as_raw_fd(), name.as_ptr(), 0o700) } == -1 {
        return Err(io::Error::last_os_error());
    }
    let directory = open_directory_at(&parent_fd, &name)?;
    let identity = directory_identity(&directory.metadata()?);
    check_identity(&parent_fd, &name, identity)?;
    let created = CreatedBackup {
        destination: destination.clone(),
        parent: parent_fd,
        _directory: directory,
        identity,
    };
    // Only clean up after mkdirat created this run's unique directory.
    // Never remove an existing backup or anything under the source root.
    let result = (|| {
        let data = destination.join("state");
        private_dir(&data)?;
        // Ancestors first, independently of the filesystem's enumeration order.
        manifest.entries.sort_by(|a, b| a.path.cmp(&b.path));
        for entry in &mut manifest.entries {
            progress.report(false)?;
            let target = data.join(&entry.path);
            match &mut entry.kind {
                Kind::Directory => private_dir(&target)?,
                Kind::File { size, sha256 } => {
                    let mut source = read_regular(&root.join(&entry.path))?;
                    let mut target = private_file(&target)?;
                    let planned_total = progress.bytes_total;
                    let (copied, digest) = transfer(&mut source, Some(&mut target), |count| {
                        progress.bytes_done += count;
                        progress.bytes_total = progress.bytes_total.max(progress.bytes_done);
                        progress.report(false)
                    })?;
                    sync_file(&target)?;
                    progress.bytes_total = planned_total
                        .saturating_sub(*size)
                        .saturating_add(copied)
                        .max(progress.bytes_done);
                    *size = copied;
                    *sha256 = digest;
                    progress.files_done += 1;
                }
                _ => {}
            }
        }
        write_new(
            &destination.join(MANIFEST),
            &serde_json::to_vec_pretty(&manifest)?,
        )?;
        before_verify(&destination)?;
        verify_data(&destination, &root, || progress.report(false))?;
        for entry in manifest.entries.iter().rev() {
            progress.report(false)?;
            if matches!(entry.kind, Kind::Directory) {
                sync_dir(&data.join(&entry.path))?;
            }
        }
        sync_dir(&data)?;
        sync_dir(&destination)?;
        sync_dir(&parent)?;
        // Publish all data AND directory entries before flushing the drive cache.
        full_sync(&destination.join(MANIFEST))?;
        progress.bytes_total = progress.bytes_done;
        progress.report(true)?;
        write_new(&destination.join(COMPLETE), b"verified\n")?;
        sync_dir(&destination)?;
        sync_dir(&parent)?;
        Ok(())
    })();
    if let Err(error) = result {
        if let Err(cleanup) = created.remove() {
            return Err(io::Error::other(format!(
                "{error}; remove partial backup {}: {cleanup}",
                destination.display()
            )));
        }
        return Err(error);
    }
    Ok(created)
}
fn verify_data(
    destination: &Path,
    root: &Path,
    mut check: impl FnMut() -> io::Result<()>,
) -> io::Result<Manifest> {
    let manifest: Manifest = serde_json::from_reader(read_regular(&destination.join(MANIFEST))?)?;
    if manifest.root != root {
        return Err(invalid("backup root differs from selected root"));
    }
    let data = destination.join("state");
    real_dir(&data)?;
    for entry in &manifest.entries {
        check()?;
        if entry.path.is_absolute()
            || entry
                .path
                .components()
                .any(|p| !matches!(p, std::path::Component::Normal(_)))
        {
            return Err(invalid("invalid backup manifest path"));
        }
        // O_NOFOLLOW guards only the last component, so every parent directory
        // must be a real directory too; a symlinked parent could point back at
        // the live root and pass the hash check with no independent copy.
        if !matches!(entry.kind, Kind::File { .. } | Kind::Directory) {
            continue;
        }
        let mut dir = data.clone();
        for component in entry.path.parent().into_iter().flat_map(Path::components) {
            dir.push(component);
            real_dir(&dir)?;
        }
        if matches!(entry.kind, Kind::Directory) {
            real_dir(&data.join(&entry.path))?;
        }
        if let Kind::File { size, ref sha256 } = entry.kind {
            let actual = transfer(&mut read_regular(&data.join(&entry.path))?, None, |_| {
                check()
            })?;
            if actual != (size, sha256.clone()) {
                return Err(invalid(format!(
                    "backup hash mismatch: {}",
                    entry.path.display()
                )));
            }
        }
    }
    #[cfg(test)]
    trace_durability("verified", destination);
    Ok(manifest)
}
fn real_dir(path: &Path) -> io::Result<()> {
    if fs::symlink_metadata(path)?.is_dir() {
        Ok(())
    } else {
        Err(invalid(format!(
            "backup path is not a real directory: {}",
            path.display()
        )))
    }
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
    verify_data(&destination, &root, || Ok(()))
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
        let error = create_with(
            &root,
            &parent,
            true,
            |_, _, _, _| ControlFlow::Continue(()),
            |path| fs::write(path.join("state/workspaces.json"), b"corrupt"),
        )
        .unwrap_err();
        assert!(error.to_string().contains("hash mismatch"));
        assert_eq!(fs::read_dir(&parent).unwrap().count(), 1);
        assert!(backup.join(COMPLETE).exists());
        assert_eq!(fs::read(root.join("workspaces.json")).unwrap(), bytes);
        fs::write(backup.join("state/workspaces.json"), b"corrupt").unwrap();
        assert!(verify(&backup, &root).is_err());
    }
    #[test]
    fn manifest_hashes_the_copy_even_if_source_changes_after_scan_and_copy() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("v6");
        let parent = temp.path().join("backups");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&parent).unwrap();
        let source = root.join("settings.json");
        fs::write(&source, b"before scan").unwrap();
        let mut changed = false;
        let backup = create_with(
            &root,
            &parent,
            true,
            |done, total, _, _| {
                if total == 1 && done == 0 && !changed {
                    // The plan has already inspected the old size and bytes.
                    fs::write(&source, b"bytes actually copied").unwrap();
                    changed = true;
                }
                ControlFlow::Continue(())
            },
            |_| fs::write(&source, b"changed again after copy"),
        )
        .unwrap();
        assert!(changed);
        let backup = backup.path();
        let manifest = verify(backup, &root).unwrap();
        let copy = backup.join("state/settings.json");
        assert_eq!(fs::read(&copy).unwrap(), b"bytes actually copied");
        let (size, sha256) = transfer(&mut read_regular(&copy).unwrap(), None, |_| Ok(())).unwrap();
        assert!(manifest.entries.iter().any(|entry| entry.path == Path::new("settings.json")
            && matches!(&entry.kind, Kind::File { size: n, sha256: digest } if *n == size && digest == &sha256)));
    }

    #[test]
    fn progress_reaches_totals_and_is_throttled() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("v6");
        let parent = temp.path().join("backups");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&parent).unwrap();
        for i in 0..32 {
            fs::write(root.join(format!("file-{i}")), [7; 1024]).unwrap();
        }
        let mut reports = vec![];
        let began = Instant::now();
        create_with_progress(&root, &parent, true, |a, b, c, d| {
            reports.push((a, b, c, d));
            ControlFlow::Continue(())
        })
        .unwrap();
        assert_eq!(reports.last(), Some(&(32, 32, 32 * 1024, 32 * 1024)));
        assert!(reports.len() >= 2);
        assert!(reports.len() <= 3 + began.elapsed().as_millis() as usize / 100);
    }

    #[test]
    fn cancellation_removes_only_its_own_partial_copy_and_preserves_source() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("v6");
        let parent = temp.path().join("backups");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&parent).unwrap();
        fs::write(root.join("settings.json"), b"original").unwrap();
        let previous = create(&root, &parent, true).unwrap();
        let mut saw_partial = false;
        let error = create_with_progress(&root, &parent, true, |done, total, _, _| {
            if total == 1 && done == 1 {
                saw_partial = fs::read_dir(&parent).unwrap().count() == 2;
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(saw_partial);
        assert_eq!(fs::read_dir(&parent).unwrap().count(), 1);
        verify(&previous, &root).unwrap();
        assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
        assert_eq!(fs::read(root.join("settings.json")).unwrap(), b"original");
    }

    #[test]
    fn cancellation_refuses_a_swapped_parent_and_preserves_unrelated_state() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let root = base.join("v6");
        let parent = base.join("backups");
        let moved = base.join("original-backups");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&parent).unwrap();
        fs::write(root.join("settings.json"), b"original").unwrap();
        let mut name = None;
        let error = create_with_progress(&root, &parent, true, |done, total, _, _| {
            if done == 1 && total == 1 {
                let destination = fs::read_dir(&parent)
                    .unwrap()
                    .next()
                    .unwrap()
                    .unwrap()
                    .file_name();
                // The attack points the old destination path into live state.
                fs::create_dir(root.join(&destination)).unwrap();
                fs::write(root.join(&destination).join("unrelated"), b"keep me").unwrap();
                fs::rename(&parent, &moved).unwrap();
                std::os::unix::fs::symlink(&root, &parent).unwrap();
                name = Some(destination);
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        })
        .unwrap_err();
        let name = name.unwrap();
        let error = error.to_string();
        assert!(
            error.contains("cancelled") && error.contains("parent identity changed"),
            "{error}"
        );
        assert_eq!(
            fs::read(root.join(&name).join("unrelated")).unwrap(),
            b"keep me"
        );
        assert_eq!(fs::read(root.join("settings.json")).unwrap(), b"original");
        assert!(moved.join(name).join("state/settings.json").exists());
        assert!(fs::symlink_metadata(parent).unwrap().is_symlink());
    }

    #[test]
    fn cleanup_refuses_a_replaced_destination_and_does_not_follow_child_links() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("v6");
        let parent = temp.path().join("backups");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&parent).unwrap();
        fs::write(root.join("settings.json"), b"original").unwrap();
        let backup =
            create_with_progress(&root, &parent, true, |_, _, _, _| ControlFlow::Continue(()))
                .unwrap();
        let path = backup.path().to_owned();
        let moved = parent.join("moved");
        fs::rename(&path, &moved).unwrap();
        fs::create_dir(&path).unwrap();
        fs::write(path.join("unrelated"), b"keep me").unwrap();
        assert!(backup
            .remove()
            .unwrap_err()
            .to_string()
            .contains("identity changed"));
        assert_eq!(fs::read(path.join("unrelated")).unwrap(), b"keep me");
        assert!(moved.join("state/settings.json").exists());

        let backup =
            create_with_progress(&root, &parent, true, |_, _, _, _| ControlFlow::Continue(()))
                .unwrap();
        let path = backup.path().to_owned();
        std::os::unix::fs::symlink(&root, path.join("state/link")).unwrap();
        backup.remove().unwrap();
        assert!(!path.exists());
        assert_eq!(fs::read(root.join("settings.json")).unwrap(), b"original");
    }

    #[test]
    fn backup_directories_are_synced_before_full_flush_and_completion_marker() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let root = base.join("v6");
        let parent = base.join("backups");
        fs::create_dir_all(root.join("chat-meta/nested")).unwrap();
        fs::create_dir(&parent).unwrap();
        fs::write(root.join("chat-meta/nested/message"), b"original").unwrap();
        DURABILITY_TRACE.with_borrow_mut(|trace| *trace = Some(vec![]));
        let backup = create(&root, &parent, true).unwrap();
        let trace = DURABILITY_TRACE.with_borrow_mut(Option::take).unwrap();
        let index = |operation, path: &Path| {
            trace
                .iter()
                .position(|event| event == &(operation, path.to_owned()))
                .unwrap()
        };
        let flush = index("full-sync", &backup.join(MANIFEST));
        assert_eq!(
            trace.iter().filter(|event| event.0 == "full-sync").count(),
            1
        );
        for directory in [
            &parent,
            &backup,
            &backup.join("state"),
            &backup.join("state/chat-meta"),
            &backup.join("state/chat-meta/nested"),
        ] {
            assert!(
                index("directory-sync", directory) < flush,
                "{} was not synced before drive flush: {trace:?}",
                directory.display()
            );
        }
        assert!(index("verified", &backup) < flush);
        let marker = backup.join(COMPLETE);
        let write = index("write", &marker);
        let file_sync = index("file-sync", &marker);
        assert!(flush < write && write < file_sync);
        assert!(trace
            .iter()
            .enumerate()
            .any(|(i, event)| i > file_sync && event == &("directory-sync", backup.clone())));
        verify(&backup, &root).unwrap();
    }

    #[test]
    fn verify_rejects_symlinked_backup_directories() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("v6");
        let parent = temp.path().join("backups");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&parent).unwrap();
        fs::write(root.join("workspaces.json"), b"{}").unwrap();
        fs::create_dir(root.join("chat-meta")).unwrap();
        fs::write(root.join("chat-meta/raw"), b"chat").unwrap();
        let backup = create(&root, &parent, false).unwrap();
        verify(&backup, &root).unwrap();
        // A symlinked parent directory inside the copy.
        fs::remove_dir_all(backup.join("state/chat-meta")).unwrap();
        std::os::unix::fs::symlink(root.join("chat-meta"), backup.join("state/chat-meta")).unwrap();
        assert!(verify(&backup, &root).is_err());
        // The whole state tree replaced with a link to the live root.
        fs::remove_dir_all(backup.join("state")).ok();
        fs::remove_file(backup.join("state")).ok();
        std::os::unix::fs::symlink(&root, backup.join("state")).unwrap();
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
