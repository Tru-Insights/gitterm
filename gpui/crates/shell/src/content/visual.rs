//! H10 visual replies on native WebKit, with Iced's agent-page boundary
//! (src/webview.rs, `WebviewSurface::Agent`): the frame guard runs at document
//! start in every frame, only the main frame can reach the bridge, navigation
//! stays on about:blank/about:srcdoc, and the store is nonpersistent. The
//! agent's document is never loaded here directly: html_render.js puts it in
//! a sandboxed srcdoc iframe with its CSP, as in the Iced chat page.
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    rc::{Rc, Weak},
};

use block2::DynBlock;
use gitterm_core::content::html_render::{FRAME_GUARD_JS, RENDER_JS, agent_navigation_allowed};
use gitterm_gpui_chat::{VisualHost, VisualMessage, VisualPlacement, VisualSurface};
use gpui::{App, Entity, IntoElement, Styled, Window, canvas};
use objc2::{
    DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, rc::Retained,
    runtime::ProtocolObject, sel,
};
use objc2_app_kit::NSView;
use objc2_foundation::{NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};
use objc2_web_kit::{
    WKNavigationAction, WKNavigationActionPolicy, WKNavigationDelegate, WKScriptMessage,
    WKScriptMessageHandler, WKUserContentController, WKUserScript, WKUserScriptInjectionTime,
    WKWebView, WKWebViewConfiguration, WKWebsiteDataStore,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

const BRIDGE: &str = "gittermVisual";

/// Main frame only, before page scripts: the private post the host page uses.
const BRIDGE_JS: &str = r#"(() => {
  const handler = window.webkit.messageHandlers.gittermVisual;
  const send = handler.postMessage.bind(handler);
  const stringify = JSON.stringify.bind(JSON);
  Object.defineProperty(window, '__gittermVisualPost', {
    value: (message) => send(stringify(message)), writable: false, configurable: false
  });
})();"#;

/// The host page: Iced's `GitTermHtml.create` call, filling the native frame.
pub(crate) fn host_page(html: &str, height: u32, light: bool) -> String {
    let document = serde_json::to_string(html)
        .expect("string serializes")
        .replace('<', "\\u003c");
    let (theme, background) = if light {
        ("light", "#eff1f5")
    } else {
        ("dark", "#1e1e2e")
    };
    format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><style>html,body{{margin:0;padding:0;height:100%;overflow:hidden;background:{background}}}iframe{{height:100% !important}}</style><script>{RENDER_JS}</script></head><body><script>(() => {{
  const post = window.__gittermVisualPost;
  const render = GitTermHtml.create({document}, {height}, '{theme}', (url) => post({{ type: 'link', url }}), () => post({{ type: 'escape' }}));
  document.body.appendChild(render.frame);
  document.addEventListener('keydown', (e) => {{ if (e.key === 'Escape') post({{ type: 'escape' }}); }}, true);
}})();</script></body></html>"#
    )
}

/// A bridge message from the host page. Links are checked again by the chat.
pub(crate) fn bridge_message(body: &str) -> Option<VisualMessage> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    match value.get("type")?.as_str()? {
        "link" => Some(VisualMessage::Link(value.get("url")?.as_str()?.to_owned())),
        "escape" => Some(VisualMessage::Escape),
        _ => None,
    }
}

#[derive(Default)]
struct FrameState {
    inbox: RefCell<VecDeque<VisualMessage>>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = Rc<FrameState>]
    struct VisualBridge;

    unsafe impl NSObjectProtocol for VisualBridge {}
    unsafe impl WKScriptMessageHandler for VisualBridge {
        #[unsafe(method(userContentController:didReceiveScriptMessage:))]
        fn received(&self, _: &WKUserContentController, message: &WKScriptMessage) {
            // WebKit's handler is reachable from subframes too; only the host
            // page's main frame may speak.
            if !unsafe { message.frameInfo().isMainFrame() } {
                return;
            }
            let body = unsafe { message.body() };
            if let Some(body) = body.downcast_ref::<NSString>()
                && let Some(message) = bridge_message(&body.to_string())
            {
                self.ivars().inbox.borrow_mut().push_back(message);
            }
        }
    }
);

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    struct VisualNavigation;

    unsafe impl NSObjectProtocol for VisualNavigation {}
    unsafe impl WKNavigationDelegate for VisualNavigation {
        #[unsafe(method(webView:decidePolicyForNavigationAction:decisionHandler:))]
        unsafe fn decide(
            &self,
            _view: &WKWebView,
            action: &WKNavigationAction,
            decision: &DynBlock<dyn Fn(WKNavigationActionPolicy)>,
        ) {
            // Every frame, as Iced's agent page: only the injected host page and
            // the srcdoc frame load. targetFrame is nil for new windows.
            let target: Option<Retained<NSObject>> = unsafe { msg_send![action, targetFrame] };
            let url = unsafe { action.request().URL() }
                .and_then(|url| url.absoluteString())
                .map(|url| url.to_string());
            let allow = target.is_some() && url.as_deref().is_some_and(agent_navigation_allowed);
            decision.call((if allow {
                WKNavigationActionPolicy::Allow
            } else {
                WKNavigationActionPolicy::Cancel
            },));
        }
    }
);

