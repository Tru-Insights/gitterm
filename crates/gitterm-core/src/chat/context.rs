//! Explicit preview roots and a status-probe specification, not a launcher.
//! Real execution remains unavailable. `CLAUDE_CONFIG_DIR` does not isolate
//! keyless Console sign-ins; settings/MCP flags do not confine a process tree.
use crate::preview::Preview;
use std::{
    collections::BTreeMap,
    ffi::OsString,
    io,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Roots {
    pub config: PathBuf,
    pub history: PathBuf,
    pub snapshots: PathBuf,
    pub transcripts: PathBuf,
    pub provider: PathBuf,
}
impl Roots {
    pub fn from_preview(preview: &Preview) -> io::Result<Self> {
        let roots = Self {
            config: preview.config(),
            history: preview.history(),
            snapshots: preview.snapshots(),
            transcripts: preview.transcripts(),
            provider: preview.provider(),
        };
        for path in [
            &roots.config,
            &roots.history,
            &roots.snapshots,
            &roots.transcripts,
            &roots.provider,
        ] {
            preview.scoped(path)?;
        }
        Ok(roots)
    }
}
#[derive(Debug)]
pub struct ProbeSpec {
    executable: PathBuf,
    args: Vec<OsString>,
    env: BTreeMap<OsString, OsString>,
    cwd: PathBuf,
    provider: PathBuf,
}
impl ProbeSpec {
    /// Descriptive only. No production API spawns this command. The executable
    /// must be explicit; it is never located via the inherited PATH.
    pub fn claude_status(preview: &Preview, executable: &Path) -> io::Result<Self> {
        if !executable.is_absolute()
            || executable
                .components()
                .any(|p| matches!(p, std::path::Component::ParentDir))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "explicit absolute executable required",
            ));
        }
        let roots = Roots::from_preview(preview)?;
        let home = preview.root().join("child/home");
        let temp = preview.root().join("child/tmp");
        let cwd = preview.root().join("child/neutral");
        for path in [&home, &temp, &cwd] {
            preview.scoped(path)?;
        }
        let env = BTreeMap::from([
            ("HOME".into(), home.into_os_string()),
            (
                "XDG_CONFIG_HOME".into(),
                preview.root().join("child/config").into_os_string(),
            ),
            ("TMPDIR".into(), temp.clone().into_os_string()),
            ("TMP".into(), temp.clone().into_os_string()),
            ("TEMP".into(), temp.into_os_string()),
            ("PATH".into(), "/usr/bin:/bin".into()),
            (
                "CLAUDE_CONFIG_DIR".into(),
                roots.provider.clone().into_os_string(),
            ),
            (
                "GITTERM_V6_CONFIG_DIR".into(),
                roots.config.into_os_string(),
            ),
            (
                "GITTERM_GPUI_CONFIG_DIR".into(),
                preview.root().as_os_str().to_os_string(),
            ),
        ]);
        // Only the documented subcommand. No unverified combination of global
        // settings/MCP flags is claimed to isolate auth subcommands. Real probe
        // execution is unavailable, including file/managed MCP initialization.
        let args = ["auth", "status"].into_iter().map(OsString::from).collect();
        Ok(Self {
            executable: executable.into(),
            args,
            env,
            cwd,
            provider: roots.provider,
        })
    }
    pub fn executable(&self) -> &Path {
        &self.executable
    }
    pub fn args(&self) -> &[OsString] {
        &self.args
    }
    pub fn environment(&self) -> &BTreeMap<OsString, OsString> {
        &self.env
    }
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }
    /// Useful for inspection and fake-child tests. Caller cannot obtain this
    /// command in production; C2 deliberately provides no real probe executor.
    #[cfg(test)]
    fn command(&self) -> std::process::Command {
        let mut command = std::process::Command::new(&self.executable);
        command
            .env_clear()
            .envs(&self.env)
            .args(&self.args)
            .current_dir(&self.cwd);
        command
    }
    pub fn unavailable(&self) -> ProbeStatus {
        ProbeStatus::Unavailable
    }
    pub fn parse(&self, exit: Option<i32>, timed_out: bool, stdout: &[u8]) -> ProbeStatus {
        if timed_out || stdout.len() > 16 * 1024 {
            return ProbeStatus::Unavailable;
        }
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Status {
            auth_method: String,
            config_directory: PathBuf,
        }
        let Ok(status) = serde_json::from_slice::<Status>(stdout) else {
            return ProbeStatus::Unavailable;
        };
        // Compare diagnostics literally, not via canonicalization or fallback.
        if status.config_directory != self.provider {
            return ProbeStatus::Unavailable;
        }
        match (exit, status.auth_method.as_str()) {
            (Some(1), "none") => ProbeStatus::NotLoggedIn,
            (Some(0), "claude.ai") => ProbeStatus::ReportedLogin,
            _ => ProbeStatus::Unavailable,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeStatus {
    Unavailable,
    NotLoggedIn,
    ReportedLogin,
}
impl ProbeStatus {
    pub fn permits_activation(self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preview::ProtectedRoots;
    fn fixture() -> (tempfile::TempDir, Preview) {
        let dir = tempfile::tempdir().unwrap();
        let p = Preview::validate(
            &dir.path().join("preview"),
            ProtectedRoots::new(&dir.path().join("host"), []).unwrap(),
        )
        .unwrap();
        (dir, p)
    }
    #[test]
    fn exact_command_and_narrow_status_contract() {
        let (_dir, preview) = fixture();
        let spec = ProbeSpec::claude_status(&preview, Path::new("/fake/claude")).unwrap();
        let command = spec.command();
        assert_eq!(command.get_program(), "/fake/claude");
        assert_eq!(command.get_args().collect::<Vec<_>>(), ["auth", "status"]);
        assert_eq!(command.get_current_dir(), Some(spec.cwd()));
        assert_eq!(command.get_envs().count(), 9);
        assert_eq!(
            spec.environment()
                .keys()
                .map(|k| k.to_str().unwrap())
                .collect::<Vec<_>>(),
            [
                "CLAUDE_CONFIG_DIR",
                "GITTERM_GPUI_CONFIG_DIR",
                "GITTERM_V6_CONFIG_DIR",
                "HOME",
                "PATH",
                "TEMP",
                "TMP",
                "TMPDIR",
                "XDG_CONFIG_HOME"
            ]
        );
        let good = serde_json::to_vec(
            &serde_json::json!({"authMethod":"claude.ai","configDirectory":preview.provider()}),
        )
        .unwrap();
        assert_eq!(
            spec.parse(Some(0), false, &good),
            ProbeStatus::ReportedLogin
        );
        assert!(!spec.parse(Some(0), false, &good).permits_activation());
        assert_eq!(spec.parse(Some(1), false, &good), ProbeStatus::Unavailable);
        assert_eq!(spec.parse(Some(0), true, &good), ProbeStatus::Unavailable);
        for value in [b"{}".as_slice(), b"not json", &vec![b' '; 16385]] {
            assert_eq!(spec.parse(Some(0), false, value), ProbeStatus::Unavailable);
        }
        for method in [
            "api_key",
            "oauth_token",
            "third_party",
            "unknown",
            "api_key_helper",
        ] {
            let value = serde_json::to_vec(
                &serde_json::json!({"authMethod":method,"configDirectory":preview.provider()}),
            )
            .unwrap();
            assert_eq!(spec.parse(Some(0), false, &value), ProbeStatus::Unavailable);
        }
        let value = serde_json::to_vec(
            &serde_json::json!({"authMethod":"none","configDirectory":preview.provider()}),
        )
        .unwrap();
        assert_eq!(spec.parse(Some(1), false, &value), ProbeStatus::NotLoggedIn);
        assert!(!spec.parse(Some(1), false, &value).permits_activation());
        let value = br#"{"authMethod":"claude.ai","configDirectory":"/elsewhere"}"#;
        assert_eq!(spec.parse(Some(0), false, value), ProbeStatus::Unavailable);
    }
    #[cfg(unix)]
    #[test]
    fn fake_child_receives_only_allowlist_without_mcp_credentials() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, preview) = fixture();
        let fake = dir.path().join("fake-status");
        // A synthetic executable, never the installed Claude CLI.
        std::fs::write(
            &fake,
            "#!/bin/sh\n/usr/bin/env\nprintf 'ARG:%s\\n' \"$@\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
        let spec = ProbeSpec::claude_status(&preview, &fake).unwrap();
        preview.create_private_dir(spec.cwd()).unwrap();
        let output = spec.command().output().unwrap();
        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).unwrap();
        for (key, value) in spec.environment() {
            assert!(stdout
                .lines()
                .any(|l| l == format!("{}={}", key.to_string_lossy(), value.to_string_lossy())));
        }
        assert!(stdout.contains("ARG:auth\nARG:status"));
        for line in stdout.lines().filter(|line| !line.starts_with("ARG:")) {
            let (key, _) = line.split_once('=').unwrap();
            assert!(
                spec.environment().contains_key(std::ffi::OsStr::new(key))
                    || ["PWD", "SHLVL", "_"].contains(&key),
                "unexpected inherited variable {key}"
            );
        }
        for forbidden in [
            "ANTHROPIC_API_KEY=",
            "HTTP_PROXY=",
            "DYLD_INSERT_LIBRARIES=",
            "GITTERM_TASK_MCP_URL=",
        ] {
            assert!(!stdout.contains(forbidden));
        }
    }
}

#[cfg(all(test, unix))]
mod timeout_test {
    use super::*;
    #[test]
    fn fake_child_timeout_remains_unavailable() {
        use std::{
            os::unix::fs::PermissionsExt,
            time::{Duration, Instant},
        };
        let dir = tempfile::tempdir().unwrap();
        let p = Preview::validate(
            &dir.path().join("preview"),
            crate::preview::ProtectedRoots::new(&dir.path().join("host"), []).unwrap(),
        )
        .unwrap();
        let fake = dir.path().join("fake-timeout");
        std::fs::write(&fake, "#!/bin/sh\nexec /bin/sleep 10\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
        let spec = ProbeSpec::claude_status(&p, &fake).unwrap();
        p.create_private_dir(spec.cwd()).unwrap();
        let mut child = spec.command().spawn().unwrap();
        let deadline = Instant::now() + Duration::from_millis(50);
        while Instant::now() < deadline {
            assert!(child.try_wait().unwrap().is_none());
            std::thread::sleep(Duration::from_millis(5));
        }
        child.kill().unwrap();
        let status = child.wait().unwrap();
        assert_eq!(
            spec.parse(status.code(), true, b""),
            ProbeStatus::Unavailable
        );
        assert!(!spec.unavailable().permits_activation());
    }
}
