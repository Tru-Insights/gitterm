//! Iced menu/shortcut inventory installed by the shell's `install_app_menus`.
//! Bind first: GPUI derives native menu accelerators from its keymap.
//! Chat find/copy actions are adapted to the existing NativeChat implementation;
//! the shell does not install NativeChat's superseded local key registrations.
//!
//! `IcedMain` means the main key handler is reachable (no consuming modal or
//! Usage view); `IcedChat` means a chat page owns focus and forwards host keys.
//! `IcedAgentTab` tracks the active tab kind even when a bottom terminal has focus;
//! `IcedChatFindAllowed` includes Iced's visible-chat and native-input focus guards.
//! `IcedUsage`, `IcedTaskSwitcher`, `IcedPark`, `IcedDiff`, `IcedTaskContext`,
//! `IcedTaskSwitcherInput`, `IcedTerminalSearch`, `IcedEditable`, `IcedChatFind`,
//! `IcedComposer`, `IcedSlash`, `IcedReview` and `IcedHelp` are
//! state contexts to be supplied by the host. `IcedStt` gates dictation builds.
//! A host must consume modal keys and resolve Escape in Iced's priority order
//! before allowing terminal input. These predicates do not implement that policy.
//!
//! Native items are enabled in Iced's builder; GPUI additionally validates action
//! availability, so the installer must register their handlers to keep them enabled.

use gitterm_core::settings::ChatWidth;
use gpui::{Action, KeyBinding, Menu, MenuItem, SystemMenuType, actions};

actions!(
    gitterm_iced,
    [
        About,
        UseV6State,
        Hide,
        HideOthers,
        ShowAll,
        Quit,
        Minimize,
        Maximize,
        Fullscreen,
        IncreaseTerminalFont,
        DecreaseTerminalFont,
        ClearTerminal,
        IncreaseUiFont,
        DecreaseUiFont,
        IncreaseChatFont,
        DecreaseChatFont,
        ToggleTheme,
        ToggleTerminalLogMirroring,
        OpenPlansViewer,
        TaskSwitcherOpen,
        TaskSwitcherClose,
        TaskSwitcherNext,
        TaskSwitcherPrevious,
        TaskSwitcherActivateFirst,
        ToggleSidebar,
        ConsoleToggle,
        RunRepoCommand,
        WorkspaceClose,
        UsageViewToggle,
        ChatsOverviewOpen,
        ToggleHelp,
        Escape,
        ParkSubmit,
        DebugSpawnAgentTab,
        AgentFindOpen,
        ToggleSearch,
        SearchNext,
        SttToggle,
        AttentionJumpNext,
        OpenFolder,
        TaskContextBack,
        TaskOverview,
        EditFile,
        Copy,
        CopyMarkdown,
        Cut,
        Paste,
        SelectAll,
        Undo,
        Redo,
        SubmitPrompt,
        Newline,
        CycleMode,
        FindNext,
        FindPrevious,
        CloseFind,
        SlashNext,
        SlashPrevious,
        FillSlash,
        SubmitSlash,
        CloseSlash,
        SubmitReview,
        CloseReview
    ]
);

macro_rules! indexed_action {
    ($name:ident) => {
        #[derive(Clone, PartialEq, Debug, Action)]
        #[action(namespace = gitterm_iced, no_json)]
        pub struct $name {
            /// Zero-based index, resolved against Iced's visible order by the host.
            pub index: usize,
        }
    };
}
indexed_action!(WorkspaceSelect);
indexed_action!(TabSelect);
indexed_action!(LaunchAgentPreset);
indexed_action!(TaskChildLaunchPreset);

/// Preview chrome keeps owner actions visible without invoking their effects.
#[derive(Clone, PartialEq, Debug, Action)]
#[action(namespace = gitterm_iced, no_json)]
pub struct PreviewUnavailable {
    pub label: String,
}

/// The Iced key handler computes a FileSelectByIndex target from current state.
#[derive(Clone, PartialEq, Debug)]
pub enum FileSelection {
    Next,
    Previous,
    First,
    Last,
}

#[derive(Clone, PartialEq, Debug, Action)]
#[action(namespace = gitterm_iced, no_json)]
pub struct FileSelectByIndex {
    pub selection: FileSelection,
}

#[derive(Clone, PartialEq, Debug, Action)]
#[action(namespace = gitterm_iced, no_json)]
pub struct SetChatWidth {
    pub width: ChatWidth,
}

/// Iced's actual macOS menu bar. There is deliberately no File, Edit or Help menu.
pub fn menus() -> Vec<Menu> {
    vec![
        Menu::new("GitTerm V6").items([
            MenuItem::action("About GitTerm V6", About),
            MenuItem::action("Use V6 state…", UseV6State),
            MenuItem::separator(),
            MenuItem::os_submenu("Services", SystemMenuType::Services),
            MenuItem::separator(),
            MenuItem::action("Hide GitTerm V6", Hide),
            MenuItem::action("Hide Others", HideOthers),
            MenuItem::action("Show All", ShowAll),
            MenuItem::separator(),
            MenuItem::action("Quit GitTerm V6", Quit),
        ]),
        Menu::new("View").items([
            MenuItem::submenu(Menu::new("Terminal Font").items([
                MenuItem::action("Increase", IncreaseTerminalFont),
                MenuItem::action("Decrease", DecreaseTerminalFont),
                MenuItem::action("Clear Terminal", ClearTerminal),
            ])),
            MenuItem::submenu(Menu::new("Sidebar Font").items([
                MenuItem::action("Increase", IncreaseUiFont),
                MenuItem::action("Decrease", DecreaseUiFont),
            ])),
            MenuItem::action("Increase Chat Text", IncreaseChatFont),
            MenuItem::action("Decrease Chat Text", DecreaseChatFont),
            MenuItem::submenu(
                Menu::new("Chat width").items(
                    ChatWidth::ALL
                        .map(|width| MenuItem::action(width.to_string(), SetChatWidth { width })),
                ),
            ),
            MenuItem::separator(),
            MenuItem::action("Toggle Light/Dark Theme", ToggleTheme),
            MenuItem::action("Toggle Terminal Log Mirroring", ToggleTerminalLogMirroring),
            MenuItem::separator(),
            MenuItem::action("Plans Viewer", OpenPlansViewer),
        ]),
        Menu::new("Window").items([
            MenuItem::action("Minimize", Minimize),
            MenuItem::action("Zoom", Maximize),
            MenuItem::separator(),
            MenuItem::action("Toggle Full Screen", Fullscreen),
        ]),
    ]
}