/// Frames that drew in the current window frame; the rest are hidden.
#[derive(Default)]
struct Frames {
    generation: Cell<u64>,
    live: RefCell<Vec<Weak<NativeFrame>>>,
}

struct NativeFrame {
    frames: Rc<Frames>,
    placed: Cell<Option<(u64, VisualPlacement)>>,
    state: Rc<FrameState>,
    parent: Retained<NSView>,
    clip: Retained<NSView>,
    view: Retained<WKWebView>,
    _navigation: Retained<VisualNavigation>,
    _bridge: Retained<VisualBridge>,
}

impl NativeFrame {
    fn new(
        frames: Rc<Frames>,
        window: &Window,
        html: &str,
        height: u32,
        light: bool,
    ) -> Result<Self, String> {
        let mtm = MainThreadMarker::new().ok_or("Visual replies require the main thread")?;
        let handle = HasWindowHandle::window_handle(window)
            .map_err(|e| format!("Cannot get native window: {e}"))?;
        let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
            return Err("Visual replies require a macOS window".into());
        };
        // SAFETY: GPUI's live AppKit handle is an NSView, borrowed on its main
        // thread and retained until this frame detaches in Drop.
        let parent = unsafe { Retained::retain(handle.ns_view.as_ptr().cast::<NSView>()) }
            .ok_or("Missing native parent view")?;
        let state = Rc::new(FrameState::default());
        let bridge: Retained<VisualBridge> = unsafe {
            let allocated = VisualBridge::alloc(mtm).set_ivars(state.clone());
            msg_send![super(allocated), init]
        };
        let navigation: Retained<VisualNavigation> = unsafe {
            let allocated = VisualNavigation::alloc(mtm).set_ivars(());
            msg_send![super(allocated), init]
        };
        // SAFETY: all WebKit configuration and view calls stay on the main thread.
        let view = unsafe {
            let configuration = WKWebViewConfiguration::new(mtm);
            configuration.setWebsiteDataStore(&WKWebsiteDataStore::nonPersistentDataStore(mtm));
            let content = configuration.userContentController();
            // Every frame: CSP does not govern WebRTC, and nested srcdoc
            // documents can run despite frame-src 'none'.
            content.addUserScript(
                &WKUserScript::initWithSource_injectionTime_forMainFrameOnly(
                    WKUserScript::alloc(mtm),
                    &NSString::from_str(FRAME_GUARD_JS),
                    WKUserScriptInjectionTime::AtDocumentStart,
                    false,
                ),
            );
            content.addUserScript(
                &WKUserScript::initWithSource_injectionTime_forMainFrameOnly(
                    WKUserScript::alloc(mtm),
                    &NSString::from_str(BRIDGE_JS),
                    WKUserScriptInjectionTime::AtDocumentStart,
                    true,
                ),
            );
            content.addScriptMessageHandler_name(
                ProtocolObject::from_ref(&*bridge),
                &NSString::from_str(BRIDGE),
            );
            let view = WKWebView::initWithFrame_configuration(
                WKWebView::alloc(mtm),
                NSRect::ZERO,
                &configuration,
            );
            view.setNavigationDelegate(Some(ProtocolObject::from_ref(&*navigation)));
            view.loadHTMLString_baseURL(&NSString::from_str(&host_page(html, height, light)), None)
                .ok_or("WebKit refused the visual reply")?;
            view
        };
        // A partly scrolled frame is cut to what the transcript shows.
        let clip = NSView::initWithFrame(NSView::alloc(mtm), NSRect::ZERO);
        if clip.respondsToSelector(sel!(setClipsToBounds:)) {
            clip.setClipsToBounds(true);
        }
        clip.addSubview(&view);
        clip.setHidden(true);
        parent.addSubview(&clip);
        Ok(Self {
            frames,
            placed: Cell::new(None),
            state,
            parent,
            clip,
            view,
            _navigation: navigation,
            _bridge: bridge,
        })
    }

    fn show(&self, placement: VisualPlacement) {
        let (bounds, visible) = (placement.bounds, placement.visible);
        let (vx, vy) = (f64::from(visible.origin.x), f64::from(visible.origin.y));
        let (vw, vh) = (
            f64::from(visible.size.width),
            f64::from(visible.size.height),
        );
        if vw <= 0. || vh <= 0. {
            return self.hide();
        }
        // AppKit may use a bottom-left origin; GPUI reports top-left points.
        let y = if self.parent.isFlipped() {
            vy
        } else {
            self.parent.bounds().size.height - vy - vh
        };
        self.clip
            .setFrame(NSRect::new(NSPoint::new(vx, y), NSSize::new(vw, vh)));
        let (bx, by) = (f64::from(bounds.origin.x), f64::from(bounds.origin.y));
        let (bw, bh) = (f64::from(bounds.size.width), f64::from(bounds.size.height));
        // The clip view is unflipped: offset from its bottom edge.
        self.view.setFrame(NSRect::new(
            NSPoint::new(bx - vx, (vy + vh) - (by + bh)),
            NSSize::new(bw, bh),
        ));
        self.clip.setHidden(false);
    }

    fn hide(&self) {
        if self.clip.isHidden() {
            return;
        }
        self.clip.setHidden(true);
        // Hidden WebKit must not keep receiving keyboard input.
        if let Some(window) = self.view.window()
            && window.firstResponder().is_some_and(|responder| {
                responder
                    .downcast_ref::<NSView>()
                    .is_some_and(|view| view.isDescendantOf(&self.clip))
            })
            && !window.makeFirstResponder(Some(&self.parent))
        {
            window.makeFirstResponder(None);
        }
    }
}

