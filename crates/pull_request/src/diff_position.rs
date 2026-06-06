//! Mapping between GitHub review-comment diff positions and editor lines.
//!
//! Two mechanisms, mirroring the VS Code extension:
//!
//! 1. **Line-based (preferred).** Modern GitHub returns explicit `line`/
//!    `startLine` (new side) and `originalLine`/`originalStartLine` (old side)
//!    on a thread, plus a `diffSide`. [`thread_anchor`] turns those into a
//!    0-based editor row range on the correct side. Used for live threads.
//! 2. **Hunk/position-based (fallback).** For outdated threads (or to render
//!    the contextual snippet), [`parse_diff_hunk`] parses a unified `diffHunk`
//!    into [`DiffLine`]s and [`position_to_line`] resolves a GitHub `position`
//!    offset to a file line.
//!
//! Wired into the diff editor by the inline-comment layer; allowed dead-code
//! until that integration lands.
#![allow(dead_code)]

use crate::provider::{DiffSide, ReviewThread};

/// The kind of change a unified-diff line represents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffChangeType {
    Context,
    Add,
    Delete,
    /// Hunk header (`@@ … @@`) or other control line.
    Control,
}

/// A single line within a parsed diff hunk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffLine {
    pub change_type: DiffChangeType,
    /// 1-based line number on the old (base) side, if applicable.
    pub old_line: Option<u32>,
    /// 1-based line number on the new (head) side, if applicable.
    pub new_line: Option<u32>,
    /// GitHub "position": 0-based offset of this line from the hunk header
    /// within the unified diff.
    pub position_in_hunk: u32,
}

/// A parsed diff hunk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffHunk {
    pub old_start: u32,
    pub old_len: u32,
    pub new_start: u32,
    pub new_len: u32,
    pub lines: Vec<DiffLine>,
}

fn change_type_of(line: &str) -> DiffChangeType {
    match line.as_bytes().first() {
        Some(b' ') => DiffChangeType::Context,
        Some(b'+') => DiffChangeType::Add,
        Some(b'-') => DiffChangeType::Delete,
        _ => DiffChangeType::Control,
    }
}

/// Parse a `@@ -old_start,old_len +new_start,new_len @@` header.
fn parse_hunk_header(line: &str) -> Option<(u32, u32, u32, u32)> {
    let rest = line.strip_prefix("@@ ")?;
    let end = rest.find(" @@")?;
    let ranges = &rest[..end];
    let (old_part, new_part) = ranges.split_once(' ')?;
    let old = old_part.strip_prefix('-')?;
    let new = new_part.strip_prefix('+')?;

    fn parse_range(range: &str) -> Option<(u32, u32)> {
        match range.split_once(',') {
            Some((start, len)) => Some((start.parse().ok()?, len.parse().ok()?)),
            None => Some((range.parse().ok()?, 1)),
        }
    }
    let (old_start, old_len) = parse_range(old)?;
    let (new_start, new_len) = parse_range(new)?;
    Some((old_start, old_len, new_start, new_len))
}

/// Parse a unified `diffHunk` string (one or more `@@` headers) into hunks.
pub fn parse_diff_hunk(diff_hunk: &str) -> Vec<DiffHunk> {
    let mut hunks: Vec<DiffHunk> = Vec::new();
    let mut old_line = 0u32;
    let mut new_line = 0u32;
    let mut position = 0u32;

    for raw in diff_hunk.lines() {
        if let Some((old_start, old_len, new_start, new_len)) = parse_hunk_header(raw) {
            hunks.push(DiffHunk {
                old_start,
                old_len,
                new_start,
                new_len,
                lines: Vec::new(),
            });
            old_line = old_start;
            new_line = new_start;
            // The header line itself is position 0 of the hunk.
            position = 0;
            continue;
        }

        let Some(hunk) = hunks.last_mut() else {
            // Skip any preamble before the first header.
            continue;
        };

        position += 1;
        let change_type = change_type_of(raw);
        let (old, new) = match change_type {
            DiffChangeType::Context => {
                let entry = (Some(old_line), Some(new_line));
                old_line += 1;
                new_line += 1;
                entry
            }
            DiffChangeType::Add => {
                let entry = (None, Some(new_line));
                new_line += 1;
                entry
            }
            DiffChangeType::Delete => {
                let entry = (Some(old_line), None);
                old_line += 1;
                entry
            }
            DiffChangeType::Control => (None, None),
        };
        hunk.lines.push(DiffLine {
            change_type,
            old_line: old,
            new_line: new,
            position_in_hunk: position,
        });
    }

    hunks
}

/// Resolve a GitHub `position` (offset within the diff hunk) to a file line on
/// the given side. Used for outdated/legacy comments.
pub fn position_to_line(hunks: &[DiffHunk], position: u32, side: DiffSide) -> Option<u32> {
    for hunk in hunks {
        if let Some(line) = hunk
            .lines
            .iter()
            .find(|line| line.position_in_hunk == position)
        {
            return match side {
                DiffSide::Left => line.old_line,
                DiffSide::Right => line.new_line,
            };
        }
    }
    None
}

/// A 0-based, inclusive editor row range for a review thread, on a side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThreadAnchor {
    pub side: DiffSide,
    pub start_row: u32,
    pub end_row: u32,
}

