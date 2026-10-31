//! Which engine drives the GitTerm-owned browser (TRU-177).
//!
//! GitTerm's browser tools (`browser_mcp`) are the only door agents have to
//! the browser. Behind that door one of two engines runs each operation:
//!
//! - `gitterm`, GitTerm's own CDP driver (`browser_control`), the default;
//! - `agent-browser`, the pinned agent-browser CLI attached to the same
//!   GitTerm-launched Chrome (`agent_browser`).
//!
//! The user picks one in `<shared config>/browser.json`, read once at
//! startup. Either way GitTerm launches and owns Chrome and its profile, and
//! the TRU-176 ownership rules (targets belong to the chat that opened them)
//! hold, because both engines keep their targets in the same registry type.

use crate::agent_browser::{
    AgentBrowserEngine, BrowserDiff, BrowserDiffMode, BrowserNetworkCapture,
    BrowserNetworkCaptureAction, BrowserSnapshotRefs, BrowserViewportRequest,
};
use crate::browser_control::{
    BrowserActionTarget, BrowserCaptureMode, BrowserControlError, BrowserControlService,
    BrowserDiagnostics, BrowserDomOutline, BrowserKey, BrowserLaunchOptions, BrowserLocator,
    BrowserNodeDetails, BrowserNodeSelector, BrowserOwnerDisconnect, BrowserSnapshot,
    BrowserStatus, BrowserTargetInfo, BrowserViewport, BrowserWaitCondition, NavigationResult,
    Result,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// The engine choice, in the shared (not per-instance) config directory.
pub const BROWSER_ENGINE_CONFIG_FILE: &str = "browser.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BrowserEngineKind {
    /// GitTerm's own CDP driver.
    #[default]
    Gitterm,
    /// The pinned agent-browser CLI, attached to GitTerm's Chrome.
    AgentBrowser,
}

/// `browser.json`: `{ "engine": "gitterm" | "agent-browser",
/// "agent_browser_path"?: "…" }`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserEngineConfig {
    pub engine: BrowserEngineKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_browser_path: Option<PathBuf>,
}

/// Read `browser.json` from the shared config directory. No file means the
/// default gitterm engine; a file that cannot be read or parsed is an error,
/// never a silent switch to either engine.
pub fn load_browser_engine_config(
    shared_config_dir: &Path,
) -> std::result::Result<BrowserEngineConfig, String> {
    let path = shared_config_dir.join(BROWSER_ENGINE_CONFIG_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(BrowserEngineConfig::default());
        }
        Err(error) => {
            return Err(format!(
                "could not read the browser engine choice {}: {error}",
                path.display()
            ));
        }
    };
    serde_json::from_str(&text).map_err(|error| {
        format!(
            "{} is not a valid browser engine choice ({error}); expected {{ \"engine\": \"gitterm\" | \"agent-browser\", \"agent_browser_path\"?: \"…\" }}",
            path.display()
        )
    })
}

/// What a click or type acted on: the element a GitTerm locator resolved,
/// or the snapshot ref an agent named (agent-browser engine only).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum BrowserActionOutcome {
    Element(BrowserActionTarget),
    Ref(BrowserRefAction),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BrowserRefAction {
    #[serde(rename = "ref")]
    pub reference: String,
    pub role: Option<String>,
    pub name: Option<String>,
}

/// The engine behind the browser tools. Cloning shares the engine.
#[derive(Clone)]
pub enum BrowserEngine {
    GitTerm(BrowserControlService),
    AgentBrowser(Arc<AgentBrowserEngine>),
    /// `browser.json` named no engine GitTerm can run: every agent call is
    /// refused with this reason rather than run on a guessed engine.
    Refused {
        browser: BrowserControlService,
        reason: String,
    },
}

/// Dispatch one operation to the engine; the refused engine reports why.
macro_rules! engine_call {
    ($engine:expr, $browser:ident => $gitterm:expr, $agent:ident => $agent_browser:expr) => {
        match $engine {
            BrowserEngine::GitTerm($browser) => $gitterm,
            BrowserEngine::AgentBrowser($agent) => $agent_browser,
            BrowserEngine::Refused { reason, .. } => Err(BrowserControlError::new(reason.clone())),
        }
    };
}

