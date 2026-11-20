//! GPUI persistence guard and new-service boundary, NOT a process sandbox.
//! Path checks and rename remain subject to filesystem replacement (TOCTOU).
//! Existing terminal and Git functionality retains its stage 1–3 authority.
use std::{
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
};

fn refuse(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message.into())
}
fn absolute(path: &Path) -> io::Result<()> {
    if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(refuse(format!(
            "absolute path without traversal required: {}",
            path.display()
        )));
    }
    Ok(())
}
/// Resolve missing tails, but never mistake a dangling link or IO error for absence.
fn resolve(path: &Path) -> io::Result<PathBuf> {
    absolute(path)?;
    let mut result = PathBuf::new();
    for part in path.components() {
        result.push(part);
        match fs::symlink_metadata(&result) {
            Ok(_) => result = fs::canonicalize(&result)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(result)
}
fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

#[derive(Clone, Debug)]
pub struct ProtectedRoots {
    paths: Vec<PathBuf>,
}
impl ProtectedRoots {
    /// Supply the actual host home and active production overrides at composition.
    pub fn new(home: &Path, overrides: impl IntoIterator<Item = PathBuf>) -> io::Result<Self> {
        absolute(home)?;
        let mut paths: Vec<_> = [
            ".config/gitterm-v3",
            ".config/gitterm-v4",
            ".config/gitterm-v5",
            ".config/gitterm-v6",
            ".claude",
            ".claude.json",
            ".codex",
            ".pi",
        ]
        .into_iter()
        .map(|name| home.join(name))
        .collect();
        paths.extend(overrides);
        let resolved = paths
            .iter()
            .map(|path| resolve(path))
            .collect::<io::Result<Vec<_>>>()?;
        // Retain the visible namespace too: a production symlink's parent is
        // still unsuitable even when its target lives somewhere else.
        let visible = paths
            .iter()
            .filter_map(|path| path.parent().zip(path.file_name()))
            .map(|(parent, name)| resolve(parent).map(|parent| parent.join(name)))
            .collect::<io::Result<Vec<_>>>()?;
        paths.extend(resolved);
        paths.extend(visible);
        paths.sort();
        paths.dedup();
        Ok(Self { paths })
    }
    pub fn check(&self, path: &Path) -> io::Result<PathBuf> {
        let actual = resolve(path)?;
        for protected in &self.paths {
            if overlaps(&actual, protected) || overlaps(&actual, &resolve(protected)?) {
                return Err(refuse(format!(
                    "protected source overlap: {}",
                    path.display()
                )));
            }
        }
        for ancestor in actual.ancestors() {
            if ancestor == actual && fs::symlink_metadata(ancestor).is_ok_and(|m| !m.is_dir()) {
                continue;
            }
            match fs::symlink_metadata(ancestor.join(".gitterm-protected")) {
                Ok(_) => {
                    return Err(refuse(format!(
                        "protected sentinel at {}",
                        ancestor.display()
                    )))
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        Ok(actual)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    Terminal,
    Git,
    Snapshot,
    Agent,
    Worker,
    Helper,
    Resume,
    MutateHistory,
    RestoreProcess,
}
/// Existing terminal/Git effects stay open; new C2 services remain closed.
#[derive(Clone, Copy, Debug)]
pub struct Capabilities {
    _private: (),
}
impl Capabilities {
    pub fn authorize(&self, effect: Effect, target: &Path) -> io::Result<()> {
        if matches!(effect, Effect::Terminal | Effect::Git) {
            return Ok(());
        }
        Err(refuse(format!(
            "preview disables {effect:?}: {}",
            target.display()
        )))
    }
}
#[derive(Debug)]
pub struct Preview {
    root: PathBuf,
    protected: ProtectedRoots,
    capabilities: Capabilities,
}
impl Preview {
    pub fn validate(root: &Path, protected: ProtectedRoots) -> io::Result<Self> {
        let root = protected.check(root)?;
        let preview = Self {
            root,
            protected,
            capabilities: Capabilities { _private: () },
        };
        for path in [
            preview.config(),
            preview.layout(),
            preview.history(),
            preview.snapshots(),
            preview.transcripts(),
            preview.provider(),
        ] {
            preview.scoped(&path)?;
        }
        Ok(preview)
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn config(&self) -> PathBuf {
        self.root.join("desktop")
    }
    pub fn layout(&self) -> PathBuf {
        self.root.clone()
    }
    pub fn history(&self) -> PathBuf {
        self.root.join("history")
    }
    pub fn snapshots(&self) -> PathBuf {
        self.root.join("artifacts/snapshots")
    }
    pub fn provider(&self) -> PathBuf {
        self.root.join("providers/claude")
    }
    pub fn transcripts(&self) -> PathBuf {
        self.provider().join("projects")
    }
    pub fn capabilities(&self) -> Capabilities {
        self.capabilities
    }
    pub fn protected(&self) -> &ProtectedRoots {
        &self.protected
    }
    pub fn scoped(&self, path: &Path) -> io::Result<PathBuf> {
        let actual = self.protected.check(path)?;
        if !actual.starts_with(&self.root)
            || actual != path
            || self.protected.check(&self.root)? != self.root
        {
            return Err(refuse(format!(
                "preview path escaped or aliased: {}",
                path.display()
            )));
        }
        Ok(actual)
    }
    /// Shell layout/catalog only. Imported history and provider state have no writer.
    #[cfg(test)]
    fn replace_layout(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        self.replace_layout_checked(path, bytes, || Ok(()))
    }
    fn replace_layout_checked(
        &self,
        path: &Path,
        bytes: &[u8],
        before_replace: impl Fn() -> io::Result<()>,
    ) -> io::Result<()> {
        let allowed = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|name| {
                name == "local-workspaces.json"
                    || name
                        .strip_prefix("layout-")
                        .and_then(|n| n.strip_suffix(".json"))
                        .is_some_and(|key| {
                            (!key.is_empty() && key.bytes().all(|c| c.is_ascii_digit()))
                                || key.strip_prefix("workspace-").is_some_and(|hash| {
                                    hash.len() == 16 && hash.bytes().all(|c| c.is_ascii_hexdigit())
                                })
                        })
            });
        if path.parent() != Some(self.root()) || !allowed {
            return Err(refuse("not a layout destination"));
        }
        self.scoped(path)?;
        let parent = path
            .parent()
            .ok_or_else(|| refuse("destination has no parent"))?;
        self.create_private_dir(parent)?;
        self.scoped(path)?;
        match fs::symlink_metadata(path) {
            Ok(meta) if meta.is_file() => {}
            Ok(_) => return Err(refuse("destination is not a regular file")),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let temp = parent.join(format!(".preview-{}.tmp", uuid::Uuid::new_v4()));
        self.scoped(&temp)?;
        let result = (|| {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temp)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            self.scoped(path)?;
            self.scoped(&temp)?;
            before_replace()?;
            fs::rename(&temp, path)
        })();
        if result.is_err() && self.scoped(&temp).is_ok() {
            let _ = fs::remove_file(&temp);
        }
        result
    }
    /// Create validated scoped directories without granting file-write or execution authority.
    pub fn create_private_dir(&self, path: &Path) -> io::Result<()> {
        self.scoped(path)?;
        // Initialization may mkdir missing ancestors of the selected root (for
        // example .config or the sample parent). Never chmod existing parents,
        // and revalidate the full scoped destination before each creation.
        for ancestor in path.ancestors().collect::<Vec<_>>().into_iter().rev() {
            match fs::symlink_metadata(ancestor) {
                Ok(meta) => {
                    if !meta.is_dir() || resolve(ancestor)? != ancestor {
                        return Err(refuse(format!(
                            "preview parent is not a plain directory: {}",
                            ancestor.display()
                        )));
                    }
                    #[cfg(unix)]
                    if ancestor != self.root && ancestor.starts_with(&self.root) {
                        use std::os::unix::fs::PermissionsExt;
                        if meta.permissions().mode() & 0o077 != 0 {
                            return Err(refuse(format!(
                                "preview directory is not private: {}",
                                ancestor.display()
                            )));
                        }
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    self.scoped(path)?;
                    if resolve(ancestor)? != ancestor {
                        return Err(refuse("preview parent changed"));
                    }
                    let mut builder = fs::DirBuilder::new();
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::DirBuilderExt;
                        builder.mode(0o700);
                    }
                    match builder.create(ancestor) {
                        Ok(()) => {}
                        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                            // Another instance may have won fresh-root creation.
                            let meta = fs::symlink_metadata(ancestor)?;
                            if !meta.is_dir() || resolve(ancestor)? != ancestor {
                                return Err(refuse("preview parent changed during creation"));
                            }
                            self.scoped(path)?;
                        }
                        Err(e) => return Err(e),
                    }
                }
                Err(e) => return Err(e),
            }
        }
        self.scoped(path)?;
        Ok(())
    }
    /// Each context opens its own handle. The persistent file is never unlinked:
    /// the kernel lock, not marker contents or a copied PID, owns each save.
    pub fn claim_writer(self: &std::sync::Arc<Self>) -> io::Result<WriterLease> {
        self.create_private_dir(&self.layout())?;
        let path = self.layout().join(".writer");
        self.scoped(&path)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = match options.open(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                check_lock_path(&path)?;
                OpenOptions::new().read(true).write(true).open(&path)?
            }
            Err(e) => return Err(e),
        };
        let lease = WriterLease {
            preview: self.clone(),
            path,
            file: std::sync::Mutex::new(Some(file)),
        };
        lease.check_identity(lease.file.lock().unwrap().as_ref().unwrap())?;
        Ok(lease)
    }
}
fn check_lock_path(path: &Path) -> io::Result<fs::Metadata> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_file() {
        return Err(refuse("preview lease is not a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.nlink() != 1 {
            return Err(refuse("preview lease is hard-linked"));
        }
    }
    Ok(meta)
}
pub struct WriterLease {
    preview: std::sync::Arc<Preview>,
    path: PathBuf,
    file: std::sync::Mutex<Option<fs::File>>,
}
impl WriterLease {
    pub fn preview(&self) -> &std::sync::Arc<Preview> {
        &self.preview
    }
    fn check_identity(&self, file: &fs::File) -> io::Result<()> {
        self.preview.scoped(&self.path)?;
        let actual = check_lock_path(&self.path)?;
        let opened = file.metadata()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if actual.dev() != opened.dev() || actual.ino() != opened.ino() {
                return Err(refuse("preview lease was replaced"));
            }
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = (actual, opened);
            Err(refuse("preview lease identity requires Unix file identity"))
        }
    }
    /// Blocking disk operation. UI hosts must call this from their ordered save
    /// queue, not from update/render callbacks. A crash releases the OS lock.
    pub fn replace_layout(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        let mut guard = self
            .file
            .lock()
            .map_err(|_| refuse("preview writer lock poisoned"))?;
        let file = guard
            .as_ref()
            .ok_or_else(|| refuse("preview writer is closed"))?;
        self.check_identity(file)?;
        file.lock()?;
        let result = self.check_identity(file).and_then(|()| {
            self.preview
                .replace_layout_checked(path, bytes, || self.check_identity(file))
        });
        if let Err(error) = file.unlock() {
            // Dropping also releases ownership; never reuse an uncertain lock.
            *guard = None;
            return Err(io::Error::new(
                error.kind(),
                format!("unlock preview writer: {error}"),
            ));
        }
        result
    }
    /// Disable surviving handles after any current write. Never remove the lock
    /// file: another instance may already be waiting on that same inode.
    pub fn close(&self) -> io::Result<()> {
        self.file
            .lock()
            .map_err(|_| refuse("preview writer lock poisoned"))?
            .take();
        Ok(())
    }
}
impl Drop for WriterLease {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

const MAX_METADATA: u64 = 4096;
fn metadata_text(path: &Path) -> io::Result<String> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_file() || meta.len() > MAX_METADATA {
        return Err(refuse("unsupported git metadata"));
    }
    let mut text = String::new();
    fs::File::open(path)?
        .take(MAX_METADATA + 1)
        .read_to_string(&mut text)?;
    if text.len() as u64 > MAX_METADATA || text.contains('\0') {
        return Err(refuse("invalid git metadata"));
    }
    Ok(text)
}
fn git_target(base: &Path, text: &str) -> io::Result<PathBuf> {
    let text = text.strip_suffix('\n').unwrap_or(text);
    let text = text.strip_suffix('\r').unwrap_or(text);
    if text.is_empty() || text.contains(['\r', '\n']) {
        return Err(refuse("malformed git metadata"));
    }
    // Git's relative metadata may contain `..`; canonicalize an existing target.
    fs::canonicalize(base.join(text))
}
#[derive(Debug)]
pub struct GitSource {
    pub checkout: PathBuf,
    pub administrative: PathBuf,
    pub common: PathBuf,
}
/// Bounded enclosing-checkout discovery without executing Git. Acceptance does
/// not authorize Git: config, helpers, filters and hooks remain executable inputs.
pub fn inspect_git(selected: &Path, protected: &ProtectedRoots) -> io::Result<Option<GitSource>> {
    let selected = protected.check(selected)?;
    if !selected.is_dir() {
        return Err(refuse("source is not a directory"));
    }
    for (depth, checkout) in selected.ancestors().enumerate() {
        if depth >= 128 {
            return Err(refuse("git discovery limit"));
        }
        let dotgit = checkout.join(".git");
        let meta = match fs::symlink_metadata(&dotgit) {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        let checkout = protected.check(checkout)?;
        protected.check(&dotgit)?;
        let administrative = if meta.is_dir() {
            dotgit
        } else if meta.is_file() {
            let text = metadata_text(&dotgit)?;
            let target = text
                .strip_prefix("gitdir: ")
                .ok_or_else(|| refuse("malformed gitdir"))?;
            git_target(&checkout, target)?
        } else {
            return Err(refuse("unsupported .git entry"));
        };
        let administrative = protected.check(&administrative)?;
        if !administrative.is_dir() {
            return Err(refuse("missing git administrative directory"));
        }
        let common_file = administrative.join("commondir");
        let common = match fs::symlink_metadata(&common_file) {
            Ok(_) => git_target(&administrative, &metadata_text(&common_file)?)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => administrative.clone(),
            Err(e) => return Err(e),
        };
        let common = protected.check(&common)?;
        if !common.is_dir() {
            return Err(refuse("missing git common directory"));
        }
        // A common directory cannot point to another administrative directory.
        if common_file.symlink_metadata().is_ok() {
            match fs::symlink_metadata(common.join("commondir")) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
                Ok(_) => return Err(refuse("recursive commondir")),
            }
        }
        return Ok(Some(GitSource {
            checkout,
            administrative,
            common,
        }));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (tempfile::TempDir, ProtectedRoots, Preview) {
        let d = tempfile::tempdir().unwrap();
        let protected =
            ProtectedRoots::new(&d.path().join("host"), [d.path().join("active-v6")]).unwrap();
        let p = Preview::validate(&d.path().join("preview"), protected.clone()).unwrap();
        (d, protected, p)
    }
    #[test]
    fn protected_roots_ancestors_missing_tails_traversal_and_sentinels() {
        let (d, protected, p) = fixture();
        for target in [
            d.path().to_path_buf(),
            d.path().join("host"),
            d.path().join("host/.config"),
            d.path().join("host/.config/gitterm-v6"),
            d.path().join("host/.config/gitterm-v6/missing/tail"),
            d.path().join("active-v6/tail"),
            d.path().join("host/.claude/projects"),
        ] {
            assert!(
                Preview::validate(&target, protected.clone()).is_err(),
                "{}",
                target.display()
            );
        }
        assert!(Preview::validate(Path::new("relative"), protected.clone()).is_err());
        assert!(p
            .replace_layout(&p.layout().join("../escape"), b"bad")
            .is_err());
        assert!(p
            .replace_layout(&p.history().join("imported.jsonl"), b"bad")
            .is_err());
        p.replace_layout(&p.layout().join("layout-1.json"), b"good")
            .unwrap();
        fs::write(p.root().join(".gitterm-protected"), "sentinel").unwrap();
        assert!(p
            .replace_layout(&p.layout().join("layout-1.json"), b"bad")
            .is_err());
    }
    #[cfg(unix)]
    #[test]
    fn dangling_links_loops_hardlinks_and_aliases_after_validation() {
        use std::os::unix::fs::symlink;
        let (d, protected, p) = fixture();
        let bad = d.path().join("bad");
        symlink(d.path().join("absent"), &bad).unwrap();
        assert!(Preview::validate(&bad, protected.clone()).is_err());
        let looped = d.path().join("loop");
        symlink(&looped, &looped).unwrap();
        assert!(Preview::validate(&looped, protected.clone()).is_err());
        let source = d.path().join("active-v6");
        fs::create_dir_all(&source).unwrap();
        let sentinel = source.join("sentinel");
        fs::write(&sentinel, b"unchanged").unwrap();
        p.create_private_dir(&p.layout()).unwrap();
        let dest = p.layout().join("layout-2.json");
        fs::hard_link(&sentinel, &dest).unwrap();
        p.replace_layout(&dest, b"new inode").unwrap();
        assert_eq!(fs::read(&sentinel).unwrap(), b"unchanged");
        fs::remove_file(&dest).unwrap();
        symlink(&sentinel, &dest).unwrap();
        assert!(p.replace_layout(&dest, b"bad").is_err());
        fs::rename(p.layout(), d.path().join("old-preview")).unwrap();
        symlink(&source, p.layout()).unwrap();
        assert!(p
            .replace_layout(&p.layout().join("layout-1.json"), b"bad")
            .is_err());
        assert_eq!(fs::read(&sentinel).unwrap(), b"unchanged");
    }
    #[cfg(unix)]
    #[test]
    fn protection_retains_original_alias_target_and_tracks_retargeting() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("original");
        let later = dir.path().join("later");
        fs::create_dir(&original).unwrap();
        fs::create_dir(&later).unwrap();
        let alias = dir.path().join("active");
        symlink(&original, &alias).unwrap();
        let protected = ProtectedRoots::new(&dir.path().join("home"), [alias.clone()]).unwrap();
        fs::remove_file(&alias).unwrap();
        symlink(&later, &alias).unwrap();
        for path in [&original, &later, &alias] {
            assert!(protected.check(&path.join("tail")).is_err());
        }
        let parent = dir.path().join("visible-parent");
        fs::create_dir(&parent).unwrap();
        let alias = parent.join("production");
        symlink(&original, &alias).unwrap();
        let protected = ProtectedRoots::new(&dir.path().join("home"), [alias]).unwrap();
        assert!(protected.check(&parent).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn existing_public_root_and_stale_marker_preserve_compatibility() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, _protected, p) = fixture();
        fs::create_dir(p.root()).unwrap();
        fs::set_permissions(p.root(), fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(p.layout().join(".writer"), "stale marker, not authority").unwrap();
        let p = std::sync::Arc::new(p);
        let first = p.claim_writer().unwrap();
        let second = p.claim_writer().unwrap();
        for writer in [&first, &second] {
            writer
                .replace_layout(&p.layout().join("layout-1.json"), b"saved")
                .unwrap();
        }
        assert_eq!(
            fs::metadata(p.root()).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(
            fs::read(p.root().join(".writer")).unwrap(),
            b"stale marker, not authority"
        );
    }
    #[cfg(unix)]
    #[test]
    fn only_legacy_effects_open_and_new_service_destinations_closed() {
        let (_d, _protected, p) = fixture();
        for effect in [Effect::Terminal, Effect::Git] {
            assert!(p
                .capabilities()
                .authorize(effect, Path::new("/workspace"))
                .is_ok());
        }
        for effect in [
            Effect::Snapshot,
            Effect::Agent,
            Effect::Worker,
            Effect::Helper,
            Effect::Resume,
            Effect::MutateHistory,
            Effect::RestoreProcess,
        ] {
            let target = p.history().join("copied-id-and-pid");
            let error = p.capabilities().authorize(effect, &target).unwrap_err();
            assert!(error.to_string().contains(target.to_str().unwrap()));
        }
        let p = std::sync::Arc::new(p);
        let writer = p.claim_writer().unwrap();
        for path in [
            p.history().join("layout-1.json"),
            p.provider().join("layout-1.json"),
            p.root().join("unrelated.json"),
            p.root().join(".writer"),
            p.root().join("nested/layout-1.json"),
        ] {
            assert!(writer.replace_layout(&path, b"bad").is_err());
        }
        // A failed write must release the lock too.
        let next = p.claim_writer().unwrap();
        let guard = next.file.lock().unwrap();
        guard.as_ref().unwrap().try_lock().unwrap();
        guard.as_ref().unwrap().unlock().unwrap();
        drop(guard);
        next.replace_layout(&p.root().join("layout-1.json"), b"good")
            .unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn fresh_sample_and_nested_override_create_only_missing_private_parents() {
        let dir = tempfile::tempdir().unwrap();
        let protected = ProtectedRoots::new(&dir.path().join("host"), []).unwrap();
        for root in [
            dir.path().join("config/sample"),
            dir.path().join("missing/nested/preview"),
        ] {
            let p = std::sync::Arc::new(Preview::validate(&root, protected.clone()).unwrap());
            let writer = p.claim_writer().unwrap();
            writer
                .replace_layout(&p.layout().join("layout-1.json"), b"fixture")
                .unwrap();
            writer.close().unwrap();
            assert!(p.layout().join(".writer").is_file());
        }
        let sibling = dir.path().join("shared/production");
        fs::create_dir_all(&sibling).unwrap();
        fs::write(sibling.join("sentinel"), b"unchanged").unwrap();
        let parent = sibling.parent().unwrap();
        let permissions = fs::metadata(parent).unwrap().permissions();
        let protected = ProtectedRoots::new(&dir.path().join("host"), [sibling.clone()]).unwrap();
        let p = std::sync::Arc::new(
            Preview::validate(&parent.join("new/nested/preview"), protected).unwrap(),
        );
        let writer = p.claim_writer().unwrap();
        writer.close().unwrap();
        assert_eq!(fs::metadata(parent).unwrap().permissions(), permissions);
        assert_eq!(fs::read(sibling.join("sentinel")).unwrap(), b"unchanged");
    }
    #[cfg(unix)]
    #[test]
    fn closing_writer_denies_late_writes_and_drop_cannot_remove_next_owners_lease() {
        let (_d, _protected, p) = fixture();
        let p = std::sync::Arc::new(p);
        let first = p.claim_writer().unwrap();
        first.close().unwrap();
        let next = p.claim_writer().unwrap();
        assert!(first
            .replace_layout(&p.layout().join("layout-2.json"), b"bad")
            .is_err());
        drop(first);
        assert!(p.claim_writer().is_ok());
        next.replace_layout(&p.layout().join("layout-3.json"), b"good")
            .unwrap();
        next.close().unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn lease_subprocess() {
        let Some(root) = std::env::var_os("GITTERM_TEST_LEASE_ROOT") else {
            return;
        };
        let root = PathBuf::from(root);
        let p = std::sync::Arc::new(
            Preview::validate(
                &root,
                ProtectedRoots::new(&root.parent().unwrap().join("host"), []).unwrap(),
            )
            .unwrap(),
        );
        let writer = p.claim_writer().unwrap();
        if std::env::var_os("GITTERM_TEST_HOLD_LEASE").is_some() {
            let guard = writer.file.lock().unwrap();
            guard.as_ref().unwrap().lock().unwrap();
            println!("LEASE_READY");
            std::io::stdout().flush().unwrap();
            std::io::stdin().read_exact(&mut [0]).unwrap();
            // Simulate abrupt process termination: no WriterLease::close or Drop.
            std::process::exit(0);
        }
        for i in 0..20 {
            writer
                .replace_layout(
                    &p.layout().join("layout-1.json"),
                    &serde_json::to_vec(&serde_json::json!({"pid":std::process::id(),"i":i}))
                        .unwrap(),
                )
                .unwrap();
            let bytes = fs::read(p.layout().join("layout-1.json")).unwrap();
            assert!(serde_json::from_slice::<serde_json::Value>(&bytes).is_ok());
        }
    }
    #[cfg(unix)]
    #[test]
    fn concurrent_fresh_instances_save_and_crashed_owner_releases_lease() {
        use std::{
            io::BufRead,
            process::{Command, Stdio},
        };
        let (dir, _protected, _) = fixture();
        let root = dir.path().join("fresh/nested/preview");
        let launch = || {
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args(["--exact", "preview::tests::lease_subprocess", "--nocapture"])
                .env("GITTERM_TEST_LEASE_ROOT", &root);
            command
        };
        let mut one = launch()
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let two = launch().output().unwrap();
        assert!(
            two.status.success(),
            "{}",
            String::from_utf8_lossy(&two.stderr)
        );
        assert!(one.wait().unwrap().success());
        let mut crashed = launch()
            .env("GITTERM_TEST_HOLD_LEASE", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut lines = std::io::BufReader::new(crashed.stdout.take().unwrap()).lines();
        assert!(lines.any(|line| line.unwrap() == "LEASE_READY"));
        let p = std::sync::Arc::new(
            Preview::validate(
                &root,
                ProtectedRoots::new(&root.parent().unwrap().join("host"), []).unwrap(),
            )
            .unwrap(),
        );
        let second = p.claim_writer().unwrap(); // Startup does not wait for an active save.
        assert!(matches!(
            second.file.lock().unwrap().as_ref().unwrap().try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        crashed.stdin.take().unwrap().write_all(b"exit").unwrap();
        assert!(crashed.wait().unwrap().success());
        second
            .replace_layout(&p.layout().join("layout-2.json"), b"saved after crash")
            .unwrap();
        assert!(p.root().join(".writer").is_file());
    }
    #[cfg(unix)]
    #[test]
    fn replaced_symlinked_and_hardlinked_lease_files_fail_closed() {
        use std::os::unix::fs::symlink;
        let (dir, _protected, p) = fixture();
        let p = std::sync::Arc::new(p);
        let writer = p.claim_writer().unwrap();
        let path = p.root().join(".writer");
        fs::rename(&path, p.root().join("old-writer")).unwrap();
        fs::write(&path, b"replacement").unwrap();
        assert!(writer
            .replace_layout(&p.root().join("layout-1.json"), b"bad")
            .is_err());
        writer.close().unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"replacement");
        let source = dir.path().join("active-v6");
        fs::create_dir(&source).unwrap();
        let sentinel = source.join("sentinel");
        fs::write(&sentinel, b"unchanged").unwrap();
        fs::remove_file(&path).unwrap();
        symlink(&sentinel, &path).unwrap();
        assert!(p.claim_writer().is_err());
        fs::remove_file(&path).unwrap();
        fs::hard_link(&sentinel, &path).unwrap();
        assert!(p.claim_writer().is_err());
        assert_eq!(fs::read(&sentinel).unwrap(), b"unchanged");
    }
    #[test]
    fn git_metadata_independent_linked_subdirectories_and_protected_common() {
        let (d, protected, _p) = fixture();
        let checkout = d.path().join("checkout");
        fs::create_dir_all(checkout.join(".git")).unwrap();
        fs::create_dir_all(checkout.join("src/deep")).unwrap();
        assert_eq!(
            inspect_git(&checkout.join("src/deep"), &protected)
                .unwrap()
                .unwrap()
                .checkout,
            checkout.canonicalize().unwrap()
        );
        let linked = d.path().join("linked");
        fs::create_dir(&linked).unwrap();
        let admin = checkout.join(".git/worktrees/linked");
        fs::create_dir_all(&admin).unwrap();
        fs::write(
            linked.join(".git"),
            "gitdir: ../checkout/.git/worktrees/linked\n",
        )
        .unwrap();
        fs::write(admin.join("commondir"), "../..\n").unwrap();
        let git = inspect_git(&linked, &protected).unwrap().unwrap();
        assert_eq!(git.common, checkout.join(".git").canonicalize().unwrap());
        let production = d.path().join("active-v6");
        fs::create_dir_all(&production).unwrap();
        fs::write(admin.join("commondir"), production.to_str().unwrap()).unwrap();
        assert!(inspect_git(&linked, &protected).is_err());
        for invalid in [
            "gitdir:",
            "gitdir: missing",
            "gitdir: one\ntwo",
            &"x".repeat(4097),
        ] {
            fs::write(linked.join(".git"), invalid).unwrap();
            assert!(inspect_git(&linked, &protected).is_err());
        }
        // No process API is used by inspect_git, including discovery failures.
    }
    #[cfg(unix)]
    #[test]
    fn git_validation_never_executes_the_path_git_trap() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let trap = dir.path().join("git");
        let marker = dir.path().join("executed-git");
        fs::write(
            &trap,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$GITTERM_TEST_GIT_MARKER\"\nexit 99\n",
        )
        .unwrap();
        fs::set_permissions(&trap, fs::Permissions::from_mode(0o700)).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact","preview::tests::git_metadata_independent_linked_subdirectories_and_protected_common"])
            .env("PATH",dir.path()).env("GITTERM_TEST_GIT_MARKER", &marker).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(!marker.exists(), "validation executed Git");
    }
    #[cfg(unix)]
    #[test]
    fn git_dangling_looping_and_oversized_common_metadata() {
        use std::os::unix::fs::symlink;
        let (d, protected, _) = fixture();
        let root = d.path().join("checkout");
        fs::create_dir_all(root.join(".git")).unwrap();
        let common = root.join(".git/commondir");
        for invalid in ["missing", ".", "one\ntwo", "../..\n\n", &"x".repeat(4097)] {
            fs::write(&common, invalid).unwrap();
            assert!(inspect_git(&root, &protected).is_err());
        }
        fs::remove_file(&common).unwrap();
        symlink(&common, &common).unwrap();
        assert!(inspect_git(&root, &protected).is_err());
        fs::remove_file(&common).unwrap();
        fs::remove_dir(root.join(".git")).unwrap();
        symlink(root.join("missing"), root.join(".git")).unwrap();
        assert!(inspect_git(&root, &protected).is_err());
    }
}
