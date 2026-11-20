//! Fixed provider context. A reported login is evidence, never execution authority.
use anyhow::{Context, Result, ensure};
use gitterm_core::{chat::context::ProbeSpec, preview::Preview};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

pub(crate) struct Provider {
    pub preview: Arc<Preview>,
    pub spec: ProbeSpec,
}
impl Provider {
    pub fn new(preview: Arc<Preview>, executable: &Path) -> Result<Self> {
        let spec = ProbeSpec::claude_status(&preview, executable)?;
        preview.scoped(&preview.provider())?;
        Ok(Self { preview, spec })
    }
    pub fn sign_in_command(&self) -> String {
        // The command uses the exact same complete environment as the probe.
        let mut command = String::from("env -i");
        for (key, value) in self.spec.environment() {
            command.push(' ');
            command.push_str(&quote(&format!(
                "{}={}",
                key.to_string_lossy(),
                value.to_string_lossy()
            )));
        }
        command.push(' ');
        command.push_str(&quote(&self.spec.executable().to_string_lossy()));
        for arg in super::fence::arguments() {
            command.push(' ');
            command.push_str(&quote(&arg));
        }
        format!(
            "cd {} && {command}\nThen /login",
            quote(&self.spec.cwd().to_string_lossy())
        )
    }
    pub fn prepare(&self) -> Result<()> {
        for path in std::iter::once(self.preview.provider())
            .chain(
                ["HOME", "XDG_CONFIG_HOME", "TMPDIR"]
                    .into_iter()
                    .map(|key| {
                        PathBuf::from(
                            self.spec
                                .environment()
                                .get(std::ffi::OsStr::new(key))
                                .expect("fixed probe environment"),
                        )
                    }),
            )
            .chain(std::iter::once(self.spec.cwd().to_path_buf()))
        {
            self.preview.create_private_dir(&path)?;
        }
        Ok(())
    }
    pub fn validate_workspace(&self, cwd: &Path) -> Result<()> {
        ensure!(
            cwd.is_absolute() && cwd.is_dir(),
            "Claude workspace must be an absolute existing folder: {}",
            cwd.display()
        );
        ensure!(
            self.preview.protected().check(cwd)? == cwd,
            "Claude workspace is aliased: {}",
            cwd.display()
        );
        ensure!(
            !cwd.starts_with(self.preview.root()) && !self.preview.root().starts_with(cwd),
            "Claude clone must not overlap preview app/provider state"
        );
        let git = gitterm_core::preview::inspect_git(cwd, self.preview.protected())?
            .context("Claude preview requires an independent clone, not a plain folder")?;
        let dotgit = cwd.join(".git");
        ensure!(
            git.checkout == cwd
                && std::fs::symlink_metadata(&dotgit)?.is_dir()
                && git.administrative == dotgit
                && git.common == dotgit,
            "Claude preview requires the root of an independent clone with local .git metadata"
        );
        for (path, directory) in [
            ("config", false),
            ("HEAD", false),
            ("objects", true),
            ("refs", true),
        ] {
            let path = dotgit.join(path);
            let meta = std::fs::symlink_metadata(&path)
                .with_context(|| format!("independent clone metadata {}", path.display()))?;
            ensure!(
                if directory {
                    meta.is_dir()
                } else {
                    meta.is_file()
                },
                "missing or aliased Git metadata: {}",
                path.display()
            );
        }
        match std::fs::symlink_metadata(dotgit.join("objects/info")) {
            Ok(meta) => ensure!(meta.is_dir(), "aliased Git objects/info"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        // Validate parent directories before inspecting redirects so discovery
        // never follows an objects/info symlink into another checkout.
        for redirect in [
            "commondir",
            "worktrees",
            "objects/info/alternates",
            "objects/info/http-alternates",
        ] {
            match std::fs::symlink_metadata(dotgit.join(redirect)) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
                Ok(_) => anyhow::bail!("independent clone cannot contain .git/{redirect}"),
            }
        }
        self.preview.scoped(&self.preview.provider())?;
        Ok(())
    }
    pub fn authorize(&self) -> Result<()> {
        // C2's process-tree/settings isolation prerequisite is still unresolved.
        // Keep this separate from auth: even ReportedLogin cannot open the gate.
        crate::preview::executable(
            Some(&self.preview),
            gitterm_core::preview::Effect::Agent,
            &self.preview.provider(),
        )
        .with_context(|| format!(
            "Claude preview activation awaits a reviewed launch API and managed-hook confinement; required fence: {}",
            super::fence::arguments().join(" ")
        ))
    }
}
pub(super) fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
impl Provider {
    /// Test-only execution seam. Production cannot substitute an executable.
    pub async fn fake_probe(&self) -> Result<gitterm_core::chat::context::ProbeStatus> {
        use tokio::io::AsyncReadExt;
        self.prepare()?;
        let mut child = tokio::process::Command::new(self.spec.executable())
            .args(self.spec.args())
            .env_clear()
            .envs(self.spec.environment())
            .current_dir(self.spec.cwd())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let mut stdout = child
            .stdout
            .take()
            .context("probe stdout missing")?
            .take(16 * 1024 + 1);
        let mut bytes = Vec::new();
        let read = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            stdout.read_to_end(&mut bytes),
        )
        .await;
        if !matches!(read, Ok(Ok(_))) || bytes.len() > 16 * 1024 {
            child.kill().await?;
            child.wait().await?;
            return Ok(gitterm_core::chat::context::ProbeStatus::Unavailable);
        }
        let status = tokio::time::timeout(std::time::Duration::from_secs(2), child.wait()).await;
        match status {
            Ok(status) => Ok(self.spec.parse(status?.code(), false, &bytes)),
            Err(_) => {
                child.kill().await?;
                child.wait().await?;
                Ok(gitterm_core::chat::context::ProbeStatus::Unavailable)
            }
        }
    }
}