impl BrowserEngine {
    /// The engine `browser.json` in `shared_config_dir` selects, driving
    /// the Chrome that `browser` owns.
    pub fn from_shared_config(shared_config_dir: &Path, browser: BrowserControlService) -> Self {
        match load_browser_engine_config(shared_config_dir) {
            Ok(config) => Self::from_config(shared_config_dir, &config, browser),
            Err(reason) => Self::Refused { browser, reason },
        }
    }

    pub fn from_config(
        shared_config_dir: &Path,
        config: &BrowserEngineConfig,
        browser: BrowserControlService,
    ) -> Self {
        match config.engine {
            BrowserEngineKind::Gitterm => Self::GitTerm(browser),
            BrowserEngineKind::AgentBrowser => Self::AgentBrowser(Arc::new(
                AgentBrowserEngine::new(shared_config_dir, config, browser),
            )),
        }
    }

    pub fn kind(&self) -> Option<BrowserEngineKind> {
        match self {
            Self::GitTerm(_) => Some(BrowserEngineKind::Gitterm),
            Self::AgentBrowser(_) => Some(BrowserEngineKind::AgentBrowser),
            Self::Refused { .. } => None,
        }
    }

    /// One line for GitTerm's log: which engine runs, or why none does.
    pub fn describe(&self) -> String {
        match self {
            Self::GitTerm(_) => "gitterm (built-in CDP driver)".to_string(),
            Self::AgentBrowser(engine) => engine.describe(),
            Self::Refused { reason, .. } => format!("refused: {reason}"),
        }
    }

    /// The GitTerm-owned browser service: Chrome's process, telemetry and
    /// evidence, whichever engine drives the pages.
    pub fn service(&self) -> &BrowserControlService {
        match self {
            Self::GitTerm(browser) | Self::Refused { browser, .. } => browser,
            Self::AgentBrowser(engine) => engine.service(),
        }
    }

    pub async fn status_for(&self, owner: &str) -> Result<BrowserStatus> {
        engine_call!(self, browser => browser.status_for(owner).await, engine => engine.status_for(owner).await)
    }

    /// Launch (or adopt) GitTerm's Chrome; the agent-browser engine also
    /// starts here, checking its pinned version.
    pub async fn launch(&self, options: BrowserLaunchOptions) -> Result<BrowserStatus> {
        engine_call!(self, browser => browser.launch(options).await, engine => engine.launch(options).await)
    }

    pub async fn targets(&self, owner: &str) -> Result<Vec<BrowserTargetInfo>> {
        engine_call!(self, browser => browser.targets(owner).await, engine => engine.targets(owner).await)
    }

    pub async fn open_target(
        &self,
        owner: &str,
        name: &str,
        url: &str,
    ) -> Result<BrowserTargetInfo> {
        engine_call!(self, browser => browser.open_target(owner, name, url).await, engine => engine.open_target(owner, name, url).await)
    }

    pub async fn focus_target(&self, owner: &str, name: &str) -> Result<()> {
        engine_call!(self, browser => browser.focus_target(owner, name).await, engine => engine.focus_target(owner, name).await)
    }

    pub async fn close_target(&self, owner: &str, name: &str) -> Result<()> {
        engine_call!(self, browser => browser.close_target(owner, name).await, engine => engine.close_target(owner, name).await)
    }

    pub async fn navigate_in(
        &self,
        owner: &str,
        target: Option<&str>,
        url: &str,
    ) -> Result<NavigationResult> {
        engine_call!(self, browser => browser.navigate_in(owner, target, url).await, engine => engine.navigate_in(owner, target, url).await)
    }

    pub async fn snapshot(&self, owner: &str) -> Result<BrowserSnapshot> {
        self.capture(owner, None, "Snapshot", BrowserCaptureMode::Viewport)
            .await
    }

