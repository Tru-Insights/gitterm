//! The Work page's Issues section (TRU-203): one line per issue key, with
//! its title, state and a hover card, the line opening the issue's URL in the
//! shared Viewer.
//! The pure parts and the `linear` CLI calls live in
//! `gitterm::issue_titles`.
//!
//! Titles are read in the background, one round at a time, started from
//! `Tick` while the section shows and some key is due
//! (`issue_titles::needs_fetch`), and applied by [`IssueEvent::Fetched`];
//! `update()` never waits on the CLI. A key whose read failed waits
//! [`FAILED_RETRY`] before it is tried again. Without a `linear` binary
//! nothing is read (logged once).

use super::*;

use gitterm::issue_titles::{self, IssueTitlesFile, KeyResult, RoundOutcome};

/// A key whose read failed (not "not found") is tried again after this.
const FAILED_RETRY: Duration = Duration::from_secs(10 * 60);
/// Titles are cut to this many characters on the line; the hover card
/// shows them whole.
const TITLE_CHARS: usize = 60;

#[derive(Debug, Clone)]
pub enum IssueEvent {
    Fetched(Box<RoundOutcome>),
    Saved(Result<(), String>),
    /// "▸ n more" / "▾ show fewer".
    AllToggle,
    /// Open an issue's URL (from the cache) in the Viewer.
    Open(String),
}

#[derive(Debug, Default)]
pub struct IssuesState {
    /// `issue-titles.json`.
    pub cache: IssueTitlesFile,
    /// The `linear` binary, once looked for (`Some(None)`: not found).
    linear: Option<Option<PathBuf>>,
    in_flight: bool,
    /// Keys whose last read failed, and when.
    failed: HashMap<String, Instant>,
    save_in_flight: bool,
    save_again: bool,
    /// Issues lists every key, not just the first
    /// [`issue_titles::ISSUES_SHOWN`].
    pub all: bool,
}

impl IssuesState {
    /// The cache from the config dir (a small read).
    pub fn load(config_dir: &Path) -> Self {
        Self {
            cache: IssueTitlesFile::load(&IssueTitlesFile::path_in(config_dir)),
            ..Self::default()
        }
    }

    /// The `linear` binary, looked for once; its absence is logged once.
    fn linear_binary(&mut self) -> Option<PathBuf> {
        self.linear
            .get_or_insert_with(|| {
                let found = issue_titles::find_linear(std::env::var_os("PATH").as_deref());
                if found.is_none() {
                    eprintln!(
                        "[issue-titles] no `linear` CLI on PATH, /opt/homebrew/bin or \
                         /usr/local/bin; issue titles are not read"
                    );
                }
                found
            })
            .clone()
    }
}

impl App {
    /// The keys the Issues section shows now: the active workspace's
    /// overview while it is visible.
    fn shown_issue_keys(&self) -> Option<Vec<String>> {
        if !self.workspace_overview_visible() {
            return None;
        }
        let key = self.overview_key()?;
        let keys = &self.workspace_overviews.get(&key)?.summary.issue_keys;
        (!keys.is_empty()).then(|| keys.clone())
    }

    /// Tick: one background round for the shown keys that are due, unless
    /// one runs.
    pub(super) fn poll_issue_titles(&mut self) -> Task<Event> {
        if self.issues.in_flight {
            return Task::none();
        }
        let Some(keys) = self.shown_issue_keys() else {
            return Task::none();
        };
        let now = chrono::Utc::now();
        let mut due: Vec<String> = Vec::new();
        for key in keys {
            if due.len() >= issue_titles::ROUND_MAX_KEYS {
                break;
            }
            let failed_recently = self
                .issues
                .failed
                .get(&key)
                .is_some_and(|at| at.elapsed() < FAILED_RETRY);
            if !failed_recently
                && !due.contains(&key)
                && issue_titles::needs_fetch(self.issues.cache.issues.get(&key), now)
            {
                due.push(key);
            }
        }
        if due.is_empty() {
            return Task::none();
        }
        let Some(binary) = self.issues.linear_binary() else {
            return Task::none();
        };
        self.issues.in_flight = true;
        let request = issue_titles::RoundRequest {
            binary,
            keys: due,
            prefixes: self.issues.cache.prefixes.clone(),
        };
        Task::perform(issue_titles::fetch_round(request), |outcome| {
            Event::Issue(IssueEvent::Fetched(Box::new(outcome)))
        })
    }

