//! Runtime identities belong to panes, independently of saved layout IDs.
use gitterm_core::session::{self, SavedIdentity, Target};
use std::sync::atomic::{AtomicUsize, Ordering};

pub(crate) struct Removed(pub Target);

pub(crate) fn subscribe(
    pane: &gpui::Entity<crate::pane::Pane>,
    window: &mut gpui::Window,
    cx: &mut gpui::Context<crate::Shell>,
) -> gpui::Subscription {
    cx.subscribe_in(pane, window, |shell, pane, event: &Removed, window, cx| {
        let missing = shell.workspaces.iter().any(|state| {
            state.store.terminals.is_some()
                && state.sessions.iter().any(|session| {
                    state.panes.get(&crate::session_pane_id(session.id)) == Some(pane)
                })
                && state
                    .dock
                    .read(cx)
                    .panel(gpui_component::dock::PanelId::from(pane.entity_id()))
                    .is_none()
        });
        if missing {
            shell.close_target(&event.0, window, cx);
        }
    })
}

pub(crate) fn fresh_target() -> Target {
    static NEXT: AtomicUsize = AtomicUsize::new(1);
    let tab_id = NEXT.fetch_add(1, Ordering::Relaxed);
    let identity = SavedIdentity::default()
        .open(|| format!("gpui-{}-{tab_id}", std::process::id()))
        .identity;
    Target {
        tab_id,
        session_uid: identity.session_uid,
        task_id: identity.task_id,
        task_session_id: identity.task_session_id,
    }
}

pub(crate) fn can_close(current: &Target, requested: &Target, last: bool) -> bool {
    let state = session::State {
        target: current.clone(),
        control: session::TurnControl::Terminal,
        task_live: false,
        last_tab: last,
    };
    matches!(
        state.reduce(session::Intent::Close {
            target: requested.clone(),
            confirmed: false,
        }),
        session::Effect::Close(_)
    )
}

impl crate::Shell {
    pub(crate) fn close_target(
        &mut self,
        requested: &Target,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(state) = self.workspaces.iter_mut().find(|state| {
            state
                .panes
                .values()
                .any(|pane| pane.read(cx).session.as_ref() == Some(requested))
        }) else {
            return;
        };
        let Some(id) = state.sessions.iter().find_map(|session| {
            let pane = state.panes.get(&crate::session_pane_id(session.id))?;
            (pane.read(cx).session.as_ref() == Some(requested)).then_some(session.id)
        }) else {
            return;
        };
        let pane_id = crate::session_pane_id(id);
        let pane = &state.panes[&pane_id];
        if !can_close(
            pane.read(cx).session.as_ref().unwrap(),
            requested,
            state.sessions.len() + usize::from(state.store.terminals.is_some()) <= 1,
        ) {
            return;
        }
        pane.update(cx, |pane, cx| pane.stop_target(requested, cx));
        self.owner.update(cx, |owner, _| {
            owner.chats.remove(&requested.session_uid);
        });
        let pane = state.panes.remove(&pane_id).unwrap();
        self.registry.borrow_mut().remove(&pane_id);
        if let Some(terminals) = &mut state.store.terminals {
            terminals.retain(|terminal| terminal.id != id.0);
        }
        state.sessions.retain(|session| session.id != id);
        if state.active_session == Some(id) {
            state.active_session = state.sessions.first().map(|session| session.id);
        }
        state
            .dock
            .update(cx, |dock, cx| dock.remove_panel(pane, window, cx));
        crate::save_layout(state, cx);
        self.focus.focus(window, cx);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn runtime_replacement_and_every_stale_binding_are_rejected() {
        let current = fresh_target();
        assert!(can_close(&current, &current, false));
        assert!(!can_close(&current, &current, true));
        assert!(!can_close(&fresh_target(), &current, false));
        for field in 0..4 {
            let mut stale = current.clone();
            match field {
                0 => stale.tab_id += 1,
                1 => stale.session_uid.push('x'),
                2 => stale.task_id = Some("fixture".into()),
                _ => stale.task_session_id = Some("fixture".into()),
            }
            assert!(!can_close(&current, &stale, false));
        }
    }
}
