//! Iced's task rail (`view_task_rail`), the task Overview's verbs
//! (`view_task_detail`) and the task modals (`view_new_task_modal`,
//! `view_task_publish_modal`, `view_task_worktree_delete_modal`,
//! `view_task_tab_close_modal`), mounted over the preview task host's seeded
//! store (H5b B8a).
//!
//! Reads work: filters, selection, opening and dismissing the forms, typing.
//! Every write or launch is built as the core `TaskMutationAction` Iced would
//! run, checked by `ServiceContext::task_action`, and stops at its preview
//! notice. Nothing here writes the store, runs git or starts a process.
use crate::{task_host::GpuiTaskHost, theme::Palette};
use gitterm_core::{
    preview::service::{ServiceContext, classify_task_action},
    session::{self, Target},
    task_commands::{TaskCreate, TaskCreateOrigin, TaskMutationAction, TaskPublish},
    task_host::TaskHost,
    task_worktree::{pr_base_branch, suggested_task_branch, suggested_task_worktree_path},
    tasks::{
        StoppingBoundary, TaskLifecycle, TaskRecord, TaskStore, TaskWorktreeState,
        WorkspaceLocationIdentity, validate_task_objective, validate_task_title,
    },
};
use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, InteractiveElement as _, IntoElement,
    ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _, Subscription,
    Window, div, prelude::FluentBuilder as _, px, rgb,
};
use gpui_component::{
    Disableable as _, IndexPath, Sizable as _,
    button::{Button, ButtonVariants as _},
    input::{Input, InputEvent, InputState},
    select::{Select, SelectEvent, SelectState},
};
use std::path::{Path, PathBuf};

#[cfg(test)]
#[path = "task_panel_tests.rs"]
mod tests;

/// Iced's `task-<8 hex>` shape. Shown in the form only; the preview never
/// stores it.
fn fresh_task_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    format!("task-{nanos:08x}")
}

/// Iced's `CUSTOM_TASK_BASE`.
const CUSTOM_BASE: &str = "Custom ref…";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum RailFilter {
    #[default]
    All,
    NeedsYou,
    Archived,
}

/// What the shell knows that the store does not, read at each render.
#[derive(Clone, Debug, Default)]
pub(crate) struct Inputs {
    pub workspace_name: String,
    /// The local workspace directory; `None` for a remote workspace.
    pub dir: Option<PathBuf>,
    pub selected: Option<String>,
    pub general_sessions: usize,
    /// Iced's `sidebar_panel_width() <= 160`.
    pub compact: bool,
    /// The open task sessions in this workspace, first per task first
    /// (Iced `first_open_task_tab`).
    pub open_sessions: Vec<Target>,
}
impl Inputs {
    fn open_session(&self, task_id: &str) -> Option<&Target> {
        self.open_sessions
            .iter()
            .find(|target| target.task_id.as_deref() == Some(task_id))
    }
}

/// The preview host's store and services; `None` when the host did not start.
pub(crate) fn host(cx: &App) -> Option<&TaskHost> {
    cx.try_global::<GpuiTaskHost>()
        .map(|host| host.0.read(cx).host())
}

fn same_dir(a: &Path, b: &Path) -> bool {
    a == b
        || a.canonicalize().unwrap_or_else(|_| a.to_owned())
            == b.canonicalize().unwrap_or_else(|_| b.to_owned())
}

fn in_workspace(task: &TaskRecord, dir: Option<&Path>) -> bool {
    match (&task.workspace.location, dir) {
        (WorkspaceLocationIdentity::Local { directory }, Some(dir)) => same_dir(directory, dir),
        _ => false,
    }
}

fn needs_attention(task: &TaskRecord) -> bool {
    task.attention.reason.is_some() || task.attention.unread
}

/// Iced `view_task_rail`'s counts and rows for one workspace and filter.
pub(crate) struct Rail<'a> {
    pub active: usize,
    pub needs_you: usize,
    pub failed: Vec<&'a TaskRecord>,
    pub finished: Vec<&'a TaskRecord>,
    pub rows: Vec<&'a TaskRecord>,
}

pub(crate) fn rail<'a>(store: &'a TaskStore, dir: Option<&Path>, filter: RailFilter) -> Rail<'a> {
    let tasks: Vec<_> = store
        .tasks()
        .iter()
        .filter(|task| in_workspace(task, dir))
        .collect();
    let live = |task: &&TaskRecord| task.lifecycle != TaskLifecycle::Archived;
    let mut rows: Vec<_> = tasks
        .iter()
        .copied()
        .filter(|task| match filter {
            RailFilter::All => live(task),
            RailFilter::NeedsYou => live(task) && needs_attention(task),
            RailFilter::Archived => !live(task),
        })
        .collect();
    rows.sort_by(|a, b| {
        crate::tasks::sort_bucket(a)
            .cmp(&crate::tasks::sort_bucket(b))
            .then_with(|| b.updated_at.cmp(&a.updated_at))
    });
    Rail {
        active: tasks.iter().filter(|task| live(task)).count(),
        needs_you: tasks
            .iter()
            .filter(|task| live(task) && needs_attention(task))
            .count(),
        failed: tasks
            .iter()
            .copied()
            .filter(|task| task.lifecycle == TaskLifecycle::Failed)
            .collect(),
        // Iced `finished_task_ids_for_workspace`.
        finished: tasks
            .iter()
            .copied()
            .filter(|task| {
                !task.lifecycle.is_active()
                    && live(task)
                    && store.task_worktree_missing(&task.task_id)
            })
            .collect(),
        rows,
    }
}

