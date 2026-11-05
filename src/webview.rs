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
// - `WebviewSurface::Viewer` hosts markdown / HTML / excalidraw HTML, the
//   plans-viewer URL and the review pane (TRU-178). It is built with an IPC
//   handler that only the review page's token-stamped messages get through.
// - `WebviewSurface::Page` hosts remote GitHub and Linear pages, with an
//   optional persistent WebKit store keyed by the signed-in account/workspace.
//
// The app keeps at most one surface visible at a time (see
// `visible_webview_surface` in main.rs).
//
// Note: Due to threading constraints (wry's WebView is not Send/Sync),
// the WebViews must be created and managed on the main thread.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

type WebViewBounds = (f32, f32, f32, f32);

/// IPC handler closure type. Receives the JSON body posted from the webview via
/// `window.ipc.postMessage(jsonString)`. Must be `Send + 'static` because wry runs
/// the handler from the platform's web-process callback path.
pub type IpcHandler = Box<dyn Fn(String) + Send + 'static>;
type PageNavigationHandler = Box<dyn Fn(&str) -> bool>;

/// Which child webview an operation targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebviewSurface {
    /// The chat page of the agent tab with this id; IPC handler installed at
    /// construction.
    Agent(usize),
    /// File viewer HTML (markdown / HTML / excalidraw), the plans viewer URL
    /// and the review pane.
    Viewer,
    /// A remote GitHub or Linear page, kept separate from the default Viewer store.
    Page,
}

/// Persistent cookie-store identity for a remote page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageDataStore {
    Default,
    GitHub(String),
    Linear(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageStoreAction {
    Navigate,
    Rebuild,
}

pub fn page_store_action(
    current: Option<&PageDataStore>,
    requested: &PageDataStore,
) -> PageStoreAction {
    if current == Some(requested) {
        PageStoreAction::Navigate
    } else {
        PageStoreAction::Rebuild
    }
}

impl PageDataStore {
    /// Normalize GitHub login casing because logins are case-insensitive.
    pub fn normalized(self) -> Self {
        match self {
            Self::GitHub(login) => Self::GitHub(login.to_ascii_lowercase()),
            store => store,
        }
    }

    /// Use shared cookies when custom stores are unavailable on this OS.
    pub fn for_current_os(self) -> Self {
        if matches!(&self, Self::Default) {
            return self.normalized();
        }
        #[cfg(not(target_os = "macos"))]
        return Self::Default;
        #[cfg(target_os = "macos")]
        if custom_data_stores_available() {
            return self.normalized();
        }
        #[cfg(target_os = "macos")]
        static LOGGED: std::sync::Once = std::sync::Once::new();
        #[cfg(target_os = "macos")]
        LOGGED.call_once(|| {
            eprintln!("[webview] named page data stores are unavailable; using the default store");
        });
        #[cfg(target_os = "macos")]
        Self::Default
    }

    pub fn label(&self) -> String {
        match self {
            Self::Default => "default login".to_string(),
            Self::GitHub(login) => format!("as {login}"),
            Self::Linear(workspace) => format!("as {workspace}"),
        }
    }

    /// A stable, namespaced identifier accepted by wry's Darwin extension.
    pub fn identifier(&self) -> Option<[u8; 16]> {
        let (namespace, key) = match self {
            Self::Default => return None,
            Self::GitHub(login) => ("github", login.to_ascii_lowercase()),
            Self::Linear(workspace) => ("linear", workspace.clone()),
        };
        let mut input = b"gitterm-v6-webkit-store-v1\0".to_vec();
        input.extend_from_slice(namespace.as_bytes());
        input.push(0);
        input.extend_from_slice(key.as_bytes());
        let digest = ring::digest::digest(&ring::digest::SHA256, &input);
        let mut identifier = [0; 16];
        identifier.copy_from_slice(&digest.as_ref()[..16]);
        Some(identifier)
    }
}

#[cfg(target_os = "macos")]
fn custom_data_stores_available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        std::process::Command::new("/usr/bin/sw_vers")
            .arg("-productVersion")
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .and_then(|version| version.trim().split('.').next()?.parse::<u32>().ok())
            .is_some_and(|major| major >= 14)
    })
}

