// WebView module for embedded markdown/mermaid rendering and the agent chat UI.
//
// Two independent child webviews ("surfaces") share the main window:
//
// - `WebviewSurface::Agent` hosts the native Claude chat page with its IPC
//   handler. It stays alive (hidden) while a viewer is shown so the chat DOM
//   survives a file or plans-viewer visit.
// - `WebviewSurface::Viewer` hosts markdown / HTML / excalidraw HTML and the
//   plans-viewer URL.
//
// The app keeps at most one surface visible at a time (see
// `visible_webview_surface` in main.rs).
//
// Note: Due to threading constraints (wry's WebView is not Send/Sync),
// the WebViews must be created and managed on the main thread.

use std::cell::RefCell;

type WebViewBounds = (f32, f32, f32, f32);

/// IPC handler closure type. Receives the JSON body posted from the webview via
/// `window.ipc.postMessage(jsonString)`. Must be `Send + 'static` because wry runs
/// the handler from the platform's web-process callback path.
pub type IpcHandler = Box<dyn Fn(String) + Send + 'static>;

/// Which of the two child webviews an operation targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebviewSurface {
    /// Native Claude chat page; IPC handler installed at construction.
    Agent,
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

// One RefCell per surface so an operation on one surface can never trip a
// borrow held by the other.
thread_local! {
    static AGENT_SURFACE: RefCell<SurfaceSlot> = const { RefCell::new(SurfaceSlot::EMPTY) };
    static VIEWER_SURFACE: RefCell<SurfaceSlot> = const { RefCell::new(SurfaceSlot::EMPTY) };
}

fn with_slot<R>(surface: WebviewSurface, f: impl FnOnce(&mut SurfaceSlot) -> R) -> R {
    match surface {
        WebviewSurface::Agent => AGENT_SURFACE.with(|slot| f(&mut slot.borrow_mut())),
        WebviewSurface::Viewer => VIEWER_SURFACE.with(|slot| f(&mut slot.borrow_mut())),
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
/// which is why the chat lives on its own surface.
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
    with_slot(surface, |slot| {
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
/// for the same staging already consumed it.
#[allow(dead_code)]
pub fn try_create_with_window(
    surface: WebviewSurface,
    window: &dyn HasWindowHandle,
) -> Result<bool, String> {
    with_slot(surface, |slot| {
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
}

/// Update the bounds (position and size) of `surface`'s webview.
pub fn update_bounds(surface: WebviewSurface, x: f32, y: f32, width: f32, height: f32) {
    with_slot(surface, |slot| {
        if let Some(webview) = slot.webview.as_ref() {
            let _ = webview.set_bounds(logical_rect((x, y, width, height)));
        }
    });
}

/// Replace the HTML shown on `surface`.
pub fn update_content(surface: WebviewSurface, html: &str) {
    with_slot(surface, |slot| {
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
    with_slot(surface, |slot| {
        if let Some(webview) = slot.webview.as_ref() {
            let _ = webview.evaluate_script(script);
        }
    });
}

/// Show or hide `surface`. Recorded even when the webview doesn't exist yet, so
/// a pending create honours the latest request.
pub fn set_visible(surface: WebviewSurface, visible: bool) {
    with_slot(surface, |slot| {
        slot.visible = visible;
        if let Some(webview) = slot.webview.as_ref() {
            let _ = webview.set_visible(visible);
        }
    });
}

/// Check whether `surface`'s webview exists.
pub fn is_active(surface: WebviewSurface) -> bool {
    with_slot(surface, |slot| slot.webview.is_some())
}

/// Destroy `surface`'s webview (and drop anything staged for it).
#[allow(dead_code)]
pub fn destroy(surface: WebviewSurface) {
    with_slot(surface, |slot| {
        *slot = SurfaceSlot::EMPTY;
    });
}
