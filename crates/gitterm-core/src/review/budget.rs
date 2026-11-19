use std::collections::HashSet;

use super::model::FileKind;

/// Lines of one file shown before it is cut (`truncated`).
pub const FILE_LINE_CAP: usize = 2_000;
/// Bytes of one file's text shown before it is cut (`truncated`).
pub const FILE_BYTE_CAP: usize = 256 * 1024;
/// Changed lines admitted for display across the whole diff, each file
/// costing at most [`FILE_LINE_CAP`]. Files that do not fit are `omitted`.
pub const ADMIT_LINE_BUDGET: usize = 20_000;
/// Hard stop on rendered lines (context included) across the diff.
pub const TOTAL_LINE_CAP: usize = 30_000;
/// Hard stop on rendered text bytes across the diff.
pub const TOTAL_BYTE_CAP: usize = 4 * 1024 * 1024;
/// Files listed in the pane; the rest are only counted (`unlisted`).
pub const FILE_LIST_CAP: usize = 2_000;
/// Untracked names read; past this the list is incomplete and viewed marks
/// are not pruned.
pub const UNTRACKED_SCAN_CAP: usize = 10_000;
/// Branch commits shown in the Commits source. One extra row detects truncation.
pub const COMMIT_LIST_CAP: usize = 200;
/// Input caps include separators; excessive metadata fails visibly.
pub const COMMIT_RECORD_BYTE_CAP: usize = 64 * 1024;
pub const COMMIT_TOTAL_BYTE_CAP: usize = 2 * 1024 * 1024;
pub const COMMIT_TEXT_BYTE_CAP: usize = 512;

/// Running totals shared by the patch and the untracked files.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    pub lines: usize,
    pub bytes: usize,
}

impl Budget {
    pub fn exhausted(&self) -> bool {
        self.lines >= TOTAL_LINE_CAP || self.bytes >= TOTAL_BYTE_CAP
    }
}

/// Where a line goes: stored, cut short, or dropped.
pub(super) enum Admit {
    Whole,
    Cut(usize),
    Drop,
}

/// What fits of a `len`-byte line into a file holding `lines` / `bytes`
/// under the per-file and total caps.
pub(super) fn admit_line(lines: usize, bytes: usize, len: usize, total: &Budget) -> Admit {
    if lines >= FILE_LINE_CAP || total.exhausted() {
        return Admit::Drop;
    }
    let room = (FILE_BYTE_CAP - bytes.min(FILE_BYTE_CAP)).min(TOTAL_BYTE_CAP - total.bytes);
    if len <= room {
        Admit::Whole
    } else if room > 0 {
        Admit::Cut(room)
    } else {
        Admit::Drop
    }
}

/// `text` cut to at most `max` bytes on a char boundary.
pub(super) fn cut_to(text: &str, max: usize) -> &str {
    let mut end = max.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Which files are shown: by kind order, then the order given, each costing
/// its changed lines up to [`FILE_LINE_CAP`], while they fit `budget`. A
/// file that does not fit is skipped and smaller ones after it may still
/// fit. Returns the admitted indices.
pub fn admit(costs: &[(FileKind, usize)], budget: usize) -> HashSet<usize> {
    let mut order: Vec<usize> = (0..costs.len()).collect();
    order.sort_by_key(|&index| costs[index].0.index());
    let mut left = budget;
    let mut admitted = HashSet::new();
    for index in order {
        let cost = costs[index].1.min(FILE_LINE_CAP);
        if cost <= left {
            left -= cost;
            admitted.insert(index);
        }
    }
    admitted
}