#[cfg(not(target_os = "macos"))]
fn custom_data_stores_available() -> bool {
    false
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
    pending: Option<(PendingContent, WebViewBounds, Option<[u8; 16]>)>,
    data_store_identifier: Option<[u8; 16]>,
    pending_ipc_handler: Option<IpcHandler>,
    page_navigation_handler: Option<Rc<RefCell<PageNavigationHandler>>>,
    visible: bool,
}

impl SurfaceSlot {
    const EMPTY: SurfaceSlot = SurfaceSlot {
        webview: None,
        pending: None,
        data_store_identifier: None,
        pending_ipc_handler: None,
        page_navigation_handler: None,
        visible: false,
    };
}

// The agent pages share one map keyed by tab id; the Viewer has its own
// RefCell so an operation on it can never trip a borrow held by the pages.
thread_local! {
    static AGENT_PAGES: RefCell<HashMap<usize, SurfaceSlot>> = RefCell::new(HashMap::new());
    static VIEWER_SURFACE: RefCell<SurfaceSlot> = const { RefCell::new(SurfaceSlot::EMPTY) };
    static PAGE_SURFACE: RefCell<SurfaceSlot> = const { RefCell::new(SurfaceSlot::EMPTY) };
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
        WebviewSurface::Page => PAGE_SURFACE.with(|slot| f(&mut slot.borrow_mut())),
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
        WebviewSurface::Page => PAGE_SURFACE.with(|slot| Some(f(&mut slot.borrow_mut()))),
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
        slot.pending = Some((PendingContent::Html(html), bounds, None));
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
    set_pending_url_with_ipc(surface, url, bounds, None);
}

/// Like `set_pending_url`, but also stages an IPC handler for the surface's
/// construction (see `set_pending_content_with_ipc`). The Viewer is staged
/// this way whatever it shows first, so the handler the review pane needs
/// (TRU-178) is installed when the surface is built.
pub fn set_pending_url_with_ipc(
    surface: WebviewSurface,
    url: String,
    bounds: (f32, f32, f32, f32),
    ipc_handler: Option<IpcHandler>,
) {
    with_slot(surface, |slot| {
        slot.pending = Some((PendingContent::Url(url), bounds, None));
        slot.pending_ipc_handler = ipc_handler;
        slot.visible = true;
    });
}

/// Stage a remote page with a persistent, account-scoped WebKit data store.
pub fn set_pending_page(url: String, bounds: (f32, f32, f32, f32), store: &PageDataStore) {
    with_slot(WebviewSurface::Page, |slot| {
        slot.pending = Some((PendingContent::Url(url), bounds, store.identifier()));
        slot.pending_ipc_handler = None;
        slot.visible = true;
    });
}

/// Update a live Page's routing policy without replacing its native handler.
/// Each surface owns its cell, so destroyed surfaces cannot read a new one's policy.
pub fn set_page_navigation_handler(handler: impl Fn(&str) -> bool + 'static) {
    with_slot(WebviewSurface::Page, |slot| {
        if let Some(cell) = &slot.page_navigation_handler {
            *cell.borrow_mut() = Box::new(handler);
        } else {
            slot.page_navigation_handler = Some(Rc::new(RefCell::new(Box::new(handler))));
        }
    });
}

#[cfg(test)]
pub fn test_page_navigation(url: &str) -> bool {
    PAGE_SURFACE.with(|slot| {
        let slot = slot.borrow();
        slot.page_navigation_handler
            .as_ref()
            .is_some_and(|handler| (handler.borrow())(url))
    })
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
        let Some((content, bounds, data_store_identifier)) = slot.pending.take() else {
            return Ok(false);
        };
        let ipc_handler = slot.pending_ipc_handler.take();

        // A Page webview cannot change its cookie store after construction.
        // Drop it before reusing the slot when its staged store differs.
        if surface == WebviewSurface::Page
            && slot.webview.is_some()
            && slot.data_store_identifier != data_store_identifier
        {
            slot.webview = None;
        }

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

        if matches!(surface, WebviewSurface::Viewer | WebviewSurface::Page) {
            builder = builder.with_new_window_req_handler(|url| {
                open_url_in_browser(&url);
                false
            });
        }

        if surface == WebviewSurface::Page {
            // The independent cell avoids reborrowing SurfaceSlot if load_url
            // invokes the policy synchronously. A missing policy fails closed.
            // shortcut: wry 0.48's GTK hook has no frame identity; revisit when
            // upstream exposes it. macOS/Windows call this for the main frame.
            let handler = slot.page_navigation_handler.clone();
            builder = builder.with_navigation_handler(move |url| {
                handler
                    .as_ref()
                    .is_some_and(|handler| (handler.borrow())(&url))
            });
        }

        #[cfg(target_os = "macos")]
        if surface == WebviewSurface::Page {
            if let Some(identifier) = data_store_identifier {
                use wry::WebViewBuilderExtDarwin;
                builder = builder.with_data_store_identifier(identifier);
            }
        }

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
        slot.data_store_identifier = data_store_identifier;
        Ok(true)
    })
    .unwrap_or(Ok(false))
}

