use git2::{DiffOptions, Repository, Status, StatusOptions};
use iced::advanced::graphics::core::Element;
use iced::keyboard::{self, key, Key, Modifiers};
use iced::widget::{button, column, container, image, row, scrollable, text, text_input, Column, Row};
use iced::{color, Length, Size, Subscription, Task, Theme};
use iced_term::{ColorPalette, SearchMatch, TerminalView};
use muda::{accelerator::Accelerator, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use serde::{Deserialize, Serialize};
use similar::{ChangeTag, TextDiff};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

mod log_server;
mod markdown;
mod webview;

// Menu item IDs stored globally for event matching
static MENU_IDS: OnceLock<MenuIds> = OnceLock::new();

#[derive(Debug)]
struct MenuIds {
    increase_terminal_font: muda::MenuId,
    decrease_terminal_font: muda::MenuId,
    increase_ui_font: muda::MenuId,
    decrease_ui_font: muda::MenuId,
    toggle_theme: muda::MenuId,
    clear_terminal: muda::MenuId,
}

fn setup_menu_bar() {
    // Create native macOS menu bar
    let menu = Menu::new();

    // App menu (GitTerm)
    let app_menu = Submenu::new("GitTerm", true);
    app_menu
        .append_items(&[
            &PredefinedMenuItem::about(None, None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::services(None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::hide(None),
            &PredefinedMenuItem::hide_others(None),
            &PredefinedMenuItem::show_all(None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::quit(None),
        ])
        .unwrap();

    // View menu (font size, theme)
    let view_menu = Submenu::new("View", true);

    // Terminal font submenu
    let terminal_font_menu = Submenu::new("Terminal Font", true);
    let increase_terminal_font = MenuItem::new(
        "Increase",
        true,
        Some(Accelerator::new(
            Some(muda::accelerator::Modifiers::META),
            muda::accelerator::Code::Equal,
        )),
    );
    let decrease_terminal_font = MenuItem::new(
        "Decrease",
        true,
        Some(Accelerator::new(
            Some(muda::accelerator::Modifiers::META),
            muda::accelerator::Code::Minus,
        )),
    );
    let clear_terminal = MenuItem::new(
        "Clear Terminal",
        true,
        Some(Accelerator::new(
            Some(muda::accelerator::Modifiers::META),
            muda::accelerator::Code::KeyK,
        )),
    );
    terminal_font_menu
        .append_items(&[&increase_terminal_font, &decrease_terminal_font, &clear_terminal])
        .unwrap();

    // UI font submenu
    let ui_font_menu = Submenu::new("Sidebar Font", true);
    let increase_ui_font = MenuItem::new(
        "Increase",
        true,
        Some(Accelerator::new(
            Some(muda::accelerator::Modifiers::META | muda::accelerator::Modifiers::SHIFT),
            muda::accelerator::Code::Equal,
        )),
    );
    let decrease_ui_font = MenuItem::new(
        "Decrease",
        true,
        Some(Accelerator::new(
            Some(muda::accelerator::Modifiers::META | muda::accelerator::Modifiers::SHIFT),
            muda::accelerator::Code::Minus,
        )),
    );
    ui_font_menu
        .append_items(&[&increase_ui_font, &decrease_ui_font])
        .unwrap();

    let toggle_theme = MenuItem::new(
        "Toggle Light/Dark Theme",
        true,
        Some(Accelerator::new(
            Some(muda::accelerator::Modifiers::META | muda::accelerator::Modifiers::SHIFT),
            muda::accelerator::Code::KeyT,
        )),
    );

    view_menu
        .append_items(&[
            &terminal_font_menu,
            &ui_font_menu,
            &PredefinedMenuItem::separator(),
            &toggle_theme,
        ])
        .unwrap();

    // Window menu
    let window_menu = Submenu::new("Window", true);
    window_menu
        .append_items(&[
            &PredefinedMenuItem::minimize(None),
            &PredefinedMenuItem::maximize(None),
            &PredefinedMenuItem::separator(),
            &PredefinedMenuItem::fullscreen(None),
        ])
        .unwrap();

    menu.append_items(&[&app_menu, &view_menu, &window_menu])
        .unwrap();

    // Store menu IDs for event handling
    let _ = MENU_IDS.set(MenuIds {
        increase_terminal_font: increase_terminal_font.id().clone(),
        decrease_terminal_font: decrease_terminal_font.id().clone(),
        increase_ui_font: increase_ui_font.id().clone(),
        decrease_ui_font: decrease_ui_font.id().clone(),
        toggle_theme: toggle_theme.id().clone(),
        clear_terminal: clear_terminal.id().clone(),
    });

    // Initialize menu for macOS - this must happen after NSApp exists
    #[cfg(target_os = "macos")]
    menu.init_for_nsapp();

    // Leak the menu to keep it alive for the lifetime of the app
    Box::leak(Box::new(menu));
}

// Persistent configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Config {
    #[serde(default = "default_terminal_font")]
    terminal_font_size: f32,
    #[serde(default = "default_ui_font")]
    ui_font_size: f32,
    #[serde(default = "default_sidebar_width")]
    sidebar_width: f32,
    #[serde(default = "default_scrollback_lines")]
    scrollback_lines: usize,
    // Legacy field for migration
    #[serde(default)]
    font_size: Option<f32>,
    theme: String,
    #[serde(default)]
    show_hidden: bool,
    #[serde(default = "default_console_height")]
    console_height: f32,
    #[serde(default = "default_console_expanded")]
    console_expanded: bool,
}

fn default_terminal_font() -> f32 { 14.0 }
fn default_ui_font() -> f32 { 13.0 }
fn default_sidebar_width() -> f32 { 280.0 }
fn default_scrollback_lines() -> usize { 100_000 }
fn default_console_height() -> f32 { DEFAULT_CONSOLE_HEIGHT }
fn default_console_expanded() -> bool { true }

impl Default for Config {
    fn default() -> Self {
        Self {
            terminal_font_size: 14.0,
            ui_font_size: 13.0,
            sidebar_width: 280.0,
            scrollback_lines: 100_000,
            font_size: None,
            theme: "dark".to_string(),
            show_hidden: false,
            console_height: DEFAULT_CONSOLE_HEIGHT,
            console_expanded: true,
        }
    }
}

impl Config {
    fn config_path() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home).join(".config").join("gitterm").join("config.json")
    }

    fn load() -> Self {
        let path = Self::config_path();
        if path.exists() {
            if let Ok(contents) = std::fs::read_to_string(&path) {
                if let Ok(config) = serde_json::from_str(&contents) {
                    return config;
                }
            }
        }
        Self::default()
    }

    fn save(&self) {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, json);
        }
    }
}

// Workspace persistence
#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkspacesFile {
    workspaces: Vec<WorkspaceConfig>,
    active_workspace: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkspaceConfig {
    name: String,
    abbrev: String,
    dir: String,
    color: WorkspaceColor,
    tabs: Vec<WorkspaceTabConfig>,
    #[serde(default)]
    run_command: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkspaceTabConfig {
    dir: String,
}

impl WorkspacesFile {
    fn file_path() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home)
            .join(".config")
            .join("gitterm")
            .join("workspaces.json")
    }

    fn load() -> Option<Self> {
        let path = Self::file_path();
        if path.exists() {
            let contents = std::fs::read_to_string(&path).ok()?;
            serde_json::from_str(&contents).ok()
        } else {
            None
        }
    }

    fn save(&self) {
        let path = Self::file_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, json);
        }
    }
}

// App theme (affects entire UI)
#[derive(Debug, Clone, Copy, PartialEq, Default)]
enum AppTheme {
    #[default]
    Dark,
    Light,
}

impl AppTheme {
    fn toggle(&self) -> Self {
        match self {
            AppTheme::Dark => AppTheme::Light,
            AppTheme::Light => AppTheme::Dark,
        }
    }

    // Terminal color palette
    fn terminal_palette(&self) -> ColorPalette {
        match self {
            AppTheme::Dark => ColorPalette {
                // Catppuccin Mocha
                background: String::from("#1e1e2e"),
                foreground: String::from("#cdd6f4"),
                black: String::from("#45475a"),
                red: String::from("#f38ba8"),
                green: String::from("#a6e3a1"),
                yellow: String::from("#f9e2af"),
                blue: String::from("#89b4fa"),
                magenta: String::from("#f5c2e7"),
                cyan: String::from("#94e2d5"),
                white: String::from("#bac2de"),
                bright_black: String::from("#585b70"),
                bright_red: String::from("#f38ba8"),
                bright_green: String::from("#a6e3a1"),
                bright_yellow: String::from("#f9e2af"),
                bright_blue: String::from("#89b4fa"),
                bright_magenta: String::from("#f5c2e7"),
                bright_cyan: String::from("#94e2d5"),
                bright_white: String::from("#a6adc8"),
                bright_foreground: Some(String::from("#cdd6f4")),
                dim_foreground: String::from("#7f849c"),
                dim_black: String::from("#313244"),
                dim_red: String::from("#a65d6d"),
                dim_green: String::from("#6e9a6d"),
                dim_yellow: String::from("#a69a74"),
                dim_blue: String::from("#5d78a6"),
                dim_magenta: String::from("#a6849c"),
                dim_cyan: String::from("#649a92"),
                dim_white: String::from("#7f849c"),
            },
            AppTheme::Light => ColorPalette {
                // Catppuccin Latte
                background: String::from("#eff1f5"),
                foreground: String::from("#4c4f69"),
                black: String::from("#5c5f77"),
                red: String::from("#d20f39"),
                green: String::from("#40a02b"),
                yellow: String::from("#df8e1d"),
                blue: String::from("#1e66f5"),
                magenta: String::from("#ea76cb"),
                cyan: String::from("#179299"),
                white: String::from("#acb0be"),
                bright_black: String::from("#6c6f85"),
                bright_red: String::from("#d20f39"),
                bright_green: String::from("#40a02b"),
                bright_yellow: String::from("#df8e1d"),
                bright_blue: String::from("#1e66f5"),
                bright_magenta: String::from("#ea76cb"),
                bright_cyan: String::from("#179299"),
                bright_white: String::from("#bcc0cc"),
                bright_foreground: Some(String::from("#4c4f69")),
                dim_foreground: String::from("#6c6f85"),
                dim_black: String::from("#4c4f69"),
                dim_red: String::from("#a10c2d"),
                dim_green: String::from("#338022"),
                dim_yellow: String::from("#b27117"),
                dim_blue: String::from("#1852c4"),
                dim_magenta: String::from("#bb5ea2"),
                dim_cyan: String::from("#12747a"),
                dim_white: String::from("#8c8fa1"),
            },
        }
    }

    // UI Colors
    fn bg_base(&self) -> iced::Color {
        match self {
            AppTheme::Dark => color!(0x1e1e2e),
            AppTheme::Light => color!(0xeff1f5),
        }
    }

    fn bg_surface(&self) -> iced::Color {
        match self {
            AppTheme::Dark => color!(0x181825),
            AppTheme::Light => color!(0xe6e9ef),
        }
    }

    fn bg_overlay(&self) -> iced::Color {
        match self {
            AppTheme::Dark => color!(0x313244),
            AppTheme::Light => color!(0xdce0e8),
        }
    }

    fn text_primary(&self) -> iced::Color {
        match self {
            AppTheme::Dark => color!(0xcdd6f4),
            AppTheme::Light => color!(0x4c4f69),
        }
    }

    fn text_secondary(&self) -> iced::Color {
        match self {
            AppTheme::Dark => color!(0x6c7086),
            AppTheme::Light => color!(0x8c8fa1),
        }
    }

    fn text_muted(&self) -> iced::Color {
        match self {
            AppTheme::Dark => color!(0x45475a),
            AppTheme::Light => color!(0xbcc0cc),
        }
    }

    fn accent(&self) -> iced::Color {
        match self {
            AppTheme::Dark => color!(0x89b4fa),
            AppTheme::Light => color!(0x1e66f5),
        }
    }

    fn border(&self) -> iced::Color {
        match self {
            AppTheme::Dark => color!(0x45475a),
            AppTheme::Light => color!(0xccd0da),
        }
    }

    fn success(&self) -> iced::Color {
        match self {
            AppTheme::Dark => color!(0xa6e3a1),
            AppTheme::Light => color!(0x40a02b),
        }
    }

    fn warning(&self) -> iced::Color {
        match self {
            AppTheme::Dark => color!(0xf9e2af),
            AppTheme::Light => color!(0xdf8e1d),
        }
    }

    fn danger(&self) -> iced::Color {
        match self {
            AppTheme::Dark => color!(0xf38ba8),
            AppTheme::Light => color!(0xd20f39),
        }
    }

    fn diff_add_bg(&self) -> iced::Color {
        match self {
            AppTheme::Dark => color!(0x1a3a1a),
            AppTheme::Light => color!(0xd4f4d4),
        }
    }

    fn diff_del_bg(&self) -> iced::Color {
        match self {
            AppTheme::Dark => color!(0x3a1a1a),
            AppTheme::Light => color!(0xf4d4d4),
        }
    }

    fn diff_add_highlight(&self) -> iced::Color {
        match self {
            AppTheme::Dark => color!(0x3a6b3a),
            AppTheme::Light => color!(0x90d090),
        }
    }

    fn diff_del_highlight(&self) -> iced::Color {
        match self {
            AppTheme::Dark => color!(0x8b3a3a),
            AppTheme::Light => color!(0xd09090),
        }
    }

    fn bg_crust(&self) -> iced::Color {
        match self {
            AppTheme::Dark => color!(0x11111b),
            AppTheme::Light => color!(0xdce0e8),
        }
    }

    fn mauve(&self) -> iced::Color {
        match self {
            AppTheme::Dark => color!(0xcba6f7),
            AppTheme::Light => color!(0x8839ef),
        }
    }

    fn peach(&self) -> iced::Color {
        match self {
            AppTheme::Dark => color!(0xfab387),
            AppTheme::Light => color!(0xfe640b),
        }
    }

    fn surface0(&self) -> iced::Color {
        match self {
            AppTheme::Dark => color!(0x313244),
            AppTheme::Light => color!(0xccd0da),
        }
    }

    fn surface2(&self) -> iced::Color {
        match self {
            AppTheme::Dark => color!(0x585b70),
            AppTheme::Light => color!(0xacb0be),
        }
    }

    fn overlay0(&self) -> iced::Color {
        match self {
            AppTheme::Dark => color!(0x6c7086),
            AppTheme::Light => color!(0x9ca0b0),
        }
    }

    fn overlay1(&self) -> iced::Color {
        match self {
            AppTheme::Dark => color!(0x7f849c),
            AppTheme::Light => color!(0x8c8fa1),
        }
    }
}

fn main() -> iced::Result {
    // Load app icon from embedded PNG
    let icon = iced::window::icon::from_file_data(include_bytes!("../assets/icon.png"), None).ok();

    iced::application(App::new, App::update, App::view)
        .title(App::title)
        .window_size(Size {
            width: 1400.0,
            height: 800.0,
        })
        .window(iced::window::Settings {
            icon,
            ..Default::default()
        })
        .subscription(App::subscription)
        .run()
}

// Sidebar mode toggle
#[derive(Debug, Clone, Copy, PartialEq)]
enum SidebarMode {
    Git,
    Files,
}

// Git file entry
#[derive(Debug, Clone)]
struct FileEntry {
    path: String,
    status: String,
    is_staged: bool,
}

// File tree entry for explorer
#[derive(Debug, Clone)]
struct FileTreeEntry {
    name: String,
    path: PathBuf,
    is_dir: bool,
}

// Inline change for word-level diffs
#[derive(Debug, Clone)]
struct InlineChange {
    change_type: ChangeType,
    value: String,
}

#[derive(Debug, Clone, PartialEq)]
enum ChangeType {
    Equal,
    Insert,
    Delete,
}

// Diff line for display
#[derive(Debug, Clone)]
struct DiffLine {
    content: String,
    line_type: DiffLineType,
    old_line_num: Option<u32>,
    new_line_num: Option<u32>,
    inline_changes: Option<Vec<InlineChange>>,
}

#[derive(Debug, Clone, PartialEq)]
enum DiffLineType {
    Context,
    Addition,
    Deletion,
    Header,
}

// Search state for terminal scrollback
#[derive(Debug, Clone, Default)]
struct SearchState {
    is_active: bool,
    query: String,
    matches: Vec<SearchMatch>,
    current_match: usize,
}

// Console panel constants
const CONSOLE_HEADER_HEIGHT: f32 = 32.0;
const CONSOLE_DIVIDER_HEIGHT: f32 = 3.0;
const DEFAULT_CONSOLE_HEIGHT: f32 = 200.0;
const MAX_CONSOLE_LINES: usize = 1000;

// Console panel status
#[derive(Debug, Clone, Copy, PartialEq)]
enum ConsoleStatus {
    Running,
    Stopped,
    Error,
    NoneConfigured,
}

#[derive(Debug, Clone)]
struct ConsoleOutputLine {
    timestamp: String,
    content: String,
    is_stderr: bool,
}

// Sent through mpsc channel from background task
#[derive(Debug)]
enum ConsoleOutputMessage {
    Stdout(String),
    Stderr(String),
    Exited(Option<i32>),
}

struct ConsoleState {
    run_command: Option<String>,
    status: ConsoleStatus,
    exit_code: Option<i32>,
    started_at: Option<std::time::Instant>,
    stopped_at: Option<std::time::Instant>,
    output_lines: Vec<ConsoleOutputLine>,
    output_rx: Option<tokio::sync::mpsc::UnboundedReceiver<ConsoleOutputMessage>>,
    child_killer: Option<tokio::sync::oneshot::Sender<()>>,
}

impl ConsoleState {
    fn new(run_command: Option<String>) -> Self {
        let status = if run_command.is_some() {
            ConsoleStatus::Stopped
        } else {
            ConsoleStatus::NoneConfigured
        };
        Self {
            run_command,
            status,
            exit_code: None,
            started_at: None,
            stopped_at: None,
            output_lines: Vec::new(),
            output_rx: None,
            child_killer: None,
        }
    }

    fn push_line(&mut self, content: String, is_stderr: bool) {
        let now = chrono::Local::now();
        self.output_lines.push(ConsoleOutputLine {
            timestamp: now.format("%H:%M:%S").to_string(),
            content,
            is_stderr,
        });
        // Cap output buffer
        if self.output_lines.len() > MAX_CONSOLE_LINES {
            let drain_count = self.output_lines.len() - MAX_CONSOLE_LINES;
            self.output_lines.drain(..drain_count);
        }
    }

    fn clear_output(&mut self) {
        self.output_lines.clear();
    }

    fn uptime_string(&self) -> String {
        if let Some(started) = self.started_at {
            let elapsed = if self.status == ConsoleStatus::Running {
                started.elapsed()
            } else if let Some(stopped) = self.stopped_at {
                stopped.duration_since(started)
            } else {
                started.elapsed()
            };
            let secs = elapsed.as_secs();
            if secs < 60 {
                format!("\u{2191} {}s", secs)
            } else if secs < 3600 {
                format!("\u{2191} {}m", secs / 60)
            } else {
                format!("\u{2191} {}h{}m", secs / 3600, (secs % 3600) / 60)
            }
        } else {
            String::new()
        }
    }

    fn is_running(&self) -> bool {
        self.status == ConsoleStatus::Running
    }

    fn spawn_process(&mut self, dir: &PathBuf) {
        let cmd_str = match &self.run_command {
            Some(cmd) => cmd.clone(),
            None => return,
        };

        // Set up channels
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let (kill_tx, kill_rx) = tokio::sync::oneshot::channel::<()>();

        self.output_rx = Some(rx);
        self.child_killer = Some(kill_tx);
        self.status = ConsoleStatus::Running;
        self.exit_code = None;
        self.started_at = Some(std::time::Instant::now());
        self.stopped_at = None;

        let dir = dir.clone();

        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            use tokio::process::Command;

            // Use a login shell so the user's full environment is available
            // (bun, nvm, cargo, etc. all add to PATH via shell profiles)
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());

