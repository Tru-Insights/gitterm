//! What the shared task host's services may do, per operation and effect.
//!
//! A [`ServiceContext`] is built once at composition and carries the state
//! root, the task MCP namespace, the browser profile directory and the child
//! environment rule. Hosts ask it before dispatching a task MCP operation and
//! again before executing each reducer effect or runner, because a denied
//! dispatch alone does not stop effects that start from restored or imported
//! state (queue promotion, automatic delivery, restore).
//!
//! The mode is fixed by the constructor. Nothing here reads an environment
//! variable to choose it, so no inherited value can widen a preview context.
use super::{refuse, Effect as ProcessEffect, Preview};
use crate::config_paths::CONFIG_DIR_ENV;
use crate::session;
use crate::task_commands::TaskMutationAction;
use std::{
    ffi::{OsStr, OsString},
    fmt, fs, io,
    path::{Path, PathBuf},
    sync::Arc,
};

/// Every inherited variable with this prefix is GitTerm service or identity
/// state (task/browser MCP URL and token, agent address and token, config
/// dir, instance id, Chrome path, worker pid file) and never reaches a child.
pub const SCRUBBED_ENV_PREFIX: &str = "GITTERM_";

/// A disposable fixture root carries this regular file.
pub const FIXTURE_MARKER: &str = ".gitterm-fixture-root";
/// Provider and browser-control homes a preview or fixture child must not
/// share with production: they are removed and the provider homes are
/// pinned under the preview state root.
pub const PROVIDER_HOME_ENV: [(&str, &str); 2] = [
    ("CODEX_HOME", "provider-homes/codex"),
    ("CLAUDE_CONFIG_DIR", "provider-homes/claude"),
];
pub const SCRUBBED_PREVIEW_ENV_PREFIXES: [&str; 1] = ["AGENT_BROWSER_"];

/// The same wording the preview UI uses for a disabled action.
pub fn preview_notice(label: &str) -> String {
    format!("{label} · not available in preview")
}

/// What an operation or effect does, which decides whether a context allows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// Stage 1–3 terminal and Git authority, unchanged by this gate.
    Existing,
    /// Reads state or replies; writes nothing durable and starts nothing.
    Read,
    /// Writes task, delegation, worktree or merge-plan state.
    Write,
    /// Starts, stops, signals or delivers input to a harness or helper process.
    Process,
    /// Acts outside any fixture target: launches Chrome, writes browser
    /// scratch/config, restores imported sessions or publishes to a remote.
    Outside,
}

/// One classified operation or effect: the label for its notice, and its access.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Class {
    pub label: &'static str,
    pub access: Access,
}

const fn class(label: &'static str, access: Access) -> Class {
    Class { label, access }
}

/// A refusal carrying the user-facing notice.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Denied {
    notice: String,
}

impl Denied {
    pub fn notice(&self) -> &str {
        &self.notice
    }
}

impl fmt::Display for Denied {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.notice)
    }
}

impl std::error::Error for Denied {}

/// Runner boundaries that no operation or reducer effect carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunnerEffect {
    /// The injected `html_preview` runner, which launches headless Chrome.
    HtmlPreview,
    /// The task MCP `/notify` ingress, before it dispatches a session event.
    NotifyIngress,
    /// Restarting, reattaching or re-arming deadlines of a restored or
    /// imported session.
    RestoreSession,
    /// Starting a queued task session or delegation once capacity frees.
    QueuePromotion,
    /// Delivering a finished result or objective to a chat without a user action.
    AutomaticDelivery,
}

/// The browser crate maps each of its tools to one of these in a table
/// (`BROWSER_TOOL_ACCESS`, tested to cover every routed tool once; a name
/// outside it counts as Interact), and passes the tool name as the label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrowserAccess {
    /// Status, targets, snapshots, DOM/node inspection, console, network,
    /// diagnostics, waits, captures and diffs returned as content.
    Read,
    /// Opening Chrome or a target.
    Launch,
    /// Navigating, clicking, typing, pressing, scrolling, resizing, setting
    /// the viewport, reloading, focusing/closing targets, disconnecting.
    Interact,
    /// agent-browser scratch files, engine config or network capture on disk.
    Scratch,
}

pub fn classify_runner(effect: RunnerEffect) -> Class {
    match effect {
        RunnerEffect::HtmlPreview => class("html_preview", Access::Outside),
        RunnerEffect::NotifyIngress => class("notify", Access::Write),
        RunnerEffect::RestoreSession => class("Restore session", Access::Outside),
        RunnerEffect::QueuePromotion => class("Start queued session", Access::Process),
        RunnerEffect::AutomaticDelivery => class("Automatic delivery", Access::Process),
    }
}

pub fn classify_browser(access: BrowserAccess) -> Access {
    match access {
        BrowserAccess::Read => Access::Read,
        BrowserAccess::Launch | BrowserAccess::Interact | BrowserAccess::Scratch => Access::Outside,
    }
}

