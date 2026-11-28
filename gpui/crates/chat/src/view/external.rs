//! Live-only outside-tool cards; deliberately absent from saved conversation.
use super::*;
use gitterm_core::chat::external::{RunState, RunView};

impl ChatView {
    pub fn set_external_runs(&mut self, runs: Vec<RunView>, cx: &mut Context<Self>) {
        let count = self.model.rows.len();
        let following = self.scroller.read(cx).is_following_tail();
        let mut changed = Vec::new();
        for run in runs {
            self.external_runs.insert(run.key.clone(), run.clone());
            changed.extend(self.model.upsert_codex_exec(&run));
        }
        self.refresh_external_roster();
        self.sync_activity_tick(cx);
        self.refresh_rows(count, following, changed, cx);
    }
    pub(super) fn refresh_external_roster(&mut self) {
        self.roster.retain(|row| !row.id.starts_with("cx:"));
        for run in self.external_runs.values() {
            let running = run.state == RunState::Running;
            let id = format!("cx:{}", run.key);
            self.roster.push(roster::RosterEntry {
                id: id.clone(),
                name: "Codex (exec)".into(),
                model: run.model.clone().unwrap_or_default(),
                checkout: run.checkout.clone().unwrap_or_default(),
                activity: format!(
                    "{}{}{}",
                    if running { "running" } else { "finished" },
                    run.checkout
                        .as_ref()
                        .map(|checkout| format!(" · in {checkout}"))
                        .unwrap_or_default(),
                    run.activity
                        .as_ref()
                        .map(|a| format!(" — {}", a.text))
                        .unwrap_or_default()
                ),
                state: if running {
                    roster::WorkerState::Running
                } else {
                    roster::WorkerState::Done
                },
                started_at_ms: u64::try_from(run.started_at_ms).ok(),
                ended_at_ms: run.ended_at_ms.and_then(|at| u64::try_from(at).ok()),
            });
            for sub in &run.subagents {
                self.roster.push(roster::RosterEntry {
                    id: format!("{id}:{}", sub.thread_id),
                    name: format!(
                        "↳ {}",
                        sub.nickname
                            .as_ref()
                            .or(sub.agent_path.as_ref())
                            .map(String::as_str)
                            .unwrap_or("subagent")
                    ),
                    model: String::new(),
                    checkout: String::new(),
                    activity: format!(
                        "{}{}{}",
                        if sub.nickname.is_some() {
                            sub.agent_path
                                .as_ref()
                                .map(|path| format!("{path} · "))
                                .unwrap_or_default()
                        } else {
                            String::new()
                        },
                        sub.state,
                        sub.activity
                            .as_ref()
                            .map(|a| format!(" — {}", a.text))
                            .unwrap_or_default()
                    ),
                    state: if sub.state == "running" {
                        roster::WorkerState::Running
                    } else {
                        roster::WorkerState::Done
                    },
                    started_at_ms: None,
                    ended_at_ms: None,
                });
            }
        }
    }
}

pub(super) fn display_title(row: &Row) -> String {
    let Some(run) = &row.external_run else {
        return row.title.clone();
    };
    let end = run
        .ended_at_ms
        .and_then(|at| u64::try_from(at).ok())
        .unwrap_or_else(crate::composer::now_ms);
    let duration = end.saturating_sub(u64::try_from(run.started_at_ms).unwrap_or(0));
    let state = if run.state == RunState::Running {
        let seconds = duration / 1000;
        format!("running {}:{:02}", seconds / 60, seconds % 60)
    } else {
        format!("finished · {}", crate::composer::elapsed(duration))
    };
    crate::model::codex_exec_title(run, &state)
}
