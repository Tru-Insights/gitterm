use git2::{DiffOptions, Repository, Status, StatusOptions};
use iced::advanced::graphics::core::Element;
use iced::keyboard::{self, key, Key, Modifiers};
use iced::widget::{button, column, container, row, scrollable, text, Column, Row};
use iced::{color, Length, Size, Subscription, Task, Theme};
use iced_term::TerminalView;
use similar::{ChangeTag, TextDiff};
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn main() -> iced::Result {
    iced::application(App::new, App::update, App::view)
        .title(App::title)
        .window_size(Size {
            width: 1400.0,
            height: 800.0,
        })
        .subscription(App::subscription)
        .run()
}

// Git file entry
#[derive(Debug, Clone)]
struct FileEntry {
    path: String,
    status: String,
    is_staged: bool,
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
}

impl TabState {
    fn new(id: usize, repo_path: PathBuf) -> Self {
        let repo_name = repo_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "repo".to_string());

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
    TabSelect(usize),
    TabClose(usize),
    OpenFolder,
    FolderSelected(Option<PathBuf>),
    FileSelect(String, bool),
    FileSelectByIndex(i32),
    ClearSelection,
    KeyPressed(Key, Modifiers),
}

struct App {
    title: String,
    tabs: Vec<TabState>,
    active_tab: usize,
    next_tab_id: usize,
}

impl App {
    fn new() -> (Self, Task<Event>) {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

        let mut app = Self {
            title: String::from("Cree8 Claude Git IDE"),
            tabs: Vec::new(),
            active_tab: 0,
            next_tab_id: 0,
        };

        if Repository::open(&cwd).is_ok() {
            app.add_tab(cwd);
        }

        (app, Task::none())
    }