/// Validate content-controlled URLs before handing them to an OS protocol handler.
pub fn browser_url(url: &str) -> Result<url::Url, String> {
    let parsed = url::Url::parse(url).map_err(|error| format!("invalid URL: {error}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(format!("refused URL scheme {}", parsed.scheme()));
    }
    Ok(parsed)
}

#[cfg(any(target_os = "windows", test))]
fn windows_browser_command(url: &str) -> std::process::Command {
    let mut command = std::process::Command::new("rundll32.exe");
    command.args(["url.dll,FileProtocolHandler", url]);
    command
}

/// Open an HTTP(S) URL using the platform's default browser.
pub fn open_url_in_browser(url: &str) {
    let url = match browser_url(url) {
        Ok(url) => url,
        Err(error) => {
            eprintln!("[webview] browser open refused: {error}");
            return;
        }
    };
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg(url.as_str()).spawn();
    #[cfg(target_os = "windows")]
    let result = windows_browser_command(url.as_str()).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let result = std::process::Command::new("xdg-open")
        .arg(url.as_str())
        .spawn();

    if let Err(error) = result {
        eprintln!("[webview] cannot open {url} in the browser: {error}");
    }
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

/// Open one roster row's activity (a subagent's block or a delegation's
/// card, by the page's roster key) in agent tab `tab_id`'s page, as a
/// click on the row would (`window.__revealRosterItem`, TRU-154). Returns
/// whether the page existed; the caller holds the key otherwise.
pub fn reveal_agent_roster_row(tab_id: usize, key: &str) -> bool {
    let literal = serde_json::Value::String(key.to_string());
    let script = format!("window.__revealRosterItem && window.__revealRosterItem({literal})");
    with_existing_slot(WebviewSurface::Agent(tab_id), |slot| {
        let Some(webview) = slot.webview.as_ref() else {
            return false;
        };
        if let Err(e) = webview.evaluate_script(&script) {
            eprintln!("[agent-webview] roster reveal failed for tab={tab_id} key={key}: {e}");
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

/// Give keyboard focus back to GitTerm's own view, the window view every
/// webview is a child of. Called when the user clicks a terminal, in case a
/// webview still holds first responder: a chat page takes Cmd+V for itself
/// (`isHostShortcut`). A click on the Iced view normally moves first
/// responder there already (seen in focus logging, 2026-10-11). A no-op
/// when no webview exists.
pub fn focus_app_view() {
    let focus_parent =
        |slot: &mut SurfaceSlot| slot.webview.as_ref().map(|webview| webview.focus_parent());
    let result = AGENT_PAGES
        .with(|pages| pages.borrow_mut().values_mut().find_map(focus_parent))
        .or_else(|| VIEWER_SURFACE.with(|slot| focus_parent(&mut slot.borrow_mut())))
        .or_else(|| PAGE_SURFACE.with(|slot| focus_parent(&mut slot.borrow_mut())));
    if let Some(Err(e)) = result {
        eprintln!("[webview] giving keyboard focus back to the app view failed: {e}");
    }
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
        WebviewSurface::Page => {
            let old = PAGE_SURFACE
                .with(|slot| std::mem::replace(&mut *slot.borrow_mut(), SurfaceSlot::EMPTY));
            drop(old);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{insert_composer_text_script, page_store_action, PageDataStore, PageStoreAction};

    #[test]
    fn page_policy_can_run_under_slot_borrow_and_old_surfaces_keep_their_own_policy() {
        super::set_page_navigation_handler(|_| false);
        let old = super::PAGE_SURFACE.with(|slot| {
            let slot = slot.borrow();
            let handler = slot.page_navigation_handler.as_ref().unwrap();
            assert!(!(handler.borrow())("https://example.test/"));
            handler.clone()
        });
        super::set_page_navigation_handler(|_| true);
        assert!((old.borrow())("https://example.test/"));
        super::destroy(super::WebviewSurface::Page);
        super::set_page_navigation_handler(|_| false);
        assert!((old.borrow())("https://example.test/"));
        super::PAGE_SURFACE.with(|slot| {
            let slot = slot.borrow();
            assert!(!(slot.page_navigation_handler.as_ref().unwrap().borrow())(
                "https://example.test/"
            ));
        });
        super::destroy(super::WebviewSurface::Page);
    }

    #[test]
    fn browser_urls_allow_only_http_and_https() {
        for url in [
            "https://example.test/",
            "http://example.test/",
            "HTTPS://example.test/",
        ] {
            assert!(super::browser_url(url).is_ok(), "{url}");
        }
        for (url, scheme) in [
            ("file:///tmp/example", "file"),
            ("javascript:alert(1)", "javascript"),
            ("data:text/html,example", "data"),
            ("sample-app:open", "sample-app"),
        ] {
            let error = super::browser_url(url).unwrap_err();
            assert!(error.contains(scheme), "{error}");
        }
        assert!(super::browser_url("not a URL").is_err());
    }

    #[test]
    fn windows_browser_opener_has_no_shell_and_keeps_url_as_one_argument() {
        let url = "https://example.test/?x=1&sample.exe=2%20value";
        let command = super::windows_browser_command(url);
        assert_eq!(command.get_program(), "rundll32.exe");
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["url.dll,FileProtocolHandler", url]
        );
    }

    #[test]
    fn a_pending_page_keeps_the_latest_hidden_visibility_request() {
        let store = PageDataStore::GitHub("sample-login".to_string());
        super::set_pending_page(
            "https://github.com/sample-owner/sample-repo/pull/1".to_string(),
            (0.0, 0.0, 100.0, 100.0),
            &store,
        );
        super::set_visible(super::WebviewSurface::Page, false);
        super::PAGE_SURFACE.with(|slot| {
            let slot = slot.borrow();
            assert!(slot.pending.is_some());
            assert!(!slot.visible);
        });
        super::destroy(super::WebviewSurface::Page);
    }

    #[test]
    fn page_store_identifiers_are_stable_and_distinct() {
        let alpha = PageDataStore::GitHub("sample-login-a".to_string());
        let beta = PageDataStore::GitHub("sample-login-b".to_string());
        assert_eq!(
            alpha.identifier(),
            Some([
                0xa9, 0x0d, 0x78, 0xb9, 0x36, 0xe7, 0x7e, 0x9f, 0x1e, 0xe4, 0x3b, 0x89, 0xd3, 0xf8,
                0xa9, 0x35,
            ])
        );
        assert_eq!(alpha.identifier(), alpha.clone().identifier());
        assert_ne!(alpha.identifier(), beta.identifier());
        assert_ne!(
            alpha.identifier(),
            PageDataStore::Linear("sample-login-a".to_string()).identifier()
        );
    }

    #[test]
    fn page_store_action_navigates_for_same_store_and_rebuilds_for_another() {
        let first = PageDataStore::GitHub("sample-login-a".to_string());
        let other = PageDataStore::GitHub("sample-login-b".to_string());
        assert_eq!(
            page_store_action(Some(&first), &first),
            PageStoreAction::Navigate
        );
        assert_eq!(
            page_store_action(Some(&first), &other),
            PageStoreAction::Rebuild
        );
        assert_eq!(page_store_action(None, &first), PageStoreAction::Rebuild);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn platforms_without_named_webkit_stores_use_the_default_store() {
        assert_eq!(
            PageDataStore::GitHub("sample-login".to_string()).for_current_os(),
            PageDataStore::Default
        );
    }

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
