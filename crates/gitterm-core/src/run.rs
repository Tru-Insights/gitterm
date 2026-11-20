//! Pure bottom-panel run and quick-command policy. Hosts own terminals and effects.

use std::path::{Path, PathBuf};

use crate::settings::QuickCommand;

/// Host repo identities remain opaque to core.
#[derive(Debug, Clone, Copy)]
pub struct RunTabFact<'a, R> {
    pub repo: R,
    pub command: &'a str,
    pub detected_url: Option<&'a str>,
}

#[derive(Debug, Clone, Copy)]
pub struct BottomTerminalFact<'a, R> {
    pub alive: bool,
    pub run: Option<RunTabFact<'a, R>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunTabAction {
    Focus(usize),
    Restart(usize),
    Spawn,
}

/// One run tab per repo per workspace; a URL does not change the Run action.
pub fn run_tab_action<'a, R: PartialEq + 'a>(
    terminals: impl IntoIterator<Item = BottomTerminalFact<'a, R>>,
    repo: R,
    command: &str,
) -> RunTabAction {
    for (idx, terminal) in terminals.into_iter().enumerate() {
        if let Some(run) = terminal.run {
            if run.repo == repo {
                return if terminal.alive && run.command == command {
                    RunTabAction::Focus(idx)
                } else {
                    RunTabAction::Restart(idx)
                };
            }
        }
    }
    RunTabAction::Spawn
}

#[derive(Debug, Clone, Copy)]
pub struct RunCommandFact<'a> {
    pub is_local: bool,
    pub command: Option<&'a str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunCommandUnavailable {
    NotListed,
    MissingRepo,
    RemoteRepo,
    MissingCommand,
}

/// Validate in Iced's original order; the host emits its existing messages.
pub fn resolve_run_command(
    listed: bool,
    repo: Option<RunCommandFact<'_>>,
) -> Result<&str, RunCommandUnavailable> {
    if !listed {
        return Err(RunCommandUnavailable::NotListed);
    }
    let repo = repo.ok_or(RunCommandUnavailable::MissingRepo)?;
    if !repo.is_local {
        return Err(RunCommandUnavailable::RemoteRepo);
    }
    repo.command.ok_or(RunCommandUnavailable::MissingCommand)
}

/// Opening the selected run tab's URL is independent of its process state.
pub fn run_tab_url<R>(terminal: Option<BottomTerminalFact<'_, R>>) -> Option<&str> {
    terminal?.run?.detected_url
}

