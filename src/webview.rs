// WebView module for embedded markdown/mermaid rendering
//
// Note: Due to threading constraints (wry's WebView is not Send/Sync),
// the WebView must be created and managed on the main thread.

use std::cell::RefCell;
use wry::raw_window_handle::{HasWindowHandle, WindowHandle};
use wry::{Rect, WebView, WebViewBuilder};

thread_local! {
    static WEBVIEW: RefCell<Option<WebView>> = const { RefCell::new(None) };
    static PENDING_HTML: RefCell<Option<(String, (f32, f32, f32, f32))>> = const { RefCell::new(None) };
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
        Ok(unsafe {
            WindowHandle::borrow_raw(self.handle.as_raw())
        })
    }
}

/// Store HTML content to be rendered when we get window access
#[allow(dead_code)]
pub fn set_pending_content(html: String, bounds: (f32, f32, f32, f32)) {
    PENDING_HTML.with(|p| {
        *p.borrow_mut() = Some((html, bounds));
    });
}

/// Try to create WebView with pending content using the given window
/// This should be called from the main thread with window access
#[allow(dead_code)]
pub fn try_create_with_window(window: &dyn HasWindowHandle) -> Result<(), String> {
    let pending = PENDING_HTML.with(|p| p.borrow_mut().take());

    if let Some((html, bounds)) = pending {
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
                webview
                    .load_html(&html)
                    .map_err(|e| format!("Failed to load HTML: {}", e))?;
                return Ok(());
            }

            // Create new WebView
            let (x, y, width, height) = bounds;

            let webview = WebViewBuilder::new()
                .with_bounds(Rect {
                    position: wry::dpi::Position::Logical(wry::dpi::LogicalPosition::new(
                        x as f64, y as f64,
                    )),
                    size: wry::dpi::Size::Logical(wry::dpi::LogicalSize::new(
                        width as f64,
                        height as f64,
                    )),
                })
                .with_html(&html)
                .with_transparent(false)
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
