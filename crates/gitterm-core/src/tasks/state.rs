//! Single task-store authority. Mutation adapters borrow this same store until
//! the preparation/launch reducers move in the next task integration slice.

use super::TaskStore;
use crate::task_commands::{TaskProjection, TaskRead, TaskReadCommand, TaskReadEffect};

#[derive(Debug, Default)]
pub struct TaskAuthority {
    store: Option<TaskStore>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_commands::runtime::command_channel;
    use crate::task_commands::ReplyId;

    fn reply_id() -> ReplyId {
        let (sender, mut receiver) = command_channel();
        let _pending = sender.enqueue((), None).unwrap();
        receiver.try_recv().unwrap().reply.id()
    }

    #[test]
    fn unavailable_store_keeps_list_get_error_asymmetry_and_reply_identity() {
        let state = TaskAuthority::default();
        let id = reply_id();
        for (query, error) in [
            (
                TaskRead::List {
                    include_archived: false,
                },
                "GitTerm's task store is unavailable",
            ),
            (
                TaskRead::Get {
                    task_id: " absent ".into(),
                },
                "Task  absent  is not available",
            ),
        ] {
            let effect = state.reduce(TaskReadCommand { reply: id, query });
            assert_eq!(effect.reply, id);
            assert_eq!(effect.projection.err().unwrap(), error);
        }
    }

    #[test]
    fn reads_preserve_order_filter_archived_and_see_adapter_writes_without_saving() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tasks.json");
        std::fs::write(
            &path,
            include_bytes!("../../../../tests/fixtures/stage45/tasks/old.tasks.json"),
        )
        .unwrap();
        let mut state = TaskAuthority::from(Some(TaskStore::load(&path).unwrap()));
        let mut archived = state.store().as_ref().unwrap().tasks()[0].clone();
        archived.task_id = "archived".into();
        archived.archived_at = Some("t1".into());
        archived.lifecycle = crate::tasks::TaskLifecycle::Archived;
        state
            .store_mut()
            .as_mut()
            .unwrap()
            .insert(archived)
            .unwrap();
        let before = std::fs::read(&path).unwrap();
        let id = reply_id();
        for (include_archived, expected) in [
            (false, vec!["fixture-task"]),
            (true, vec!["fixture-task", "archived"]),
        ] {
            let effect = state.reduce(TaskReadCommand {
                reply: id,
                query: TaskRead::List { include_archived },
            });
            let TaskProjection::List(tasks) = effect.projection.unwrap() else {
                panic!("expected list")
            };
            assert_eq!(
                tasks
                    .iter()
                    .map(|task| task.task_id.as_str())
                    .collect::<Vec<_>>(),
                expected
            );
        }
        let effect = state.reduce(TaskReadCommand {
            reply: id,
            query: TaskRead::Get {
                task_id: "archived".into(),
            },
        });
        let TaskProjection::Get(task) = effect.projection.unwrap() else {
            panic!("expected task")
        };
        assert!(task.archived_at.is_some());
        assert_eq!(std::fs::read(path).unwrap(), before);
    }
}

impl From<Option<TaskStore>> for TaskAuthority {
    fn from(store: Option<TaskStore>) -> Self {
        Self { store }
    }
}

impl TaskAuthority {
    /// Transitional Iced/delegation seam. No second writable store is loaded.
    pub fn store(&self) -> &Option<TaskStore> {
        &self.store
    }

    pub fn store_mut(&mut self) -> &mut Option<TaskStore> {
        &mut self.store
    }

    pub fn reduce(&self, command: TaskReadCommand) -> TaskReadEffect {
        let projection = match command.query {
            TaskRead::List { include_archived } => self
                .store
                .as_ref()
                .ok_or_else(|| "GitTerm's task store is unavailable".to_string())
                .map(|store| {
                    TaskProjection::List(
                        store
                            .tasks()
                            .iter()
                            .filter(|task| include_archived || task.archived_at.is_none())
                            .cloned()
                            .collect(),
                    )
                }),
            TaskRead::Get { task_id } => self
                .store
                .as_ref()
                .and_then(|store| store.get(&task_id))
                .cloned()
                .map(Box::new)
                .map(TaskProjection::Get)
                .ok_or_else(|| format!("Task {task_id} is not available")),
        };
        TaskReadEffect {
            reply: command.reply,
            projection,
        }
    }
}
