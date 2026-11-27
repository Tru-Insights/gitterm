#[cfg(target_os = "macos")]
extern crate gpui_kit as gpui;

#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "macos")]
mod launch;

#[cfg(target_os = "macos")]
mod text_smoothing {
    #![allow(non_camel_case_types, non_snake_case)]

    use std::ffi::{c_char, c_void};

    type CFStringRef = *const c_void;
    type CFTypeRef = *const c_void;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFAllocatorDefault: *const c_void;
        static kCFPreferencesCurrentApplication: CFStringRef;

        fn CFStringCreateWithCString(
            allocator: *const c_void,
            string: *const c_char,
            encoding: u32,
        ) -> CFStringRef;
        fn CFNumberCreate(
            allocator: *const c_void,
            number_type: u32,
            value: *const c_void,
        ) -> CFTypeRef;
        fn CFRelease(value: CFTypeRef);
        fn CFPreferencesCopyAppValue(key: CFStringRef, application_id: CFStringRef) -> CFTypeRef;
        fn CFPreferencesSetAppValue(
            key: CFStringRef,
            value: CFTypeRef,
            application_id: CFStringRef,
        );
        fn CFPreferencesAppSynchronize(application_id: CFStringRef) -> bool;
    }

    fn set_default_if_absent<T>(read: impl FnOnce() -> Option<T>, write: impl FnOnce(i32)) {
        if read().is_none() {
            write(0);
        }
    }

    fn cf_string(value: &'static [u8]) -> CFStringRef {
        // CFStringCreateWithCString copies the input into the returned string.
        unsafe {
            CFStringCreateWithCString(kCFAllocatorDefault, value.as_ptr().cast(), 0x0800_0100)
        }
    }

    pub(super) fn initialize() {
        let application = unsafe { kCFPreferencesCurrentApplication };
        let key = cf_string(b"AppleFontSmoothing\0");
        if key.is_null() {
            return;
        }

        // Match Iced's antialiased text while preserving an explicit user preference.
        set_default_if_absent(
            || {
                let value = unsafe { CFPreferencesCopyAppValue(key, application) };
                if value.is_null() {
                    None
                } else {
                    unsafe { CFRelease(value) };
                    Some(())
                }
            },
            |zero| {
                // kCFNumberSInt32Type is 3 in CoreFoundation's CFNumberType enum.
                let value =
                    unsafe { CFNumberCreate(kCFAllocatorDefault, 3, (&zero as *const i32).cast()) };
                if value.is_null() {
                    return;
                }
                unsafe {
                    CFPreferencesSetAppValue(key, value, application);
                    CFPreferencesAppSynchronize(application);
                    CFRelease(value);
                }
            },
        );

        unsafe { CFRelease(key) };
    }

    #[cfg(test)]
    mod tests {
        use super::set_default_if_absent;
        use std::cell::Cell;

        #[test]
        fn sets_zero_only_when_preference_is_absent() {
            let written = Cell::new(None);
            set_default_if_absent(|| None::<i32>, |value| written.set(Some(value)));
            assert_eq!(written.get(), Some(0));

            for existing in [0, 1] {
                let written = Cell::new(None);
                set_default_if_absent(|| Some(existing), |value| written.set(Some(value)));
                assert_eq!(written.get(), None);
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn main() {
    text_smoothing::initialize();
    if let Err(error) = run() {
        eprintln!("{error}");
        gitterm_gpui_shell::show_startup_failure(&error);
    }
}

#[cfg(target_os = "macos")]
fn run() -> Result<(), String> {
    let route = launch::parse(std::env::args_os().skip(1)).unwrap_or_else(|| usage());
    route
        .authorize(|| {
            gitterm_gpui_shell::preview::roots_from_environment(false)
                .map_err(|error| std::io::Error::other(format!("{error:#}")))
        })
        .map_err(|error| error.to_string())?;
    match route {
        launch::Launch::Chat(path) => {
            let records = gitterm_gpui_chat::events::load_fixture(&path)
                .map_err(|error| error.to_string())?;
            gitterm_gpui_chat::run(records);
        }
        launch::Launch::Terminal => macos::run(),
        launch::Launch::Review(args) => gitterm_gpui_review::run_fixture(args),
        launch::Launch::ReviewRepo(path) => gitterm_gpui_review::run_repo(path),
        launch::Launch::Shell { folders, sample } => {
            gitterm_gpui_shell::run(folders, sample).map_err(|error| format!("{error:#}"))?;
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn usage() -> ! {
    eprintln!(
        "Usage: gitterm-gpui [--workspace <dir> ... | --sample | --terminal | --chat <fixture.jsonl> | --chat-live <fixture.jsonl> | --review <fixture-dir> | --review-repo <dir>]"
    );
    std::process::exit(2);
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("gitterm-gpui is currently supported only on macOS");
    std::process::exit(1);
}
