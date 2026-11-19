//! Copies-only preview authority. This is a guard, NOT a sandbox: path checks
//! and rename are subject to concurrent filesystem replacement (TOCTOU). No
//! untrusted executable writer is authorized, even after validation succeeds.
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
/// Closed until process-tree confinement and credential behavior are established.
#[derive(Clone, Copy, Debug)]
pub struct Capabilities {
    _private: (),
}
impl Capabilities {
    pub fn authorize(&self, effect: Effect, target: &Path) -> io::Result<()> {
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
        self.root.join("layout")
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
    fn replace_layout(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        if !path.starts_with(self.layout()) {
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
            fs::rename(&temp, path)
        })();
        if result.is_err() && self.scoped(&temp).is_ok() {
            let _ = fs::remove_file(&temp);
        }
        result
    }
    pub(crate) fn create_private_dir(&self, path: &Path) -> io::Result<()> {
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
                    if ancestor.starts_with(&self.root) {
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
                    builder.create(ancestor)?;
                }
                Err(e) => return Err(e),
            }
        }
        self.scoped(path)?;
        Ok(())
    }
    /// Exclusive writable owner; stale leases fail closed. No copied PID is trusted.
    pub fn claim_writer(self: &std::sync::Arc<Self>) -> io::Result<WriterLease> {
        self.create_private_dir(&self.layout())?;
        let path = self.layout().join(".writer");
        self.scoped(&path)?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options
            .open(&path)?
            .write_all(b"preview writer; no PID recovery\n")?;
        Ok(WriterLease {
            preview: self.clone(),
            path,
            active: std::sync::Mutex::new(true),
        })
    }
}
pub struct WriterLease {
    preview: std::sync::Arc<Preview>,
    path: PathBuf,
    active: std::sync::Mutex<bool>,
}
impl WriterLease {
    pub fn preview(&self) -> &std::sync::Arc<Preview> {
        &self.preview
    }
    pub fn replace_layout(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        let active = self
            .active
            .lock()
            .map_err(|_| refuse("preview writer lock poisoned"))?;
        if !*active {
            return Err(refuse("preview writer is closed"));
        }
        self.preview.replace_layout(path, bytes)
    }
    /// Stop new writes and wait for any current write before releasing ownership.
    /// Call from the host shutdown lifecycle; macOS termination may skip Drop.
    pub fn close(&self) -> io::Result<()> {
        let mut active = self
            .active
            .lock()
            .map_err(|_| refuse("preview writer lock poisoned"))?;
        if !*active {
            return Ok(());
        }
        *active = false;
        self.preview.scoped(&self.path)?;
        fs::remove_file(&self.path)
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
        p.replace_layout(&p.layout().join("missing/layout.json"), b"good")
            .unwrap();
        fs::write(p.root().join(".gitterm-protected"), "sentinel").unwrap();
        assert!(p
            .replace_layout(&p.layout().join("missing/layout.json"), b"bad")
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
        let dest = p.layout().join("copied.json");
        fs::hard_link(&sentinel, &dest).unwrap();
        p.replace_layout(&dest, b"new inode").unwrap();
        assert_eq!(fs::read(&sentinel).unwrap(), b"unchanged");
        fs::remove_file(&dest).unwrap();
        symlink(&sentinel, &dest).unwrap();
        assert!(p.replace_layout(&dest, b"bad").is_err());
        fs::rename(p.layout(), p.root().join("old-layout")).unwrap();
        symlink(&source, p.layout()).unwrap();
        assert!(p
            .replace_layout(&p.layout().join("sentinel"), b"bad")
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
    fn existing_public_directories_and_stale_owner_lease_are_refused() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, _protected, p) = fixture();
        fs::create_dir(p.root()).unwrap();
        fs::set_permissions(p.root(), fs::Permissions::from_mode(0o755)).unwrap();
        let p = std::sync::Arc::new(p);
        assert!(p.claim_writer().is_err());
        fs::set_permissions(p.root(), fs::Permissions::from_mode(0o700)).unwrap();
        p.create_private_dir(&p.layout()).unwrap();
        fs::write(p.layout().join(".writer"), "copied-pid-not-authority").unwrap();
        assert!(p.claim_writer().is_err());
    }
    #[test]
    fn all_live_authorities_closed_and_second_writer_refused() {
        let (_d, _protected, p) = fixture();
        for effect in [
            Effect::Terminal,
            Effect::Git,
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
        let first = p.claim_writer().unwrap();
        assert!(p.claim_writer().is_err());
        drop(first);
        assert!(p.claim_writer().is_ok());
    }
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
                .replace_layout(&p.layout().join("layout.json"), b"fixture")
                .unwrap();
            writer.close().unwrap();
            assert!(!p.layout().join(".writer").exists());
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
    #[test]
    fn closing_writer_denies_late_writes_and_drop_cannot_remove_next_owners_lease() {
        let (_d, _protected, p) = fixture();
        let p = std::sync::Arc::new(p);
        let first = p.claim_writer().unwrap();
        first.close().unwrap();
        let next = p.claim_writer().unwrap();
        assert!(first
            .replace_layout(&p.layout().join("late.json"), b"bad")
            .is_err());
        drop(first);
        assert!(p.claim_writer().is_err());
        next.replace_layout(&p.layout().join("current.json"), b"good")
            .unwrap();
        next.close().unwrap();
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