    fn add_tab(&mut self, repo_path: PathBuf) {
        let id = self.next_tab_id;
        self.next_tab_id += 1;

        let mut tab = TabState::new(id, repo_path.clone());

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string());
        let term_settings = iced_term::settings::Settings {
            backend: iced_term::settings::BackendSettings {
                program: shell,
                working_directory: Some(repo_path),
                ..Default::default()
            },
            ..Default::default()
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
            iced::time::every(Duration::from_millis(2500)).map(|_| Event::Tick),
            iced::event::listen_with(|event, _status, _id| {
                if let iced::Event::Keyboard(keyboard::Event::KeyPressed {
                    key,
                    modifiers,
                    ..
                }) = event
                {
                    Some(Event::KeyPressed(key, modifiers))
                } else {
                    None
                }
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
                                tab.terminal_title = Some(title);
                            }
                            _ => {}
                        }
                    }
                }
            }
            Event::Tick => {
                // Poll git status for active tab
                if let Some(tab) = self.active_tab_mut() {
                    if tab.last_poll.elapsed() >= Duration::from_millis(2500) {
                        tab.fetch_status();
                    }
                }
            }
            Event::TabSelect(idx) => {
                if idx < self.tabs.len() {
                    self.active_tab = idx;
                }
            }
            Event::TabClose(idx) => {
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
                            .set_title("Select Git Repository")
                            .pick_folder()
                            .await;
                        folder.map(|f| f.path().to_path_buf())
                    },
                    Event::FolderSelected,
                );
            }
            Event::FolderSelected(Some(path)) => {
                if Repository::open(&path).is_ok() {
                    self.add_tab(path);
                }
            }
            Event::FolderSelected(None) => {}
            Event::FileSelect(path, is_staged) => {
                if let Some(tab) = self.active_tab_mut() {
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
                if let Some(tab) = self.active_tab_mut() {
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
                // Only handle keys when not viewing terminal (diff panel is visible)
                if let Some(tab) = self.active_tab() {
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
                if modifiers.command() {
                    if let Key::Character(c) = key.as_ref() {
                        if let Ok(num) = c.parse::<usize>() {
                            if num >= 1 && num <= 9 && num <= self.tabs.len() {
                                return Task::done(Event::TabSelect(num - 1));
                            }
                        }
                    }
                }
            }
        }
        Task::none()
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
        let mut tabs_row = Row::new().spacing(2);

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

            let close_btn = button(text("x").size(13))
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

        container(tabs_row.padding(4))
            .width(Length::Fill)
            .style(|_| container::Style {
                background: Some(color!(0x1e1e2e).into()),
                ..Default::default()
            })
            .into()
    }

    fn view_content(&self) -> Element<'_, Event, Theme, iced::Renderer> {
        if let Some(tab) = self.tabs.get(self.active_tab) {
            let file_list = self.view_file_list(tab);

            let main_panel = if tab.selected_file.is_some() {
                self.view_diff_panel(tab)
            } else {
                self.view_terminal(tab)
            };

            row![file_list, main_panel]
                .spacing(0)
                .width(Length::Fill)
                .height(Length::Fill)
                .into()
        } else {
            container(
                column![
                    text("No repository open").size(16),
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
            .into()
        }
    }

    fn view_file_list<'a>(
        &'a self,
        tab: &'a TabState,
    ) -> Element<'a, Event, Theme, iced::Renderer> {
        let mut content = Column::new().spacing(8).padding(12);

        content = content.push(
            text(format!(" {}", tab.branch_name))
                .size(14)
                .color(color!(0x89b4fa)),
        );

        if !tab.staged.is_empty() {
            content = content.push(
                text(format!("Staged ({})", tab.staged.len()))
                    .size(12)
                    .color(color!(0xa6e3a1)),
            );
            for file in &tab.staged {
                content = content.push(self.view_file_item(file, tab));
            }
        }

        if !tab.unstaged.is_empty() {
            content = content.push(
                text(format!("Unstaged ({})", tab.unstaged.len()))
                    .size(12)
                    .color(color!(0xf9e2af)),
            );
            for file in &tab.unstaged {
                content = content.push(self.view_file_item(file, tab));
            }
        }

        if !tab.untracked.is_empty() {
            content = content.push(
                text(format!("Untracked ({})", tab.untracked.len()))
                    .size(12)
                    .color(color!(0x6c7086)),
            );
            for file in &tab.untracked {
                content = content.push(self.view_file_item(file, tab));
            }
        }

        if tab.staged.is_empty() && tab.unstaged.is_empty() && tab.untracked.is_empty() {
            content = content.push(text("No changes").size(13).color(color!(0x6c7086)));
        }

        container(scrollable(content).height(Length::Fill))
            .width(Length::Fixed(280.0))
            .height(Length::Fill)
            .style(|_| container::Style {
                background: Some(color!(0x181825).into()),
                ..Default::default()
            })
            .into()
    }

    fn view_file_item<'a>(
        &'a self,
        file: &'a FileEntry,
        tab: &'a TabState,
    ) -> Element<'a, Event, Theme, iced::Renderer> {
        let status_color = match file.status.as_str() {
            "A" => color!(0xa6e3a1),
            "M" => color!(0xf9e2af),
            "D" => color!(0xf38ba8),
            "R" => color!(0x89b4fa),
            _ => color!(0x6c7086),
        };

        let is_selected = tab.selected_file.as_ref() == Some(&file.path);
        let text_color = if is_selected {
            color!(0xffffff)
        } else {
            color!(0xcdd6f4)
        };

        let file_row = row![
            text(&file.status)
                .size(12)
                .color(status_color)
                .width(Length::Fixed(20.0)),
            text(&file.path).size(12).color(text_color),
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
        let mut content = Column::new().spacing(0);

        // Header
        let header = row![
            text(tab.selected_file.as_deref().unwrap_or(""))
                .size(13)
                .color(color!(0xcdd6f4)),
            iced::widget::Space::new().width(Length::Fill),
            text("j/k: navigate  Esc: back")
                .size(11)
                .color(color!(0x6c7086)),
            iced::widget::Space::new().width(Length::Fixed(16.0)),
            button(text("Back to Terminal").size(12))
                .style(button::secondary)
                .padding([4, 8])
                .on_press(Event::ClearSelection),
        ]
        .padding(8)
        .spacing(8);

        content = content.push(
            container(header).width(Length::Fill).style(|_| container::Style {
                background: Some(color!(0x313244).into()),
                ..Default::default()
            }),
        );

        // Diff content
        let mut diff_column = Column::new().spacing(0);

        if tab.diff_lines.is_empty() {
            diff_column =
                diff_column.push(text("No diff available").size(12).color(color!(0x6c7086)));
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

        container(content)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(|_| container::Style {
                background: Some(color!(0x1e1e2e).into()),
                ..Default::default()
            })
            .into()
    }

    fn view_diff_line<'a>(&'a self, line: &'a DiffLine) -> Element<'a, Event, Theme, iced::Renderer> {
        let (line_color, bg_color) = match line.line_type {
            DiffLineType::Addition => (color!(0xa6e3a1), Some(color!(0x1a3a1a))),
            DiffLineType::Deletion => (color!(0xf38ba8), Some(color!(0x3a1a1a))),
            DiffLineType::Header => (color!(0x89b4fa), None),
            DiffLineType::Context => (color!(0x6c7086), None),
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
                            (color!(0xffffff), Some(color!(0x8b3a3a)))
                        }
                        (DiffLineType::Addition, ChangeType::Insert) => {
                            (color!(0xffffff), Some(color!(0x3a6b3a)))
                        }
                        _ => (line_color, None),
                    };

                    let change_text = text(&change.value)
                        .size(12)
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
                    .size(12)
                    .color(line_color)
                    .font(iced::Font::MONOSPACE)
                    .into()
            };

        let line_row = if line.line_type == DiffLineType::Header {
            row![content_element].spacing(0)
        } else {
            row![
                text(old_num)
                    .size(12)
                    .color(color!(0x45475a))
                    .font(iced::Font::MONOSPACE),
                text(new_num)
                    .size(12)
                    .color(color!(0x45475a))
                    .font(iced::Font::MONOSPACE),
                text(prefix)
                    .size(12)
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
        // Delay showing terminal for 500ms after tab creation to let layout settle
        let ready = tab.created_at.elapsed() > Duration::from_millis(500);

        if let Some(term) = &tab.terminal {
            if ready {
                let tab_id = tab.id;
                container(TerminalView::show(term).map(move |e| Event::Terminal(tab_id, e)))
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .style(|_| container::Style {
                        background: Some(color!(0x1e1e2e).into()),
                        ..Default::default()
                    })
                    .into()
            } else {
                // Show loading state while terminal initializes
                container(text("").size(14))
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .style(|_| container::Style {
                        background: Some(color!(0x1e1e2e).into()),
                        ..Default::default()
                    })
                    .into()
            }
        } else {
            container(text("Terminal unavailable").size(14))
                .width(Length::Fill)
                .height(Length::Fill)
                .center_x(Length::Fill)
                .center_y(Length::Fill)
                .into()
        }
    }
}