    pub async fn capture(
        &self,
        owner: &str,
        target: Option<&str>,
        label: &str,
        capture_mode: BrowserCaptureMode,
    ) -> Result<BrowserSnapshot> {
        engine_call!(self, browser => browser.capture(owner, target, label, capture_mode).await, engine => engine.capture(owner, target, label, capture_mode).await)
    }

    pub async fn click_in(
        &self,
        owner: &str,
        target: Option<&str>,
        locator: &BrowserLocator,
    ) -> Result<BrowserActionOutcome> {
        engine_call!(self, browser => browser.click_in(owner, target, locator).await.map(BrowserActionOutcome::Element), engine => engine.click_in(owner, target, locator).await)
    }

    pub async fn type_text_in(
        &self,
        owner: &str,
        target: Option<&str>,
        locator: &BrowserLocator,
        text: &str,
    ) -> Result<BrowserActionOutcome> {
        engine_call!(self, browser => browser.type_text_in(owner, target, locator, text).await.map(BrowserActionOutcome::Element), engine => engine.type_text_in(owner, target, locator, text).await)
    }

    pub async fn press_in(&self, owner: &str, target: Option<&str>, key: BrowserKey) -> Result<()> {
        engine_call!(self, browser => browser.press_in(owner, target, key).await, engine => engine.press_in(owner, target, key).await)
    }

    pub async fn scroll_in(
        &self,
        owner: &str,
        target: Option<&str>,
        delta_x: f64,
        delta_y: f64,
    ) -> Result<()> {
        engine_call!(self, browser => browser.scroll_in(owner, target, delta_x, delta_y).await, engine => engine.scroll_in(owner, target, delta_x, delta_y).await)
    }

    pub async fn resize_in(
        &self,
        owner: &str,
        target: Option<&str>,
        viewport: BrowserViewport,
    ) -> Result<BrowserViewport> {
        engine_call!(self, browser => browser.resize_in(owner, target, viewport).await, engine => engine.resize_in(owner, target, viewport).await)
    }

    pub async fn reload_in(
        &self,
        owner: &str,
        target: Option<&str>,
        ignore_cache: bool,
    ) -> Result<()> {
        engine_call!(self, browser => browser.reload_in(owner, target, ignore_cache).await, engine => engine.reload_in(owner, target, ignore_cache).await)
    }

    pub async fn wait_for_in(
        &self,
        owner: &str,
        target: Option<&str>,
        condition: &BrowserWaitCondition,
        max_wait: Duration,
    ) -> Result<()> {
        engine_call!(self, browser => browser.wait_for_in(owner, target, condition, max_wait).await, engine => engine.wait_for_in(owner, target, condition, max_wait).await)
    }

    pub async fn diagnostics_in(
        &self,
        owner: &str,
        target: Option<&str>,
    ) -> Result<BrowserDiagnostics> {
        engine_call!(self, browser => browser.diagnostics_in(owner, target).await, engine => engine.diagnostics_in(owner, target).await)
    }

    pub async fn dom_outline_in(
        &self,
        owner: &str,
        target: Option<&str>,
    ) -> Result<BrowserDomOutline> {
        engine_call!(self, browser => browser.dom_outline_in(owner, target).await, engine => engine.dom_outline_in(owner, target).await)
    }

    pub async fn node_details_in(
        &self,
        owner: &str,
        target: Option<&str>,
        selector: &BrowserNodeSelector,
    ) -> Result<BrowserNodeDetails> {
        engine_call!(self, browser => browser.node_details_in(owner, target, selector).await, engine => engine.node_details_in(owner, target, selector).await)
    }

    pub async fn disconnect_owner(&self, owner: &str) -> Result<BrowserOwnerDisconnect> {
        engine_call!(self, browser => browser.disconnect_owner(owner).await, engine => engine.disconnect_owner(owner).await)
    }

    /// The chats that closed give up their targets (TRU-176): the tabs stay
    /// open as the user's, and the agent-browser engine also stops their
    /// sessions. Runs for the refused engine too: it is GitTerm's UI, not
    /// an agent, and only releases.
    pub async fn release_owners(&self, owners: &HashSet<String>) -> Result<()> {
        match self {
            Self::GitTerm(browser) | Self::Refused { browser, .. } => {
                browser.release_owners(owners).await
            }
            Self::AgentBrowser(engine) => engine.release_owners(owners).await,
        }
    }

