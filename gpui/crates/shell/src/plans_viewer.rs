//! Iced's dedicated Plans Viewer, hosted by the existing retained WebKit surface.
//! Only transport differs: a captured, read-only route response replaces localhost
//! fetches. All rendering, TOC, collapsing and FileReader drop stay in the asset.
use crate::{Shell, WorkspaceId, content, theme::Palette};
use gitterm_core::plans::{self, DocumentSource};
use gpui::*;
use gpui_component::button::{Button, ButtonVariants as _};
use std::path::{Path, PathBuf};

const VIEWER_HTML: &str = include_str!("../../../../assets/plans-viewer.html");
const SURFACE: &str = "plans-viewer-document";
// WebKit owns keyboard focus after a document click, so route Escape back to
// the shell explicitly. This bridge has no file, shell or application actions.
const ESCAPE_SCRIPT: &str = r#"<script>
document.addEventListener('keydown', e => {
  if (e.key === 'Escape') window.webkit?.messageHandlers?.gittermViewer?.postMessage('close');
});
</script>"#;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Request {
    workspace: WorkspaceId,
    session: Option<crate::data::SessionId>,
    home: PathBuf,
    source: DocumentSource,
    name: Option<String>,
    dark: bool,
}

impl Request {
    fn root(&self) -> PathBuf {
        let (plans, docs) = gitterm_core::workspace_config::document_roots(&self.home);
        match self.source {
            DocumentSource::Plans => plans,
            DocumentSource::Docs => docs,
        }
    }

    fn document(&self) -> content::Document {
        let root = self.root();
        let (markdown, error) = match self.name.as_deref() {
            None => (None, None),
            Some(name) => {
                let read = || -> Result<String, String> {
                    if !plans::is_safe_relative_md_path(name) {
                        return Err("invalid document path".into());
                    }
                    let path = plans::resolve_contained(&root, Path::new(name))
                        .map_err(|error| error.to_string())?;
                    let bytes = std::fs::read(&path)
                        .map_err(|error| format!("Read {}: {error}", path.display()))?;
                    // fetch(...).text() decodes UTF-8 with replacement and removes a BOM.
                    let text = String::from_utf8_lossy(&bytes);
                    Ok(text.strip_prefix('\u{feff}').unwrap_or(&text).to_owned())
                };
                match read() {
                    Ok(markdown) => (Some(markdown), None),
                    Err(error) => (None, Some(error)),
                }
            }
        };
        let mut query = format!(
            "theme={}&nav=hide",
            if self.dark { "dark" } else { "light" }
        );
        if let Some(name) = &self.name {
            // Encode the selected path without treating &, # or + as query syntax.
            query.push_str("&plan=");
            for byte in name.bytes() {
                use std::fmt::Write as _;
                write!(query, "%{byte:02X}").unwrap();
            }
        }
        let data = serde_json::json!({
            "query": query,
            "route": format!("/{}", self.source.route_segment()),
            "plans": plans::list_documents(&root).plans,
            "name": self.name,
            "markdown": markdown,
            "error": error,
        });
        // JSON script data is inert even when a Markdown file contains </script>.
        let data = data.to_string().replace('<', "\\u003c");
        let html = VIEWER_HTML.replacen(
            "<script type=\"module\">",
            &format!("<script id=\"gitterm-plans-data\" type=\"application/json\">{data}</script>\n<script type=\"module\">"),
            1,
        ).replacen(
            gitterm_core::content::bar_hint::BAR_HINT_SLOT,
            &gitterm_core::content::bar_hint::bar_hint_script("var(--text)", "var(--bg)"),
            1,
        );
        content::Document::plans_viewer(html.replacen(
            "</body>",
            &format!("{ESCAPE_SCRIPT}</body>"),
            1,
        ))
    }
}

impl Shell {
    fn plans_home(&self, workspace: WorkspaceId, cx: &App) -> Option<PathBuf> {
        self.owner
            .read(cx)
            .iced_workspace(workspace)
            .map(|workspace| PathBuf::from(&workspace.dir))
            .or_else(|| {
                self.owner
                    .read(cx)
                    .model
                    .workspaces
                    .iter()
                    .find(|local| local.id == workspace)
                    .map(|local| local.root.clone())
            })
    }

    pub(crate) fn toggle_plans(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let closing = self.plans_open;
        if closing {
            self.plans_open = false;
            self.plans_content
                .update(cx, |content, _| content.close(SURFACE));
            self.plans_request = None;
        } else if self.active_workspace(cx).is_some() {
            if self.usage_open {
                self.toggle_usage(window, cx);
            }
            self.plans_return_focus = window.focused(cx);
            self.plans_open = true;
            self.focus.focus(window, cx);
            self.sync_plans_viewer(window, cx);
        }
        self.content.update(cx, |content, cx| {
            content.picker = self.plans_open || self.usage_open || self.browser.evidence_open;
            content.sync_covers(window, cx);
        });
        // The underlying content cover may restore its own focus; the explicit
        // Plans return target takes precedence after that cover has closed.
        if closing && let Some(focus) = self.plans_return_focus.take() {
            focus.focus(window, cx);
        }
        cx.notify();
    }

