//! Trusted task-panel ingress. Decisions use TaskAuthority; effects stay on
//! the same owner and services as task MCP, with Preview always read-only.
use super::*;
use gitterm_core::task_commands::TaskPublish;

type StopCompletion = tokio::sync::oneshot::Sender<Result<(), String>>;

#[derive(Clone, Debug)]
pub(crate) struct StopTask {
    pub task_id: String,
    pub targets: Vec<session::Target>,
    completion: Arc<std::sync::Mutex<Option<StopCompletion>>>,
}
impl StopTask {
    pub(crate) fn complete(&self, result: Result<(), String>) {
        if let Some(send) = self.completion.lock().unwrap().take() {
            _ = send.send(result);
        }
    }
}
impl gpui::EventEmitter<StopTask> for TaskHostOwner {}

impl TaskHostOwner {
    pub(crate) fn panel_facts(&self, cx: &App) -> TaskMutationFacts {
        let settings = crate::workspace_config::load_iced_settings_from(&self.iced_config);
        let mut facts = self.launch_facts(&settings, cx);
        facts.sessions.extend(self.terminal_session_states());
        facts.publish_busy = self.panel_publishing.iter().cloned().collect();
        facts
    }

    pub(crate) fn panel_resume_facts(
        &self,
        mutation: &TaskMutation,
        facts: &mut TaskMutationFacts,
    ) {
        if let TaskMutation::ResumeConversation {
            task_id, chat_id, ..
        } = mutation
            && self
                .host
                .store()
                .as_ref()
                .and_then(|s| s.get(task_id))
                .is_some_and(|task| {
                    task.sessions.iter().any(|s| {
                        s.conversation.as_ref().is_some_and(|c| {
                            c.session_id == *chat_id
                                && c.backend
                                    == gitterm_core::tasks::HarnessConversationBackend::Claude
                        })
                    })
                })
            && !dirs::home_dir().is_some_and(|home| {
                super::launch::claude_transcript_exists(&home.join(".claude/projects"), chat_id)
            })
        {
            facts.phantom_conversations.push(chat_id.clone());
        }
    }

    pub(crate) fn create_from_panel(
        &mut self,
        input: TaskCreate,
        task_id: String,
        cx: &mut Context<Self>,
    ) -> Result<tokio::sync::oneshot::Receiver<Result<serde_json::Value, String>>, String> {
        let action = TaskMutationAction::PrepareTask(input.clone());
        self.panel_authorize(&action)?;
        let (send, receive) = tokio::sync::oneshot::channel();
        let mut pending = self.begin_create_input(None, None, input, "Create task", Some(send))?;
        // Iced displays this identity in the form before submitting it.
        pending.task_id = task_id.clone();
        pending.request.task_id = task_id;
        self.resolve_create(pending, cx);
        Ok(receive)
    }

    pub(crate) fn owns_fixture_terminal(&self, target: &session::Target) -> bool {
        !self.fixtures.is_empty()
            && self
                .terminal_session_states()
                .iter()
                .any(|s| s.target == *target)
    }

    pub(crate) fn panel_authorize(&self, action: &TaskMutationAction) -> Result<(), String> {
        if let TaskMutationAction::Session(session::Effect::Close(target)) = action
            && self.owns_fixture_terminal(target)
        {
            let path = target.task_id.as_deref().and_then(|id| {
                self.host
                    .store()
                    .as_ref()?
                    .get(id)?
                    .worktree
                    .path
                    .as_deref()
            });
            return self
                .host
                .services()
                .task_action(action, path)
                .map_err(|e| e.notice().to_owned());
        }
        if self.host.services().is_preview() {
            return Err(preview_notice(
                gitterm_core::preview::service::classify_task_action(action).label,
            ));
        }
        self.host
            .services()
            .task_action(action, None)
            .map_err(|e| e.notice().to_owned())
    }

