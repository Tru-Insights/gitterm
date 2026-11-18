#[cfg(target_os = "macos")]
fn main() {
    gitterm_gpui_shell::run();
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("gitterm-gpui is currently supported only on macOS");
    std::process::exit(1);
}
