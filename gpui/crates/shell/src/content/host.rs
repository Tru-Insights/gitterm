//! Narrow local WebKit host. No Wry, IPC handlers, URL loader or persistent store.
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    rc::Rc,
    sync::Arc,
};

use block2::{DynBlock, RcBlock};
use gpui::{Image, ImageFormat, Window};
use objc2::{
    AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
    rc::Retained, runtime::ProtocolObject,
};
use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep, NSImage, NSView};
use objc2_foundation::{
    NSDictionary, NSError, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString,
};
use objc2_web_kit::{
    WKNavigationAction, WKNavigationActionPolicy, WKNavigationDelegate, WKSnapshotConfiguration,
    WKWebView, WKWebViewConfiguration, WKWebsiteDataStore,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use super::{
    Document,
    overlay::{Geometry, Snapshot, SnapshotTicket},
};

/// UI-thread interface. `hide` MUST synchronously hide/detach the native view;
/// snapshot initiation and completion must never control native visibility.
pub trait SurfaceHost {
    fn load(&mut self, document: &Document) -> Result<(), String>;
    fn show(&mut self, geometry: Geometry) -> Result<(), String>;
    fn hide(&mut self);
    fn clear(&mut self);
    fn snapshot(&mut self, ticket: SnapshotTicket) -> Result<(), String>;
}

pub type SnapshotResult = (SnapshotTicket, Result<Snapshot, String>);

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = Cell<bool>]
    struct LocalNavigation;

    unsafe impl NSObjectProtocol for LocalNavigation {}
    unsafe impl WKNavigationDelegate for LocalNavigation {
        #[unsafe(method(webView:decidePolicyForNavigationAction:decisionHandler:))]
        unsafe fn decide(
            &self,
            _view: &WKWebView,
            action: &WKNavigationAction,
            decision: &DynBlock<dyn Fn(WKNavigationActionPolicy)>,
        ) {
            // loadHTMLString has about:blank as its document URL. Local rendered
            // content may fetch renderer assets but cannot navigate into Page,
            // local files, custom schemes, or launch an external application.
            let url = unsafe { action.request().URL() }.and_then(|url| url.absoluteString());
            let allow = allow_injected_document(self.ivars(), url.as_ref().map(|url| url.to_string()).as_deref());
            decision.call((if allow {
                WKNavigationActionPolicy::Allow
            } else {
                WKNavigationActionPolicy::Cancel
            },));
        }
    }
);

pub(super) fn allow_injected_document(pending: &Cell<bool>, url: Option<&str>) -> bool {
    pending.replace(false) && url == Some("about:blank")
}

/// One hidden-on-construction child view with an immutable nonpersistent store.
/// Drop before its GPUI window. PR9 drains results on the UI thread and feeds
/// them to `complete_snapshot`, then notifies the pane; callbacks never show it.
pub struct LocalWebKitHost {
    view: Retained<WKWebView>,
    parent: Retained<NSView>,
    _navigation: Retained<LocalNavigation>,
    results: Rc<RefCell<VecDeque<SnapshotResult>>>,
}

impl LocalWebKitHost {
    pub fn new(window: &Window) -> Result<Self, String> {
        let mtm = MainThreadMarker::new().ok_or("WebKit host requires the main thread")?;
        let handle = HasWindowHandle::window_handle(window)
            .map_err(|e| format!("Cannot get native window: {e}"))?;
        let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
            return Err("Retained content requires a macOS window".into());
        };
        // SAFETY: GPUI's live AppKit handle is an NSView, borrowed on its main
        // thread. Retain it until this host detaches in Drop.
        let parent = unsafe { Retained::retain(handle.ns_view.as_ptr().cast::<NSView>()) }
            .ok_or("Missing native parent view")?;
        let navigation: Retained<LocalNavigation> = unsafe {
            let allocated = LocalNavigation::alloc(mtm).set_ivars(Cell::new(false));
            msg_send![super(allocated), init]
        };
        // SAFETY: configuration and all native view operations stay on the main
        // thread. No user scripts/message handlers or persistent data are added.
        let view = unsafe {
            let configuration = WKWebViewConfiguration::new(mtm);
            configuration.setWebsiteDataStore(&WKWebsiteDataStore::nonPersistentDataStore(mtm));
            let view = WKWebView::initWithFrame_configuration(
                WKWebView::alloc(mtm),
                NSRect::ZERO,
                &configuration,
            );
            view.setNavigationDelegate(Some(ProtocolObject::from_ref(&*navigation)));
            view
        };
        view.setHidden(true);
        parent.addSubview(&view);
        Ok(Self {
            view,
            parent,
            _navigation: navigation,
            results: Rc::default(),
        })
    }

    pub fn drain_snapshots(&mut self) -> Vec<SnapshotResult> {
        self.results.borrow_mut().drain(..).collect()
    }
}

