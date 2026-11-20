//! Conversation browser state. Sources are supplied by the host; loading an
//! index never authorizes catch-up/backfill writes to provider or facts roots.
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::chats::{ChatBackend, ChatIndexEntry, ChatPreview, ChatRef, ChatScope};

pub const REMOTE_INDEX_FRESH: Duration = Duration::from_secs(60);
pub const INDEX_FRESH: Duration = Duration::from_secs(30);

/// None identifies the local machine, as in ChatRef.
pub type Machine = Option<String>;
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexStamp {
    pub machine: Machine,
    pub generation: u64,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewStamp {
    /// Tab session UID, retained across tab switching; replaced on tab creation.
    pub owner: String,
    pub source_generation: u64,
    pub chat: ChatRef,
    pub path: PathBuf,
    pub backend: ChatBackend,
    pub generation: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Filter {
    pub query: String,
    pub scope: ChatScope,
    pub backend: Option<ChatBackend>,
    pub machine: Machine,
    pub workspace_root: Option<PathBuf>,
}
#[derive(Debug, Default)]
pub struct Index {
    pub entries: Vec<ChatIndexEntry>,
    pub loaded_at: Option<Instant>,
    pub error: Option<String>,
    loading: Option<IndexStamp>,
}
#[derive(Debug)]
pub enum Intent<P = ChatPreview> {
    SetFilter(Filter),
    /// Refresh every stale source on pane open. Force is the explicit refresh.
    LoadIndex {
        machine: Machine,
        now: Instant,
        force: bool,
    },
    IndexFinished {
        stamp: IndexStamp,
        result: Result<Vec<ChatIndexEntry>, String>,
        now: Instant,
    },
    /// Host resolved this exact (machine, id) to its transcript and backend.
    Select {
        owner: String,
        chat: ChatRef,
        path: PathBuf,
        backend: ChatBackend,
    },
    PreviewFinished {
        stamp: PreviewStamp,
        result: Result<P, String>,
    },
    ClosePreview(String),
    /// Must be sent when a remote configuration is replaced, even with the same id.
    SourceChanged(Machine),
    /// Source disconnected/replaced: old index and preview jobs lose authority.
    RemoveMachine(Machine),
}
#[derive(Debug)]
pub enum Effect {
    LoadIndex(IndexStamp),
    LoadPreview(PreviewStamp),
    /// Host may recompute optional relevance ranking; filtering itself is pure.
    FilterChanged,
    IndexChanged(Machine),
}
#[derive(Debug)]
pub struct State<P = ChatPreview> {
    filter: Filter,
    indexes: HashMap<Machine, Index>,
    selections: HashMap<String, Selection<P>>,
    sources: HashMap<Machine, u64>,
    generation: u64,
}
impl<P> Default for State<P> {
    fn default() -> Self {
        Self {
            filter: Filter::default(),
            indexes: HashMap::new(),
            selections: HashMap::new(),
            sources: HashMap::new(),
            generation: 0,
        }
    }
}
/// P is one atomic preview result (tail plus digest in Iced). Core does not
/// split that result or allow an old digest to replace a current tail.
#[derive(Debug)]
struct Selection<P> {
    stamp: PreviewStamp,
    loading: bool,
    preview: Option<P>,
    error: Option<String>,
}
impl<P> State<P> {
    pub fn filter(&self) -> &Filter {
        &self.filter
    }
    pub fn index(&self, machine: &Machine) -> Option<&Index> {
        self.indexes.get(machine)
    }
    pub fn selected(&self, owner: &str) -> Option<&PreviewStamp> {
        self.selections.get(owner).map(|s| &s.stamp)
    }
    pub fn preview(&self, owner: &str) -> Option<&P> {
        self.selections.get(owner).and_then(|s| s.preview.as_ref())
    }
    pub fn preview_error(&self, owner: &str) -> Option<&str> {
        self.selections.get(owner).and_then(|s| s.error.as_deref())
    }
    pub fn loading_index(&self, machine: &Machine) -> bool {
        self.indexes
            .get(machine)
            .is_some_and(|i| i.loading.is_some())
    }
    pub fn filtered_entries(&self) -> Vec<(Machine, &ChatIndexEntry)> {
        let query = self.filter.query.to_lowercase();
        self.indexes
            .iter()
            .filter(|(machine, _)| {
                self.filter.scope == ChatScope::Everywhere || **machine == self.filter.machine
            })
            .flat_map(|(machine, index)| {
                index
                    .entries
                    .iter()
                    .map(move |entry| (machine.clone(), entry))
            })
            .filter(|(_, entry)| {
                self.filter.scope != ChatScope::Workspace
                    || self
                        .filter
                        .workspace_root
                        .as_ref()
                        .is_some_and(|root| entry.in_workspace(root))
            })
            .filter(|(_, entry)| {
                self.filter
                    .backend
                    .is_none_or(|backend| entry.backend == backend)
            })
            .filter(|(_, entry)| entry.matches_query(&query))
            .collect()
    }
    fn generation(&mut self) -> u64 {
        self.generation = self
            .generation
            .checked_add(1)
            .expect("Find generation exhausted");
        self.generation
    }
    pub fn reduce(&mut self, intent: Intent<P>) -> Vec<Effect> {
        match intent {
            Intent::SetFilter(filter) => {
                self.filter = filter;
                vec![Effect::FilterChanged]
            }
            Intent::LoadIndex {
                machine,
                now,
                force,
            } => {
                if self.indexes.get(&machine).is_some_and(|index| {
                    index.loading.is_some()
                        || (!force
                            && index.loaded_at.is_some_and(|at| {
                                now.saturating_duration_since(at)
                                    < if machine.is_some() {
                                        REMOTE_INDEX_FRESH
                                    } else {
                                        INDEX_FRESH
                                    }
                            }))
                }) {
                    return vec![];
                }
                let stamp = IndexStamp {
                    machine: machine.clone(),
                    generation: self.generation(),
                };
                self.indexes.entry(machine).or_default().loading = Some(stamp.clone());
                vec![Effect::LoadIndex(stamp)]
            }
            Intent::IndexFinished { stamp, result, now } => {
                let Some(index) = self
                    .indexes
                    .get_mut(&stamp.machine)
                    .filter(|i| i.loading.as_ref() == Some(&stamp))
                else {
                    return vec![];
                };
                index.loading = None;
                index.loaded_at = Some(now);
                match result {
                    Ok(entries) => {
                        index.entries = entries;
                        index.error = None;
                    }
                    Err(error) => {
                        index.error = Some(error);
                        return vec![];
                    }
                }
                vec![Effect::IndexChanged(stamp.machine)]
            }
            Intent::Select {
                owner,
                chat,
                path,
                backend,
            } => {
                if self
                    .selected(&owner)
                    .is_some_and(|stamp| stamp.chat == chat)
                {
                    return self.reduce(Intent::ClosePreview(owner));
                }
                let source_generation = *self.sources.entry(chat.remote_id.clone()).or_default();
                let stamp = PreviewStamp {
                    owner: owner.clone(),
                    source_generation,
                    chat,
                    path,
                    backend,
                    generation: self.generation(),
                };
                self.selections.insert(
                    owner,
                    Selection {
                        stamp: stamp.clone(),
                        loading: true,
                        preview: None,
                        error: None,
                    },
                );
                vec![Effect::LoadPreview(stamp)]
            }
            Intent::PreviewFinished { stamp, result } => {
                let Some(selection) = self
                    .selections
                    .get_mut(&stamp.owner)
                    .filter(|s| s.loading && s.stamp == stamp)
                else {
                    return vec![];
                };
                selection.loading = false;
                match result {
                    Ok(preview) => {
                        selection.preview = Some(preview);
                        selection.error = None;
                    }
                    Err(error) => selection.error = Some(error),
                }
                vec![]
            }
            Intent::ClosePreview(owner) => {
                self.selections.remove(&owner);
                vec![]
            }
            Intent::RemoveMachine(machine) | Intent::SourceChanged(machine) => {
                self.indexes.remove(&machine);
                let generation = self.generation();
                self.sources.insert(machine.clone(), generation);
                self.selections
                    .retain(|_, s| s.stamp.chat.remote_id != machine);
                vec![]
            }
        }
    }
}

#[cfg(test)]
mod tests;
