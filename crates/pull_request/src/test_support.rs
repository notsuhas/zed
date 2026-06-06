use crate::review_provider::FileChangeStatus;
use gpui::SharedString;

#[derive(Clone)]
pub struct VisualReviewPanelState {
    pub pull_request: VisualPullRequest,
    pub files: Vec<VisualPullRequestFile>,
    pub comments: Vec<VisualReviewComment>,
    pub tree_view: bool,
    pub collapsed_threads: Vec<u64>,
    pub draft: Option<VisualInlineCommentDraft>,
}

#[derive(Clone)]
pub struct VisualPullRequest {
    pub number: u32,
    pub title: SharedString,
    pub author: SharedString,
    pub base_ref: SharedString,
    pub head_ref: SharedString,
    pub head_sha: SharedString,
}

#[derive(Clone)]
pub struct VisualPullRequestFile {
    pub path: SharedString,
    pub status: VisualFileStatus,
    pub additions: u32,
    pub deletions: u32,
}

#[derive(Clone)]
pub enum VisualFileStatus {
    Added,
    Modified,
    Deleted,
    Renamed { from: SharedString },
}

impl From<VisualFileStatus> for FileChangeStatus {
    fn from(status: VisualFileStatus) -> Self {
        match status {
            VisualFileStatus::Added => Self::Added,
            VisualFileStatus::Modified => Self::Modified,
            VisualFileStatus::Deleted => Self::Deleted,
            VisualFileStatus::Renamed { from } => Self::Renamed { from },
        }
    }
}

#[derive(Clone)]
pub struct VisualReviewComment {
    pub id: u64,
    pub author: SharedString,
    pub body: SharedString,
    pub created_at: SharedString,
    pub path: SharedString,
    pub line: u32,
    pub reply_to: Option<u64>,
}

#[derive(Clone)]
pub enum VisualInlineCommentDraft {
    NewThread {
        path: SharedString,
        line: u32,
        body: SharedString,
    },
    Reply {
        comment_id: u64,
        body: SharedString,
    },
}