/// A picked repo directory overrides the workspace home for a plain terminal.
pub fn new_bottom_terminal_dir(home: &Path, picked: Option<PathBuf>) -> PathBuf {
    picked.unwrap_or_else(|| home.to_path_buf())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuickCommandTarget {
    SelectedPlainTerminal,
    Spawn { cwd: PathBuf },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuickCommandLaunch {
    pub target: QuickCommandTarget,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuickCommandUnavailable {
    NoWorkspace,
}

/// Use the selected plain terminal, even if exited; otherwise create one at home.
/// Unknown commands are ignored; a missing workspace tells the host to return early.
pub fn resolve_quick_command<R>(
    commands: &[QuickCommand],
    index: usize,
    selected: Option<BottomTerminalFact<'_, R>>,
    home: Option<&Path>,
) -> Result<Option<QuickCommandLaunch>, QuickCommandUnavailable> {
    let Some(command) = commands.get(index) else {
        return Ok(None);
    };
    let target = if selected.is_some_and(|terminal| terminal.run.is_none()) {
        QuickCommandTarget::SelectedPlainTerminal
    } else {
        QuickCommandTarget::Spawn {
            cwd: new_bottom_terminal_dir(home.ok_or(QuickCommandUnavailable::NoWorkspace)?, None),
        }
    };
    Ok(Some(QuickCommandLaunch {
        target,
        bytes: format!("{}\n", command.command).into_bytes(),
    }))
}

/// Preserve a later selection, or select the removed terminal's neighbour.
pub fn bottom_selection_after_close(
    selected: Option<usize>,
    closed: usize,
    remaining: usize,
) -> Option<usize> {
    if remaining == 0 {
        return None;
    }
    match selected? {
        idx if idx > closed => Some(idx - 1),
        idx if idx == closed => Some(closed.min(remaining - 1)),
        idx => Some(idx),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(alive: bool) -> BottomTerminalFact<'static, u64> {
        BottomTerminalFact { alive, run: None }
    }

    fn run<'a>(alive: bool, command: &'a str, url: Option<&'a str>) -> BottomTerminalFact<'a, u64> {
        BottomTerminalFact {
            alive,
            run: Some(RunTabFact {
                repo: 1,
                command,
                detected_url: url,
            }),
        }
    }

    #[test]
    fn a_run_action_opens_one_tab_and_then_reuses_it() {
        assert_eq!(
            run_tab_action([], 1_u64, "npm run dev"),
            RunTabAction::Spawn
        );
        assert_eq!(
            run_tab_action([plain(true)], 1, "npm run dev"),
            RunTabAction::Spawn
        );
        for url in [None, Some("http://localhost:5173/")] {
            for alive in [false, true] {
                for command in ["npm run dev", "npm run start"] {
                    let terminals = [plain(true), run(alive, command, url)];
                    let expected = if alive && command == "npm run dev" {
                        RunTabAction::Focus(1)
                    } else {
                        RunTabAction::Restart(1)
                    };
                    assert_eq!(run_tab_action(terminals, 1, "npm run dev"), expected);
                    assert_eq!(
                        run_tab_action(terminals, 2, "cargo run"),
                        RunTabAction::Spawn
                    );
                }
            }
        }
        // Even malformed duplicate state uses the first matching tab.
        assert_eq!(
            run_tab_action([run(false, "old", None), run(true, "new", None)], 1, "new"),
            RunTabAction::Restart(0)
        );
    }

    #[test]
    fn run_command_validation_keeps_its_order_and_accepts_exact_command() {
        assert_eq!(
            resolve_run_command(false, None),
            Err(RunCommandUnavailable::NotListed)
        );
        assert_eq!(
            resolve_run_command(true, None),
            Err(RunCommandUnavailable::MissingRepo)
        );
        assert_eq!(
            resolve_run_command(
                true,
                Some(RunCommandFact {
                    is_local: false,
                    command: None
                })
            ),
            Err(RunCommandUnavailable::RemoteRepo)
        );
        assert_eq!(
            resolve_run_command(
                true,
                Some(RunCommandFact {
                    is_local: true,
                    command: None
                })
            ),
            Err(RunCommandUnavailable::MissingCommand)
        );
        for command in ["", "  cargo run\n"] {
            assert_eq!(
                resolve_run_command(
                    true,
                    Some(RunCommandFact {
                        is_local: true,
                        command: Some(command)
                    })
                ),
                Ok(command)
            );
            assert_eq!(
                resolve_run_command(
                    false,
                    Some(RunCommandFact {
                        is_local: false,
                        command: Some(command)
                    })
                ),
                Err(RunCommandUnavailable::NotListed)
            );
        }
    }

    #[test]
    fn only_the_selected_run_tabs_url_is_opened_even_after_exit() {
        assert_eq!(run_tab_url::<u64>(None), None);
        assert_eq!(run_tab_url(Some(plain(true))), None);
        assert_eq!(run_tab_url(Some(run(true, "make", None))), None);
        for alive in [false, true] {
            assert_eq!(
                run_tab_url(Some(run(alive, "make", Some("http://localhost/")))),
                Some("http://localhost/")
            );
        }
    }

    #[test]
    fn quick_commands_preserve_bytes_and_choose_plain_terminal_or_home() {
        let home = Path::new("/w/home");
        for command in ["echo café", "", "echo hi\n"] {
            let commands = [QuickCommand {
                name: "example".into(),
                command: command.into(),
            }];
            assert_eq!(
                resolve_quick_command::<u64>(&commands, 1, None, Some(home)),
                Ok(None)
            );
            assert_eq!(
                resolve_quick_command::<u64>(&commands, 1, None, None),
                Ok(None)
            );
            assert_eq!(
                resolve_quick_command::<u64>(&commands, 0, None, None),
                Err(QuickCommandUnavailable::NoWorkspace)
            );
            for alive in [false, true] {
                let launch = resolve_quick_command(&commands, 0, Some(plain(alive)), Some(home))
                    .unwrap()
                    .unwrap();
                assert_eq!(launch.target, QuickCommandTarget::SelectedPlainTerminal);
                assert_eq!(launch.bytes, format!("{command}\n").into_bytes());
                for selected in [None, Some(run(alive, "make", Some("http://localhost/")))] {
                    let launch = resolve_quick_command(&commands, 0, selected, Some(home))
                        .unwrap()
                        .unwrap();
                    assert_eq!(
                        launch.target,
                        QuickCommandTarget::Spawn { cwd: home.into() }
                    );
                    assert_eq!(launch.bytes, format!("{command}\n").into_bytes());
                }
            }
        }
    }

    #[test]
    fn a_new_bottom_terminal_opens_in_the_workspace_home() {
        let home = Path::new("/w/work/launch");
        assert_eq!(new_bottom_terminal_dir(home, None), home);
        assert_eq!(
            new_bottom_terminal_dir(home, Some("/w/api".into())),
            Path::new("/w/api")
        );
    }

    #[test]
    fn closing_a_bottom_terminal_keeps_a_sensible_selection() {
        assert_eq!(bottom_selection_after_close(Some(2), 0, 2), Some(1));
        assert_eq!(bottom_selection_after_close(Some(0), 1, 2), Some(0));
        assert_eq!(bottom_selection_after_close(Some(1), 1, 2), Some(1));
        assert_eq!(bottom_selection_after_close(Some(2), 2, 2), Some(1));
        assert_eq!(bottom_selection_after_close(Some(0), 0, 0), None);
        assert_eq!(bottom_selection_after_close(None, 0, 1), None);
    }
}