    /// Only the UI calls this: control callers keep their issued-caller gate.
    pub(crate) fn execute_panel_action(
        &mut self,
        action: TaskMutationAction,
        cx: &mut Context<Self>,
    ) -> Result<Task<Result<(), String>>, String> {
        self.panel_authorize(&action)?;
        let now = chrono::Utc::now().to_rfc3339();
        match action {
            TaskMutationAction::None => {}
            TaskMutationAction::ArchiveTask { task_id } => {
                self.writable_store()?
                    .archive(&task_id, &now)
                    .map_err(|e| e.to_string())?;
            }
            TaskMutationAction::CancelQueue { task_id } => {
                self.host
                    .launch_queue_mut()
                    .retain(|q| q.task_id != task_id);
                self.writable_store()?
                    .record_lifecycle_signal(&task_id, TaskLifecycle::Ready, None, &now)
                    .map_err(|e| e.to_string())?;
            }
            TaskMutationAction::StopTask { task_id, targets } => {
                let targets = targets
                    .into_iter()
                    .filter(|target| !self.stop_panel_unadopted(target, cx))
                    .collect();
                return Ok(self.stop_panel_tabs(task_id, targets, cx));
            }
            TaskMutationAction::Session(session::Effect::Close(target)) => {
                if self.stop_panel_unadopted(&target, cx) {
                    return Ok(Task::ready(Ok(())));
                }
                return Ok(self.stop_panel_tabs(String::new(), vec![target], cx));
            }
            TaskMutationAction::ClearWorktree { task_id } => {
                self.panel_worktree_removed(&task_id)?
            }
            TaskMutationAction::RemoveWorktree {
                task_id,
                repository,
                path,
            } => {
                let services = self.host.services().clone();
                return Ok(cx.spawn(async move |owner, cx| {
                    let result = cx.background_spawn(async move {
                        services.task_action(&TaskMutationAction::RemoveWorktree {
                            task_id: task_id.clone(), repository: repository.clone(), path: path.clone()
                        }, None).map_err(|e| e.notice().to_owned())?;
                        gitterm_core::task_worktree::remove_task_worktree_blocking(&repository, &path, false).map_err(|e| e.to_string())?;
                        Ok::<_, String>(task_id)
                    }).await?;
                    owner.update(cx, |owner, cx| {
                        let saved = owner.panel_worktree_removed(&result);
                        cx.notify();
                        saved.map_err(|e| format!("Worktree deleted, but the task record could not be updated: {e}"))
                    }).map_err(|e| e.to_string())?
                }));
            }
            TaskMutationAction::Publish {
                task_id,
                action,
                path,
                branch,
                base,
                title,
                body,
            } => {
                let account = self.panel_gh_account(&task_id)?;
                if !self.panel_publishing.insert(task_id.clone()) {
                    return Ok(Task::ready(Ok(())));
                }
                let runtime = self
                    .servers
                    .as_ref()
                    .ok_or(SHUT_DOWN)?
                    .runtime
                    .handle()
                    .clone();
                return Ok(cx.spawn(async move |owner, cx| {
                    let result = runtime
                        .spawn(async move {
                            match action {
                                TaskPublish::PushBranch => {
                                    gitterm_core::task_worktree::push_task_branch(
                                        path, branch, account,
                                    )
                                    .await
                                }
                                TaskPublish::OpenDraftPr => {
                                    gitterm_core::task_worktree::open_task_draft_pr(
                                        path, branch, base, title, body, account,
                                    )
                                    .await
                                }
                            }
                            .map_err(|e| e.to_string())
                        })
                        .await
                        .map_err(|e| e.to_string())
                        .and_then(|r| r);
                    owner
                        .update(cx, |owner, cx| {
                            owner.panel_publishing.remove(&task_id);
                            cx.notify();
                            let found = result?;
                            if let TaskMutationAction::PersistDelivery { delivery, .. } = owner
                                .reduce(
                                    TaskMutation::RecordDelivery {
                                        task_id: task_id.clone(),
                                        commit_sha: found.local_head,
                                        pr_url: found.pull_request.as_ref().map(|p| p.url.clone()),
                                        pr_number: found.pull_request.as_ref().map(|p| p.number),
                                    },
                                    &Default::default(),
                                )?
                            {
                                owner
                                    .writable_store()?
                                    .record_delivery(
                                        &task_id,
                                        delivery,
                                        &chrono::Utc::now().to_rfc3339(),
                                    )
                                    .map_err(|e| e.to_string())?;
                            }
                            Ok(())
                        })
                        .map_err(|e| e.to_string())?
                }));
            }
            TaskMutationAction::ResumeConversation { task_id, session } => {
                let task = self
                    .host
                    .store()
                    .as_ref()
                    .and_then(|s| s.get(&task_id))
                    .cloned()
                    .ok_or("Task is no longer in the task store")?;
                self.writable_store()?
                    .upsert_session(&task_id, *session.clone(), &now)
                    .map_err(|e| e.to_string())?;
                let Some(conversation) = &session.conversation else {
                    return Err("This Claude session has no recorded history — it never received a message. Launch a new session instead.".into());
                };
                let mut command =
                    gitterm_core::task_host::conversation_resume_command(conversation);
                if conversation.backend == gitterm_core::tasks::HarnessConversationBackend::Codex {
                    command = gitterm_core::task_mcp::configure_codex_notify(
                        &command,
                        &self
                            .servers
                            .as_ref()
                            .ok_or(SHUT_DOWN)?
                            .task_mcp
                            .notify_endpoint(),
                        &task_id,
                        &session.task_session_id,
                    );
                }
                let path = task
                    .worktree
                    .path
                    .ok_or("This task has no prepared worktree")?;
                let tab: gitterm_core::workspace_config::WorkspaceTabConfig =
                    serde_json::from_value(serde_json::json!({
                        "dir": path, "task_id": task_id,
                        "task_session_id": session.task_session_id,
                        "chat_session_id": conversation.session_id, "startup_command": command,
                    }))
                    .map_err(|e| e.to_string())?;
                let launched = self.open_live_terminal(&task.workspace.name, &tab, false, cx)?;
                self.mark_panel_resume_live(&session.task_session_id, cx);
                cx.emit(launched);
            }
            _ => return Err("Unsupported task panel effect".into()),
        }
        cx.notify();
        Ok(Task::ready(Ok(())))
    }

