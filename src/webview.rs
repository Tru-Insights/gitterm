// WebView module for embedded markdown/mermaid rendering and the agent chat UI.
//
// Independent child webviews ("surfaces") share the main window:
//
// - `WebviewSurface::Agent(tab_id)` is one agent tab's chat page, with its own
//   IPC handler. Each chat tab gets its own page, kept alive (hidden) while
//   another tab, a file or the plans viewer is shown, so switching back is a
//   show/hide with the DOM (scroll position, open tool cards, composer draft)
//   intact. main.rs caps how many pages live at once and destroys the least
//   recently shown one (`promote_agent_page`).
// - `WebviewSurface::Viewer` hosts markdown / HTML / excalidraw HTML and the
//   plans-viewer URL.
//
// The app keeps at most one surface visible at a time (see
// `visible_webview_surface` in main.rs).
//
// Note: Due to threading constraints (wry's WebView is not Send/Sync),
// the WebViews must be created and managed on the main thread.

use std::cell::RefCell;
use std::collections::HashMap;

type WebViewBounds = (f32, f32, f32, f32);

/// IPC handler closure type. Receives the JSON body posted from the webview via
/// `window.ipc.postMessage(jsonString)`. Must be `Send + 'static` because wry runs
/// the handler from the platform's web-process callback path.
pub type IpcHandler = Box<dyn Fn(String) + Send + 'static>;

/// Which child webview an operation targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebviewSurface {
    /// The chat page of the agent tab with this id; IPC handler installed at
    /// construction.
    Agent(usize),
    /// File viewer HTML (markdown / HTML / excalidraw) and the plans viewer URL.
    Viewer,
}

/// What kind of content is staged for the next webview create / reuse. The
/// markdown / excalidraw / agent surfaces stage HTML. The plans viewer stages
/// a URL pointing at the local warp server.
enum PendingContent {
    Html(String),
    Url(String),
}

use wry::raw_window_handle::{HasWindowHandle, WindowHandle};
use wry::{Rect, WebView, WebViewBuilder};

/// Per-surface state: the live webview (once created), content staged for the
/// next `try_create_with_window`, and the visibility the app last asked for.
/// Visibility is recorded even before the webview exists so a create that runs
/// after the app hid the surface doesn't pop it up.
struct SurfaceSlot {
    webview: Option<WebView>,
    pending: Option<(PendingContent, WebViewBounds)>,
    pending_ipc_handler: Option<IpcHandler>,
    visible: bool,
}

impl SurfaceSlot {
    const EMPTY: SurfaceSlot = SurfaceSlot {
        webview: None,
        pending: None,
        pending_ipc_handler: None,
        visible: false,
    };
}

// The agent pages share one map keyed by tab id; the Viewer has its own
// RefCell so an operation on it can never trip a borrow held by the pages.
thread_local! {
    static AGENT_PAGES: RefCell<HashMap<usize, SurfaceSlot>> = RefCell::new(HashMap::new());
    static VIEWER_SURFACE: RefCell<SurfaceSlot> = const { RefCell::new(SurfaceSlot::EMPTY) };
}

/// Run `f` on `surface`'s slot, creating an empty agent page slot on demand.
/// Only staging uses this; everything else goes through `with_existing_slot`
/// so a query or a script for a tab without a page never leaves a slot behind.
fn with_slot<R>(surface: WebviewSurface, f: impl FnOnce(&mut SurfaceSlot) -> R) -> R {
    match surface {
        WebviewSurface::Agent(tab_id) => AGENT_PAGES.with(|pages| {
            f(pages
                .borrow_mut()
                .entry(tab_id)
                .or_insert(SurfaceSlot::EMPTY))
        }),
        WebviewSurface::Viewer => VIEWER_SURFACE.with(|slot| f(&mut slot.borrow_mut())),
    }
}

/// Run `f` on `surface`'s slot if it has one (the Viewer always does).
fn with_existing_slot<R>(
    surface: WebviewSurface,
    f: impl FnOnce(&mut SurfaceSlot) -> R,
) -> Option<R> {
    match surface {
        WebviewSurface::Agent(tab_id) => {
            AGENT_PAGES.with(|pages| pages.borrow_mut().get_mut(&tab_id).map(f))
        }
        WebviewSurface::Viewer => VIEWER_SURFACE.with(|slot| Some(f(&mut slot.borrow_mut()))),
    }
}

fn logical_rect((x, y, width, height): WebViewBounds) -> Rect {
    Rect {
        position: wry::dpi::Position::Logical(wry::dpi::LogicalPosition::new(x as f64, y as f64)),
        size: wry::dpi::Size::Logical(wry::dpi::LogicalSize::new(width as f64, height as f64)),
    }
}

