//! Native TRU-270 acceptance probe. Uses the production chat page, WryWebView
//! hook, guarded IPC and pasteboard writer, without an agent/network process.
//! GITTERM_V6_CONFIG_DIR must name a fresh absolute temporary directory.
#[cfg(target_os = "macos")]
mod native {
    #[allow(dead_code)]
    mod webview {
        include!("../src/webview.rs");

        use objc2::{
            class, msg_send, msg_send_id,
            rc::{Allocated, Retained},
            runtime::{AnyObject, Bool},
            sel,
        };
        use wry::WebViewExtMacOS;

        pub struct ProbeMenu(Retained<AnyObject>);
        impl ProbeMenu {
            pub fn open() -> Self {
                let wk = with_existing_slot(WebviewSurface::Agent(1), |slot| {
                    let webview = slot.webview.as_ref().unwrap();
                    assert!(
                        markdown_menu::install(webview),
                        "wry hook unavailable or already owned"
                    );
                    webview.webview()
                })
                .unwrap();
                unsafe {
                    let class = objc2::runtime::AnyClass::get("WryWebView")
                        .expect("wry class contract changed");
                    let belongs: Bool = msg_send![&*wk, isKindOfClass: class];
                    assert!(belongs.as_bool(), "wry class contract changed");
                    let menu: Retained<AnyObject> = msg_send_id![class!(NSMenu), new];
                    let _: () = msg_send![&*menu, setAutoenablesItems: Bool::NO];
                    let title: Retained<AnyObject> =
                        msg_send_id![class!(NSString), stringWithUTF8String: c"Copy".as_ptr()];
                    let empty: Retained<AnyObject> =
                        msg_send_id![class!(NSString), stringWithUTF8String: c"".as_ptr()];
                    let item: Allocated<AnyObject> = msg_send_id![class!(NSMenuItem), alloc];
                    let item: Retained<AnyObject> = msg_send_id![item, initWithTitle: &*title action: sel!(copy:) keyEquivalent: &*empty];
                    let _: () = msg_send![&*item, setTarget: &*wk];
                    let _: () = msg_send![&*menu, addItem: &*item];
                    let _: () = msg_send![&*wk, willOpenMenu: &*menu withEvent: std::ptr::null::<AnyObject>()];
                    Self(menu)
                }
            }
            pub fn count(&self) -> usize {
                unsafe { msg_send![&*self.0, numberOfItems] }
            }
            pub fn titles(&self) -> Vec<String> {
                (0..self.count())
                    .map(|index| unsafe {
                        let item: *mut AnyObject = msg_send![&*self.0, itemAtIndex: index];
                        let title: *mut AnyObject = msg_send![item, title];
                        let text: *const std::ffi::c_char = msg_send![title, UTF8String];
                        std::ffi::CStr::from_ptr(text)
                            .to_string_lossy()
                            .into_owned()
                    })
                    .collect()
            }
            pub fn copy(&self, index: usize) {
                unsafe {
                    let _: () = msg_send![&*self.0, performActionForItemAtIndex: index];
                }
            }
        }
    }

    use std::time::{Duration, Instant};
    use tao::event::Event;
    use tao::event_loop::{ControlFlow, EventLoopBuilder};
    use tao::platform::run_return::EventLoopExtRunReturn;
    use tao::window::WindowBuilder;