    /// Work's Documents rows open Plans/Docs; Files retains its ordinary viewer.
    pub(crate) fn open_work_document(
        &mut self,
        workspace: WorkspaceId,
        path: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let request = self.plans_home(workspace, cx).and_then(|home| {
            let (plans, docs) = gitterm_core::workspace_config::document_roots(&home);
            [(DocumentSource::Plans, plans), (DocumentSource::Docs, docs)]
                .into_iter()
                .find_map(|(source, root)| {
                    let name = path.strip_prefix(root).ok()?.to_str()?.replace('\\', "/");
                    plans::is_safe_relative_md_path(&name).then(|| Request {
                        workspace,
                        session: self
                            .workspaces
                            .iter()
                            .find(|state| state.workspace.id == workspace)
                            .and_then(|state| state.active_session),
                        home: home.clone(),
                        source,
                        name: Some(name),
                        dark: gpui_component::Theme::global(cx).mode.is_dark(),
                    })
                })
        });
        if let Some(request) = request {
            if self.usage_open {
                self.toggle_usage(window, cx);
            }
            if !self.plans_open {
                self.plans_return_focus = window.focused(cx);
            }
            self.plans_open = true;
            self.load_plans(request, window, cx);
            self.focus.focus(window, cx);
            cx.notify();
        } else {
            self.owner.update(cx, |owner, cx| {
                owner.open_file(workspace, path.to_owned(), cx)
            });
        }
    }

    fn load_plans(&mut self, request: Request, window: &mut Window, cx: &mut Context<Self>) {
        let origin = content::Origin {
            workspace: request.workspace.0,
            pane: 0,
            source: request.root(),
        };
        self.plans_request = Some(request.clone());
        self.plans_content.update(cx, |content, cx| {
            content.open_prepared(
                SURFACE.into(),
                origin,
                move || Ok(request.document()),
                window,
                cx,
            );
            content.select(Some(SURFACE.into()));
        });
    }

    pub(crate) fn sync_plans_viewer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.plans_open {
            return;
        }
        let Some(workspace) = self.active_workspace(cx) else {
            return;
        };
        // Iced's WorkspaceSelect/TabSelect presents the new tab's surfaces,
        // dismissing the standalone Plans viewer rather than carrying it over.
        let session = self.active(cx).active_session;
        if self
            .plans_request
            .as_ref()
            .is_some_and(|request| request.workspace != workspace || request.session != session)
        {
            self.plans_return_focus = None;
            self.toggle_plans(window, cx);
            self.focus_main_terminal(window, cx);
            return;
        }
        let Some(home) = self.plans_home(workspace, cx) else {
            return;
        };
        let dark = gpui_component::Theme::global(cx).mode.is_dark();
        let request = match self.plans_request.clone() {
            Some(mut request) if request.workspace == workspace && request.home == home => {
                request.dark = dark;
                request
            }
            _ => Request {
                workspace,
                session,
                home,
                source: DocumentSource::Plans,
                name: None,
                dark,
            },
        };
        if self.plans_request.as_ref() != Some(&request) {
            self.load_plans(request, window, cx);
        }
        self.plans_content.update(cx, |content, cx| {
            content.set_focus_target(self.focus.clone(), self.focus.clone());
            content.picker = self.usage_open
                || self.browser.evidence_open
                || self.task_switcher.is_some()
                || self.session_picker.is_some()
                || self.park_sheet.is_some()
                || self.new_work_sheet.is_some()
                || self.settings_sheet.is_some()
                || self.phone_panel().is_some()
                || self.task_panel_modal_open(cx);
            content.sync_covers(window, cx);
        });
    }

    pub(crate) fn plans_viewer(&self, cx: &Context<Self>) -> gpui::Stateful<gpui::Div> {
        let p = Palette::current(cx);
        div()
            .id("plans-viewer")
            .debug_selector(|| "plans-viewer".into())
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .bg(rgb(p.base))
            .child(
                div()
                    .h(px(40.))
                    .px_2()
                    .flex()
                    .items_center()
                    .gap_2()
                    .bg(rgb(p.overlay))
                    .child("Plans Viewer")
                    .child(div().flex_1())
                    .child(div().text_color(rgb(p.subtext0)).child("Esc: close"))
                    .child(
                        Button::new("close-plans")
                            .ghost()
                            .label("Back to Terminal")
                            .on_click(
                                cx.listener(|this, _, window, cx| this.toggle_plans(window, cx)),
                            ),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .child(content::window::ContentWindow::body(
                        &self.plans_content,
                        SURFACE,
                        cx,
                    )),
            )
    }
}

#[cfg(test)]
mod tests;