/// Iced's rail-row Archive: settled, not archived, not compact.
pub(crate) fn row_archivable(task: &TaskRecord, compact: bool) -> bool {
    !compact && task.lifecycle != TaskLifecycle::Archived && !task.lifecycle.is_active()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verb {
    ReviewChanges,
    PushBranch,
    OpenDraftPr,
    Stop,
    CancelQueue,
    Archive { enabled: bool },
}

/// Iced `view_task_detail`'s state verbs in its order (T03/T04 verbs are
/// not mounted here). Publishing is never busy in the preview.
pub(crate) fn verbs(task: &TaskRecord, open_session: bool) -> Vec<Verb> {
    let mut verbs = vec![];
    if task.worktree.state == TaskWorktreeState::Ready
        && task.worktree.path.is_some()
        && task.lifecycle != TaskLifecycle::Archived
    {
        verbs.push(Verb::ReviewChanges);
        if matches!(
            task.workspace.location,
            WorkspaceLocationIdentity::Local { .. }
        ) {
            verbs.push(Verb::PushBranch);
            if task.delivery.pr_url.is_none() {
                verbs.push(Verb::OpenDraftPr);
            }
        }
    }
    if task.lifecycle.is_active() && open_session {
        verbs.push(Verb::Stop);
    }
    if task.lifecycle == TaskLifecycle::Queued {
        verbs.push(Verb::CancelQueue);
    }
    verbs.push(Verb::Archive {
        enabled: !task.lifecycle.is_active() && task.lifecycle != TaskLifecycle::Archived,
    });
    verbs
}

/// Iced shows `Delete worktree…` under TASK SETTINGS whenever a path is recorded.
pub(crate) fn delete_opener(task: &TaskRecord) -> bool {
    task.worktree.path.is_some()
}

/// Iced's tab-close rule: a task session that is still running asks first.
/// The preview hosts no task process, so "running" is the record's lifecycle.
pub(crate) fn close_needs_confirm(
    current: &Target,
    requested: &Target,
    last: bool,
    cx: &App,
) -> bool {
    let task_live = requested.task_id.as_deref().is_some_and(|id| {
        host(cx)
            .and_then(|host| host.store().as_ref())
            .and_then(|store| store.get(id))
            .is_some_and(|task| task.lifecycle.is_active())
    });
    let state = session::State {
        target: current.clone(),
        control: session::TurnControl::Terminal,
        task_live,
        last_tab: last,
    };
    matches!(
        state.reduce(session::Intent::Close {
            target: requested.clone(),
            confirmed: false,
        }),
        session::Effect::ConfirmClose(_)
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Write {
    ArchiveFailed,
    ArchiveFinished,
    Archive(String),
    Publish {
        task_id: String,
        action: TaskPublish,
    },
    PublishConfirm,
    Stop(String),
    CancelQueue(String),
    DeleteConfirm,
    CreateSubmit,
    StopAndClose,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TaskPanelOutcome {
    Select(String),
    Back,
    Review(PathBuf),
    Notice(String),
}

pub(crate) struct CreateForm {
    pub task_id: String,
    workspace_name: String,
    repository_path: PathBuf,
    pub title: Entity<InputState>,
    pub objective: Entity<InputState>,
    pub issue: Entity<InputState>,
    pub base: Entity<SelectState<Vec<String>>>,
    pub custom_base: Entity<InputState>,
    pub base_custom: bool,
    pub stopping_boundary: StoppingBoundary,
    pub error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

pub(crate) enum Modal {
    Create(Box<CreateForm>),
    Publish {
        task_id: String,
        action: TaskPublish,
        error: Option<String>,
    },
    Delete {
        task_id: String,
        path: PathBuf,
        /// Iced decides this from the inspection; the preview cannot inspect
        /// (it may write the index), so the record's missing path decides.
        record_only: bool,
        inspection: String,
        error: Option<String>,
    },
    StopAndClose {
        target: Target,
        error: Option<String>,
    },
}

#[derive(Default)]
pub(crate) struct TaskPanel {
    pub filter: RailFilter,
    pub modal: Option<Modal>,
    /// The latest refusal, where Iced shows `task_ui_error`.
    pub notice: Option<String>,
}
impl EventEmitter<TaskPanelOutcome> for TaskPanel {}

/// A denied action's notice. GPUI only runs preview contexts, which deny
/// every task write and launch.
// shortcut: an allowed write is reported, not run; B9–B11 add the executor.
fn refuse(services: &ServiceContext, action: &TaskMutationAction) -> String {
    match services.task_action(action, None) {
        Err(denied) => denied.notice().to_owned(),
        Ok(()) => format!(
            "{} · not wired in the GPUI preview",
            classify_task_action(action).label
        ),
    }
}

impl TaskPanel {
    pub(crate) fn modal_open(&self) -> bool {
        self.modal.is_some()
    }

    pub(crate) fn select_filter(&mut self, filter: RailFilter, cx: &mut Context<Self>) {
        self.filter = filter;
        cx.notify();
    }

    pub(crate) fn dismiss(&mut self, cx: &mut Context<Self>) {
        self.modal = None;
        cx.notify();
    }

    fn show_notice(&mut self, notice: String, cx: &mut Context<Self>) {
        match &mut self.modal {
            Some(Modal::Create(form)) => form.error = Some(notice.clone()),
            Some(
                Modal::Publish { error, .. }
                | Modal::Delete { error, .. }
                | Modal::StopAndClose { error, .. },
            ) => *error = Some(notice.clone()),
            None => {}
        }
        self.notice = Some(notice.clone());
        cx.emit(TaskPanelOutcome::Notice(notice));
        cx.notify();
    }

    /// Iced `TaskCreateOpen`: the same preconditions, then an empty form.
    pub(crate) fn open_create(
        &mut self,
        inputs: &Inputs,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.notice = None;
        if host(cx).and_then(|host| host.store().as_ref()).is_none() {
            self.notice = Some("The task store is unavailable".into());
            cx.notify();
            return;
        }
        let Some(dir) = inputs.dir.clone() else {
            self.notice =
                Some("Local task creation is available only from a workspace on this Mac".into());
            cx.notify();
            return;
        };
        let mut input = |placeholder: &'static str, cx: &mut Context<Self>| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        let title = input("Short outcome", cx);
        let objective = input("What should the agent accomplish?", cx);
        let issue = input("TRU-105 (optional)", cx);
        let custom_base = input("branch, tag, remote ref, or SHA", cx);
        // Iced's choices before local branches load; the preview reads none.
        let base = cx.new(|cx| {
            SelectState::new(
                vec!["HEAD".to_owned(), CUSTOM_BASE.to_owned()],
                Some(IndexPath::default().row(0)),
                window,
                cx,
            )
        });
        title.update(cx, |input, cx| input.focus(window, cx));
        let mut subscriptions = vec![];
        for field in [&title, &objective, &issue, &custom_base] {
            subscriptions.push(cx.subscribe_in(
                field,
                window,
                |this, _, event, _, cx| match event {
                    InputEvent::Change => {
                        if let Some(Modal::Create(form)) = &mut this.modal {
                            form.error = None;
                        }
                        cx.notify();
                    }
                    InputEvent::PressEnter { .. } => {
                        this.write(Write::CreateSubmit, &Inputs::default(), cx)
                    }
                    _ => {}
                },
            ));
        }
        subscriptions.push(
            cx.subscribe(&base, |this, _, event: &SelectEvent<Vec<String>>, cx| {
                let SelectEvent::Confirm(choice) = event;
                if let Some(Modal::Create(form)) = &mut this.modal {
                    form.base_custom = choice.as_deref() == Some(CUSTOM_BASE);
                    form.error = None;
                }
                cx.notify();
            }),
        );
        self.modal = Some(Modal::Create(Box::new(CreateForm {
            task_id: fresh_task_id(),
            workspace_name: inputs.workspace_name.clone(),
            repository_path: dir,
            title,
            objective,
            issue,
            base,
            custom_base,
            base_custom: false,
            stopping_boundary: StoppingBoundary::ImplementUntilTestsPass,
            error: None,
            _subscriptions: subscriptions,
        })));
        cx.notify();
    }

    pub(crate) fn select_boundary(&mut self, boundary: StoppingBoundary, cx: &mut Context<Self>) {
        if let Some(Modal::Create(form)) = &mut self.modal {
            form.stopping_boundary = boundary;
            form.error = None;
            cx.notify();
        }
    }

    /// Iced `TaskWorktreeDeleteOpen`: the inspection it starts is checked first.
    pub(crate) fn open_delete(&mut self, task_id: &str, inputs: &Inputs, cx: &mut Context<Self>) {
        let Some(host) = host(cx) else { return };
        let Some(store) = host.store().as_ref() else {
            return;
        };
        let Some(task) = store.get(task_id) else {
            return;
        };
        let (Some(path), WorkspaceLocationIdentity::Local { directory }) =
            (task.worktree.path.clone(), &task.workspace.location)
        else {
            return;
        };
        let inspect = TaskMutationAction::InspectCleanup {
            task_id: task_id.to_owned(),
            repository: directory.clone(),
            path: path.clone(),
            branch: task.branch.clone(),
            base_commit: task.base.commit.clone(),
            process_running: inputs.open_session(task_id).is_some(),
            pull_request_url: task.delivery.pr_url.clone(),
        };
        let inspection = refuse(host.services(), &inspect);
        self.modal = Some(Modal::Delete {
            task_id: task_id.to_owned(),
            path,
            record_only: store.task_worktree_missing(task_id),
            inspection,
            error: None,
        });
        cx.notify();
    }

    /// Iced `TaskTabClose` on a running task session.
    pub(crate) fn open_stop_and_close(&mut self, target: Target, cx: &mut Context<Self>) {
        self.modal = Some(Modal::StopAndClose {
            target,
            error: None,
        });
        cx.notify();
    }

    /// The core action Iced would run for `write`, or `None` where Iced does
    /// nothing (a vanished task, no failed tasks).
    fn action(
        &self,
        write: &Write,
        store: &TaskStore,
        inputs: &Inputs,
        cx: &App,
    ) -> Result<Option<TaskMutationAction>, String> {
        let archive = |tasks: Vec<&TaskRecord>| {
            tasks.first().map(|task| TaskMutationAction::ArchiveTask {
                task_id: task.task_id.clone(),
            })
        };
        let publish = |task_id: &str, action: TaskPublish, confirmed: bool| {
            // Iced `reduce_task_publish` policy (core `TaskMutation::Publish`).
            let task = store.get(task_id)?;
            if task.issue.is_none() && !confirmed {
                return Some(TaskMutationAction::ConfirmPublish {
                    task_id: task_id.to_owned(),
                    action,
                });
            }
            let (title, body) = task.draft_pr_copy();
            Some(TaskMutationAction::Publish {
                task_id: task_id.to_owned(),
                action,
                path: task.worktree.path.clone()?,
                branch: task.branch.clone(),
                base: pr_base_branch(&task.base.reference).to_owned(),
                title,
                body,
            })
        };
        Ok(match write {
            Write::ArchiveFailed => archive(rail(store, inputs.dir.as_deref(), self.filter).failed),
            Write::ArchiveFinished => {
                archive(rail(store, inputs.dir.as_deref(), self.filter).finished)
            }
            Write::Archive(task_id) => {
                store.get(task_id).map(|_| TaskMutationAction::ArchiveTask {
                    task_id: task_id.clone(),
                })
            }
            Write::Publish { task_id, action } => publish(task_id, *action, false),
            Write::PublishConfirm => match &self.modal {
                Some(Modal::Publish {
                    task_id, action, ..
                }) => publish(task_id, *action, true),
                _ => None,
            },
            // Iced `TaskStop` closes the task's tabs, then records Stopped.
            Write::Stop(task_id) => inputs
                .open_session(task_id)
                .map(|target| TaskMutationAction::Session(session::Effect::Close(target.clone()))),
            // Iced `TaskQueueCancel` records the task Ready again.
            Write::CancelQueue(task_id) => store.get(task_id).map(|task| {
                let mut record = task.clone();
                record.lifecycle = TaskLifecycle::Ready;
                TaskMutationAction::PersistPreparing {
                    record: Box::new(record),
                }
            }),
            Write::DeleteConfirm => match &self.modal {
                Some(Modal::Delete {
                    task_id,
                    path,
                    record_only,
                    ..
                }) => match (*record_only, store.get(task_id)) {
                    (true, _) => Some(TaskMutationAction::ClearWorktree {
                        task_id: task_id.clone(),
                    }),
                    (false, Some(task)) => match &task.workspace.location {
                        WorkspaceLocationIdentity::Local { directory } => {
                            Some(TaskMutationAction::RemoveWorktree {
                                task_id: task_id.clone(),
                                repository: directory.clone(),
                                path: path.clone(),
                            })
                        }
                        _ => return Err("Could not resolve the task's repository directory".into()),
                    },
                    (false, None) => None,
                },
                _ => None,
            },
            Write::CreateSubmit => match &self.modal {
                Some(Modal::Create(form)) => {
                    // Iced validates the form through the core reducer first.
                    let read =
                        |input: &Entity<InputState>| input.read(cx).value().trim().to_owned();
                    let title = read(&form.title);
                    let objective = read(&form.objective);
                    validate_task_title(&title).map_err(|_| "Title is required".to_owned())?;
                    validate_task_objective(&objective)
                        .map_err(|_| "Objective is required".to_owned())?;
                    let base = if form.base_custom {
                        read(&form.custom_base)
                    } else {
                        form.base
                            .read(cx)
                            .selected_value()
                            .cloned()
                            .unwrap_or_else(|| "HEAD".into())
                    };
                    Some(TaskMutationAction::PrepareTask(TaskCreate {
                        title,
                        objective,
                        repository_path: form.repository_path.clone(),
                        workspace_name: Some(form.workspace_name.clone()),
                        issue_key: Some(read(&form.issue)),
                        base_reference: Some(base),
                        stopping_boundary: form.stopping_boundary,
                        origin: TaskCreateOrigin::Form,
                    }))
                }
                _ => None,
            },
            Write::StopAndClose => match &self.modal {
                Some(Modal::StopAndClose { target, .. }) => Some(TaskMutationAction::Session(
                    session::Effect::Close(target.clone()),
                )),
                _ => None,
            },
        })
    }

    /// Runs Iced's decision for `write` up to the service check. A read
    /// (the no-issue publish confirmation) opens its modal; anything else
    /// shows the context's notice and changes nothing.
    pub(crate) fn write(&mut self, write: Write, inputs: &Inputs, cx: &mut Context<Self>) {
        let Some(host) = host(cx) else {
            self.show_notice("The task store is unavailable".into(), cx);
            return;
        };
        let Some(store) = host.store().as_ref() else {
            self.show_notice("The task store is unavailable".into(), cx);
            return;
        };
        let action = match self.action(&write, store, inputs, cx) {
            Ok(Some(action)) => action,
            Ok(None) => return,
            Err(error) => {
                self.show_notice(error, cx);
                return;
            }
        };
        if let TaskMutationAction::ConfirmPublish {
            task_id,
            action: publish,
        } = &action
            && host.services().task_action(&action, None).is_ok()
        {
            self.modal = Some(Modal::Publish {
                task_id: task_id.clone(),
                action: *publish,
                error: None,
            });
            cx.notify();
            return;
        }
        let notice = refuse(host.services(), &action);
        self.show_notice(notice, cx);
    }
}

// ---- Rendering: free functions so the shell places each surface where Iced does.

fn muted(p: &Palette) -> gpui::Rgba {
    rgb(p.muted)
}

fn ghost(id: impl Into<SharedString>, label: impl Into<SharedString>) -> Button {
    let id = id.into();
    let selector = id.clone();
    Button::new(gpui::ElementId::Name(id))
        .debug_selector(move || selector.to_string())
        .ghost()
        .small()
        .label(label.into())
}

/// Iced `view_task_rail`, for the sidebar's Tasks mode.
pub(crate) fn rail_view(panel: &Entity<TaskPanel>, inputs: Inputs, cx: &App) -> gpui::AnyElement {
    let p = Palette::current(cx);
    let state = panel.read(cx);
    let filter = state.filter;
    let store = host(cx).and_then(|host| host.store().as_ref());
    let model = store.map(|store| rail(store, inputs.dir.as_deref(), filter));
    let inputs = std::rc::Rc::new(inputs);
    let mut content = div()
        .id("task-rail")
        .debug_selector(|| "task-rail".into())
        .flex()
        .flex_col()
        .gap(px(8.))
        .px(px(8.))
        .py(px(10.))
        .size_full()
        .overflow_y_scroll()
        .text_size(px(12.));

    let new_task = {
        let (panel, inputs) = (panel.clone(), inputs.clone());
        ghost("task-new", "+ New task")
            .text_color(rgb(p.blue))
            .on_click(move |_, window, cx| {
                panel.update(cx, |panel, cx| panel.open_create(&inputs, window, cx))
            })
    };
    content = content.child(
        div()
            .flex()
            .items_center()
            .child(
                div()
                    .flex_1()
                    .text_size(px(10.))
                    .text_color(muted(&p))
                    .child(inputs.workspace_name.to_uppercase()),
            )
            .child(new_task),
    );

    let general_selected = inputs.selected.is_none();
    let general = if general_selected {
        format!(
            "● General{}",
            if inputs.compact {
                String::new()
            } else {
                format!(
                    " · {} session{}",
                    inputs.general_sessions,
                    if inputs.general_sessions == 1 {
                        ""
                    } else {
                        "s"
                    }
                )
            }
        )
    } else if inputs.compact {
        "← Sessions".into()
    } else {
        "← Sessions · General".into()
    };
    let back = panel.clone();
    content = content.child(
        div()
            .id("task-general")
            .debug_selector(|| "task-general".into())
            .px(px(9.))
            .py(px(8.))
            .rounded(px(6.))
            .when(general_selected, |row| {
                row.bg(rgb(p.surface))
                    .border_1()
                    .border_color(rgb(p.surface2))
            })
            .hover(|row| row.bg(rgb(p.overlay)))
            .child(general)
            .on_click(move |_, _, cx| back.update(cx, |_, cx| cx.emit(TaskPanelOutcome::Back))),
    );

    let Some(model) = model else {
        return content
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(rgb(p.red))
                    .child("The task store is unavailable"),
            )
            .into_any_element();
    };
    content = content.child(
        div()
            .text_size(px(10.))
            .text_color(muted(&p))
            .child(format!("TASKS · {}", model.active)),
    );
    if !inputs.compact {
        let chip = |id: &'static str, label: String, chip_filter: RailFilter| {
            let active = filter == chip_filter;
            let panel = panel.clone();
            ghost(id, label)
                .xsmall()
                .rounded(px(10.))
                .border_1()
                .border_color(rgb(if active { p.lavender } else { p.border }))
                .when(active, |chip| {
                    chip.bg(rgb(p.lavender)).text_color(rgb(p.crust))
                })
                .when(!active, |chip| chip.text_color(muted(&p)))
                .on_click(move |_, _, cx| {
                    panel.update(cx, |panel, cx| panel.select_filter(chip_filter, cx))
                })
        };
        let danger_chip = |id: &'static str, label: String, write: Write| {
            let (panel, inputs) = (panel.clone(), inputs.clone());
            ghost(id, label)
                .xsmall()
                .rounded(px(10.))
                .border_1()
                .border_color(rgb(p.red))
                .text_color(rgb(p.red))
                .on_click(move |_, _, cx| {
                    panel.update(cx, |panel, cx| panel.write(write.clone(), &inputs, cx))
                })
        };
        content = content.child(
            div()
                .flex()
                .flex_wrap()
                .gap(px(4.))
                .child(chip("task-filter-all", "All".into(), RailFilter::All))
                .child(chip(
                    "task-filter-needs-you",
                    format!("Needs you · {}", model.needs_you),
                    RailFilter::NeedsYou,
                ))
                .child(chip(
                    "task-filter-archived",
                    "Archived".into(),
                    RailFilter::Archived,
                ))
                .when(!model.failed.is_empty(), |row| {
                    row.child(danger_chip(
                        "task-archive-failed",
                        format!("Archive failed · {}", model.failed.len()),
                        Write::ArchiveFailed,
                    ))
                })
                .when(!model.finished.is_empty(), |row| {
                    row.child(danger_chip(
                        "task-archive-finished",
                        format!("Archive {} finished", model.finished.len()),
                        Write::ArchiveFinished,
                    ))
                }),
        );
    }
    if let Some(notice) = &state.notice {
        content = content.child(
            div()
                .id("task-rail-notice")
                .debug_selector(|| "task-rail-notice".into())
                .text_size(px(10.))
                .text_color(rgb(p.red))
                .child(notice.clone()),
        );
    }
    if model.rows.is_empty() {
        content = content.child(div().text_size(px(11.)).text_color(rgb(p.subtext0)).child(
            match filter {
                RailFilter::All => "No active tasks",
                RailFilter::NeedsYou => "Nothing needs you",
                RailFilter::Archived => "No archived tasks",
            },
        ));
    }
    for task in model.rows {
        let selected = inputs.selected.as_deref() == Some(task.task_id.as_str());
        let issue = task.issue.as_ref().map(|issue| issue.key.clone());
        let mut title = div()
            .flex()
            .gap(px(7.))
            .child(div().text_color(muted(&p)).child("▸"))
            .child(
                div()
                    .text_color(rgb(if issue.is_some() { p.blue } else { p.text }))
                    .child(issue.clone().unwrap_or_else(|| task.title.clone())),
            );
        if !inputs.compact {
            title = title.child(if issue.is_some() {
                div().text_color(rgb(p.text)).child(task.title.clone())
            } else {
                div().text_size(px(9.)).text_color(muted(&p)).child("local")
            });
        }
        let state_line = crate::task_switcher::state_copy(task, &[], false);
        let id = task.task_id.clone();
        let select = panel.clone();
        let row = div()
            .id(SharedString::from(format!("task-row-{id}")))
            .debug_selector({
                let id = id.clone();
                move || format!("task-row-{id}")
            })
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(3.))
            .px(px(8.))
            .py(px(7.))
            .rounded(px(6.))
            .when(selected, |row| {
                row.bg(rgb(p.surface))
                    .border_1()
                    .border_color(rgb(p.surface2))
            })
            .hover(|row| row.bg(rgb(p.overlay)))
            .child(title)
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(muted(&p))
                    .child(state_line),
            )
            .on_click({
                let id = id.clone();
                move |_, _, cx| {
                    select.update(cx, |_, cx| cx.emit(TaskPanelOutcome::Select(id.clone())))
                }
            });
        let mut line = div().flex().items_center().gap(px(2.)).child(row);
        if row_archivable(task, inputs.compact) {
            let (panel, inputs) = (panel.clone(), inputs.clone());
            line = line.child(
                ghost(format!("task-row-archive-{id}"), "Archive")
                    .xsmall()
                    .text_color(muted(&p))
                    .on_click(move |_, _, cx| {
                        panel.update(cx, |panel, cx| {
                            panel.write(Write::Archive(id.clone()), &inputs, cx)
                        })
                    }),
            );
        }
        content = content.child(line);
    }
    content.into_any_element()
}

