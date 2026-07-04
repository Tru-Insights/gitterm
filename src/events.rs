#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SidebarMode {
    Git,
    Files,
    /// Shows skills / plugins / hooks / settings. UI labels this tab "Agent"
    /// (formerly "Claude") since these are agent-runtime concerns.
    Claude,
    /// Legacy variant — no UI entry. Kept so existing `view_agent_sidebar`
    /// and `agent_sidebar` field references compile until a future cleanup.
    Agent,
    /// Lists `.md` files from the active workspace's `.plans/` directory.
    /// Clicking an item opens the plans viewer in the right pane.
    Plans,
    /// Read-only browser over harness conversations (claude transcripts),
    /// scoped to the active workspace by default. TRU-78.
    Chats,
    /// Lists persistent remote tmux sessions reachable over SSH.
    Remote,
}
