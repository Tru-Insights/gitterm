//! Frozen pure Iced rules from main.rs at 1454685 (TRU-274).
#![allow(dead_code)]
use std::path::PathBuf;
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ExistingWorkStatus {
    Open,
    Parked,
    /// Done or closed (Close parks then marks done): kept in the file;
    /// Reopen clears the done state and resumes it.
    Done,
}

impl ExistingWorkStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Parked => "parked",
            Self::Done => "done",
        }
    }
}

#[derive(Debug)]
struct ExistingWork {
    name: String,
    status: ExistingWorkStatus,
}
const NEW_WORK_EXISTING_SHOWN: usize = 5;
fn existing_work_matches<'a>(work: &'a [ExistingWork], name: &str) -> Vec<&'a ExistingWork> {
    let query = name.split_whitespace().collect::<Vec<_>>().join(" ");
    let query = query.to_lowercase();
    let mut matches: Vec<&ExistingWork> = work
        .iter()
        .filter(|item| {
            if query.is_empty() {
                item.status != ExistingWorkStatus::Done
            } else {
                item.name.to_lowercase().contains(&query)
            }
        })
        .collect();
    // Stable: recency order holds within a status.
    matches.sort_by_key(|item| item.status);
    matches.truncate(NEW_WORK_EXISTING_SHOWN);
    matches
}

/// Whether some existing work is called `name` (ignoring case and extra
/// spaces): Create then makes another, with a numbered folder.
fn existing_work_named(work: &[ExistingWork], name: &str) -> bool {
    let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
    !name.is_empty()
        && work.iter().any(|item| {
            item.name
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .to_lowercase()
                == name.to_lowercase()
        })
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct WorkStart {
    repos: Vec<PathBuf>,
    decisions_repo: Option<PathBuf>,
}

impl WorkStart {
    /// Rejects a decisions repo that is not one of `repos`.
    fn new(repos: Vec<PathBuf>, decisions_repo: Option<PathBuf>) -> Result<Self, String> {
        if let Some(dir) = &decisions_repo {
            if !repos.contains(dir) {
                return Err(format!(
                    "The decisions repo {} is not one of the starting repos.",
                    dir.display()
                ));
            }
        }
        Ok(Self {
            repos,
            decisions_repo,
        })
    }
}

#[test]
fn iced_sheet_baseline() {
    use ExistingWorkStatus::*;
    let work = [
        ("Old migration", Done),
        ("Migration dry run", Parked),
        ("Cloud migration", Open),
        ("Data migration", Open),
    ]
    .into_iter()
    .map(|(name, status)| ExistingWork {
        name: name.into(),
        status,
    })
    .collect::<Vec<_>>();
    assert_eq!(
        existing_work_matches(&work, "  MIGRATION ")
            .iter()
            .map(|w| w.name.as_str())
            .collect::<Vec<_>>(),
        [
            "Cloud migration",
            "Data migration",
            "Migration dry run",
            "Old migration"
        ]
    );
    assert_eq!(existing_work_matches(&work, " ").len(), 3);
    assert!(existing_work_named(&work, " cloud  MIGRATION "));
    assert!(!existing_work_named(&work, "cloud"));
    assert!(WorkStart::new(vec![], None).is_ok());
    assert_eq!(
        WorkStart::new(vec![], Some(PathBuf::from("/api"))).unwrap_err(),
        "The decisions repo /api is not one of the starting repos."
    );
}

#[test]
fn shared_sheet_matches_frozen_iced_rules() {
    use gitterm_core::work::catalog as core;
    use ExistingWorkStatus::*;
    let work = [
        ("Cloud migration", Open),
        ("Cloud  migration", Parked),
        ("Old migration", Done),
        ("Migration dry run", Parked),
        ("Data migration", Open),
        ("Migrate auth", Parked),
        ("Done thing", Done),
        ("Billing export", Open),
    ];
    let iced = work
        .iter()
        .map(|(name, status)| ExistingWork {
            name: (*name).into(),
            status: *status,
        })
        .collect::<Vec<_>>();
    let shared = work
        .iter()
        .map(|(name, status)| core::ExistingWork {
            name: (*name).into(),
            status: match status {
                Open => core::ExistingWorkStatus::Open,
                Parked => core::ExistingWorkStatus::Parked,
                Done => core::ExistingWorkStatus::Done,
            },
            action: None,
        })
        .collect::<Vec<_>>();
    for query in [
        "",
        "  ",
        "MiGRATION",
        "cloud  migration",
        "i",
        "nothing",
        "Old migration",
    ] {
        assert_eq!(
            existing_work_matches(&iced, query)
                .iter()
                .map(|w| &w.name)
                .collect::<Vec<_>>(),
            core::existing_work_matches(&shared, query)
                .iter()
                .map(|w| &w.name)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            existing_work_named(&iced, query),
            core::existing_work_named(&shared, query)
        );
    }
    for repos in [
        vec![],
        vec![PathBuf::from("/api")],
        vec![PathBuf::from("/api"), PathBuf::from("/api")],
    ] {
        for decisions in [
            None,
            Some(PathBuf::from("/api")),
            Some(PathBuf::from("/other")),
        ] {
            let iced = WorkStart::new(repos.clone(), decisions.clone());
            let shared = core::WorkStart::new(repos.clone(), decisions.clone());
            match (iced, shared) {
                (Ok(a), Ok(b)) => {
                    assert_eq!(a.repos, b.repos);
                    assert_eq!(a.decisions_repo, b.decisions_repo);
                }
                (Err(a), Err(b)) => assert_eq!(a, b),
                pair => panic!("validation differs: {pair:?}"),
            }
        }
    }
}
