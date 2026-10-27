//! Open-file watch: how close GitTerm is to its open-file ceiling.
//!
//! Every terminal, chat process and connection holds file descriptors, and
//! a process may hold only as many as its soft `RLIMIT_NOFILE` (inherited
//! from whatever launched it; 256 for apps started from Finder or
//! Terminal). At the ceiling a new terminal or session fails to open
//! until something is closed. The app samples its usage on its tick and
//! appends a line to `resource.log` in the config dir whenever usage moves
//! into another band, and whenever opening a terminal fails, so the next
//! time a tab will not open the log says whether this was the cause.

use std::io::Write as _;
use std::path::Path;

/// The log file under the GitTerm config dir.
pub const LOG_FILE: &str = "resource.log";

/// Band edges, in percent of the soft limit.
const BAND_EDGES: [u64; 3] = [50, 75, 90];

/// Open descriptors and the soft limit on them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FdUsage {
    pub open: usize,
    /// `None` when unlimited or unknown.
    pub soft_limit: Option<u64>,
}

impl FdUsage {
    /// 0 below 50%, 1 from 50%, 2 from 75%, 3 from 90% of the limit.
    pub fn band(&self) -> usize {
        let Some(limit) = self.soft_limit.filter(|limit| *limit > 0) else {
            return 0;
        };
        let percent = self.open as u64 * 100 / limit;
        BAND_EDGES.iter().filter(|edge| percent >= **edge).count()
    }

    /// "123 of 256 open files (48%)".
    pub fn describe(&self) -> String {
        match self.soft_limit {
            Some(limit) if limit > 0 => format!(
                "{} of {} open files ({}%)",
                self.open,
                limit,
                self.open as u64 * 100 / limit
            ),
            _ => format!("{} open files (no limit)", self.open),
        }
    }
}

/// This process's current usage.
#[cfg(unix)]
pub fn fd_usage() -> std::io::Result<FdUsage> {
    // Listing the directory opens one descriptor of its own.
    let dir = if cfg!(target_os = "linux") {
        "/proc/self/fd"
    } else {
        "/dev/fd"
    };
    let open = std::fs::read_dir(dir)?.count().saturating_sub(1);
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit writes into the struct we own.
    let soft_limit = if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } == 0 {
        (limit.rlim_cur != libc::RLIM_INFINITY).then_some(limit.rlim_cur)
    } else {
        return Err(std::io::Error::last_os_error());
    };
    Ok(FdUsage { open, soft_limit })
}

#[cfg(not(unix))]
pub fn fd_usage() -> std::io::Result<FdUsage> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "open-file usage is only tracked on Unix",
    ))
}

/// Remembers the last band logged, so the tick logs only changes.
#[derive(Debug, Default)]
pub struct FdWatch {
    last_band: Option<usize>,
    peak: usize,
}

impl FdWatch {
    /// The line to log for this sample, if any: the first sample, and
    /// every move into another band (with the peak since the last line).
    pub fn observe(&mut self, usage: FdUsage) -> Option<String> {
        self.peak = self.peak.max(usage.open);
        let band = usage.band();
        let line = match self.last_band {
            None => format!("watching: {}", usage.describe()),
            Some(last) if band > last => format!("rising: {}", usage.describe()),
            Some(last) if band < last => {
                format!("falling: {} (peak {})", usage.describe(), self.peak)
            }
            Some(_) => return None,
        };
        if self.last_band.is_some_and(|last| band < last) {
            self.peak = usage.open;
        }
        self.last_band = Some(band);
        Some(line)
    }
}

/// Append `line` to the log under `config_dir`, timestamped. A failure to
/// write is reported on stderr; the watch must never disturb the app.
pub fn append_log(config_dir: &Path, line: &str) {
    let path = config_dir.join(LOG_FILE);
    let result = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut file| {
            writeln!(
                file,
                "{} {line}",
                chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
            )
        });
    if let Err(error) = result {
        eprintln!("[resource] could not write {}: {error}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(open: usize) -> FdUsage {
        FdUsage {
            open,
            soft_limit: Some(256),
        }
    }

    #[test]
    fn bands_follow_the_share_of_the_limit() {
        assert_eq!(usage(100).band(), 0);
        assert_eq!(usage(128).band(), 1);
        assert_eq!(usage(192).band(), 2);
        assert_eq!(usage(231).band(), 3);
        let unlimited = FdUsage {
            open: 10_000,
            soft_limit: None,
        };
        assert_eq!(unlimited.band(), 0);
        assert_eq!(unlimited.describe(), "10000 open files (no limit)");
        assert_eq!(usage(128).describe(), "128 of 256 open files (50%)");
    }

    #[test]
    fn the_watch_logs_the_start_and_band_changes_only() {
        let mut watch = FdWatch::default();
        assert_eq!(
            watch.observe(usage(100)).as_deref(),
            Some("watching: 100 of 256 open files (39%)")
        );
        assert_eq!(watch.observe(usage(120)), None);
        assert_eq!(
            watch.observe(usage(200)).as_deref(),
            Some("rising: 200 of 256 open files (78%)")
        );
        assert_eq!(
            watch.observe(usage(240)).as_deref(),
            Some("rising: 240 of 256 open files (93%)")
        );
        assert_eq!(watch.observe(usage(235)), None);
        assert_eq!(
            watch.observe(usage(90)).as_deref(),
            Some("falling: 90 of 256 open files (35%) (peak 240)")
        );
    }

    #[cfg(unix)]
    #[test]
    fn this_process_reports_its_usage() {
        let usage = fd_usage().unwrap();
        assert!(usage.open >= 3, "stdin, stdout and stderr: {usage:?}");
    }
}
