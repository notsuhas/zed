//! Build a collapsible directory tree from a flat list of changed-file paths,
//! mirroring the VS Code "Files Changed" tree layout.

use crate::provider::PullRequestFile;
use std::collections::{BTreeMap, HashSet};

/// A single rendered row in the file tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeRow {
    /// Indentation depth (0 at the root).
    pub depth: usize,
    /// Display name (the path segment for this row).
    pub name: String,
    /// `true` for a directory row, `false` for a file row.
    pub is_dir: bool,
    /// For directory rows: the full path, used as the collapse key.
    pub dir_path: String,
    /// For file rows: index into the original `files` slice.
    pub file_index: Option<usize>,
}

#[derive(Default)]
struct Node {
    dirs: BTreeMap<String, Node>,
    /// (file name, index into `files`).
    files: Vec<(String, usize)>,
}

impl Node {
    fn insert(&mut self, segments: &[&str], index: usize) {
        match segments {
            [] => {}
            [file] => self.files.push(((*file).to_string(), index)),
            [dir, rest @ ..] => {
                self.dirs
                    .entry((*dir).to_string())
                    .or_default()
                    .insert(rest, index);
            }
        }
    }

    fn emit(&self, depth: usize, prefix: &str, collapsed: &HashSet<String>, rows: &mut Vec<TreeRow>) {
        for (name, child) in &self.dirs {
            let dir_path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            rows.push(TreeRow {
                depth,
                name: name.clone(),
                is_dir: true,
                dir_path: dir_path.clone(),
                file_index: None,
            });
            if !collapsed.contains(&dir_path) {
                child.emit(depth + 1, &dir_path, collapsed, rows);
            }
        }
        for (name, index) in &self.files {
            rows.push(TreeRow {
                depth,
                name: name.clone(),
                is_dir: false,
                dir_path: String::new(),
                file_index: Some(*index),
            });
        }
    }
}

/// Build the visible tree rows for `files`, hiding the children of any
/// directory whose full path is in `collapsed`. Directories sort before files
/// at each level (both alphabetically).
pub fn build_tree_rows(files: &[PullRequestFile], collapsed: &HashSet<String>) -> Vec<TreeRow> {
    let mut root = Node::default();
    for (index, file) in files.iter().enumerate() {
        let segments: Vec<&str> = file.path.split('/').filter(|s| !s.is_empty()).collect();
        root.insert(&segments, index);
    }
    let mut rows = Vec::new();
    root.emit(0, "", collapsed, &mut rows);
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{FileChangeStatus, PullRequestFile, ViewedState};

    fn file(path: &str) -> PullRequestFile {
        PullRequestFile {
            path: path.into(),
            status: FileChangeStatus::Modified,
            additions: 0,
            deletions: 0,
            viewed_state: ViewedState::Unviewed,
        }
    }

    #[test]
    fn builds_nested_tree_dirs_before_files() {
        let files = vec![
            file("src/main.rs"),
            file("src/ui/panel.rs"),
            file("README.md"),
        ];
        let rows = build_tree_rows(&files, &HashSet::new());
        let shape: Vec<(usize, &str, bool)> = rows
            .iter()
            .map(|r| (r.depth, r.name.as_str(), r.is_dir))
            .collect();
        assert_eq!(
            shape,
            vec![
                (0, "src", true),
                (1, "ui", true),
                (2, "panel.rs", false),
                (1, "main.rs", false),
                (0, "README.md", false),
            ]
        );
    }

    #[test]
    fn collapsing_a_directory_hides_its_children() {
        let files = vec![file("src/main.rs"), file("src/ui/panel.rs"), file("top.rs")];
        let mut collapsed = HashSet::new();
        collapsed.insert("src".to_string());
        let rows = build_tree_rows(&files, &collapsed);
        let shape: Vec<(usize, &str, bool)> = rows
            .iter()
            .map(|r| (r.depth, r.name.as_str(), r.is_dir))
            .collect();
        assert_eq!(shape, vec![(0, "src", true), (0, "top.rs", false)]);
    }

    #[test]
    fn file_rows_carry_their_index() {
        let files = vec![file("a.rs"), file("dir/b.rs")];
        let rows = build_tree_rows(&files, &HashSet::new());
        // dir first, then its file, then root file.
        assert_eq!(rows[0].name, "dir");
        assert_eq!(rows[1].file_index, Some(1));
        assert_eq!(rows[2].file_index, Some(0));
    }
}
