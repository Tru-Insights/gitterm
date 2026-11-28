//! Headless JavaScriptCore: execute the shared viewer functions without WebKit,
//! a native window, external executables, network or additional dependencies.
use std::{
    ffi::{CString, c_char, c_void},
    ptr,
};
type Ref = *mut c_void;
#[link(name = "JavaScriptCore", kind = "framework")]
unsafe extern "C" {
    fn JSGlobalContextCreate(class: Ref) -> Ref;
    fn JSGlobalContextRelease(context: Ref);
    fn JSStringCreateWithUTF8CString(string: *const c_char) -> Ref;
    fn JSStringRelease(string: Ref);
    fn JSEvaluateScript(
        context: Ref,
        script: Ref,
        this: Ref,
        url: Ref,
        line: i32,
        exception: *mut Ref,
    ) -> Ref;
    fn JSValueToStringCopy(context: Ref, value: Ref, exception: *mut Ref) -> Ref;
    fn JSStringGetMaximumUTF8CStringSize(string: Ref) -> usize;
    fn JSStringGetUTF8CString(string: Ref, buffer: *mut c_char, size: usize) -> usize;
}

fn run(name: &str) {
    let script = format!(
        "const asset = {}; const testName = {}; const escapeScript = {};\n{}",
        serde_json::to_string(super::super::VIEWER_HTML).unwrap(),
        serde_json::to_string(name).unwrap(),
        serde_json::to_string(super::super::ESCAPE_SCRIPT).unwrap(),
        include_str!("page_tests.js")
    );
    let script = CString::new(script).unwrap();
    // SAFETY: each test owns its context and UTF-8 strings, passes no host objects
    // or callbacks, and releases every created JavaScriptCore reference below.
    let error = unsafe {
        let context = JSGlobalContextCreate(ptr::null_mut());
        assert!(!context.is_null());
        let script = JSStringCreateWithUTF8CString(script.as_ptr());
        let mut exception = ptr::null_mut();
        JSEvaluateScript(
            context,
            script,
            ptr::null_mut(),
            ptr::null_mut(),
            1,
            &mut exception,
        );
        let error = if exception.is_null() {
            None
        } else {
            let message = JSValueToStringCopy(context, exception, ptr::null_mut());
            let mut bytes = vec![0u8; JSStringGetMaximumUTF8CStringSize(message)];
            let length = JSStringGetUTF8CString(message, bytes.as_mut_ptr().cast(), bytes.len());
            JSStringRelease(message);
            Some(String::from_utf8_lossy(&bytes[..length.saturating_sub(1)]).into_owned())
        };
        JSStringRelease(script);
        JSGlobalContextRelease(context);
        error
    };
    assert!(error.is_none(), "{name}: {}", error.unwrap_or_default());
}

#[test]
fn plans_viewer_toc_built_from_headings() {
    run("toc");
}

#[test]
fn plans_viewer_collapse_expand_one_and_all() {
    run("collapse");
}

#[test]
fn plans_viewer_markdown_drop_is_read_only_first_file() {
    run("drop");
}

#[test]
fn plans_viewer_escape_from_webkit_requests_only_close() {
    run("escape");
}