            let mut cmd = Command::new(&shell);
            cmd.arg("-l")
                .arg("-c")
                .arg(&cmd_str)
                .current_dir(&dir)
                .env("TERM", "dumb")
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());

            // Spawn in its own process group so we can kill the entire tree
            #[cfg(unix)]
            cmd.process_group(0);

            let mut child = match cmd.spawn() {
                Ok(child) => child,
                Err(e) => {
                    let _ = tx.send(ConsoleOutputMessage::Stderr(format!("Failed to start: {}", e)));
                    let _ = tx.send(ConsoleOutputMessage::Exited(Some(1)));
                    return;
                }
            };

            let child_pid = child.id();

            let stdout = child.stdout.take().unwrap();
            let stderr = child.stderr.take().unwrap();

            let tx_out = tx.clone();
            let stdout_task = tokio::spawn(async move {
                let reader = BufReader::new(stdout);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if tx_out.send(ConsoleOutputMessage::Stdout(line)).is_err() {
                        break;
                    }
                }
            });

            let tx_err = tx.clone();
            let stderr_task = tokio::spawn(async move {
                let reader = BufReader::new(stderr);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if tx_err.send(ConsoleOutputMessage::Stderr(line)).is_err() {
                        break;
                    }
                }
            });

            // Wait for either kill signal or natural exit
            tokio::select! {
                _ = kill_rx => {
                    // Kill the entire process group (shell + all children)
                    #[cfg(unix)]
                    if let Some(pid) = child_pid {
                        unsafe { libc::kill(-(pid as i32), libc::SIGTERM); }
                        // Give processes a moment to clean up, then force kill
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                        unsafe { libc::kill(-(pid as i32), libc::SIGKILL); }
                    }
                    #[cfg(not(unix))]
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    let _ = tx.send(ConsoleOutputMessage::Exited(None));
                }
                status = child.wait() => {
                    let code = status.ok().and_then(|s| s.code());
                    let _ = tx.send(ConsoleOutputMessage::Exited(code));
                }
            }

            stdout_task.abort();
            stderr_task.abort();
        });
    }

    fn kill_process(&mut self) {
        if let Some(killer) = self.child_killer.take() {
            let _ = killer.send(());
        }
        self.stopped_at = Some(std::time::Instant::now());
        // output_rx will drain remaining messages including Exited
    }
}

fn detect_run_command(dir: &PathBuf) -> Option<String> {
    // Detect package manager (used by multiple checks)
    let detect_pm = |dir: &PathBuf| -> &str {
        if dir.join("bun.lockb").exists() || dir.join("bun.lock").exists() {
            "bun"
        } else if dir.join("yarn.lock").exists() {
            "yarn"
        } else if dir.join("pnpm-lock.yaml").exists() {
            "pnpm"
        } else {
            "npm"
        }
    };

    // 1. Tauri app (has src-tauri/ directory with Cargo.toml)
    if dir.join("src-tauri").join("Cargo.toml").exists() {
        // Check if package.json has a "tauri" script (frontend-driven setup)
        if dir.join("package.json").exists() {
            let pm = detect_pm(dir);
            if let Ok(contents) = std::fs::read_to_string(dir.join("package.json")) {
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&contents) {
                    if let Some(scripts) = json.get("scripts").and_then(|s| s.as_object()) {
                        if scripts.contains_key("tauri") {
                            return Some(format!("{} run tauri", pm));
                        }
                        if scripts.contains_key("tauri:dev") {
                            return Some(format!("{} run tauri:dev", pm));
                        }
                    }
                }
            }
        }
        // Fallback: run cargo tauri dev from inside src-tauri
        return Some("cd src-tauri && cargo tauri dev".to_string());
    }

    // 2. package.json (check before bare Cargo.toml for hybrid projects)
    if dir.join("package.json").exists() {
        let pm = detect_pm(dir);

        if let Ok(contents) = std::fs::read_to_string(dir.join("package.json")) {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&contents) {
                if let Some(scripts) = json.get("scripts").and_then(|s| s.as_object()) {
                    // Priority: tauri dev > dev > start
                    for script_name in &["tauri dev", "dev", "start"] {
                        if scripts.contains_key(*script_name) {
                            if *script_name == "start" && pm == "npm" {
                                return Some("npm start".to_string());
                            }
                            return Some(format!("{} run {}", pm, script_name));
                        }
                    }
                }
            }
        }
    }

    // 3. Cargo.toml (plain Rust project)
    if dir.join("Cargo.toml").exists() {
        return Some("cargo run".to_string());
    }

    // 4. docker-compose
    if dir.join("docker-compose.yml").exists() || dir.join("docker-compose.yaml").exists() {
        return Some("docker compose up".to_string());
    }

    // 5. Go project
    if dir.join("go.mod").exists() {
        if dir.join("main.go").exists() || dir.join("cmd").is_dir() {
            return Some("go run .".to_string());
        }
    }

    None
}

// Tab state
struct TabState {
    id: usize,
    repo_path: PathBuf,
    repo_name: String,
    terminal: Option<iced_term::Terminal>,
    staged: Vec<FileEntry>,
    unstaged: Vec<FileEntry>,
    untracked: Vec<FileEntry>,
    branch_name: String,
    last_poll: Instant,
    selected_file: Option<String>,
    selected_is_staged: bool,
    diff_lines: Vec<DiffLine>,
    // For keyboard navigation
    file_index: i32,
    // Track when tab was created for delayed terminal display
    created_at: Instant,
    // Terminal title (set by shell/programs via OSC escape codes)
    terminal_title: Option<String>,
    // Sidebar mode (Git or Files)
    sidebar_mode: SidebarMode,
    // File explorer state
    current_dir: PathBuf,
    file_tree: Vec<FileTreeEntry>,
    // File viewer state
    viewing_file_path: Option<PathBuf>,
    file_content: Vec<String>,
    image_handle: Option<image::Handle>,
    // Markdown WebView content (rendered HTML)
    webview_content: Option<String>,
    // Search state
    search: SearchState,
}

impl TabState {
    fn new(id: usize, repo_path: PathBuf) -> Self {
        let repo_name = repo_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "repo".to_string());
        let current_dir = repo_path.clone();

        Self {
            id,
            repo_path,
            repo_name,
            terminal: None,
            staged: Vec::new(),
            unstaged: Vec::new(),
            untracked: Vec::new(),
            branch_name: String::from("main"),
            last_poll: Instant::now(),
            selected_file: None,
            selected_is_staged: false,
            diff_lines: Vec::new(),
            file_index: -1,
            created_at: Instant::now(),
            terminal_title: None,
            sidebar_mode: SidebarMode::Git,
            current_dir,
            file_tree: Vec::new(),
            viewing_file_path: None,
            file_content: Vec::new(),
            image_handle: None,
            webview_content: None,
            search: SearchState::default(),
        }
    }

    fn is_image_file(path: &PathBuf) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|ext| matches!(ext.to_lowercase().as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico"))
            .unwrap_or(false)
    }

    fn is_markdown_file(path: &PathBuf) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|ext| matches!(ext.to_lowercase().as_str(), "md" | "markdown"))
            .unwrap_or(false)
    }

    /// Try to extract a directory path from the terminal title.
    /// Handles common shell title formats:
    /// - "~/path" or "/absolute/path"
    /// - "~/path (extra)" - path followed by parenthetical info
    /// - "dirname — zsh" (starship style)
    /// - "user@host:~/path" (standard zsh/bash)
    fn extract_dir_from_title(title: &str) -> Option<PathBuf> {
        let home = std::env::var("HOME").ok()?;

        // Helper to expand ~ and check if path exists
        let try_path = |s: &str| -> Option<PathBuf> {
            let s = s.trim();
            let expanded = if s.starts_with("~/") {
                format!("{}/{}", home, &s[2..])
            } else if s == "~" {
                home.clone()
            } else if s.starts_with('/') {
                s.to_string()
            } else {
                return None;
            };
            let path = PathBuf::from(&expanded);
            if path.is_dir() { Some(path) } else { None }
        };

        // First: look for path at the start, ending at " (" which often indicates extra info
        // e.g., "~/GitRepo/project (18) ⌘1" -> extract "~/GitRepo/project"
        if let Some(paren_pos) = title.find(" (") {
            let candidate = &title[..paren_pos];
            if let Some(path) = try_path(candidate) {
                return Some(path);
            }
        }

        // Second: try the whole title as-is
        if let Some(path) = try_path(title) {
            return Some(path);
        }

        // Third: split by em-dash or colon (but NOT hyphen, as paths often have hyphens)
        for sep in &['\u{2014}', ':'] { // em-dash, colon
            for part in title.split(*sep) {
                // Also try stripping at " (" within each part
                let part = if let Some(pos) = part.find(" (") {
                    &part[..pos]
                } else {
                    part
                };
                if let Some(path) = try_path(part) {
                    return Some(path);
                }
            }
        }

        None
    }

    fn fetch_file_tree(&mut self, show_hidden: bool) {
        self.file_tree.clear();

        if let Ok(entries) = std::fs::read_dir(&self.current_dir) {
            let mut dirs: Vec<FileTreeEntry> = Vec::new();
            let mut files: Vec<FileTreeEntry> = Vec::new();

            for entry in entries.flatten() {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();

                // Skip hidden files unless show_hidden is true
                if !show_hidden && name.starts_with('.') {
                    continue;
                }
                // Always skip node_modules and target
                if name == "node_modules" || name == "target" {
                    continue;
                }

                let is_dir = path.is_dir();
                let entry = FileTreeEntry { name, path, is_dir };

                if is_dir {
                    dirs.push(entry);
                } else {
                    files.push(entry);
                }
            }

            // Sort alphabetically
            dirs.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
            files.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));

            // Dirs first, then files
            self.file_tree.extend(dirs);
            self.file_tree.extend(files);
        }
    }

    fn load_file(&mut self, path: &PathBuf, is_dark_theme: bool) {
        self.file_content.clear();
        self.image_handle = None;
        self.webview_content = None;
        self.viewing_file_path = Some(path.clone());

        if Self::is_markdown_file(path) {
            // Load as markdown - render to HTML and store for potential browser viewing
            if let Ok(content) = std::fs::read_to_string(path) {
                let html = markdown::render_markdown_to_html(&content, is_dark_theme);
                self.webview_content = Some(html);
                // Also store raw content for Iced-based rendering
                self.file_content = content.lines().map(|s| s.to_string()).collect();
            }
        } else if Self::is_image_file(path) {
            // Load as image
            self.image_handle = Some(image::Handle::from_path(path));
        } else if let Ok(content) = std::fs::read_to_string(path) {
            // Load as text
            self.file_content = content.lines().map(|s| s.to_string()).collect();
        }
    }

    fn total_changes(&self) -> usize {
        self.staged.len() + self.unstaged.len() + self.untracked.len()
    }

    fn all_files(&self) -> Vec<&FileEntry> {
        self.staged
            .iter()
            .chain(self.unstaged.iter())
            .chain(self.untracked.iter())
            .collect()
    }

    fn fetch_status(&mut self) {
        if let Ok(repo) = Repository::open(&self.repo_path) {
            // Get branch name
            if let Ok(head) = repo.head() {
                if let Some(name) = head.shorthand() {
                    self.branch_name = name.to_string();
                }
            }

            // Get file statuses
            let mut opts = StatusOptions::new();
            opts.include_untracked(true)
                .recurse_untracked_dirs(true)
                .include_ignored(false);

            self.staged.clear();
            self.unstaged.clear();
            self.untracked.clear();

            if let Ok(statuses) = repo.statuses(Some(&mut opts)) {
                for entry in statuses.iter() {
                    let path = entry.path().unwrap_or("").to_string();
                    let status = entry.status();

                    if status.contains(Status::INDEX_NEW)
                        || status.contains(Status::INDEX_MODIFIED)
                        || status.contains(Status::INDEX_DELETED)
                        || status.contains(Status::INDEX_RENAMED)
                    {
                        self.staged.push(FileEntry {
                            path: path.clone(),
                            status: status_char(status, true),
                            is_staged: true,
                        });
                    }

                    if status.contains(Status::WT_MODIFIED)
                        || status.contains(Status::WT_DELETED)
                        || status.contains(Status::WT_RENAMED)
                    {
                        self.unstaged.push(FileEntry {
                            path: path.clone(),
                            status: status_char(status, false),
                            is_staged: false,
                        });
                    }

                    if status.contains(Status::WT_NEW) {
                        self.untracked.push(FileEntry {
                            path,
                            status: "?".to_string(),
                            is_staged: false,
                        });
                    }
                }
            }
        }
        self.last_poll = Instant::now();
    }

    fn fetch_diff(&mut self, file_path: &str, staged: bool) {
        self.diff_lines.clear();

        let Ok(repo) = Repository::open(&self.repo_path) else {
            return;
        };

        // Check if it's untracked
        let is_untracked = self.untracked.iter().any(|f| f.path == file_path);

        if is_untracked {
            // Read file content for untracked files
            let full_path = self.repo_path.join(file_path);
            if let Ok(content) = std::fs::read_to_string(&full_path) {
                self.diff_lines.push(DiffLine {
                    content: format!("@@ -0,0 +1,{} @@ (new file)", content.lines().count()),
                    line_type: DiffLineType::Header,
                    old_line_num: None,
                    new_line_num: None,
                    inline_changes: None,
                });
                for (i, line) in content.lines().enumerate() {
                    self.diff_lines.push(DiffLine {
                        content: line.to_string(),
                        line_type: DiffLineType::Addition,
                        old_line_num: None,
                        new_line_num: Some((i + 1) as u32),
                        inline_changes: None,
                    });
                }
            }
            return;
        }

        let mut diff_opts = DiffOptions::new();
        diff_opts.pathspec(file_path);

        let diff = if staged {
            let head_tree = repo.head().ok().and_then(|h| h.peel_to_tree().ok());
            repo.diff_tree_to_index(head_tree.as_ref(), None, Some(&mut diff_opts))
        } else {
            repo.diff_index_to_workdir(None, Some(&mut diff_opts))
        };

        if let Ok(diff) = diff {
            let _ = diff.print(git2::DiffFormat::Patch, |_delta, hunk, line| {
                let content = String::from_utf8_lossy(line.content()).to_string();
                let content = content.trim_end().to_string();

                match line.origin() {
                    'H' => {
                        if let Some(h) = hunk {
                            self.diff_lines.push(DiffLine {
                                content: format!(
                                    "@@ -{},{} +{},{} @@",
                                    h.old_start(),
                                    h.old_lines(),
                                    h.new_start(),
                                    h.new_lines()
                                ),
                                line_type: DiffLineType::Header,
                                old_line_num: None,
                                new_line_num: None,
                                inline_changes: None,
                            });
                        }
                    }
                    '+' => {
                        self.diff_lines.push(DiffLine {
                            content,
                            line_type: DiffLineType::Addition,
                            old_line_num: None,
                            new_line_num: line.new_lineno(),
                            inline_changes: None,
                        });
                    }
                    '-' => {
                        self.diff_lines.push(DiffLine {
                            content,
                            line_type: DiffLineType::Deletion,
                            old_line_num: line.old_lineno(),
                            new_line_num: None,
                            inline_changes: None,
                        });
                    }
                    ' ' => {
                        self.diff_lines.push(DiffLine {
                            content,
                            line_type: DiffLineType::Context,
                            old_line_num: line.old_lineno(),
                            new_line_num: line.new_lineno(),
                            inline_changes: None,
                        });
                    }
                    _ => {}
                }
                true
            });

            // Post-process to add word-level diffs
            self.add_word_diffs();
        }
    }

    fn add_word_diffs(&mut self) {
        let mut i = 0;
        while i < self.diff_lines.len() {
            if self.diff_lines[i].line_type == DiffLineType::Deletion {
                // Count consecutive deletions
                let mut del_end = i + 1;
                while del_end < self.diff_lines.len()
                    && self.diff_lines[del_end].line_type == DiffLineType::Deletion
                {
                    del_end += 1;
                }

                // Count consecutive additions after deletions
                let mut add_end = del_end;
                while add_end < self.diff_lines.len()
                    && self.diff_lines[add_end].line_type == DiffLineType::Addition
                {
                    add_end += 1;
                }

                let del_count = del_end - i;
                let add_count = add_end - del_end;

                // Pair up deletions with additions
                let pairs = del_count.min(add_count);
                for j in 0..pairs {
                    let del_idx = i + j;
                    let add_idx = del_end + j;

                    let del_content = self.diff_lines[del_idx].content.clone();
                    let add_content = self.diff_lines[add_idx].content.clone();

                    let word_changes = compute_word_diff(&del_content, &add_content);

                    // Check if there's meaningful overlap
                    let has_equal = word_changes.iter().any(|c| c.change_type == ChangeType::Equal);

                    if has_equal {
                        // Build inline changes for deletion line
                        let del_inline: Vec<InlineChange> = word_changes
                            .iter()
                            .filter(|c| c.change_type == ChangeType::Equal || c.change_type == ChangeType::Delete)
                            .cloned()
                            .collect();

                        // Build inline changes for addition line
                        let add_inline: Vec<InlineChange> = word_changes
                            .iter()
                            .filter(|c| c.change_type == ChangeType::Equal || c.change_type == ChangeType::Insert)
                            .cloned()
                            .collect();

                        self.diff_lines[del_idx].inline_changes = Some(del_inline);
                        self.diff_lines[add_idx].inline_changes = Some(add_inline);
                    }
                }

                i = add_end;
            } else {
                i += 1;
            }
        }
    }
}

// Workspace color palette
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum WorkspaceColor {
    Lavender,
    Blue,
    Green,
    Peach,
    Pink,
    Yellow,
    Red,
    Teal,
}

impl WorkspaceColor {
    fn color(&self, theme: &AppTheme) -> iced::Color {
        match theme {
            AppTheme::Dark => match self {
                Self::Lavender => color!(0xb4befe),
                Self::Blue => color!(0x89b4fa),
                Self::Green => color!(0xa6e3a1),
                Self::Peach => color!(0xfab387),
                Self::Pink => color!(0xf5c2e7),
                Self::Yellow => color!(0xf9e2af),
                Self::Red => color!(0xf38ba8),
                Self::Teal => color!(0x94e2d5),
            },
            AppTheme::Light => match self {
                Self::Lavender => color!(0x7287fd),
                Self::Blue => color!(0x1e66f5),
                Self::Green => color!(0x40a02b),
                Self::Peach => color!(0xfe640b),
                Self::Pink => color!(0xea76cb),
                Self::Yellow => color!(0xdf8e1d),
                Self::Red => color!(0xd20f39),
                Self::Teal => color!(0x179299),
            },
        }
    }

    fn from_index(idx: usize) -> Self {
        match idx % 8 {
            0 => Self::Lavender,
            1 => Self::Blue,
            2 => Self::Green,
            3 => Self::Peach,
            4 => Self::Pink,
            5 => Self::Yellow,
            6 => Self::Red,
            _ => Self::Teal,
        }
    }
}

// Workspace groups tabs by project
struct Workspace {
    name: String,
    abbrev: String,
    dir: PathBuf,
    color: WorkspaceColor,
    tabs: Vec<TabState>,
    active_tab: usize,
    console: ConsoleState,
}

impl Workspace {
    fn new(name: String, dir: PathBuf, color: WorkspaceColor) -> Self {
        let abbrev = Self::derive_abbrev(&name);
        let run_command = detect_run_command(&dir);
        let console = ConsoleState::new(run_command);
        Self {
            name,
            abbrev,
            dir,
            color,
            tabs: Vec::new(),
            active_tab: 0,
            console,
        }
    }

    fn derive_abbrev(name: &str) -> String {
        name.chars()
            .take(2)
            .collect::<String>()
            .to_uppercase()
    }