/// Wrapper that holds a raw window handle and implements HasWindowHandle
/// This allows us to work with trait objects from Iced
#[allow(dead_code)]
struct WindowHandleWrapper<'a> {
    handle: WindowHandle<'a>,
}

impl<'a> HasWindowHandle for WindowHandleWrapper<'a> {
    fn window_handle(&self) -> Result<WindowHandle<'_>, wry::raw_window_handle::HandleError> {
        // SAFETY: We're just re-wrapping the same handle
        Ok(unsafe { WindowHandle::borrow_raw(self.handle.as_raw()) })
    }
}

/// Store HTML content to be rendered on `surface` when we get window access. No
/// IPC handler is installed — appropriate for the markdown / excalidraw / HTML
/// viewers which only render content and don't post messages back to Rust.
#[allow(dead_code)]
pub fn set_pending_content(surface: WebviewSurface, html: String, bounds: (f32, f32, f32, f32)) {
    set_pending_content_with_ipc(surface, html, bounds, None);
}

/// Like `set_pending_content`, but also stages an optional IPC handler closure.
/// The handler will be installed on the webview at construction time and invoked
/// whenever JS calls `window.ipc.postMessage(jsonString)`. Use this for the agent
/// chat UI which needs a return path from JS to Rust (submit prompt, stop, etc.).
///
/// The handler is consumed (taken) by the next `try_create_with_window` call for
/// the same surface. If that surface's webview already exists, the handler is
/// dropped — wry doesn't support replacing an IPC handler post-construction,
/// which is why each chat page lives on its own surface.
#[allow(dead_code)]
pub fn set_pending_content_with_ipc(
    surface: WebviewSurface,
    html: String,
    bounds: (f32, f32, f32, f32),
    ipc_handler: Option<IpcHandler>,
) {
    with_slot(surface, |slot| {
        slot.pending = Some((PendingContent::Html(html), bounds));
        slot.pending_ipc_handler = ipc_handler;
        slot.visible = true;
    });
}

/// Stage a URL for the next `try_create_with_window` on `surface` to load. Used
/// by the plans viewer (and any other surface that wants to navigate the
/// embedded webview to a localhost route instead of injecting HTML). No IPC
/// handler; URL-loaded surfaces communicate only via HTTP back to the warp server.
#[allow(dead_code)]
pub fn set_pending_url(surface: WebviewSurface, url: String, bounds: (f32, f32, f32, f32)) {
    with_slot(surface, |slot| {
        slot.pending = Some((PendingContent::Url(url), bounds));
        slot.pending_ipc_handler = None;
        slot.visible = true;
    });
}

/// Navigate the live webview on `surface` to a URL. No-op if that surface has no
/// webview. Use this when the webview is already up (`is_active()` returns true)
/// and you want to swap its location without recreating the surface.
#[allow(dead_code)]
pub fn navigate_to_url(surface: WebviewSurface, url: &str) {
    with_existing_slot(surface, |slot| {
        if let Some(webview) = slot.webview.as_ref() {
            if let Err(e) = webview.load_url(url) {
                eprintln!("[webview] {surface:?} load_url({url}) failed: {e}");
            }
        }
    });
}

/// Try to create the WebView for `surface` with its pending content using the
/// given window. This should be called from the main thread with window access.
///
/// Returns `Ok(true)` when staged content was loaded (into a new or the existing
/// webview) and `Ok(false)` when nothing was staged — e.g. a second create task
/// for the same staging already consumed it, or the agent page was destroyed
/// before its create task ran.
#[allow(dead_code)]
pub fn try_create_with_window(
    surface: WebviewSurface,
    window: &dyn HasWindowHandle,
) -> Result<bool, String> {
    with_existing_slot(surface, |slot| {
        let Some((content, bounds)) = slot.pending.take() else {
            return Ok(false);
        };
        let ipc_handler = slot.pending_ipc_handler.take();

        // If WebView already exists, update content and apply visibility.
        if let Some(webview) = slot.webview.as_ref() {
            // ipc_handler is dropped here — wry has no API to replace the handler
            // post-construction. See `set_pending_content_with_ipc`.
            drop(ipc_handler);
            let _ = webview.set_bounds(logical_rect(bounds));
            match &content {
                PendingContent::Html(html) => {
                    webview
                        .load_html(html)
                        .map_err(|e| format!("Failed to load HTML: {}", e))?;
                }
                PendingContent::Url(url) => {
                    webview
                        .load_url(url)
                        .map_err(|e| format!("Failed to load URL: {}", e))?;
                }
            }
            let _ = webview.set_visible(slot.visible);
            return Ok(true);
        }

        // Get the raw handle from the trait object
        let handle = window
            .window_handle()
            .map_err(|e| format!("Failed to get window handle: {:?}", e))?;

        // Create a sized wrapper
        let wrapper = WindowHandleWrapper { handle };

        let mut builder = WebViewBuilder::new()
            .with_bounds(logical_rect(bounds))
            .with_transparent(false)
            .with_visible(slot.visible);

        builder = match &content {
            PendingContent::Html(html) => builder.with_html(html),
            PendingContent::Url(url) => builder.with_url(url),
        };

        if let Some(handler) = ipc_handler {
            builder = builder.with_ipc_handler(move |req: wry::http::Request<String>| {
                handler(req.into_body());
            });
        }

        let webview = builder
            .build_as_child(&wrapper)
            .map_err(|e| format!("Failed to create {surface:?} WebView: {}", e))?;

        slot.webview = Some(webview);
        Ok(true)
    })
    .unwrap_or(Ok(false))
}