/// Compute the editor row range for a live thread using GitHub's explicit line
/// numbers. Returns `None` when the thread carries no line for its side (e.g.
/// a file-level or fully outdated thread that must fall back to the hunk).
pub fn thread_anchor(thread: &ReviewThread) -> Option<ThreadAnchor> {
    let (end_line, start_line) = match thread.diff_side {
        DiffSide::Right => (thread.line, thread.start_line),
        DiffSide::Left => (thread.original_line, thread.original_start_line),
    };
    let end_line = end_line?;
    let start_line = start_line.unwrap_or(end_line);
    // GitHub line numbers are 1-based; editor rows are 0-based.
    let start_row = start_line.saturating_sub(1);
    let end_row = end_line.saturating_sub(1);
    Some(ThreadAnchor {
        side: thread.diff_side,
        start_row: start_row.min(end_row),
        end_row: start_row.max(end_row),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ReviewThread;
    use gpui::SharedString;

    fn thread(
        side: DiffSide,
        line: Option<u32>,
        start_line: Option<u32>,
        original_line: Option<u32>,
    ) -> ReviewThread {
        ReviewThread {
            id: SharedString::from("t"),
            path: SharedString::from("f"),
            diff_side: side,
            line,
            start_line,
            original_line,
            original_start_line: None,
            is_resolved: false,
            is_outdated: false,
            viewer_can_resolve: false,
            viewer_can_unresolve: false,
            comments: Vec::new(),
        }
    }

    #[test]
    fn parses_hunk_header_and_line_numbers() {
        let hunk = "@@ -10,3 +10,4 @@ fn main()\n ctx1\n-removed\n+added1\n+added2\n ctx2";
        let hunks = parse_diff_hunk(hunk);
        assert_eq!(hunks.len(), 1);
        let h = &hunks[0];
        assert_eq!((h.old_start, h.old_len, h.new_start, h.new_len), (10, 3, 10, 4));

        // " ctx1" — context, old 10 / new 10, position 1
        assert_eq!(h.lines[0].change_type, DiffChangeType::Context);
        assert_eq!(h.lines[0].old_line, Some(10));
        assert_eq!(h.lines[0].new_line, Some(10));
        assert_eq!(h.lines[0].position_in_hunk, 1);

        // "-removed" — delete, old 11, no new, position 2
        assert_eq!(h.lines[1].change_type, DiffChangeType::Delete);
        assert_eq!(h.lines[1].old_line, Some(11));
        assert_eq!(h.lines[1].new_line, None);

        // "+added1" — add, new 11, position 3
        assert_eq!(h.lines[2].change_type, DiffChangeType::Add);
        assert_eq!(h.lines[2].new_line, Some(11));
        assert_eq!(h.lines[2].position_in_hunk, 3);

        // "+added2" — add, new 12
        assert_eq!(h.lines[3].new_line, Some(12));

        // " ctx2" — context, old 12 / new 13
        assert_eq!(h.lines[4].old_line, Some(12));
        assert_eq!(h.lines[4].new_line, Some(13));
    }

    #[test]
    fn position_resolves_to_correct_side() {
        let hunk = "@@ -10,3 +10,4 @@\n ctx1\n-removed\n+added1\n+added2\n ctx2";
        let hunks = parse_diff_hunk(hunk);
        // position 3 is "+added1" → new line 11 on the right, nothing on left.
        assert_eq!(position_to_line(&hunks, 3, DiffSide::Right), Some(11));
        assert_eq!(position_to_line(&hunks, 3, DiffSide::Left), None);
        // position 2 is "-removed" → old line 11 on the left.
        assert_eq!(position_to_line(&hunks, 2, DiffSide::Left), Some(11));
        assert_eq!(position_to_line(&hunks, 99, DiffSide::Right), None);
    }

    #[test]
    fn parses_multiple_hunks() {
        let hunk = "@@ -1,1 +1,1 @@\n-a\n+b\n@@ -20,2 +20,2 @@\n ctx\n+new";
        let hunks = parse_diff_hunk(hunk);
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[1].new_start, 20);
        assert_eq!(hunks[1].lines[1].new_line, Some(21));
    }

    #[test]
    fn single_line_thread_right_side_is_zero_based() {
        let anchor = thread_anchor(&thread(DiffSide::Right, Some(42), None, None)).unwrap();
        assert_eq!(anchor.side, DiffSide::Right);
        assert_eq!(anchor.start_row, 41);
        assert_eq!(anchor.end_row, 41);
    }

    #[test]
    fn multi_line_thread_uses_start_and_end() {
        let anchor =
            thread_anchor(&thread(DiffSide::Right, Some(50), Some(45), None)).unwrap();
        assert_eq!(anchor.start_row, 44);
        assert_eq!(anchor.end_row, 49);
    }

    #[test]
    fn left_side_thread_uses_original_line() {
        let anchor = thread_anchor(&thread(DiffSide::Left, None, None, Some(7))).unwrap();
        assert_eq!(anchor.side, DiffSide::Left);
        assert_eq!(anchor.start_row, 6);
        assert_eq!(anchor.end_row, 6);
    }

    #[test]
    fn thread_without_line_for_side_is_none() {
        assert!(thread_anchor(&thread(DiffSide::Right, None, None, Some(7))).is_none());
    }
}