/// Iced `view_task_detail` for the selected task: its card with the state
/// verbs, and TASK SETTINGS with `Delete worktree…`.
pub(crate) fn overview_view(
    panel: &Entity<TaskPanel>,
    inputs: Inputs,
    cx: &App,
) -> gpui::AnyElement {
    let p = Palette::current(cx);
    let task = inputs
        .selected
        .as_deref()
        .and_then(|id| host(cx)?.store().as_ref()?.get(id))
        .cloned();
    let page = div()
        .id("task-overview")
        .debug_selector(|| "task-overview".into())
        .flex_1()
        .h_full()
        .p_4()
        .flex()
        .flex_col()
        .gap_3()
        .overflow_y_scroll();
    let Some(task) = task else {
        let back = panel.clone();
        return page
            .child("Task not found")
            .child(
                div()
                    .text_color(muted(&p))
                    .child("This task is no longer in the task store."),
            )
            .child(
                ghost("task-overview-back", "Back to General").on_click(move |_, _, cx| {
                    back.update(cx, |_, cx| cx.emit(TaskPanelOutcome::Back))
                }),
            )
            .into_any_element();
    };
    let inputs = std::rc::Rc::new(inputs);
    let open_session = inputs.open_session(&task.task_id).is_some();
    let mut row = div().flex().flex_wrap().gap(px(10.));
    for verb in verbs(&task, open_session) {
        let (writer, inputs, id) = (panel.clone(), inputs.clone(), task.task_id.clone());
        let write = move |write: Write| {
            move |_: &gpui::ClickEvent, _: &mut Window, cx: &mut App| {
                writer.update(cx, |panel, cx| panel.write(write.clone(), &inputs, cx))
            }
        };
        row = row.child(match verb {
            Verb::ReviewChanges => {
                let review = task.worktree.path.clone();
                let panel = panel.clone();
                ghost("task-review-changes", "Review changes").on_click(move |_, _, cx| {
                    if let Some(path) = review.clone() {
                        panel.update(cx, |_, cx| cx.emit(TaskPanelOutcome::Review(path)));
                    }
                })
            }
            Verb::PushBranch => {
                ghost("task-push-branch", "Push branch").on_click(write(Write::Publish {
                    task_id: id,
                    action: TaskPublish::PushBranch,
                }))
            }
            Verb::OpenDraftPr => {
                ghost("task-open-draft-pr", "Open draft PR").on_click(write(Write::Publish {
                    task_id: id,
                    action: TaskPublish::OpenDraftPr,
                }))
            }
            Verb::Stop => ghost("task-stop", "■ Stop")
                .text_color(rgb(p.red))
                .on_click(write(Write::Stop(id))),
            Verb::CancelQueue => {
                ghost("task-cancel-queue", "Cancel queue").on_click(write(Write::CancelQueue(id)))
            }
            Verb::Archive { enabled } => ghost("task-archive", "Archive")
                .disabled(!enabled)
                .on_click(write(Write::Archive(id))),
        });
    }
    let mut page = page
        .child(div().text_size(px(20.)).child(task.title.clone()))
        .child(
            div()
                .text_color(rgb(p.subtext0))
                .child(task.objective.clone()),
        )
        .child(crate::task_switcher::state_copy(&task, &[], false))
        .child(row);
    if let Some(notice) = &panel.read(cx).notice {
        page = page.child(
            div()
                .text_size(px(11.))
                .text_color(rgb(p.red))
                .child(notice.clone()),
        );
    }
    if delete_opener(&task) {
        let (panel, inputs, id) = (panel.clone(), inputs.clone(), task.task_id.clone());
        page = page
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(muted(&p))
                    .child("TASK SETTINGS"),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(
                        ghost("task-delete-worktree-open", "Delete worktree…")
                            .text_color(rgb(p.red))
                            .on_click(move |_, _, cx| {
                                panel.update(cx, |panel, cx| panel.open_delete(&id, &inputs, cx))
                            }),
                    )
                    .child(div().text_size(px(10.)).text_color(muted(&p)).child(
                        "Removes the working copy from disk. The branch and task record stay.",
                    )),
            );
    }
    page.into_any_element()
}

