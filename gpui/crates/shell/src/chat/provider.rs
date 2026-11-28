//! Fixed provider context. A reported login is evidence, never execution authority.
use anyhow::{Context, Result, ensure};
use gitterm_core::{chat::context::ProbeSpec, harness::claude::ClaudeMcpServer, preview::Preview};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

pub(crate) struct Provider {
    pub preview: Arc<Preview>,
    pub spec: ProbeSpec,
    real_home: PathBuf,
}
const HELP_LIMIT: usize = 256 * 1024;
const AUTH_LIMIT: usize = 16 * 1024;

impl Provider {
    pub fn new(preview: Arc<Preview>, executable: &Path) -> Result<Self> {
        let home = dirs::home_dir().context("resolve GitTerm process home for Claude keychain")?;
        Self::new_with_home(preview, executable, home)
    }
    fn new_with_home(preview: Arc<Preview>, executable: &Path, real_home: PathBuf) -> Result<Self> {
        let spec = ProbeSpec::claude_status(&preview, executable)?;
        preview.scoped(&preview.provider())?;
        Ok(Self {
            preview,
            spec,
            real_home,
        })
    }
    #[cfg(test)]
    pub(super) fn fixture(preview: Arc<Preview>, executable: &Path) -> Result<Self> {
        let home = preview
            .root()
            .parent()
            .expect("temporary fixture parent")
            .join("host");
        Self::new_with_home(preview, executable, home)
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
        self.prepare_keychain()?;
        Ok(())
    }
    fn prepare_keychain(&self) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            let home = PathBuf::from(&self.spec.environment()[std::ffi::OsStr::new("HOME")]);
            let library = home.join("Library");
            self.preview.create_private_dir(&library)?;
            let link = library.join("Keychains");
            let target = self.real_home.join("Library/Keychains");
            // This one leaf is intentionally outside preview scope. Never scope,
            // create directories in, chmod, or otherwise write through its target.
            match std::fs::symlink_metadata(&link) {
                Ok(meta) => ensure!(
                    meta.is_symlink()
                        && std::fs::read_link(&link).with_context(|| format!(
                            "read Claude keychain link {}",
                            link.display()
                        ))? == target,
                    "Claude keychain path already exists with a different type or target: {}",
                    link.display()
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    std::os::unix::fs::symlink(&target, &link).with_context(|| {
                        format!("create Claude keychain link {}", link.display())
                    })?;
                }
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("inspect Claude keychain path {}", link.display())
                    });
                }
            }
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
    /// `mcp_servers` are the chat's task and browser servers, their caller
    /// already fixed by the pane; none launches with no MCP server at all.
    pub fn start(
        self: Arc<Self>,
        cwd: PathBuf,
        mcp_servers: Vec<ClaudeMcpServer>,
    ) -> Result<(
        super::OwnedRuntime,
        tokio::sync::mpsc::UnboundedReceiver<gitterm_core::chat::runtime::RuntimeEvent>,
    )> {
        self.start_with_policy(
            cwd,
            mcp_servers,
            super::auth::AUTH_STATUS_SCHEMA,
            PathBuf::from("/"),
        )
    }
    fn start_with_policy(
        self: Arc<Self>,
        cwd: PathBuf,
        mcp_servers: Vec<ClaudeMcpServer>,
        schema: super::auth::Schema,
        managed_root: PathBuf,
    ) -> Result<(
        super::OwnedRuntime,
        tokio::sync::mpsc::UnboundedReceiver<gitterm_core::chat::runtime::RuntimeEvent>,
    )> {
        let provider = self.clone();
        self.start_with_preflight(cwd, mcp_servers, move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(provider.check_launch(schema, &managed_root))
        })
    }
    fn start_with_preflight(
        self: Arc<Self>,
        cwd: PathBuf,
        mcp_servers: Vec<ClaudeMcpServer>,
        preflight: impl FnOnce() -> Result<()> + Send + 'static,
    ) -> Result<(
        super::OwnedRuntime,
        tokio::sync::mpsc::UnboundedReceiver<gitterm_core::chat::runtime::RuntimeEvent>,
    )> {
        self.validate_workspace(&cwd)?;
        self.prepare()?;
        // The MCP env only adds keys: it never replaces the isolated
        // HOME/config/temp environment the probe and sign-in share.
        for (key, _) in mcp_servers.iter().flat_map(|server| &server.env) {
            ensure!(
                !self
                    .spec
                    .environment()
                    .contains_key(std::ffi::OsStr::new(key)),
                "MCP env {key} would replace the fixed Claude environment"
            );
        }
        let lease = Arc::new(super::lease::ProviderLease::claim(&self.preview)?);
        let session_lease = lease.clone();
        let (session, events) = gitterm_core::chat::runtime::Session::spawn_runtime(
            move |close_token| {
                // Runs on the session thread, never GPUI's update/render thread.
                std::thread::scope(|scope| {
                    scope
                    .spawn(preflight)
                    .join()
                    .map_err(|_| "Claude preview gate thread panicked".to_string())?
                    .map_err(|e| format!("{e:#}\nSign in once with a personal Pro/Max account in the isolated root:\n{}", self.sign_in_command()))
                })?;
                if close_token.is_cancelled() {
                    return Err("Claude preview closed before launch".into());
                }
                self.validate_workspace(&cwd)
                    .map_err(|e| format!("{e:#}"))?;
                self.prepare().map_err(|e| format!("{e:#}"))?;
                session_lease
                    .validate(&self.preview)
                    .map_err(|e| format!("{e:#}"))?;
                Ok(self.launch(cwd, session_lease, mcp_servers))
            },
        );
        Ok((
            super::OwnedRuntime {
                session,
                _lease: Some(lease),
                _conversation: None,
            },
            events,
        ))
    }
    async fn check_launch(&self, schema: super::auth::Schema, managed_root: &Path) -> Result<()> {
        super::managed::check(managed_root)?;
        // Claude Code 2.1.327 help is ~22 KiB; auth status keeps the tighter auth cap.
        let (exit, help) = self.output(&["--help"], HELP_LIMIT).await?;
        ensure!(
            exit == Some(0),
            "Claude --help failed; preview launch refused"
        );
        super::fence::check_help(std::str::from_utf8(&help).context("unparseable Claude help")?)?;
        let (exit, status) = self
            .output(&["auth", "status", "--json"], AUTH_LIMIT)
            .await?;
        super::auth::personal_auth(exit, &status, &self.preview.provider(), schema)?;
        // Recheck immediately before returning launch authority.
        super::managed::check(managed_root)
    }
    async fn output(&self, args: &[&str], limit: usize) -> Result<(Option<i32>, Vec<u8>)> {
        use tokio::io::AsyncReadExt;
        let mut child = tokio::process::Command::new(self.spec.executable())
            .args(args)
            .env_clear()
            .envs(self.spec.environment())
            .current_dir(self.spec.cwd())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("start Claude {}", args.join(" ")))?;
        let mut stdout = child
            .stdout
            .take()
            .context("Claude gate stdout missing")?
            .take(limit as u64 + 1);
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let mut bytes = Vec::new();
            stdout.read_to_end(&mut bytes).await?;
            ensure!(bytes.len() <= limit, "Claude gate output exceeds limit");
            Ok::<_, anyhow::Error>((child.wait().await?.code(), bytes))
        })
        .await;
        match result {
            Ok(Ok(output)) => Ok(output),
            failed => {
                // No output, account data or credentials are included in diagnostics.
                let _ = child.start_kill();
                child.wait().await.context("reap Claude gate process")?;
                match failed {
                    Ok(Err(error)) => Err(error),
                    _ => anyhow::bail!(
                        "Claude {} timed out; preview launch refused",
                        args.join(" ")
                    ),
                }
            }
        }
    }
    fn launch(
        &self,
        cwd: PathBuf,
        lease: Arc<super::lease::ProviderLease>,
        mcp_servers: Vec<ClaudeMcpServer>,
    ) -> gitterm_core::harness::claude::Launch {
        use gitterm_core::harness::claude::LaunchFlags;
        let flags = if mcp_servers.is_empty() {
            LaunchFlags::FreshPreview
        } else {
            LaunchFlags::PreviewTools
        };
        // The token reaches the child only here, never argv: the config
        // names it as ${VAR}, and PreviewTools refuses a literal.
        let mut environment: Vec<_> = self
            .spec
            .environment()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        environment.extend(
            mcp_servers
                .iter()
                .flat_map(|server| &server.env)
                .map(|(k, v)| (k.into(), v.into())),
        );
        gitterm_core::harness::claude::Launch {
            flags,
            config: gitterm_core::harness::claude::ClaudeSessionConfig {
                snapshot_facts_dir: None,
                cwd,
                model: None,
                permission_mode: "default".into(),
                effort: None,
                resume: None,
                wire_log_dir: None,
                mcp_servers,
                chrome: false,
                guidelines_file: None,
                guidelines_content: None,
            },
            program: self.spec.executable().into(),
            environment,
            temporary_dir: PathBuf::from(
                self.spec
                    .environment()
                    .get(std::ffi::OsStr::new("TMPDIR"))
                    .expect("fixed temp"),
            ),
            snapshots: Box::new(NoSnapshots { _lease: lease }),
        }
    }
}
/// Live deliberately bypasses every Preview preparation, account probe and lease.
/// The program and environment are composed by the same core policy as Iced.
#[cfg(test)]
pub(super) fn start_live(
    services: &gitterm_core::preview::service::ServiceContext,
    config: gitterm_core::harness::claude::ClaudeSessionConfig,
    preview_root: PathBuf,
    conversation: &str,
    instructions: LiveInstructions,
) -> Result<(
    super::OwnedRuntime,
    tokio::sync::mpsc::UnboundedReceiver<gitterm_core::chat::runtime::RuntimeEvent>,
)> {
    ensure!(
        !services.is_preview(),
        "Live chat requires production services"
    );
    validate_live_cwd(&config.cwd)?;
    let claim = Arc::new(ConversationClaim::claim(services, conversation)?);
    if let Some(id) = &config.resume {
        claim.provider_id(id)?;
    }
    start_live_claimed(services, config, preview_root, claim, instructions)
}