/// The preview's existing process effects keep their stage 1–3 meaning.
pub fn classify_process(effect: ProcessEffect) -> Class {
    match effect {
        ProcessEffect::Terminal => class("Terminal", Access::Existing),
        ProcessEffect::Git => class("Git", Access::Existing),
        ProcessEffect::Snapshot => class("Snapshot", Access::Write),
        ProcessEffect::MutateHistory => class("Edit history", Access::Write),
        ProcessEffect::Agent => class("Agent", Access::Process),
        ProcessEffect::Worker => class("Worker", Access::Process),
        ProcessEffect::Helper => class("Helper", Access::Process),
        ProcessEffect::Resume => class("Resume session", Access::Outside),
        ProcessEffect::RestoreProcess => class("Restore process", Access::Outside),
    }
}

/// A Work PR action's `gh` run (`work::pr_actions::RunPrAction`) writes to
/// GitHub, outside any fixture target. The label is the confirmation's verb.
pub fn classify_pr_action(action: crate::prs::PrAction) -> Class {
    use crate::prs::PrAction as A;
    match action {
        A::Approve => class("Approve", Access::Outside),
        A::Merge(_) => class("Merge", Access::Outside),
        A::Ready => class("Ready for review", Access::Outside),
    }
}

/// A phone panel call to a remote agent (`remote::phone_panel::PhoneCall`).
/// Create and revoke grant or withdraw a phone's access in the agent's
/// registry on another machine, which no fixture root can contain, so they
/// are Outside rather than Write (local state) or Process (local children).
/// List reads that registry with the desktop token and writes nothing.
pub fn classify_phone_request(request: &crate::remote::phone_panel::PhoneRequest) -> Class {
    use crate::remote::phone_panel::{PhoneRequest as R, CREATE_LABEL};
    match request {
        R::List => class("Paired phones", Access::Read),
        R::Create { .. } => class(CREATE_LABEL, Access::Outside),
        R::Revoke { .. } => class("Revoke", Access::Outside),
    }
}

/// Labels are the MCP tool names, so the notice reads "<tool> · not available in preview".
#[cfg(feature = "runtime")]
pub fn classify_operation(operation: &crate::task_mcp::TaskControlOperation) -> Class {
    use crate::task_mcp::TaskControlOperation as Op;
    match operation {
        Op::PlanLimits => class("plan_limits", Access::Read),
        // Content goes to the caller's chat; the host keeps it unsaved in preview.
        Op::HtmlRender(_) => class("html_render", Access::Read),
        Op::List(_) => class("task_list", Access::Read),
        Op::Get(_) => class("task_get", Access::Read),
        Op::GetDelegation(_) => class("delegation_get", Access::Read),
        Op::ListDelegations(_) => class("delegation_list", Access::Read),
        Op::MergePlanGet => class("merge_plan_get", Access::Read),
        Op::Create(_) => class("task_create", Access::Write),
        Op::UpdateHandoff(_) => class("task_update_handoff", Access::Write),
        // Every notify type, not only turn completion: the ingress is closed.
        Op::SessionEvent(_) => class("notify", Access::Write),
        Op::MergePlanSet(_) => class("merge_plan_set", Access::Write),
        Op::LaunchSession(_) => class("task_launch_session", Access::Process),
        Op::RequestReview(_) => class("review_request", Access::Process),
        Op::RequestConsult(_) => class("consult_request", Access::Process),
        Op::DelegateTask(_) => class("delegate_task", Access::Process),
    }
}

#[cfg(feature = "runtime")]
pub fn classify_delegation_effect(effect: &crate::delegation_state::Effect) -> Class {
    use crate::delegation_state::Effect as E;
    match effect {
        E::Check { .. } => class("Check delegation checkout", Access::Read),
        E::Refresh(_) => class("Refresh delegation", Access::Read),
        E::Rejected(_) => class("Reject delegation", Access::Read),
        E::AlreadyDelivered => class("Delivery", Access::Read),
        E::Insert { .. } => class("Save delegation", Access::Write),
        E::StartRejected { .. } => class("Save delegation", Access::Write),
        E::RecordStarted { .. } => class("Save delegation", Access::Write),
        E::RecordCompletion(_) => class("Save delegation", Access::Write),
        E::PersistDismiss { .. } => class("Dismiss delegation", Access::Write),
        E::RecordDelivery(_) => class("Save delivery", Access::Write),
        // Only queue admission emits CheckStart: it is the promotion step.
        E::CheckStart { .. } => class("Start queued delegation", Access::Process),
        E::Start(_) => class("Start delegation", Access::Process),
        E::Abort(_) => class("Stop delegation", Access::Process),
        E::Flush(_) => class("Deliver results", Access::Process),
        E::Deliver { .. } => class("Deliver results", Access::Process),
    }
}