macro_rules! shortcuts {
    ($(($key:literal, $action:ident $( { $field:ident: $value:expr } )?, $context:expr)),* $(,)?) => {
        /// Canonical macOS chords, Iced command names, and GPUI focus predicates.
        /// Contexts preserve page-local commands rather than making them app-wide.
        pub fn iced_shortcuts() -> &'static [(&'static str, &'static str, Option<&'static str>)] {
            &[$(($key, stringify!($action), $context)),*]
        }

        pub fn key_bindings() -> Vec<KeyBinding> {
            vec![$(KeyBinding::new($key, $action $( { $field: $value } )?, $context)),*]
        }
    };
}

shortcuts![
    // Native predefined accelerators (muda's macOS defaults).
    ("cmd-h", Hide, None),
    ("cmd-alt-h", HideOthers, None),
    ("cmd-q", Quit, None),
    ("cmd-m", Minimize, None),
    ("cmd-ctrl-f", Fullscreen, None),
    // Native View accelerators. The main handler also routes +/- by active tab.
    (
        "cmd-=",
        IncreaseTerminalFont,
        Some("GitTermShell && IcedMain && !IcedAgentTab")
    ),
    (
        "cmd--",
        DecreaseTerminalFont,
        Some("GitTermShell && IcedMain && !IcedAgentTab")
    ),
    (
        "cmd-=",
        IncreaseChatFont,
        Some("GitTermShell && ((IcedMain && IcedAgentTab) || IcedChat)")
    ),
    (
        "cmd--",
        DecreaseChatFont,
        Some("GitTermShell && ((IcedMain && IcedAgentTab) || IcedChat)")
    ),
    ("cmd-shift-=", IncreaseUiFont, None),
    ("cmd-shift--", DecreaseUiFont, None),
    ("cmd-k", ClearTerminal, Some("GitTermShell && IcedMain")),
    ("cmd-shift-t", ToggleTheme, None),
    ("cmd-shift-l", ToggleTerminalLogMirroring, None),
    ("cmd-shift-p", OpenPlansViewer, None),
    (
        "ctrl-space",
        SttToggle,
        Some("GitTermShell && IcedStt && (IcedMain || IcedChat)")
    ),
    (
        "cmd-t",
        TaskSwitcherOpen,
        Some("GitTermShell && (IcedMain || IcedChat)")
    ),
    (
        "cmd-t",
        TaskSwitcherClose,
        Some("GitTermShell && IcedTaskSwitcher")
    ),
    (
        "down",
        TaskSwitcherNext,
        Some("GitTermShell && IcedTaskSwitcher")
    ),
    (
        "up",
        TaskSwitcherPrevious,
        Some("GitTermShell && IcedTaskSwitcher")
    ),
    (
        "enter",
        TaskSwitcherActivateFirst,
        Some("GitTermShell && IcedTaskSwitcherInput")
    ),
    ("cmd-b", ToggleSidebar, Some("GitTermShell && IcedMain")),
    ("cmd-j", ConsoleToggle, Some("GitTermShell && IcedMain")),
    (
        "cmd-shift-r",
        RunRepoCommand,
        Some("GitTermShell && IcedMain")
    ),
    (
        "cmd-shift-w",
        WorkspaceClose,
        Some("GitTermShell && IcedMain")
    ),
    (
        "cmd-shift-u",
        UsageViewToggle,
        Some("GitTermShell && (IcedMain || IcedChat || IcedUsage)")
    ),
    (
        "cmd-shift-o",
        ChatsOverviewOpen,
        Some("GitTermShell && (IcedMain || IcedChat || IcedUsage)")
    ),
    (
        "alt-/",
        ToggleHelp,
        Some("GitTermShell && (IcedMain || IcedHelp)")
    ),
    (
        "ctrl-`",
        AttentionJumpNext,
        Some("GitTermShell && IcedMain")
    ),
    ("alt-shift-t", OpenFolder, Some("GitTermShell && IcedMain")),
    (
        "cmd-up",
        TaskContextBack,
        Some("GitTermShell && IcedMain && IcedTaskContext")
    ),
    (
        "cmd-0",
        TaskOverview,
        Some("GitTermShell && IcedMain && IcedTaskContext")
    ),
    ("escape", Escape, Some("GitTermShell")),
    ("enter", ParkSubmit, Some("GitTermShell > IcedPark > Input")),
    // This branch precedes (and shadows) the terminal SearchPrev branch in Iced.
    (
        "cmd-shift-g",
        DebugSpawnAgentTab,
        Some("GitTermShell && IcedMain")
    ),
    (
        "cmd-f",
        ToggleSearch,
        Some("GitTermShell && IcedMain && !IcedAgentTab")
    ),
    (
        "cmd-f",
        AgentFindOpen,
        Some("GitTermShell && (IcedMain || IcedChat) && IcedChatFindAllowed")
    ),
    (
        "cmd-g",
        SearchNext,
        Some("GitTermShell && IcedMain && IcedTerminalSearch")
    ),
    (
        "j",
        FileSelectByIndex {
            selection: FileSelection::Next
        },
        Some("GitTermShell && IcedMain && IcedDiff")
    ),
    (
        "k",
        FileSelectByIndex {
            selection: FileSelection::Previous
        },
        Some("GitTermShell && IcedMain && IcedDiff")
    ),
    (
        "g",
        FileSelectByIndex {
            selection: FileSelection::First
        },
        Some("GitTermShell && IcedMain && IcedDiff")
    ),
    (
        "shift-g",
        FileSelectByIndex {
            selection: FileSelection::Last
        },
        Some("GitTermShell && IcedMain && IcedDiff")
    ),
    ("e", EditFile, Some("GitTermShell && IcedMain && IcedDiff")),
    (
        "ctrl-1",
        WorkspaceSelect { index: 0 },
        Some("GitTermShell && (IcedMain || IcedChat || IcedUsage)")
    ),
    (
        "cmd-1",
        TabSelect { index: 0 },
        Some("GitTermShell && (IcedMain || IcedChat)")
    ),
    (
        "alt-shift-1",
        LaunchAgentPreset { index: 0 },
        Some("GitTermShell && IcedMain && !IcedTaskContext")
    ),
    (
        "alt-shift-1",
        TaskChildLaunchPreset { index: 0 },
        Some("GitTermShell && IcedMain && IcedTaskContext")
    ),
    (
        "ctrl-2",
        WorkspaceSelect { index: 1 },
        Some("GitTermShell && (IcedMain || IcedChat || IcedUsage)")
    ),
    (
        "cmd-2",
        TabSelect { index: 1 },
        Some("GitTermShell && (IcedMain || IcedChat)")
    ),
    (
        "alt-shift-2",
        LaunchAgentPreset { index: 1 },
        Some("GitTermShell && IcedMain && !IcedTaskContext")
    ),
    (
        "alt-shift-2",
        TaskChildLaunchPreset { index: 1 },
        Some("GitTermShell && IcedMain && IcedTaskContext")
    ),
    (
        "ctrl-3",
        WorkspaceSelect { index: 2 },
        Some("GitTermShell && (IcedMain || IcedChat || IcedUsage)")
    ),
    (
        "cmd-3",
        TabSelect { index: 2 },
        Some("GitTermShell && (IcedMain || IcedChat)")
    ),
    (
        "alt-shift-3",
        LaunchAgentPreset { index: 2 },
        Some("GitTermShell && IcedMain && !IcedTaskContext")
    ),
    (
        "alt-shift-3",
        TaskChildLaunchPreset { index: 2 },
        Some("GitTermShell && IcedMain && IcedTaskContext")
    ),
    (
        "ctrl-4",
        WorkspaceSelect { index: 3 },
        Some("GitTermShell && (IcedMain || IcedChat || IcedUsage)")
    ),
    (
        "cmd-4",
        TabSelect { index: 3 },
        Some("GitTermShell && (IcedMain || IcedChat)")
    ),
    (
        "alt-shift-4",
        LaunchAgentPreset { index: 3 },
        Some("GitTermShell && IcedMain && !IcedTaskContext")
    ),
    (
        "alt-shift-4",
        TaskChildLaunchPreset { index: 3 },
        Some("GitTermShell && IcedMain && IcedTaskContext")
    ),
    (
        "ctrl-5",
        WorkspaceSelect { index: 4 },
        Some("GitTermShell && (IcedMain || IcedChat || IcedUsage)")
    ),
    (
        "cmd-5",
        TabSelect { index: 4 },
        Some("GitTermShell && (IcedMain || IcedChat)")
    ),
    (
        "alt-shift-5",
        LaunchAgentPreset { index: 4 },
        Some("GitTermShell && IcedMain && !IcedTaskContext")
    ),
    (
        "alt-shift-5",
        TaskChildLaunchPreset { index: 4 },
        Some("GitTermShell && IcedMain && IcedTaskContext")
    ),
    (
        "ctrl-6",
        WorkspaceSelect { index: 5 },
        Some("GitTermShell && (IcedMain || IcedChat || IcedUsage)")
    ),
    (
        "cmd-6",
        TabSelect { index: 5 },
        Some("GitTermShell && (IcedMain || IcedChat)")
    ),
    (
        "alt-shift-6",
        LaunchAgentPreset { index: 5 },
        Some("GitTermShell && IcedMain && !IcedTaskContext")
    ),
    (
        "alt-shift-6",
        TaskChildLaunchPreset { index: 5 },
        Some("GitTermShell && IcedMain && IcedTaskContext")
    ),
    (
        "ctrl-7",
        WorkspaceSelect { index: 6 },
        Some("GitTermShell && (IcedMain || IcedChat || IcedUsage)")
    ),
    (
        "cmd-7",
        TabSelect { index: 6 },
        Some("GitTermShell && (IcedMain || IcedChat)")
    ),
    (
        "alt-shift-7",
        LaunchAgentPreset { index: 6 },
        Some("GitTermShell && IcedMain && !IcedTaskContext")
    ),
    (
        "alt-shift-7",
        TaskChildLaunchPreset { index: 6 },
        Some("GitTermShell && IcedMain && IcedTaskContext")
    ),
    (
        "ctrl-8",
        WorkspaceSelect { index: 7 },
        Some("GitTermShell && (IcedMain || IcedChat || IcedUsage)")
    ),
    (
        "cmd-8",
        TabSelect { index: 7 },
        Some("GitTermShell && (IcedMain || IcedChat)")
    ),
    (
        "alt-shift-8",
        LaunchAgentPreset { index: 7 },
        Some("GitTermShell && IcedMain && !IcedTaskContext")
    ),
    (
        "alt-shift-8",
        TaskChildLaunchPreset { index: 7 },
        Some("GitTermShell && IcedMain && IcedTaskContext")
    ),
    (
        "ctrl-9",
        WorkspaceSelect { index: 8 },
        Some("GitTermShell && (IcedMain || IcedChat || IcedUsage)")
    ),
    (
        "cmd-9",
        TabSelect { index: 8 },
        Some("GitTermShell && (IcedMain || IcedChat)")
    ),
    (
        "alt-shift-9",
        LaunchAgentPreset { index: 8 },
        Some("GitTermShell && IcedMain && !IcedTaskContext")
    ),
    (
        "alt-shift-9",
        TaskChildLaunchPreset { index: 8 },
        Some("GitTermShell && IcedMain && IcedTaskContext")
    ),
    // Page-local keys from agent_chat.html; never install these globally.
    // Composer adapters match at the Input leaf so they outrank component defaults.
    ("cmd-c", Copy, Some("NativeChat")),
    (
        "cmd-shift-c",
        CopyMarkdown,
        Some("NativeChat && !IcedEditable")
    ),
    ("cmd-x", Cut, Some("NativeChat")),
    ("cmd-v", Paste, Some("NativeChat")),
    ("cmd-a", SelectAll, Some("NativeChat")),
    ("cmd-z", Undo, Some("NativeChat")),
    ("cmd-shift-z", Redo, Some("NativeChat")),
    (
        "enter",
        SubmitPrompt,
        Some("(NativeChat && IcedComposer && !IcedSlash) > Input")
    ),
    (
        "shift-enter",
        Newline,
        Some("(NativeChat && IcedComposer) > Input")
    ),
    (
        "shift-tab",
        CycleMode,
        Some("(NativeChat && IcedComposer) > Input")
    ),
    ("enter", FindNext, Some("NativeChat && IcedChatFind")),
    (
        "shift-enter",
        FindPrevious,
        Some("NativeChat && IcedChatFind")
    ),
    (
        "escape",
        CloseFind,
        Some("(GitTermShell && IcedChatFind) > NativeChat")
    ),
    ("down", SlashNext, Some("(NativeChat && IcedSlash) > Input")),
    (
        "up",
        SlashPrevious,
        Some("(NativeChat && IcedSlash) > Input")
    ),
    ("tab", FillSlash, Some("(NativeChat && IcedSlash) > Input")),
    (
        "enter",
        SubmitSlash,
        Some("(NativeChat && IcedSlash) > Input")
    ),
    (
        "escape",
        CloseSlash,
        Some("(NativeChat && IcedSlash) > Input")
    ),
    ("escape", CloseReview, Some("NativeChat && IcedReview")),
    ("cmd-enter", SubmitReview, Some("NativeChat && IcedReview")),
    ("ctrl-enter", SubmitReview, Some("NativeChat && IcedReview")),
];