#[cfg(test)]
impl Provider {
    pub async fn fake_start(
        self: Arc<Self>,
        cwd: PathBuf,
    ) -> Result<
        Option<(
            gitterm_core::chat::runtime::Session,
            tokio::sync::mpsc::UnboundedReceiver<gitterm_core::chat::types::HarnessEvent>,
        )>,
    > {
        self.validate_workspace(&cwd)?;
        if self.fake_probe().await? != gitterm_core::chat::context::ProbeStatus::ReportedLogin {
            return Ok(None);
        }
        let (session, events) = gitterm_core::chat::runtime::Session::spawn(move |_| {
            self.validate_workspace(&cwd)
                .map_err(|e| format!("{e:#}"))?;
            self.prepare().map_err(|e| format!("{e:#}"))?;
            Ok(gitterm_core::harness::claude::Launch {
                config: gitterm_core::harness::claude::ClaudeSessionConfig {
                    snapshot_facts_dir: None,
                    cwd,
                    model: None,
                    permission_mode: "default".into(),
                    effort: None,
                    resume: None,
                    wire_log_dir: None,
                    mcp_servers: vec![],
                    chrome: false,
                    guidelines_file: None,
                    guidelines_content: None,
                },
                program: self.spec.executable().into(),
                environment: self
                    .spec
                    .environment()
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
                temporary_dir: PathBuf::from(
                    self.spec
                        .environment()
                        .get(std::ffi::OsStr::new("TMPDIR"))
                        .expect("fixed temp"),
                ),
                snapshots: Box::new(NoSnapshots),
            })
        });
        Ok(Some((session, events)))
    }
}
#[cfg(test)]
struct NoSnapshots;
#[cfg(test)]
impl gitterm_core::harness::claude::SnapshotRecorder for NoSnapshots {
    fn observe(&mut self, _: &gitterm_core::chat::types::HarnessEvent) {}
    fn invalidate_end(&mut self) {}
    fn discard_turn(&mut self) {}
    fn bind<'a>(&'a mut self, _: &'a str) -> gitterm_core::harness::claude::SnapshotFuture<'a> {
        Box::pin(std::future::ready(()))
    }
    fn begin(&mut self) -> gitterm_core::harness::claude::SnapshotFuture<'_> {
        Box::pin(std::future::ready(()))
    }
    fn capture_end(&mut self) -> gitterm_core::harness::claude::SnapshotFuture<'_> {
        Box::pin(std::future::ready(()))
    }
    fn finish(&mut self) -> gitterm_core::harness::claude::SnapshotFuture<'_> {
        Box::pin(std::future::ready(()))
    }
    fn close(&mut self) -> gitterm_core::harness::claude::SnapshotFuture<'_> {
        Box::pin(std::future::ready(()))
    }
}