    fn apply_issue_round(&mut self, outcome: RoundOutcome) -> Task<Event> {
        self.issues.in_flight = false;
        // Failures logged once per distinct error, with their keys.
        let mut failures: Vec<(&str, Vec<&str>)> = Vec::new();
        for (key, result) in &outcome.results {
            match result {
                KeyResult::Failed(error) => {
                    self.issues.failed.insert(key.clone(), Instant::now());
                    match failures.iter_mut().find(|(known, _)| *known == error) {
                        Some((_, keys)) => keys.push(key),
                        None => failures.push((error, vec![key])),
                    }
                }
                KeyResult::NotFound => {
                    self.issues.failed.remove(key);
                    eprintln!("[issue-titles] {key}: no linear workspace knows it");
                }
                KeyResult::Found { .. } => {
                    self.issues.failed.remove(key);
                }
            }
        }
        for (error, keys) in failures {
            eprintln!("[issue-titles] {}: not read: {error}", keys.join(", "));
        }
        let before = self.issues.cache.clone();
        self.issues.cache.absorb(&outcome, &chat_facts::now_stamp());
        if self.issues.cache == before {
            return Task::none();
        }
        self.save_issue_titles()
    }

    /// Writes the cache off the UI thread, one write at a time.
    fn save_issue_titles(&mut self) -> Task<Event> {
        if self.issues.save_in_flight {
            self.issues.save_again = true;
            return Task::none();
        }
        self.issues.save_in_flight = true;
        let file = self.issues.cache.clone();
        let path = IssueTitlesFile::path_in(&config::global_config_dir());
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || file.save(&path))
                    .await
                    .unwrap_or_else(|error| Err(format!("issue titles save task failed: {error}")))
            },
            |result| Event::Issue(IssueEvent::Saved(result)),
        )
    }

    pub(super) fn update_issues(&mut self, event: IssueEvent) -> Task<Event> {
        match event {
            IssueEvent::Fetched(outcome) => return self.apply_issue_round(*outcome),
            IssueEvent::Saved(result) => {
                self.issues.save_in_flight = false;
                if let Err(error) = result {
                    eprintln!("[issue-titles] cache not saved: {error}");
                }
                if self.issues.save_again {
                    self.issues.save_again = false;
                    return self.save_issue_titles();
                }
            }
            IssueEvent::AllToggle => self.issues.all = !self.issues.all,
            IssueEvent::Open(url) => {
                return self.open_viewer_page(url);
            }
        }
        Task::none()
    }

    /// The Issues section's rows: key, title (cut) and state per line, the
    /// first [`issue_titles::ISSUES_SHOWN`] then "▸ n more". Without a
    /// Linear entry the state slot says why ("not read yet", ...). A line
    /// whose URL is cached opens it on a click; there is no hover card.
    pub(super) fn view_work_issues<'a>(
        &'a self,
        keys: &'a [String],
    ) -> Element<'a, Event, Theme, iced::Renderer> {
        let theme = &self.theme;
        let mono = iced::Font::with_name("Menlo");
        let font_small = self.ui_font_small();
        let text_primary = theme.text_primary();
        let text_secondary = theme.text_secondary();
        let text_muted = theme.text_muted();
        let linear_missing = matches!(self.issues.linear, Some(None));
        let (shown, fold_label) = issue_titles::fold(keys.len(), self.issues.all);
        let mut col = Column::new().spacing(1);
        for key in keys.iter().take(shown) {
            let cached = self.issues.cache.issues.get(key);
            let found = cached.filter(|entry| !entry.not_found);
            let title = found.and_then(|entry| entry.title.as_deref());
            // The muted slot: the issue's state, or why there is none
            // (TRU-208; this used to be only in a hover card).
            let state = match (found, cached) {
                (Some(entry), _) => entry.state.clone().unwrap_or_default(),
                (None, Some(_)) => "no Linear workspace knows this key".to_string(),
                (None, None) if linear_missing => "no linear CLI".to_string(),
                (None, None) => "not read yet".to_string(),
            };
            let url = found.and_then(|entry| entry.url.clone());
            let line = row![
                text(key.clone())
                    .size(font_small)
                    .color(text_secondary)
                    .font(mono),
                text(title.map_or_else(String::new, |title| {
                    issue_titles::shorten(title, TITLE_CHARS)
                }))
                .size(font_small)
                .color(text_primary)
                .width(Length::Fill),
                text(state).size(10).color(text_muted),
            ]
            .spacing(6)
            .align_y(iced::Alignment::Center);
            let line: Element<'a, Event, Theme, iced::Renderer> = match url {
                Some(url) => button(line)
                    .style(self.flat_button_style())
                    .padding([3, 6])
                    .width(Length::Fill)
                    .on_press(Event::Issue(IssueEvent::Open(url)))
                    .into(),
                None => container(line).padding([3, 6]).width(Length::Fill).into(),
            };
            col = col.push(line);
        }
        if let Some(label) = fold_label {
            col = col.push(
                button(text(label).size(10).color(text_muted).font(mono))
                    .style(self.flat_button_style())
                    .padding([2, 6])
                    .on_press(Event::Issue(IssueEvent::AllToggle)),
            );
        }
        col.into()
    }
}