impl SurfaceHost for LocalWebKitHost {
    fn load(&mut self, document: &Document) -> Result<(), String> {
        self.hide();
        // SAFETY: main-thread WebKit instance, owned string, no filesystem base
        // URL or read-access grant. Store stays ephemeral across replacements.
        unsafe {
            self.view.stopLoading();
            self._navigation.ivars().set(true);
            self.view
                .loadHTMLString_baseURL(&NSString::from_str(document.html()), None)
        }
        .ok_or("WebKit refused retained HTML")?;
        Ok(())
    }

    fn show(&mut self, geometry: Geometry) -> Result<(), String> {
        geometry
            .pixel_size()
            .ok_or("Invalid retained content geometry")?;
        // AppKit may use a bottom-left origin; GPUI reports top-left points.
        let y = if self.parent.isFlipped() {
            geometry.y
        } else {
            self.parent.bounds().size.height - geometry.y - geometry.height
        };
        self.view.setFrame(NSRect::new(
            NSPoint::new(geometry.x, y),
            NSSize::new(geometry.width, geometry.height),
        ));
        self.view.setHidden(false);
        Ok(())
    }

    fn hide(&mut self) {
        self.view.setHidden(true);
        // Hidden WebKit must not keep receiving keyboard input. Do not disturb
        // a native dialog's responder when another cover calls hide again.
        if let Some(window) = self.view.window()
            && window.firstResponder().is_some_and(|responder| {
                responder
                    .downcast_ref::<NSView>()
                    .is_some_and(|view| view.isDescendantOf(&self.view))
            })
            && !window.makeFirstResponder(Some(&self.parent))
        {
            window.makeFirstResponder(None);
        }
    }

    fn clear(&mut self) {
        self.hide();
        unsafe {
            self.view.stopLoading();
            self._navigation.ivars().set(true);
            self.view
                .loadHTMLString_baseURL(&NSString::from_str(""), None);
        }
    }

    fn snapshot(&mut self, ticket: SnapshotTicket) -> Result<(), String> {
        let results = self.results.clone();
        let completion = RcBlock::new(move |image: *mut NSImage, error: *mut NSError| {
            // SAFETY: WebKit keeps callback arguments alive for this invocation
            // and invokes its snapshot completion on the main thread.
            let result = unsafe {
                if !error.is_null() {
                    Err(format!(
                        "WebKit snapshot failed: {}",
                        (*error).localizedDescription()
                    ))
                } else {
                    encode_snapshot(image)
                }
            };
            results.borrow_mut().push_back((ticket.clone(), result));
        });
        // Capture is initiated before the coordinator hides the native view.
        // There is deliberately no visibility mutation in the completion block.
        unsafe {
            let configuration = WKSnapshotConfiguration::new(self.view.mtm());
            // Capture the painted state, without waiting for the upcoming hide.
            configuration.setAfterScreenUpdates(false);
            self.view
                .takeSnapshotWithConfiguration_completionHandler(Some(&configuration), &completion);
        }
        Ok(())
    }
}

unsafe fn encode_snapshot(image: *mut NSImage) -> Result<Snapshot, String> {
    let image = unsafe { image.as_ref() }.ok_or("WebKit returned no snapshot")?;
    let data = image
        .TIFFRepresentation()
        .ok_or("Cannot read WebKit snapshot pixels")?;
    let bitmap = NSBitmapImageRep::initWithData(NSBitmapImageRep::alloc(), &data)
        .ok_or("Cannot decode WebKit snapshot pixels")?;
    let width = u32::try_from(bitmap.pixelsWide()).map_err(|_| "Invalid snapshot width")?;
    let height = u32::try_from(bitmap.pixelsHigh()).map_err(|_| "Invalid snapshot height")?;
    let data = unsafe {
        bitmap.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new())
    }
    .ok_or("Cannot encode WebKit snapshot")?;
    Ok(Snapshot {
        image: Arc::new(Image::from_bytes(ImageFormat::Png, data.to_vec())),
        pixel_size: (width, height),
    })
}

impl Drop for LocalWebKitHost {
    fn drop(&mut self) {
        self.hide();
        unsafe {
            self.view.stopLoading();
            self.view.setNavigationDelegate(None);
        }
        self.view.removeFromSuperview();
    }
}
