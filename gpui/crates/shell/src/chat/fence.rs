//! A proposed launch contract, not execution authority. Keep C2 closed until the
//! shared launch API carries these arguments and managed writers are confined.
#[cfg(test)]
use anyhow::{Result, ensure};

pub(super) const FLAGS: [&str; 3] = ["--restricted", "--strict-mcp-config", "--safe-mode"];

#[cfg(test)]
pub(super) fn check_help(help: &str) -> Result<()> {
    for flag in FLAGS.into_iter().chain(["--permission-mode"]) {
        ensure!(
            option(help, flag).is_some(),
            "Claude preview requires documented CLI option {flag}"
        );
    }
    let mode = option(help, "--permission-mode").expect("checked option");
    // Do not guess aliases from the mode's UI label. The installed help must
    // advertise the exact requested mode before this contract can be used.
    ensure!(
        mode.contains("\"default\"") || mode.contains("'default'"),
        "Claude preview requires --permission-mode default; installed help does not advertise it"
    );
    Ok(())
}

#[cfg(test)]
fn option<'a>(help: &'a str, flag: &str) -> Option<&'a str> {
    let start = help
        .lines()
        .find(|line| line.split_whitespace().next() == Some(flag))?;
    let offset = help.find(start)?;
    let section = &help[offset..];
    let end = section
        .match_indices('\n')
        .find_map(|(offset, _)| {
            let rest = &section[offset + 1..];
            let line = rest.lines().next().unwrap_or_default().trim_start();
            (line.starts_with('-') || line == "Commands:").then_some(offset)
        })
        .unwrap_or(section.len());
    Some(&section[..end])
}

/// For the denial diagnostic/tests until the core launch port can carry the fence.
pub(super) fn arguments() -> Vec<String> {
    FLAGS
        .into_iter()
        .chain(["--permission-mode", "default"])
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    const HELP: &str = "Options:\n  --restricted  restricted tools\n  --strict-mcp-config  no implicit MCP\n  --safe-mode  customizations off\n  --permission-mode <mode>\n    (choices: \"default\", \"acceptEdits\")\n  --model <model>  model\n";

    #[test]
    fn exact_fence_and_missing_capabilities_fail_closed() {
        check_help(HELP).unwrap();
        assert_eq!(
            arguments(),
            [
                "--restricted",
                "--strict-mcp-config",
                "--safe-mode",
                "--permission-mode",
                "default"
            ]
        );
        for flag in FLAGS {
            assert!(check_help(&HELP.replace(flag, "--unknown")).is_err());
        }
        assert!(check_help(&HELP.replace("\"default\"", "\"manual\"")).is_err());
        assert!(
            check_help("description mentions --restricted --strict-mcp-config --safe-mode")
                .is_err()
        );
        assert!(check_help(&HELP.replace("\"default\"", "\"defaultPlus\"")).is_err());
        for bypass in [
            "--dangerously-skip-permissions",
            "--allow-dangerously-skip-permissions",
            "--add-dir",
            "--mcp-config",
            "--tools",
            "--bare",
            "--resume",
        ] {
            assert!(!arguments().iter().any(|arg| arg == bypass));
        }
    }
}