pub fn classify_task_action(action: &TaskMutationAction) -> Class {
    use TaskMutationAction as A;
    match action {
        A::None => class("Task", Access::Read),
        // Read-only Git: no command writes or takes optional locks
        // (`task_worktree::inspect_cleanup`, test `inspection_spawns_only_read_git`).
        A::InspectCleanup { .. } => class("Inspect worktree", Access::Read),
        A::ConfirmPublish { .. } => class("Publish", Access::Read),
        A::WorkerAuthorityChecked { .. } => class("Check worker", Access::Read),
        A::PrepareTask(_) => class("Create task", Access::Write),
        A::PersistPreparing { .. } => class("Save task", Access::Write),
        A::PersistPreparation { .. } => class("Save task", Access::Write),
        A::PersistDelivery { .. } => class("Save delivery", Access::Write),
        A::PersistHandoff { .. } => class("Save handoff", Access::Write),
        A::ArchiveTask { .. } => class("Close task", Access::Write),
        A::RemoveWorktree { .. } => class("Remove worktree", Access::Write),
        A::ClearWorktree { .. } => class("Remove worktree", Access::Write),
        A::QueueSession { .. } => class("Queue session", Access::Write),
        A::PersistWorkerPreparation { .. } => class("Save task", Access::Write),
        A::PersistWorkerPreparationFailure { .. } => class("Save task", Access::Write),
        A::PersistWorkerCheckoutReuse { .. } => class("Save task", Access::Write),
        A::Session(effect) => classify_session_effect(effect),
        A::LaunchSession { .. } => class("Launch session", Access::Process),
        A::StartWorker { .. } => class("Start worker", Access::Process),
        A::Publish { .. } => class("Publish", Access::Outside),
    }
}

pub fn classify_session_effect(effect: &session::Effect) -> Class {
    use session::Effect as S;
    match effect {
        S::None => class("Session", Access::Read),
        S::ConfirmClose(_) => class("Close session", Access::Read),
        S::Close(_) => class("Close session", Access::Process),
        S::InterruptHarness(_) => class("Stop turn", Access::Process),
        S::CancelTaskTurn(_) => class("Stop turn", Access::Process),
    }
}

/// The task MCP listener identity and bearer secret this context's server uses.
#[derive(Clone)]
pub struct TaskMcpNamespace {
    instance_id: String,
    token: String,
}

impl TaskMcpNamespace {
    fn fresh(instance_id: String) -> Self {
        let token = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        Self { instance_id, token }
    }
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }
    pub fn token(&self) -> &str {
        &self.token
    }
}

impl fmt::Debug for TaskMcpNamespace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaskMcpNamespace")
            .field("instance_id", &self.instance_id)
            .field("token", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, Debug)]
enum Mode {
    Preview(Arc<Preview>),
    FixtureLaunch {
        preview: Arc<Preview>,
        fixtures: Vec<PathBuf>,
    },
    Production,
}

/// Composition-time authority for the shared task host's services. A clone
/// decides the same way, for a check that runs off the host's thread.
#[derive(Clone, Debug)]
pub struct ServiceContext {
    mode: Mode,
    gpui_live: bool,
    root: PathBuf,
    task_mcp: TaskMcpNamespace,
}

impl ServiceContext {
    /// Normal preview: reads are allowed; every write, launch and outside
    /// effect is denied. The root is the already validated preview root.
    pub fn preview(preview: Arc<Preview>) -> io::Result<Self> {
        let root = preview.scoped(preview.root())?;
        Ok(Self {
            mode: Mode::Preview(preview),
            gpui_live: false,
            root,
            task_mcp: TaskMcpNamespace::fresh(preview_instance_id()),
        })
    }

    /// Test/acceptance only: also allows writes and process effects whose
    /// resolved target lies in one of `fixtures`. Each fixture must resolve
    /// to a directory under the system temp dir, outside every protected root,
    /// holding a regular [`FIXTURE_MARKER`] file. Normal preview startup never
    /// calls this; a [`ServiceContext::preview`] value is never widened by it.
    pub fn fixture_launch(
        preview: Arc<Preview>,
        fixtures: impl IntoIterator<Item = PathBuf>,
    ) -> io::Result<Self> {
        let temp = fs::canonicalize(std::env::temp_dir())?;
        let fixtures = fixtures
            .into_iter()
            .map(|path| fixture_root(&preview, &temp, &path))
            .collect::<io::Result<Vec<_>>>()?;
        if fixtures.is_empty() {
            return Err(refuse("fixture launch needs at least one fixture root"));
        }
        let root = preview.scoped(preview.root())?;
        Ok(Self {
            mode: Mode::FixtureLaunch { preview, fixtures },
            gpui_live: false,
            root,
            task_mcp: TaskMcpNamespace::fresh(preview_instance_id()),
        })
    }

