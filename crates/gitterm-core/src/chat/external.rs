//! Live-only display contracts for the shared outside-tool watcher.
use serde::Serialize;

/// What a thread did last, as one line for the page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Activity {
    pub text: String,
    /// When the rollout line was written (Unix ms), if it says.
    pub at_ms: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Running,
    Finished,
}

/// A Codex subagent of an exec run, as the page shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SubagentView {
    pub thread_id: String,
    pub nickname: Option<String>,
    pub agent_path: Option<String>,
    pub depth: Option<u64>,
    /// `running`, or `done` once its turn completed or the run ended.
    pub state: &'static str,
    pub activity: Option<Activity>,
    pub rollout_path: String,
}

/// An exec run, as the page shows it (`{"kind":"codex_exec","run":…}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunView {
    /// `<pid>-<start microseconds>`: a pid reused later is another run.
    pub key: String,
    pub pid: u32,
    pub state: RunState,
    pub started_at_ms: i64,
    /// When GitTerm saw the process gone (within one poll of its exit).
    pub ended_at_ms: Option<i64>,
    /// Where it works: `-C`, else the rollout's `cwd`.
    pub cwd: Option<String>,
    /// The last component of `cwd`.
    pub checkout: Option<String>,
    pub add_dirs: Vec<String>,
    pub sandbox: Option<String>,
    pub model: Option<String>,
    pub rollout_path: Option<String>,
    pub thread_id: Option<String>,
    pub cli_version: Option<String>,
    pub activity: Option<Activity>,
    pub subagents: Vec<SubagentView>,
}
