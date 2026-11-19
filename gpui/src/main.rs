#[cfg(target_os = "macos")]
extern crate gpui_kit as gpui;

#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "macos")]
mod launch;

#[cfg(target_os = "macos")]
fn main() {
    match launch::parse(std::env::args_os().skip(1)).unwrap_or_else(|| usage()) {
        launch::Launch::Chat(path) => match gitterm_gpui_chat::events::load_fixture(&path) {
            Ok(records) => gitterm_gpui_chat::run(records),
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(1);
            }
        },
        launch::Launch::Terminal => macos::run(),
        launch::Launch::Shell { folders, sample } => {
            if let Err(error) = gitterm_gpui_shell::run(folders, sample) {
                eprintln!("{error:#}");
                std::process::exit(1);
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn usage() -> ! {
    eprintln!(
        "Usage: gitterm-gpui [--workspace <dir> ... | --sample | --terminal | --chat <fixture.jsonl>]"
    );
    std::process::exit(2);
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("gitterm-gpui is currently supported only on macOS");
    std::process::exit(1);
}