    /// The real V6 root with every operation and effect allowed.
    /// Deliberately no protected-root check. Iced's task host gate uses it
    /// today (its MCP server keeps its own namespace); GPUI only at cutover,
    /// when Iced no longer runs.
    pub fn production(root: &Path, instance_id: impl Into<String>) -> io::Result<Self> {
        // Only absoluteness: Iced's config accepts an absolute override that
        // contains `..`, and production must start wherever Iced does.
        if !root.is_absolute() {
            return Err(refuse(format!(
                "production root must be absolute: {}",
                root.display()
            )));
        }
        Ok(Self {
            mode: Mode::Production,
            gpui_live: false,
            root: root.to_path_buf(),
            task_mcp: TaskMcpNamespace::fresh(instance_id.into()),
        })
    }

    /// Live admits task/merge-plan writes, guarded PR actions, task execution
    /// and result delivery. General restore and
    /// notify ingress stay closed; current workers have scoped notify admission.
    pub fn for_gpui_live(mut self) -> Self {
        self.gpui_live = true;
        self
    }
    /// Browser setup must not open a default target or initialize an external
    /// engine in Preview, fixtures, or this restricted Live slice.
    pub fn browser_read_only(&self) -> bool {
        self.gpui_live || self.is_preview()
    }

    pub fn is_preview(&self) -> bool {
        !matches!(self.mode, Mode::Production)
    }
    pub fn state_root(&self) -> &Path {
        &self.root
    }
    pub fn task_mcp(&self) -> &TaskMcpNamespace {
        &self.task_mcp
    }
    /// The same relative name as production's `browser-profile`, so cutover
    /// changes only the root. In preview it is checked to stay inside it.
    pub fn browser_profile_dir(&self) -> io::Result<PathBuf> {
        let dir = self.root.join("browser-profile");
        match &self.mode {
            Mode::Preview(preview) | Mode::FixtureLaunch { preview, .. } => preview.scoped(&dir),
            Mode::Production => Ok(dir),
        }
    }

    /// The environment for a child: every inherited `GITTERM_*` variable is
    /// removed and the V6 config dir is pinned to this root, because legacy
    /// config discovery falls back to the real home when it is absent. The
    /// host adds its own MCP URL/token afterwards. Spawn with `env_clear()`
    /// and this list, so nothing else is inherited behind it.
    ///
    /// In preview the root is re-scoped first: a root that has moved, or was
    /// replaced by a link since composition, is refused.
    pub fn child_env(
        &self,
        inherited: impl IntoIterator<Item = (OsString, OsString)>,
    ) -> io::Result<Vec<(OsString, OsString)>> {
        let preview = match &self.mode {
            Mode::Preview(preview) | Mode::FixtureLaunch { preview, .. } => Some(preview),
            Mode::Production => None,
        };
        if let Some(preview) = preview {
            let root = preview.scoped(&self.root)?;
            if !fs::symlink_metadata(&root)?.is_dir() {
                return Err(refuse(format!(
                    "preview root is no longer a directory: {}",
                    root.display()
                )));
            }
        }
        let preview = preview.is_some();
        let mut env: Vec<_> = inherited
            .into_iter()
            .filter(|(name, _)| !(is_scrubbed(name) || (preview && is_preview_scrubbed(name))))
            .collect();
        env.push((CONFIG_DIR_ENV.into(), self.root.clone().into_os_string()));
        if preview {
            // shortcut: HOME itself is still inherited (shells and git need it);
            // tools that write under ~ outside these homes are not isolated yet.
            // Revisit when H5b wires real preview children.
            for (name, dir) in PROVIDER_HOME_ENV {
                env.push((name.into(), self.root.join(dir).into_os_string()));
            }
        }
        Ok(env)
    }

    pub fn authorize(&self, class: Class, target: Option<&Path>) -> Result<(), Denied> {
        self.decide(class.label, class.access, target)
    }

    /// Check before dispatch. `target` is the checkout the operation acts in,
    /// when the host knows it; only a fixture-launch context uses it.
    #[cfg(feature = "runtime")]
    pub fn operation(
        &self,
        operation: &crate::task_mcp::TaskControlOperation,
        target: Option<&Path>,
    ) -> Result<(), Denied> {
        self.authorize(classify_operation(operation), target)
    }

    #[cfg(feature = "runtime")]
    pub fn delegation_effect(
        &self,
        effect: &crate::delegation_state::Effect,
        target: Option<&Path>,
    ) -> Result<(), Denied> {
        self.authorize(classify_delegation_effect(effect), target)
    }

    pub fn task_action(
        &self,
        action: &TaskMutationAction,
        target: Option<&Path>,
    ) -> Result<(), Denied> {
        self.authorize(classify_task_action(action), target)
    }

    pub fn process(&self, effect: ProcessEffect, target: Option<&Path>) -> Result<(), Denied> {
        self.authorize(classify_process(effect), target)
    }

    pub fn runner(&self, effect: RunnerEffect, target: Option<&Path>) -> Result<(), Denied> {
        self.authorize(classify_runner(effect), target)
    }