impl Drop for NativeFrame {
    fn drop(&mut self) {
        self.hide();
        unsafe {
            self.view.stopLoading();
            self.view.setNavigationDelegate(None);
            self.view
                .configuration()
                .userContentController()
                .removeScriptMessageHandlerForName(&NSString::from_str(BRIDGE));
        }
        self.clip.removeFromSuperview();
    }
}

struct NativeSurface(Rc<NativeFrame>);

impl VisualSurface for NativeSurface {
    fn place(&mut self, placement: VisualPlacement) {
        let generation = self.0.frames.generation.get();
        self.0.placed.set(Some((generation, placement)));
    }
    fn drain(&mut self) -> Vec<VisualMessage> {
        self.0.state.inbox.borrow_mut().drain(..).collect()
    }
}

#[derive(Clone)]
struct VisualFrames(Rc<Frames>);
impl gpui::Global for VisualFrames {}

/// The app's native host for chat visual replies. Tests install a fake.
pub(crate) fn install(cx: &mut App) {
    let frames = Rc::new(Frames::default());
    cx.set_global(VisualFrames(frames.clone()));
    cx.set_global(VisualHost(Rc::new(move |window, html, height, light| {
        let frame = Rc::new(NativeFrame::new(
            frames.clone(),
            window,
            html,
            height,
            light,
        )?);
        frames.live.borrow_mut().push(Rc::downgrade(&frame));
        Ok(Box::new(NativeSurface(frame)) as Box<dyn VisualSurface>)
    })));
}

/// Last in the shell's prepaint: show frames placed in this frame, hide the
/// rest (scrolled away, another pane, or covered by a menu, dialog or drag).
pub(crate) fn reconciler(content: Entity<super::window::ContentWindow>) -> impl IntoElement {
    canvas(
        move |_, _, cx| {
            let Some(frames) = cx.try_global::<VisualFrames>().map(|f| f.0.clone()) else {
                return;
            };
            let covered = content.read(cx).covered();
            let generation = frames.generation.get();
            frames.live.borrow_mut().retain(|frame| {
                let Some(frame) = frame.upgrade() else {
                    return false;
                };
                match frame.placed.get() {
                    Some((placed, placement)) if placed == generation && !covered => {
                        frame.show(placement)
                    }
                    _ => frame.hide(),
                }
                true
            });
            frames.generation.set(generation.wrapping_add(1));
        },
        |_, _, _, _| {},
    )
    .absolute()
    .size_0()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bridge_accepts_only_link_and_escape() {
        assert_eq!(
            bridge_message(r#"{"type":"link","url":"https://example.com"}"#),
            Some(VisualMessage::Link("https://example.com".into()))
        );
        assert_eq!(
            bridge_message(r#"{"type":"escape"}"#),
            Some(VisualMessage::Escape)
        );
        for body in [
            r#"{"type":"link"}"#,
            r#"{"type":"link","url":1}"#,
            r#"{"type":"open_url","url":"https://example.com"}"#,
            "not json",
        ] {
            assert_eq!(bridge_message(body), None, "{body}");
        }
    }

    #[test]
    fn host_page_embeds_the_document_as_inert_script_data() {
        let page = host_page("</script><script>alert(1)</script>", 240, false);
        assert!(!page.contains("</script><script>alert(1)"));
        assert!(page.contains("\\u003c/script>\\u003cscript>alert(1)\\u003c/script>"));
        assert!(page.contains("GitTermHtml.create("));
        assert!(page.contains(", 240, 'dark',"));
        assert!(host_page("x", 100, true).contains(", 100, 'light',"));
        // The guard installs before any page script and fails closed without it.
        assert!(RENDER_JS.contains("__gittermHtmlGuard !== true"));
        assert!(FRAME_GUARD_JS.contains("__gittermHtmlGuard"));
        for url in ["about:blank", "about:srcdoc"] {
            assert!(agent_navigation_allowed(url));
        }
        for url in ["https://example.com", "about:blank#x", "file:///etc/hosts"] {
            assert!(!agent_navigation_allowed(url));
        }
    }
}