    fn active_tab(&self) -> Option<&TabState> {
        self.tabs.get(self.active_tab)
    }

    fn active_tab_mut(&mut self) -> Option<&mut TabState> {
        self.tabs.get_mut(self.active_tab)
    }
}

fn compute_word_diff(old_text: &str, new_text: &str) -> Vec<InlineChange> {
    let diff = TextDiff::from_words(old_text, new_text);
    diff.iter_all_changes()
        .map(|change| {
            let change_type = match change.tag() {
                ChangeTag::Equal => ChangeType::Equal,
                ChangeTag::Insert => ChangeType::Insert,
                ChangeTag::Delete => ChangeType::Delete,
            };
            InlineChange {
                change_type,
                value: change.value().to_string(),
            }
        })
        .collect()
}

fn status_char(status: Status, staged: bool) -> String {
    if staged {
        if status.contains(Status::INDEX_NEW) {
            "A".to_string()
        } else if status.contains(Status::INDEX_MODIFIED) {
            "M".to_string()
        } else if status.contains(Status::INDEX_DELETED) {
            "D".to_string()
        } else if status.contains(Status::INDEX_RENAMED) {
            "R".to_string()
        } else {
            "?".to_string()
        }
    } else if status.contains(Status::WT_MODIFIED) {
        "M".to_string()
    } else if status.contains(Status::WT_DELETED) {
        "D".to_string()
    } else if status.contains(Status::WT_RENAMED) {
        "R".to_string()
    } else {
        "?".to_string()
    }
}

#[derive(Debug, Clone)]
pub enum Event {
    Terminal(usize, iced_term::Event),
    Tick,
    InitMenu,
    CheckMenu,
    TabSelect(usize),
    TabClose(usize),
    OpenFolder,
    FolderSelected(Option<PathBuf>),
    FileSelect(String, bool),
    FileSelectByIndex(i32),
    ClearSelection,
    KeyPressed(Key, Modifiers),
    // File explorer events
    ToggleSidebarMode,
    NavigateDir(PathBuf),
    NavigateUp,
    ViewFile(PathBuf),
    CloseFileView,
    CopyFileContent,
    OpenFileInBrowser,
    // Theme
    ToggleTheme,
    // Font size - Terminal
    IncreaseTerminalFont,
    DecreaseTerminalFont,
    ClearTerminal,
    // Font size - UI
    IncreaseUiFont,
    DecreaseUiFont,
    // Hidden files
    ToggleHidden,
    // Divider dragging
    DividerDragStart,
    DividerDragEnd,
    MouseMoved(f32, f32),
    // Search events
    ToggleSearch,
    SearchQueryChanged(String),
    SearchExecute,
    SearchNext,
    SearchPrev,
    SearchClose,
    // Markdown preview
    OpenMarkdownInBrowser,
    // Window events
    WindowResized(f32, f32),
    WindowCloseRequested,
    // Workspace events
    WorkspaceSelect(usize),
    WorkspaceClose(usize),
    WorkspaceCreate,
    WorkspaceCreated(Option<PathBuf>),
    // Slide animation events
    SlideAnimationTick,
    // Edge peek events
    EdgePeekEnter(bool),  // true=right, false=left
    EdgePeekExit,
    SlideScrolled(scrollable::Viewport),
    // Console panel events
    ConsoleToggle,
    ConsoleStart,
    ConsoleStop,
    ConsoleRestart,
    ConsoleClearOutput,
    ConsoleDividerDragStart,
    ConsoleCommandEditStart,
    ConsoleCommandChanged(String),
    ConsoleCommandSubmit,
    ConsoleCommandCancel,
}

struct App {
    title: String,
    workspaces: Vec<Workspace>,
    active_workspace_idx: usize,
    next_tab_id: usize,
    theme: AppTheme,
    terminal_font_size: f32,
    ui_font_size: f32,
    sidebar_width: f32,
    scrollback_lines: usize,
    dragging_divider: bool,
    show_hidden: bool,
    window_size: (f32, f32),
    log_server_state: log_server::ServerState,
    console_expanded: bool,
    console_height: f32,
    dragging_console_divider: bool,
    editing_console_command: Option<String>,
    // Slide animation state
    slide_offset: f32,
    slide_target: f32,
    slide_animating: bool,
    slide_start_time: Option<Instant>,
    slide_start_offset: f32,
    // User scroll tracking (for swipe debounce)
    last_user_scroll: Option<Instant>,
    // Edge peek state
    edge_peek_left: bool,
    edge_peek_right: bool,
}

const SPINE_WIDTH: f32 = 16.0;

const SLIDE_DURATION_MS: f32 = 400.0;
const SWIPE_DEBOUNCE_MS: u64 = 150;
const EDGE_PEEK_ZONE: f32 = 30.0;

fn workspace_scrollable_id() -> iced::widget::Id {
    iced::widget::Id::new("ws-slide")
}

const MIN_FONT_SIZE: f32 = 10.0;
const MAX_FONT_SIZE: f32 = 24.0;
const FONT_SIZE_STEP: f32 = 1.0;

impl App {
    /// UI font size
    fn ui_font(&self) -> f32 {
        self.ui_font_size
    }

    /// Small UI font size (for hints, secondary text)
    fn ui_font_small(&self) -> f32 {
        self.ui_font_size - 1.0
    }

    fn save_config(&self) {
        let config = Config {
            terminal_font_size: self.terminal_font_size,
            ui_font_size: self.ui_font_size,
            sidebar_width: self.sidebar_width,
            scrollback_lines: self.scrollback_lines,
            font_size: None,
            theme: match self.theme {
                AppTheme::Dark => "dark".to_string(),
                AppTheme::Light => "light".to_string(),
            },
            show_hidden: self.show_hidden,
            console_height: self.console_height,
            console_expanded: self.console_expanded,
        };
        config.save();
    }

    fn save_workspaces(&self) {
        let ws_file = WorkspacesFile {
            workspaces: self.workspaces.iter().map(|ws| {
                WorkspaceConfig {
                    name: ws.name.clone(),
                    abbrev: ws.abbrev.clone(),
                    dir: ws.dir.to_string_lossy().to_string(),
                    color: ws.color,
                    tabs: ws.tabs.iter().map(|tab| {
                        WorkspaceTabConfig {
                            dir: tab.current_dir.to_string_lossy().to_string(),
                        }
                    }).collect(),
                    run_command: ws.console.run_command.clone(),
                }
            }).collect(),
            active_workspace: self.active_workspace_idx,
        };
        ws_file.save();
    }

    /// Update the log server with current terminal content
    fn update_log_server(&self) {
        let state = self.log_server_state.clone();
        let mut terminal_snapshots = std::collections::HashMap::new();
        let mut file_snapshots = std::collections::HashMap::new();

        // Collect terminal content and file content from all tabs across all workspaces
        for tab in self.workspaces.iter().flat_map(|ws| ws.tabs.iter()) {
            if let Some(term) = &tab.terminal {
                let content = term.get_all_text();
                let snapshot = log_server::TerminalSnapshot {
                    tab_id: tab.id,
                    tab_name: tab.repo_name.clone(),
                    content,
                };
                terminal_snapshots.insert(tab.id, snapshot);
            }

            // If tab is viewing a file, add it to file snapshots
            if let Some(file_path) = &tab.viewing_file_path {
                if !tab.file_content.is_empty() {
                    let content = tab.file_content.join("\n");
                    let snapshot = log_server::FileSnapshot {
                        tab_id: tab.id,
                        file_path: file_path.to_string_lossy().to_string(),
                        content,
                    };
                    file_snapshots.insert(tab.id, snapshot);
                }
            }
        }

        // Update the shared state (spawn a task to avoid blocking)
        tokio::spawn(async move {
            let mut terminals = state.terminals.write().await;
            *terminals = terminal_snapshots;
            let mut files = state.files.write().await;
            *files = file_snapshots;
        });
    }
}