    pub fn run() -> Result<(), Box<dyn std::error::Error>> {
        let config = std::env::var("GITTERM_V6_CONFIG_DIR")?;
        assert!(
            std::path::Path::new(&config).is_absolute(),
            "isolated absolute config required"
        );
        assert_ne!(
            std::path::Path::new(&config),
            dirs::home_dir().unwrap().join(".config/gitterm-v6")
        );
        let mut event_loop = EventLoopBuilder::<String>::with_user_event().build();
        let window = WindowBuilder::new()
            .with_title("GitTerm TRU-270 native acceptance")
            .with_inner_size(tao::dpi::LogicalSize::new(1000., 800.))
            .build(&event_loop)?;
        let proxy = event_loop.create_proxy();
        let mut page = gitterm::chat_page::agent_chat_html();
        page = page.replace("</body>", r#"<script>
          window.addEventListener('load', () => {
            window.__setTabId(1, null, null, false);
            window.__appendEvent({type:'user_prompt', text:'Synthetic TRU-270 transcript'});
            window.__appendEvent({kind:'harness',event:{type:'text_delta',data:'Before **selected bold** after.\n'}});
            window.__appendEvent({kind:'harness',event:{type:'turn_completed',data:{status:{state:'completed'},usage:{}}}});
            document.activeElement.blur();
            const bold = document.querySelector('#messages .assistant strong');
            const range = document.createRange(); range.selectNodeContents(bold);
            window.getSelection().removeAllRanges(); window.getSelection().addRange(range);
            bold.dispatchEvent(new MouseEvent('contextmenu',{bubbles:true,cancelable:true}));
            window.__gittermNativePost(JSON.stringify({type:'test_ready',tabId:1}));
          });
        </script></body>"#);
        let surface = webview::WebviewSurface::Agent(1);
        webview::set_pending_content_with_ipc(
            surface,
            page,
            (0., 0., 1000., 800.),
            Some(Box::new(move |body| {
                proxy.send_event(body).unwrap();
            })),
        );
        webview::try_create_with_window(surface, &window)?;
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut menu: Option<webview::ProbeMenu> = None;
        let mut acted = false;
        let mut copied = false;
        let mut ordinary_copy = false;
        let mut empty_menu = None;
        let mut finished = false;
        event_loop.run_return(|event, _, flow| {
            *flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(30));
            if let Event::UserEvent(body) = event {
                let value: serde_json::Value = serde_json::from_str(&body).unwrap();
                match value["type"].as_str().unwrap() {
                    "test_ready" => menu = Some(webview::ProbeMenu::open()),
                    "copy_markdown" => {
                        let (id, text) = gitterm::chat_page::markdown_clipboard_message(value).unwrap();
                        assert_eq!(id, 1); assert_eq!(text, "**selected bold**");
                        webview::copy_markdown_to_pasteboard(&text).unwrap();
                        copied = true;
                        println!("PASS native menu action -> guarded typed IPC -> plain NSPasteboard");
                        // A stale transcript selection must not offer the item over the composer.
                        webview::evaluate_script(surface, "document.getElementById('prompt').dispatchEvent(new MouseEvent('contextmenu',{bubbles:true,cancelable:true})); window.__gittermNativePost(JSON.stringify({type:'empty_menu',tabId:1}));");
                    }
                    "ordinary_copy" => {
                        let plain = std::process::Command::new("/usr/bin/pbpaste").output().unwrap();
                        assert_eq!(String::from_utf8(plain.stdout).unwrap(), "selected bold");
                        println!("PASS native Copy action and page Cmd+C path preserve selected plain contents");
                        ordinary_copy = true;
                        webview::evaluate_script(surface, "document.querySelector('#messages .assistant strong').dispatchEvent(new MouseEvent('contextmenu',{bubbles:true,cancelable:true})); window.__gittermNativePost(JSON.stringify({type:'markdown_ready',tabId:1}));");
                    }
                    "markdown_ready" => { menu = Some(webview::ProbeMenu::open()); acted = false; },
                    "empty_menu" => empty_menu = Some((webview::ProbeMenu::open(), Instant::now())),
                    other => panic!("unexpected IPC kind {other}"),
                }
            }
            if let Some(menu) = &menu {
                if menu.count() == 2 && !acted {
                    assert_eq!(menu.titles(), vec!["Copy", "Copy as Markdown"]);
                    println!("PASS hook appends to supplied NSMenu, preserves Copy, class= WryWebView");
                    if !ordinary_copy {
                        // Exercise the existing native Copy menu item's action before
                        // the markdown action overwrites it. No custom copy handler.
                        menu.copy(0);
                        webview::evaluate_script(surface, "document.dispatchEvent(new KeyboardEvent('keydown',{key:'c',metaKey:true,bubbles:true,cancelable:true})); window.__gittermNativePost(JSON.stringify({type:'ordinary_copy',tabId:1}));");
                        acted = true;
                    } else { menu.copy(1); acted = true; }
                }
            }
            if let Some((menu, opened)) = &empty_menu {
                if !finished && opened.elapsed() > Duration::from_millis(300) {
                    assert_eq!(menu.titles(), vec!["Copy"]);
                    assert!(copied);
                    println!("PASS composer menu retains Copy and has no markdown item");
                    finished = true; *flow = ControlFlow::Exit;
                }
            }
            if Instant::now() >= deadline { *flow = ControlFlow::Exit; }
        });
        assert!(finished, "native probe timed out");
        println!("Native probe passed. Clipboard contains the synthetic selection only.");
        Ok(())
    }
}
#[cfg(target_os = "macos")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    native::run()
}
#[cfg(not(target_os = "macos"))]
fn main() {
    println!("Copy as Markdown native probe is macOS only");
}
