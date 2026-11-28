//! Viewer actions: clipboard reads run off-thread; external opens use the platform.
use crate::Shell;
use gpui::{App, ClipboardItem, Context, Window};
use std::path::{Path, PathBuf};

/// Both Iced routes, carried through the normal window action tree.
#[derive(Clone, Debug, PartialEq, Eq, gpui::Action)]
#[action(namespace = gitterm_shell, no_json)]
pub(crate) struct OpenEditor {
    pub path: PathBuf,
    pub line: Option<usize>,
    pub terminal: bool,
}

impl OpenEditor {
    pub(crate) fn configured(path: PathBuf, line: Option<usize>) -> Self {
        Self {
            path,
            line,
            terminal: false,
        }
    }
    pub(crate) fn terminal(path: PathBuf) -> Self {
        Self {
            path,
            line: None,
            terminal: true,
        }
    }
}

pub(super) fn is_rendered(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "md" | "markdown" | "html" | "htm" | "excalidraw"
            )
        })
}

/// Paths handed to the platform opener, recorded instead under test so no
/// browser is ever launched.
#[cfg(test)]
pub(crate) static OPENED: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

pub(super) fn open_file(path: &Path, cx: &App) -> Result<(), String> {
    #[cfg(test)]
    {
        _ = cx;
        OPENED.lock().unwrap().push(path.to_path_buf());
        Ok(())
    }
    #[cfg(not(test))]
    open_platform(path, cx)
}

#[cfg(not(test))]
fn open_platform(path: &Path, cx: &App) -> Result<(), String> {
    use objc2_foundation::{NSString, NSURL};
    let path = path
        .to_str()
        .ok_or_else(|| format!("Open {}: invalid path", path.display()))?;
    let url = NSURL::fileURLWithPath(&NSString::from_str(path));
    let url = url
        .absoluteString()
        .ok_or_else(|| format!("Open {path}: invalid file URL"))?;
    cx.open_url(&url.to_string());
    Ok(())
}

#[cfg(test)]
thread_local! {
    /// Tests redirect the browser page into their own temp dir.
    pub(crate) static TEST_OUT_DIR: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Where the browser page is written: the temp dir (tests: their own dir).
fn browser_out_dir() -> PathBuf {
    #[cfg(test)]
    if let Some(dir) = TEST_OUT_DIR.with(|dir| dir.borrow().clone()) {
        return dir;
    }
    std::env::temp_dir()
}

/// The page Iced opens in the browser, written to a temp file: the rendered
/// HTML for markdown/HTML/Excalidraw, the content server's `/file/{id}` page
/// for anything else. The `gitterm-gpui-` prefix never collides with Iced's.
/// As in Iced: rendered content missing opens the file itself; an empty raw
/// file opens nothing (`Ok(None)`).
pub(super) fn browser_page(
    root: &Path,
    relative: &Path,
    dark: bool,
    out_dir: &Path,
) -> Result<Option<PathBuf>, String> {
    let path = root.join(relative);
    let load = gitterm_core::content::collect_contained_file_load(0, root, relative, dark)
        .map_err(|e| format!("Open {} in browser: {e}", path.display()))?;
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "preview".to_string());
    let (html, suffix) = if is_rendered(relative) {
        let Some(html) = load.webview_content else {
            return Ok(Some(path));
        };
        (html, "preview")
    } else {
        if load.file_content.is_empty() {
            return Ok(None);
        }
        let html =
            gitterm_core::file_page::file_page_html(&path.to_string_lossy(), &load.file_content);
        (html, "file")
    };
    let temp = out_dir.join(format!("gitterm-gpui-{stem}_{suffix}.html"));
    std::fs::write(&temp, html).map_err(|e| format!("Write {}: {e}", temp.display()))?;
    Ok(Some(temp))
}