/// Update the bounds (position and size) of `surface`'s webview.
pub fn update_bounds(surface: WebviewSurface, x: f32, y: f32, width: f32, height: f32) {
    with_existing_slot(surface, |slot| {
        if let Some(webview) = slot.webview.as_ref() {
            let _ = webview.set_bounds(logical_rect((x, y, width, height)));
        }
    });
}

/// Replace the HTML shown on `surface`.
pub fn update_content(surface: WebviewSurface, html: &str) {
    with_existing_slot(surface, |slot| {
        if let Some(webview) = slot.webview.as_ref() {
            if let Err(e) = webview.load_html(html) {
                eprintln!("[webview] {surface:?} load_html failed: {e}");
            }
        }
    });
}

/// Run a JavaScript snippet inside `surface`'s webview. No-op if it doesn't exist.
///
/// This is the Rust→JS push channel used by the agent chat UI to inject streamed
/// events into the page (e.g. `window.__appendEvent({...})`). For Rust←JS, see
/// `set_pending_content_with_ipc`.
#[allow(dead_code)]
pub fn evaluate_script(surface: WebviewSurface, script: &str) {
    with_existing_slot(surface, |slot| {
        if let Some(webview) = slot.webview.as_ref() {
            let _ = webview.evaluate_script(script);
        }
    });
}

fn apply_visible(slot: &mut SurfaceSlot, visible: bool) {
    slot.visible = visible;
    if let Some(webview) = slot.webview.as_ref() {
        let _ = webview.set_visible(visible);
    }
}

/// Show or hide `surface`. Recorded even when the webview doesn't exist yet
/// (but is staged), so a pending create honours the latest request.
pub fn set_visible(surface: WebviewSurface, visible: bool) {
    with_existing_slot(surface, |slot| apply_visible(slot, visible));
}

/// Show the page of agent tab `tab_id` (if it has one) and hide every other
/// agent page; `None` hides them all. Keeps the at-most-one-visible rule for
/// the agent pages in one place.
pub fn show_only_agent_page(tab_id: Option<usize>) {
    AGENT_PAGES.with(|pages| {
        for (id, slot) in pages.borrow_mut().iter_mut() {
            apply_visible(slot, Some(*id) == tab_id);
        }
    });
}

/// Give agent tab `tab_id`'s page keyboard focus and put the caret in its
/// composer, so typing goes straight into the chat after a tab switch. A
/// hidden page may otherwise keep first-responder status.
pub fn focus_agent_composer(tab_id: usize) {
    with_existing_slot(WebviewSurface::Agent(tab_id), |slot| {
        if let Some(webview) = slot.webview.as_ref() {
            if let Err(e) = webview.focus() {
                eprintln!("[agent-webview] focus failed for tab={tab_id}: {e}");
            }
            let _ = webview.evaluate_script("window.__focusComposer && window.__focusComposer()");
        }
    });
}

/// The script that inserts `text` at the composer caret of an agent chat page
/// (`window.__insertComposerText`). The text is JSON-encoded, so quotes,
/// backslashes, newlines and `</script>` arrive as literal characters.
fn insert_composer_text_script(text: &str) -> String {
    let literal = serde_json::Value::String(text.to_string());
    format!("window.__insertComposerText && window.__insertComposerText({literal})")
}