pub(super) fn start_live_claimed(
    services: &gitterm_core::preview::service::ServiceContext,
    config: gitterm_core::harness::claude::ClaudeSessionConfig,
    preview_root: PathBuf,
    claim: Arc<ConversationClaim>,
    instructions: LiveInstructions,
) -> Result<(
    super::OwnedRuntime,
    tokio::sync::mpsc::UnboundedReceiver<gitterm_core::chat::runtime::RuntimeEvent>,
)> {
    ensure!(
        !services.is_preview(),
        "Live chat requires production services"
    );
    validate_live_cwd(&config.cwd)?;
    if let Some(id) = &config.resume {
        claim.provider_id(id)?;
    }
    let root = services.state_root().to_path_buf();
    let (session, events) = gitterm_core::chat::runtime::Session::spawn_runtime(move |close| {
        use gitterm_core::{guidelines, harness::local_launch};
        if close.is_cancelled() {
            return Err("Claude chat closed before launch".into());
        }
        validate_live_cwd(&config.cwd).map_err(|e| format!("{e:#}"))?;
        let program = local_launch::resolve_claude_program()?;
        let mut launch = local_launch::local_launch(config, program, Box::new(LiveSnapshots));
        validate_live_environment(&launch.environment, &preview_root)
            .map_err(|e| format!("{e:#}"))?;
        let guidelines = guidelines::for_launch(
            &root,
            &launch.config.cwd,
            instructions.workspace_off,
            guidelines::InstructionFile::ClaudeMd,
        );
        let applied = guidelines.is_some();
        // Status is sent back to the pane without persisting chat facts.
        let _ = instructions.status.send(applied);
        let instructions = guidelines::launch_instructions(
            guidelines,
            instructions.places.as_ref(),
            true,
            &root.join("launch-instructions"),
        );
        launch.config.guidelines_file = instructions.map(|instructions| instructions.path);
        Ok(launch)
    });
    Ok((
        super::OwnedRuntime {
            session,
            _lease: None,
            _conversation: Some(claim),
        },
        events,
    ))
}

