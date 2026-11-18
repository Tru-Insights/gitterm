#[cfg(target_os = "macos")]
extern crate gpui_kit as gpui;

#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "macos")]
fn main() {
    let mut args = std::env::args_os().skip(1);
    if let Some(arg) = args.next() {
        if arg == "--chat"
            && let Some(path) = args.next().filter(|_| args.next().is_none())
        {
            match gitterm_gpui_chat::events::load_fixture(std::path::Path::new(&path)) {
                Ok(records) => {
                    gitterm_gpui_chat::run(records);
                    return;
                }
                Err(error) => {
                    eprintln!("{error}");
                    std::process::exit(1);
                }
            }
        }
        eprintln!("Usage: gitterm-gpui [--chat <fixture.jsonl>]");
        std::process::exit(2);
    }
    macos::run();
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("gitterm-gpui is currently supported only on macOS");
    std::process::exit(1);
}