    pub fn pr_action(&self, run: &crate::work::pr_actions::RunPrAction) -> Result<(), Denied> {
        self.authorize(classify_pr_action(run.action), None)
    }

    pub fn phone<C>(&self, call: &crate::remote::phone_panel::PhoneCall<C>) -> Result<(), Denied> {
        self.authorize(classify_phone_request(&call.request), None)
    }

    /// `tool` is the browser MCP tool name, used as the notice label.
    pub fn browser(&self, tool: &str, access: BrowserAccess) -> Result<(), Denied> {
        self.decide(tool, classify_browser(access), None)
    }

    /// Authorizes `class` for every target, keeping what each resolved to so
    /// [`Self::recheck`] can refuse a path swapped after async preparation.
    /// No target means the decision without one (denied for a fixture write).
    pub fn authorize_targets(
        &self,
        class: Class,
        targets: &[PathBuf],
    ) -> Result<AuthorizedTargets, Denied> {
        if targets.is_empty() {
            self.authorize(class, None)?;
        }
        let resolved = targets
            .iter()
            .map(|target| {
                self.authorize(class, Some(target))?;
                let actual = match &self.mode {
                    Mode::Production => target.clone(),
                    Mode::Preview(preview) | Mode::FixtureLaunch { preview, .. } => preview
                        .protected()
                        .check(target)
                        .map_err(|_| denied(class.label))?,
                };
                Ok((target.clone(), actual))
            })
            .collect::<Result<_, Denied>>()?;
        Ok(AuthorizedTargets { class, resolved })
    }

    /// Re-authorizes targets after async preparation: each must still pass
    /// and still resolve to the path it resolved to when it was authorized.
    pub fn recheck(&self, authorized: &AuthorizedTargets) -> Result<(), Denied> {
        let class = authorized.class;
        if authorized.resolved.is_empty() {
            return self.authorize(class, None);
        }
        for (target, actual) in &authorized.resolved {
            self.authorize(class, Some(target))?;
            if let Mode::Preview(preview) | Mode::FixtureLaunch { preview, .. } = &self.mode {
                if preview.protected().check(target).ok().as_ref() != Some(actual) {
                    return Err(denied(class.label));
                }
            }
        }
        Ok(())
    }

    /// Fixture launch only, right before a child starts in `authorized`'s
    /// targets with `child_env` as its whole environment: Git, run in that
    /// environment, names the work tree, Git dir and common dir each target
    /// uses, and each must resolve inside the target's fixture. Git config
    /// can name a work tree elsewhere (`core.worktree`, also from a
    /// `config.worktree` under `extensions.worktreeConfig`), which
    /// [`Self::recheck`] cannot see: it checks the admin dirs only. Call it
    /// after `recheck`. A Git failure is a refusal, except for a target in
    /// no repository at all (nothing between it and its fixture root holds
    /// a `.git` or is a Git dir), where no config can name a work tree.
    ///
    /// Preview and production pass unchanged: preview never authorizes a
    /// launch, and production checks nothing.
    pub fn recheck_git_work_trees(
        &self,
        authorized: &AuthorizedTargets,
        child_env: &[(OsString, OsString)],
    ) -> Result<(), Denied> {
        let Mode::FixtureLaunch { preview, fixtures } = &self.mode else {
            return Ok(());
        };
        let label = authorized.class.label;
        for (_, actual) in &authorized.resolved {
            let fixture = fixtures
                .iter()
                .find(|root| actual.starts_with(root))
                .ok_or_else(|| denied(label))?;
            check_effective_git(preview, fixture, actual, child_env).map_err(|_| denied(label))?;
        }
        Ok(())
    }

    fn decide(&self, label: &str, access: Access, target: Option<&Path>) -> Result<(), Denied> {
        let live_task_write = access == Access::Write
            && matches!(
                label,
                "task_create"
                    | "task_update_handoff"
                    | "delegate_task"
                    | "Save delegation"
                    | "Queue session"
                    | "Worker notify"
                    | "merge_plan_set"
                    | "Dismiss delegation"
                    | "Save delivery"
            );
        let live_task_process = access == Access::Process
            && matches!(
                label,
                "task_launch_session"
                    | "review_request"
                    | "consult_request"
                    | "delegate_task"
                    | "Launch session"
                    | "Start worker"
                    | "Start queued session"
                    | "Start queued delegation"
                    | "Start delegation"
                    | "Stop delegation"
                    | "Automatic delivery"
                    | "Deliver results"
            );
        let live_pr_action =
            access == Access::Outside && matches!(label, "Approve" | "Merge" | "Ready for review");
        if self.gpui_live
            && !matches!(access, Access::Read | Access::Existing)
            && !live_task_write
            && !live_task_process
            && !live_pr_action
        {
            return Err(denied(label));
        }
        let allowed = match (&self.mode, access) {
            (Mode::Production, _) | (_, Access::Existing | Access::Read) => true,
            (Mode::Preview(_), _) | (Mode::FixtureLaunch { .. }, Access::Outside) => false,
            (Mode::FixtureLaunch { preview, fixtures }, Access::Write | Access::Process) => {
                target.is_some_and(|target| fixture_target(preview, fixtures, target).is_ok())
            }
        };
        if allowed {
            Ok(())
        } else {
            Err(denied(label))
        }
    }
}

