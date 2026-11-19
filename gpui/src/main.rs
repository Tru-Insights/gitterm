#[cfg(target_os = "macos")]
extern crate gpui_kit as gpui;

#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "macos")]
fn main() {
    let mut args = std::env::args_os().skip(1);
    if let Some(arg) = args.next().filter(|arg| arg != "--terminal") {
        if arg == "--review" {
            gitterm_gpui_review::run_fixture(args.collect());
            return;
        }
        if matches!(arg.to_str(), Some("--chat" | "--chat-live"))
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
        eprintln!(
            "Usage: gitterm-gpui [--terminal | --chat <fixture.jsonl> | --chat-live <fixture.jsonl> | --review <fixture-dir>]"
        );
        std::process::exit(2);
    }
    macos::run();
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("gitterm-gpui is currently supported only on macOS");
    std::process::exit(1);
}