pub(super) struct LiveInstructions {
    pub workspace_off: bool,
    pub places: Option<gitterm_core::guidelines::DocumentPlaces>,
    pub status: std::sync::mpsc::Sender<bool>,
}

fn validate_live_cwd(cwd: &Path) -> Result<()> {
    ensure!(
        cwd.is_absolute() && cwd.is_dir(),
        "Claude workspace must be an absolute existing folder: {}",
        cwd.display()
    );
    Ok(())
}

fn validate_live_environment(
    environment: &[(std::ffi::OsString, std::ffi::OsString)],
    preview_root: &Path,
) -> Result<()> {
    use std::ffi::OsStr;
    let value = |key: &str| {
        environment
            .iter()
            .find(|(name, _)| name == OsStr::new(key))
            .map(|(_, value)| value)
    };
    let home = PathBuf::from(value("HOME").context("Live Claude requires HOME")?);
    ensure!(
        home.is_absolute() && home.is_dir(),
        "Live Claude HOME must be an absolute existing directory"
    );
    let config = value("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".claude"));
    ensure!(
        config.is_absolute() && (!config.exists() || config.is_dir()),
        "Live Claude CLAUDE_CONFIG_DIR must select an absolute directory"
    );
    let preview = resolve_provider_path(preview_root)?;
    for path in [&home, &config] {
        ensure!(
            !resolve_provider_path(path)?.starts_with(&preview),
            "Live Claude cannot use Preview provider state"
        );
    }
    Ok(())
}

/// Preserve missing tails while resolving links and rejecting dangling links.
fn resolve_provider_path(path: &Path) -> Result<PathBuf> {
    ensure!(
        path.is_absolute(),
        "Live Claude provider path must be absolute"
    );
    let mut resolved = PathBuf::new();
    for part in path.components() {
        if part == std::path::Component::ParentDir {
            resolved.pop();
            continue;
        }
        resolved.push(part);
        match std::fs::symlink_metadata(&resolved) {
            Ok(_) => resolved = std::fs::canonicalize(&resolved)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(resolved)
}

/// Retained through runtime completion, so a closing child still owns its conversation.
pub(super) struct ConversationClaim {
    keys: std::sync::Mutex<std::collections::HashSet<String>>,
}
fn conversations() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static OPEN: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    OPEN.get_or_init(Default::default)
}
impl ConversationClaim {
    pub(super) fn claim(
        services: &gitterm_core::preview::service::ServiceContext,
        conversation: &str,
    ) -> Result<Self> {
        ensure!(
            !services.is_preview(),
            "Live chat requires production services"
        );
        ensure!(
            !conversation.is_empty(),
            "Live chat requires a conversation identity"
        );
        let claim = Self {
            keys: Default::default(),
        };
        claim.insert(format!(
            "caller:{}:{conversation}",
            services.state_root().display()
        ))?;
        Ok(claim)
    }
    fn insert(&self, key: String) -> Result<()> {
        let mut owned = self.keys.lock().expect("owned conversation keys");
        if owned.contains(&key) {
            return Ok(());
        }
        ensure!(
            conversations()
                .lock()
                .expect("conversation registry")
                .insert(key.clone()),
            "Conversation is already open"
        );
        owned.insert(key);
        Ok(())
    }
    pub(super) fn acp_id(
        &self,
        agent: gitterm_core::chat::types::AcpAgent,
        id: &str,
    ) -> Result<()> {
        ensure!(!id.is_empty(), "Empty provider session identity");
        self.insert(format!("acp:{agent:?}:{id}"))
    }
    pub(super) fn provider_id(&self, id: &str) -> Result<()> {
        ensure!(!id.is_empty(), "Empty provider session identity");
        // Namespace comes from the effective provider root. Tests never inspect
        // the user's provider home when only claiming a synthetic identity.
        #[cfg(test)]
        let root = live_provider_root()?;
        #[cfg(not(test))]
        let root = resolve_provider_path(&live_provider_root()?)?;
        self.insert(format!("claude:{}:{id}", root.display()))
    }
}
impl Drop for ConversationClaim {
    fn drop(&mut self) {
        let mut registry = conversations().lock().expect("conversation registry");
        for key in self
            .keys
            .get_mut()
            .expect("owned conversation keys")
            .drain()
        {
            registry.remove(&key);
        }
    }
}

pub(super) fn live_provider_root() -> Result<PathBuf> {
    let root = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".claude")))
        .context("Resolve Claude provider directory")?;
    ensure!(
        root.is_absolute(),
        "Claude provider directory must be absolute"
    );
    Ok(root)
}