/// Iced's menu bar is muda's NSMenu: AppKit resolves its key equivalents
/// before winit sees the key, and Iced's menu poll (`CheckMenu`) dispatches
/// with no modal check. So every item of `menus()` and its accelerator still
/// runs behind any Iced modal; every other key reaches Iced's `KeyPressed`,
/// which a modal consumes except for its own keys.
pub const NATIVE_MENU_ACTIONS: &[&str] = &[
    "About",
    "UseV6State",
    "Hide",
    "HideOthers",
    "ShowAll",
    "Quit",
    "Minimize",
    "Maximize",
    "Fullscreen",
    "IncreaseTerminalFont",
    "DecreaseTerminalFont",
    "ClearTerminal",
    "IncreaseUiFont",
    "DecreaseUiFont",
    "IncreaseChatFont",
    "DecreaseChatFont",
    "SetChatWidth",
    "ToggleTheme",
    "ToggleTerminalLogMirroring",
    "OpenPlansViewer",
];

/// A shell surface that consumes background commands while it is open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Modal {
    TaskSwitcher,
    Usage,
    /// A `gpui_component` dialog: the shortcuts help or a confirmation.
    Dialog,
    Task,
    SessionPicker,
}

impl Modal {
    pub const ALL: [Modal; 5] = [
        Modal::TaskSwitcher,
        Modal::Usage,
        Modal::Dialog,
        Modal::Task,
        Modal::SessionPicker,
    ];