fn boundary_label(boundary: StoppingBoundary) -> &'static str {
    match boundary {
        StoppingBoundary::PlanOnly => "Plan only",
        StoppingBoundary::ImplementUntilTestsPass => "Tests pass",
        StoppingBoundary::PrepareDraftPr => "Draft PR",
    }
}

/// The open task modal over the window, as Iced stacks it over the main view.
pub(crate) fn modal_view(
    panel: &Entity<TaskPanel>,
    inputs: Inputs,
    cx: &App,
) -> Option<gpui::AnyElement> {
    let p = Palette::current(cx);
    let state = panel.read(cx);
    let modal = state.modal.as_ref()?;
    let inputs = std::rc::Rc::new(inputs);
    let task_title = |task_id: &str, fallback: &'static str| {
        host(cx)
            .and_then(|host| host.store().as_ref())
            .and_then(|store| store.get(task_id))
            .map_or_else(|| fallback.to_owned(), |task| task.title.clone())
    };
    let dismiss = |id: &'static str, label: &'static str| {
        let panel = panel.clone();
        ghost(id, label).on_click(move |_, _, cx| panel.update(cx, |panel, cx| panel.dismiss(cx)))
    };
    let confirm = |id: &'static str, label: String, write: Write| {
        let (panel, inputs) = (panel.clone(), inputs.clone());
        ghost(id, label)
            .text_color(rgb(p.red))
            .on_click(move |_, _, cx| {
                panel.update(cx, |panel, cx| panel.write(write.clone(), &inputs, cx))
            })
    };
    let error = |error: &Option<String>| {
        error
            .clone()
            .map(|error| div().text_size(px(11.)).text_color(rgb(p.red)).child(error))
    };
    let buttons = || div().flex().justify_end().gap(px(8.));
    let mut card = div()
        .id("task-modal")
        .debug_selector(|| "task-modal".into())
        .flex()
        .flex_col()
        .gap(px(12.))
        .px(px(26.))
        .py(px(22.))
        .bg(rgb(p.mantle))
        .border_1()
        .border_color(rgb(p.border))
        .rounded(px(7.))
        .text_size(px(12.))
        .text_color(rgb(p.text));
    match modal {
        Modal::Publish {
            task_id,
            action,
            error: err,
        } => {
            let (heading, label) = match action {
                TaskPublish::PushBranch => ("Push without a linked issue?", "Push anyway"),
                TaskPublish::OpenDraftPr => {
                    ("Open a draft PR without a linked issue?", "Open PR anyway")
                }
            };
            card = card
                .w(px(560.))
                .child(div().text_size(px(16.)).child(heading))
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(muted(&p))
                        .child(format!("“{}”", task_title(task_id, "this task"))),
                )
                .child(div().text_color(rgb(p.subtext0)).child(
                    "This task has no linked issue. Commits and pull requests in this repo \
                     carry an issue key — link an issue to the task first, or continue \
                     without one.",
                ))
                .children(error(err))
                .child(
                    buttons()
                        .child(dismiss("task-publish-cancel", "Cancel"))
                        .child(confirm(
                            "task-publish-confirm",
                            label.into(),
                            Write::PublishConfirm,
                        )),
                );
        }
        Modal::Delete {
            task_id,
            path,
            record_only,
            inspection,
            error: err,
        } => {
            let label = if *record_only {
                "Clear worktree record"
            } else {
                "Delete worktree"
            };
            card = card
                .w(px(560.))
                .child(div().text_size(px(16.)).child("Delete worktree?"))
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(muted(&p))
                        .child(format!(
                            "“{}” · {}",
                            task_title(task_id, "this task"),
                            path.display()
                        )),
                )
                .child(
                    div()
                        .text_color(rgb(p.red))
                        .child(format!("Could not inspect the worktree: {inspection}")),
                )
                .children(error(err))
                .child(
                    buttons()
                        .child(dismiss("task-delete-keep", "Keep worktree"))
                        .child(confirm(
                            "task-delete-confirm",
                            label.into(),
                            Write::DeleteConfirm,
                        )),
                );
        }
        Modal::StopAndClose { target, error: err } => {
            let title = target
                .task_id
                .as_deref()
                .map_or_else(|| "this task".to_owned(), |id| task_title(id, "this task"));
            card =
                card.w(px(510.))
                    .child(div().text_size(px(16.)).child("Stop this task session?"))
                    .child(
                        div()
                            .text_color(rgb(p.subtext0))
                            .child(format!("A session of “{title}” is running in this tab.")),
                    )
                    .child(div().text_size(px(11.)).text_color(muted(&p)).child(
                        "Closing stops it. The task and its worktree stay available to resume.",
                    ))
                    .children(error(err))
                    .child(
                        buttons()
                            .child(dismiss("task-close-keep", "Keep open"))
                            .child(confirm(
                                "task-stop-and-close",
                                "Stop and close".into(),
                                Write::StopAndClose,
                            )),
                    );
        }
        Modal::Create(form) => {
            card = card
                .w(px(620.))
                .children(create_form(panel, form, &inputs, &p, cx))
        }
    }
    Some(
        div()
            .id("task-modal-backdrop")
            .occlude()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::Rgba {
                a: 0.82,
                ..rgb(p.crust)
            })
            .child(card)
            .into_any_element(),
    )
}