pub(super) struct LiveSnapshots;
impl gitterm_core::harness::claude::SnapshotRecorder for LiveSnapshots {
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
            std::time::Duration::from_secs(30),
            stdout.read_to_end(&mut bytes),
        )
        .await;
        if !matches!(read, Ok(Ok(_))) || bytes.len() > 16 * 1024 {
            child.kill().await?;
            child.wait().await?;
            return Ok(gitterm_core::chat::context::ProbeStatus::Unavailable);
        }
        let status = tokio::time::timeout(std::time::Duration::from_secs(30), child.wait()).await;
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
        mcp_servers: Vec<ClaudeMcpServer>,
    ) -> Result<
        Option<(
            super::OwnedRuntime,
            tokio::sync::mpsc::UnboundedReceiver<gitterm_core::chat::runtime::RuntimeEvent>,
        )>,
    > {
        self.validate_workspace(&cwd)?;
        if self.fake_probe().await? != gitterm_core::chat::context::ProbeStatus::ReportedLogin {
            return Ok(None);
        }
        let managed = self.preview.root().join("fixture-no-managed-policy");
        self.start_with_policy(cwd, mcp_servers, super::auth::FIXTURE_SCHEMA, managed)
            .map(Some)
    }
}
struct NoSnapshots {
    _lease: Arc<super::lease::ProviderLease>,
}
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