    /// The keys Iced's `KeyPressed` hands to this modal (Escape for all).
    fn own(self) -> &'static [&'static str] {
        match self {
            // Escape, ⌘T, ↑/↓, and the query field's Enter.
            Modal::TaskSwitcher => &[
                "TaskSwitcherOpen",
                "TaskSwitcherClose",
                "TaskSwitcherNext",
                "TaskSwitcherPrevious",
                "TaskSwitcherActivateFirst",
            ],
            // Escape, ⌘⇧U, ⌘⇧O and Ctrl+1-9.
            Modal::Usage => &["UsageViewToggle", "ChatsOverviewOpen", "WorkspaceSelect"],
            // Escape or ⌥/ closes the help.
            Modal::Dialog => &["ToggleHelp"],
            _ => &[],
        }
    }

    /// Whether the action named `name` (namespaced or not) still runs
    /// while this modal is open.
    pub fn allows(self, name: &str) -> bool {
        let name = name.strip_prefix("gitterm_iced::").unwrap_or(name);
        name == "Escape" || NATIVE_MENU_ACTIONS.contains(&name) || self.own().contains(&name)
    }
}

/// A surface Iced's `KeyPressed` handler catches only Escape for (Park also
/// Enter); every other key and command reaches the window behind, so these
/// are not guarded and `menu_context` keeps `IcedMain` behind them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sheet {
    Park,
    NewWork,
    Settings,
    Phone,
    Plans,
}

impl Sheet {
    pub const ALL: [Sheet; 5] = [
        Sheet::Park,
        Sheet::NewWork,
        Sheet::Settings,
        Sheet::Phone,
        Sheet::Plans,
    ];
}