fn create_form(
    panel: &Entity<TaskPanel>,
    form: &CreateForm,
    inputs: &std::rc::Rc<Inputs>,
    p: &Palette,
    cx: &App,
) -> Vec<gpui::AnyElement> {
    let label = |text: &'static str| div().text_size(px(10.)).text_color(muted(p)).child(text);
    let field = |text: &'static str, selector: &'static str, input: &Entity<InputState>| {
        div()
            .flex()
            .flex_col()
            .gap(px(4.))
            .child(label(text))
            .child(
                div()
                    .debug_selector(move || selector.into())
                    .child(Input::new(input).small()),
            )
    };
    let issue = form.issue.read(cx).value().trim().to_owned();
    let branch = suggested_task_branch(
        &form.task_id,
        (!issue.is_empty()).then_some(issue.as_str()),
        form.title.read(cx).value().as_ref(),
    );
    let worktree = branch
        .as_deref()
        .and_then(|branch| {
            suggested_task_worktree_path(
                &form.repository_path,
                &crate::workspace_config::iced_config_dir().join("worktrees"),
                branch,
            )
        })
        .map_or_else(|| "—".to_owned(), |path| path.display().to_string());
    let mut base = div()
        .flex_1()
        .flex()
        .flex_col()
        .gap(px(4.))
        .child(label("BASE"))
        .child(Select::new(&form.base).small().w_full());
    if form.base_custom {
        base = base.child(
            div()
                .debug_selector(|| "task-create-custom-base".into())
                .child(Input::new(&form.custom_base).small()),
        );
    }
    let mut boundaries = div().flex().gap(px(6.));
    for boundary in [
        StoppingBoundary::PlanOnly,
        StoppingBoundary::ImplementUntilTestsPass,
        StoppingBoundary::PrepareDraftPr,
    ] {
        let selected = boundary == form.stopping_boundary;
        let panel = panel.clone();
        boundaries = boundaries.child(
            ghost(
                format!("task-boundary-{boundary:?}"),
                boundary_label(boundary),
            )
            .border_1()
            .border_color(rgb(if selected { p.blue } else { p.border }))
            .when(selected, |chip| {
                chip.bg(rgb(p.blue)).text_color(rgb(p.base))
            })
            .on_click(move |_, _, cx| {
                panel.update(cx, |panel, cx| panel.select_boundary(boundary, cx))
            }),
        );
    }
    let cancel = panel.clone();
    let submit = (panel.clone(), inputs.clone());
    vec![
        div()
            .flex()
            .items_center()
            .child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap(px(3.))
                    .child(div().text_size(px(17.)).child("New task"))
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(muted(p))
                            .child(format!("{} · This Mac", form.workspace_name)),
                    ),
            )
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(muted(p))
                    .child(form.task_id.clone()),
            )
            .into_any_element(),
        field("TITLE", "task-create-title", &form.title).into_any_element(),
        field("OBJECTIVE", "task-create-objective", &form.objective).into_any_element(),
        div()
            .flex()
            .gap(px(8.))
            .child(field("LINEAR ISSUE", "task-create-issue", &form.issue).flex_1())
            .child(base)
            .into_any_element(),
        label("STOPPING BOUNDARY").into_any_element(),
        boundaries.into_any_element(),
        div()
            .flex()
            .flex_col()
            .gap(px(4.))
            .p(px(8.))
            .bg(rgb(p.base))
            .border_1()
            .border_color(rgb(p.border))
            .rounded(px(3.))
            .text_size(px(10.))
            .text_color(rgb(p.subtext0))
            .child(format!(
                "BRANCH     {}",
                branch.unwrap_or_else(|| "—".into())
            ))
            .child(format!("WORKTREE   {worktree}"))
            .into_any_element(),
        div()
            .children(
                form.error
                    .clone()
                    .map(|error| div().text_size(px(11.)).text_color(rgb(p.red)).child(error)),
            )
            .into_any_element(),
        div()
            .flex()
            .justify_end()
            .gap(px(8.))
            .child(
                ghost("task-create-cancel", "Cancel")
                    .on_click(move |_, _, cx| cancel.update(cx, |panel, cx| panel.dismiss(cx))),
            )
            .child(
                ghost("task-create-submit", "Create task").on_click(move |_, _, cx| {
                    let (panel, inputs) = &submit;
                    panel.update(cx, |panel, cx| panel.write(Write::CreateSubmit, inputs, cx))
                }),
            )
            .into_any_element(),
    ]
}

