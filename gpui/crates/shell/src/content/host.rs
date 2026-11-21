//! WebKit host: local Viewer documents in a nonpersistent store, or a remote
//! Page in GPUI's own login store. No Wry or IPC.
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
    NSDictionary, NSError, NSObject, NSObjectProtocol, NSOperatingSystemVersion, NSPoint,
    NSProcessInfo, NSRect, NSSize, NSString, NSURL, NSURLRequest, NSUUID,
};
use objc2_web_kit::{
    WKNavigation, WKNavigationAction, WKNavigationActionPolicy, WKNavigationDelegate,
    WKSnapshotConfiguration, WKWebView, WKWebViewConfiguration, WKWebsiteDataStore,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use super::{
    Document,
    overlay::{Geometry, LoadTicket, Snapshot, SnapshotTicket},
};

/// UI-thread interface. `hide` MUST synchronously hide/detach the native view;
/// snapshot initiation and completion must never control native visibility.
pub trait SurfaceHost {
    /// Start injection; report native navigation completion separately. Returning
    /// Ok does not permit display or capture of the still-loading document.
    fn load(&mut self, ticket: LoadTicket, document: &Document) -> Result<(), String>;
    fn show(&mut self, geometry: Geometry) -> Result<(), String>;
    fn hide(&mut self);
    fn clear(&mut self);
    fn snapshot(&mut self, ticket: SnapshotTicket) -> Result<(), String>;
}

pub type SnapshotResult = (SnapshotTicket, Result<Snapshot, String>);
pub type NavigationResult = (LoadTicket, Result<(), String>);

#[derive(Default)]
struct NavigationState {
    injection: Cell<bool>,
    /// A remote Page: navigation follows `page_navigation`, not the local policy.
    page: Cell<bool>,
    /// The Page URL just requested; its own first navigation always loads.
    page_request: RefCell<Option<String>>,
    pending: RefCell<Option<(Retained<WKNavigation>, LoadTicket)>>,
    results: RefCell<VecDeque<NavigationResult>>,
    external: RefCell<Vec<String>>,
}

impl NavigationState {
    fn finish(&self, navigation: Option<&WKNavigation>, result: Result<(), String>) {
        let mut pending = self.pending.borrow_mut();
        if let Some((expected, _)) = pending.as_ref()
            && navigation.is_some_and(|navigation| std::ptr::eq(&**expected, navigation))
        {
            let (_, ticket) = pending.take().expect("matched pending navigation");
            self.results.borrow_mut().push_back((ticket, result));
        }
    }
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = NavigationState]
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
            let url = unsafe { action.request().URL() }.and_then(|url| url.absoluteString());
            // targetFrame is nil for target=_blank/window.open. No new native
            // window or persistent Page surface is created for local documents.
            let target: Option<Retained<NSObject>> = unsafe { msg_send![action, targetFrame] };
            let url = url.as_ref().map(|url| url.to_string());
            let policy = if self.ivars().page.get() {
                // SAFETY: targetFrame is a WKFrameInfo, which answers isMainFrame.
                let main_frame = target
                    .as_ref()
                    .is_some_and(|frame| unsafe { msg_send![&**frame, isMainFrame] });
                let requested = main_frame
                    .then(|| self.ivars().page_request.borrow_mut().take())
                    .flatten();
                if requested.is_some() && requested == url {
                    DocumentNavigation::Allow
                } else {
                    page_navigation(url.as_deref(), target.is_none(), main_frame)
                }
            } else {
                document_navigation(&self.ivars().injection, url.as_deref(), target.is_none())
            };
            if policy == DocumentNavigation::External
                && let Some(url) = url
            {
                self.ivars().external.borrow_mut().push(url);
            }
            decision.call((if policy == DocumentNavigation::Allow {
                WKNavigationActionPolicy::Allow
            } else {
                WKNavigationActionPolicy::Cancel
            },));
        }

        #[unsafe(method(webView:didFinishNavigation:))]
        unsafe fn finished(&self, _view: &WKWebView, navigation: Option<&WKNavigation>) {
            self.ivars().finish(navigation, Ok(()));
        }

        #[unsafe(method(webView:didFailNavigation:withError:))]
        unsafe fn failed(
            &self,
            _view: &WKWebView,
            navigation: Option<&WKNavigation>,
            error: &NSError,
        ) {
            self.ivars().finish(
                navigation,
                Err(format!(
                    "Retained content navigation failed: {}",
                    error.localizedDescription()
                )),
            );
        }

        #[unsafe(method(webView:didFailProvisionalNavigation:withError:))]
        unsafe fn provisional_failed(
            &self,
            _view: &WKWebView,
            navigation: Option<&WKNavigation>,
            error: &NSError,
        ) {
            self.ivars().finish(
                navigation,
                Err(format!(
                    "Retained content navigation failed: {}",
                    error.localizedDescription()
                )),
            );
        }
    }
);

