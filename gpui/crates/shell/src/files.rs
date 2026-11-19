//! One-directory-at-a-time listings. No recursive scan, watcher or retained
//! collapsed-directory cache; expanding and paging always read the filesystem.
use crate::data::WorkspaceId;
use std::{collections::HashMap, path::PathBuf};

pub const PAGE_SIZE: usize = 2_000;
#[derive(Clone, Debug)]
pub struct Entry {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
}
#[derive(Clone, Debug)]
pub struct Request {
    pub workspace: WorkspaceId,
    pub path: PathBuf,
    generation: u64,
    limit: usize,
}
pub struct Outcome {
    pub request: Request,
    pub result: Result<Listing, String>,
}
pub struct Listing {
    entries: Vec<Entry>,
    more: bool,
}
#[derive(Default)]
struct Directory {
    generation: u64,
    limit: usize,
    entries: Vec<Entry>,
    more: bool,
    loading: bool,
    error: Option<String>,
}
#[derive(Default)]
pub struct FileTree {
    pub root: PathBuf,
    directories: HashMap<PathBuf, Directory>,
    generation: u64,
}
#[derive(Clone, Debug)]
pub enum Row {
    Entry {
        entry: Entry,
        expanded: bool,
        depth: usize,
    },
    Loading {
        depth: usize,
    },
    Error {
        message: String,
        depth: usize,
    },
    More {
        path: PathBuf,
        depth: usize,
    },
    Empty {
        depth: usize,
    },
}
impl FileTree {
    pub fn refresh(&mut self, workspace: WorkspaceId, root: PathBuf) -> Request {
        self.root = root.clone();
        self.directories.clear();
        self.request(workspace, root, PAGE_SIZE)
    }
    pub fn toggle(&mut self, workspace: WorkspaceId, path: PathBuf) -> Option<Request> {
        if self.directories.contains_key(&path) {
            self.directories.retain(|key, _| !key.starts_with(&path));
            None
        } else {
            Some(self.request(workspace, path, PAGE_SIZE))
        }
    }
    pub fn more(&mut self, workspace: WorkspaceId, path: PathBuf) -> Option<Request> {
        let directory = self.directories.get(&path)?;
        if directory.loading || !directory.more {
            return None;
        }
        let limit = directory.limit.saturating_add(PAGE_SIZE);
        Some(self.request(workspace, path, limit))
    }
    fn request(&mut self, workspace: WorkspaceId, path: PathBuf, limit: usize) -> Request {
        self.generation += 1;
        self.directories.insert(
            path.clone(),
            Directory {
                generation: self.generation,
                limit,
                loading: true,
                ..Default::default()
            },
        );
        Request {
            workspace,
            path,
            generation: self.generation,
            limit,
        }
    }
    pub fn complete(&mut self, outcome: Outcome) {
        let Some(directory) = self.directories.get_mut(&outcome.request.path) else {
            return;
        };
        if directory.generation != outcome.request.generation {
            return;
        }
        directory.loading = false;
        match outcome.result {
            Ok(listing) => {
                directory.entries = listing.entries;
                directory.more = listing.more;
                directory.error = None;
            }
            Err(error) => directory.error = Some(error),
        }
    }
    pub fn rows(&self) -> Vec<Row> {
        let mut rows = vec![];
        self.append_directory(&self.root, 0, &mut rows);
        rows
    }
    fn append_entries(&self, path: &PathBuf, depth: usize, rows: &mut Vec<Row>) {
        let Some(dir) = self.directories.get(path) else {
            return;
        };
        for entry in &dir.entries {
            let expanded = entry.is_dir && self.directories.contains_key(&entry.path);
            rows.push(Row::Entry {
                entry: entry.clone(),
                expanded,
                depth,
            });
            if expanded {
                self.append_directory(&entry.path, depth + 1, rows);
            }
        }
    }
    fn append_directory(&self, path: &PathBuf, depth: usize, rows: &mut Vec<Row>) {
        let Some(dir) = self.directories.get(path) else {
            return;
        };
        if dir.loading {
            rows.push(Row::Loading { depth });
        }
        if let Some(message) = &dir.error {
            rows.push(Row::Error {
                message: message.clone(),
                depth,
            });
        }
        if !dir.loading && dir.error.is_none() && dir.entries.is_empty() {
            rows.push(Row::Empty { depth });
        }
        self.append_entries(path, depth, rows);
        if dir.more {
            rows.push(Row::More {
                path: path.clone(),
                depth,
            });
        }
    }
}

pub fn list(request: &Request) -> Result<Listing, String> {
    let fail = |error| format!("Read {}: {error}", request.path.display());
    let mut entries = vec![];
    for entry in std::fs::read_dir(&request.path).map_err(fail)? {
        let entry = entry.map_err(fail)?;
        let name = entry.file_name().to_string_lossy().into_owned();
        // Match Iced's local_list_dir with its default show_hidden=true:
        // dotfiles (including .git) and gitignored paths remain visible.
        if name == "node_modules" || name == "target" {
            continue;
        }
        let path = entry.path();
        let is_dir = match std::fs::metadata(&path) {
            Ok(metadata) => metadata.is_dir(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(format!("Inspect {}: {error}", path.display())),
        };
        entries.push(Entry { path, name, is_dir });
    }
    entries.sort_by_cached_key(|entry| (!entry.is_dir, entry.name.to_lowercase()));
    let more = entries.len() > request.limit;
    entries.truncate(request.limit);
    Ok(Listing { entries, more })
}

/// Small owner-scoped job set, using the same executor as status. Replaced jobs
/// cannot publish stale results; dropping the owner aborts the async bridges.
#[derive(Default)]
pub struct FileJobs {
    jobs: HashMap<(WorkspaceId, PathBuf), tokio::task::JoinHandle<()>>,
}
impl FileJobs {
    pub fn execute(
        &mut self,
        handle: &tokio::runtime::Handle,
        request: Request,
        sender: tokio::sync::mpsc::UnboundedSender<Outcome>,
    ) {
        self.jobs.retain(|_, job| !job.is_finished());
        let key = (request.workspace, request.path.clone());
        if let Some(previous) = self.jobs.remove(&key) {
            previous.abort();
        }
        let executor = handle.clone();
        self.jobs.insert(
            key,
            handle.spawn(async move {
                let blocking_request = request.clone();
                let result = executor
                    .spawn_blocking(move || list(&blocking_request))
                    .await
                    .unwrap_or_else(|error| {
                        Err(format!(
                            "Read {}: listing task failed: {error}",
                            request.path.display()
                        ))
                    });
                let _ = sender.send(Outcome { request, result });
            }),
        );
    }
}
impl Drop for FileJobs {
    fn drop(&mut self) {
        for job in self.jobs.values() {
            job.abort();
        }
    }
}

#[cfg(test)]
mod tests;
