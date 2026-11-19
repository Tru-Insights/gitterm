//! Bounded background services. Copy a coherent grid in short lock slices, then
//! run Alacritty's search/text APIs on the detached terminal, away from parsing.
use crate::{Backend, SearchMatch, backend::escape_regex, events::EventProxy};
use alacritty_terminal::{
    event::VoidListener,
    grid::Dimensions,
    index::{Column, Direction, Line, Point},
    sync::FairMutex,
    term::{
        Config, Term,
        search::{RegexIter, RegexSearch},
        test::TermSize,
    },
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceError {
    Busy,
    InvalidPattern,
    NoExportCallback,
    WorkerStopped,
    Spawn(String),
}

pub enum JobState<T> {
    Pending,
    Ready(Result<T, ServiceError>),
}

/// One result, delivered without blocking. Poll only until Ready.
pub struct ServiceJob<T>(mpsc::Receiver<Result<T, ServiceError>>);
impl<T> ServiceJob<T> {
    pub fn poll(&self) -> JobState<T> {
        match self.0.try_recv() {
            Ok(value) => JobState::Ready(value),
            Err(mpsc::TryRecvError::Empty) => JobState::Pending,
            Err(mpsc::TryRecvError::Disconnected) => {
                JobState::Ready(Err(ServiceError::WorkerStopped))
            }
        }
    }
}

pub struct SearchResults {
    pub revision: u64,
    pub matches: Vec<SearchMatch>,
}

pub(crate) fn snapshot(
    source: &FairMutex<Term<EventProxy>>,
    events: &EventProxy,
    last_lines: usize,
) -> Result<(Term<VoidListener>, u64), ServiceError> {
    let (revision, columns, rows, count, bottom) = {
        let term = source.try_lock_unfair().ok_or(ServiceError::Busy)?;
        let count = last_lines.min(term.total_lines());
        (
            events.revision(),
            term.columns(),
            term.screen_lines().min(count).max(1),
            count,
            term.bottommost_line(),
        )
    };
    let mut copy = Term::new(
        Config {
            scrolling_history: count.saturating_sub(rows),
            ..Default::default()
        },
        &TermSize::new(columns, rows),
        VoidListener,
    );
    copy.grid_mut().initialize_all();
    let source_top = bottom.0 + 1 - count as i32;
    let target_top = copy.topmost_line().0;
    for offset in (0..count).step_by(32) {
        let batch = {
            let term = source.try_lock_unfair().ok_or(ServiceError::Busy)?;
            if events.revision() != revision
                || term.columns() != columns
                || term.bottommost_line() != bottom
            {
                return Err(ServiceError::Busy);
            }
            (offset..(offset + 32).min(count))
                .map(|i| term.grid()[Line(source_top + i as i32)].clone())
                .collect::<Vec<_>>()
        };
        for (i, row) in batch.into_iter().enumerate() {
            copy.grid_mut()[Line(target_top + (offset + i) as i32)] = row;
        }
        std::thread::yield_now();
    }
    Ok((copy, revision))
}

impl Backend {
    fn service<T: Send + 'static>(
        &self,
        work: impl FnOnce(Term<VoidListener>, u64) -> Result<T, ServiceError> + Send + 'static,
    ) -> Result<ServiceJob<T>, ServiceError> {
        if self
            .service_busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(ServiceError::Busy);
        }
        let busy = self.service_busy.clone();
        let source = self.term.clone();
        let events = self.events.clone();
        let (send, recv) = mpsc::sync_channel(1);
        let result = std::thread::Builder::new()
            .name("terminal-service".into())
            .spawn(move || {
                struct Release(Arc<AtomicBool>);
                impl Drop for Release {
                    fn drop(&mut self) {
                        self.0.store(false, Ordering::Release);
                    }
                }
                let release = Release(busy);
                let result = snapshot(&source, &events, usize::MAX)
                    .and_then(|(term, revision)| work(term, revision));
                drop(release);
                let _ = send.send(result);
            });
        if let Err(error) = result {
            self.service_busy.store(false, Ordering::Release);
            return Err(ServiceError::Spawn(error.to_string()));
        }
        Ok(ServiceJob(recv))
    }

    /// Literal search. Admission and parser contention report Busy, never zero matches.
    pub fn search_job(&self, pattern: String) -> Result<ServiceJob<SearchResults>, ServiceError> {
        self.service(move |term, revision| {
            let mut matches = Vec::new();
            if !pattern.is_empty() {
                let mut regex = RegexSearch::new(&escape_regex(&pattern))
                    .map_err(|_| ServiceError::InvalidPattern)?;
                for found in RegexIter::new(
                    Point::new(term.topmost_line(), Column(0)),
                    Point::new(term.bottommost_line(), term.last_column()),
                    Direction::Right,
                    &term,
                    &mut regex,
                ) {
                    matches.push(SearchMatch {
                        start: *found.start(),
                        end: *found.end(),
                    });
                }
            }
            Ok(SearchResults { revision, matches })
        })
    }

    /// Plain text only; the app chooses where to save it. No file IO occurs here.
    pub fn export_job(&self) -> Result<ServiceJob<String>, ServiceError> {
        self.service(|term, _| Ok(Self::all_text(&term)))
    }

    /// Last N physical grid lines (including blank viewport lines), without rendering.
    /// Locks at most 32 rows at a time; concurrent mutation returns Busy so callers
    /// never mistake a torn snapshot for a coherent log. Work scales with N, not history.
    pub fn tail_snapshot(&self, lines: usize) -> Result<String, ServiceError> {
        if lines == 0 {
            return Ok(String::new());
        }
        let (term, _) = snapshot(&self.term, &self.events, lines)?;
        Ok(Self::all_text(&term))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BackendSettings, test_support::wait};
    use alacritty_terminal::{selection::SelectionType, vte::ansi::Processor};

    fn backend(history: usize) -> Backend {
        let mut backend = Backend::new_deferred(BackendSettings::default());
        backend.resize_cells(16, 2);
        backend.set_history_limit(history);
        assert!(backend.sync());
        backend
    }
    fn feed(backend: &Backend, bytes: &[u8]) {
        let mut term = backend.term.lock_unfair();
        let mut parser: Processor = Processor::new();
        parser.advance(&mut *term, bytes);
        backend.events.invalidate();
    }
    fn finish<T>(job: &ServiceJob<T>) -> Result<T, ServiceError> {
        let mut result = None;
        wait(|| {
            if let JobState::Ready(value) = job.poll() {
                result = Some(value);
            }
            result.is_some()
        });
        result.unwrap()
    }

    #[test]
    fn background_literal_search_100k_and_contention() {
        let mut backend = backend(100_000);
        feed(&backend, b"row\r\n".repeat(200).as_slice());
        feed(&backend, b"keep[.]\r\n");
        feed(&backend, b"row\r\n".repeat(99_900).as_slice());
        feed(&backend, b"needle[.]\r\nneedle[.]");
        assert_eq!(backend.term.lock_unfair().grid().history_size(), 100_000);
        let result = finish(&backend.search_job("needle[.]".into()).unwrap()).unwrap();
        assert_eq!(result.matches.len(), 2);
        assert!(backend.set_find_results(result));
        let first = backend.last_content.find_match.as_ref().unwrap().start;
        assert!(backend.next_find(false));
        assert_ne!(
            backend.last_content.find_match.as_ref().unwrap().start,
            first
        );
        assert!(backend.next_find(true));
        assert_eq!(
            backend.last_content.find_match.as_ref().unwrap().start,
            first
        );

        let history = finish(&backend.search_job("keep[.]".into()).unwrap()).unwrap();
        assert_eq!(history.matches.len(), 1);
        assert!(backend.set_find_results(history));
        assert!(backend.last_content.display_offset > 99_000);
        let source = backend.term.clone();
        let _lock = source.lock_unfair();
        let job = backend.search_job("row".into()).unwrap();
        assert!(matches!(finish(&job), Err(ServiceError::Busy)));
        assert!(matches!(backend.tail_snapshot(2), Err(ServiceError::Busy)));
        // No foreground wait on the parser for scroll/highlight either.
        assert!(!backend.next_find(false));
    }

    #[test]
    fn background_export_includes_first_cell_wrap_and_unicode() {
        let backend = backend(4);
        feed(&backend, "A界e\u{301}\r\nsecond\r\nthird".as_bytes());
        let text = finish(&backend.export_job().unwrap()).unwrap();
        assert_eq!(text, "A界e\u{301}\nsecond\nthird");
        feed(&backend, b"\r\n12345678901234567");
        let wrapped = finish(&backend.export_job().unwrap()).unwrap();
        assert!(wrapped.ends_with("third\n12345678901234567"));
        // Explicit bounded admission, including when no receiver is retained.
        backend.service_busy.store(true, Ordering::Release);
        assert!(matches!(backend.export_job(), Err(ServiceError::Busy)));
        assert!(matches!(
            backend.search_job("x".into()),
            Err(ServiceError::Busy)
        ));
        backend.service_busy.store(false, Ordering::Release);
    }

    #[test]
    fn tail_reads_only_last_n_lines_without_rendering() {
        let backend = backend(100_000);
        feed(&backend, b"old\r\n".repeat(10_000).as_slice());
        feed(&backend, "A界e\u{301}\r\nlast".as_bytes());
        assert_eq!(backend.tail_snapshot(0).unwrap(), "");
        assert_eq!(backend.tail_snapshot(1).unwrap(), "last");
        assert_eq!(backend.tail_snapshot(2).unwrap(), "A界e\u{301}\nlast");
        assert_eq!(
            backend.tail_snapshot(usize::MAX).unwrap(),
            Backend::all_text(&*backend.term.lock_unfair())
        );
        // The render snapshot was never synchronized with the new output.
        assert!(
            backend
                .last_content
                .cells
                .iter()
                .all(|cell| cell.cell.c == ' ')
        );
    }

    #[test]
    fn eviction_invalidates_matches_and_rotates_or_clears_selection() {
        let mut backend = backend(2);
        feed(&backend, b"old\r\nneedle\r\nend");
        assert!(backend.sync());
        let old = finish(&backend.search_job("needle".into()).unwrap()).unwrap();
        assert!(backend.set_find_results(old));
        let point = backend.last_content.find_match.as_ref().unwrap().start;
        backend.select(point, SelectionType::Simple, false);
        backend.extend_selection(Point::new(point.line, Column(5)));
        assert!(backend.sync());
        assert_eq!(backend.selectable_content().unwrap(), "needle");
        feed(&backend, b"\r\nnew1");
        assert!(backend.sync());
        assert!(backend.last_content.find_match.is_none());
        assert_eq!(backend.selectable_content().unwrap(), "needle");
        // Pending coordinates from an old render may not select replacement text.
        backend.select(point, SelectionType::Simple, false);
        feed(&backend, b"\r\nnew2\r\nnew3\r\nnew4");
        assert!(backend.sync());
        assert!(backend.selectable_content().unwrap().is_empty());
        assert!(!backend.next_find(false));
        let fresh = finish(&backend.search_job("needle".into()).unwrap()).unwrap();
        assert!(fresh.matches.is_empty());
    }
}
