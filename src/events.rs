/// The Tasks mode is retired from the sidebar (TRU-192): the task system,
/// the task switcher and the task MCP tools stay; only the header and rail
/// entry is off. Flip to bring the mode back.
pub const TASKS_MODE_ENABLED: bool = false;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SidebarMode {
    /// Reached from a repo row in Work (TRU-192), with a back control.
    Git,
    Files,
    /// Shows skills / plugins / hooks / settings. Reached from the agent
    /// line in Work (TRU-192), with a back control.
    Claude,
    /// "Find": read-only browser over harness conversations (claude
    /// transcripts), on this Mac or everywhere. TRU-78.
    Chats,
    /// Durable cross-workspace task registry and launch surface; off the
    /// header unless [`TASKS_MODE_ENABLED`].
    Tasks,
    /// "Work": the workspace read as one piece of work: coordinators and
    /// fan-out, repos, documents, owned branches with live git state,
    /// issue keys (TRU-167, `workspace_overview`). The default.
    Workspace,
}

impl SidebarMode {
    /// The sidebar header's modes, in order (TRU-192).
    pub fn header_modes() -> Vec<SidebarMode> {
        let mut modes = vec![
            SidebarMode::Workspace,
            SidebarMode::Files,
            SidebarMode::Chats,
        ];
        if TASKS_MODE_ENABLED {
            modes.push(SidebarMode::Tasks);
        }
        modes
    }

    /// The header mode this mode shows under: Git and the agent
    /// configuration open from Work.
    pub fn header_mode(self) -> SidebarMode {
        match self {
            SidebarMode::Git | SidebarMode::Claude => SidebarMode::Workspace,
            mode => mode,
        }
    }

    /// The header label and the collapsed rail's glyph.
    pub fn label_and_glyph(self) -> (&'static str, &'static str) {
        match self {
            SidebarMode::Workspace => ("Work", "\u{25A6}"),
            SidebarMode::Files => ("Files", "\u{1F4C1}"),
            SidebarMode::Chats => ("Find", "\u{224B}"),
            SidebarMode::Tasks => ("Tasks", "\u{25C8}"),
            SidebarMode::Git => ("Git", "\u{2387}"),
            SidebarMode::Claude => ("Agent", "\u{2726}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_header_is_work_files_and_find_in_that_order() {
        let labels: Vec<&str> = SidebarMode::header_modes()
            .into_iter()
            .map(|mode| mode.label_and_glyph().0)
            .collect();
        assert_eq!(labels, ["Work", "Files", "Find"]);
        assert_eq!(SidebarMode::header_modes()[0], SidebarMode::Workspace);
    }

    #[test]
    fn git_and_the_agent_view_show_under_work() {
        assert_eq!(SidebarMode::Git.header_mode(), SidebarMode::Workspace);
        assert_eq!(SidebarMode::Claude.header_mode(), SidebarMode::Workspace);
        for mode in SidebarMode::header_modes() {
            assert_eq!(mode.header_mode(), mode);
        }
    }
}