/// Actions without a host adapter remain unregistered, so GPUI's native menu
/// validation disables their menu items.
pub const MISSING_HOST_ACTIONS: &[&str] = &[];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    const ICED: &str = include_str!("../../../../src/main.rs");
    const CHAT: &str = include_str!("../../../../assets/agent_chat.html");

    fn section<'a>(source: &'a str, start: &str, end: &str) -> &'a str {
        source
            .split_once(start)
            .unwrap()
            .1
            .split_once(end)
            .unwrap()
            .0
    }

    fn packed(source: &str) -> String {
        source.split_whitespace().collect()
    }

    fn source_menu_items(source: &str, variable: &str) -> Vec<String> {
        let prefix = format!(";{variable}.append_items(&[");
        section(source, &prefix, "])")
            .trim_end_matches(',')
            .split(",&")
            .map(|item| item.trim_start_matches('&').to_owned())
            .collect()
    }

    // Parse the Iced builder itself, including append order and separators.
    // Predefined labels below are muda 0.17.1's macOS labels (Zoom, not Maximize).
    fn source_menu(source: &str, variable: &str) -> Vec<String> {
        let mut result = vec![];
        for item in source_menu_items(source, variable) {
            if let Some(kind) = item.strip_prefix("PredefinedMenuItem::") {
                result.push(
                    match kind {
                        "about(None,None)" => "AboutGitTermV6",
                        "services(None)" => "Services",
                        "hide(None)" => "HideGitTermV6",
                        "hide_others(None)" => "HideOthers",
                        "show_all(None)" => "ShowAll",
                        "quit(None)" => "QuitGitTermV6",
                        "minimize(None)" => "Minimize",
                        "maximize(None)" => "Zoom",
                        "fullscreen(None)" => "ToggleFullScreen",
                        "separator()" => "---",
                        _ => panic!("Unmapped Iced predefined item: {kind}"),
                    }
                    .to_owned(),
                );
            } else if item == "chat_width_menu" {
                assert!(source.contains("config::ChatWidth::ALL.into_iter()"));
                assert!(source.contains("MenuItem::new(width.to_string(),true,None)"));
                result.push("Chatwidth".into());
                result.extend(ChatWidth::ALL.map(|w| format!("  {w}")));
            } else {
                let declaration = source.split_once(&format!("let{item}=")).unwrap().1;
                if declaration.starts_with("Submenu::new(") {
                    let title = section(declaration, "Submenu::new(\"", "\",true)");
                    result.push(title.to_owned());
                    result.extend(
                        source_menu(source, &item)
                            .into_iter()
                            .map(|s| format!("  {s}")),
                    );
                } else {
                    // Also checks enabled=true, rather than just copying the label.
                    result.push(section(declaration, "MenuItem::new(\"", "\",true,").to_owned());
                }
            }
        }
        result
    }

    fn actual_menu(menu: &Menu) -> Vec<String> {
        assert!(!menu.disabled);
        let mut result = vec![];
        for item in &menu.items {
            assert!(!item.is_disabled());
            assert!(!item.is_checked());
            match item {
                MenuItem::Separator => result.push("---".into()),
                MenuItem::Submenu(menu) => {
                    result.push(packed(&menu.name));
                    result.extend(actual_menu(menu).into_iter().map(|s| format!("  {s}")));
                }
                MenuItem::SystemMenu(menu) => {
                    assert!(menu.menu_type == SystemMenuType::Services);
                    result.push(packed(&menu.name));
                }
                MenuItem::Action { name, .. } => result.push(packed(name)),
            }
        }
        result
    }

    #[test]
    fn menu_titles_order_separators_and_enabled_state_equal_iced_builder() {
        let source = packed(section(
            ICED,
            "fn setup_menu_bar()",
            "// === Speech-to-Text",
        ));
        let variables = source_menu_items(&source, "menu");
        assert_eq!(variables, ["app_menu", "view_menu", "window_menu"]);
        let menus = menus();
        assert_eq!(
            menus.iter().map(|m| m.name.as_ref()).collect::<Vec<_>>(),
            ["GitTerm V6", "View", "Window"]
        );
        assert!(source.contains("Submenu::new(config::APP_NAME,true)"));
        assert!(source.contains("Submenu::new(\"View\",true)"));
        assert!(source.contains("Submenu::new(\"Window\",true)"));
        assert!(
            include_str!("../../../../src/config.rs")
                .contains("pub const APP_NAME: &str = \"GitTerm V6\";")
        );
        assert!(
            matches!(&menus[0].items[1], MenuItem::Action { name, action, .. }
            if name.as_ref() == "Use V6 state…" && action.as_any().is::<UseV6State>())
        );
        for (menu, variable) in menus.iter().zip(variables) {
            // The GPUI activation entry is additional; retain exact parity for
            // every inherited Iced item and its order/separator/enabled state.
            let inherited: Vec<_> = actual_menu(menu)
                .into_iter()
                .filter(|name| name != "UseV6state…")
                .collect();
            assert_eq!(inherited, source_menu(&source, &variable));
        }
    }

    fn event_after(source: &str, anchor: &str) -> String {
        let tail = source
            .split_once(anchor)
            .unwrap_or_else(|| panic!("Missing Iced branch: {anchor}"))
            .1;
        let event = tail.split_once("Event::").unwrap().1;
        event
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect()
    }

    fn add(rows: &mut Vec<(String, String)>, key: &str, command: &str) {
        rows.push((key.into(), command.into()));
    }

    // Independent inventory: read returned commands out of Iced branches. The
    // anchors assert each chord's actual condition, not just a comment/help label.
    fn source_shortcuts() -> Vec<(String, String)> {
        let mut rows = vec![];
        let keys = packed(section(
            ICED,
            "Event::KeyPressed(key, modifiers) => {",
            "Event::ToggleSidebar =>",
        ));
        for (key, anchor) in [
            ("cmd-b", "ifc==\"b\"&&!modifiers.shift()"),
            ("cmd-j", "ifc==\"j\"&&!modifiers.shift()"),
            ("cmd-shift-r", "if(c==\"r\"||c==\"R\")&&modifiers.shift()"),
            ("cmd-shift-w", "if(c==\"w\"||c==\"W\")&&modifiers.shift()"),
            ("cmd-shift-g", "if(c==\"g\"||c==\"G\")&&modifiers.shift()"),
            (
                "cmd-t",
                "ifglobal_shortcut(&key,modifiers)==Some(GlobalShortcut::TaskSwitcher)",
            ),
            (
                "cmd-shift-u",
                "ifglobal_shortcut(&key,modifiers)==Some(GlobalShortcut::UsageView)",
            ),
            (
                "cmd-shift-o",
                "ifglobal_shortcut(&key,modifiers)==Some(GlobalShortcut::ChatsOverview)",
            ),
            (
                "ctrl-space",
                "ifglobal_shortcut(&key,modifiers)==Some(GlobalShortcut::Dictation)",
            ),
            ("ctrl-`", "ifc==\"`\""),
            ("alt-shift-t", "ifc==\"t\"||c==\"T\""),
            ("cmd-k", "ifc==\"k\""),
            (
                "cmd-0",
                "elseifc==\"0\"&&self.active_task_context_id().is_some()",
            ),
            (
                "cmd-up",
                "ifmatches!(key.as_ref(),Key::Named(key::Named::ArrowUp))&&self.active_task_context_id().is_some()",
            ),
            (
                "enter",
                "returnself.update(Event::ParkCancel),Key::Named(key::Named::Enter)",
            ),
        ] {
            add(&mut rows, key, &event_after(&keys, anchor));
        }
        assert!(keys.contains("ifmodifiers.alt()&&!modifiers.command()"));
        assert!(keys.contains("ifc==\"/\"||c==\"÷\""));
        add(
            &mut rows,
            "alt-/",
            &event_after(&keys, "ifc==\"/\"||c==\"÷\""),
        );
        assert!(keys.contains("ifc==\"g\"&&tab.search.is_active"));
        add(
            &mut rows,
            "cmd-g",
            &event_after(&keys, "returnTask::done(Event::SearchPrev);"),
        );
        assert!(
            keys.find("Event::DebugSpawnAgentTab").unwrap()
                < keys.find("Event::SearchPrev").unwrap()
        );
        // Shift+G is actually the debug branch; never advertise SearchPrev here.
        assert!(keys.contains("Key::Named(key::Named::ArrowDown|key::Named::ArrowUp)"));
        assert!(keys.contains("highlighted+1"));
        assert!(keys.contains("highlighted.saturating_sub(1)"));
        add(&mut rows, "cmd-t", "TaskSwitcherClose");
        add(&mut rows, "down", "TaskSwitcherNext");
        add(&mut rows, "up", "TaskSwitcherPrevious");
        assert!(ICED.contains(".on_submit(Event::TaskSwitcherActivateFirst)"));
        add(&mut rows, "enter", "TaskSwitcherActivateFirst");
        assert!(keys.contains("Some(GlobalShortcut::SelectWorkspace(num))"));
        assert!(keys.contains("Some(GlobalShortcut::SelectTab(num))"));
        assert!(keys.contains("Event::LaunchAgentPreset(num-1)"));
        assert!(keys.contains("Event::TaskChildLaunchPreset(task_id.to_string(),num-1)"));
        for digit in 1..=9 {
            add(&mut rows, &format!("ctrl-{digit}"), "WorkspaceSelect");
            add(&mut rows, &format!("cmd-{digit}"), "TabSelect");
            add(
                &mut rows,
                &format!("alt-shift-{digit}"),
                "LaunchAgentPreset",
            );
            add(
                &mut rows,
                &format!("alt-shift-{digit}"),
                "TaskChildLaunchPreset",
            );
        }
        let find = packed(section(ICED, "fn find_shortcut(", "fn chat_find_allowed("));
        assert!(find.contains("modifiers==Modifiers::COMMAND"));
        assert!(find.contains("c.eq_ignore_ascii_case(\"f\")"));
        for event in ["AgentFindOpen", "ToggleSearch"] {
            assert!(find.contains(&format!("Event::{event}")));
            add(&mut rows, "cmd-f", event);
        }
        let font = packed(section(ICED, "fn font_shortcut(", "/// Read a chord"));
        assert!(font.contains("Key::Character(\"=\"|\"+\")"));
        assert!(font.contains("Key::Character(\"-\"|\"_\")"));
        for (key, event) in [
            ("cmd-=", "IncreaseTerminalFont"),
            ("cmd--", "DecreaseTerminalFont"),
            ("cmd-=", "IncreaseChatFont"),
            ("cmd--", "DecreaseChatFont"),
            ("cmd-shift-=", "IncreaseUiFont"),
            ("cmd-shift--", "DecreaseUiFont"),
        ] {
            assert!(font.contains(&format!("Event::{event}")));
            add(&mut rows, key, event);
        }
        // View menu accelerators: read Code and modifiers from their constructor.
        let menu = packed(section(
            ICED,
            "fn setup_menu_bar()",
            "// === Speech-to-Text",
        ));
        for (variable, command) in [
            ("toggle_theme", "ToggleTheme"),
            (
                "toggle_terminal_log_mirroring",
                "ToggleTerminalLogMirroring",
            ),
            ("open_plans_viewer", "OpenPlansViewer"),
        ] {
            let declaration = section(&menu, &format!("let{variable}=MenuItem::new("), ";");
            assert!(declaration.contains("Modifiers::META|muda::accelerator::Modifiers::SHIFT"));
            let code = section(declaration, "Code::Key", ",").to_ascii_lowercase();
            add(&mut rows, &format!("cmd-shift-{code}"), command);
        }
        for (kind, key, command) in [
            ("hide", "cmd-h", "Hide"),
            ("hide_others", "cmd-alt-h", "HideOthers"),
            ("minimize", "cmd-m", "Minimize"),
            ("fullscreen", "cmd-ctrl-f", "Fullscreen"),
        ] {
            assert!(menu.contains(&format!("PredefinedMenuItem::{kind}(None)")));
            add(&mut rows, key, command);
        }
        assert!(section(ICED, "let quit = MenuItem::new(", ";").contains("\"Quit GitTerm V6\""));
        let quit = section(&menu, "letquit=MenuItem::new(", ";");
        assert!(quit.contains("Some(Accelerator::new(Some(muda::accelerator::Modifiers::META),muda::accelerator::Code::KeyQ,))"));
        assert!(menu.contains(
            "&PredefinedMenuItem::show_all(None),&PredefinedMenuItem::separator(),&quit,"
        ));
        add(&mut rows, "cmd-q", "Quit");
        for (key, condition, command) in [
            ("j", "Key::Character(\"j\")", "FileSelectByIndex"),
            ("k", "Key::Character(\"k\")", "FileSelectByIndex"),
            ("g", "Key::Character(\"g\")", "FileSelectByIndex"),
            ("shift-g", "Key::Character(\"G\")", "FileSelectByIndex"),
            ("e", "Key::Character(\"e\")", "EditFile"),
        ] {
            assert!(keys.contains(condition));
            add(&mut rows, key, command);
        }
        assert!(keys.contains("Key::Named(key::Named::Escape)"));
        add(&mut rows, "escape", "Escape");
        // Page-local editing, composer, slash-menu, find and review bindings.
        for (key, condition, command) in [
            ("cmd-c", "if (k === 'c') return 'copy'", "Copy"),
            ("cmd-x", "if (k === 'x') return 'cut'", "Cut"),
            ("cmd-a", "if (k === 'a') return 'selectAll'", "SelectAll"),
            ("cmd-z", "e.shiftKey ? 'redo' : 'undo'", "Undo"),
            ("cmd-shift-z", "e.shiftKey ? 'redo' : 'undo'", "Redo"),
            ("cmd-v", "k.toLowerCase() === 'v'", "Paste"),
            (
                "cmd-shift-c",
                "event.key.toLowerCase() === 'c'",
                "CopyMarkdown",
            ),
            (
                "enter",
                "e.key === 'Enter' && !e.shiftKey && !e.isComposing",
                "SubmitPrompt",
            ),
            (
                "shift-enter",
                "e.key === 'Enter' && !e.shiftKey && !e.isComposing",
                "Newline",
            ),
            (
                "shift-tab",
                "e.key === 'Tab' && e.shiftKey && !e.isComposing",
                "CycleMode",
            ),
            ("enter", "stepFind(e.shiftKey ? -1 : 1)", "FindNext"),
            (
                "shift-enter",
                "stepFind(e.shiftKey ? -1 : 1)",
                "FindPrevious",
            ),
            ("escape", "if (e.key === 'Escape') closeFind()", "CloseFind"),
            (
                "down",
                "e.key === 'ArrowDown' || e.key === 'ArrowUp'",
                "SlashNext",
            ),
            (
                "up",
                "e.key === 'ArrowDown' || e.key === 'ArrowUp'",
                "SlashPrevious",
            ),
            ("tab", "e.key === 'Tab' && !e.shiftKey", "FillSlash"),
            (
                "enter",
                "if (!c.argumentHint) submitPrompt()",
                "SubmitSlash",
            ),
            (
                "escape",
                "slashMenuEl.classList.remove('visible')",
                "CloseSlash",
            ),
            (
                "escape",
                "if (e.key === 'Escape') { closeReview(); promptEl.focus(); }",
                "CloseReview",
            ),
            (
                "cmd-enter",
                "e.key === 'Enter' && (e.metaKey || e.ctrlKey)",
                "SubmitReview",
            ),
            (
                "ctrl-enter",
                "e.key === 'Enter' && (e.metaKey || e.ctrlKey)",
                "SubmitReview",
            ),
        ] {
            assert!(
                CHAT.contains(condition),
                "Iced page condition changed: {condition}"
            );
            add(&mut rows, key, command);
        }
        rows.sort();
        rows
    }

    #[test]
    fn shortcut_table_equals_iced_source_inventory_without_extra_bindings() {
        let mut actual = iced_shortcuts()
            .iter()
            .map(|(k, a, _)| (k.to_string(), a.to_string()))
            .collect::<Vec<_>>();
        actual.sort();
        assert_eq!(actual, source_shortcuts());
        assert!(
            !actual
                .iter()
                .any(|(_, a)| a == "NewTerminal" || a == "SearchPrev")
        );
        for absent in ["cmd-o", "cmd-w", "alt-shift-c", "alt-shift-x"] {
            assert!(!actual.iter().any(|(key, _)| key == absent));
        }
    }

    #[test]
    fn bindings_match_table_actions_contexts_and_index_payloads() {
        let bindings = key_bindings();
        assert_eq!(bindings.len(), iced_shortcuts().len());
        let mut unique = BTreeSet::new();
        for (binding, &(key, action, context)) in bindings.iter().zip(iced_shortcuts()) {
            assert_eq!(binding.action().name(), format!("gitterm_iced::{action}"));
            let expected = KeyBinding::new(key, Escape, context);
            assert_eq!(binding.keystrokes(), expected.keystrokes());
            assert_eq!(binding.predicate(), expected.predicate());
            assert!(
                unique.insert((key, context)),
                "Ambiguous binding: {key} {context:?}"
            );
            if let Some(digit) = key.chars().last().and_then(|c| c.to_digit(10)) {
                let index = digit.saturating_sub(1) as usize;
                let expected: Option<Box<dyn Action>> = match action {
                    "WorkspaceSelect" => Some(Box::new(WorkspaceSelect { index })),
                    "TabSelect" => Some(Box::new(TabSelect { index })),
                    "LaunchAgentPreset" => Some(Box::new(LaunchAgentPreset { index })),
                    "TaskChildLaunchPreset" => Some(Box::new(TaskChildLaunchPreset { index })),
                    _ => None,
                };
                if let Some(expected) = expected {
                    assert!(binding.action().partial_eq(expected.as_ref()));
                }
            }
        }
        // Plain navigation must remain focus-scoped; native OS/View accelerators
        // alone can be context-free. A menu declaration isn't a host handler.
        let globals: BTreeMap<_, _> = iced_shortcuts()
            .iter()
            .filter(|(_, _, c)| c.is_none())
            .map(|(k, a, _)| (*k, *a))
            .collect();
        assert_eq!(globals.len(), 10);
        for (key, _, context) in iced_shortcuts() {
            if !key.contains('-') || key.starts_with("alt-shift-") {
                assert!(context.is_some());
            }
        }
        for action in MISSING_HOST_ACTIONS {
            assert!(
                iced_shortcuts().iter().any(|(_, a, _)| a == action) || *action == "SetChatWidth",
                "Missing definition for host TODO {action}"
            );
        }
    }

    // Semantic inventory assertions above explain the mapping. These small source
    // snapshots additionally catch newly added/removed branches, so an Iced edit
    // cannot silently leave the independently audited inventory incomplete.
    #[test]
    fn iced_dispatch_changes_require_a_shortcut_reaudit() {
        fn fingerprint(source: &str) -> u64 {
            packed(source)
                .bytes()
                .fold(0xcbf29ce484222325, |hash, byte| {
                    (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
                })
        }
        assert_eq!(
            fingerprint(section(ICED, "fn global_shortcut(", "/// What a chord")),
            0x989cfcea710f24c0,
            "Re-audit the Iced shortcut inventory after changing its dispatch source"
        );
        assert_eq!(
            fingerprint(section(ICED, "fn find_shortcut(", "fn chat_find_allowed(")),
            0x43b64c892d5f5d32,
            "Re-audit the Iced shortcut inventory after changing its dispatch source"
        );
        assert_eq!(
            fingerprint(section(ICED, "fn font_shortcut(", "/// Read a chord")),
            0xf52bb4ffa253bfe3,
            "Re-audit the Iced shortcut inventory after changing its dispatch source"
        );
        assert_eq!(
            fingerprint(section(
                ICED,
                "Event::KeyPressed(key, modifiers) => {",
                "Event::ToggleSidebar =>"
            )),
            0xc1c3291f578cbd49,
            "Re-audit the Iced shortcut inventory after changing its dispatch source"
        );
        assert_eq!(
            fingerprint(section(
                CHAT,
                "promptEl.addEventListener('keydown'",
                "function autoGrow()"
            )),
            0x20af18bc65fb59b3,
            "Re-audit the Iced shortcut inventory after changing its dispatch source"
        );
        assert_eq!(
            fingerprint(section(CHAT, "function slashKey(", "// ----")),
            0xe97e44b342556d1f,
            "Re-audit the Iced shortcut inventory after changing its dispatch source"
        );
        assert_eq!(
            fingerprint(section(
                CHAT,
                "function editingCommand(",
                "refreshComposer();"
            )),
            0xe11baea9bbbd717b,
            "Re-audit the Iced shortcut inventory after changing its dispatch source"
        );
    }

    #[test]
    fn menu_commands_and_payloads_match_iced_dispatch() {
        let menus = menus();
        let view = &menus[1];
        let poll = packed(section(
            ICED,
            "// Poll for native menu events",
            "let _menu_poll_elapsed",
        ));
        for (variable, item) in [
            ("increase_chat_font", &view.items[2]),
            ("decrease_chat_font", &view.items[3]),
            ("toggle_theme", &view.items[6]),
            ("toggle_terminal_log_mirroring", &view.items[7]),
            ("open_plans_viewer", &view.items[9]),
        ] {
            let MenuItem::Action { action, .. } = item else {
                panic!("Expected action")
            };
            assert_eq!(
                action.name(),
                format!(
                    "gitterm_iced::{}",
                    event_after(&poll, &format!("event.id==ids.{variable}"))
                )
            );
        }
        for (index, variables) in [
            (
                0,
                vec![
                    "increase_terminal_font",
                    "decrease_terminal_font",
                    "clear_terminal",
                ],
            ),
            (1, vec!["increase_ui_font", "decrease_ui_font"]),
        ] {
            let MenuItem::Submenu(menu) = &view.items[index] else {
                panic!("Expected submenu")
            };
            for (item, variable) in menu.items.iter().zip(variables) {
                let MenuItem::Action { action, .. } = item else {
                    panic!("Expected action")
                };
                assert_eq!(
                    action.name(),
                    format!(
                        "gitterm_iced::{}",
                        event_after(&poll, &format!("event.id==ids.{variable}"))
                    )
                );
            }
        }
        let MenuItem::Submenu(widths) = &view.items[4] else {
            panic!("Expected width submenu")
        };
        for (item, width) in widths.items.iter().zip(ChatWidth::ALL) {
            let MenuItem::Action { action, .. } = item else {
                panic!("Expected width action")
            };
            assert!(action.partial_eq(&SetChatWidth { width }));
        }
        for (key, selection) in [
            ("j", FileSelection::Next),
            ("k", FileSelection::Previous),
            ("g", FileSelection::First),
            ("shift-g", FileSelection::Last),
        ] {
            let bindings = key_bindings();
            let index = iced_shortcuts()
                .iter()
                .position(|(k, action, _)| *k == key && *action == "FileSelectByIndex")
                .unwrap();
            assert!(
                bindings[index]
                    .action()
                    .partial_eq(&FileSelectByIndex { selection })
            );
        }
    }
}
