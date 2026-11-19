#[cfg(target_os = "macos")]
extern crate gpui_kit as gpui;

#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "macos")]
mod launch;

#[cfg(target_os = "macos")]
fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

#[cfg(target_os = "macos")]
fn run() -> Result<(), String> {
    let route = launch::parse(std::env::args_os().skip(1)).unwrap_or_else(|| usage());
    // Standalone fixture routes need validated roots but no writable lease.
    let preview = match &route {
        launch::Launch::Shell { .. } => None,
        _ => Some(
            gitterm_gpui_shell::preview::roots_from_environment(false)
                .map_err(|error| format!("{error:#}"))?,
        ),
    };
    if let Some(preview) = &preview {
        route
            .authorize(preview)
            .map_err(|error| error.to_string())?;
    }
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
