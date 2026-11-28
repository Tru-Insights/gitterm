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
    show_hidden: bool,
}
pub struct Outcome {
    pub request: Request,
    pub result: Result<Listing, String>,
}
#[derive(Clone)]
pub(crate) struct RemoteNavigation {
    pub current: PathBuf,
    pub parent: Option<PathBuf>,
    pub display: String,
}
pub struct Listing {
    navigation: Option<RemoteNavigation>,
    entries: Vec<Entry>,
    more: bool,
}
#[derive(Default)]
struct Directory {
    generation: u64,
    limit: usize,
    navigation: Option<RemoteNavigation>,
    entries: Vec<Entry>,
    more: bool,
    loading: bool,
    error: Option<String>,
}
pub struct FileTree {
    pub root: PathBuf,
    directories: HashMap<PathBuf, Directory>,
    generation: u64,
    show_hidden: bool,
}
impl Default for FileTree {
    fn default() -> Self {
        Self {
            root: PathBuf::new(),
            directories: HashMap::new(),
            generation: 0,
            show_hidden: true,
        }
    }
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
    pub(crate) fn set_show_hidden(&mut self, show_hidden: bool) {
        self.show_hidden = show_hidden;
    }
    pub fn refresh(&mut self, workspace: WorkspaceId, root: PathBuf) -> Request {
        self.root = root.clone();
        self.directories.clear();
        self.request(workspace, root, PAGE_SIZE)
    }
    pub(crate) fn set_root(&mut self, root: PathBuf) {
        self.root = root;
        self.directories.clear();
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
            show_hidden: self.show_hidden,
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
                directory.navigation = listing.navigation;
                directory.entries = listing.entries;
                directory.more = listing.more;
                directory.error = None;
            }
            Err(error) => directory.error = Some(error),
        }
    }
    pub(crate) fn remote_navigation(&self) -> Option<&RemoteNavigation> {
        self.directories.get(&self.root)?.navigation.as_ref()
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
        if name == "node_modules" || name == "target" {
            continue;
        }
        if !request.show_hidden && name.starts_with('.') {
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
    Ok(Listing {
        entries,
        more,
        navigation: None,
    })
}

/// Small owner-scoped job set, using the same executor as status. Replaced jobs
/// cannot publish stale results; dropping the owner aborts the async bridges.
#[derive(Default)]
pub struct FileJobs {
    jobs: HashMap<(WorkspaceId, PathBuf), tokio::task::JoinHandle<()>>,
}
impl FileJobs {
    pub(crate) fn execute(
        &mut self,
        handle: &tokio::runtime::Handle,
        request: Request,
        source: Result<Option<crate::remote_client::source::RemoteSource>, String>,
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
                    .spawn_blocking(move || match source {
                        Ok(None) => list(&blocking_request),
                        Ok(Some(source)) => list_remote(&blocking_request, &source),
                        Err(error) => Err(error),
                    })
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

fn list_remote(
    request: &Request,
    source: &crate::remote_client::source::RemoteSource,
) -> Result<Listing, String> {
    let listing = source.client.source_list(
        &source.spec,
        request.path.to_string_lossy().into_owned(),
        request.show_hidden,
    )?;
    if let Some(error) = listing.error {
        return Err(error);
    }
    let navigation = RemoteNavigation {
        current: PathBuf::from(listing.current_dir.display()),
        parent: listing.parent.map(|path| PathBuf::from(path.display())),
        display: listing.display_dir,
    };
    let mut entries: Vec<_> = listing
        .entries
        .into_iter()
        .map(|entry| Entry {
            path: PathBuf::from(entry.path.display()),
            name: entry.name,
            is_dir: entry.is_dir,
        })
        .collect();
    let more = entries.len() > request.limit;
    entries.truncate(request.limit);
    Ok(Listing {
        entries,
        more,
        navigation: Some(navigation),
    })
}

#[cfg(test)]
mod tests;
