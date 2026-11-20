//! Review pane (TRU-178): what the work changed in one checkout, read as a
//! structured diff and rendered as one page in the Viewer webview.
//!
//! A Workspace overview branch row opens it. The diff comes from the same
//! git calls the overview's branch diff used (`git diff HEAD` for
//! Uncommitted, `git diff <base>...HEAD` for vs base, and parent-to-commit
//! diffs for Commits; `ls-files --others` for untracked files,
//! `merge-base` for the base commit), read as a file
//! list first (`--raw --numstat -z`) and then as a patch for the files that
//! fit the caps. Everything here is free of iced and wry: main.rs runs
//! [`load_review`] off the UI thread, renders [`render_page`] into the
//! Viewer surface and feeds the page's messages through [`parse_ipc`].
//!
//! Caps, so a huge diff can never hang the app:
//!
//! - a file shows at most [`FILE_LINE_CAP`] lines or [`FILE_BYTE_CAP`]
//!   bytes of text and is marked `truncated` past that (untracked files are
//!   read only that far);
//! - files are admitted in kind order (code, config, docs, tests,
//!   generated) while their changed lines fit [`ADMIT_LINE_BUDGET`]; the
//!   rest are listed with their counts but collapsed as "too large to show
//!   here" (`omitted`);
//! - rendering stops for good at [`TOTAL_LINE_CAP`] lines or
//!   [`TOTAL_BYTE_CAP`] bytes, context included;
//! - at most [`FILE_LIST_CAP`] files are listed; the rest are counted.
//!
//! Viewed marks live in `<shared config>/review-viewed.json`
//! ([`ViewedMarks`]), keyed by canonical checkout and file path, holding
//! the content hash of the version viewed.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::workspace_overview::{git_text, DiffSource, GIT_TIMEOUT};

pub use gitterm_core::review::viewed::{ViewedState, VIEWED_FILE};
pub use gitterm_core::review::{budget::*, model::*, patch::*};

/// Compatibility adapter retaining the desktop's atomic-write helper.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ViewedMarks(gitterm_core::review::viewed::ViewedMarks);