/// Targets one decision authorized, with the path each resolved to then.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorizedTargets {
    class: Class,
    resolved: Vec<(PathBuf, PathBuf)>,
}

fn denied(label: &str) -> Denied {
    Denied {
        notice: preview_notice(label),
    }
}

fn is_preview_scrubbed(name: &OsStr) -> bool {
    name.to_str().is_none_or(|name| {
        PROVIDER_HOME_ENV.iter().any(|(env, _)| name == *env)
            || SCRUBBED_PREVIEW_ENV_PREFIXES
                .iter()
                .any(|prefix| name.starts_with(prefix))
    })
}

pub fn is_scrubbed(name: &OsStr) -> bool {
    name.to_str()
        .is_none_or(|name| name.starts_with(SCRUBBED_ENV_PREFIX))
}

fn preview_instance_id() -> String {
    format!("gpui-preview-{}", uuid::Uuid::new_v4().simple())
}

/// The resolved path decides, so a link inside the temp dir to a real
/// checkout is refused. The fixture's own Git admin dirs (a `.git` file's
/// `gitdir:` and its `commondir`) must resolve inside it too.
fn fixture_root(preview: &Preview, temp: &Path, path: &Path) -> io::Result<PathBuf> {
    let actual = preview.protected().check(path)?;
    if actual == temp || !actual.starts_with(temp) || !fs::symlink_metadata(&actual)?.is_dir() {
        return Err(refuse(format!(
            "fixture root is not a directory under the temp dir: {}",
            path.display()
        )));
    }
    if !fs::symlink_metadata(actual.join(FIXTURE_MARKER))?.is_file() {
        return Err(refuse(format!(
            "fixture marker is not a regular file: {}",
            path.display()
        )));
    }
    check_git_admin(preview, &actual, &actual)?;
    Ok(actual)
}

/// A fixture write or launch target: it resolves inside one fixture, and so
/// does the Git admin dir (and its `commondir`) of every checkout between
/// it and that fixture's root, because Git writes there too.
fn fixture_target(preview: &Preview, fixtures: &[PathBuf], target: &Path) -> io::Result<PathBuf> {
    let actual = preview.protected().check(target)?;
    let fixture = fixtures
        .iter()
        .find(|root| actual.starts_with(root))
        .ok_or_else(|| refuse(format!("not in a fixture: {}", target.display())))?;
    for dir in actual
        .ancestors()
        .take_while(|dir| dir.starts_with(fixture))
    {
        check_git_admin(preview, fixture, dir)?;
    }
    Ok(actual)
}

/// What a child started in `checkout` with `env` (its whole environment)
/// works on, as Git itself resolves it: the work tree, Git dir and common
/// dir from `rev-parse` (which only reads, and fetches nothing), with no
/// fsmonitor hook and no transport allowed. Each must resolve inside
/// `fixture` and outside every protected root, as [`fixture_root`] checks.
fn check_effective_git(
    preview: &Preview,
    fixture: &Path,
    checkout: &Path,
    env: &[(OsString, OsString)],
) -> io::Result<()> {
    // Started in `checkout`, as the child is, so a relative `PATH` entry
    // finds the same `git` the child would.
    let rev_parse = |args: &[&str]| {
        std::process::Command::new("git")
            .current_dir(checkout)
            .env_clear()
            .envs(env.iter().map(|(name, value)| (name, value)))
            .args(["-c", "core.fsmonitor=false", "-c", "protocol.allow=never"])
            .arg("-C")
            .arg(checkout)
            .arg("rev-parse")
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
    };
    let output = rev_parse(&["--show-toplevel", "--absolute-git-dir", "--git-common-dir"])?;
    let failed = |why: &str| {
        refuse(format!(
            "git cannot name the work tree of {}: {why}",
            checkout.display()
        ))
    };
    if !output.status.success() {
        // A repository Git finds without a work tree (a bare one, possibly
        // above this fixture when the child's ceiling is another fixture's
        // parent) is not one a fixture run may use.
        if rev_parse(&["--absolute-git-dir"])?.status.success() {
            return Err(failed("a repository with no work tree"));
        }
        // With no repository found, nothing Git's discovery reaches (every
        // folder up to the child's ceiling, past the fixture root when the
        // ceiling is another fixture's parent) may hold a `.git` or be a Git
        // dir, so the failure is not a repository Git could not open.
        let ceilings: Vec<PathBuf> = env
            .iter()
            .filter(|(name, _)| name == "GIT_CEILING_DIRECTORIES")
            .flat_map(|(_, value)| std::env::split_paths(value).collect::<Vec<_>>())
            .map(|ceiling| fs::canonicalize(&ceiling).unwrap_or(ceiling))
            .collect();
        for dir in checkout
            .ancestors()
            .take_while(|dir| !ceilings.iter().any(|ceiling| ceiling == dir))
        {
            if exists_no_follow(&dir.join(".git"))? || exists_no_follow(&dir.join("HEAD"))? {
                return Err(failed(String::from_utf8_lossy(&output.stderr).trim()));
            }
        }
        return Ok(());
    }
    let paths: [PathBuf; 3] = rev_parse_paths(checkout, &output.stdout).map_err(failed)?;
    for path in paths {
        let actual = preview.protected().check(&fs::canonicalize(&path)?)?;
        if !actual.starts_with(fixture) {
            return Err(refuse(format!(
                "git work tree or admin dir outside the fixture: {}",
                path.display()
            )));
        }
    }
    Ok(())
}