#[cfg(test)]
mod keychain_tests {
    use super::super::tests::fixture;
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

    #[cfg(target_os = "macos")]
    #[test]
    fn prepare_links_only_keychains_and_is_idempotent_without_touching_target() {
        let (dir, preview) = fixture();
        let real_home = dir.path().join("injected-host");
        let target = real_home.join("Library/Keychains");
        let provider =
            Provider::new_with_home(preview.clone(), Path::new("/fake/claude"), real_home).unwrap();
        provider.prepare().unwrap();
        let home = preview.root().join("child/home");
        let link = home.join("Library/Keychains");
        assert_eq!(std::fs::read_link(&link).unwrap(), target);
        assert!(
            !target.exists(),
            "prepare must not create or inspect target contents"
        );
        assert_eq!(
            std::fs::metadata(home.join("Library"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let before = std::fs::symlink_metadata(&link).unwrap();
        provider.prepare().unwrap();
        let after = std::fs::symlink_metadata(&link).unwrap();
        assert_eq!((before.dev(), before.ino()), (after.dev(), after.ino()));
        assert_eq!(
            provider.spec.environment()[std::ffi::OsStr::new("HOME")],
            home
        );
        assert!(!home.join(".claude").exists());
        assert!(!home.join(".config").exists());
        assert!(
            provider
                .sign_in_command()
                .contains(&format!("HOME={}", home.display()))
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn prepare_refuses_and_preserves_file_directory_and_wrong_link() {
        for kind in ["file", "directory", "wrong-link", "dangling-link"] {
            let (dir, preview) = fixture();
            let provider = Provider::fixture(preview.clone(), Path::new("/fake/claude")).unwrap();
            let library = preview.root().join("child/home/Library");
            preview.create_private_dir(&library).unwrap();
            let path = library.join("Keychains");
            let wrong = dir.path().join("wrong");
            match kind {
                "file" => std::fs::write(&path, "keep").unwrap(),
                "directory" => {
                    std::fs::create_dir(&path).unwrap();
                    std::fs::write(path.join("keep"), "keep").unwrap();
                }
                "wrong-link" => {
                    std::fs::create_dir(&wrong).unwrap();
                    std::fs::write(wrong.join("keep"), "keep").unwrap();
                    symlink(&wrong, &path).unwrap();
                }
                _ => symlink(&wrong, &path).unwrap(),
            }
            let before = std::fs::symlink_metadata(&path).unwrap();
            let error = format!("{:#}", provider.prepare().unwrap_err());
            assert!(error.contains(path.to_str().unwrap()), "{error}");
            let after = std::fs::symlink_metadata(&path).unwrap();
            assert_eq!(
                (before.dev(), before.ino(), before.mode()),
                (after.dev(), after.ino(), after.mode())
            );
            match kind {
                "file" => assert_eq!(std::fs::read_to_string(path).unwrap(), "keep"),
                "directory" => {
                    assert_eq!(std::fs::read_to_string(path.join("keep")).unwrap(), "keep")
                }
                _ => {
                    assert_eq!(std::fs::read_link(path).unwrap(), wrong);
                    if kind == "wrong-link" {
                        assert_eq!(std::fs::read_to_string(wrong.join("keep")).unwrap(), "keep");
                    }
                }
            }
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn prepare_refuses_aliased_or_non_private_library_without_writing_outside() {
        for alias in [true, false] {
            let (dir, preview) = fixture();
            let provider = Provider::fixture(preview.clone(), Path::new("/fake/claude")).unwrap();
            let home = preview.root().join("child/home");
            preview.create_private_dir(&home).unwrap();
            let outside = dir.path().join("outside");
            std::fs::create_dir(&outside).unwrap();
            let library = home.join("Library");
            if alias {
                symlink(&outside, &library).unwrap();
            } else {
                std::fs::create_dir(&library).unwrap();
                std::fs::set_permissions(&library, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            assert!(provider.prepare().is_err());
            assert!(!library.join("Keychains").exists());
            assert_eq!(std::fs::read_dir(outside).unwrap().count(), 0);
        }
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn prepare_does_not_create_library_off_macos() {
        let (_dir, preview) = fixture();
        let provider = Provider::fixture(preview.clone(), Path::new("/fake/claude")).unwrap();
        provider.prepare().unwrap();
        assert!(!preview.root().join("child/home/Library").exists());
    }
}

#[cfg(test)]
mod gate_tests {
    use super::super::{
        auth::{AUTH_STATUS_SCHEMA, FIXTURE_SCHEMA},
        tests::{fixture, script},
    };
    use super::*;
    #[test]
    fn closing_during_preflight_never_spawns_chat_and_drains_lease() {
        let (dir, preview) = fixture();
        let spawned = dir.path().join("chat-spawned");
        let fake = script(
            dir.path(),
            "fake-close-gate",
            &format!(
                "#!/bin/sh\ntouch {}; exit 97\n",
                quote(spawned.to_str().unwrap())
            ),
        );
        let cwd = dir.path().join("clone");
        super::super::tests::independent_clone(&cwd);
        let provider = Arc::new(Provider::fixture(preview.clone(), &fake).unwrap());
        let (entered, started) = std::sync::mpsc::channel();
        let (release, proceed) = std::sync::mpsc::channel();
        let (runtime, _events) = provider
            .start_with_preflight(cwd, vec![], move || {
                entered.send(()).unwrap();
                proceed.recv().unwrap();
                Ok(())
            })
            .unwrap();
        // Hold the session at the preflight boundary without racing the real
        // auth subprocess's five-second deadline or polling marker files.
        started.recv().unwrap();
        let shutdown = super::super::RuntimeShutdown::new();
        assert!(shutdown.enqueue(runtime).is_ok());
        let held_during_preflight = super::super::lease::ProviderLease::claim(&preview).is_err();
        release.send(()).unwrap();
        shutdown.drain().unwrap();
        assert!(held_during_preflight);
        assert!(
            !spawned.exists(),
            "a close during auth must not start a chat"
        );
        drop(super::super::lease::ProviderLease::claim(&preview).unwrap());
    }

    #[tokio::test]
    async fn launch_gate_requires_help_and_auth_every_time_without_spawning_chat() {
        let (dir, preview) = fixture();
        let help = "  --restricted\n  --strict-mcp-config\n  --safe-mode\n  --permission-mode <mode> (choices: \"manual\")\n";
        let marker = dir.path().join("calls");
        let good = serde_json::json!({"fixture_signed_in":true,"fixture_method":"fixture_personal","fixture_plan":"fixture_max","fixture_root":preview.provider(),"fixture_provider":"fixture_first_party"});
        for (help, status, schema, pass) in [
            (help.to_owned(), good.to_string(), FIXTURE_SCHEMA, true),
            (
                help.to_owned(),
                super::super::auth::tests::signed_in(&preview.provider()).to_string(),
                AUTH_STATUS_SCHEMA,
                true,
            ),
            (
                help.replace("--restricted", "--missing"),
                good.to_string(),
                FIXTURE_SCHEMA,
                false,
            ),
            (help.to_owned(), "{}".into(), FIXTURE_SCHEMA, false),
            // Real 2.1.327 help is ~22 KiB, over the auth cap.
            (
                format!("{help}{}", "x".repeat(24 * 1024)),
                good.to_string(),
                FIXTURE_SCHEMA,
                true,
            ),
            (
                format!("{help}{}", "x".repeat(HELP_LIMIT)),
                good.to_string(),
                FIXTURE_SCHEMA,
                false,
            ),
        ] {
            let fake = script(
                dir.path(),
                "fake-gate",
                &format!(
                    "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\ncase \"$*\" in\n --help) printf '%s' {};;\n 'auth status --json') printf '%s' {};;\n *) exit 97;;\nesac\n",
                    quote(marker.to_str().unwrap()),
                    quote(&help),
                    quote(&status)
                ),
            );
            let provider = Provider::fixture(preview.clone(), &fake).unwrap();
            provider.prepare().unwrap();
            assert_eq!(
                provider
                    .check_launch(schema, &dir.path().join("no-policy"))
                    .await
                    .is_ok(),
                pass
            );
        }
        let calls = std::fs::read_to_string(marker).unwrap();
        assert_eq!(
            calls
                .lines()
                .filter(|line| *line == "auth status --json")
                .count(),
            4
        );
        assert!(
            calls
                .lines()
                .all(|line| line == "--help" || line == "auth status --json")
        );
    }
}

#[cfg(test)]
mod live_validation_tests {
    use super::*;
    use std::ffi::OsString;
    #[test]
    fn validates_effective_home_and_config_without_overlaying_preview() {
        let dir = tempfile::tempdir_in("/private/tmp").unwrap();
        let home = dir.path().join("home");
        let preview = dir.path().join("preview");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(preview.join("provider")).unwrap();
        let env = |home: &Path, config: Option<&Path>| {
            let mut env: Vec<(OsString, OsString)> = vec![("HOME".into(), home.into())];
            env.extend(config.map(|config| ("CLAUDE_CONFIG_DIR".into(), config.into())));
            env
        };
        assert!(validate_live_environment(&env(&home, None), &preview).is_ok());
        let missing_preview = dir.path().join("not-prepared-preview");
        assert!(
            validate_live_environment(
                &env(&home, Some(&missing_preview.join("provider"))),
                &missing_preview
            )
            .is_err()
        );
        assert!(
            validate_live_environment(&env(&home, Some(&dir.path().join("custom"))), &preview)
                .is_ok()
        );
        assert!(validate_live_environment(&[], &preview).is_err());
        assert!(validate_live_environment(&env(Path::new("relative"), None), &preview).is_err());
        assert!(
            validate_live_environment(&env(&dir.path().join("absent"), None), &preview).is_err()
        );
        assert!(
            validate_live_environment(&env(&home, Some(Path::new("relative"))), &preview).is_err()
        );
        assert!(
            validate_live_environment(&env(&home, Some(&preview.join("provider"))), &preview)
                .is_err()
        );
        assert!(validate_live_environment(&env(&preview, None), &preview).is_err());
        let file = dir.path().join("file");
        std::fs::write(&file, "fixture").unwrap();
        assert!(validate_live_environment(&env(&home, Some(&file)), &preview).is_err());
        let alias = dir.path().join("alias");
        std::os::unix::fs::symlink(&preview, &alias).unwrap();
        assert!(
            validate_live_environment(&env(&home, Some(&alias.join("new-config"))), &preview)
                .is_err()
        );
    }
}