    fn stop_panel_unadopted(&mut self, target: &session::Target, cx: &mut Context<Self>) -> bool {
        let Some(session) = target.task_session_id.as_deref() else {
            return false;
        };
        if !self.terminals.contains_key(session)
            || !self
                .terminal_session_states()
                .iter()
                .any(|s| s.target == *target)
        {
            return false;
        }
        self.session_closed(session, cx);
        true
    }

    fn panel_worktree_removed(&mut self, task_id: &str) -> Result<(), String> {
        let action = self.reduce(
            TaskMutation::WorktreeRemoved {
                task_id: task_id.into(),
            },
            &Default::default(),
        )?;
        self.panel_authorize(&action)?;
        self.writable_store()?
            .clear_worktree(task_id, &chrono::Utc::now().to_rfc3339())
            .map_err(|e| e.to_string())
    }

    fn stop_panel_tabs(
        &mut self,
        task_id: String,
        targets: Vec<session::Target>,
        cx: &mut Context<Self>,
    ) -> Task<Result<(), String>> {
        let (send, receive) = tokio::sync::oneshot::channel();
        cx.emit(StopTask {
            task_id,
            targets,
            completion: Arc::new(std::sync::Mutex::new(Some(send))),
        });
        cx.spawn(async move |_, _| {
            receive.await.unwrap_or_else(|_| {
                Err("The shell closed before the task's tabs could be stopped".into())
            })
        })
    }

    pub(crate) fn panel_stop_completed(
        &mut self,
        task_id: &str,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if !task_id.is_empty()
            && let Some((target, detail)) =
                self.task_session_outcome(task_id, Some((TaskLifecycle::Stopped, None)), cx)
        {
            // A new session may have started while shell persistence was pending.
            self.writable_store()?
                .record_lifecycle_signal(task_id, target, detail, &chrono::Utc::now().to_rfc3339())
                .map_err(|e| e.to_string())?;
            if target == TaskLifecycle::Stopped {
                self.promote_queued_tasks(cx);
            }
            cx.notify();
        }
        Ok(())
    }

    fn panel_gh_account(&self, task_id: &str) -> Result<Option<String>, String> {
        let task = self
            .host
            .store()
            .as_ref()
            .and_then(|s| s.get(task_id))
            .ok_or("Task is no longer in the task store")?;
        let path = self.process_root().join("workspaces.json");
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(format!(
                    "read task workspace environment {}: {e}",
                    path.display()
                ));
            }
        };
        let mut file: gitterm_core::workspace_config::WorkspacesFile =
            serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        file.normalize_repos();
        let main = (task
            .repository
            .common_dir
            .file_name()
            .is_some_and(|n| n == ".git"))
        .then(|| task.repository.common_dir.parent().map(Path::to_path_buf))
        .flatten();
        let workspace = file.workspaces.iter().find(|workspace| {
            let gitterm_core::tasks::WorkspaceLocationIdentity::Local { directory } =
                &task.workspace.location
            else {
                return false;
            };
            let directory = gitterm_core::task_host::canonical_task_workspace_directory(
                self.host.store().as_ref(),
                directory,
            );
            let root = match &workspace.location {
                Some(gitterm_core::remote::config::WorkspaceLocationConfig::Local { root }) => {
                    Path::new(root)
                }
                Some(gitterm_core::remote::config::WorkspaceLocationConfig::RemoteAgent {
                    ..
                }) => return false,
                None if workspace.is_legacy_remote() => return false,
                None => Path::new(workspace.home_dir()),
            };
            let same = |path: &Path| {
                directory == std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
            };
            same(root) || same(Path::new(&workspace.dir))
        });
        let Some(workspace) = workspace else {
            return Ok(None);
        };
        Ok(file
            .workspace_env(&workspace.name, main.as_deref())
            .remove(gitterm_core::gh_identity::GH_ACCOUNT_ENV_KEY)
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty()))
    }
}