pub(super) fn allow_injected_document(pending: &Cell<bool>, url: Option<&str>) -> bool {
    pending.replace(false) && url == Some("about:blank")
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum DocumentNavigation {
    Allow,
    External,
    Cancel,
}

pub(super) fn document_navigation(
    pending: &Cell<bool>,
    url: Option<&str>,
    new_window: bool,
) -> DocumentNavigation {
    if allow_injected_document(pending, url) {
        return DocumentNavigation::Allow;
    }
    let Some(url) = url else {
        return DocumentNavigation::Cancel;
    };
    if !new_window && url.starts_with("about:blank#") {
        return DocumentNavigation::Allow;
    }
    let parsed = NSURL::URLWithString(&NSString::from_str(url));
    let network = parsed.as_ref().is_some_and(|url| {
        url.scheme().is_some_and(|scheme| {
            matches!(
                scheme.to_string().to_ascii_lowercase().as_str(),
                "http" | "https"
            )
        }) && url.host().is_some_and(|host| !host.is_empty())
    });
    if network {
        if new_window {
            DocumentNavigation::External
        } else {
            DocumentNavigation::Allow
        }
    } else {
        DocumentNavigation::Cancel
    }
}

/// Page link policy, after Iced's: GitHub, Linear and their sign-in providers
/// stay in the Page's login store; other web links open in the browser.
/// shortcut: a link to another owner's or workspace's store stays in this
/// store (Iced reopens the Page in that store); port Iced's Reopen when needed.
pub(super) fn page_navigation(
    url: Option<&str>,
    new_window: bool,
    main_frame: bool,
) -> DocumentNavigation {
    let Some(parsed) = url.and_then(|url| gitterm_core::page_store::browser_url(url).ok()) else {
        return DocumentNavigation::Cancel;
    };
    if new_window {
        return DocumentNavigation::External;
    }
    // Embedded frames of an allowed page load in place.
    if !main_frame {
        return DocumentNavigation::Allow;
    }
    let service = matches!(
        parsed.host_str(),
        Some(
            "github.com"
                | "linear.app"
                | "accounts.google.com"
                | "appleid.apple.com"
                | "idmsa.apple.com"
        )
    );
    if parsed.scheme() == "https"
        && parsed.username().is_empty()
        && parsed.password().is_none()
        && parsed.port().is_none()
        && service
    {
        DocumentNavigation::Allow
    } else {
        DocumentNavigation::External
    }
}

/// The WebKit store a Page uses: GPUI's own default store, or the persistent
/// store with Iced's identifier for that login, held in GPUI's app data.
fn page_data_store(
    store: &gitterm_core::page_store::PageStore,
    mtm: MainThreadMarker,
) -> Result<Retained<WKWebsiteDataStore>, String> {
    let unavailable = || format!("Page {} · not available in preview", store.label());
    let Some(bytes) = store.identifier() else {
        return Ok(unsafe { WKWebsiteDataStore::defaultDataStore(mtm) });
    };
    // dataStoreForIdentifier: is macOS 14+.
    let version = NSOperatingSystemVersion {
        majorVersion: 14,
        minorVersion: 0,
        patchVersion: 0,
    };
    if !NSProcessInfo::processInfo().isOperatingSystemAtLeastVersion(version) {
        return Err(unavailable());
    }
    let identifier =
        NSUUID::from_string(&NSString::from_str(&uuid_string(bytes))).ok_or_else(unavailable)?;
    Ok(unsafe { WKWebsiteDataStore::dataStoreForIdentifier(&identifier, mtm) })
}

/// RFC 4122 text form, so NSUUID parses the bytes without a raw-pointer init.
pub(super) fn uuid_string(bytes: [u8; 16]) -> String {
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// One hidden-on-construction child view with an immutable store: nonpersistent
/// for local documents, the Page's login store for a remote Page.
/// Drop before its GPUI window. PR9 drains results on the UI thread and feeds
/// them to `complete_snapshot`, then notifies the pane; callbacks never show it.
pub struct LocalWebKitHost {
    view: Retained<WKWebView>,
    parent: Retained<NSView>,
    navigation: Retained<LocalNavigation>,
    results: Rc<RefCell<VecDeque<SnapshotResult>>>,
}

impl LocalWebKitHost {
    pub fn new(
        window: &Window,
        page: Option<&gitterm_core::page_store::PageStore>,
    ) -> Result<Self, String> {
        let mtm = MainThreadMarker::new().ok_or("WebKit host requires the main thread")?;
        let store = match page {
            Some(store) => page_data_store(store, mtm)?,
            None => unsafe { WKWebsiteDataStore::nonPersistentDataStore(mtm) },
        };
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
            let allocated = LocalNavigation::alloc(mtm).set_ivars(NavigationState {
                page: Cell::new(page.is_some()),
                ..NavigationState::default()
            });
            msg_send![super(allocated), init]
        };
        // SAFETY: configuration and all native view operations stay on the main
        // thread. No user scripts/message handlers are added; only a Page persists data.
        let view = unsafe {
            let configuration = WKWebViewConfiguration::new(mtm);
            configuration.setWebsiteDataStore(&store);
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
            navigation,
            results: Rc::default(),
        })
    }

    pub fn drain_external(&mut self) -> Vec<String> {
        std::mem::take(&mut *self.navigation.ivars().external.borrow_mut())
    }

    pub fn drain_snapshots(&mut self) -> Vec<SnapshotResult> {
        self.results.borrow_mut().drain(..).collect()
    }

    pub fn drain_navigation(&mut self) -> Vec<NavigationResult> {
        self.navigation
            .ivars()
            .results
            .borrow_mut()
            .drain(..)
            .collect()
    }
}

