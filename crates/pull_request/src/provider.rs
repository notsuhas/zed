//! Forge-agnostic pull-request review provider.
//!
//! The UI layer talks only to [`PullRequestProvider`] and the domain types in
//! this module; concrete forges (GitHub today, GitLab later) implement the
//! trait. Domain types are intentionally decoupled from any forge's wire
//! format so the UI never sees GitHub/GitLab specifics.
//!
//! Parts of the trait/type surface (reactions, merge, create, timeline, …)
//! are implemented ahead of being wired into the panel; dead-code is allowed
//! crate-side until each is consumed.
#![allow(dead_code)]

use anyhow::Result;
use gpui::SharedString;
use std::future::Future;
use std::pin::Pin;

/// Boxed, `Send` future returned by every provider method. Providers are object
/// types behind `Arc<dyn PullRequestProvider>`, so async methods can't use
/// `impl Future`; we box them instead of pulling in `async-trait`.
pub type ProviderFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

/// A user/organization actor (comment author, reviewer, assignee).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Actor {
    pub login: SharedString,
    pub avatar_url: Option<SharedString>,
}

/// Lifecycle state of a pull request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PullRequestState {
    Open,
    Closed,
    Merged,
}

/// Which side of a diff a line/thread refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffSide {
    /// The base (original) revision.
    Left,
    /// The head (modified) revision.
    Right,
}

/// Per-file change kind within a pull request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileChangeStatus {
    Added,
    Modified,
    Deleted,
    Renamed { from: SharedString },
    Copied { from: SharedString },
}

/// Whether the current viewer has marked a file as viewed on the server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewedState {
    Viewed,
    Unviewed,
    /// Server marked the file's prior "viewed" state as dismissed because the
    /// file changed since it was viewed.
    Dismissed,
}

/// A reaction (emoji) group on a reactable subject (PR body, comment).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReactionGroup {
    pub content: SharedString,
    pub viewer_has_reacted: bool,
    pub count: u32,
}

/// Aggregate CI status for a commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckStatus {
    Pending,
    Success,
    Failure,
    Error,
    Cancelled,
    Neutral,
    Skipped,
}

/// A single CI check / status context.
#[derive(Clone, Debug)]
pub struct CheckRun {
    pub name: SharedString,
    pub status: CheckStatus,
    pub url: Option<SharedString>,
    pub description: Option<SharedString>,
}

/// Reviewer's latest review verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewVerdict {
    Pending,
    Commented,
    Approved,
    ChangesRequested,
    Dismissed,
}

/// A reviewer and their latest verdict (if any).
#[derive(Clone, Debug)]
pub struct Reviewer {
    pub actor: Actor,
    pub verdict: Option<ReviewVerdict>,
}

/// The event a submitted review carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewEvent {
    Comment,
    Approve,
    RequestChanges,
}

impl ReviewEvent {
    pub fn graphql(self) -> &'static str {
        match self {
            ReviewEvent::Comment => "COMMENT",
            ReviewEvent::Approve => "APPROVE",
            ReviewEvent::RequestChanges => "REQUEST_CHANGES",
        }
    }
}

/// How a pull request should be merged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeMethod {
    Merge,
    Squash,
    Rebase,
}

impl MergeMethod {
    pub fn graphql(self) -> &'static str {
        match self {
            MergeMethod::Merge => "MERGE",
            MergeMethod::Squash => "SQUASH",
            MergeMethod::Rebase => "REBASE",
        }
    }
}

/// Stable identity of a pull request: GitHub's node id (for mutations) plus the
/// human-facing number, owner, and repo (for REST/lookup).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullRequestId {
    pub node_id: SharedString,
    pub number: u32,
    pub owner: SharedString,
    pub repo: SharedString,
}

/// Summary of a pull request as shown in the list view.
#[derive(Clone, Debug)]
pub struct PullRequestInfo {
    pub id: PullRequestId,
    pub title: SharedString,
    pub author: Actor,
    pub state: PullRequestState,
    pub is_draft: bool,
    pub base_ref: SharedString,
    pub head_ref: SharedString,
    pub base_sha: SharedString,
    pub head_sha: SharedString,
    pub created_at: SharedString,
    pub updated_at: SharedString,
    pub additions: u32,
    pub deletions: u32,
    pub comment_count: u32,
    pub url: SharedString,
}

/// Full pull-request detail backing the overview screen.
#[derive(Clone, Debug)]
pub struct PullRequest {
    pub info: PullRequestInfo,
    pub body: SharedString,
    pub labels: Vec<SharedString>,
    pub assignees: Vec<Actor>,
    pub reviewers: Vec<Reviewer>,
    pub milestone: Option<SharedString>,
    pub mergeable: Option<bool>,
    /// e.g. "CLEAN", "BLOCKED", "BEHIND", "DIRTY", "UNSTABLE".
    pub merge_state_status: Option<SharedString>,
    pub checks: Vec<CheckRun>,
    pub check_rollup: Option<CheckStatus>,
    pub reactions: Vec<ReactionGroup>,
    pub viewer_can_update: bool,
    pub viewer_can_merge: bool,
}