    pub async fn snapshot_refs_in(
        &self,
        owner: &str,
        target: Option<&str>,
    ) -> Result<BrowserSnapshotRefs> {
        engine_call!(self, _browser => Err(agent_browser_only("browser_snapshot_refs")), engine => engine.snapshot_refs_in(owner, target).await)
    }

    pub async fn diff_in(
        &self,
        owner: &str,
        target: Option<&str>,
        baseline_label: &str,
        baseline_target: Option<&str>,
        mode: BrowserDiffMode,
    ) -> Result<BrowserDiff> {
        engine_call!(self, _browser => Err(agent_browser_only("browser_diff")), engine => engine.diff_in(owner, target, baseline_label, baseline_target, mode).await)
    }

    pub async fn network_capture_in(
        &self,
        owner: &str,
        target: Option<&str>,
        action: BrowserNetworkCaptureAction,
    ) -> Result<BrowserNetworkCapture> {
        engine_call!(self, _browser => Err(agent_browser_only("browser_network_capture")), engine => engine.network_capture_in(owner, target, action).await)
    }

    pub async fn viewport_in(
        &self,
        owner: &str,
        target: Option<&str>,
        request: &BrowserViewportRequest,
    ) -> Result<BrowserViewport> {
        engine_call!(self, _browser => Err(agent_browser_only("browser_viewport")), engine => engine.viewport_in(owner, target, request).await)
    }
}

fn agent_browser_only(tool: &str) -> BrowserControlError {
    BrowserControlError::new(format!(
        "{tool} needs the agent-browser engine; this GitTerm runs the gitterm engine"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn no_browser_json_means_the_gitterm_engine() {
        let temp = tempdir().unwrap();
        assert_eq!(
            load_browser_engine_config(temp.path()).unwrap(),
            BrowserEngineConfig::default()
        );
        let engine =
            BrowserEngine::from_shared_config(temp.path(), BrowserControlService::new(temp.path()));
        assert_eq!(engine.kind(), Some(BrowserEngineKind::Gitterm));
    }

    #[test]
    fn browser_json_selects_agent_browser_with_an_optional_path() {
        let temp = tempdir().unwrap();
        std::fs::write(
            temp.path().join(BROWSER_ENGINE_CONFIG_FILE),
            r#"{ "engine": "agent-browser", "agent_browser_path": "/opt/ab/agent-browser" }"#,
        )
        .unwrap();
        let config = load_browser_engine_config(temp.path()).unwrap();
        assert_eq!(config.engine, BrowserEngineKind::AgentBrowser);
        assert_eq!(
            config.agent_browser_path,
            Some(PathBuf::from("/opt/ab/agent-browser"))
        );
        std::fs::write(
            temp.path().join(BROWSER_ENGINE_CONFIG_FILE),
            r#"{ "engine": "gitterm" }"#,
        )
        .unwrap();
        assert_eq!(
            load_browser_engine_config(temp.path()).unwrap().engine,
            BrowserEngineKind::Gitterm
        );
    }

    #[tokio::test]
    async fn an_unreadable_choice_refuses_instead_of_guessing_an_engine() {
        let temp = tempdir().unwrap();
        for invalid in [
            r#"{ "engine": "playwright" }"#,
            r#"{ "agent_browser_path": "/x" }"#,
            r#"{ "engine": "gitterm", "extra": 1 }"#,
            "not json",
        ] {
            std::fs::write(temp.path().join(BROWSER_ENGINE_CONFIG_FILE), invalid).unwrap();
            let error = load_browser_engine_config(temp.path()).unwrap_err();
            assert!(error.contains("browser.json"), "{error}");
            let engine = BrowserEngine::from_shared_config(
                temp.path(),
                BrowserControlService::new(temp.path()),
            );
            assert_eq!(engine.kind(), None, "{invalid}");
            let refused = engine.targets("chat-a").await.unwrap_err().to_string();
            assert_eq!(refused, error);
        }
    }
}