impl SurfaceHost for LocalWebKitHost {
    fn load(&mut self, ticket: LoadTicket, document: &Document) -> Result<(), String> {
        self.hide();
        self.navigation.ivars().pending.borrow_mut().take();
        self.navigation.ivars().external.borrow_mut().clear();
        // SAFETY: main-thread WebKit instance, owned string, no filesystem base
        // URL or read-access grant. The store never changes across replacements.
        let navigation = unsafe {
            self.view.stopLoading();
            if let Some(url) = document.page_url() {
                self.navigation.ivars().injection.set(false);
                let request = NSURL::URLWithString(&NSString::from_str(url))
                    .ok_or("WebKit refused the Page URL")?;
                // Record WebKit's own spelling, which the policy callback reports.
                *self.navigation.ivars().page_request.borrow_mut() =
                    request.absoluteString().map(|url| url.to_string());
                let url = request;
                self.view.loadRequest(&NSURLRequest::requestWithURL(&url))
            } else {
                self.navigation.ivars().injection.set(true);
                self.view
                    .loadHTMLString_baseURL(&NSString::from_str(document.html()), None)
            }
        }
        .ok_or("WebKit refused retained HTML")?;
        *self.navigation.ivars().pending.borrow_mut() = Some((navigation, ticket));
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
        self.navigation.ivars().pending.borrow_mut().take();
        self.navigation.ivars().external.borrow_mut().clear();
        unsafe {
            self.view.stopLoading();
            self.navigation.ivars().injection.set(true);
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