/// A file changed in a pull request.
#[derive(Clone, Debug)]
pub struct PullRequestFile {
    pub path: SharedString,
    pub status: FileChangeStatus,
    pub additions: u32,
    pub deletions: u32,
    pub viewed_state: ViewedState,
}

/// A single inline review comment within a thread.
#[derive(Clone, Debug)]
pub struct ReviewComment {
    /// GraphQL node id (for mutations).
    pub id: SharedString,
    /// REST `databaseId` (for reply targets / REST fallbacks).
    pub database_id: Option<u64>,
    pub author: Actor,
    pub body: SharedString,
    pub diff_hunk: Option<SharedString>,
    pub created_at: SharedString,
    pub reactions: Vec<ReactionGroup>,
    pub viewer_can_update: bool,
    pub viewer_can_delete: bool,
    /// The pending review this comment belongs to, if any.
    pub review_database_id: Option<u64>,
}

/// An inline review thread anchored to a file/line range.
#[derive(Clone, Debug)]
pub struct ReviewThread {
    pub id: SharedString,
    pub path: SharedString,
    pub diff_side: DiffSide,
    /// 1-based end line on the thread's side (new side for `Right`).
    pub line: Option<u32>,
    /// 1-based start line for multi-line threads.
    pub start_line: Option<u32>,
    pub original_line: Option<u32>,
    pub original_start_line: Option<u32>,
    pub is_resolved: bool,
    pub is_outdated: bool,
    pub viewer_can_resolve: bool,
    pub viewer_can_unresolve: bool,
    pub comments: Vec<ReviewComment>,
}

/// An entry in a pull request's activity timeline.
#[derive(Clone, Debug)]
pub enum TimelineItem {
    Commit {
        oid: SharedString,
        message: SharedString,
        author: Option<Actor>,
    },
    Review {
        author: Actor,
        verdict: ReviewVerdict,
        body: SharedString,
        created_at: SharedString,
    },
    Comment {
        author: Actor,
        body: SharedString,
        created_at: SharedString,
    },
    Merged {
        actor: Option<Actor>,
        created_at: SharedString,
    },
    Closed {
        actor: Option<Actor>,
        created_at: SharedString,
    },
    Reopened {
        actor: Option<Actor>,
        created_at: SharedString,
    },
    HeadRefForcePushed {
        actor: Option<Actor>,
        created_at: SharedString,
    },
    Other {
        kind: SharedString,
        created_at: SharedString,
    },
}

/// Target for a newly-submitted inline comment.
#[derive(Clone, Debug)]
pub enum CommentTarget {
    /// Start a new review thread on a file line range.
    NewThread {
        path: SharedString,
        side: DiffSide,
        line: u32,
        start_line: Option<u32>,
    },
    /// Reply to an existing thread.
    Reply { in_reply_to: SharedString },
}

/// Inputs for filing a new comment, optionally within a pending review batch.
#[derive(Clone, Debug)]
pub struct NewComment {
    pub pull_request: PullRequestId,
    pub body: SharedString,
    pub target: CommentTarget,
    /// When set, the comment is attached to this pending review batch.
    pub review_id: Option<SharedString>,
    pub commit_sha: SharedString,
}

/// Inputs for creating a pull request.
#[derive(Clone, Debug)]
pub struct CreatePullRequest {
    pub owner: SharedString,
    pub repo: SharedString,
    pub base_ref: SharedString,
    pub head_ref: SharedString,
    pub title: SharedString,
    pub body: SharedString,
    pub draft: bool,
}

/// The capability surface a forge implementation provides to the review UI.
///
/// All methods are network operations returning a boxed `Send` future so the
/// trait stays object-safe behind `Arc<dyn PullRequestProvider>`.
pub trait PullRequestProvider: Send + Sync {
    /// Display name of the forge (e.g. "GitHub").
    fn name(&self) -> &'static str;

