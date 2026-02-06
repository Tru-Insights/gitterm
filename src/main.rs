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
}

fn default_terminal_font() -> f32 { 14.0 }
fn default_ui_font() -> f32 { 13.0 }
fn default_sidebar_width() -> f32 { 280.0 }
fn default_scrollback_lines() -> usize { 100_000 }

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
}

struct App {
    title: String,
    tabs: Vec<TabState>,
    active_tab: usize,
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
        };
        config.save();
    }

    /// Update the log server with current terminal content
    fn update_log_server(&self) {
        let state = self.log_server_state.clone();
        let mut terminal_snapshots = std::collections::HashMap::new();
        let mut file_snapshots = std::collections::HashMap::new();

        // Collect terminal content and file content from all tabs
        for tab in &self.tabs {
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
            tabs: Vec::new(),
            active_tab: 0,
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
        };

        // Open home directory by default
        let home = std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or(cwd);
        app.add_tab(home);

        // Return a task to initialize the menu bar after the app starts
        (app, Task::done(Event::InitMenu))
    }

    fn add_tab(&mut self, repo_path: PathBuf) {
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
        self.tabs.push(tab);
        self.active_tab = self.tabs.len() - 1;
    }

    fn active_tab(&self) -> Option<&TabState> {
        self.tabs.get(self.active_tab)
    }

    fn active_tab_mut(&mut self) -> Option<&mut TabState> {
        self.tabs.get_mut(self.active_tab)
    }

    fn title(&self) -> String {
        if let Some(tab) = self.tabs.get(self.active_tab) {
            format!("{} - {}", self.title, tab.repo_name)
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
                _ => None,
            }),
        ];

        for tab in &self.tabs {
            if let Some(term) = &tab.terminal {
                subs.push(
                    term.subscription()
                        .with(tab.id)
                        .map(|(tab_id, e)| Event::Terminal(tab_id, e)),
                );
            }
        }

        Subscription::batch(subs)
    }

    fn update(&mut self, event: Event) -> Task<Event> {
        match event {
            Event::Terminal(tab_id, iced_term::Event::BackendCall(_, cmd)) => {
                if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
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
            }
            Event::TabSelect(idx) => {
                // Hide WebView when switching tabs
                webview::set_visible(false);
                if idx < self.tabs.len() {
                    self.active_tab = idx;
                }
            }
            Event::TabClose(idx) => {
                // Hide WebView when closing tabs
                webview::set_visible(false);
                if idx < self.tabs.len() && self.tabs.len() > 1 {
                    self.tabs.remove(idx);
                    if self.active_tab >= self.tabs.len() {
                        self.active_tab = self.tabs.len() - 1;
                    }
                }
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
                            if num >= 1 && num <= 9 && num <= self.tabs.len() {
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
            }
            Event::MouseMoved(x, _y) => {
                if self.dragging_divider {
                    // Clamp sidebar width between 150 and 600 pixels
                    self.sidebar_width = x.clamp(150.0, 600.0);

                    // Update WebView bounds if active
                    if webview::is_active() {
                        let bounds = self.calculate_webview_bounds();
                        webview::update_bounds(bounds.0, bounds.1, bounds.2, bounds.3);
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
                        let url = format!("http://localhost:3030/file/{}", tab.id);
                        let _ = std::process::Command::new("open")
                            .arg(&url)
                            .spawn();
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
            Event::WindowResized(width, height) => {
                self.window_size = (width, height);

                // Update WebView bounds if active
                if webview::is_active() {
                    let bounds = self.calculate_webview_bounds();
                    webview::update_bounds(bounds.0, bounds.1, bounds.2, bounds.3);
                }
            }
        }
        Task::none()
    }

    /// Calculate WebView bounds based on current layout
    fn calculate_webview_bounds(&self) -> (f32, f32, f32, f32) {
        let tab_bar_height = 40.0;
        let header_height = 45.0;
        let x = self.sidebar_width + 4.0; // sidebar + divider
        let y = tab_bar_height + header_height;
        let width = (self.window_size.0 - x).max(100.0);
        let height = (self.window_size.1 - y).max(100.0);
        (x, y, width, height)
    }

    fn recreate_terminals(&mut self) {
        for tab in &mut self.tabs {
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
        let tab_bar = self.view_tab_bar();
        let content = self.view_content();

        column![tab_bar, content]
            .spacing(0)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn view_tab_bar(&self) -> Element<'_, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        let mut tabs_row = Row::new().spacing(4);

        for (idx, tab) in self.tabs.iter().enumerate() {
            let is_active = idx == self.active_tab;
            let changes = tab.total_changes();

            // Use terminal title if available, otherwise repo name
            let base_title = tab
                .terminal_title
                .as_ref()
                .map(|t| {
                    // Truncate long titles
                    if t.len() > 25 {
                        format!("{}...", &t[..22])
                    } else {
                        t.clone()
                    }
                })
                .unwrap_or_else(|| tab.repo_name.clone());

            // Build tab label with shortcut
            let shortcut = if idx < 9 {
                format!("  \u{2318}{}", idx + 1)
            } else {
                String::new()
            };

            let tab_label = if changes > 0 {
                format!("{} ({}){}", base_title, changes, shortcut)
            } else {
                format!("{}{}", base_title, shortcut)
            };

            let tab_btn = button(text(tab_label).size(13))
                .style(if is_active {
                    button::primary
                } else {
                    button::secondary
                })
                .padding([6, 12])
                .on_press(Event::TabSelect(idx));

            let close_btn = button(text("x").size(13).color(theme.text_secondary()))
                .style(button::text)
                .padding([6, 8])
                .on_press(Event::TabClose(idx));

            tabs_row = tabs_row.push(row![tab_btn, close_btn].spacing(0));
        }

        let add_btn = button(text("+").size(14))
            .style(button::secondary)
            .padding([6, 12])
            .on_press(Event::OpenFolder);

        tabs_row = tabs_row.push(add_btn);

        let bg = theme.bg_base();
        container(tabs_row.padding(4).spacing(8).align_y(iced::Alignment::Center))
        .width(Length::Fill)
        .style(move |_| container::Style {
            background: Some(bg.into()),
            ..Default::default()
        })
        .into()
    }

    fn view_content(&self) -> Element<'_, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        if let Some(tab) = self.tabs.get(self.active_tab) {
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
        let git_style = if tab.sidebar_mode == SidebarMode::Git {
            button::primary
        } else {
            button::secondary
        };
        let files_style = if tab.sidebar_mode == SidebarMode::Files {
            button::primary
        } else {
            button::secondary
        };

        let changes = tab.total_changes();
        let git_label = if changes > 0 {
            format!("Git ({})", changes)
        } else {
            "Git".to_string()
        };

        let bg = theme.bg_base();
        let font = self.ui_font();
        container(
            row![
                button(text(git_label).size(font))
                    .style(git_style)
                    .padding([4, 12])
                    .on_press(Event::ToggleSidebarMode),
                button(text("Files").size(font))
                    .style(files_style)
                    .padding([4, 12])
                    .on_press(Event::ToggleSidebarMode),
            ]
            .spacing(4),
        )
        .padding(8)
        .width(Length::Fill)
        .style(move |_| container::Style {
            background: Some(bg.into()),
            ..Default::default()
        })
        .into()
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

        // Only show branch name if this is a git repo
        if Repository::open(&tab.repo_path).is_ok() {
            content = content.push(
                text(format!(" {}", tab.branch_name))
                    .size(self.ui_font())
                    .color(theme.accent()),
            );
        }

        if !tab.staged.is_empty() {
            content = content.push(
                text(format!("Staged ({})", tab.staged.len()))
                    .size(font)
                    .color(theme.success()),
            );
            for file in &tab.staged {
                content = content.push(self.view_file_item(file, tab));
            }
        }

        if !tab.unstaged.is_empty() {
            content = content.push(
                text(format!("Unstaged ({})", tab.unstaged.len()))
                    .size(font)
                    .color(theme.warning()),
            );
            for file in &tab.unstaged {
                content = content.push(self.view_file_item(file, tab));
            }
        }

        if !tab.untracked.is_empty() {
            content = content.push(
                text(format!("Untracked ({})", tab.untracked.len()))
                    .size(font)
                    .color(theme.text_secondary()),
            );
            for file in &tab.untracked {
                content = content.push(self.view_file_item(file, tab));
            }
        }

        if tab.staged.is_empty() && tab.unstaged.is_empty() && tab.untracked.is_empty() {
            // Check if this is actually a git repo
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
        // Delay showing terminal for 500ms after tab creation to let layout settle
        let ready = tab.created_at.elapsed() > Duration::from_millis(500);

        let bg = theme.bg_base();
        let terminal_view: Element<'a, Event, Theme, iced::Renderer> = if let Some(term) = &tab.terminal {
            if ready {
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
                // Show loading state while terminal initializes
                container(text("").size(14))
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .style(move |_| container::Style {
                        background: Some(bg.into()),
                        ..Default::default()
                    })
                    .into()
            }
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