// ---- Shell hookup.

impl crate::Shell {
    /// The panel, created with its outcome subscription on first use.
    pub(crate) fn ensure_task_panel(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<TaskPanel> {
        if let Some(panel) = &self.task_panel {
            return panel.clone();
        }
        let panel = cx.new(|_| TaskPanel::default());
        let subscription = cx.subscribe_in(
            &panel,
            window,
            |shell, _, outcome: &TaskPanelOutcome, window, cx| {
                match outcome {
                    TaskPanelOutcome::Select(task) => shell.activate_task(task, window, cx),
                    // Iced `TaskContextBack` keeps the rail as the host tab's sidebar.
                    TaskPanelOutcome::Back => {
                        shell.general_context(false, window, cx);
                        shell.show_task_rail(cx);
                    }
                    // Iced lands on a task tab with Git Changes; the preview opens
                    // the worktree's review, a read.
                    TaskPanelOutcome::Review(checkout) => {
                        shell.open_work_review(checkout.clone(), false, window, cx)
                    }
                    TaskPanelOutcome::Notice(notice) => shell.owner.update(cx, |owner, cx| {
                        owner.error = Some(notice.clone());
                        cx.notify();
                    }),
                }
            },
        );
        let observe = cx.observe(&panel, |_, _, cx| cx.notify());
        self._subscriptions.extend([subscription, observe]);
        if let Some(host) = cx.try_global::<GpuiTaskHost>().map(|host| host.0.clone()) {
            self._subscriptions
                .push(cx.observe(&host, |_, _, cx| cx.notify()));
        }
        self.task_panel = Some(panel.clone());
        panel
    }

    pub(crate) fn task_panel_modal_open(&self, cx: &App) -> bool {
        self.task_panel
            .as_ref()
            .is_some_and(|panel| panel.read(cx).modal_open())
    }

    /// Iced sets the host tab's sidebar to Tasks on entering the Overview.
    pub(crate) fn show_task_rail(&mut self, cx: &mut Context<Self>) {
        self.update_regions(
            |regions| {
                regions.sidebar_mode = crate::layout::SidebarMode::Tasks;
                regions.sidebar_collapsed = false;
            },
            cx,
        );
    }

    pub(crate) fn task_inputs(&self, cx: &App) -> Inputs {
        let Some(workspace) = self.active_workspace(cx) else {
            return Inputs::default();
        };
        let state = self.active(cx);
        let targets: Vec<Target> = state
            .sessions
            .iter()
            .filter_map(|session| {
                state
                    .panes
                    .get(&crate::session_pane_id(session.id))?
                    .read(cx)
                    .session
                    .clone()
            })
            .collect();
        Inputs {
            workspace_name: state.workspace.name.clone(),
            dir: self.data.terminal_cwd(workspace, cx),
            selected: self.selected_task.clone(),
            general_sessions: state
                .sessions
                .iter()
                .filter(|session| self.session_task(session.id, cx).is_none())
                .count(),
            compact: state.store.regions.sidebar_size() <= 160.,
            open_sessions: targets
                .into_iter()
                .filter(|target| target.task_id.is_some())
                .collect(),
        }
    }
}