/// `N` paths from `rev-parse`, one per line, each joined to `checkout`
/// (Git prints a common dir relative to it). A path holding a newline shows
/// as extra lines and is refused.
fn rev_parse_paths<const N: usize>(
    checkout: &Path,
    stdout: &[u8],
) -> Result<[PathBuf; N], &'static str> {
    const UNEXPECTED: &str = "unexpected rev-parse output";
    let stdout = std::str::from_utf8(stdout).map_err(|_| "non-UTF-8 path")?;
    let lines: Vec<&str> = stdout
        .strip_suffix('\n')
        .ok_or(UNEXPECTED)?
        .split('\n')
        .collect();
    if lines.iter().any(|line| line.is_empty()) {
        return Err(UNEXPECTED);
    }
    lines
        .into_iter()
        .map(|line| checkout.join(line))
        .collect::<Vec<_>>()
        .try_into()
        .map_err(|_| UNEXPECTED)
}

/// When `checkout` holds a `.git` or is itself a Git dir (it holds `HEAD`),
/// the admin dir and common dir Git uses from there must resolve inside
/// `fixture` and outside every protected root. Git itself names them: it
/// reads a `.git` file and `commondir` differently from a line parser (it
/// keeps embedded newlines and trailing spaces), and a Git dir needs no
/// `.git` at all to carry a `commondir` of its own.
fn check_git_admin(preview: &Preview, fixture: &Path, checkout: &Path) -> io::Result<()> {
    let dot_git = checkout.join(".git");
    let has_dot_git = exists_no_follow(&dot_git)?;
    if !has_dot_git && !exists_no_follow(&checkout.join("HEAD"))? {
        return Ok(());
    }
    if has_dot_git {
        match fs::metadata(&dot_git) {
            Ok(metadata) if metadata.is_file() => check_pointer_text(&dot_git)?,
            Ok(_) => {}
            // A dangling `.git` link is refused, not mistaken for absence.
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(refuse(format!("dangling git link: {}", dot_git.display())));
            }
            Err(error) => return Err(error),
        }
    }
    // Git prints a relative common dir (`.git`, `../..`), which the
    // protected check refuses as traversal; the OS resolves it.
    let inside = |path: &Path| -> io::Result<PathBuf> {
        let actual = preview.protected().check(&fs::canonicalize(path)?)?;
        if !actual.starts_with(fixture) {
            return Err(refuse(format!(
                "git admin dir outside the fixture: {}",
                path.display()
            )));
        }
        Ok(actual)
    };
    let (git_dir, common_dir) = git_admin_dirs(checkout)?;
    let git_dir = inside(&git_dir)?;
    let commondir = git_dir.join("commondir");
    if exists_no_follow(&commondir)? {
        // A dangling link fails here rather than reading as absent.
        check_pointer_text(&commondir)?;
    }
    let common_dir = inside(&common_dir)?;
    // A link under the admin dirs can send a ref or object write out of the
    // fixture (`refs/heads/task` -> elsewhere); Git's own dirs hold none.
    refuse_symlinks_under(&git_dir)?;
    if common_dir != git_dir {
        refuse_symlinks_under(&common_dir)?;
    }
    Ok(())
}

/// Walks `dir` without following links and refuses the first symlink.
fn refuse_symlinks_under(dir: &Path) -> io::Result<()> {
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir)? {
            let path = entry?.path();
            let kind = fs::symlink_metadata(&path)?.file_type();
            if kind.is_symlink() {
                return Err(refuse(format!(
                    "symlink inside a git admin dir: {}",
                    path.display()
                )));
            }
            if kind.is_dir() {
                pending.push(path);
            }
        }
    }
    Ok(())
}