impl std::fmt::Debug for ViewedMarks {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl std::ops::Deref for ViewedMarks {
    type Target = gitterm_core::review::viewed::ViewedMarks;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for ViewedMarks {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl ViewedMarks {
    pub fn load(dir: &Path) -> Result<Self, String> {
        gitterm_core::review::viewed::ViewedMarks::load(dir).map(Self)
    }

    pub fn save(&self, dir: &Path) -> Result<(), String> {
        std::fs::create_dir_all(dir)
            .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
        let json = self.0.encode()?;
        crate::merge_plan::write_atomically(&dir.join(VIEWED_FILE), &json)
    }
}

pub use gitterm_core::review::collect::{branch_commits, commit_diff_endpoints, ReviewLoad};

/// Loads the review of `target` for `source`. Never call on the UI thread.
pub async fn load_review(target: ReviewTarget, source: DiffSource) -> Result<ReviewLoad, String> {
    load_review_source(target, source.into()).await
}

/// Desktop composition keeps ambient snapshot roots outside core.
pub async fn load_review_source(
    target: ReviewTarget,
    source: ReviewSource,
) -> Result<ReviewLoad, String> {
    let checkout = target.checkout.clone();
    let snapshot_source = source.clone();
    let snapshot_read = async move {
        let turns = match &snapshot_source {
            ReviewSource::Turns { chat, .. } => {
                crate::turn_snapshots::turns(&checkout, chat).await?
            }
            _ => Vec::new(),
        };
        let diff_checkout = if matches!(
            &snapshot_source,
            ReviewSource::Turns {
                selected: Some(_),
                ..
            }
        ) {
            crate::turn_snapshots::store_path(&checkout).await?
        } else {
            checkout
        };
        Ok((turns, diff_checkout))
    };
    gitterm_core::review::collect::load_review_source_with_snapshots(target, source, snapshot_read)
        .await
}

// ---------------------------------------------------------------------------
// Viewed marks.

// ---------------------------------------------------------------------------
// IPC from the page.

/// A message the review page posts with `window.ipc.postMessage`. Every
/// message carries the page's `token`; main.rs drops messages whose token
/// is not the open pane's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewIpc {
    /// The Uncommitted / vs base switch; `hide_noise` is the toggle's state
    /// to keep across the reload.
    SetSource {
        token: String,
        source: DiffSource,
        hide_noise: bool,
    },
    ShowTurns {
        token: String,
    },
    SelectTurn {
        token: String,
        number: u64,
    },
    ShowCommits {
        token: String,
    },
    SelectCommits {
        token: String,
        first: String,
        last: String,
        pending: bool,
    },
    /// A file's Viewed checkbox.
    SetViewed {
        token: String,
        path: String,
        viewed: bool,
    },
    Close {
        token: String,
    },
    /// Open a listed file in GitTerm's file viewer.
    OpenFile {
        token: String,
        path: String,
    },
}

impl ReviewIpc {
    pub fn token(&self) -> &str {
        match self {
            ReviewIpc::SetSource { token, .. }
            | ReviewIpc::ShowTurns { token }
            | ReviewIpc::SelectTurn { token, .. }
            | ReviewIpc::ShowCommits { token }
            | ReviewIpc::SelectCommits { token, .. }
            | ReviewIpc::SetViewed { token, .. }
            | ReviewIpc::Close { token }
            | ReviewIpc::OpenFile { token, .. } => token,
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum WireIpc {
    #[serde(rename = "review.source")]
    Source {
        token: String,
        source: String,
        hide_noise: bool,
    },
    #[serde(rename = "review.turns")]
    Turns { token: String },
    #[serde(rename = "review.turn")]
    Turn { token: String, number: u64 },
    #[serde(rename = "review.commits")]
    Commits { token: String },
    #[serde(rename = "review.commit_range")]
    CommitRange {
        token: String,
        first: String,
        last: String,
        pending: bool,
    },
    #[serde(rename = "review.viewed")]
    Viewed {
        token: String,
        path: String,
        viewed: bool,
    },
    #[serde(rename = "review.close")]
    Close { token: String },
    #[serde(rename = "review.open_file")]
    OpenFile { token: String, path: String },
}

/// A diff path from the page: relative, no `..`, not empty.
fn page_path(path: String) -> Result<String, String> {
    let bad = path.is_empty()
        || path.starts_with('/')
        || path.split('/').any(|part| part == "..")
        || path.contains('\0');
    if bad {
        Err(format!("review page sent an invalid path {path:?}"))
    } else {
        Ok(path)
    }
}

/// Parses one message body from the Viewer page.
pub fn parse_ipc(body: &str) -> Result<ReviewIpc, String> {
    let wire: WireIpc = serde_json::from_str(body)
        .map_err(|error| format!("not a review message ({error}): {body}"))?;
    Ok(match wire {
        WireIpc::Source {
            token,
            source,
            hide_noise,
        } => ReviewIpc::SetSource {
            token,
            source: match source.as_str() {
                "uncommitted" => DiffSource::Uncommitted,
                "vs_base" => DiffSource::VsBase,
                other => return Err(format!("review page sent an unknown source {other:?}")),
            },
            hide_noise,
        },
        WireIpc::Viewed {
            token,
            path,
            viewed,
        } => ReviewIpc::SetViewed {
            token,
            path: page_path(path)?,
            viewed,
        },
        WireIpc::Turns { token } => ReviewIpc::ShowTurns { token },
        WireIpc::Turn { token, number } => ReviewIpc::SelectTurn { token, number },
        WireIpc::Commits { token } => ReviewIpc::ShowCommits { token },
        WireIpc::CommitRange {
            token,
            first,
            last,
            pending,
        } => {
            let valid = |sha: &str| {
                !sha.is_empty() && sha.len() <= 64 && sha.bytes().all(|b| b.is_ascii_hexdigit())
            };
            if !valid(&first) || !valid(&last) || (pending && first != last) {
                return Err("review page sent an invalid commit id".to_string());
            }
            ReviewIpc::SelectCommits {
                token,
                first,
                last,
                pending,
            }
        }
        WireIpc::Close { token } => ReviewIpc::Close { token },
        WireIpc::OpenFile { token, path } => ReviewIpc::OpenFile {
            token,
            path: page_path(path)?,
        },
    })
}

// ---------------------------------------------------------------------------
// Opening from a chat (TRU-179).
//
// An Edit, MultiEdit or Write card in the chat page posts
// `{"type": "open_review", "path": "<absolute file>"}`. The pane opens on
// the checkout holding that file, scrolled to it.

/// The file an `open_review` message from the chat page names: an absolute,
/// normalized path (no `.` or `..` parts, no NUL).
pub fn chat_review_path(message: &serde_json::Value) -> Result<PathBuf, String> {
    let path = message
        .get("path")
        .and_then(|path| path.as_str())
        .ok_or_else(|| format!("open_review without a path: {message}"))?;
    let bad = !path.starts_with('/')
        || path.contains('\0')
        || path.split('/').any(|part| part == "." || part == "..");
    if bad {
        return Err(format!("open_review sent an invalid path {path:?}"));
    }
    Ok(PathBuf::from(path))
}

/// The checkout `file` belongs to: its nearest ancestor holding a `.git`
/// entry (the directory of a main checkout, or the file a linked worktree
/// or submodule has). A worktree is its own checkout.
pub fn checkout_containing(file: &Path) -> Option<PathBuf> {
    file.ancestors()
        .skip(1)
        .find(|dir| dir.join(".git").exists())
        .map(Path::to_path_buf)
}

/// `file` relative to `checkout`, `/`-separated as git lists it.
pub fn checkout_relative(checkout: &Path, file: &Path) -> Result<String, String> {
    let rel = file
        .strip_prefix(checkout)
        .map_err(|_| format!("{} is not inside {}", file.display(), checkout.display()))?;
    let parts: Vec<String> = rel
        .components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect();
    if parts.is_empty() {
        return Err(format!("{} is the checkout itself", file.display()));
    }
    Ok(parts.join("/"))
}

/// Resolves an `open_review` for `file` in `checkout`. `known` is the
/// overview's row for the checkout when there is one (its owner and base);
/// otherwise the checkout is read as a row would be. Never call on the UI
/// thread.
pub async fn resolve_edit_review(
    checkout: PathBuf,
    file: PathBuf,
    known: Option<ReviewTarget>,
) -> Result<EditReview, String> {
    let focus = checkout_relative(&checkout, &file)?;
    let target = match known {
        Some(target) => target,
        None => {
            let facts = crate::workspace_overview::checkout_facts(&checkout).await?;
            ReviewTarget {
                repo: crate::workspace_overview::folder_name(&facts.repo),
                branch: facts.branch.unwrap_or_else(|| "(detached)".to_string()),
                owner: "opened from a chat".to_string(),
                base: facts.base,
                checkout,
            }
        }
    };
    let pathspec = format!(":(literal){focus}");
    let status = git_text(
        &target.checkout,
        &[
            "--no-optional-locks",
            "status",
            "--porcelain",
            "--untracked-files=all",
            "--",
            &pathspec,
        ],
        GIT_TIMEOUT,
    )
    .await?;
    let source = edit_review_source(!status.trim().is_empty(), target.base.is_some());
    Ok(EditReview {
        target,
        source,
        focus,
    })
}

// ---------------------------------------------------------------------------
// The page.

const PAGE_CSS: &str = include_str!("../assets/review_pane.css");
const PAGE_JS: &str = include_str!("../assets/review_pane.js");
/// Vendored, inlined (no network at runtime); license in `assets/vendor/`.
const HIGHLIGHT_JS: &str = include_str!("../assets/vendor/highlight.min.js");

/// Escapes text for HTML content and quoted attribute values. Every piece
/// of diff text goes through here: diff content is untrusted.
pub fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// What the page body shows.
#[derive(Debug, Clone, Copy)]
pub enum PageState<'a> {
    Loading,
    Error(&'a str),
    /// The diff with each file's viewed state, in `diff.files` order.
    Ready {
        diff: &'a ReviewDiff,
        viewed: &'a [ViewedState],
    },
}

#[derive(Debug, Clone)]
pub struct ReviewPage<'a> {
    pub target: &'a ReviewTarget,
    pub source: ReviewSource,
    pub commits: &'a [ReviewCommit],
    pub turns: &'a [crate::turn_snapshots::Turn],
    pub turns_available: bool,
    pub commits_truncated: bool,
    pub commits_error: Option<&'a str>,
    pub state: PageState<'a>,
    pub dark: bool,
    pub hide_noise: bool,
    /// Echoed in every IPC message.
    pub token: &'a str,
    /// The CSP nonce for the page's inline style and scripts.
    pub nonce: &'a str,
    /// A checkout-relative file the page scrolls to on load (a chat's edit
    /// card opened the pane, TRU-179).
    pub focus: Option<&'a str>,
}

fn plural(count: usize, one: &str) -> String {
    if count == 1 {
        format!("{count} {one}")
    } else {
        format!("{count} {one}s")
    }
}

fn counts_html(additions: usize, deletions: usize) -> String {
    format!("<span class=\"ad\">+{additions}</span><span class=\"de\">\u{2212}{deletions}</span>")
}

/// The whole page: CSP, toolbar, file tree and file sections.
pub fn render_page(page: &ReviewPage) -> String {
    let nonce = escape_html(page.nonce);
    let mut html = String::with_capacity(64 * 1024);
    html.push_str("<!DOCTYPE html>\n<html lang=\"en\" data-theme=\"");
    html.push_str(if page.dark { "dark" } else { "light" });
    html.push_str("\">\n<head>\n<meta charset=\"utf-8\">\n");
    html.push_str(&format!(
        "<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; \
         script-src 'nonce-{nonce}'; style-src 'nonce-{nonce}'; img-src 'none'; \
         font-src 'none'; connect-src 'none'; media-src 'none'; object-src 'none'; \
         frame-src 'none'; base-uri 'none'; form-action 'none'\">\n"
    ));
    html.push_str("<title>Review</title>\n");
    html.push_str(&format!(
        "<style nonce=\"{nonce}\">\n{PAGE_CSS}\n</style>\n"
    ));
    html.push_str("</head>\n");
    // The focused file, when this view lists it; the script scrolls there.
    let focus = match (page.focus, page.state) {
        (Some(path), PageState::Ready { diff, .. }) if diff.file(path).is_some() => {
            format!(" data-focus=\"{}\"", escape_html(path))
        }
        _ => String::new(),
    };
    html.push_str(&format!(
        "<body class=\"{}\" data-token=\"{}\" data-source=\"{}\" {}{focus}>\n",
        if page.hide_noise { "hide-noise" } else { "" },
        escape_html(page.token),
        match &page.source {
            ReviewSource::Uncommitted => "uncommitted",
            ReviewSource::VsBase => "vs_base",
            ReviewSource::Commits(_) => "commits",
            ReviewSource::Turns { .. } => "turns",
        },
        match &page.source {
            ReviewSource::Commits(Some(range)) if range.pending =>
                format!("data-first-commit=\"{}\"", escape_html(&range.first)),
            _ => String::new(),
        }
    ));
    render_toolbar(&mut html, page);
    html.push_str("<div id=\"body\">\n");
    render_commits(&mut html, page);
    render_turns(&mut html, page);
    match page.state {
        PageState::Loading => {
            html.push_str(
                "<nav id=\"tree\"></nav><main id=\"files\"><p class=\"empty\">Loading changes\u{2026}</p></main>\n",
            );
        }
        PageState::Error(error) => {
            html.push_str(&format!(
                "<nav id=\"tree\"></nav><main id=\"files\"><p class=\"empty error\">{}</p></main>\n",
                escape_html(error)
            ));
        }
        PageState::Ready { diff, viewed } => {
            render_tree(&mut html, diff, viewed);
            render_files(&mut html, page, diff, viewed);
        }
    }
    html.push_str("</div>\n");
    html.push_str(&format!(
        "<script nonce=\"{nonce}\">\n{HIGHLIGHT_JS}\n</script>\n"
    ));
    html.push_str(&format!(
        "<script nonce=\"{nonce}\">\n{PAGE_JS}\n</script>\n"
    ));
    html.push_str("</body>\n</html>\n");
    html
}

fn render_toolbar(html: &mut String, page: &ReviewPage) {
    let target = page.target;
    html.push_str("<header id=\"toolbar\">\n<div class=\"row\">");
    html.push_str(&format!(
        "<span class=\"branch\">\u{2387} {}</span><span class=\"repo\">\u{00B7} {}</span>",
        escape_html(&target.branch),
        escape_html(&target.repo)
    ));
    let on = |source: ReviewSource| if page.source == source { " on" } else { "" };
    let vs_label = match &target.base {
        Some(base) => format!("vs {}", base.reference),
        None => "vs base".to_string(),
    };
    html.push_str(&format!(
        "<div class=\"seg\" role=\"group\" aria-label=\"Diff source\">\
         <button type=\"button\" data-source=\"uncommitted\" class=\"src{}\">Uncommitted</button>\
         <button type=\"button\" data-source=\"vs_base\" class=\"src{}\"{}>{}</button>",
        on(ReviewSource::Uncommitted),
        on(ReviewSource::VsBase),
        if target.base.is_none() {
            " disabled"
        } else {
            ""
        },
        escape_html(&vs_label)
    ));
    let commits_on = matches!(&page.source, ReviewSource::Commits(_));
    html.push_str(&format!(
        "<button type=\"button\" data-source=\"commits\" class=\"src{}\"{}>Commits</button>",
        if commits_on { " on" } else { "" },
        if target.base.is_none() {
            " disabled"
        } else {
            ""
        }
    ));
    html.push_str(&format!(
        "<button type=\"button\" data-source=\"turns\" class=\"src{}\"{} title=\"Review turns of the active local chat\">Turns</button>",
        if matches!(&page.source, ReviewSource::Turns { .. }) {
            " on"
        } else {
            ""
        },
        if page.turns_available { "" } else { " disabled" }
    ));
    html.push_str("</div>");
    html.push_str(&format!(
        "<label class=\"toggle\"><input type=\"checkbox\" id=\"hide-noise\"{}> Hide tests &amp; generated</label>",
        if page.hide_noise { " checked" } else { "" }
    ));
    html.push_str(
        "<span class=\"spacer\"></span><button type=\"button\" id=\"close\">Close</button></div>\n",
    );

    let mut facts = vec![
        escape_html(&target.checkout.display().to_string()),
        format!("owner: {}", escape_html(&target.owner)),
    ];
    match &target.base {
        Some(base) => {
            let mut copy = format!(
                "base {} ({})",
                escape_html(&base.reference),
                escape_html(&base.describe())
            );
            if let PageState::Ready { diff, .. } = page.state {
                if matches!(&page.source, ReviewSource::VsBase) {
                    if let Some(merge_base) = &diff.merge_base {
                        copy.push_str(&format!(" @ {} (merge-base)", escape_html(merge_base)));
                    }
                }
            }
            facts.push(copy);
        }
        None => facts.push(
            "no base: no task base, tracked branch, or develop/main/master/trunk here".to_string(),
        ),
    }
    html.push_str(&format!(
        "<div class=\"facts\">{}</div>\n",
        facts.join(" \u{00B7} ")
    ));
    if let (Some(path), PageState::Ready { diff, .. }) = (page.focus, page.state) {
        if diff.file(path).is_none() {
            let view = match &page.source {
                ReviewSource::Uncommitted => "uncommitted changes",
                ReviewSource::VsBase => "changes against the base",
                ReviewSource::Commits(_) => "selected commits",
                ReviewSource::Turns { .. } => "selected turn",
            };
            html.push_str(&format!(
                "<div class=\"focus-note\">{} is not in the {view}{}.</div>\n",
                escape_html(path),
                if diff.unlisted > 0 {
                    " (or is past the files listed)"
                } else {
                    ""
                }
            ));
        }
    }

    if let PageState::Ready { diff, .. } = page.state {
        let all = diff.totals.all();
        html.push_str("<div class=\"totals\">");
        html.push_str(&format!(
            "<span class=\"tot all\">{} {}</span>",
            plural(all.files, "file"),
            counts_html(all.additions, all.deletions)
        ));
        for kind in FileKind::ALL {
            let totals = diff.totals.get(kind);
            if totals.files == 0 {
                continue;
            }
            html.push_str(&format!(
                "<span class=\"tot k-{label}\"><span class=\"kb k-{label}\">{label}</span> {} {}</span>",
                totals.files,
                counts_html(totals.additions, totals.deletions),
                label = kind.label()
            ));
        }
        if diff.unlisted > 0 {
            html.push_str(&format!(
                "<span class=\"tot more\">+{} not listed</span>",
                plural(diff.unlisted, "file")
            ));
        }
        html.push_str("</div>\n");
    }
    html.push_str("</header>\n");
}

fn render_turns(html: &mut String, page: &ReviewPage) {
    if !matches!(&page.source, ReviewSource::Turns { .. }) {
        return;
    }
    html.push_str(
        "<aside id=\"turns\"><div class=\"commit-heading\">Turns of the active chat</div>",
    );
    if page.turns.is_empty() {
        html.push_str(
            "<p class=\"commit-cap\">No turn snapshots for this chat in this checkout.</p>",
        );
    }
    for turn in page.turns {
        let selected = matches!(&page.source, ReviewSource::Turns { selected: Some(n), .. } if *n == turn.number);
        html.push_str(&format!("<button type=\"button\" class=\"commit{}\" data-turn=\"{}\"{}>Turn {}<small>{}{}</small></button>",
            if selected { " selected" } else { "" }, turn.number,
            if turn.start.is_none() || turn.end.is_none() { " disabled" } else { "" }, turn.number, commit_age(turn.timestamp),
            if turn.start.is_none() { " · no start snapshot" } else if turn.end.is_none() { " · running or incomplete" } else { "" }));
    }
    html.push_str("</aside>");
}

fn render_commits(html: &mut String, page: &ReviewPage) {
    html.push_str("<aside id=\"commits\"><div class=\"commit-heading\">Select one commit, then another for a range</div>");
    if page.commits_truncated {
        html.push_str(&format!("<div class=\"commit-cap\">Showing the newest {COMMIT_LIST_CAP} commits; older commits are omitted.</div>"));
    }
    if let Some(error) = page.commits_error {
        html.push_str(&format!(
            "<div class=\"commit-cap\">Cannot load commits: {}</div>",
            escape_html(error)
        ));
    }
    for commit in page.commits {
        html.push_str(&format!(
            "<button type=\"button\" class=\"commit{}\" data-commit=\"{}\" title=\"{} — {}\"><code>{}</code><span>{}</span><small>{} · {}</small></button>",
            if let ReviewSource::Commits(Some(range)) = &page.source {
                if commit.sha == range.first || commit.sha == range.last { " selected" } else { "" }
            } else { "" },
            escape_html(&commit.sha), escape_html(&commit.subject), escape_html(&commit.author),
            escape_html(&commit.short_sha), escape_html(&commit.subject), escape_html(&commit.author),
            escape_html(&commit_age(commit.authored_at))
        ));
    }
    html.push_str("</aside>\n");
}

fn commit_age(authored_at: i64) -> String {
    let seconds = chrono::Utc::now()
        .timestamp()
        .saturating_sub(authored_at)
        .max(0);
    if seconds < 60 {
        return "now".to_string();
    }
    if seconds < 3_600 {
        return format!("{}m ago", seconds / 60);
    }
    if seconds < 86_400 {
        return format!("{}h ago", seconds / 3_600);
    }
    if seconds < 2_592_000 {
        return format!("{}d ago", seconds / 86_400);
    }
    format!("{}mo ago", seconds / 2_592_000)
}

/// A folder of the file tree.
#[derive(Default)]
struct TreeDir<'a> {
    name: &'a str,
    dirs: Vec<TreeDir<'a>>,
    files: Vec<usize>,
}

impl<'a> TreeDir<'a> {
    fn insert(&mut self, parts: &[&'a str], index: usize) {
        match parts {
            [] => {}
            [_file] => self.files.push(index),
            [dir, rest @ ..] => {
                let position = match self.dirs.iter().position(|d| d.name == *dir) {
                    Some(position) => position,
                    None => {
                        self.dirs.push(TreeDir {
                            name: dir,
                            ..TreeDir::default()
                        });
                        self.dirs.len() - 1
                    }
                };
                self.dirs[position].insert(rest, index);
            }
        }
    }

    fn all_noise(&self, files: &[ReviewFile]) -> bool {
        self.files.iter().all(|&index| files[index].kind.is_noise())
            && self.dirs.iter().all(|dir| dir.all_noise(files))
    }
}

fn render_tree(html: &mut String, diff: &ReviewDiff, viewed: &[ViewedState]) {
    let mut root = TreeDir::default();
    for (index, file) in diff.files.iter().enumerate() {
        let parts: Vec<&str> = file.path.split('/').collect();
        root.insert(&parts, index);
    }
    let noise = diff
        .files
        .iter()
        .filter(|file| file.kind.is_noise())
        .count();
    html.push_str("<nav id=\"tree\" aria-label=\"Changed files\">\n");
    if diff.files.is_empty() {
        html.push_str("<p class=\"empty\">No files</p>\n");
    }
    render_tree_dir(html, &root, diff, viewed);
    if noise > 0 {
        html.push_str(&format!(
            "<p class=\"noise-note\">{} hidden (tests &amp; generated)</p>\n",
            plural(noise, "file")
        ));
    }
    html.push_str("</nav>\n");
}

fn render_tree_dir(html: &mut String, dir: &TreeDir, diff: &ReviewDiff, viewed: &[ViewedState]) {
    for child in &dir.dirs {
        let noise = if child.all_noise(&diff.files) {
            " noise"
        } else {
            ""
        };
        html.push_str(&format!(
            "<details class=\"dir{noise}\" open><summary>{}</summary><div class=\"kids\">\n",
            escape_html(child.name)
        ));
        render_tree_dir(html, child, diff, viewed);
        html.push_str("</div></details>\n");
    }
    for &index in &dir.files {
        let file = &diff.files[index];
        let name = file.path.rsplit('/').next().unwrap_or(&file.path);
        let mut classes = format!("tf k-{}", file.kind.label());
        if file.kind.is_noise() {
            classes.push_str(" noise");
        }
        match viewed.get(index) {
            Some(ViewedState::Viewed) => classes.push_str(" viewed"),
            Some(ViewedState::ChangedSinceViewed) => classes.push_str(" stale"),
            _ => {}
        }
        html.push_str(&format!(
            "<button type=\"button\" class=\"{classes}\" data-idx=\"{index}\" title=\"{}\">\
             <span class=\"st s-{}\">{}</span><span class=\"nm\">{}</span>\
             <span class=\"kb k-{}\">{}</span>{}</button>\n",
            escape_html(&file.path),
            file.status.label(),
            file.status.letter(),
            escape_html(name),
            file.kind.label(),
            file.kind.label(),
            counts_html(file.additions, file.deletions)
        ));
    }
}

fn render_files(html: &mut String, page: &ReviewPage, diff: &ReviewDiff, viewed: &[ViewedState]) {
    html.push_str("<main id=\"files\">\n");
    if diff.files.is_empty() {
        let empty = match (&page.source, &page.target.base) {
            (ReviewSource::Turns { selected: None, .. }, _) => {
                "Select a completed turn to review its changes".to_string()
            }
            (ReviewSource::Turns { .. }, _) => "No changes in the selected turn".to_string(),
            (ReviewSource::Uncommitted, _) => "No uncommitted changes".to_string(),
            (ReviewSource::VsBase, Some(base)) => format!("No changes against {}", base.reference),
            (ReviewSource::VsBase, None) => "No base branch".to_string(),
            (ReviewSource::Commits(None), _) => "Choose a commit to review".to_string(),
            (ReviewSource::Commits(Some(_)), _) => "No changes in the selected commits".to_string(),
        };
        html.push_str(&format!("<p class=\"empty\">{}</p>\n", escape_html(&empty)));
    }
    for (index, file) in diff.files.iter().enumerate() {
        render_file(
            html,
            index,
            file,
            viewed.get(index).copied().unwrap_or(ViewedState::Unviewed),
            !matches!(&page.source, ReviewSource::Commits(_)),
        );
    }
    html.push_str("</main>\n");
}

fn render_file(
    html: &mut String,
    index: usize,
    file: &ReviewFile,
    viewed: ViewedState,
    can_open_worktree_file: bool,
) {
    let mut classes = format!("file k-{} s-{}", file.kind.label(), file.status.label());
    if file.kind.is_noise() {
        classes.push_str(" noise");
    }
    match viewed {
        ViewedState::Viewed => classes.push_str(" viewed collapsed"),
        ViewedState::ChangedSinceViewed => classes.push_str(" stale"),
        ViewedState::Unviewed => {}
    }
    if file.omitted {
        classes.push_str(" collapsed");
    }
    html.push_str(&format!(
        "<section class=\"{classes}\" id=\"f{index}\" data-idx=\"{index}\" data-path=\"{}\">\n",
        escape_html(&file.path)
    ));
    // Header.
    let path = match &file.old_path {
        Some(old) => format!(
            "{} <span class=\"arrow\">\u{2192}</span> {}",
            escape_html(old),
            escape_html(&file.path)
        ),
        None => escape_html(&file.path),
    };
    html.push_str(&format!(
        "<div class=\"fh\"><button type=\"button\" class=\"chev\" aria-label=\"Collapse or expand\"></button>\
         <span class=\"st s-{}\" title=\"{}\">{}</span><span class=\"path\">{path}</span>\
         <span class=\"kb k-{}\">{}</span>{}",
        file.status.label(),
        file.status.label(),
        file.status.letter(),
        file.kind.label(),
        file.kind.label(),
        counts_html(file.additions, file.deletions)
    ));
    if file.omitted {
        html.push_str("<span class=\"badge big\">too large to show here</span>");
    }
    html.push_str("<span class=\"badge changed\">Changed since viewed</span>");
    html.push_str("<span class=\"spacer\"></span>");
    if can_open_worktree_file && file.status != FileStatus::Deleted {
        html.push_str("<button type=\"button\" class=\"open\">Open</button>");
    }
    let can_mark = file.content_id.is_some();
    html.push_str(&format!(
        "<label class=\"vw\"{}><input type=\"checkbox\" class=\"viewed\"{}{}> Viewed</label></div>\n",
        if can_mark {
            ""
        } else {
            " title=\"This version could not be hashed, so it cannot be marked viewed\""
        },
        if viewed == ViewedState::Viewed {
            " checked"
        } else {
            ""
        },
        if can_mark { "" } else { " disabled" }
    ));
    // Body.
    html.push_str("<div class=\"fb\">");
    if file.omitted {
        html.push_str("<p class=\"note\">Too large to show here.</p>");
    } else if file.status == FileStatus::Binary {
        html.push_str("<p class=\"note\">Binary file; not shown.</p>");
    } else if let Some(note) = &file.note {
        html.push_str(&format!("<p class=\"note\">{}</p>", escape_html(note)));
    } else if file.hunks.is_empty() {
        let why = if file.additions + file.deletions == 0 {
            if file.status == FileStatus::Untracked {
                "Empty file."
            } else {
                "No text changes (mode, rename or metadata only)."
            }
        } else {
            "Not shown: the diff reached its size cap."
        };
        html.push_str(&format!("<p class=\"note\">{why}</p>"));
    } else {
        html.push_str(
            "<table class=\"d\"><colgroup><col class=\"cn\"><col class=\"cn\"><col></colgroup><tbody>\n",
        );
        for hunk in &file.hunks {
            html.push_str(&format!(
                "<tr class=\"hh\"><td colspan=\"3\">{}</td></tr>\n",
                escape_html(&hunk.header)
            ));
            for line in &hunk.lines {
                let class = match line.op {
                    LineOp::Context => "x",
                    LineOp::Add => "a",
                    LineOp::Del => "r",
                };
                let number = |n: Option<u32>| n.map(|n| n.to_string()).unwrap_or_default();
                html.push_str(&format!(
                    "<tr class=\"{class}\"><td class=\"o\" data-n=\"{}\"></td><td class=\"n\" data-n=\"{}\"></td><td class=\"c\">{}</td></tr>\n",
                    number(line.old_no),
                    number(line.new_no),
                    escape_html(&line.text)
                ));
            }
        }
        html.push_str("</tbody></table>");
        if file.truncated {
            html.push_str(&format!(
                "<p class=\"note\">Cut at {} lines shown (caps: {} lines or {} KB per file).</p>",
                file.shown_lines(),
                FILE_LINE_CAP,
                FILE_BYTE_CAP / 1024
            ));
        }
    }
    html.push_str("</div>\n</section>\n");
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace_overview::BaseChoice;
    use std::collections::BTreeSet;

    include!("review_collect_tests.rs");

    fn file(path: &str, kind: FileKind, adds: usize, dels: usize) -> ReviewFile {
        ReviewFile {
            path: path.to_string(),
            old_path: None,
            status: FileStatus::Modified,
            kind,
            additions: adds,
            deletions: dels,
            truncated: false,
            omitted: false,
            note: None,
            hunks: Vec::new(),
            content_id: Some(format!("id-{path}")),
        }
    }

    #[test]
    fn totals_split_by_kind() {
        let files = [
            file("src/a.rs", FileKind::Code, 3, 1),
            file("src/b.rs", FileKind::Code, 2, 0),
            file("tests/a.rs", FileKind::Test, 10, 2),
            file("Cargo.lock", FileKind::Generated, 40, 40),
        ];
        let totals = ReviewTotals::of(&files);
        assert_eq!(
            totals.get(FileKind::Code),
            KindTotals {
                files: 2,
                additions: 5,
                deletions: 1
            }
        );
        assert_eq!(totals.get(FileKind::Test).additions, 10);
        assert_eq!(totals.get(FileKind::Docs), KindTotals::default());
        assert_eq!(
            totals.all(),
            KindTotals {
                files: 4,
                additions: 55,
                deletions: 43
            }
        );
    }

    #[test]
    fn viewed_marks_go_stale_and_prune() {
        let dir = tempfile::tempdir().unwrap();
        let checkout = Path::new("/tmp/acme/portal");
        let mut a = file("src/a.rs", FileKind::Code, 1, 0);
        let b = file("src/b.rs", FileKind::Code, 1, 0);
        let mut marks = ViewedMarks::load(dir.path()).unwrap();
        assert_eq!(marks.state(checkout, &a), ViewedState::Unviewed);
        assert!(marks.set(checkout, &a, true));
        assert!(marks.set(checkout, &b, true));
        assert_eq!(marks.state(checkout, &a), ViewedState::Viewed);
        // Another checkout of the same repo has its own marks.
        assert_eq!(
            marks.state(Path::new("/tmp/acme/portal-prj-101"), &a),
            ViewedState::Unviewed
        );
        marks.save(dir.path()).unwrap();
        let mut marks = ViewedMarks::load(dir.path()).unwrap();
        // The content changed since it was viewed.
        a.content_id = Some("id-new".to_string());
        assert_eq!(marks.state(checkout, &a), ViewedState::ChangedSinceViewed);
        // A file with no content id cannot be marked.
        let mut unhashed = file("src/c.rs", FileKind::Code, 1, 0);
        unhashed.content_id = None;
        assert!(!marks.set(checkout, &unhashed, true));
        // b left every diff: pruned; a is still live.
        let live = BTreeSet::from(["src/a.rs".to_string()]);
        assert!(marks.prune(checkout, &live));
        assert!(!marks.prune(checkout, &live));
        assert_eq!(marks.state(checkout, &b), ViewedState::Unviewed);
        assert_eq!(marks.state(checkout, &a), ViewedState::ChangedSinceViewed);
        // Unmarking the last file drops the checkout.
        assert!(marks.set(checkout, &a, false));
        assert!(marks.checkouts.is_empty());
        // A corrupt file is an error, never silently replaced.
        std::fs::write(dir.path().join(VIEWED_FILE), "not json").unwrap();
        assert!(ViewedMarks::load(dir.path()).is_err());
    }

    #[test]
    fn ipc_messages_parse() {
        assert_eq!(
            parse_ipc(
                r#"{"type":"review.source","token":"t1","source":"vs_base","hide_noise":false}"#
            ),
            Ok(ReviewIpc::SetSource {
                token: "t1".into(),
                source: DiffSource::VsBase,
                hide_noise: false
            })
        );
        assert_eq!(
            parse_ipc(r#"{"type":"review.commits","token":"t1"}"#),
            Ok(ReviewIpc::ShowCommits { token: "t1".into() })
        );
        assert_eq!(
            parse_ipc(
                r#"{"type":"review.commit_range","token":"t1","first":"abcdef1","last":"abcdef2","pending":false}"#
            ),
            Ok(ReviewIpc::SelectCommits {
                token: "t1".into(),
                first: "abcdef1".into(),
                last: "abcdef2".into(),
                pending: false
            })
        );
        assert_eq!(
            parse_ipc(r#"{"type":"review.viewed","token":"t1","path":"src/a.rs","viewed":true}"#),
            Ok(ReviewIpc::SetViewed {
                token: "t1".into(),
                path: "src/a.rs".into(),
                viewed: true
            })
        );
        assert_eq!(
            parse_ipc(r#"{"type":"review.close","token":"t1"}"#),
            Ok(ReviewIpc::Close { token: "t1".into() })
        );
        let open =
            parse_ipc(r#"{"type":"review.open_file","token":"t1","path":"docs/a b.md"}"#).unwrap();
        assert_eq!(open.token(), "t1");
        assert_eq!(
            open,
            ReviewIpc::OpenFile {
                token: "t1".into(),
                path: "docs/a b.md".into()
            }
        );
        for bad in [
            r#"{"type":"review.source","token":"t1","source":"commits","hide_noise":true}"#,
            r#"{"type":"review.open_file","token":"t1","path":"../etc/passwd"}"#,
            r#"{"type":"review.open_file","token":"t1","path":"/etc/passwd"}"#,
            r#"{"type":"review.viewed","token":"t1","path":""}"#,
            r#"{"type":"review.close"}"#,
            r#"{"type":"submit","tabId":1,"text":"x"}"#,
            "not json",
        ] {
            assert!(parse_ipc(bad).is_err(), "{bad}");
        }
    }

    fn hostile_diff() -> (ReviewTarget, ReviewDiff) {
        let hostile = "</script><img src=x onerror=alert(1)>";
        let target = ReviewTarget {
            checkout: PathBuf::from("/tmp/acme/portal"),
            repo: format!("portal{hostile}"),
            branch: format!("work/prj-101{hostile}"),
            owner: "coordinator".into(),
            base: None,
        };
        let mut f = file(&format!("src/{hostile}.rs"), FileKind::Code, 1, 1);
        f.hunks = vec![ReviewHunk {
            header: format!("@@ -1 +1 @@ {hostile}"),
            lines: vec![
                ReviewLine {
                    op: LineOp::Del,
                    old_no: Some(1),
                    new_no: None,
                    text: hostile.to_string(),
                },
                ReviewLine {
                    op: LineOp::Add,
                    old_no: None,
                    new_no: Some(1),
                    text: format!("let s = \"{hostile}\"; // ' & \""),
                },
            ],
        }];
        let files = vec![f];
        let diff = ReviewDiff {
            checkout: target.checkout.clone(),
            branch: target.branch.clone(),
            base: None,
            source: DiffSource::Uncommitted,
            merge_base: None,
            totals: ReviewTotals::of(&files),
            files,
            unlisted: 0,
        };
        (target, diff)
    }

    #[test]
    fn page_escapes_hostile_text() {
        assert_eq!(
            escape_html("</script><img src=x onerror=\"a('b')\">&"),
            "&lt;/script&gt;&lt;img src=x onerror=&quot;a(&#39;b&#39;)&quot;&gt;&amp;"
        );
        let (target, diff) = hostile_diff();
        let viewed = [ViewedState::Unviewed];
        let html = render_page(&ReviewPage {
            target: &target,
            source: ReviewSource::Uncommitted,
            commits: &[],
            turns: &[],
            turns_available: true,
            commits_truncated: false,
            commits_error: None,
            state: PageState::Ready {
                diff: &diff,
                viewed: &viewed,
            },
            dark: true,
            hide_noise: true,
            token: "tok\"en",
            nonce: "n0nce",
            focus: None,
        });
        assert!(!html.contains("<img"), "an unescaped tag reached the page");
        assert!(!html.contains("onerror=alert(1)>"));
        assert!(html.contains("&lt;/script&gt;&lt;img src=x onerror=alert(1)&gt;"));
        assert!(html.contains("data-token=\"tok&quot;en\""));
        // Only the page's own three elements carry the nonce, and the CSP
        // allows nothing else.
        assert_eq!(html.matches("nonce=\"n0nce\"").count(), 3);
        assert!(html.contains("default-src 'none'; script-src 'nonce-n0nce'"));
        // Every literal </script> left closes one of the page's scripts.
        assert_eq!(html.matches("</script>").count(), 2);
        assert!(
            !html.contains("src=\"http"),
            "no element loads a remote URL"
        );
    }

    #[test]
    fn page_marks_viewed_and_stale_files() {
        let (target, mut diff) = hostile_diff();
        diff.files = vec![
            file("src/a.rs", FileKind::Code, 1, 0),
            file("src/b.rs", FileKind::Code, 1, 0),
            file("Cargo.lock", FileKind::Generated, 1, 0),
        ];
        diff.files[2].omitted = true;
        diff.totals = ReviewTotals::of(&diff.files);
        let viewed = [
            ViewedState::Viewed,
            ViewedState::ChangedSinceViewed,
            ViewedState::Unviewed,
        ];
        let html = render_page(&ReviewPage {
            target: &target,
            source: ReviewSource::Uncommitted,
            commits: &[],
            turns: &[],
            turns_available: true,
            commits_truncated: false,
            commits_error: None,
            state: PageState::Ready {
                diff: &diff,
                viewed: &viewed,
            },
            dark: false,
            hide_noise: false,
            token: "t",
            nonce: "n",
            focus: None,
        });
        assert!(html.contains("class=\"file k-code s-modified viewed collapsed\" id=\"f0\""));
        assert!(html.contains("class=\"file k-code s-modified stale\" id=\"f1\""));
        assert!(html.contains("class=\"file k-generated s-modified noise collapsed\" id=\"f2\""));
        assert!(html.contains("too large to show here"));
        assert!(html.contains("data-theme=\"light\""));
        assert!(html
            .contains("<button type=\"button\" data-source=\"vs_base\" class=\"src\" disabled>"));
    }

    #[test]
    fn loading_and_error_pages_render() {
        let (target, _) = hostile_diff();
        for state in [PageState::Loading, PageState::Error("git <failed>")] {
            let html = render_page(&ReviewPage {
                target: &target,
                source: ReviewSource::VsBase,
                commits: &[],
                turns: &[],
                turns_available: true,
                commits_truncated: false,
                commits_error: None,
                state,
                dark: true,
                hide_noise: true,
                token: "t",
                nonce: "n",
                focus: None,
            });
            assert!(html.contains("id=\"close\""));
            assert!(!html.contains("git <failed>"));
        }
    }

    #[test]
    fn turns_picker_only_enables_complete_pairs_and_preserves_the_diff_slots() {
        let (target, diff) = hostile_diff();
        let turns = [
            crate::turn_snapshots::Turn {
                number: 3,
                start: None,
                end: Some("b".into()),
                timestamp: 1,
            },
            crate::turn_snapshots::Turn {
                number: 2,
                start: Some("a".into()),
                end: None,
                timestamp: 1,
            },
            crate::turn_snapshots::Turn {
                number: 1,
                start: Some("a".into()),
                end: Some("b".into()),
                timestamp: 1,
            },
        ];
        let viewed = vec![ViewedState::Unviewed; diff.files.len()];
        let html = render_page(&ReviewPage {
            target: &target,
            source: ReviewSource::Turns {
                chat: "chat".into(),
                selected: Some(1),
            },
            commits: &[],
            turns: &turns,
            turns_available: true,
            commits_truncated: false,
            commits_error: None,
            state: PageState::Ready {
                diff: &diff,
                viewed: &viewed,
            },
            dark: true,
            hide_noise: false,
            token: "turn-token",
            nonce: "n",
            focus: None,
        });
        assert!(html.contains("data-turn=\"3\" disabled"));
        assert!(html.contains("data-turn=\"2\" disabled"));
        assert!(html.contains("running or incomplete"));
        assert!(html.contains("class=\"commit selected\" data-turn=\"1\">"));
        assert!(html.contains("id=\"tree\"") && html.contains("id=\"files\""));
        assert_eq!(
            parse_ipc(r#"{"type":"review.turn","token":"turn-token","number":1}"#).unwrap(),
            ReviewIpc::SelectTurn {
                token: "turn-token".into(),
                number: 1
            }
        );
        assert!(parse_ipc(r#"{"type":"review.turn","token":"t","number":"--all"}"#).is_err());
    }

    #[test]
    fn commits_page_keeps_fixed_review_slots_and_shows_the_commit_cap() {
        let (mut target, diff) = hostile_diff();
        target.base = Some(BaseChoice {
            reference: "main".into(),
            source: crate::workspace_overview::BaseSource::Local,
            considered: 1,
        });
        let commits = [ReviewCommit {
            sha: "abcdef1".into(),
            short_sha: "abcdef1".into(),
            subject: "Example commit".into(),
            author: "Tracey".into(),
            authored_at: chrono::Utc::now().timestamp() - 3_600,
        }];
        let viewed = vec![ViewedState::Unviewed; diff.files.len()];
        let html = render_page(&ReviewPage {
            target: &target,
            source: ReviewSource::Commits(Some(CommitRange {
                first: "abcdef1".into(),
                last: "abcdef1".into(),
                pending: true,
            })),
            commits: &commits,
            turns: &[],
            turns_available: true,
            commits_truncated: true,
            commits_error: None,
            state: PageState::Ready {
                diff: &diff,
                viewed: &viewed,
            },
            dark: true,
            hide_noise: false,
            token: "t",
            nonce: "n",
            focus: None,
        });
        assert!(html.contains("data-source=\"commits\" data-first-commit=\"abcdef1\""));
        assert!(html.contains("Showing the newest 200 commits; older commits are omitted."));
        assert!(html.contains("<aside id=\"commits\">"));
        assert!(html.contains("id=\"tree\""));
        assert!(html.contains("Example commit"));
        assert!(!html.contains("class=\"open\""));
    }

    // -----------------------------------------------------------------------
    // Real git.

    fn git(dir: &Path, args: &[&str]) {
        let status = crate::agentd::git::git_command()
            // A commit hook's author override would defeat the fixture's user.name.
            .env_remove("GIT_AUTHOR_NAME")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .current_dir(dir)
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed");
    }

    fn git_out(dir: &Path, args: &[&str]) -> String {
        let out = crate::agentd::git::git_command()
            .args(args)
            .current_dir(dir)
            .output()
            .expect("run git");
        assert!(out.status.success(), "git {args:?} failed");
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    fn target(checkout: &Path, base: Option<&str>) -> ReviewTarget {
        ReviewTarget {
            checkout: checkout.to_path_buf(),
            repo: "portal".into(),
            branch: "work/prj-101".into(),
            owner: "coordinator".into(),
            base: base.map(|reference| BaseChoice {
                reference: reference.to_string(),
                source: crate::workspace_overview::BaseSource::Local,
                considered: 1,
            }),
        }
    }

    fn review_fixture() -> tempfile::TempDir {
        let temp = tempfile::Builder::new()
            .prefix("tru188-")
            .tempdir()
            .unwrap();
        git(temp.path(), &["init", "-q", "-b", "main"]);
        git(temp.path(), &["commit", "--allow-empty", "-qm", "base"]);
        git(temp.path(), &["checkout", "-qb", "feature"]);
        temp
    }

    #[tokio::test]
    async fn regression_review_selection_uses_full_ids_despite_short_sha_refs() {
        let temp = review_fixture();
        let dir = temp.path();
        let base = git_out(dir, &["rev-parse", "HEAD"]);
        git(dir, &["commit", "--allow-empty", "-qm", "selected"]);
        let selected = git_out(dir, &["rev-parse", "HEAD"]);
        let short = git_out(dir, &["rev-parse", "--short", "HEAD"]);
        git(dir, &["tag", &short, &base]);
        let (commits, _) = branch_commits(dir, "main").await.unwrap();
        let endpoints = commit_diff_endpoints(
            dir,
            &CommitRange {
                first: commits[0].sha.clone(),
                last: commits[0].sha.clone(),
                pending: false,
            },
        )
        .await
        .unwrap();
        assert_eq!(endpoints, (base, selected.clone()));
        assert_eq!(commits[0].sha, selected);
    }

    #[tokio::test]
    async fn regression_review_range_respects_topology_with_clock_skew() {
        let temp = review_fixture();
        let dir = temp.path();
        let tree = git_out(dir, &["rev-parse", "HEAD^{tree}"]);
        let base = git_out(dir, &["rev-parse", "HEAD"]);
        let dated = |parents: &[&str], date: &str, subject: &str| {
            let mut command = crate::agentd::git::git_command();
            command
                .current_dir(dir)
                .env("GIT_AUTHOR_DATE", date)
                .env("GIT_COMMITTER_DATE", date)
                .args([
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@example.com",
                    "commit-tree",
                    &tree,
                ]);
            for parent in parents {
                command.args(["-p", parent]);
            }
            let out = command.args(["-m", subject]).output().unwrap();
            assert!(out.status.success());
            String::from_utf8(out.stdout).unwrap().trim().to_string()
        };
        let a = dated(&[&base], "2000000000 +0000", "A");
        let b = dated(&[&a], "1000000000 +0000", "B");
        let merge = dated(&[&a, &b], "2100000000 +0000", "merge");
        git(dir, &["update-ref", "HEAD", &merge]);
        let (commits, _) = branch_commits(dir, "main").await.unwrap();
        assert_eq!(
            commits
                .iter()
                .map(|c| c.subject.as_str())
                .collect::<Vec<_>>(),
            ["merge", "B", "A"]
        );
        let endpoints = commit_diff_endpoints(
            dir,
            &CommitRange {
                first: commits[2].sha.clone(),
                last: commits[1].sha.clone(),
                pending: false,
            },
        )
        .await
        .unwrap();
        assert_eq!(endpoints, (base, b));
    }

    #[tokio::test]
    async fn regression_review_metadata_is_bounded() {
        let temp = review_fixture();
        let dir = temp.path();
        let message = dir.join("message");
        std::fs::write(&message, "界".repeat(1024)).unwrap();
        git(
            dir,
            &[
                "-c",
                &format!("user.name={}", "a".repeat(1024)),
                "commit",
                "--allow-empty",
                "-qF",
                message.to_str().unwrap(),
            ],
        );
        let (commits, _) = branch_commits(dir, "main").await.unwrap();
        assert!(commits[0].subject.len() <= 512 && commits[0].subject.ends_with('…'));
        assert!(commits[0].author.len() <= 512 && commits[0].author.ends_with('…'));
        std::fs::write(&message, "x".repeat(128 * 1024)).unwrap();
        git(
            dir,
            &["commit", "--allow-empty", "-qF", message.to_str().unwrap()],
        );
        assert!(
            branch_commits(dir, "main").await.is_err(),
            "oversized record must fail visibly"
        );
    }

    #[tokio::test]
    async fn regression_review_metadata_aggregate_is_bounded() {
        let temp = review_fixture();
        let dir = temp.path();
        let message = dir.join("message");
        std::fs::write(&message, "x".repeat(COMMIT_RECORD_BYTE_CAP - 1024)).unwrap();
        for _ in 0..34 {
            git(
                dir,
                &["commit", "--allow-empty", "-qF", message.to_str().unwrap()],
            );
        }
        let error = branch_commits(dir, "main").await.unwrap_err();
        assert!(error.contains("byte limit"), "{error}");
    }

    #[test]
    fn regression_review_completed_range_does_not_restore_pending_anchor() {
        assert!(parse_ipc(r#"{"type":"review.commit_range","token":"t","first":"abcdef1","last":"abcdef2","pending":true}"#).is_err());
        assert!(matches!(
            parse_ipc(
                r#"{"type":"review.commit_range","token":"t","first":"abcdef1","last":"abcdef1","pending":true}"#
            ),
            Ok(ReviewIpc::SelectCommits { pending: true, .. })
        ));
        let (target, _) = hostile_diff();
        for (last, pending) in [("abcdef2", false), ("abcdef1", false), ("abcdef1", true)] {
            let html = render_page(&ReviewPage {
                target: &target,
                source: ReviewSource::Commits(Some(CommitRange {
                    first: "abcdef1".into(),
                    last: last.into(),
                    pending,
                })),
                commits: &[],
                turns: &[],
                turns_available: false,
                commits_truncated: false,
                commits_error: None,
                state: PageState::Loading,
                dark: true,
                hide_noise: false,
                token: "t",
                nonce: "n",
                focus: None,
            });
            let body = html
                .split("<body")
                .nth(1)
                .unwrap()
                .split('>')
                .next()
                .unwrap();
            assert!(
                body.contains("data-first-commit") == pending,
                "completed range restored its anchor: {body}"
            );
        }
    }

    #[tokio::test]
    async fn branch_commit_list_is_newest_first_and_bounded_to_base() {
        let temp = tempfile::tempdir().unwrap();
        git(temp.path(), &["init", "-q", "-b", "main"]);
        git(
            temp.path(),
            &[
                "-c",
                "user.name=Tracey",
                "-c",
                "user.email=tracey@example.com",
                "commit",
                "--allow-empty",
                "-q",
                "-m",
                "base",
            ],
        );
        git(temp.path(), &["checkout", "-q", "-b", "feature"]);
        for subject in ["first", "second", "third"] {
            git(
                temp.path(),
                &[
                    "-c",
                    "user.name=Tracey",
                    "-c",
                    "user.email=tracey@example.com",
                    "commit",
                    "--allow-empty",
                    "-q",
                    "-m",
                    subject,
                ],
            );
        }
        let (commits, truncated) = branch_commits(temp.path(), "main").await.unwrap();
        assert!(!truncated);
        assert_eq!(
            commits
                .iter()
                .map(|commit| commit.subject.as_str())
                .collect::<Vec<_>>(),
            ["third", "second", "first"]
        );
        assert!(commits.iter().all(|commit| !commit.author.is_empty()));

        for index in 0..COMMIT_LIST_CAP {
            git(
                temp.path(),
                &[
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@example.com",
                    "commit",
                    "--allow-empty",
                    "-q",
                    "-m",
                    &format!("extra-{index}"),
                ],
            );
        }
        let (commits, truncated) = branch_commits(temp.path(), "main").await.unwrap();
        assert!(truncated);
        assert_eq!(commits.len(), COMMIT_LIST_CAP);
        assert_eq!(commits[0].subject, format!("extra-{}", COMMIT_LIST_CAP - 1));
    }

    #[tokio::test]
    async fn commit_diff_endpoints_cover_one_commit_ranges_and_roots() {
        let temp = tempfile::tempdir().unwrap();
        git(temp.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(temp.path().join("one.txt"), "one\n").unwrap();
        git(temp.path(), &["add", "one.txt"]);
        git(
            temp.path(),
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "commit",
                "-q",
                "-m",
                "root",
            ],
        );
        let root = git_out(temp.path(), &["rev-parse", "HEAD"]);
        let one = commit_diff_endpoints(
            temp.path(),
            &CommitRange {
                first: root.clone(),
                last: root.clone(),
                pending: false,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            one.0,
            git_out(temp.path(), &["hash-object", "-t", "tree", "--stdin"])
        );
        assert_eq!(one.1, root);
        std::fs::write(temp.path().join("two.txt"), "two\n").unwrap();
        git(temp.path(), &["add", "two.txt"]);
        git(
            temp.path(),
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "commit",
                "-q",
                "-m",
                "second",
            ],
        );
        let second = git_out(temp.path(), &["rev-parse", "HEAD"]);
        let single = commit_diff_endpoints(
            temp.path(),
            &CommitRange {
                first: second.clone(),
                last: second.clone(),
                pending: false,
            },
        )
        .await
        .unwrap();
        assert_eq!(single, (root.clone(), second.clone()));
        let range = commit_diff_endpoints(
            temp.path(),
            &CommitRange {
                first: root.clone(),
                last: second.clone(),
                pending: false,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            range.0,
            git_out(temp.path(), &["hash-object", "-t", "tree", "--stdin"])
        );
        assert_eq!(range.1, second);
        let changed = git_out(temp.path(), &["diff", "--name-only", &range.0, &range.1]);
        assert_eq!(changed.lines().collect::<Vec<_>>(), ["one.txt", "two.txt"]);
        let marker = temp.path().join("shell-ran");
        let injected = format!("$(touch {})", marker.display());
        assert!(commit_diff_endpoints(
            temp.path(),
            &CommitRange {
                first: injected,
                last: second,
                pending: false
            }
        )
        .await
        .is_err());
        assert!(
            !marker.exists(),
            "git arguments must never be interpreted by a shell"
        );
    }

    #[tokio::test]
    async fn real_git_review_reads_every_kind_of_change() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("portal");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        let body: String = (0..40).map(|i| format!("line {i}\n")).collect();
        std::fs::create_dir(repo.join("src")).unwrap();
        std::fs::write(repo.join("src/a.rs"), "fn a() {}\nfn b() {}\n").unwrap();
        std::fs::write(repo.join("src/moved.rs"), &body).unwrap();
        std::fs::write(repo.join("gone.txt"), "bye\n").unwrap();
        std::fs::write(repo.join("logo.png"), [0u8, 1, 2, 3]).unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "init"]);
        git(&repo, &["checkout", "-q", "-b", "work/prj-101"]);
        std::fs::write(repo.join("src/c.rs"), "fn c() {}\n").unwrap();
        git(&repo, &["add", "src/c.rs"]);
        git(&repo, &["commit", "-q", "-m", "work"]);
        // Uncommitted: modified, renamed with an edit, deleted, binary,
        // untracked text (one past the line cap) and untracked binary.
        std::fs::write(repo.join("src/a.rs"), "fn a() {}\nfn b2() {}\n").unwrap();
        git(&repo, &["mv", "src/moved.rs", "src/renamed.rs"]);
        std::fs::write(repo.join("src/renamed.rs"), format!("{body}tail\n")).unwrap();
        git(&repo, &["add", "src/renamed.rs"]);
        std::fs::remove_file(repo.join("gone.txt")).unwrap();
        std::fs::write(repo.join("logo.png"), [0u8, 9, 9, 9]).unwrap();
        let long: String = (0..FILE_LINE_CAP + 1).map(|i| format!("n {i}\n")).collect();
        std::fs::write(repo.join("notes.md"), &long).unwrap();
        std::fs::write(repo.join("blob.dat"), [7u8, 0, 7]).unwrap();
        std::fs::write(repo.join("Cargo.lock"), "# generated\n").unwrap();

        let load = load_review(target(&repo, Some("main")), DiffSource::Uncommitted)
            .await
            .unwrap();
        let diff = &load.diff;
        let by_path = |path: &str| diff.file(path).unwrap_or_else(|| panic!("{path} missing"));
        let paths: Vec<&str> = diff.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "src/a.rs",
                "src/renamed.rs",
                "Cargo.lock",
                "blob.dat",
                "gone.txt",
                "logo.png",
                "notes.md"
            ]
        );
        let a = by_path("src/a.rs");
        assert_eq!(
            (a.status, a.kind, a.additions, a.deletions),
            (FileStatus::Modified, FileKind::Code, 1, 1)
        );
        let ops: Vec<(LineOp, &str)> = a.hunks[0]
            .lines
            .iter()
            .map(|l| (l.op, l.text.as_str()))
            .collect();
        assert!(
            ops.contains(&(LineOp::Del, "fn b() {}")) && ops.contains(&(LineOp::Add, "fn b2() {}"))
        );
        // The working-tree hash, as git computes it.
        assert_eq!(
            a.content_id.as_deref(),
            Some(git_out(&repo, &["hash-object", "src/a.rs"]).as_str())
        );
        let renamed = by_path("src/renamed.rs");
        assert_eq!(renamed.status, FileStatus::Renamed);
        assert_eq!(renamed.old_path.as_deref(), Some("src/moved.rs"));
        assert_eq!(renamed.additions, 1);
        assert!(renamed.hunks[0]
            .lines
            .iter()
            .any(|l| l.op == LineOp::Add && l.text == "tail"));
        let gone = by_path("gone.txt");
        assert_eq!(gone.status, FileStatus::Deleted);
        assert_eq!(gone.hunks[0].lines[0].text, "bye");
        assert!(gone.content_id.as_deref().unwrap().starts_with("deleted:"));
        let logo = by_path("logo.png");
        assert_eq!(logo.status, FileStatus::Binary);
        assert!(logo.hunks.is_empty());
        let notes = by_path("notes.md");
        assert_eq!(
            (notes.status, notes.kind),
            (FileStatus::Untracked, FileKind::Docs)
        );
        assert!(notes.truncated);
        assert_eq!(notes.shown_lines(), FILE_LINE_CAP);
        let blob = by_path("blob.dat");
        assert_eq!(blob.status, FileStatus::Binary);
        assert!(blob.hunks.is_empty() && blob.content_id.is_some());
        let lock = by_path("Cargo.lock");
        assert_eq!((lock.kind, lock.additions), (FileKind::Generated, 1));
        // Binary assets classify as code, as in T3.
        assert_eq!(diff.totals.get(FileKind::Code).files, 4);
        assert_eq!(diff.merge_base, None);
        // Both sources' paths are live: src/c.rs is only in vs base.
        let live = load.live_paths.as_ref().unwrap();
        assert!(live.contains("src/c.rs") && live.contains("notes.md"));

        let vs_base = load_review(target(&repo, Some("main")), DiffSource::VsBase)
            .await
            .unwrap();
        let paths: Vec<&str> = vs_base.diff.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["src/c.rs"]);
        let c = &vs_base.diff.files[0];
        assert_eq!(c.status, FileStatus::Added);
        // A committed version's id is its blob.
        assert_eq!(
            c.content_id.as_deref(),
            Some(git_out(&repo, &["rev-parse", "HEAD:src/c.rs"]).as_str())
        );
        assert_eq!(vs_base.diff.merge_base.as_ref().map(String::len), Some(7));
        assert!(vs_base.live_paths.unwrap().contains("notes.md"));
        assert!(load_review(target(&repo, None), DiffSource::VsBase)
            .await
            .is_err());
    }

    // -----------------------------------------------------------------------
    // Opening from a chat (TRU-179).

    #[test]
    fn chat_review_path_takes_only_normalized_absolute_paths() {
        let ok =
            serde_json::json!({"type": "open_review", "path": "/tmp/acme/src/a b.rs", "tabId": 3});
        assert_eq!(
            chat_review_path(&ok).unwrap(),
            PathBuf::from("/tmp/acme/src/a b.rs")
        );
        for bad in [
            serde_json::json!({"type": "open_review"}),
            serde_json::json!({"type": "open_review", "path": 7}),
            serde_json::json!({"type": "open_review", "path": ""}),
            serde_json::json!({"type": "open_review", "path": "src/a.rs"}),
            serde_json::json!({"type": "open_review", "path": "/tmp/acme/../etc/passwd"}),
            serde_json::json!({"type": "open_review", "path": "/tmp/acme/./a.rs"}),
            serde_json::json!({"type": "open_review", "path": "/tmp/acme/a\u{0}.rs"}),
        ] {
            assert!(chat_review_path(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn checkout_is_the_nearest_git_ancestor_and_a_worktree_is_its_own() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("acme");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("src/deep")).unwrap();
        // A linked worktree inside the main checkout has a `.git` file.
        let worktree = repo.join(".worktrees/acme-prj-7");
        std::fs::create_dir_all(worktree.join("src")).unwrap();
        std::fs::write(
            worktree.join(".git"),
            "gitdir: ../../.git/worktrees/acme-prj-7\n",
        )
        .unwrap();
        assert_eq!(
            checkout_containing(&repo.join("src/deep/a.rs")),
            Some(repo.clone())
        );
        // The file need not exist (a Write that failed, a deleted file).
        assert_eq!(
            checkout_containing(&repo.join("new/b.rs")),
            Some(repo.clone())
        );
        assert_eq!(
            checkout_containing(&worktree.join("src/a.rs")),
            Some(worktree.clone())
        );
        let outside = temp.path().join("loose/a.rs");
        assert_eq!(
            checkout_containing(&outside).filter(|found| found.starts_with(temp.path())),
            None
        );
        assert_eq!(
            checkout_relative(&worktree, &worktree.join("src/a.rs")).unwrap(),
            "src/a.rs"
        );
        assert!(checkout_relative(&worktree, &repo.join("src/a.rs")).is_err());
        assert!(checkout_relative(&worktree, &worktree).is_err());
    }

    #[test]
    fn edit_review_prefers_uncommitted_then_vs_base() {
        assert_eq!(edit_review_source(true, true), DiffSource::Uncommitted);
        assert_eq!(edit_review_source(false, true), DiffSource::VsBase);
        assert_eq!(edit_review_source(false, false), DiffSource::Uncommitted);
    }

    #[test]
    fn page_scrolls_to_a_listed_focus_and_notes_a_missing_one() {
        let (target, mut diff) = hostile_diff();
        diff.files = vec![file("src/a.rs", FileKind::Code, 1, 0)];
        diff.totals = ReviewTotals::of(&diff.files);
        let viewed = [ViewedState::Unviewed];
        let page = |focus| {
            render_page(&ReviewPage {
                target: &target,
                source: ReviewSource::Uncommitted,
                commits: &[],
                turns: &[],
                turns_available: true,
                commits_truncated: false,
                commits_error: None,
                state: PageState::Ready {
                    diff: &diff,
                    viewed: &viewed,
                },
                dark: true,
                hide_noise: false,
                token: "t",
                nonce: "n",
                focus,
            })
        };
        let listed = page(Some("src/a.rs"));
        assert!(listed.contains("data-focus=\"src/a.rs\""));
        assert!(!listed.contains("class=\"focus-note\""));
        let missing = page(Some("src/<b>.rs"));
        assert!(!missing.contains("data-focus="));
        assert!(missing.contains(
            "<div class=\"focus-note\">src/&lt;b&gt;.rs is not in the uncommitted changes.</div>"
        ));
        let none = page(None);
        assert!(!none.contains("data-focus=") && !none.contains("class=\"focus-note\""));
    }

    #[tokio::test]
    async fn real_git_edit_review_resolves_a_worktree_file() {
        let temp = tempfile::tempdir().unwrap();
        let root = crate::worktree_follow::resolve_for_match(temp.path());
        let repo = root.join("acme");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("a.txt"), "one\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "init"]);
        let worktree = root.join("acme-prj-7");
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "prj-7",
                worktree.to_str().unwrap(),
                "main",
            ],
        );
        std::fs::create_dir(worktree.join("src")).unwrap();
        std::fs::write(worktree.join("src/done.rs"), "fn done() {}\n").unwrap();
        git(&worktree, &["add", "."]);
        git(&worktree, &["commit", "-q", "-m", "done"]);
        std::fs::write(worktree.join("src/[wip].rs"), "fn wip() {}\n").unwrap();

        let wip = worktree.join("src/[wip].rs");
        let checkout = checkout_containing(&wip).unwrap();
        assert_eq!(checkout, worktree);
        let open = resolve_edit_review(checkout.clone(), wip, None)
            .await
            .unwrap();
        assert_eq!(open.focus, "src/[wip].rs");
        assert_eq!(open.source, DiffSource::Uncommitted, "untracked counts");
        assert_eq!(open.target.checkout, worktree);
        assert_eq!(open.target.repo, "acme");
        assert_eq!(open.target.branch, "prj-7");
        assert_eq!(
            open.target.base.as_ref().map(|b| b.reference.as_str()),
            Some("main")
        );

        // Committed, clean: vs base, which lists it.
        let done = resolve_edit_review(checkout, worktree.join("src/done.rs"), None)
            .await
            .unwrap();
        assert_eq!(done.source, DiffSource::VsBase);
        let load = load_review(done.target.clone(), done.source).await.unwrap();
        assert!(load.diff.file(&done.focus).is_some());

        // A row's target is used as it is.
        let known = target(&worktree, None);
        let open = resolve_edit_review(worktree.clone(), worktree.join("a.txt"), Some(known))
            .await
            .unwrap();
        assert_eq!(open.target.owner, "coordinator");
        assert_eq!(open.source, DiffSource::Uncommitted, "no base to compare");
    }
}
