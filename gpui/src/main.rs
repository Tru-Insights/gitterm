#[cfg(target_os = "macos")]
extern crate gpui_kit as gpui;

#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "macos")]
fn main() {
    macos::run();
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("gitterm-gpui is currently supported only on macOS");
    std::process::exit(1);
}