fn exists_no_follow(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// A `.git` file or `commondir` holds one line: any control character other
/// than its trailing newline (an embedded `\n`, a `\r`, a NUL) is refused
/// before Git is asked, because Git and a reader of lines disagree on it.
fn check_pointer_text(file: &Path) -> io::Result<()> {
    let bytes = fs::read(file)?;
    let body = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
    if body.is_empty() || body.iter().any(u8::is_ascii_control) {
        return Err(refuse(format!(
            "git pointer is not one plain line: {}",
            file.display()
        )));
    }
    Ok(())
}

/// The admin dir and common dir Git uses from `checkout`, as Git resolves
/// them (`rev-parse`, which only reads). A Git failure is a refusal. The
/// caller's `GIT_*` variables are cleared so the files on disk decide.
fn git_admin_dirs(checkout: &Path) -> io::Result<(PathBuf, PathBuf)> {
    let mut command = std::process::Command::new("git");
    for (name, _) in std::env::vars_os() {
        if name.to_str().is_none_or(|name| name.starts_with("GIT_")) {
            command.env_remove(name);
        }
    }
    let output = command
        .arg("-C")
        .arg(checkout)
        .args(["rev-parse", "--absolute-git-dir", "--git-common-dir"])
        .stdin(std::process::Stdio::null())
        .output()?;
    let failed = |why: &str| {
        refuse(format!(
            "git cannot name the admin dir of {}: {why}",
            checkout.display()
        ))
    };
    if !output.status.success() {
        return Err(failed(String::from_utf8_lossy(&output.stderr).trim()));
    }
    let [git_dir, common_dir] = rev_parse_paths(checkout, &output.stdout).map_err(failed)?;
    Ok((git_dir, common_dir))
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;

#[cfg(test)]
mod live_tests {
    use super::*;
    #[test]
    fn live_pr_actions_are_admitted_only_in_production() {
        let dir = tempfile::tempdir().unwrap();
        let preview = Preview::validate(
            &dir.path().join("preview"),
            crate::preview::ProtectedRoots::new(
                &dir.path().join("home"),
                [dir.path().join("live")],
            )
            .unwrap(),
        )
        .unwrap();
        let live = ServiceContext::production(&dir.path().join("live"), "example")
            .unwrap()
            .for_gpui_live();
        let sandbox = ServiceContext::preview(Arc::new(preview)).unwrap();
        for action in [
            crate::prs::PrAction::Approve,
            crate::prs::PrAction::Ready,
            crate::prs::PrAction::Merge(crate::merge_plan::MergeMethod::Squash),
        ] {
            let run = crate::work::pr_actions::RunPrAction {
                action,
                key: crate::merge_plan::PrKey::new("owner/repo", 53),
                account: None,
            };
            assert!(live.pr_action(&run).is_ok());
            assert!(sandbox.pr_action(&run).is_err());
        }
        assert!(live
            .authorize(class("merge_plan_set", Access::Write), None)
            .is_ok());
        assert!(sandbox
            .authorize(class("merge_plan_set", Access::Write), None)
            .is_err());
    }

    #[test]
    fn live_pr_integration_keeps_production_identity_but_closes_later_slice_effects() {
        let temp = tempfile::tempdir().unwrap();
        let production = ServiceContext::production(temp.path(), "274").unwrap();
        let live = production.clone().for_gpui_live();
        assert!(!live.is_preview());
        assert!(live.browser_read_only());
        assert_eq!(production.task_mcp().token(), live.task_mcp().token());
        for label in [
            "task_create",
            "task_update_handoff",
            "delegate_task",
            "Save delegation",
            "Queue session",
        ] {
            assert!(live.authorize(class(label, Access::Write), None).is_ok());
        }
        for label in [
            "task_launch_session",
            "delegate_task",
            "review_request",
            "consult_request",
            "Launch session",
            "Start worker",
            "Start queued session",
            "Start queued delegation",
            "Start delegation",
            "Stop delegation",
        ] {
            assert!(live.authorize(class(label, Access::Process), None).is_ok());
        }
        for label in ["merge_plan_set", "Dismiss delegation", "Save delivery"] {
            assert!(live.authorize(class(label, Access::Write), None).is_ok());
        }
        for label in ["Automatic delivery", "Deliver results"] {
            assert!(live.authorize(class(label, Access::Process), None).is_ok());
        }
        assert_eq!(
            live.authorize(class("notify", Access::Write), None)
                .unwrap_err()
                .notice(),
            preview_notice("notify")
        );
        for access in [Access::Write, Access::Process, Access::Outside] {
            let class = Class {
                label: "Live gated effect",
                access,
            };
            assert!(production.authorize(class, None).is_ok());
            assert_eq!(
                live.authorize(class, None).unwrap_err().notice(),
                preview_notice(class.label)
            );
        }
        for access in [Access::Read, Access::Existing] {
            assert!(live
                .authorize(
                    Class {
                        label: "read",
                        access
                    },
                    None
                )
                .is_ok());
        }
    }
}
