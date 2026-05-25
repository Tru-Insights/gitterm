// WebView module for embedded markdown/mermaid rendering and (in v1, TRU-29) the agent
// chat UI.
//
// Note: Due to threading constraints (wry's WebView is not Send/Sync),
// the WebView must be created and managed on the main thread.

use std::cell::RefCell;

type WebViewBounds = (f32, f32, f32, f32);

/// IPC handler closure type. Receives the JSON body posted from the webview via
/// `window.ipc.postMessage(jsonString)`. Must be `Send + 'static` because wry runs
/// the handler from the platform's web-process callback path.
pub type IpcHandler = Box<dyn Fn(String) + Send + 'static>;

/// What kind of content is staged for the next webview create / reuse. The
/// markdown / excalidraw / agent surfaces stage HTML. The plans viewer stages
/// a URL pointing at the local warp server.
enum PendingContent {
    Html(String),
    Url(String),
}

use wry::raw_window_handle::{HasWindowHandle, WindowHandle};
use wry::{Rect, WebView, WebViewBuilder};

thread_local! {
    static WEBVIEW: RefCell<Option<WebView>> = const { RefCell::new(None) };
    static PENDING_CONTENT: RefCell<Option<(PendingContent, WebViewBounds)>> = const { RefCell::new(None) };
    static PENDING_IPC_HANDLER: RefCell<Option<IpcHandler>> = const { RefCell::new(None) };
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

/// Store HTML content to be rendered when we get window access. No IPC handler is
/// installed — appropriate for the markdown / excalidraw / HTML viewers which only
/// render content and don't post messages back to Rust.
#[allow(dead_code)]
pub fn set_pending_content(html: String, bounds: (f32, f32, f32, f32)) {
    set_pending_content_with_ipc(html, bounds, None);
}

/// Like `set_pending_content`, but also stages an optional IPC handler closure.
/// The handler will be installed on the webview at construction time and invoked
/// whenever JS calls `window.ipc.postMessage(jsonString)`. Use this for the agent
/// chat UI which needs a return path from JS to Rust (submit prompt, stop, etc.).
///
/// The handler is consumed (taken) by the next `try_create_with_window` call. If
/// the webview already exists when `try_create_with_window` runs, the handler is
/// dropped — wry doesn't support replacing an IPC handler post-construction. In
/// practice this means: pick a single owner of the agent webview lifecycle.
#[allow(dead_code)]
pub fn set_pending_content_with_ipc(
    html: String,
    bounds: (f32, f32, f32, f32),
    ipc_handler: Option<IpcHandler>,
) {
    PENDING_CONTENT.with(|p| {
        *p.borrow_mut() = Some((PendingContent::Html(html), bounds));
    });
    PENDING_IPC_HANDLER.with(|h| {
        *h.borrow_mut() = ipc_handler;
    });
}

/// Stage a URL for the next `try_create_with_window` to load. Used by the
/// plans viewer (and any other surface that wants to navigate the embedded
/// webview to a localhost route instead of injecting HTML). No IPC handler;
/// URL-loaded surfaces communicate only via HTTP back to the warp server.
#[allow(dead_code)]
pub fn set_pending_url(url: String, bounds: (f32, f32, f32, f32)) {
    PENDING_CONTENT.with(|p| {
        *p.borrow_mut() = Some((PendingContent::Url(url), bounds));
    });
    PENDING_IPC_HANDLER.with(|h| {
        *h.borrow_mut() = None;
    });
}

/// Navigate the live webview to a URL. No-op if no webview exists. Use this
/// when the webview is already up (`is_active()` returns true) and you want
/// to swap its location without recreating the surface.
#[allow(dead_code)]
pub fn navigate_to_url(url: &str) {
    WEBVIEW.with(|wv| {
        if let Some(webview) = wv.borrow().as_ref() {
            let _ = webview.load_url(url);
        }
    });
}

/// Try to create WebView with pending content using the given window
/// This should be called from the main thread with window access
#[allow(dead_code)]
pub fn try_create_with_window(window: &dyn HasWindowHandle) -> Result<(), String> {
    let pending = PENDING_CONTENT.with(|p| p.borrow_mut().take());
    let ipc_handler = PENDING_IPC_HANDLER.with(|h| h.borrow_mut().take());

    if let Some((content, bounds)) = pending {
        // Get the raw handle from the trait object
        let handle = window
            .window_handle()
            .map_err(|e| format!("Failed to get window handle: {:?}", e))?;

        // Create a sized wrapper
        let wrapper = WindowHandleWrapper { handle };

        WEBVIEW.with(|wv| {
            let mut wv_ref = wv.borrow_mut();

            // If WebView already exists, update content and make visible
            if let Some(webview) = wv_ref.as_ref() {
                let _ = webview.set_visible(true);
                // Update bounds in case they changed
                let (x, y, width, height) = bounds;
                let _ = webview.set_bounds(Rect {
                    position: wry::dpi::Position::Logical(wry::dpi::LogicalPosition::new(
                        x as f64, y as f64,
                    )),
                    size: wry::dpi::Size::Logical(wry::dpi::LogicalSize::new(
                        width as f64,
                        height as f64,
                    )),
                });
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
                // ipc_handler is dropped here — wry has no API to replace the handler
                // post-construction. See module-level docs on `set_pending_content_with_ipc`.
                return Ok(());
            }

            // Create new WebView
            let (x, y, width, height) = bounds;

            let mut builder = WebViewBuilder::new()
                .with_bounds(Rect {
                    position: wry::dpi::Position::Logical(wry::dpi::LogicalPosition::new(
                        x as f64, y as f64,
                    )),
                    size: wry::dpi::Size::Logical(wry::dpi::LogicalSize::new(
                        width as f64,
                        height as f64,
                    )),
                })
                .with_transparent(false);

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
                .map_err(|e| format!("Failed to create WebView: {}", e))?;

            *wv_ref = Some(webview);
            Ok(())
        })
    } else {
        Ok(()) // Nothing to do
    }
}

/// Update WebView bounds (position and size)
pub fn update_bounds(x: f32, y: f32, width: f32, height: f32) {
    WEBVIEW.with(|wv| {
        if let Some(webview) = wv.borrow().as_ref() {
            let _ = webview.set_bounds(Rect {
                position: wry::dpi::Position::Logical(wry::dpi::LogicalPosition::new(
                    x as f64, y as f64,
                )),
                size: wry::dpi::Size::Logical(wry::dpi::LogicalSize::new(
                    width as f64,
                    height as f64,
                )),
            });
        }
    });
}

/// Update WebView content
pub fn update_content(html: &str) {
    WEBVIEW.with(|wv| {
        if let Some(webview) = wv.borrow().as_ref() {
            let _ = webview.load_html(html);
        }
    });
}

/// Run a JavaScript snippet inside the webview. No-op if no webview exists.
///
/// This is the Rust→JS push channel used by the agent chat UI to inject streamed
/// events into the page (e.g. `window.__appendEvent({...})`). For Rust←JS, see
/// `set_pending_content_with_ipc`.
#[allow(dead_code)]
pub fn evaluate_script(script: &str) {
    WEBVIEW.with(|wv| {
        if let Some(webview) = wv.borrow().as_ref() {
            let _ = webview.evaluate_script(script);
        }
    });
}

/// Show or hide the WebView
pub fn set_visible(visible: bool) {
    WEBVIEW.with(|wv| {
        if let Some(webview) = wv.borrow().as_ref() {
            let _ = webview.set_visible(visible);
        }
    });
}

/// Check if WebView exists
pub fn is_active() -> bool {
    WEBVIEW.with(|wv| wv.borrow().is_some())
}

/// Destroy the WebView
#[allow(dead_code)]
pub fn destroy() {
    WEBVIEW.with(|wv| {
        *wv.borrow_mut() = None;
    });
}
