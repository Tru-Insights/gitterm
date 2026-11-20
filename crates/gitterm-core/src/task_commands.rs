//! Task command/effect contracts. Transport handles never enter reducer state.

use crate::tasks::TaskRecord;

/// Process-local correlation only; never persisted or sent on the MCP wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ReplyId(uuid::Uuid);

#[derive(Debug, Clone)]
pub enum TaskRead {
    List { include_archived: bool },
    Get { task_id: String },
}

#[derive(Debug, Clone)]
pub struct TaskReadCommand {
    pub reply: ReplyId,
    pub query: TaskRead,
}

#[derive(Debug)]
pub enum TaskProjection {
    List(Vec<TaskRecord>),
    Get(Box<TaskRecord>),
}

/// The host adds live session/preset facts synchronously before replying.
#[derive(Debug)]
pub struct TaskReadEffect {
    pub reply: ReplyId,
    pub projection: Result<TaskProjection, String>,
}

#[cfg(feature = "runtime")]
pub mod runtime;