impl Shell {
    pub(crate) fn open_editor(
        &mut self,
        request: OpenEditor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.active_workspace(cx) else {
            return;
        };
        if !self.local || self.data.terminal_cwd(workspace, cx).is_none() {
            return;
        }
        #[cfg(test)]
        {
            let _ = window;
            self.editor_requests.push(request);
        }
        #[cfg(not(test))]
        if request.terminal {
            let cwd = self
                .active(cx)
                .active_session
                .and_then(|id| self.active(cx).panes.get(&crate::session_pane_id(id)))
                .and_then(|pane| pane.read(cx).task_folder(cx))
                .or_else(|| self.data.terminal_cwd(workspace, cx));
            let command = gitterm_core::editor::terminal_command(
                std::env::var("EDITOR").ok().as_deref(),
                &request.path,
            );
            self.new_terminal_launch(
                gpui_component::dock::DockPlacement::Center,
                cwd.clone(),
                Some(crate::PaneStart::Launch(gitterm_terminal::TerminalLaunch {
                    cwd,
                    startup_command: Some(command),
                    ..Default::default()
                })),
                window,
                cx,
            );
        } else {
            let setting = self.editor.clone();
            let built =
                gitterm_core::editor::build_open_command(&setting, &request.path, request.line);
            let executor = self.editor_executor.clone();
            cx.spawn(async move |this, cx| {
                let result = cx
                    .background_executor()
                    .spawn(async move {
                        let result = match built {
                            Ok(editor) => executor
                                .block_on(gitterm_core::editor::launch_command(editor.command)),
                            Err(error) => Err(error),
                        };
                        result.map_err(|error| gitterm_core::editor::open_error(&setting, &error))
                    })
                    .await;
                _ = this.update(cx, |this, cx| {
                    this.owner.update(cx, |owner, cx| {
                        owner.error = result.err();
                        cx.notify();
                    });
                });
            })
            .detach();
        }
    }

    pub(crate) fn edit_selected_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(workspace) = self.active_workspace(cx) else {
            return;
        };
        let path = self
            .content
            .read(cx)
            .visits
            .get(&workspace)
            .and_then(|visit| self.active(cx).panes.get(&visit.pane))
            .and_then(|pane| match &pane.read(cx).content {
                crate::pane::PaneContent::Review(review) => review.read(cx).selected_path(),
                _ => None,
            })
            .or_else(|| {
                self.selected_files
                    .get(&workspace)
                    .map(|file| file.path.clone())
            });
        if let Some(path) = path {
            self.open_editor(OpenEditor::terminal(path), window, cx);
        }
    }

    pub(super) fn viewer_error(&self, error: String, cx: &mut Context<Self>) {
        self.owner.update(cx, |owner, cx| {
            owner.error = Some(error);
            cx.notify();
        });
    }

    pub(super) fn open_viewer_in_browser(
        &self,
        root: PathBuf,
        relative: PathBuf,
        dark: bool,
        cx: &mut Context<Self>,
    ) {
        let out_dir = browser_out_dir();
        let remote_html = self.content.read(cx).remote_html(&root, &relative);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    if let Some(html) = remote_html? {
                        let mut file = tempfile::Builder::new()
                            .prefix("gitterm-gpui-")
                            .suffix("-preview.html")
                            .tempfile_in(&out_dir)
                            .map_err(|error| {
                                format!("Create remote preview in {}: {error}", out_dir.display())
                            })?;
                        use std::io::Write as _;
                        file.write_all(html.as_bytes())
                            .map_err(|error| format!("Write remote preview: {error}"))?;
                        let (_, path) = file
                            .keep()
                            .map_err(|error| format!("Keep remote preview: {error}"))?;
                        Ok(Some(path))
                    } else {
                        browser_page(&root, &relative, dark, &out_dir)
                    }
                })
                .await;
            _ = this.update(cx, |this, cx| {
                if let Err(error) =
                    result.and_then(|page| page.map_or(Ok(()), |page| open_file(&page, cx)))
                {
                    this.viewer_error(error, cx);
                }
            });
        })
        .detach();
    }

    pub(super) fn copy_viewer_file(
        &self,
        root: PathBuf,
        relative: PathBuf,
        cx: &mut Context<Self>,
    ) {
        if let Some(text) = self
            .content
            .read(cx)
            .displayed_text(&root, &relative)
            .filter(|text| !text.is_empty())
        {
            cx.write_to_clipboard(ClipboardItem::new_string(text.to_owned()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_load_rejects_missing_invalid_text_and_paths_outside_root() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("outside"), "outside fixture").unwrap();
        std::os::unix::fs::symlink(outside.path().join("outside"), root.path().join("link"))
            .unwrap();
        assert!(super::super::Document::local_file(root.path(), Path::new("link"), true).is_err());
        std::fs::write(root.path().join("binary"), [0xff]).unwrap();
        assert!(
            super::super::Document::local_file(root.path(), Path::new("binary"), true).is_err()
        );
        assert!(
            super::super::Document::local_file(root.path(), Path::new("missing"), true)
                .unwrap_err()
                .contains("missing")
        );
    }
}