impl App {
    fn new() -> (Self, Task<Event>) {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let config = Config::load();

        let theme = if config.theme == "light" {
            AppTheme::Light
        } else {
            AppTheme::Dark
        };

        // Handle migration from old single font_size config
        let (terminal_font, ui_font) = if let Some(old_size) = config.font_size {
            (old_size, old_size - 1.0)
        } else {
            (config.terminal_font_size, config.ui_font_size)
        };

        // Initialize log server state
        let log_server_state = log_server::ServerState::new();

        // Start the HTTP log server in the background
        let server_state = log_server_state.clone();
        tokio::spawn(async move {
            log_server::start_server(server_state).await;
        });

        let mut app = Self {
            title: String::from("GitTerm"),
            workspaces: Vec::new(),
            active_workspace_idx: 0,
            next_tab_id: 0,
            theme,
            terminal_font_size: terminal_font.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE),
            ui_font_size: ui_font.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE),
            sidebar_width: config.sidebar_width.clamp(150.0, 600.0),
            scrollback_lines: config.scrollback_lines,
            dragging_divider: false,
            show_hidden: config.show_hidden,
            window_size: (1400.0, 800.0), // Initial size, updated on resize
            log_server_state,
            console_expanded: config.console_expanded,
            console_height: config.console_height.clamp(32.0, 600.0),
            dragging_console_divider: false,
            editing_console_command: None,
            slide_offset: 0.0,
            slide_target: 0.0,
            slide_animating: false,
            slide_start_time: None,
            slide_start_offset: 0.0,
            last_user_scroll: None,
            edge_peek_left: false,
            edge_peek_right: false,
        };

        // Try to restore workspaces from saved config
        if let Some(ws_file) = WorkspacesFile::load() {
            for ws_config in &ws_file.workspaces {
                let dir = PathBuf::from(&ws_config.dir);
                let home = std::env::var("HOME").unwrap_or_default();
                // If workspace dir is $HOME, name the workspace after its first tab's repo instead
                let name = if dir == PathBuf::from(&home) {
                    ws_config.tabs.first()
                        .map(|t| PathBuf::from(&t.dir))
                        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
                        .unwrap_or_else(|| ws_config.name.clone())
                } else {
                    ws_config.name.clone()
                };
                let mut workspace = Workspace::new(name, dir.clone(), ws_config.color);
                workspace.abbrev = ws_config.abbrev.clone();
                // Restore saved run command if present
                if let Some(cmd) = &ws_config.run_command {
                    workspace.console.run_command = Some(cmd.clone());
                    workspace.console.status = ConsoleStatus::Stopped;
                }

                if ws_config.tabs.is_empty() {
                    // Always have at least one tab
                    app.add_tab_to_workspace(&mut workspace, dir);
                } else {
                    for tab_config in &ws_config.tabs {
                        let tab_dir = PathBuf::from(&tab_config.dir);
                        app.add_tab_to_workspace(&mut workspace, tab_dir);
                    }
                }

                app.workspaces.push(workspace);
            }
            app.active_workspace_idx = ws_file.active_workspace.min(app.workspaces.len().saturating_sub(1));
        }

        // If no workspaces were loaded, create one from the current directory
        if app.workspaces.is_empty() {
            let dir = cwd;
            let name = dir
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "Home".to_string());
            let mut workspace = Workspace::new(name, dir.clone(), WorkspaceColor::Lavender);
            app.add_tab_to_workspace(&mut workspace, dir);
            app.workspaces.push(workspace);
        }

        // Set initial slide position for active workspace
        let viewport_width = app.content_viewport_width();
        let initial_offset = app.active_workspace_idx as f32 * viewport_width;
        app.slide_offset = initial_offset;
        app.slide_target = initial_offset;

        // Return a task to initialize the menu bar after the app starts
        (app, Task::done(Event::InitMenu))
    }

    fn add_tab_to_workspace(&mut self, workspace: &mut Workspace, repo_path: PathBuf) {
        let tab = self.create_tab(repo_path);
        workspace.tabs.push(tab);
        workspace.active_tab = workspace.tabs.len() - 1;
    }

    fn add_tab(&mut self, repo_path: PathBuf) {
        let tab = self.create_tab(repo_path);
        if let Some(ws) = self.active_workspace_mut() {
            ws.tabs.push(tab);
            ws.active_tab = ws.tabs.len() - 1;
        }
    }

    fn create_tab(&mut self, repo_path: PathBuf) -> TabState {
        let id = self.next_tab_id;
        self.next_tab_id += 1;

        let mut tab = TabState::new(id, repo_path.clone());

        // Get shell - platform-specific defaults
        #[cfg(target_os = "windows")]
        let shell = std::env::var("COMSPEC").unwrap_or_else(|_| "powershell.exe".to_string());

        #[cfg(not(target_os = "windows"))]
        let shell = std::env::var("SHELL").ok().or_else(|| {
            // When running as app bundle, SHELL may not be set - check passwd
            let user = std::env::var("USER").ok()?;
            let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
            for line in passwd.lines() {
                let parts: Vec<&str> = line.split(':').collect();
                if parts.first() == Some(&user.as_str()) {
                    return parts.get(6).map(|s| s.to_string());
                }
            }
            None
        }).unwrap_or_else(|| "/bin/zsh".to_string());

        // Build environment for PTY - app bundles have minimal env, so we need to set essentials
        let mut env = std::collections::HashMap::new();

        #[cfg(not(target_os = "windows"))]
        {
            env.insert("TERM".to_string(), "xterm-256color".to_string());
            env.insert("COLORTERM".to_string(), "truecolor".to_string());
            env.insert("LANG".to_string(), std::env::var("LANG").unwrap_or_else(|_| "en_US.UTF-8".to_string()));
            if let Ok(home) = std::env::var("HOME") {
                env.insert("HOME".to_string(), home.clone());
                env.insert("PATH".to_string(), format!("{}/.local/bin:{}/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin", home, home));
            }
            if let Ok(user) = std::env::var("USER") {
                env.insert("USER".to_string(), user.clone());
                env.insert("LOGNAME".to_string(), user);
            }
            env.insert("SHELL".to_string(), shell.clone());
        }

        #[cfg(target_os = "windows")]
        {
            // On Windows, inherit most environment variables from parent process
            for (key, value) in std::env::vars() {
                env.insert(key, value);
            }
        }

        // Add precmd hook to set terminal title to current directory
        // This enables the sidebar to sync with terminal directory changes
        env.insert("GITTERM_PRECMD".to_string(), "1".to_string());

        // Determine shell type for the right initialization
        let is_zsh = shell.contains("zsh");
        let is_bash = shell.contains("bash");
        let is_windows = cfg!(target_os = "windows");

        // Build args to inject precmd hook
        let args = if is_windows {
            // Windows shells don't use login flag
            vec![]
        } else if is_zsh {
            // For zsh: use ZDOTDIR to inject our precmd before user's config
            // Create a custom .zshrc that sets up title reporting then sources user config
            let home = std::env::var("HOME").unwrap_or_default();
            let gitterm_dir = format!("{home}/.config/gitterm/zsh");
            let gitterm_zshrc = format!("{gitterm_dir}/.zshrc");

            // Ensure the directory exists
            let _ = std::fs::create_dir_all(&gitterm_dir);
            let zshrc_content = format!(
                r#"# GitTerm shell integration - sets terminal title on directory change
_gitterm_set_title() {{ print -Pn "\e]0;%~\a" }}
autoload -Uz add-zsh-hook
add-zsh-hook precmd _gitterm_set_title
add-zsh-hook chpwd _gitterm_set_title
# Set title immediately
_gitterm_set_title
# Source user's normal config files
[[ -f "{home}/.zshenv" ]] && source "{home}/.zshenv"
[[ -f "{home}/.zprofile" ]] && source "{home}/.zprofile"
[[ -f "{home}/.zshrc" ]] && source "{home}/.zshrc"
"#
            );
            let _ = std::fs::write(&gitterm_zshrc, zshrc_content);

            env.insert("ZDOTDIR".to_string(), gitterm_dir);
            vec!["-l".to_string()]
        } else if is_bash {
            // For bash: use PROMPT_COMMAND
            env.insert("PROMPT_COMMAND".to_string(), r#"printf "\e]0;%s\a" "$PWD""#.to_string());
            vec!["-l".to_string()]
        } else {
            vec!["-l".to_string()]
        };

        let term_settings = iced_term::settings::Settings {
            backend: iced_term::settings::BackendSettings {
                program: shell,
                args,
                working_directory: Some(repo_path),
                scrollback_lines: self.scrollback_lines,
                env,
                ..Default::default()
            },
            theme: iced_term::settings::ThemeSettings::new(Box::new(
                self.theme.terminal_palette(),
            )),
            font: iced_term::settings::FontSettings {
                size: self.terminal_font_size,
                ..Default::default()
            },
        };

        if let Ok(terminal) = iced_term::Terminal::new(id as u64, term_settings) {
            tab.terminal = Some(terminal);
        }

        tab.fetch_status();
        tab
    }

    /// Width of the content area (window width minus spine)
    fn content_viewport_width(&self) -> f32 {
        (self.window_size.0 - SPINE_WIDTH).max(1.0)
    }

    fn active_workspace(&self) -> Option<&Workspace> {
        self.workspaces.get(self.active_workspace_idx)
    }

    fn active_workspace_mut(&mut self) -> Option<&mut Workspace> {
        self.workspaces.get_mut(self.active_workspace_idx)
    }

    fn active_tab(&self) -> Option<&TabState> {
        self.active_workspace().and_then(|ws| ws.active_tab())
    }

    fn active_tab_mut(&mut self) -> Option<&mut TabState> {
        self.active_workspace_mut().and_then(|ws| ws.active_tab_mut())
    }

    fn title(&self) -> String {
        if let Some(ws) = self.active_workspace() {
            if let Some(tab) = ws.active_tab() {
                format!("{} - {} - {}", self.title, ws.name, tab.repo_name)
            } else {
                format!("{} - {}", self.title, ws.name)
            }
        } else {
            self.title.clone()
        }
    }

    fn subscription(&self) -> Subscription<Event> {
        let mut subs = vec![
            iced::time::every(Duration::from_millis(5000)).map(|_| Event::Tick),
            // Poll menu events frequently
            iced::time::every(Duration::from_millis(50)).map(|_| Event::CheckMenu),
            iced::event::listen_with(|event, _status, _id| match event {
                iced::Event::Keyboard(keyboard::Event::KeyPressed {
                    key, modifiers, ..
                }) => Some(Event::KeyPressed(key, modifiers)),
                iced::Event::Mouse(iced::mouse::Event::CursorMoved { position }) => {
                    Some(Event::MouseMoved(position.x, position.y))
                }
                iced::Event::Mouse(iced::mouse::Event::ButtonReleased(
                    iced::mouse::Button::Left,
                )) => Some(Event::DividerDragEnd),
                iced::Event::Window(iced::window::Event::Resized(size)) => {
                    Some(Event::WindowResized(size.width, size.height))
                }
                iced::Event::Window(iced::window::Event::CloseRequested) => {
                    Some(Event::WindowCloseRequested)
                }
                _ => None,
            }),
        ];

        // Animation tick (~60fps) — when animating or waiting for swipe debounce
        if self.slide_animating || self.last_user_scroll.is_some() {
            subs.push(
                iced::time::every(Duration::from_millis(16))
                    .map(|_| Event::SlideAnimationTick),
            );
        }

        for ws in &self.workspaces {
            for tab in &ws.tabs {
                if let Some(term) = &tab.terminal {
                    subs.push(
                        term.subscription()
                            .with(tab.id)
                            .map(|(tab_id, e)| Event::Terminal(tab_id, e)),
                    );
                }
            }
        }

        Subscription::batch(subs)
    }

    fn update(&mut self, event: Event) -> Task<Event> {
        match event {
            Event::Terminal(tab_id, iced_term::Event::BackendCall(_, cmd)) => {
                // Don't forward keyboard input to terminal while editing console command
                if self.editing_console_command.is_some() {
                    return Task::none();
                }
                if let Some(tab) = self.workspaces.iter_mut().flat_map(|ws| ws.tabs.iter_mut()).find(|t| t.id == tab_id) {
                    if let Some(term) = &mut tab.terminal {
                        match term.handle(iced_term::Command::ProxyToBackend(cmd)) {
                            iced_term::actions::Action::Shutdown => {}
                            iced_term::actions::Action::ChangeTitle(title) => {
                                // Set tab-specific title
                                tab.terminal_title = Some(title.clone());

                                // Try to sync sidebar directory from terminal title
                                if let Some(dir) = TabState::extract_dir_from_title(&title) {
                                    if dir != tab.current_dir {
                                        tab.current_dir = dir.clone();
                                        // Always refresh file tree so it's ready when switching to Files mode
                                        tab.fetch_file_tree(self.show_hidden);

                                        // Check if we're in a different git repo and update git status
                                        if let Ok(repo) = Repository::discover(&dir) {
                                            if let Some(repo_root) = repo.workdir() {
                                                let new_repo_path = repo_root.to_path_buf();
                                                if new_repo_path != tab.repo_path {
                                                    // Different repo - update repo_path and refresh
                                                    tab.repo_path = new_repo_path;
                                                    tab.repo_name = tab.repo_path
                                                        .file_name()
                                                        .map(|n| n.to_string_lossy().to_string())
                                                        .unwrap_or_else(|| "repo".to_string());
                                                    tab.fetch_status();
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            Event::Tick => {
                // Poll git status only when viewing a diff (not the terminal)
                if let Some(tab) = self.active_tab_mut() {
                    let viewing_diff = tab.selected_file.is_some();
                    if viewing_diff && tab.last_poll.elapsed() >= Duration::from_millis(5000) {
                        tab.fetch_status();
                    }
                }

                // Update log server with terminal content
                self.update_log_server();

                // Periodically save workspace state
                self.save_workspaces();
            }
            Event::InitMenu => {
                // Initialize native macOS menu bar (must happen after NSApp exists)
                setup_menu_bar();
            }
            Event::CheckMenu => {
                // Poll for native menu events
                if let Ok(event) = MenuEvent::receiver().try_recv() {
                    if let Some(ids) = MENU_IDS.get() {
                        if event.id == ids.increase_terminal_font {
                            return self.update(Event::IncreaseTerminalFont);
                        } else if event.id == ids.decrease_terminal_font {
                            return self.update(Event::DecreaseTerminalFont);
                        } else if event.id == ids.increase_ui_font {
                            return self.update(Event::IncreaseUiFont);
                        } else if event.id == ids.decrease_ui_font {
                            return self.update(Event::DecreaseUiFont);
                        } else if event.id == ids.toggle_theme {
                            return self.update(Event::ToggleTheme);
                        } else if event.id == ids.clear_terminal {
                            return self.update(Event::ClearTerminal);
                        }
                    }
                }

                // Drain console output for all workspaces
                let mut auto_expand = false;
                for ws in &mut self.workspaces {
                    // Take rx out to avoid double-borrow
                    if let Some(mut rx) = ws.console.output_rx.take() {
                        let mut count = 0;
                        let mut exited_info = None;
                        let mut messages = Vec::new();
                        loop {
                            match rx.try_recv() {
                                Ok(msg) => messages.push(msg),
                                Err(_) => break,
                            }
                            count += 1;
                            if count >= 200 { break; }
                        }
                        for msg in messages {
                            match msg {
                                ConsoleOutputMessage::Stdout(line) => {
                                    ws.console.push_line(line, false);
                                }
                                ConsoleOutputMessage::Stderr(line) => {
                                    ws.console.push_line(line, true);
                                }
                                ConsoleOutputMessage::Exited(code) => {
                                    exited_info = Some(code);
                                }
                            }
                        }
                        if let Some(code) = exited_info {
                            ws.console.exit_code = code;
                            ws.console.stopped_at = Some(std::time::Instant::now());
                            if code.is_some() && code != Some(0) {
                                ws.console.status = ConsoleStatus::Error;
                                auto_expand = true;
                            } else {
                                ws.console.status = ConsoleStatus::Stopped;
                            }
                            ws.console.child_killer = None;
                            // Don't put rx back — process is done
                        } else {
                            // Put rx back
                            ws.console.output_rx = Some(rx);
                        }
                    }
                }
                if auto_expand {
                    self.console_expanded = true;
                }
            }
            Event::TabSelect(idx) => {
                // Hide WebView when switching tabs
                webview::set_visible(false);
                if let Some(ws) = self.active_workspace_mut() {
                    if idx < ws.tabs.len() {
                        ws.active_tab = idx;
                    }
                }
            }
            Event::TabClose(idx) => {
                // Hide WebView when closing tabs
                webview::set_visible(false);
                if let Some(ws) = self.active_workspace_mut() {
                    if idx < ws.tabs.len() && ws.tabs.len() > 1 {
                        ws.tabs.remove(idx);
                        if ws.active_tab >= ws.tabs.len() {
                            ws.active_tab = ws.tabs.len() - 1;
                        }
                    }
                }
                self.save_workspaces();
            }
            Event::OpenFolder => {
                return Task::perform(
                    async {
                        let folder = rfd::AsyncFileDialog::new()
                            .set_title("Select Folder")
                            .pick_folder()
                            .await;
                        folder.map(|f| f.path().to_path_buf())
                    },
                    Event::FolderSelected,
                );
            }
            Event::FolderSelected(Some(path)) => {
                // Allow any folder, not just git repos
                self.add_tab(path);
                self.save_workspaces();
            }
            Event::FolderSelected(None) => {}
            Event::FileSelect(path, is_staged) => {
                // Hide WebView when switching to git diff view
                webview::set_visible(false);

                if let Some(tab) = self.active_tab_mut() {
                    // Clear file viewer if open
                    tab.viewing_file_path = None;
                    tab.file_content.clear();
                    tab.image_handle = None;
                    tab.webview_content = None;
                    // Find the index of this file
                    let all_files = tab.all_files();
                    if let Some(idx) = all_files.iter().position(|f| f.path == path) {
                        tab.file_index = idx as i32;
                    }
                    tab.selected_file = Some(path.clone());
                    tab.selected_is_staged = is_staged;
                    tab.fetch_diff(&path, is_staged);
                }
            }
            Event::FileSelectByIndex(idx) => {
                // Hide WebView when switching to git diff view
                webview::set_visible(false);

                if let Some(tab) = self.active_tab_mut() {
                    // Clear file viewer if open
                    tab.viewing_file_path = None;
                    tab.file_content.clear();
                    tab.image_handle = None;
                    tab.webview_content = None;

                    let total = tab.total_changes() as i32;
                    if total == 0 {
                        return Task::none();
                    }

                    let new_idx = idx.clamp(0, total - 1);
                    tab.file_index = new_idx;

                    let all_files = tab.all_files();
                    if let Some(file) = all_files.get(new_idx as usize) {
                        let path = file.path.clone();
                        let is_staged = file.is_staged;
                        tab.selected_file = Some(path.clone());
                        tab.selected_is_staged = is_staged;
                        tab.fetch_diff(&path, is_staged);
                    }
                }
            }
            Event::ClearSelection => {
                if let Some(tab) = self.active_tab_mut() {
                    tab.selected_file = None;
                    tab.file_index = -1;
                    tab.diff_lines.clear();
                }
            }
            Event::KeyPressed(key, modifiers) => {
                // Escape cancels console command editing
                if self.editing_console_command.is_some() {
                    if let Key::Named(key::Named::Escape) = key.as_ref() {
                        return Task::done(Event::ConsoleCommandCancel);
                    }
                }

                // Console shortcuts (Cmd+J, Cmd+Shift+R) - before search shortcuts
                if modifiers.command() {
                    if let Key::Character(c) = key.as_ref() {
                        // Cmd+J - Toggle console panel
                        if c == "j" && !modifiers.shift() {
                            return Task::done(Event::ConsoleToggle);
                        }
                        // Cmd+Shift+R - Restart console process
                        if (c == "r" || c == "R") && modifiers.shift() {
                            return Task::done(Event::ConsoleRestart);
                        }
                    }
                }

                // Handle search shortcuts first (Cmd+F, Cmd+G, Escape when search active)
                if let Some(tab) = self.active_tab() {
                    // Search shortcuts
                    if modifiers.command() {
                        if let Key::Character(c) = key.as_ref() {
                            // Cmd+F - Toggle search
                            if c == "f" {
                                return Task::done(Event::ToggleSearch);
                            }
                            // Cmd+G / Cmd+Shift+G - Next/Prev match
                            if c == "g" && tab.search.is_active {
                                if modifiers.shift() {
                                    return Task::done(Event::SearchPrev);
                                } else {
                                    return Task::done(Event::SearchNext);
                                }
                            }
                            // Cmd+K - Clear terminal
                            if c == "k" {
                                return Task::done(Event::ClearTerminal);
                            }
                        }
                    }

                    // Escape - Close search if active
                    if tab.search.is_active {
                        if let Key::Named(key::Named::Escape) = key.as_ref() {
                            return Task::done(Event::SearchClose);
                        }
                    }

                    // Handle Escape in file viewer
                    if tab.viewing_file_path.is_some() {
                        if let Key::Named(key::Named::Escape) = key.as_ref() {
                            return Task::done(Event::CloseFileView);
                        }
                    }

                    if tab.selected_file.is_some() {
                        // In diff view - handle navigation
                        match key.as_ref() {
                            Key::Named(key::Named::Escape) => {
                                return Task::done(Event::ClearSelection);
                            }
                            Key::Character(c) if c == "j" => {
                                let new_idx = tab.file_index + 1;
                                return Task::done(Event::FileSelectByIndex(new_idx));
                            }
                            Key::Character(c) if c == "k" => {
                                let new_idx = tab.file_index - 1;
                                return Task::done(Event::FileSelectByIndex(new_idx));
                            }
                            Key::Character(c) if c == "g" => {
                                return Task::done(Event::FileSelectByIndex(0));
                            }
                            Key::Character(c) if c == "G" => {
                                let last = (tab.total_changes() as i32) - 1;
                                return Task::done(Event::FileSelectByIndex(last));
                            }
                            _ => {}
                        }
                    }
                }

                // Workspace switching with Ctrl+1-9
                if modifiers.control() && !modifiers.command() {
                    if let Key::Character(c) = key.as_ref() {
                        if let Ok(num) = c.parse::<usize>() {
                            if num >= 1 && num <= 9 && num <= self.workspaces.len() {
                                return Task::done(Event::WorkspaceSelect(num - 1));
                            }
                        }
                    }
                }

                // Tab switching with Cmd+1-9
                // Terminal font: Cmd+Plus/Minus, UI font: Cmd+Shift+Plus/Minus
                if modifiers.command() {
                    if let Key::Character(c) = key.as_ref() {
                        if c == "=" || c == "+" {
                            if modifiers.shift() {
                                return Task::done(Event::IncreaseUiFont);
                            } else {
                                return Task::done(Event::IncreaseTerminalFont);
                            }
                        } else if c == "-" || c == "_" {
                            if modifiers.shift() {
                                return Task::done(Event::DecreaseUiFont);
                            } else {
                                return Task::done(Event::DecreaseTerminalFont);
                            }
                        } else if let Ok(num) = c.parse::<usize>() {
                            let tab_count = self.active_workspace().map(|ws| ws.tabs.len()).unwrap_or(0);
                            if num >= 1 && num <= 9 && num <= tab_count {
                                return Task::done(Event::TabSelect(num - 1));
                            }
                        }
                    }
                }
            }
            Event::ToggleSidebarMode => {
                // Hide WebView when switching modes
                webview::set_visible(false);

                let show_hidden = self.show_hidden;
                if let Some(tab) = self.active_tab_mut() {
                    tab.sidebar_mode = match tab.sidebar_mode {
                        SidebarMode::Git => {
                            // Switching to Files mode - clear git selection
                            // Keep current_dir as-is (it tracks the terminal's directory)
                            tab.selected_file = None;
                            tab.diff_lines.clear();
                            tab.fetch_file_tree(show_hidden);
                            SidebarMode::Files
                        }
                        SidebarMode::Files => {
                            // Switching to Git mode - clear file viewer and refresh status
                            tab.viewing_file_path = None;
                            tab.file_content.clear();
                            tab.image_handle = None;
                            tab.webview_content = None;
                            tab.fetch_status();
                            SidebarMode::Git
                        }
                    };
                }
            }
            Event::NavigateDir(path) => {
                let show_hidden = self.show_hidden;
                if let Some(tab) = self.active_tab_mut() {
                    tab.current_dir = path;
                    tab.fetch_file_tree(show_hidden);
                }
            }
            Event::NavigateUp => {
                let show_hidden = self.show_hidden;
                if let Some(tab) = self.active_tab_mut() {
                    if let Some(parent) = tab.current_dir.parent() {
                        // Don't go above repo root
                        if parent.starts_with(&tab.repo_path) || parent == tab.repo_path {
                            tab.current_dir = parent.to_path_buf();
                            tab.fetch_file_tree(show_hidden);
                        }
                    }
                }
            }
            Event::ToggleHidden => {
                self.show_hidden = !self.show_hidden;
                self.save_config();
                let show_hidden = self.show_hidden;
                if let Some(tab) = self.active_tab_mut() {
                    if tab.sidebar_mode == SidebarMode::Files {
                        tab.fetch_file_tree(show_hidden);
                    }
                }
            }
            Event::DividerDragStart => {
                self.dragging_divider = true;
            }
            Event::DividerDragEnd => {
                if self.dragging_divider {
                    self.dragging_divider = false;
                    self.save_config();
                }
                if self.dragging_console_divider {
                    self.dragging_console_divider = false;
                    self.save_config();
                }
            }
            Event::MouseMoved(x, y) => {
                if self.dragging_divider {
                    // Clamp sidebar width between 150 and 600 pixels (subtract rail width)
                    self.sidebar_width = (x - SPINE_WIDTH).clamp(150.0, 600.0);

                    // Update WebView bounds if active
                    if webview::is_active() {
                        let bounds = self.calculate_webview_bounds();
                        webview::update_bounds(bounds.0, bounds.1, bounds.2, bounds.3);
                    }
                }
                if self.dragging_console_divider {
                    // Console height = distance from bottom of window to mouse position
                    let new_height = (self.window_size.1 - y).clamp(32.0, self.window_size.1 - 140.0);
                    self.console_height = new_height;

                    // Update WebView bounds if active
                    if webview::is_active() {
                        let bounds = self.calculate_webview_bounds();
                        webview::update_bounds(bounds.0, bounds.1, bounds.2, bounds.3);
                    }
                }

                // Edge peek detection — check if cursor is near left/right edge of content area
                if !self.dragging_divider && !self.dragging_console_divider {
                    let content_x = x - SPINE_WIDTH; // x relative to content area
                    let content_width = self.content_viewport_width();
                    let has_left = self.active_workspace_idx > 0;
                    let has_right = self.active_workspace_idx + 1 < self.workspaces.len();

                    let near_left = has_left && content_x >= 0.0 && content_x < EDGE_PEEK_ZONE;
                    let near_right = has_right && content_x > content_width - EDGE_PEEK_ZONE && content_x <= content_width;

                    if near_left != self.edge_peek_left || near_right != self.edge_peek_right {
                        self.edge_peek_left = near_left;
                        self.edge_peek_right = near_right;
                    }
                }
            }
            Event::ViewFile(path) => {
                let is_dark_theme = self.theme == AppTheme::Dark;
                let is_markdown = TabState::is_markdown_file(&path);

                // Hide WebView if switching to non-markdown
                if !is_markdown && webview::is_active() {
                    webview::set_visible(false);
                }

                if let Some(tab) = self.active_tab_mut() {
                    // Clear git selection if any
                    tab.selected_file = None;
                    tab.diff_lines.clear();
                    tab.load_file(&path, is_dark_theme);
                }

                // Create/update WebView for markdown files
                if is_markdown {
                    if let Some(tab) = self.active_tab() {
                        if let Some(html) = tab.webview_content.clone() {
                            let bounds = self.calculate_webview_bounds();
                            // Store the content, then get window access to create WebView
                            webview::set_pending_content(html, bounds);
                            return iced::window::oldest().then(move |opt_id| {
                                if let Some(id) = opt_id {
                                    iced::window::run(id, move |window| {
                                        if let Err(e) = webview::try_create_with_window(window) {
                                            eprintln!("WebView error: {}", e);
                                        }
                                    })
                                    .discard()
                                } else {
                                    Task::none()
                                }
                            });
                        }
                    }
                }
            }
            Event::CloseFileView => {
                // Hide WebView
                webview::set_visible(false);

                if let Some(tab) = self.active_tab_mut() {
                    tab.viewing_file_path = None;
                    tab.file_content.clear();
                    tab.image_handle = None;
                    tab.webview_content = None;
                }
            }
            Event::CopyFileContent => {
                if let Some(tab) = self.active_tab() {
                    if !tab.file_content.is_empty() {
                        let content = tab.file_content.join("\n");
                        return iced::clipboard::write(content);
                    }
                }
            }
            Event::OpenFileInBrowser => {
                if let Some(tab) = self.active_tab() {
                    if tab.viewing_file_path.is_some() && !tab.file_content.is_empty() {
                        if let Some(base_url) = self.log_server_state.base_url() {
                            let url = format!("{}/file/{}", base_url, tab.id);
                            let _ = std::process::Command::new("open")
                                .arg(&url)
                                .spawn();
                        }
                    }
                }
            }
            Event::ToggleTheme => {
                self.theme = self.theme.toggle();
                self.save_config();
                self.recreate_terminals();

                // Re-render markdown if viewing one
                let is_dark = self.theme == AppTheme::Dark;
                if let Some(tab) = self.active_tab_mut() {
                    if let Some(path) = &tab.viewing_file_path.clone() {
                        if TabState::is_markdown_file(path) {
                            tab.load_file(path, is_dark);
                            // Update WebView content
                            if let Some(html) = &tab.webview_content {
                                webview::update_content(html);
                            }
                        }
                    }
                }
            }
            Event::IncreaseTerminalFont => {
                let new_size = (self.terminal_font_size + FONT_SIZE_STEP).min(MAX_FONT_SIZE);
                if new_size != self.terminal_font_size {
                    self.terminal_font_size = new_size;
                    self.save_config();
                    self.recreate_terminals();
                }
            }
            Event::DecreaseTerminalFont => {
                let new_size = (self.terminal_font_size - FONT_SIZE_STEP).max(MIN_FONT_SIZE);
                if new_size != self.terminal_font_size {
                    self.terminal_font_size = new_size;
                    self.save_config();
                    self.recreate_terminals();
                }
            }
            Event::ClearTerminal => {
                if let Some(tab) = self.active_tab_mut() {
                    if let Some(term) = &mut tab.terminal {
                        // Send the clear command to the terminal
                        term.handle(iced_term::Command::ProxyToBackend(
                            iced_term::backend::Command::Write(b"clear\n".to_vec())
                        ));
                    }
                }
            }
            Event::IncreaseUiFont => {
                let new_size = (self.ui_font_size + FONT_SIZE_STEP).min(MAX_FONT_SIZE);
                if new_size != self.ui_font_size {
                    self.ui_font_size = new_size;
                    self.save_config();
                }
            }
            Event::DecreaseUiFont => {
                let new_size = (self.ui_font_size - FONT_SIZE_STEP).max(MIN_FONT_SIZE);
                if new_size != self.ui_font_size {
                    self.ui_font_size = new_size;
                    self.save_config();
                }
            }
            // Search events
            Event::ToggleSearch => {
                if let Some(tab) = self.active_tab_mut() {
                    tab.search.is_active = !tab.search.is_active;
                    if !tab.search.is_active {
                        // Clear search state when closing
                        tab.search.query.clear();
                        tab.search.matches.clear();
                        tab.search.current_match = 0;
                    }
                }
            }
            Event::SearchQueryChanged(query) => {
                if let Some(tab) = self.active_tab_mut() {
                    tab.search.query = query;
                }
            }
            Event::SearchExecute => {
                if let Some(tab) = self.active_tab_mut() {
                    if let Some(term) = &mut tab.terminal {
                        let matches = term.search_all(&tab.search.query);
                        tab.search.matches = matches;
                        tab.search.current_match = 0;

                        // Scroll to first match if found
                        if let Some(first_match) = tab.search.matches.first() {
                            let line = first_match.start.line.0;
                            term.scroll_to_line(line);
                        }
                    }
                }
            }
            Event::SearchNext => {
                if let Some(tab) = self.active_tab_mut() {
                    if !tab.search.matches.is_empty() {
                        tab.search.current_match = (tab.search.current_match + 1) % tab.search.matches.len();
                        if let Some(term) = &mut tab.terminal {
                            let current = &tab.search.matches[tab.search.current_match];
                            term.scroll_to_line(current.start.line.0);
                        }
                    }
                }
            }
            Event::SearchPrev => {
                if let Some(tab) = self.active_tab_mut() {
                    if !tab.search.matches.is_empty() {
                        if tab.search.current_match == 0 {
                            tab.search.current_match = tab.search.matches.len() - 1;
                        } else {
                            tab.search.current_match -= 1;
                        }
                        if let Some(term) = &mut tab.terminal {
                            let current = &tab.search.matches[tab.search.current_match];
                            term.scroll_to_line(current.start.line.0);
                        }
                    }
                }
            }
            Event::SearchClose => {
                if let Some(tab) = self.active_tab_mut() {
                    tab.search.is_active = false;
                    tab.search.query.clear();
                    tab.search.matches.clear();
                    tab.search.current_match = 0;
                }
            }
            Event::OpenMarkdownInBrowser => {
                // Write HTML to temp file and open in browser
                if let Some(tab) = self.active_tab() {
                    if let Some(html) = &tab.webview_content {
                        let temp_dir = std::env::temp_dir();
                        let file_name = tab
                            .viewing_file_path
                            .as_ref()
                            .and_then(|p| p.file_stem())
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_else(|| "preview".to_string());
                        let temp_path = temp_dir.join(format!("{}_preview.html", file_name));

                        if std::fs::write(&temp_path, html).is_ok() {
                            // Open in default browser
                            #[cfg(target_os = "macos")]
                            {
                                let _ = std::process::Command::new("open")
                                    .arg(&temp_path)
                                    .spawn();
                            }
                            #[cfg(target_os = "linux")]
                            {
                                let _ = std::process::Command::new("xdg-open")
                                    .arg(&temp_path)
                                    .spawn();
                            }
                            #[cfg(target_os = "windows")]
                            {
                                let _ = std::process::Command::new("cmd")
                                    .args(["/C", "start", ""])
                                    .arg(&temp_path)
                                    .spawn();
                            }
                        }
                    }
                }
            }
            Event::WindowCloseRequested => {
                // Kill all console processes
                for ws in &mut self.workspaces {
                    ws.console.kill_process();
                }
                // Signal the log server to shut down
                self.log_server_state.shutdown.notify_one();
                // Close the window
                return iced::window::oldest().then(|opt_id| {
                    if let Some(id) = opt_id {
                        iced::window::close(id)
                    } else {
                        iced::exit()
                    }
                });
            }
            Event::WindowResized(width, height) => {
                self.window_size = (width, height);
                // Clamp console height to new window bounds
                self.console_height = self.console_height.clamp(32.0, (height - 140.0).max(32.0));

                // Recalculate slide position for new viewport width (snap, no animation)
                let viewport_width = self.content_viewport_width();
                let new_target = self.active_workspace_idx as f32 * viewport_width;
                self.slide_offset = new_target;
                self.slide_target = new_target;
                self.slide_animating = false;
                self.slide_start_time = None;

                let scroll_task = iced::advanced::widget::operate(
                    iced::advanced::widget::operation::scrollable::scroll_to(
                        workspace_scrollable_id().into(),
                        scrollable::AbsoluteOffset { x: Some(new_target), y: None },
                    ),
                );

                // Update WebView bounds if active
                if webview::is_active() {
                    let bounds = self.calculate_webview_bounds();
                    webview::update_bounds(bounds.0, bounds.1, bounds.2, bounds.3);
                }

                return scroll_task;
            }
            Event::WorkspaceSelect(idx) => {
                webview::set_visible(false);
                self.editing_console_command = None;
                if idx < self.workspaces.len() && idx != self.active_workspace_idx {
                    let viewport_width = self.content_viewport_width();
                    let target = idx as f32 * viewport_width;

                    // Start animation from current position
                    self.slide_start_offset = self.slide_offset;
                    self.slide_target = target;
                    self.slide_start_time = Some(Instant::now());
                    self.slide_animating = true;

                    // Update active workspace immediately (tab bar + console switch instantly)
                    self.active_workspace_idx = idx;
                    self.save_workspaces();

                    // Set scrollable to starting position for the animation
                    return iced::advanced::widget::operate(
                        iced::advanced::widget::operation::scrollable::scroll_to(
                            workspace_scrollable_id().into(),
                            scrollable::AbsoluteOffset { x: Some(self.slide_start_offset), y: None },
                        ),
                    );
                }
            }
            Event::SlideAnimationTick => {
                // After user stops swiping, snap to nearest workspace
                if !self.slide_animating {
                    if let Some(last_scroll) = self.last_user_scroll {
                        if last_scroll.elapsed().as_millis() >= SWIPE_DEBOUNCE_MS as u128 {
                            self.last_user_scroll = None;
                            let viewport_width = self.content_viewport_width();
                            let target = self.active_workspace_idx as f32 * viewport_width;
                            if (self.slide_offset - target).abs() > 1.0 {
                                self.slide_start_offset = self.slide_offset;
                                self.slide_target = target;
                                self.slide_start_time = Some(Instant::now());
                                self.slide_animating = true;
                            }
                        }
                    }
                    return Task::none();
                }

                // Animate slide with ease-out cubic
                if let Some(start_time) = self.slide_start_time {
                    let elapsed = start_time.elapsed().as_millis() as f32;
                    let t = (elapsed / SLIDE_DURATION_MS).min(1.0);
                    let eased = 1.0 - (1.0 - t).powi(3);
                    self.slide_offset =
                        self.slide_start_offset + (self.slide_target - self.slide_start_offset) * eased;

                    if t >= 1.0 {
                        self.slide_offset = self.slide_target;
                        self.slide_animating = false;
                        self.slide_start_time = None;
                    }

                    let offset_x = self.slide_offset;
                    return iced::advanced::widget::operate(
                        iced::advanced::widget::operation::scrollable::scroll_to(
                            workspace_scrollable_id().into(),
                            scrollable::AbsoluteOffset { x: Some(offset_x), y: None },
                        ),
                    );
                }
            }
            Event::EdgePeekEnter(is_right) => {
                if is_right {
                    self.edge_peek_right = true;
                } else {
                    self.edge_peek_left = true;
                }
            }
            Event::EdgePeekExit => {
                self.edge_peek_left = false;
                self.edge_peek_right = false;
            }
            Event::SlideScrolled(viewport) => {
                // User swiped — track position, debounce snap until they stop
                if !self.slide_animating {
                    let viewport_width = self.content_viewport_width();
                    if viewport_width > 0.0 {
                        let offset = viewport.absolute_offset().x;
                        self.slide_offset = offset;
                        self.last_user_scroll = Some(Instant::now());

                        // Update active workspace based on current scroll position
                        let nearest = ((offset + viewport_width * 0.5) / viewport_width) as usize;
                        let nearest = nearest.min(self.workspaces.len().saturating_sub(1));
                        if nearest != self.active_workspace_idx {
                            self.active_workspace_idx = nearest;
                            self.save_workspaces();
                            webview::set_visible(false);
                            self.editing_console_command = None;
                        }
                    }
                }
            }
            Event::WorkspaceClose(idx) => {
                webview::set_visible(false);
                if idx < self.workspaces.len() && self.workspaces.len() > 1 {
                    // Kill console process before removing workspace
                    self.workspaces[idx].console.kill_process();
                    self.workspaces.remove(idx);
                    if self.active_workspace_idx >= self.workspaces.len() {
                        self.active_workspace_idx = self.workspaces.len() - 1;
                    }
                    self.save_workspaces();

                    // Snap slide to new active workspace (no animation)
                    let viewport_width = self.content_viewport_width();
                    let new_target = self.active_workspace_idx as f32 * viewport_width;
                    self.slide_offset = new_target;
                    self.slide_target = new_target;
                    self.slide_animating = false;
                    self.slide_start_time = None;

                    return iced::advanced::widget::operate(
                        iced::advanced::widget::operation::scrollable::scroll_to(
                            workspace_scrollable_id().into(),
                            scrollable::AbsoluteOffset { x: Some(new_target), y: None },
                        ),
                    );
                }
            }
            Event::WorkspaceCreate => {
                return Task::perform(
                    async {
                        let folder = rfd::AsyncFileDialog::new()
                            .set_title("Select Workspace Folder")
                            .pick_folder()
                            .await;
                        folder.map(|f| f.path().to_path_buf())
                    },
                    Event::WorkspaceCreated,
                );
            }
            Event::WorkspaceCreated(Some(path)) => {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "Workspace".to_string());
                let color = WorkspaceColor::from_index(self.workspaces.len());
                let mut workspace = Workspace::new(name, path.clone(), color);
                self.add_tab_to_workspace(&mut workspace, path);
                self.workspaces.push(workspace);
                self.active_workspace_idx = self.workspaces.len() - 1;
                self.save_workspaces();

                // Snap slide state to new workspace position
                // (no scroll_to needed — view renders active workspace directly when not animating)
                let viewport_width = self.content_viewport_width();
                let new_target = self.active_workspace_idx as f32 * viewport_width;
                self.slide_offset = new_target;
                self.slide_target = new_target;
                self.slide_animating = false;
                self.slide_start_time = None;
            }
            Event::WorkspaceCreated(None) => {}
            // Console panel events
            Event::ConsoleToggle => {
                self.console_expanded = !self.console_expanded;
                self.save_config();
                // Update WebView bounds if active
                if webview::is_active() {
                    let bounds = self.calculate_webview_bounds();
                    webview::update_bounds(bounds.0, bounds.1, bounds.2, bounds.3);
                }
            }
            Event::ConsoleStart => {
                if let Some(ws) = self.active_workspace_mut() {
                    // Use active tab's directory (tracks terminal cwd), fall back to workspace root
                    let dir = ws.active_tab()
                        .map(|t| t.current_dir.clone())
                        .unwrap_or_else(|| ws.dir.clone());
                    ws.console.spawn_process(&dir);
                }
                self.console_expanded = true;
            }
            Event::ConsoleStop => {
                if let Some(ws) = self.active_workspace_mut() {
                    ws.console.kill_process();
                    ws.console.status = ConsoleStatus::Stopped;
                }
            }
            Event::ConsoleRestart => {
                if let Some(ws) = self.active_workspace_mut() {
                    ws.console.kill_process();
                    let dir = ws.active_tab()
                        .map(|t| t.current_dir.clone())
                        .unwrap_or_else(|| ws.dir.clone());
                    ws.console.spawn_process(&dir);
                }
                self.console_expanded = true;
            }
            Event::ConsoleClearOutput => {
                if let Some(ws) = self.active_workspace_mut() {
                    ws.console.clear_output();
                }
            }
            Event::ConsoleDividerDragStart => {
                self.dragging_console_divider = true;
            }
            Event::ConsoleCommandEditStart => {
                let current = self.active_workspace()
                    .and_then(|ws| ws.console.run_command.clone())
                    .unwrap_or_default();
                self.editing_console_command = Some(current);
            }
            Event::ConsoleCommandChanged(val) => {
                self.editing_console_command = Some(val);
            }
            Event::ConsoleCommandSubmit => {
                if let Some(cmd) = self.editing_console_command.take() {
                    if let Some(ws) = self.active_workspace_mut() {
                        if cmd.trim().is_empty() {
                            ws.console.run_command = None;
                            ws.console.status = ConsoleStatus::NoneConfigured;
                        } else {
                            ws.console.run_command = Some(cmd.trim().to_string());
                            if !ws.console.is_running() {
                                ws.console.status = ConsoleStatus::Stopped;
                            }
                        }
                    }
                    self.save_workspaces();
                }
            }
            Event::ConsoleCommandCancel => {
                self.editing_console_command = None;
            }
        }
        Task::none()
    }

    /// Calculate WebView bounds based on current layout
    fn calculate_webview_bounds(&self) -> (f32, f32, f32, f32) {
        let tab_bar_height = 40.0;
        let header_height = 45.0;
        let x = SPINE_WIDTH + self.sidebar_width + 4.0; // rail + sidebar + divider
        let y = tab_bar_height + header_height;
        let width = (self.window_size.0 - x).max(100.0);
        // Subtract console panel height
        let console_h = if self.console_expanded {
            self.console_height + CONSOLE_DIVIDER_HEIGHT
        } else {
            CONSOLE_HEADER_HEIGHT
        };
        let height = (self.window_size.1 - y - console_h).max(100.0);
        (x, y, width, height)
    }

    fn recreate_terminals(&mut self) {
        for tab in self.workspaces.iter_mut().flat_map(|ws| ws.tabs.iter_mut()) {
            // Get shell - platform-specific defaults
            #[cfg(target_os = "windows")]
            let shell = std::env::var("COMSPEC").unwrap_or_else(|_| "powershell.exe".to_string());

            #[cfg(not(target_os = "windows"))]
            let shell = std::env::var("SHELL").ok().or_else(|| {
                let user = std::env::var("USER").ok()?;
                let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
                for line in passwd.lines() {
                    let parts: Vec<&str> = line.split(':').collect();
                    if parts.first() == Some(&user.as_str()) {
                        return parts.get(6).map(|s| s.to_string());
                    }
                }
                None
            }).unwrap_or_else(|| "/bin/zsh".to_string());

            // Build environment for PTY
            let mut env = std::collections::HashMap::new();

            #[cfg(not(target_os = "windows"))]
            {
                env.insert("TERM".to_string(), "xterm-256color".to_string());
                env.insert("COLORTERM".to_string(), "truecolor".to_string());
                env.insert("LANG".to_string(), std::env::var("LANG").unwrap_or_else(|_| "en_US.UTF-8".to_string()));
                if let Ok(home) = std::env::var("HOME") {
                    env.insert("HOME".to_string(), home.clone());
                    env.insert("PATH".to_string(), format!("{}/.local/bin:{}/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin", home, home));
                }
                if let Ok(user) = std::env::var("USER") {
                    env.insert("USER".to_string(), user.clone());
                    env.insert("LOGNAME".to_string(), user);
                }
                env.insert("SHELL".to_string(), shell.clone());
            }

            #[cfg(target_os = "windows")]
            {
                // On Windows, inherit most environment variables from parent process
                for (key, value) in std::env::vars() {
                    env.insert(key, value);
                }
            }

            // Add precmd hook to set terminal title to current directory
            env.insert("GITTERM_PRECMD".to_string(), "1".to_string());

            // Determine shell type for the right initialization
            let is_zsh = shell.contains("zsh");
            let is_bash = shell.contains("bash");
            let is_windows = cfg!(target_os = "windows");

            // Build args to inject precmd hook
            let args = if is_windows {
                // Windows shells don't use login flag
                vec![]
            } else if is_zsh {
                // For zsh: use ZDOTDIR to inject our precmd before user's config
                let home = std::env::var("HOME").unwrap_or_default();
                let gitterm_dir = format!("{home}/.config/gitterm/zsh");
                let gitterm_zshrc = format!("{gitterm_dir}/.zshrc");

                let _ = std::fs::create_dir_all(&gitterm_dir);
                let zshrc_content = format!(
                    r#"# GitTerm shell integration - sets terminal title on directory change
_gitterm_set_title() {{ print -Pn "\e]0;%~\a" }}
autoload -Uz add-zsh-hook
add-zsh-hook precmd _gitterm_set_title
add-zsh-hook chpwd _gitterm_set_title
_gitterm_set_title
[[ -f "{home}/.zshenv" ]] && source "{home}/.zshenv"
[[ -f "{home}/.zprofile" ]] && source "{home}/.zprofile"
[[ -f "{home}/.zshrc" ]] && source "{home}/.zshrc"
"#
                );
                let _ = std::fs::write(&gitterm_zshrc, zshrc_content);

                env.insert("ZDOTDIR".to_string(), gitterm_dir);
                vec!["-l".to_string()]
            } else if is_bash {
                env.insert("PROMPT_COMMAND".to_string(), r#"printf "\e]0;%s\a" "$PWD""#.to_string());
                vec!["-l".to_string()]
            } else {
                vec!["-l".to_string()]
            };

            let term_settings = iced_term::settings::Settings {
                backend: iced_term::settings::BackendSettings {
                    program: shell,
                    args,
                    working_directory: Some(tab.repo_path.clone()),
                    scrollback_lines: self.scrollback_lines,
                    env,
                    ..Default::default()
                },
                theme: iced_term::settings::ThemeSettings::new(Box::new(
                    self.theme.terminal_palette(),
                )),
                font: iced_term::settings::FontSettings {
                    size: self.terminal_font_size,
                    ..Default::default()
                },
            };
            if let Ok(terminal) = iced_term::Terminal::new(tab.id as u64, term_settings) {
                tab.terminal = Some(terminal);
                tab.created_at = Instant::now();
            }
        }
    }

    fn view(&self) -> Element<'_, Event, Theme, iced::Renderer> {
        let spine = self.view_spine();
        let tab_bar = self.view_tab_bar();
        let content = self.view_workspace_slide();
        let console_panel = self.view_console_panel();

        let mut main_col = Column::new().spacing(0).width(Length::Fill).height(Length::Fill);
        main_col = main_col.push(tab_bar);
        main_col = main_col.push(content);

        // Console divider (only when expanded)
        if self.console_expanded {
            let theme = &self.theme;
            let divider_color = if self.dragging_console_divider {
                theme.accent()
            } else {
                theme.surface0()
            };
            let console_divider = iced::widget::mouse_area(
                container(iced::widget::Space::new())
                    .width(Length::Fill)
                    .height(Length::Fixed(CONSOLE_DIVIDER_HEIGHT))
                    .style(move |_| container::Style {
                        background: Some(divider_color.into()),
                        ..Default::default()
                    }),
            )
            .on_press(Event::ConsoleDividerDragStart)
            .interaction(iced::mouse::Interaction::ResizingVertically);
            main_col = main_col.push(console_divider);
        }

        main_col = main_col.push(console_panel);

        // Bottom workspace bar
        let workspace_bar = self.view_workspace_bar();
        main_col = main_col.push(workspace_bar);

        row![spine, main_col]
            .spacing(0)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn view_workspace_bar(&self) -> Element<'_, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        let mut bar_row = Row::new().spacing(0).align_y(iced::Alignment::Center);

        for (idx, ws) in self.workspaces.iter().enumerate() {
            let is_active = idx == self.active_workspace_idx;
            let ws_color = ws.color.color(theme);
            let text_color = if is_active { ws_color } else { theme.overlay0() };
            let active_bg = theme.bg_base();
            let hover_bg = theme.surface0();

            // Colored dot before name
            let dot_color = if is_active { ws_color } else { theme.surface2() };
            let dot = container(iced::widget::Space::new().width(0).height(0))
                .width(Length::Fixed(6.0))
                .height(Length::Fixed(6.0))
                .style(move |_| container::Style {
                    background: Some(dot_color.into()),
                    border: iced::Border {
                        radius: 3.0.into(),
                        ..Default::default()
                    },
                    ..Default::default()
                });

            let label = text(&ws.name)
                .size(11)
                .color(text_color)
                .font(iced::Font::with_name("Menlo"));

            let btn_content = row![dot, label]
                .spacing(6)
                .align_y(iced::Alignment::Center);

            // Active workspace: colored top accent line above the button
            if is_active {
                let accent_line = container(iced::widget::Space::new().width(0).height(0))
                    .width(Length::Fill)
                    .height(Length::Fixed(2.0))
                    .style(move |_| container::Style {
                        background: Some(ws_color.into()),
                        ..Default::default()
                    });

                let ws_btn = button(btn_content)
                    .style(move |_theme, _status| button::Style {
                        background: Some(active_bg.into()),
                        text_color: iced::Color::WHITE,
                        border: iced::Border::default(),
                        ..Default::default()
                    })
                    .padding([4, 12])
                    .on_press(Event::WorkspaceSelect(idx));

                let stacked = column![accent_line, ws_btn].spacing(0);
                bar_row = bar_row.push(stacked);
            } else {
                let ws_btn = button(btn_content)
                    .style(move |_theme, status| {
                        let bg = if matches!(status, button::Status::Hovered) {
                            hover_bg
                        } else {
                            iced::Color::TRANSPARENT
                        };
                        button::Style {
                            background: Some(bg.into()),
                            text_color: iced::Color::WHITE,
                            border: iced::Border::default(),
                            ..Default::default()
                        }
                    })
                    .padding([6, 12])
                    .on_press(Event::WorkspaceSelect(idx));

                bar_row = bar_row.push(ws_btn);
            }

            // Separator between workspaces
            if idx < self.workspaces.len() - 1 {
                let sep_color = theme.surface0();
                bar_row = bar_row.push(
                    container(iced::widget::Space::new().width(0).height(0))
                        .width(Length::Fixed(1.0))
                        .height(Length::Fixed(14.0))
                        .style(move |_| container::Style {
                            background: Some(sep_color.into()),
                            ..Default::default()
                        }),
                );
            }
        }

        // "+ workspace" button at the end
        let ws_add_color = theme.overlay0();
        let ws_add_hover = theme.overlay1();
        let ws_add_btn = button(
            text("+ workspace").size(11).color(ws_add_color).font(iced::Font::with_name("Menlo")),
        )
        .style(move |_theme, status| {
            let tc = if matches!(status, button::Status::Hovered) {
                ws_add_hover
            } else {
                ws_add_color
            };
            button::Style {
                background: Some(iced::Color::TRANSPARENT.into()),
                text_color: tc,
                ..Default::default()
            }
        })
        .padding([6, 12])
        .on_press(Event::WorkspaceCreate);
        bar_row = bar_row.push(ws_add_btn);

        bar_row = bar_row.push(iced::widget::Space::new().width(Length::Fill));

        let bg = theme.bg_crust();
        let top_border_color = theme.surface0();

        let top_border = container(iced::widget::Space::new().height(0))
            .width(Length::Fill)
            .height(Length::Fixed(1.0))
            .style(move |_| container::Style {
                background: Some(top_border_color.into()),
                ..Default::default()
            });

        let bar_container = container(
            bar_row.padding([0, 4]),
        )
        .width(Length::Fill)
        .style(move |_| container::Style {
            background: Some(bg.into()),
            ..Default::default()
        });

        column![top_border, bar_container].into()
    }

    fn view_spine(&self) -> Element<'_, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        let mut dots = Column::new().spacing(8).align_x(iced::Alignment::Center);

        for (idx, ws) in self.workspaces.iter().enumerate() {
            let is_active = idx == self.active_workspace_idx;
            let ws_color = ws.color.color(theme);
            let inactive_color = theme.surface2();

            let (dot_w, dot_h) = if is_active { (4.0, 18.0) } else { (4.0, 4.0) };
            let dot_color = if is_active { ws_color } else { inactive_color };

            let dot = container(iced::widget::Space::new().width(0).height(0))
                .width(Length::Fixed(dot_w))
                .height(Length::Fixed(dot_h))
                .style(move |_| container::Style {
                    background: Some(dot_color.into()),
                    border: iced::Border {
                        radius: 2.0.into(),
                        ..Default::default()
                    },
                    ..Default::default()
                });

            let hover_bg = theme.surface0();
            let dot_btn = button(
                container(dot)
                    .width(Length::Fixed(SPINE_WIDTH - 1.0))
                    .center_x(Length::Fixed(SPINE_WIDTH - 1.0))
                    .center_y(Length::Shrink),
            )
            .style(move |_theme, status| {
                let bg = if matches!(status, button::Status::Hovered) {
                    hover_bg
                } else {
                    iced::Color::TRANSPARENT
                };
                button::Style {
                    background: Some(bg.into()),
                    border: iced::Border::default(),
                    text_color: iced::Color::WHITE,
                    ..Default::default()
                }
            })
            .padding([4, 0])
            .on_press(Event::WorkspaceSelect(idx));

            dots = dots.push(dot_btn);
        }

        let bg = theme.bg_crust();
        let border_color = theme.surface0();

        let spine_content = container(
            container(dots)
                .height(Length::Fill)
                .center_y(Length::Fill),
        )
        .width(Length::Fixed(SPINE_WIDTH))
        .height(Length::Fill)
        .style(move |_| container::Style {
            background: Some(bg.into()),
            ..Default::default()
        });

        // Right border as a separate 1px column
        let border_line = container(iced::widget::Space::new().width(0).height(0))
            .width(Length::Fixed(1.0))
            .height(Length::Fill)
            .style(move |_| container::Style {
                background: Some(border_color.into()),
                ..Default::default()
            });

        row![spine_content, border_line].into()
    }

    fn view_tab_bar(&self) -> Element<'_, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        let mut tabs_row = Row::new().spacing(2);

        // Left edge peek indicator
        if self.edge_peek_left {
            if let Some(left_ws) = self.workspaces.get(self.active_workspace_idx.wrapping_sub(1)) {
                let ws_color = left_ws.color.color(theme);
                let peek_btn = button(
                    text(format!("\u{2039} {}", left_ws.name))
                        .size(11)
                        .color(ws_color)
                        .font(iced::Font::with_name("Menlo")),
                )
                .style(move |_theme, _status| button::Style {
                    background: Some(iced::Color::TRANSPARENT.into()),
                    text_color: ws_color,
                    ..Default::default()
                })
                .padding([4, 6])
                .on_press(Event::WorkspaceSelect(self.active_workspace_idx - 1));
                tabs_row = tabs_row.push(peek_btn);
            }
        }

        let (tabs, active_tab_idx) = if let Some(ws) = self.active_workspace() {
            (ws.tabs.as_slice(), ws.active_tab)
        } else {
            (&[] as &[TabState], 0)
        };

        for (idx, tab) in tabs.iter().enumerate() {
            let is_active = idx == active_tab_idx;

            // Determine if this is a Claude Code tab
            let is_claude = tab
                .terminal_title
                .as_ref()
                .map(|t| t.to_lowercase().contains("claude"))
                .unwrap_or(false);

            // Icon prefix
            let icon_str = if is_claude { "✦ " } else { "▶ " };
            let icon_color = if is_claude {
                theme.peach()
            } else {
                theme.success()
            };

            // Tab label - truncate at 20 chars
            let base_title = tab
                .terminal_title
                .as_ref()
                .map(|t| {
                    if t.len() > 20 {
                        format!("{}…", &t[..19])
                    } else {
                        t.clone()
                    }
                })
                .unwrap_or_else(|| tab.repo_name.clone());

            let text_color = if is_active {
                theme.text_primary()
            } else {
                theme.overlay1()
            };
            let active_bg = theme.bg_base();
            let hover_bg = theme.surface0();

            // Build tab content: icon + label + shortcut
            let mut tab_content = Row::new().spacing(0).align_y(iced::Alignment::Center);
            tab_content = tab_content.push(text(icon_str).size(12).color(icon_color));
            tab_content = tab_content.push(
                text(base_title)
                    .size(13)
                    .color(text_color)
                    .font(iced::Font::with_name("Menlo")),
            );

            if idx < 9 {
                tab_content = tab_content.push(
                    text(format!(" \u{2318}{}", idx + 1))
                        .size(10)
                        .color(theme.surface2())
                        .font(iced::Font::with_name("Menlo")),
                );
            }

            let tab_btn = button(tab_content)
                .style(move |_theme, status| {
                    let bg = if is_active {
                        Some(active_bg.into())
                    } else if matches!(status, button::Status::Hovered) {
                        Some(hover_bg.into())
                    } else {
                        Some(iced::Color::TRANSPARENT.into())
                    };
                    button::Style {
                        background: bg,
                        border: iced::Border {
                            radius: 6.0.into(),
                            ..Default::default()
                        },
                        text_color: iced::Color::WHITE,
                        ..Default::default()
                    }
                })
                .padding([4, 10])
                .on_press(Event::TabSelect(idx));

            // Close button
            let close_color = theme.overlay0();
            let close_hover = theme.text_primary();
            let close_btn = button(text("\u{00d7}").size(14).color(close_color))
                .style(move |_theme, status| {
                    let tc = if matches!(status, button::Status::Hovered) {
                        close_hover
                    } else {
                        close_color
                    };
                    button::Style {
                        background: Some(iced::Color::TRANSPARENT.into()),
                        text_color: tc,
                        ..Default::default()
                    }
                })
                .padding([4, 4])
                .on_press(Event::TabClose(idx));

            tabs_row = tabs_row
                .push(row![tab_btn, close_btn].spacing(0).align_y(iced::Alignment::Center));
        }

        // Add tab button
        let add_color = theme.overlay0();
        let add_hover = theme.text_primary();
        let add_btn = button(text("+").size(14).color(add_color))
            .style(move |_theme, status| {
                let tc = if matches!(status, button::Status::Hovered) {
                    add_hover
                } else {
                    add_color
                };
                button::Style {
                    background: Some(iced::Color::TRANSPARENT.into()),
                    text_color: tc,
                    ..Default::default()
                }
            })
            .padding([4, 8])
            .on_press(Event::OpenFolder);
        tabs_row = tabs_row.push(add_btn);

        // Right side: spacer + workspace name + branch + add workspace + peek
        tabs_row = tabs_row.push(iced::widget::Space::new().width(Length::Fill));

        if let Some(ws) = self.active_workspace() {
            let ws_color = ws.color.color(theme);
            tabs_row = tabs_row.push(
                text(&ws.name)
                    .size(12)
                    .color(ws_color)
                    .font(iced::Font::with_name("Menlo")),
            );

            // Close workspace button (only if more than one workspace)
            if self.workspaces.len() > 1 {
                let close_color = theme.overlay0();
                let close_hover = theme.text_primary();
                let ws_idx = self.active_workspace_idx;
                let close_ws_btn = button(text("\u{00d7}").size(12).color(close_color))
                    .style(move |_theme, status| {
                        let tc = if matches!(status, button::Status::Hovered) {
                            close_hover
                        } else {
                            close_color
                        };
                        button::Style {
                            background: Some(iced::Color::TRANSPARENT.into()),
                            text_color: tc,
                            ..Default::default()
                        }
                    })
                    .padding([2, 4])
                    .on_press(Event::WorkspaceClose(ws_idx));
                tabs_row = tabs_row.push(close_ws_btn);
            }

            if let Some(tab) = self.active_tab() {
                tabs_row = tabs_row.push(
                    text(format!("  \u{e0a0} {}", tab.branch_name))
                        .size(12)
                        .color(theme.overlay0())
                        .font(iced::Font::with_name("Menlo")),
                );
            }
        }

        // Right edge peek indicator
        if self.edge_peek_right {
            if let Some(right_ws) = self.workspaces.get(self.active_workspace_idx + 1) {
                let ws_color = right_ws.color.color(theme);
                let peek_btn = button(
                    text(format!("{} \u{203a}", right_ws.name))
                        .size(11)
                        .color(ws_color)
                        .font(iced::Font::with_name("Menlo")),
                )
                .style(move |_theme, _status| button::Style {
                    background: Some(iced::Color::TRANSPARENT.into()),
                    text_color: ws_color,
                    ..Default::default()
                })
                .padding([4, 6])
                .on_press(Event::WorkspaceSelect(self.active_workspace_idx + 1));
                tabs_row = tabs_row.push(peek_btn);
            }
        }

        let bg = theme.bg_crust();
        let border_color = theme.surface0();
        let tab_container = container(
            tabs_row
                .padding([4, 8])
                .align_y(iced::Alignment::Center),
        )
        .width(Length::Fill)
        .style(move |_| container::Style {
            background: Some(bg.into()),
            ..Default::default()
        });

        // Bottom border as separator
        let separator = container(iced::widget::Space::new().height(0))
            .width(Length::Fill)
            .height(Length::Fixed(1.0))
            .style(move |_| container::Style {
                background: Some(border_color.into()),
                ..Default::default()
            });

        column![tab_container, separator].into()
    }

    fn view_workspace_content<'a>(&'a self, ws: &'a Workspace) -> Element<'a, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        if let Some(tab) = ws.active_tab() {
            let sidebar = self.view_sidebar(tab);

            let main_panel = if tab.viewing_file_path.is_some() {
                self.view_file_content(tab)
            } else if tab.selected_file.is_some() {
                self.view_diff_panel(tab)
            } else {
                self.view_terminal(tab)
            };

            // Draggable divider
            let divider_color = if self.dragging_divider {
                theme.accent()
            } else {
                theme.border()
            };
            let divider = iced::widget::mouse_area(
                container(iced::widget::Space::new())
                    .width(Length::Fixed(4.0))
                    .height(Length::Fill)
                    .style(move |_| container::Style {
                        background: Some(divider_color.into()),
                        ..Default::default()
                    }),
            )
            .on_press(Event::DividerDragStart)
            .interaction(iced::mouse::Interaction::ResizingHorizontally);

            row![sidebar, divider, main_panel]
                .spacing(0)
                .width(Length::Fill)
                .height(Length::Fill)
                .into()
        } else {
            let bg = theme.bg_base();
            container(
                column![
                    text("No repository open").size(16).color(theme.text_primary()),
                    button(text("Open Folder").size(14))
                        .style(button::primary)
                        .padding([8, 16])
                        .on_press(Event::OpenFolder)
                ]
                .spacing(16)
                .align_x(iced::Alignment::Center),
            )
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .style(move |_| container::Style {
                background: Some(bg.into()),
                ..Default::default()
            })
            .into()
        }
    }

    fn view_workspace_slide(&self) -> Element<'_, Event, Theme, iced::Renderer> {
        let viewport_width = self.content_viewport_width();
        let active_idx = self.active_workspace_idx;
        let theme = &self.theme;

        // When not animating or swiping, render active workspace directly
        // (avoids scroll_to timing issues, especially after creating new workspaces)
        let is_swiping = self.last_user_scroll.is_some();
        if !self.slide_animating && !is_swiping {
            if let Some(ws) = self.workspaces.get(active_idx) {
                return container(self.view_workspace_content(ws))
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .clip(true)
                    .into();
            }
        }

        let mut panels = Row::new().spacing(0);

        for (idx, ws) in self.workspaces.iter().enumerate() {
            // Only render full content for active workspace and immediate neighbors
            let panel: Element<'_, Event, Theme, iced::Renderer> =
                if (idx as i32 - active_idx as i32).unsigned_abs() <= 1 {
                    self.view_workspace_content(ws)
                } else {
                    // Distant workspace: render colored placeholder
                    let bg = theme.bg_base();
                    container(iced::widget::Space::new())
                        .width(Length::Fill)
                        .height(Length::Fill)
                        .style(move |_| container::Style {
                            background: Some(bg.into()),
                            ..Default::default()
                        })
                        .into()
                };

            let panel_container = container(panel)
                .width(Length::Fixed(viewport_width))
                .height(Length::Fill)
                .clip(true);
            panels = panels.push(panel_container);
        }

        scrollable(panels)
            .direction(scrollable::Direction::Horizontal(
                scrollable::Scrollbar::new().width(0).scroller_width(0),
            ))
            .id(workspace_scrollable_id())
            .on_scroll(Event::SlideScrolled)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(|_theme, _status| {
                let transparent_rail = scrollable::Rail {
                    background: None,
                    border: iced::Border::default(),
                    scroller: scrollable::Scroller {
                        background: iced::Color::TRANSPARENT.into(),
                        border: iced::Border::default(),
                    },
                };
                scrollable::Style {
                    container: container::Style::default(),
                    vertical_rail: transparent_rail,
                    horizontal_rail: transparent_rail,
                    gap: None,
                    auto_scroll: scrollable::AutoScroll {
                        background: iced::Color::TRANSPARENT.into(),
                        border: iced::Border::default(),
                        shadow: iced::Shadow::default(),
                        icon: iced::Color::TRANSPARENT,
                    },
                }
            })
            .into()
    }

    fn view_sidebar<'a>(
        &'a self,
        tab: &'a TabState,
    ) -> Element<'a, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        let mut content = Column::new().spacing(0);

        // Mode toggle buttons
        let toggle = self.view_sidebar_toggle(tab);
        content = content.push(toggle);

        // Content based on mode
        let mode_content: Element<'_, Event, Theme, iced::Renderer> = match tab.sidebar_mode {
            SidebarMode::Git => self.view_git_list(tab),
            SidebarMode::Files => self.view_file_tree(tab),
        };

        content = content.push(mode_content);

        let bg = theme.bg_surface();
        container(content)
            .width(Length::Fixed(self.sidebar_width))
            .height(Length::Fill)
            .style(move |_| container::Style {
                background: Some(bg.into()),
                ..Default::default()
            })
            .into()
    }

    fn view_sidebar_toggle<'a>(
        &'a self,
        tab: &'a TabState,
    ) -> Element<'a, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        let font = self.ui_font();
        let changes = tab.total_changes();

        let git_active = tab.sidebar_mode == SidebarMode::Git;
        let files_active = tab.sidebar_mode == SidebarMode::Files;
        let surface0 = theme.surface0();

        // Git button with optional badge
        let git_text_color = if git_active {
            theme.text_primary()
        } else {
            theme.overlay1()
        };

        let mut git_content: Row<'_, Event, Theme, iced::Renderer> =
            Row::new().spacing(4).align_y(iced::Alignment::Center);
        git_content = git_content.push(text("Git").size(font).color(git_text_color));

        if changes > 0 {
            let badge_bg = iced::Color {
                a: 0.2,
                ..theme.warning()
            };
            let warning_color = theme.warning();
            let badge = container(text(format!("{}", changes)).size(10).color(warning_color))
                .padding([1, 6])
                .style(move |_| container::Style {
                    background: Some(badge_bg.into()),
                    border: iced::Border {
                        radius: 8.0.into(),
                        ..Default::default()
                    },
                    ..Default::default()
                });
            git_content = git_content.push(badge);
        }

        let git_btn = button(git_content)
            .style(move |_theme, status| {
                let bg = if git_active {
                    Some(surface0.into())
                } else if matches!(status, button::Status::Hovered) {
                    Some(surface0.into())
                } else {
                    Some(iced::Color::TRANSPARENT.into())
                };
                button::Style {
                    background: bg,
                    border: iced::Border {
                        radius: 6.0.into(),
                        ..Default::default()
                    },
                    text_color: git_text_color,
                    ..Default::default()
                }
            })
            .padding([4, 12])
            .on_press(Event::ToggleSidebarMode);

        // Files button
        let files_text_color = if files_active {
            theme.text_primary()
        } else {
            theme.overlay1()
        };

        let files_btn = button(text("Files").size(font).color(files_text_color))
            .style(move |_theme, status| {
                let bg = if files_active {
                    Some(surface0.into())
                } else if matches!(status, button::Status::Hovered) {
                    Some(surface0.into())
                } else {
                    Some(iced::Color::TRANSPARENT.into())
                };
                button::Style {
                    background: bg,
                    border: iced::Border {
                        radius: 6.0.into(),
                        ..Default::default()
                    },
                    text_color: files_text_color,
                    ..Default::default()
                }
            })
            .padding([4, 12])
            .on_press(Event::ToggleSidebarMode);

        let bg = theme.bg_crust();
        let border_color = theme.surface0();

        let toggle_row = container(row![git_btn, files_btn].spacing(4))
            .padding(8)
            .width(Length::Fill)
            .style(move |_| container::Style {
                background: Some(bg.into()),
                ..Default::default()
            });

        let separator = container(iced::widget::Space::new().height(0))
            .width(Length::Fill)
            .height(Length::Fixed(1.0))
            .style(move |_| container::Style {
                background: Some(border_color.into()),
                ..Default::default()
            });

        column![toggle_row, separator].into()
    }

    fn view_file_tree<'a>(
        &'a self,
        tab: &'a TabState,
    ) -> Element<'a, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        let font = self.ui_font();
        let font_small = self.ui_font_small();
        let mut content = Column::new().spacing(2).padding(8);

        // Current path - show relative to repo if inside it, otherwise show with ~ for home
        let home = std::env::var("HOME").unwrap_or_default();
        let path_display = if let Ok(rel_path) = tab.current_dir.strip_prefix(&tab.repo_path) {
            // Inside repo - show repo_name/relative/path/
            if rel_path.as_os_str().is_empty() {
                format!("{}/", tab.repo_name)
            } else {
                format!("{}/{}/", tab.repo_name, rel_path.display())
            }
        } else if let Ok(rel_home) = tab.current_dir.strip_prefix(&home) {
            // Outside repo but under home - show ~/path/
            format!("~/{}/", rel_home.display())
        } else {
            // Absolute path
            format!("{}/", tab.current_dir.display())
        };

        // Path and hidden toggle
        let hidden_label = if self.show_hidden { "Hide .*" } else { "Show .*" };
        content = content.push(
            row![
                text(path_display)
                    .size(font)
                    .color(theme.accent()),
                iced::widget::Space::new().width(Length::Fill),
                button(text(hidden_label).size(font_small))
                    .style(button::text)
                    .padding([2, 6])
                    .on_press(Event::ToggleHidden),
            ]
            .padding([4, 0])
            .align_y(iced::Alignment::Center),
        );

        // Up button (if not at repo root)
        if tab.current_dir != tab.repo_path {
            let muted = theme.text_secondary();
            content = content.push(
                button(
                    row![
                        text("..").size(font).color(muted).width(Length::Fixed(20.0)),
                        text("(parent)").size(font_small).color(muted),
                    ]
                    .spacing(8),
                )
                .style(button::text)
                .padding([4, 8])
                .on_press(Event::NavigateUp),
            );
        }

        // File tree entries
        for entry in &tab.file_tree {
            let (icon, name_suffix, icon_color, name_color, bg_color) = if entry.is_dir {
                // Folders: blue folder icon, trailing /, light background
                (
                    "📁",
                    "/",
                    theme.accent(),
                    theme.accent(),
                    Some(theme.bg_base()),
                )
            } else {
                // Files: colored by extension
                let ext = entry.path.extension().and_then(|e| e.to_str()).unwrap_or("");
                let file_color = match ext {
                    "ts" | "tsx" => theme.accent(),
                    "js" | "jsx" => theme.warning(),
                    "md" => theme.success(),
                    "json" => theme.warning(),
                    "rs" => match self.theme {
                        AppTheme::Dark => color!(0xfab387),
                        AppTheme::Light => color!(0xfe640b),
                    },
                    "toml" | "yml" | "yaml" => match self.theme {
                        AppTheme::Dark => color!(0x94e2d5),
                        AppTheme::Light => color!(0x179299),
                    },
                    "css" | "scss" => match self.theme {
                        AppTheme::Dark => color!(0xcba6f7),
                        AppTheme::Light => color!(0x8839ef),
                    },
                    "html" => theme.danger(),
                    _ => theme.text_secondary(),
                };
                ("  ", "", file_color, theme.text_primary(), None)
            };

            let entry_row = row![
                text(icon).size(font).color(icon_color).width(Length::Fixed(24.0)),
                text(format!("{}{}", entry.name, name_suffix)).size(font).color(name_color),
            ]
            .spacing(4);

            let event = if entry.is_dir {
                Event::NavigateDir(entry.path.clone())
            } else {
                Event::ViewFile(entry.path.clone())
            };

            let btn = button(entry_row)
                .style(button::text)
                .padding([4, 8])
                .width(Length::Fill)
                .on_press(event);

            if let Some(bg) = bg_color {
                content = content.push(
                    container(btn)
                        .width(Length::Fill)
                        .style(move |_| container::Style {
                            background: Some(bg.into()),
                            ..Default::default()
                        }),
                );
            } else {
                content = content.push(btn);
            }
        }

        if tab.file_tree.is_empty() {
            content = content.push(
                text("Empty directory")
                    .size(font)
                    .color(theme.text_secondary()),
            );
        }

        scrollable(content)
            .height(Length::Fill)
            .width(Length::Fill)
            .into()
    }

    fn view_file_content<'a>(
        &'a self,
        tab: &'a TabState,
    ) -> Element<'a, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        let font = self.ui_font();
        let font_small = self.ui_font_small();
        let mut content = Column::new().spacing(0);

        // Header with filename and close button
        let file_name = tab
            .viewing_file_path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        let rel_path = tab
            .viewing_file_path
            .as_ref()
            .and_then(|p| p.strip_prefix(&tab.repo_path).ok())
            .map(|p| p.display().to_string())
            .unwrap_or(file_name.clone());

        // Check if this is a markdown file with rendered content
        let is_markdown = tab.webview_content.is_some();

        let header_bg = theme.bg_overlay();
        let header = if is_markdown {
            // Markdown header with "View in Browser" button for Mermaid support
            row![
                text(rel_path).size(font).color(theme.text_primary()),
                iced::widget::Space::new().width(Length::Fill),
                button(text("View in Browser").size(font))
                    .style(button::secondary)
                    .padding([4, 8])
                    .on_press(Event::OpenMarkdownInBrowser),
                iced::widget::Space::new().width(Length::Fixed(8.0)),
                text("Esc: close").size(font_small).color(theme.text_secondary()),
                iced::widget::Space::new().width(Length::Fixed(16.0)),
                button(text("Close").size(font))
                    .style(button::secondary)
                    .padding([4, 8])
                    .on_press(Event::CloseFileView),
            ]
            .padding(8)
            .spacing(8)
        } else {
            row![
                text(rel_path).size(font).color(theme.text_primary()),
                iced::widget::Space::new().width(Length::Fill),
                button(text("Copy All").size(font))
                    .style(button::secondary)
                    .padding([4, 8])
                    .on_press(Event::CopyFileContent),
                iced::widget::Space::new().width(Length::Fixed(8.0)),
                button(text("Open in Browser").size(font))
                    .style(button::secondary)
                    .padding([4, 8])
                    .on_press(Event::OpenFileInBrowser),
                iced::widget::Space::new().width(Length::Fixed(8.0)),
                text("Esc: close").size(font_small).color(theme.text_secondary()),
                iced::widget::Space::new().width(Length::Fixed(16.0)),
                button(text("Close").size(font))
                    .style(button::secondary)
                    .padding([4, 8])
                    .on_press(Event::CloseFileView),
            ]
            .padding(8)
            .spacing(8)
        };

        content = content.push(
            container(header)
                .width(Length::Fill)
                .style(move |_| container::Style {
                    background: Some(header_bg.into()),
                    ..Default::default()
                }),
        );

        // Check if we're viewing an image
        if let Some(handle) = &tab.image_handle {
            // Display image
            let img = image(handle.clone())
                .content_fit(iced::ContentFit::Contain);

            content = content.push(
                scrollable(
                    container(img)
                        .width(Length::Fill)
                        .center_x(Length::Fill)
                        .padding(16)
                )
                .height(Length::Fill)
                .width(Length::Fill),
            );
        } else if is_markdown && webview::is_active() {
            // WebView is rendering markdown - show placeholder
            let bg = theme.bg_base();
            content = content.push(
                container(iced::widget::Space::new())
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .style(move |_| container::Style {
                        background: Some(bg.into()),
                        ..Default::default()
                    }),
            );
        } else if is_markdown {
            // Fallback: Render markdown with Iced-native formatting (WebView not ready)
            content = content.push(self.view_markdown_content(tab));
        } else {
            // File content with line numbers
            let mut file_column = Column::new().spacing(0);

            let ext = tab
                .viewing_file_path
                .as_ref()
                .and_then(|p| p.extension())
                .and_then(|e| e.to_str())
                .unwrap_or("");

            for (i, line) in tab.file_content.iter().enumerate() {
                let line_num = format!("{:4}", i + 1);

                // Simple syntax highlighting based on extension
                let line_color = self.get_syntax_color(line, ext);

                let line_row = row![
                    text(line_num)
                        .size(font)
                        .color(theme.text_muted())
                        .font(iced::Font::MONOSPACE),
                    text(" ")
                        .size(font)
                        .font(iced::Font::MONOSPACE),
                    text(line)
                        .size(font)
                        .color(line_color)
                        .font(iced::Font::MONOSPACE),
                ]
                .spacing(0);

                file_column = file_column.push(
                    container(line_row)
                        .width(Length::Fill)
                        .padding([1, 4]),
                );
            }

            if tab.file_content.is_empty() {
                file_column = file_column.push(
                    text("(empty file)")
                        .size(font)
                        .color(theme.text_secondary()),
                );
            }

            content = content.push(
                scrollable(file_column.padding(8))
                    .height(Length::Fill)
                    .width(Length::Fill),
            );
        }

        let bg = theme.bg_base();
        container(content)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(move |_| container::Style {
                background: Some(bg.into()),
                ..Default::default()
            })
            .into()
    }

    fn view_markdown_content<'a>(
        &'a self,
        tab: &'a TabState,
    ) -> Element<'a, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        let font = self.ui_font();
        let mut content = Column::new().spacing(8).padding(16);

        let mut in_code_block = false;
        let mut in_mermaid_block = false;
        let mut code_block_content: Vec<String> = Vec::new();
        let mut in_list = false;

        for line in &tab.file_content {
            let trimmed = line.trim();

            // Handle code blocks
            if trimmed.starts_with("```") {
                if in_mermaid_block {
                    // End of mermaid block - just close it (placeholder already shown)
                    in_mermaid_block = false;
                    continue;
                } else if in_code_block {
                    // End of code block - render accumulated content
                    let code_bg = theme.bg_overlay();
                    // Create an owned string for the text widget
                    let code_content: String = code_block_content.join("\n");
                    let mut code_col = Column::new().spacing(0);
                    for code_line in code_content.lines() {
                        code_col = code_col.push(
                            text(code_line.to_string())
                                .size(font - 1.0)
                                .font(iced::Font::MONOSPACE)
                                .color(theme.text_primary()),
                        );
                    }
                    content = content.push(
                        container(code_col)
                            .width(Length::Fill)
                            .padding(12)
                            .style(move |_| container::Style {
                                background: Some(code_bg.into()),
                                border: iced::Border {
                                    radius: 6.0.into(),
                                    ..Default::default()
                                },
                                ..Default::default()
                            }),
                    );
                    code_block_content.clear();
                    in_code_block = false;
                } else {
                    // Start of code block - check for mermaid
                    let lang = trimmed.strip_prefix("```").unwrap_or("");
                    if lang == "mermaid" {
                        // Show a placeholder for mermaid diagrams
                        content = content.push(
                            container(
                                column![
                                    text("Mermaid Diagram")
                                        .size(font)
                                        .color(theme.accent()),
                                    text("Click \"View in Browser\" to see the rendered diagram")
                                        .size(font - 2.0)
                                        .color(theme.text_secondary()),
                                ]
                                .spacing(4)
                                .align_x(iced::Alignment::Center),
                            )
                            .width(Length::Fill)
                            .padding(24)
                            .style(move |_| container::Style {
                                background: Some(theme.bg_overlay().into()),
                                border: iced::Border {
                                    radius: 6.0.into(),
                                    color: theme.accent(),
                                    width: 1.0,
                                },
                                ..Default::default()
                            }),
                        );
                        // Skip content until closing ```
                        in_mermaid_block = true;
                    } else {
                        in_code_block = true;
                    }
                }
                continue;
            }

            // Skip mermaid block content
            if in_mermaid_block {
                continue;
            }

            if in_code_block {
                code_block_content.push(line.clone());
                continue;
            }

            // Headers
            if trimmed.starts_with("######") {
                let header_text = trimmed.strip_prefix("######").unwrap_or("").trim();
                content = content.push(
                    text(header_text)
                        .size(font)
                        .color(theme.text_primary()),
                );
            } else if trimmed.starts_with("#####") {
                let header_text = trimmed.strip_prefix("#####").unwrap_or("").trim();
                content = content.push(
                    text(header_text)
                        .size(font + 1.0)
                        .color(theme.text_primary()),
                );
            } else if trimmed.starts_with("####") {
                let header_text = trimmed.strip_prefix("####").unwrap_or("").trim();
                content = content.push(
                    text(header_text)
                        .size(font + 2.0)
                        .color(theme.text_primary()),
                );
            } else if trimmed.starts_with("###") {
                let header_text = trimmed.strip_prefix("###").unwrap_or("").trim();
                content = content.push(
                    text(header_text)
                        .size(font + 4.0)
                        .color(theme.text_primary()),
                );
            } else if trimmed.starts_with("##") {
                let header_text = trimmed.strip_prefix("##").unwrap_or("").trim();
                let border_color = theme.border();
                content = content.push(
                    column![
                        text(header_text)
                            .size(font + 6.0)
                            .color(theme.text_primary()),
                        container(iced::widget::Space::new())
                            .width(Length::Fill)
                            .height(Length::Fixed(1.0))
                            .style(move |_| container::Style {
                                background: Some(border_color.into()),
                                ..Default::default()
                            }),
                    ]
                    .spacing(4),
                );
            } else if trimmed.starts_with('#') && !trimmed.starts_with("##") {
                let header_text = trimmed.strip_prefix('#').unwrap_or("").trim();
                let border_color = theme.border();
                content = content.push(
                    column![
                        text(header_text)
                            .size(font + 10.0)
                            .color(theme.text_primary()),
                        container(iced::widget::Space::new())
                            .width(Length::Fill)
                            .height(Length::Fixed(1.0))
                            .style(move |_| container::Style {
                                background: Some(border_color.into()),
                                ..Default::default()
                            }),
                    ]
                    .spacing(4),
                );
            }
            // Blockquotes
            else if trimmed.starts_with('>') {
                let quote_text = trimmed.strip_prefix('>').unwrap_or("").trim();
                let border_color = theme.border();
                content = content.push(
                    container(
                        text(quote_text)
                            .size(font)
                            .color(theme.text_secondary()),
                    )
                    .padding([8, 16])
                    .style(move |_| container::Style {
                        border: iced::Border {
                            color: border_color,
                            width: 0.0,
                            radius: 0.0.into(),
                        },
                        ..Default::default()
                    }),
                );
            }
            // Horizontal rule
            else if trimmed == "---" || trimmed == "***" || trimmed == "___" {
                let border_color = theme.border();
                content = content.push(
                    container(iced::widget::Space::new())
                        .width(Length::Fill)
                        .height(Length::Fixed(1.0))
                        .style(move |_| container::Style {
                            background: Some(border_color.into()),
                            ..Default::default()
                        }),
                );
            }
            // Unordered lists
            else if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
                let list_text = &trimmed[2..];
                content = content.push(
                    row![
                        text("  \u{2022}  ").size(font).color(theme.text_secondary()),
                        text(list_text).size(font).color(theme.text_primary()),
                    ]
                    .spacing(0),
                );
                in_list = true;
            }
            // Task lists
            else if trimmed.starts_with("- [ ] ") || trimmed.starts_with("- [x] ") || trimmed.starts_with("- [X] ") {
                let is_checked = trimmed.starts_with("- [x]") || trimmed.starts_with("- [X]");
                let task_text = &trimmed[6..];
                let checkbox = if is_checked { "\u{2611}" } else { "\u{2610}" };
                content = content.push(
                    row![
                        text(format!("  {}  ", checkbox))
                            .size(font)
                            .color(if is_checked { theme.success() } else { theme.text_secondary() }),
                        text(task_text).size(font).color(theme.text_primary()),
                    ]
                    .spacing(0),
                );
            }
            // Ordered lists (basic)
            else if trimmed.len() > 2 && trimmed.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) && trimmed.contains(". ") {
                if let Some(pos) = trimmed.find(". ") {
                    let num = &trimmed[..pos];
                    let list_text = &trimmed[pos + 2..];
                    content = content.push(
                        row![
                            text(format!("  {}.  ", num)).size(font).color(theme.text_secondary()),
                            text(list_text).size(font).color(theme.text_primary()),
                        ]
                        .spacing(0),
                    );
                }
            }
            // Empty line
            else if trimmed.is_empty() {
                if in_list {
                    in_list = false;
                }
                content = content.push(iced::widget::Space::new().height(Length::Fixed(8.0)));
            }
            // Regular paragraph
            else {
                content = content.push(
                    text(line)
                        .size(font)
                        .color(theme.text_primary()),
                );
            }
        }

        scrollable(content)
            .height(Length::Fill)
            .width(Length::Fill)
            .into()
    }

    fn get_syntax_color(&self, line: &str, ext: &str) -> iced::Color {
        let theme = &self.theme;
        let trimmed = line.trim();

        // Theme-aware syntax colors
        let comment = theme.text_secondary();
        let keyword = match self.theme {
            AppTheme::Dark => color!(0xcba6f7),
            AppTheme::Light => color!(0x8839ef),
        };
        let declaration = theme.accent();
        let function = theme.success();
        let control = theme.warning();
        let types = match self.theme {
            AppTheme::Dark => color!(0xfab387),
            AppTheme::Light => color!(0xfe640b),
        };
        let default = theme.text_primary();

        match ext {
            "ts" | "tsx" | "js" | "jsx" => {
                if trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed.starts_with("*") {
                    comment
                } else if trimmed.starts_with("import ") || trimmed.starts_with("export ") {
                    keyword
                } else if trimmed.starts_with("const ") || trimmed.starts_with("let ") || trimmed.starts_with("var ") {
                    declaration
                } else if trimmed.starts_with("function ") || trimmed.starts_with("async ") || trimmed.contains("=>") {
                    function
                } else if trimmed.starts_with("return ") || trimmed.starts_with("if ") || trimmed.starts_with("else") {
                    control
                } else {
                    default
                }
            }
            "md" => {
                if trimmed.starts_with('#') {
                    declaration
                } else if trimmed.starts_with('-') || trimmed.starts_with('*') || trimmed.starts_with(|c: char| c.is_numeric()) {
                    function
                } else if trimmed.starts_with('>') {
                    comment
                } else if trimmed.starts_with("```") {
                    control
                } else {
                    default
                }
            }
            "rs" => {
                if trimmed.starts_with("//") {
                    comment
                } else if trimmed.starts_with("use ") || trimmed.starts_with("mod ") || trimmed.starts_with("pub ") {
                    keyword
                } else if trimmed.starts_with("fn ") || trimmed.starts_with("impl ") {
                    function
                } else if trimmed.starts_with("let ") || trimmed.starts_with("const ") {
                    declaration
                } else if trimmed.starts_with("struct ") || trimmed.starts_with("enum ") {
                    types
                } else {
                    default
                }
            }
            "json" => {
                if trimmed.starts_with('"') && trimmed.contains(':') {
                    declaration
                } else {
                    function
                }
            }
            _ => default,
        }
    }

    fn view_git_list<'a>(
        &'a self,
        tab: &'a TabState,
    ) -> Element<'a, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        let font = self.ui_font();
        let mut content = Column::new().spacing(8).padding(8);

        // Branch display - styled rounded container with diamond icon
        if Repository::open(&tab.repo_path).is_ok() {
            let branch_bg = theme.bg_base();
            let mauve = theme.mauve();
            let branch_container = container(
                row![
                    text("\u{25c6}").size(font).color(mauve),
                    text(&tab.branch_name)
                        .size(font)
                        .color(mauve)
                        .font(iced::Font::with_name("Menlo")),
                ]
                .spacing(6)
                .align_y(iced::Alignment::Center),
            )
            .padding([4, 10])
            .style(move |_| container::Style {
                background: Some(branch_bg.into()),
                border: iced::Border {
                    radius: 6.0.into(),
                    ..Default::default()
                },
                ..Default::default()
            });
            content = content.push(branch_container);
        }

        if !tab.staged.is_empty() {
            content = content.push(
                row![
                    text("S T A G E D").size(10).color(theme.overlay0()),
                    text(format!("{}", tab.staged.len()))
                        .size(10)
                        .color(theme.success()),
                ]
                .spacing(6),
            );
            for file in &tab.staged {
                content = content.push(self.view_file_item(file, tab));
            }
        }

        if !tab.unstaged.is_empty() {
            content = content.push(
                row![
                    text("U N S T A G E D").size(10).color(theme.overlay0()),
                    text(format!("{}", tab.unstaged.len()))
                        .size(10)
                        .color(theme.warning()),
                ]
                .spacing(6),
            );
            for file in &tab.unstaged {
                content = content.push(self.view_file_item(file, tab));
            }
        }

        if !tab.untracked.is_empty() {
            content = content.push(
                row![
                    text("U N T R A C K E D").size(10).color(theme.overlay0()),
                    text(format!("{}", tab.untracked.len()))
                        .size(10)
                        .color(theme.text_secondary()),
                ]
                .spacing(6),
            );
            for file in &tab.untracked {
                content = content.push(self.view_file_item(file, tab));
            }
        }

        if tab.staged.is_empty() && tab.unstaged.is_empty() && tab.untracked.is_empty() {
            let msg = if Repository::open(&tab.repo_path).is_ok() {
                "No changes"
            } else {
                "Not a git repository"
            };
            content = content.push(text(msg).size(font).color(theme.text_secondary()));
        }

        scrollable(content)
            .height(Length::Fill)
            .width(Length::Fill)
            .into()
    }

    fn view_file_item<'a>(
        &'a self,
        file: &'a FileEntry,
        tab: &'a TabState,
    ) -> Element<'a, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        let font = self.ui_font();
        let status_color = match file.status.as_str() {
            "A" => theme.success(),
            "M" => theme.warning(),
            "D" => theme.danger(),
            "R" => theme.accent(),
            _ => theme.text_secondary(),
        };

        let is_selected = tab.selected_file.as_ref() == Some(&file.path);
        let text_color = if is_selected {
            match self.theme {
                AppTheme::Dark => color!(0xffffff),
                AppTheme::Light => color!(0xffffff),
            }
        } else {
            theme.text_primary()
        };

        let file_row = row![
            text(&file.status)
                .size(font)
                .color(status_color)
                .width(Length::Fixed(20.0)),
            text(&file.path).size(font).color(text_color),
        ]
        .spacing(8);

        let btn_style = if is_selected {
            button::primary
        } else {
            button::text
        };

        button(file_row)
            .style(btn_style)
            .padding([4, 8])
            .on_press(Event::FileSelect(file.path.clone(), file.is_staged))
            .into()
    }

    fn view_diff_panel<'a>(
        &'a self,
        tab: &'a TabState,
    ) -> Element<'a, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        let font = self.ui_font();
        let font_small = self.ui_font_small();
        let mut content = Column::new().spacing(0);

        // Header
        let header_bg = theme.bg_overlay();
        let header = row![
            text(tab.selected_file.as_deref().unwrap_or(""))
                .size(font)
                .color(theme.text_primary()),
            iced::widget::Space::new().width(Length::Fill),
            text("j/k: navigate  Esc: back")
                .size(font_small)
                .color(theme.text_secondary()),
            iced::widget::Space::new().width(Length::Fixed(16.0)),
            button(text("Back to Terminal").size(font))
                .style(button::secondary)
                .padding([4, 8])
                .on_press(Event::ClearSelection),
        ]
        .padding(8)
        .spacing(8);

        content = content.push(
            container(header).width(Length::Fill).style(move |_| container::Style {
                background: Some(header_bg.into()),
                ..Default::default()
            }),
        );

        // Diff content
        let mut diff_column = Column::new().spacing(0);

        if tab.diff_lines.is_empty() {
            diff_column =
                diff_column.push(text("No diff available").size(font).color(theme.text_secondary()));
        } else {
            for line in &tab.diff_lines {
                diff_column = diff_column.push(self.view_diff_line(line));
            }
        }

        content = content.push(
            scrollable(diff_column.padding(8))
                .height(Length::Fill)
                .width(Length::Fill),
        );

        let bg = theme.bg_base();
        container(content)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(move |_| container::Style {
                background: Some(bg.into()),
                ..Default::default()
            })
            .into()
    }

    fn view_diff_line<'a>(&'a self, line: &'a DiffLine) -> Element<'a, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        let font = self.ui_font();
        let (line_color, bg_color) = match line.line_type {
            DiffLineType::Addition => (theme.success(), Some(theme.diff_add_bg())),
            DiffLineType::Deletion => (theme.danger(), Some(theme.diff_del_bg())),
            DiffLineType::Header => (theme.accent(), None),
            DiffLineType::Context => (theme.text_secondary(), None),
        };

        // Line numbers
        let old_num = line
            .old_line_num
            .map(|n| format!("{:4}", n))
            .unwrap_or_else(|| "    ".to_string());
        let new_num = line
            .new_line_num
            .map(|n| format!("{:4}", n))
            .unwrap_or_else(|| "    ".to_string());

        let prefix = match line.line_type {
            DiffLineType::Addition => "+",
            DiffLineType::Deletion => "-",
            DiffLineType::Context => " ",
            DiffLineType::Header => "",
        };

        // Build content - either with inline changes or plain
        let content_element: Element<'a, Event, Theme, iced::Renderer> =
            if let Some(ref changes) = line.inline_changes {
                // Build rich text with word-level highlighting
                let mut content_row = Row::new().spacing(0);
                for change in changes {
                    let (change_color, change_bg) = match (&line.line_type, &change.change_type) {
                        (DiffLineType::Deletion, ChangeType::Delete) => {
                            (color!(0xffffff), Some(theme.diff_del_highlight()))
                        }
                        (DiffLineType::Addition, ChangeType::Insert) => {
                            (color!(0xffffff), Some(theme.diff_add_highlight()))
                        }
                        _ => (line_color, None),
                    };

                    let change_text = text(&change.value)
                        .size(font)
                        .color(change_color)
                        .font(iced::Font::MONOSPACE);

                    if let Some(bg) = change_bg {
                        content_row = content_row.push(
                            container(change_text).style(move |_| container::Style {
                                background: Some(bg.into()),
                                ..Default::default()
                            }),
                        );
                    } else {
                        content_row = content_row.push(change_text);
                    }
                }
                content_row.into()
            } else {
                text(&line.content)
                    .size(font)
                    .color(line_color)
                    .font(iced::Font::MONOSPACE)
                    .into()
            };

        let line_num_color = theme.text_muted();
        let line_row = if line.line_type == DiffLineType::Header {
            row![content_element].spacing(0)
        } else {
            row![
                text(old_num)
                    .size(font)
                    .color(line_num_color)
                    .font(iced::Font::MONOSPACE),
                text(new_num)
                    .size(font)
                    .color(line_num_color)
                    .font(iced::Font::MONOSPACE),
                text(prefix)
                    .size(font)
                    .color(line_color)
                    .font(iced::Font::MONOSPACE),
                content_element,
            ]
            .spacing(4)
        };

        let line_container = container(line_row).width(Length::Fill).padding([1, 4]);

        if let Some(bg) = bg_color {
            line_container
                .style(move |_| container::Style {
                    background: Some(bg.into()),
                    ..Default::default()
                })
                .into()
        } else {
            line_container.into()
        }
    }

    fn view_terminal<'a>(
        &'a self,
        tab: &'a TabState,
    ) -> Element<'a, Event, Theme, iced::Renderer> {
        let theme = &self.theme;

        let bg = theme.bg_base();
        let terminal_view: Element<'a, Event, Theme, iced::Renderer> = if let Some(term) = &tab.terminal {
            let tab_id = tab.id;
            container(TerminalView::show(term).map(move |e| Event::Terminal(tab_id, e)))
                .width(Length::Fill)
                .height(Length::Fill)
                .padding(4)
                .style(move |_| container::Style {
                    background: Some(bg.into()),
                    ..Default::default()
                })
                .into()
        } else {
            container(text("Terminal unavailable").size(14).color(theme.text_secondary()))
                .width(Length::Fill)
                .height(Length::Fill)
                .center_x(Length::Fill)
                .center_y(Length::Fill)
                .into()
        };

        // Stack search bar on top of terminal when active
        if tab.search.is_active {
            let search_bar = self.view_search_bar(tab);
            column![search_bar, terminal_view]
                .spacing(0)
                .width(Length::Fill)
                .height(Length::Fill)
                .into()
        } else {
            terminal_view
        }
    }

    fn view_console_panel(&self) -> Element<'_, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        let ws = match self.active_workspace() {
            Some(ws) => ws,
            None => {
                return iced::widget::Space::new().width(0).height(0).into();
            }
        };
        let console = &ws.console;

        let header = self.view_console_header(console);

        if !self.console_expanded {
            // Collapsed: just the header bar
            let border_color = theme.surface0();
            return container(header)
                .width(Length::Fill)
                .height(Length::Fixed(CONSOLE_HEADER_HEIGHT))
                .style(move |_| container::Style {
                    background: None,
                    border: iced::Border {
                        width: 1.0,
                        color: border_color,
                        radius: 0.0.into(),
                    },
                    ..Default::default()
                })
                .into();
        }

        // Expanded: header + output area
        let output = self.view_console_output(console);

        let bg = theme.bg_crust();
        container(
            column![header, output]
                .spacing(0)
                .width(Length::Fill)
                .height(Length::Fill),
        )
        .width(Length::Fill)
        .height(Length::Fixed(self.console_height))
        .style(move |_| container::Style {
            background: Some(bg.into()),
            ..Default::default()
        })
        .into()
    }

    fn view_console_header<'a>(&'a self, console: &'a ConsoleState) -> Element<'a, Event, Theme, iced::Renderer> {
        let theme = &self.theme;

        // Chevron button (toggle expand/collapse)
        let chevron = if self.console_expanded { "\u{25BC}" } else { "\u{25B6}" };
        let chevron_color = theme.overlay0();
        let chevron_btn = button(
            text(chevron).size(10).color(chevron_color)
        )
        .style(|_theme, _status| button::Style {
            background: Some(iced::Color::TRANSPARENT.into()),
            ..Default::default()
        })
        .padding([4, 6])
        .on_press(Event::ConsoleToggle);

        // Status dot
        let dot_color = match console.status {
            ConsoleStatus::Running => theme.success(),
            ConsoleStatus::Error => theme.danger(),
            ConsoleStatus::Stopped | ConsoleStatus::NoneConfigured => theme.overlay0(),
        };
        let status_dot = container(iced::widget::Space::new())
            .width(Length::Fixed(8.0))
            .height(Length::Fixed(8.0))
            .style(move |_| container::Style {
                background: Some(dot_color.into()),
                border: iced::Border {
                    radius: 4.0.into(),
                    ..Default::default()
                },
                ..Default::default()
            });

        // Process name — click to edit, or show text input when editing
        let name_element: Element<'a, Event, Theme, iced::Renderer> = if let Some(edit_val) = &self.editing_console_command {
            let input_bg = theme.bg_base();
            let input_border = theme.accent();
            text_input("e.g. cargo run, bun run dev", edit_val)
                .on_input(Event::ConsoleCommandChanged)
                .on_submit(Event::ConsoleCommandSubmit)
                .size(12)
                .width(Length::Fixed(220.0))
                .padding([3, 6])
                .style(move |_theme, _status| text_input::Style {
                    background: input_bg.into(),
                    border: iced::Border {
                        width: 1.0,
                        color: input_border,
                        radius: 3.0.into(),
                    },
                    icon: iced::Color::TRANSPARENT,
                    placeholder: theme.overlay0(),
                    value: theme.text_primary(),
                    selection: theme.accent(),
                })
                .into()
        } else {
            let process_name = console.run_command.as_deref().unwrap_or("Click to set command");
            let name_color = if console.run_command.is_some() {
                theme.text_primary()
            } else {
                theme.overlay0()
            };
            let hover_bg = theme.surface0();
            button(
                text(process_name)
                    .size(12)
                    .color(name_color)
                    .font(iced::Font::with_name("Menlo"))
            )
            .style(move |_theme, status| {
                let bg = if matches!(status, button::Status::Hovered) {
                    hover_bg
                } else {
                    iced::Color::TRANSPARENT
                };
                button::Style {
                    background: Some(bg.into()),
                    border: iced::Border { radius: 3.0.into(), ..Default::default() },
                    text_color: name_color,
                    ..Default::default()
                }
            })
            .padding([2, 4])
            .on_press(Event::ConsoleCommandEditStart)
            .into()
        };

        // Uptime
        let uptime = console.uptime_string();
        let uptime_label = text(uptime)
            .size(11)
            .color(theme.overlay0())
            .font(iced::Font::with_name("Menlo"));

        // Spacer
        let spacer = iced::widget::Space::new().width(Length::Fill);

        // Action buttons
        let btn_color = theme.overlay1();
        let hover_bg = theme.surface0();

        let action_btn_style = move |_theme: &Theme, status: button::Status| {
            let bg = if matches!(status, button::Status::Hovered) {
                hover_bg
            } else {
                iced::Color::TRANSPARENT
            };
            button::Style {
                background: Some(bg.into()),
                border: iced::Border {
                    radius: 4.0.into(),
                    ..Default::default()
                },
                text_color: btn_color,
                ..Default::default()
            }
        };

        // Clear button
        let clear_btn = button(text("\u{2300}").size(12).color(btn_color))
            .style(action_btn_style)
            .padding([2, 6])
            .on_press(Event::ConsoleClearOutput);

        // Restart button
        let restart_btn = button(text("\u{21BB}").size(12).color(btn_color))
            .style(action_btn_style)
            .padding([2, 6])
            .on_press(Event::ConsoleRestart);

        // Stop/Start button
        let stop_start_btn = if console.is_running() {
            let stop_color = theme.danger();
            button(text("\u{25A0}").size(12).color(stop_color))
                .style(move |_theme, status| {
                    let bg = if matches!(status, button::Status::Hovered) {
                        hover_bg
                    } else {
                        iced::Color::TRANSPARENT
                    };
                    button::Style {
                        background: Some(bg.into()),
                        border: iced::Border { radius: 4.0.into(), ..Default::default() },
                        text_color: stop_color,
                        ..Default::default()
                    }
                })
                .padding([2, 6])
                .on_press(Event::ConsoleStop)
        } else {
            let start_color = theme.success();
            button(text("\u{25B6}").size(12).color(start_color))
                .style(move |_theme, status| {
                    let bg = if matches!(status, button::Status::Hovered) {
                        hover_bg
                    } else {
                        iced::Color::TRANSPARENT
                    };
                    button::Style {
                        background: Some(bg.into()),
                        border: iced::Border { radius: 4.0.into(), ..Default::default() },
                        text_color: start_color,
                        ..Default::default()
                    }
                })
                .padding([2, 6])
                .on_press_maybe(if console.run_command.is_some() { Some(Event::ConsoleStart) } else { None })
        };

        let header_bg = theme.bg_surface();
        let top_border = theme.surface0();

        container(
            row![
                chevron_btn,
                status_dot,
                name_element,
                uptime_label,
                spacer,
                clear_btn,
                restart_btn,
                stop_start_btn,
            ]
            .spacing(6)
            .align_y(iced::Alignment::Center)
            .padding([0, 8]),
        )
        .width(Length::Fill)
        .height(Length::Fixed(CONSOLE_HEADER_HEIGHT))
        .center_y(Length::Fixed(CONSOLE_HEADER_HEIGHT))
        .style(move |_| container::Style {
            background: Some(header_bg.into()),
            border: iced::Border {
                width: 1.0,
                color: top_border,
                radius: 0.0.into(),
            },
            ..Default::default()
        })
        .into()
    }

    fn view_console_output<'a>(&'a self, console: &'a ConsoleState) -> Element<'a, Event, Theme, iced::Renderer> {
        let theme = &self.theme;

        if console.output_lines.is_empty() {
            // Show hint text
            let hint = if console.run_command.is_none() {
                "No command configured for this workspace"
            } else if console.status == ConsoleStatus::Stopped || console.status == ConsoleStatus::NoneConfigured {
                "Press \u{25B6} to start"
            } else {
                "Waiting for output..."
            };

            let bg = theme.bg_crust();
            return container(
                text(hint)
                    .size(12)
                    .color(theme.overlay0())
                    .font(iced::Font::with_name("Menlo")),
            )
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .style(move |_| container::Style {
                background: Some(bg.into()),
                ..Default::default()
            })
            .into();
        }

        let timestamp_color = theme.surface2();
        let text_color = color!(0xa6adc8); // subtext0
        let stderr_color = theme.danger();

        let mut output_col = Column::new().spacing(0).padding([4, 8]);

        for line in &console.output_lines {
            let ts = text(&line.timestamp)
                .size(10)
                .color(timestamp_color)
                .font(iced::Font::with_name("Menlo"));

            let content_color = if line.is_stderr { stderr_color } else { text_color };
            let content = text(&line.content)
                .size(11)
                .color(content_color)
                .font(iced::Font::with_name("Menlo"));

            output_col = output_col.push(
                row![ts, content]
                    .spacing(8)
                    .align_y(iced::Alignment::Start),
            );
        }

        let bg = theme.bg_crust();
        container(
            scrollable(output_col)
                .anchor_bottom()
                .width(Length::Fill)
                .height(Length::Fill),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .style(move |_| container::Style {
            background: Some(bg.into()),
            ..Default::default()
        })
        .into()
    }

    fn view_search_bar<'a>(
        &'a self,
        tab: &'a TabState,
    ) -> Element<'a, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        let font = self.ui_font();
        let font_small = self.ui_font_small();

        // Match count display
        let match_display = if tab.search.matches.is_empty() {
            if tab.search.query.is_empty() {
                String::new()
            } else {
                "No matches".to_string()
            }
        } else {
            format!("{}/{}", tab.search.current_match + 1, tab.search.matches.len())
        };

        let has_matches = !tab.search.matches.is_empty();

        let search_input = text_input("Search...", &tab.search.query)
            .on_input(Event::SearchQueryChanged)
            .on_submit(Event::SearchExecute)
            .size(font)
            .width(Length::Fixed(200.0))
            .padding([4, 8]);

        let prev_btn = button(text("<").size(font))
            .style(if has_matches { button::secondary } else { button::text })
            .padding([4, 8])
            .on_press_maybe(if has_matches { Some(Event::SearchPrev) } else { None });

        let next_btn = button(text(">").size(font))
            .style(if has_matches { button::secondary } else { button::text })
            .padding([4, 8])
            .on_press_maybe(if has_matches { Some(Event::SearchNext) } else { None });

        let close_btn = button(text("x").size(font))
            .style(button::text)
            .padding([4, 8])
            .on_press(Event::SearchClose);

        let bar_bg = theme.bg_overlay();
        container(
            row![
                search_input,
                text(match_display).size(font_small).color(theme.text_secondary()),
                prev_btn,
                next_btn,
                iced::widget::Space::new().width(Length::Fill),
                text("Esc: close  Cmd+G: next  Cmd+Shift+G: prev")
                    .size(font_small)
                    .color(theme.text_muted()),
                close_btn,
            ]
            .spacing(8)
            .padding(8)
            .align_y(iced::Alignment::Center),
        )
        .width(Length::Fill)
        .style(move |_| container::Style {
            background: Some(bar_bg.into()),
            ..Default::default()
        })
        .into()
    }
}
