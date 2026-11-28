//! Iced's five-second outside-tool poll, using its shared watcher unchanged.
use super::*;
use gitterm_core::{
    chat::types::AgentBackend,
    codex_exec::{Libproc, Root, Watcher},
};
use std::time::Duration;

impl ChatPane {
    pub(super) fn watch_external_runs(&mut self, cx: &mut Context<Self>) {
        if self._external_watch.is_some()
            || !self.controller.live
            || self.controller.remote
            || self.controller.config.backend() != AgentBackend::Claude
        {
            return;
        }
        self._external_watch = Some(cx.spawn(async move |pane, cx| {
            let watcher = Arc::new(std::sync::Mutex::new(Watcher::default()));
            loop {
                cx.background_executor().timer(Duration::from_secs(5)).await;
                let Ok((tab_id, pid)) = pane.update(cx, |pane, _| {
                    (
                        pane.target().tab_id,
                        pane.controller
                            .runtime
                            .as_ref()
                            .and_then(|runtime| runtime.local_pid()),
                    )
                }) else {
                    break;
                };
                let watcher = watcher.clone();
                let runs = cx
                    .background_executor()
                    .spawn(async move {
                        let roots = match pid {
                            Some(pid) => match Root::capture(tab_id, pid) {
                                Ok(root) => root.into_iter().collect::<Vec<_>>(),
                                Err(error) => {
                                    eprintln!("[codex-exec] capturing chat {tab_id}: {error}");
                                    vec![]
                                }
                            },
                            None => vec![],
                        };
                        let mut watcher = watcher.lock().unwrap_or_else(|e| e.into_inner());
                        watcher
                            .poll(&Libproc, &roots, now_ms())
                            .into_iter()
                            .filter_map(|(tab, run)| (tab == tab_id).then_some(run))
                            .collect::<Vec<_>>()
                    })
                    .await;
                if pane
                    .update(cx, |pane, cx| {
                        pane.view
                            .update(cx, |view, cx| view.set_external_runs(runs, cx))
                    })
                    .is_err()
                {
                    break;
                }
            }
        }));
    }
}