/// Insert dictated `text` at the caret of agent tab `tab_id`'s composer
/// (replacing any selection) and give the page keyboard focus. Returns
/// whether the page existed; the caller holds the text otherwise.
pub fn insert_agent_composer_text(tab_id: usize, text: &str) -> bool {
    let script = insert_composer_text_script(text);
    with_existing_slot(WebviewSurface::Agent(tab_id), |slot| {
        let Some(webview) = slot.webview.as_ref() else {
            return false;
        };
        if let Err(e) = webview.focus() {
            eprintln!("[agent-webview] focus failed for tab={tab_id}: {e}");
        }
        if let Err(e) = webview.evaluate_script(&script) {
            eprintln!(
                "[agent-webview] composer insert failed for tab={tab_id} ({} chars): {e}",
                text.chars().count()
            );
        }
        true
    })
    .unwrap_or(false)
}

/// Paste the system pasteboard into agent tab `tab_id`'s page, where its
/// focus or selection is, as an Edit menu's Paste would: the page forwards
/// Cmd+V (GitTerm has no Edit menu, so WebKit never gets `paste:` from the
/// key), and this sends the native `paste:` action to its WKWebView. WebKit
/// then fires a trusted `paste` event carrying the pasteboard, image files
/// included, before inserting any text.
pub fn paste_into_agent_page(tab_id: usize) {
    let found = with_existing_slot(WebviewSurface::Agent(tab_id), |slot| {
        let Some(webview) = slot.webview.as_ref() else {
            return false;
        };
        native_paste(webview, tab_id);
        true
    });
    if found != Some(true) {
        eprintln!("[agent-webview] paste for tab={tab_id} dropped: the tab has no chat page");
    }
}

#[cfg(target_os = "macos")]
fn native_paste(webview: &WebView, _tab_id: usize) {
    use wry::WebViewExtMacOS;
    let wk = webview.webview();
    // SAFETY: `paste:` is the NSResponder action WKWebView implements for
    // the Edit menu; it takes the sender (nil here) and returns nothing.
    // Called on the main thread, where the webview lives.
    unsafe {
        let _: () = objc2::msg_send![&*wk, paste: std::ptr::null::<objc2::runtime::AnyObject>()];
    }
}

#[cfg(not(target_os = "macos"))]
fn native_paste(_webview: &WebView, tab_id: usize) {
    // Only macOS pages forward Cmd+V; WebView2 pastes Ctrl+V itself.
    eprintln!("[agent-webview] paste for tab={tab_id} ignored: native paste is macOS only");
}

/// Hide every agent page.
pub fn hide_agent_pages() {
    show_only_agent_page(None);
}

/// Check whether `surface`'s webview exists.
pub fn is_active(surface: WebviewSurface) -> bool {
    with_existing_slot(surface, |slot| slot.webview.is_some()).unwrap_or(false)
}

/// Destroy `surface`'s webview (and drop anything staged for it). An agent
/// page's slot is removed outright.
#[allow(dead_code)]
pub fn destroy(surface: WebviewSurface) {
    match surface {
        WebviewSurface::Agent(tab_id) => {
            // Drop the WebView after the map borrow ends.
            let removed = AGENT_PAGES.with(|pages| pages.borrow_mut().remove(&tab_id));
            drop(removed);
        }
        WebviewSurface::Viewer => {
            let old = VIEWER_SURFACE
                .with(|slot| std::mem::replace(&mut *slot.borrow_mut(), SurfaceSlot::EMPTY));
            drop(old);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::insert_composer_text_script;

    /// The argument the page receives, decoded as the JS engine would read
    /// the JSON literal.
    fn decoded_argument(script: &str) -> String {
        let prefix = "window.__insertComposerText && window.__insertComposerText(";
        let literal = script
            .strip_prefix(prefix)
            .and_then(|rest| rest.strip_suffix(')'))
            .expect("script shape");
        serde_json::from_str(literal).expect("argument is a JSON string literal")
    }

    #[test]
    fn composer_insert_script_round_trips_awkward_text() {
        for text in [
            "plain words",
            "say \"hi\" and it's fine",
            "back\\slash \\n not a newline",
            "two\nlines\r\nand a\ttab",
            "</script><script>alert(1)</script>",
            "${template} `tick` ); window.x = 1; (",
            "caf\u{e9} \u{2028} \u{2029} \u{1f399}",
        ] {
            assert_eq!(decoded_argument(&insert_composer_text_script(text)), text);
        }
    }

    #[test]
    fn composer_insert_script_keeps_text_inside_one_string_literal() {
        let script = insert_composer_text_script("a\") ; evil(); (\"");
        // Every quote and newline in the text is escaped, so the call has
        // exactly one argument and nothing runs outside it.
        assert!(!script.contains('\n'));
        assert_eq!(
            script,
            r#"window.__insertComposerText && window.__insertComposerText("a\") ; evil(); (\"")"#
        );
    }
}