    /// List pull requests matching a forge search `query` string.
    fn list_pull_requests<'a>(
        &'a self,
        owner: &'a str,
        repo: &'a str,
        query: &'a str,
    ) -> ProviderFuture<'a, Vec<PullRequestInfo>>;

    /// Fetch full detail for a single pull request.
    fn fetch_pull_request<'a>(
        &'a self,
        owner: &'a str,
        repo: &'a str,
        number: u32,
    ) -> ProviderFuture<'a, PullRequest>;

    /// Fetch the changed files (with viewed state) for a pull request.
    fn fetch_files<'a>(
        &'a self,
        owner: &'a str,
        repo: &'a str,
        number: u32,
    ) -> ProviderFuture<'a, Vec<PullRequestFile>>;

    /// Fetch the review threads (inline comments) for a pull request.
    fn fetch_review_threads<'a>(
        &'a self,
        owner: &'a str,
        repo: &'a str,
        number: u32,
    ) -> ProviderFuture<'a, Vec<ReviewThread>>;

    /// Fetch the activity timeline for a pull request.
    fn fetch_timeline<'a>(
        &'a self,
        owner: &'a str,
        repo: &'a str,
        number: u32,
    ) -> ProviderFuture<'a, Vec<TimelineItem>>;

    /// Mark a file as viewed on the server.
    fn mark_file_viewed<'a>(
        &'a self,
        pull_request_node_id: &'a str,
        path: &'a str,
    ) -> ProviderFuture<'a, ()>;

    /// Clear a file's viewed state on the server.
    fn unmark_file_viewed<'a>(
        &'a self,
        pull_request_node_id: &'a str,
        path: &'a str,
    ) -> ProviderFuture<'a, ()>;

    /// Return the id of the viewer's pending (unsubmitted) review, if any.
    fn pending_review_id<'a>(
        &'a self,
        owner: &'a str,
        repo: &'a str,
        number: u32,
    ) -> ProviderFuture<'a, Option<SharedString>>;

    /// Start a new pending review batch, returning its id.
    fn start_review<'a>(
        &'a self,
        pull_request_node_id: &'a str,
    ) -> ProviderFuture<'a, SharedString>;

    /// Add an inline comment (new thread or reply). Returns the created thread
    /// when a new thread was started.
    fn add_comment<'a>(
        &'a self,
        comment: &'a NewComment,
    ) -> ProviderFuture<'a, Option<ReviewThread>>;

    /// Edit an existing comment body.
    fn edit_comment<'a>(
        &'a self,
        comment_node_id: &'a str,
        body: &'a str,
    ) -> ProviderFuture<'a, ReviewComment>;

    /// Delete a review comment (by REST database id — GitHub has no GraphQL
    /// mutation for this).
    fn delete_comment<'a>(
        &'a self,
        owner: &'a str,
        repo: &'a str,
        comment_database_id: u64,
    ) -> ProviderFuture<'a, ()>;

    /// Submit a pending review (or post a single review when `review_id` is the
    /// freshly-started batch).
    fn submit_review<'a>(
        &'a self,
        review_node_id: &'a str,
        event: ReviewEvent,
        body: &'a str,
    ) -> ProviderFuture<'a, ()>;

    /// Discard a pending review.
    fn delete_review<'a>(&'a self, review_node_id: &'a str) -> ProviderFuture<'a, ()>;

    /// Resolve a review thread.
    fn resolve_thread<'a>(&'a self, thread_node_id: &'a str) -> ProviderFuture<'a, ReviewThread>;

    /// Unresolve a review thread.
    fn unresolve_thread<'a>(&'a self, thread_node_id: &'a str) -> ProviderFuture<'a, ReviewThread>;

    /// Add an emoji reaction to a subject (comment/PR body).
    fn add_reaction<'a>(
        &'a self,
        subject_node_id: &'a str,
        content: &'a str,
    ) -> ProviderFuture<'a, ()>;

    /// Remove an emoji reaction from a subject.
    fn remove_reaction<'a>(
        &'a self,
        subject_node_id: &'a str,
        content: &'a str,
    ) -> ProviderFuture<'a, ()>;

    /// Close a pull request.
    fn close_pull_request<'a>(&'a self, pull_request_node_id: &'a str) -> ProviderFuture<'a, ()>;

    /// Reopen a closed pull request.
    fn reopen_pull_request<'a>(&'a self, pull_request_node_id: &'a str) -> ProviderFuture<'a, ()>;

    /// Mark a draft pull request ready for review.
    fn mark_ready_for_review<'a>(
        &'a self,
        pull_request_node_id: &'a str,
    ) -> ProviderFuture<'a, ()>;

    /// Convert a pull request back to draft.
    fn convert_to_draft<'a>(&'a self, pull_request_node_id: &'a str) -> ProviderFuture<'a, ()>;

    /// Update a pull request's title and/or body.
    fn update_pull_request<'a>(
        &'a self,
        pull_request_node_id: &'a str,
        title: Option<&'a str>,
        body: Option<&'a str>,
    ) -> ProviderFuture<'a, ()>;

    /// Merge a pull request.
    fn merge_pull_request<'a>(
        &'a self,
        pull_request_node_id: &'a str,
        method: MergeMethod,
        commit_headline: Option<&'a str>,
    ) -> ProviderFuture<'a, ()>;

    /// Create a new pull request.
    fn create_pull_request<'a>(
        &'a self,
        input: &'a CreatePullRequest,
    ) -> ProviderFuture<'a, PullRequestInfo>;

    /// Fetch a file's text content at a revision (`<oid>:<path>` expression).
    /// `None` means the path didn't exist at that revision or is binary.
    fn fetch_file_content<'a>(
        &'a self,
        owner: &'a str,
        repo: &'a str,
        expression: &'a str,
    ) -> ProviderFuture<'a, Option<SharedString>>;
}
