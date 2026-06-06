//! GitHub implementation of [`PullRequestProvider`] over the GraphQL v4 API.
//!
//! Wire (`Gql*`) types deserialize the query selections in `github_queries`,
//! then map into the forge-agnostic domain types in `provider`. The mapping
//! layer is where all GitHub-specific string enums (`"OPEN"`, `"RIGHT"`,
//! `"VIEWED"`, …) are normalized.
#![allow(dead_code)]

use crate::github_graphql::execute;
use crate::github_queries as q;
use crate::provider::*;
use gpui::SharedString;
use http_client::HttpClient;
use serde::Deserialize;
use std::sync::Arc;

pub struct GitHubProvider {
    http_client: Arc<dyn HttpClient>,
    token: Option<String>,
}

impl GitHubProvider {
    pub fn new(http_client: Arc<dyn HttpClient>, token: Option<String>) -> Self {
        Self { http_client, token }
    }

    fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }
}

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlActor {
    login: Option<String>,
    name: Option<String>,
    avatar_url: Option<String>,
}

impl GqlActor {
    fn into_actor(self) -> Actor {
        Actor {
            login: self
                .login
                .or(self.name)
                .unwrap_or_else(|| "ghost".into())
                .into(),
            avatar_url: self.avatar_url.map(Into::into),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlTotalCount {
    total_count: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlReactionGroup {
    content: String,
    viewer_has_reacted: bool,
    reactors: GqlTotalCount,
}

impl GqlReactionGroup {
    fn into_group(self) -> ReactionGroup {
        ReactionGroup {
            content: self.content.into(),
            viewer_has_reacted: self.viewer_has_reacted,
            count: self.reactors.total_count,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlPrInfo {
    id: String,
    number: u32,
    title: String,
    state: String,
    is_draft: bool,
    created_at: String,
    updated_at: String,
    additions: u32,
    deletions: u32,
    url: String,
    author: Option<GqlActor>,
    base_ref_name: String,
    head_ref_name: String,
    base_ref_oid: String,
    head_ref_oid: String,
    comments: GqlTotalCount,
}

impl GqlPrInfo {
    fn into_info(self, owner: &str, repo: &str) -> PullRequestInfo {
        PullRequestInfo {
            id: PullRequestId {
                node_id: self.id.into(),
                number: self.number,
                owner: owner.to_string().into(),
                repo: repo.to_string().into(),
            },
            title: self.title.into(),
            author: self.author.map(GqlActor::into_actor).unwrap_or(Actor {
                login: "ghost".into(),
                avatar_url: None,
            }),
            state: map_pr_state(&self.state),
            is_draft: self.is_draft,
            base_ref: self.base_ref_name.into(),
            head_ref: self.head_ref_name.into(),
            base_sha: self.base_ref_oid.into(),
            head_sha: self.head_ref_oid.into(),
            created_at: self.created_at.into(),
            updated_at: self.updated_at.into(),
            additions: self.additions,
            deletions: self.deletions,
            comment_count: self.comments.total_count,
            url: self.url.into(),
        }
    }
}

#[derive(Deserialize)]
struct ListData {
    search: GqlSearch,
}

#[derive(Deserialize)]
struct GqlSearch {
    nodes: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
struct RepoData<T> {
    repository: RepoPullRequest<T>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepoPullRequest<T> {
    pull_request: T,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlPrDetail {
    #[serde(flatten)]
    info: GqlPrInfo,
    body: Option<String>,
    mergeable: Option<String>,
    merge_state_status: Option<String>,
    viewer_can_update: bool,
    viewer_can_merge_as_admin: bool,
    milestone: Option<GqlMilestone>,
    labels: GqlNodes<GqlLabel>,
    assignees: GqlNodes<GqlActor>,
    reaction_groups: Vec<GqlReactionGroup>,
    latest_reviews: GqlNodes<GqlLatestReview>,
    review_requests: GqlNodes<GqlReviewRequest>,
    commits: GqlNodes<GqlCommitNode>,
}

#[derive(Deserialize)]
struct GqlNodes<T> {
    nodes: Vec<T>,
}

#[derive(Deserialize)]
struct GqlMilestone {
    title: String,
}

#[derive(Deserialize)]
struct GqlLabel {
    name: String,
}

#[derive(Deserialize)]
struct GqlLatestReview {
    state: String,
    author: Option<GqlActor>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlReviewRequest {
    requested_reviewer: Option<GqlActor>,
}

#[derive(Deserialize)]
struct GqlCommitNode {
    commit: GqlCommit,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlCommit {
    status_check_rollup: Option<GqlStatusRollup>,
}

#[derive(Deserialize)]
struct GqlStatusRollup {
    state: String,
    contexts: GqlNodes<GqlCheckContext>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlCheckContext {
    // CheckRun
    name: Option<String>,
    conclusion: Option<String>,
    status: Option<String>,
    details_url: Option<String>,
    // StatusContext
    context: Option<String>,
    state: Option<String>,
    target_url: Option<String>,
    description: Option<String>,
}

impl GqlCheckContext {
    fn into_check(self) -> CheckRun {
        if let Some(name) = self.name {
            // CheckRun: prefer conclusion, fall back to in-flight status.
            let status = self
                .conclusion
                .as_deref()
                .map(map_check_conclusion)
                .unwrap_or_else(|| map_check_status(self.status.as_deref()));
            CheckRun {
                name: name.into(),
                status,
                url: self.details_url.map(Into::into),
                description: None,
            }
        } else {
            CheckRun {
                name: self.context.unwrap_or_default().into(),
                status: map_status_state(self.state.as_deref()),
                url: self.target_url.map(Into::into),
                description: self.description.map(Into::into),
            }
        }
    }
}

#[derive(Deserialize)]
struct FilesData {
    repository: RepoPullRequest<GqlFilesConnection>,
}

#[derive(Deserialize)]
struct GqlFilesConnection {
    files: GqlPageConnection<GqlFile>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlPageConnection<T> {
    nodes: Vec<T>,
    page_info: GqlPageInfo,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlPageInfo {
    has_next_page: bool,
    end_cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlFile {
    path: String,
    additions: u32,
    deletions: u32,
    change_type: String,
    viewer_viewed_state: String,
}

#[derive(Deserialize)]
struct ThreadsData {
    repository: RepoPullRequest<GqlThreadsConnection>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlThreadsConnection {
    review_threads: GqlPageConnection<GqlThread>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlThread {
    id: String,
    path: String,
    diff_side: String,
    line: Option<u32>,
    start_line: Option<u32>,
    original_line: Option<u32>,
    original_start_line: Option<u32>,
    is_resolved: bool,
    is_outdated: bool,
    viewer_can_resolve: bool,
    viewer_can_unresolve: bool,
    comments: GqlNodes<GqlComment>,
}

impl GqlThread {
    fn into_thread(self) -> ReviewThread {
        ReviewThread {
            id: self.id.into(),
            path: self.path.into(),
            diff_side: map_diff_side(&self.diff_side),
            line: self.line,
            start_line: self.start_line,
            original_line: self.original_line,
            original_start_line: self.original_start_line,
            is_resolved: self.is_resolved,
            is_outdated: self.is_outdated,
            viewer_can_resolve: self.viewer_can_resolve,
            viewer_can_unresolve: self.viewer_can_unresolve,
            comments: self.comments.nodes.into_iter().map(GqlComment::into_comment).collect(),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlComment {
    id: String,
    database_id: Option<u64>,
    author: Option<GqlActor>,
    body: String,
    diff_hunk: Option<String>,
    created_at: String,
    viewer_can_update: bool,
    viewer_can_delete: bool,
    pull_request_review: Option<GqlReviewRef>,
    reaction_groups: Vec<GqlReactionGroup>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlReviewRef {
    database_id: Option<u64>,
}

impl GqlComment {
    fn into_comment(self) -> ReviewComment {
        ReviewComment {
            id: self.id.into(),
            database_id: self.database_id,
            author: self.author.map(GqlActor::into_actor).unwrap_or(Actor {
                login: "ghost".into(),
                avatar_url: None,
            }),
            body: self.body.into(),
            diff_hunk: self.diff_hunk.map(Into::into),
            created_at: self.created_at.into(),
            reactions: self
                .reaction_groups
                .into_iter()
                .map(GqlReactionGroup::into_group)
                .collect(),
            viewer_can_update: self.viewer_can_update,
            viewer_can_delete: self.viewer_can_delete,
            review_database_id: self.pull_request_review.and_then(|r| r.database_id),
        }
    }
}

#[derive(Deserialize)]
struct TimelineData {
    repository: RepoPullRequest<GqlTimelineConnection>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlTimelineConnection {
    timeline_items: GqlNodes<GqlTimelineItem>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlTimelineItem {
    #[serde(rename = "__typename")]
    typename: String,
    commit: Option<GqlTimelineCommit>,
    author: Option<GqlActor>,
    actor: Option<GqlActor>,
    state: Option<String>,
    body: Option<String>,
    created_at: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlTimelineCommit {
    oid: String,
    message_headline: String,
    author: Option<GqlCommitAuthor>,
}

#[derive(Deserialize)]
struct GqlCommitAuthor {
    user: Option<GqlActor>,
}

impl GqlTimelineItem {
    fn into_item(self) -> TimelineItem {
        let created_at: SharedString = self.created_at.clone().unwrap_or_default().into();
        match self.typename.as_str() {
            "PullRequestCommit" => {
                let commit = self.commit;
                TimelineItem::Commit {
                    oid: commit.as_ref().map(|c| c.oid.clone()).unwrap_or_default().into(),
                    message: commit
                        .as_ref()
                        .map(|c| c.message_headline.clone())
                        .unwrap_or_default()
                        .into(),
                    author: commit
                        .and_then(|c| c.author)
                        .and_then(|a| a.user)
                        .map(GqlActor::into_actor),
                }
            }
            "PullRequestReview" => TimelineItem::Review {
                author: self.author.map(GqlActor::into_actor).unwrap_or(Actor {
                    login: "ghost".into(),
                    avatar_url: None,
                }),
                verdict: map_review_verdict(self.state.as_deref().unwrap_or("COMMENTED")),
                body: self.body.unwrap_or_default().into(),
                created_at,
            },
            "IssueComment" => TimelineItem::Comment {
                author: self.author.map(GqlActor::into_actor).unwrap_or(Actor {
                    login: "ghost".into(),
                    avatar_url: None,
                }),
                body: self.body.unwrap_or_default().into(),
                created_at,
            },
            "MergedEvent" => TimelineItem::Merged {
                actor: self.actor.map(GqlActor::into_actor),
                created_at,
            },
            "ClosedEvent" => TimelineItem::Closed {
                actor: self.actor.map(GqlActor::into_actor),
                created_at,
            },
            "ReopenedEvent" => TimelineItem::Reopened {
                actor: self.actor.map(GqlActor::into_actor),
                created_at,
            },
            "HeadRefForcePushedEvent" => TimelineItem::HeadRefForcePushed {
                actor: self.actor.map(GqlActor::into_actor),
                created_at,
            },
            other => TimelineItem::Other {
                kind: other.to_string().into(),
                created_at,
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Enum mapping
// ---------------------------------------------------------------------------

fn map_pr_state(state: &str) -> PullRequestState {
    match state {
        "MERGED" => PullRequestState::Merged,
        "CLOSED" => PullRequestState::Closed,
        _ => PullRequestState::Open,
    }
}

fn map_diff_side(side: &str) -> DiffSide {
    match side {
        "LEFT" => DiffSide::Left,
        _ => DiffSide::Right,
    }
}

fn map_change_type(change_type: &str) -> FileChangeStatus {
    match change_type {
        "ADDED" => FileChangeStatus::Added,
        "DELETED" => FileChangeStatus::Deleted,
        "RENAMED" => FileChangeStatus::Renamed { from: "".into() },
        "COPIED" => FileChangeStatus::Copied { from: "".into() },
        _ => FileChangeStatus::Modified,
    }
}

fn map_viewed_state(state: &str) -> ViewedState {
    match state {
        "VIEWED" => ViewedState::Viewed,
        "DISMISSED" => ViewedState::Dismissed,
        _ => ViewedState::Unviewed,
    }
}

fn map_review_verdict(state: &str) -> ReviewVerdict {
    match state {
        "APPROVED" => ReviewVerdict::Approved,
        "CHANGES_REQUESTED" => ReviewVerdict::ChangesRequested,
        "DISMISSED" => ReviewVerdict::Dismissed,
        "PENDING" => ReviewVerdict::Pending,
        _ => ReviewVerdict::Commented,
    }
}

fn map_status_state(state: Option<&str>) -> CheckStatus {
    match state {
        Some("SUCCESS") => CheckStatus::Success,
        Some("FAILURE") => CheckStatus::Failure,
        Some("ERROR") => CheckStatus::Error,
        Some("EXPECTED") | Some("PENDING") => CheckStatus::Pending,
        _ => CheckStatus::Pending,
    }
}

fn map_check_conclusion(conclusion: &str) -> CheckStatus {
    match conclusion {
        "SUCCESS" => CheckStatus::Success,
        "FAILURE" | "STARTUP_FAILURE" | "TIMED_OUT" => CheckStatus::Failure,
        "ACTION_REQUIRED" | "STALE" => CheckStatus::Error,
        "CANCELLED" => CheckStatus::Cancelled,
        "NEUTRAL" => CheckStatus::Neutral,
        "SKIPPED" => CheckStatus::Skipped,
        _ => CheckStatus::Pending,
    }
}

fn map_check_status(status: Option<&str>) -> CheckStatus {
    match status {
        Some("COMPLETED") => CheckStatus::Success,
        _ => CheckStatus::Pending,
    }
}

fn reviewers_from(
    latest: Vec<GqlLatestReview>,
    requests: Vec<GqlReviewRequest>,
) -> Vec<Reviewer> {
    let mut reviewers: Vec<Reviewer> = latest
        .into_iter()
        .filter_map(|review| {
            review.author.map(|author| Reviewer {
                actor: author.into_actor(),
                verdict: Some(map_review_verdict(&review.state)),
            })
        })
        .collect();

    for request in requests {
        if let Some(reviewer) = request.requested_reviewer {
            let actor = reviewer.into_actor();
            if !reviewers.iter().any(|r| r.actor.login == actor.login) {
                reviewers.push(Reviewer {
                    actor,
                    verdict: None,
                });
            }
        }
    }

    reviewers
}

fn map_detail(detail: GqlPrDetail, owner: &str, repo: &str) -> PullRequest {
    let (rollup_state, checks) = detail
        .commits
        .nodes
        .into_iter()
        .next()
        .and_then(|node| node.commit.status_check_rollup)
        .map(|rollup| {
            let checks = rollup
                .contexts
                .nodes
                .into_iter()
                .map(GqlCheckContext::into_check)
                .collect::<Vec<_>>();
            (Some(map_status_state(Some(&rollup.state))), checks)
        })
        .unwrap_or((None, Vec::new()));

    let body = detail.body.clone().unwrap_or_default();
    let mergeable = detail.mergeable.as_deref().map(|m| m == "MERGEABLE");
    let reviewers = reviewers_from(detail.latest_reviews.nodes, detail.review_requests.nodes);

    PullRequest {
        info: detail.info.into_info(owner, repo),
        body: body.into(),
        labels: detail
            .labels
            .nodes
            .into_iter()
            .map(|l| l.name.into())
            .collect(),
        assignees: detail
            .assignees
            .nodes
            .into_iter()
            .map(GqlActor::into_actor)
            .collect(),
        reviewers,
        milestone: detail.milestone.map(|m| m.title.into()),
        mergeable,
        merge_state_status: detail.merge_state_status.map(Into::into),
        checks,
        check_rollup: rollup_state,
        reactions: detail
            .reaction_groups
            .into_iter()
            .map(GqlReactionGroup::into_group)
            .collect(),
        viewer_can_update: detail.viewer_can_update,
        viewer_can_merge: detail.viewer_can_merge_as_admin,
    }
}

// ---------------------------------------------------------------------------
// Trait implementation
// ---------------------------------------------------------------------------

impl PullRequestProvider for GitHubProvider {
    fn name(&self) -> &'static str {
        "GitHub"
    }

    fn list_pull_requests<'a>(
        &'a self,
        owner: &'a str,
        repo: &'a str,
        query: &'a str,
    ) -> ProviderFuture<'a, Vec<PullRequestInfo>> {
        Box::pin(async move {
            let full_query = format!("repo:{owner}/{repo} is:pr {query}");
            let data: ListData = execute(
                &self.http_client,
                self.token(),
                &q::list_pull_requests(),
                serde_json::json!({ "query": full_query }),
            )
            .await?;
            Ok(data
                .search
                .nodes
                .into_iter()
                .filter_map(|node| serde_json::from_value::<GqlPrInfo>(node).ok())
                .map(|info| info.into_info(owner, repo))
                .collect())
        })
    }

    fn fetch_pull_request<'a>(
        &'a self,
        owner: &'a str,
        repo: &'a str,
        number: u32,
    ) -> ProviderFuture<'a, PullRequest> {
        Box::pin(async move {
            let data: RepoData<GqlPrDetail> = execute(
                &self.http_client,
                self.token(),
                &q::pull_request(),
                serde_json::json!({ "owner": owner, "name": repo, "number": number }),
            )
            .await?;
            Ok(map_detail(data.repository.pull_request, owner, repo))
        })
    }

    fn fetch_files<'a>(
        &'a self,
        owner: &'a str,
        repo: &'a str,
        number: u32,
    ) -> ProviderFuture<'a, Vec<PullRequestFile>> {
        Box::pin(async move {
            let mut files = Vec::new();
            let mut after: Option<String> = None;
            loop {
                let data: FilesData = execute(
                    &self.http_client,
                    self.token(),
                    q::pull_request_files(),
                    serde_json::json!({
                        "owner": owner, "name": repo, "number": number, "after": after
                    }),
                )
                .await?;
                let connection = data.repository.pull_request.files;
                for file in connection.nodes {
                    files.push(PullRequestFile {
                        path: file.path.into(),
                        status: map_change_type(&file.change_type),
                        additions: file.additions,
                        deletions: file.deletions,
                        viewed_state: map_viewed_state(&file.viewer_viewed_state),
                    });
                }
                if connection.page_info.has_next_page {
                    after = connection.page_info.end_cursor;
                } else {
                    break;
                }
            }
            Ok(files)
        })
    }

    fn fetch_review_threads<'a>(
        &'a self,
        owner: &'a str,
        repo: &'a str,
        number: u32,
    ) -> ProviderFuture<'a, Vec<ReviewThread>> {
        Box::pin(async move {
            let mut threads = Vec::new();
            let mut after: Option<String> = None;
            let query = q::pull_request_threads();
            loop {
                let data: ThreadsData = execute(
                    &self.http_client,
                    self.token(),
                    &query,
                    serde_json::json!({
                        "owner": owner, "name": repo, "number": number, "after": after
                    }),
                )
                .await?;
                let connection = data.repository.pull_request.review_threads;
                for thread in connection.nodes {
                    threads.push(thread.into_thread());
                }
                if connection.page_info.has_next_page {
                    after = connection.page_info.end_cursor;
                } else {
                    break;
                }
            }
            Ok(threads)
        })
    }

    fn fetch_timeline<'a>(
        &'a self,
        owner: &'a str,
        repo: &'a str,
        number: u32,
    ) -> ProviderFuture<'a, Vec<TimelineItem>> {
        Box::pin(async move {
            let data: TimelineData = execute(
                &self.http_client,
                self.token(),
                q::pull_request_timeline(),
                serde_json::json!({ "owner": owner, "name": repo, "number": number }),
            )
            .await?;
            Ok(data
                .repository
                .pull_request
                .timeline_items
                .nodes
                .into_iter()
                .map(GqlTimelineItem::into_item)
                .collect())
        })
    }

    fn mark_file_viewed<'a>(
        &'a self,
        pull_request_node_id: &'a str,
        path: &'a str,
    ) -> ProviderFuture<'a, ()> {
        Box::pin(async move {
            let _: serde_json::Value = execute(
                &self.http_client,
                self.token(),
                q::mark_file_as_viewed(),
                serde_json::json!({ "pullRequestId": pull_request_node_id, "path": path }),
            )
            .await?;
            Ok(())
        })
    }

    fn unmark_file_viewed<'a>(
        &'a self,
        pull_request_node_id: &'a str,
        path: &'a str,
    ) -> ProviderFuture<'a, ()> {
        Box::pin(async move {
            let _: serde_json::Value = execute(
                &self.http_client,
                self.token(),
                q::unmark_file_as_viewed(),
                serde_json::json!({ "pullRequestId": pull_request_node_id, "path": path }),
            )
            .await?;
            Ok(())
        })
    }

    fn pending_review_id<'a>(
        &'a self,
        owner: &'a str,
        repo: &'a str,
        number: u32,
    ) -> ProviderFuture<'a, Option<SharedString>> {
        Box::pin(async move {
            #[derive(Deserialize)]
            struct Data {
                repository: RepoPullRequest<Reviews>,
            }
            #[derive(Deserialize)]
            struct Reviews {
                reviews: GqlNodes<IdNode>,
            }
            #[derive(Deserialize)]
            struct IdNode {
                id: String,
            }
            let data: Data = execute(
                &self.http_client,
                self.token(),
                q::pending_review_id(),
                serde_json::json!({ "owner": owner, "name": repo, "number": number }),
            )
            .await?;
            Ok(data
                .repository
                .pull_request
                .reviews
                .nodes
                .into_iter()
                .next()
                .map(|node| node.id.into()))
        })
    }

    fn start_review<'a>(
        &'a self,
        pull_request_node_id: &'a str,
    ) -> ProviderFuture<'a, SharedString> {
        Box::pin(async move {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Data {
                add_pull_request_review: ReviewWrap,
            }
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct ReviewWrap {
                pull_request_review: IdNode,
            }
            #[derive(Deserialize)]
            struct IdNode {
                id: String,
            }
            let data: Data = execute(
                &self.http_client,
                self.token(),
                q::start_review(),
                serde_json::json!({ "pullRequestId": pull_request_node_id }),
            )
            .await?;
            Ok(data.add_pull_request_review.pull_request_review.id.into())
        })
    }

    fn add_comment<'a>(
        &'a self,
        comment: &'a NewComment,
    ) -> ProviderFuture<'a, Option<ReviewThread>> {
        Box::pin(async move {
            match &comment.target {
                CommentTarget::NewThread {
                    path,
                    side,
                    line,
                    start_line,
                } => {
                    #[derive(Deserialize)]
                    #[serde(rename_all = "camelCase")]
                    struct Data {
                        add_pull_request_review_thread: ThreadWrap,
                    }
                    #[derive(Deserialize)]
                    struct ThreadWrap {
                        thread: GqlThread,
                    }
                    let side = match side {
                        DiffSide::Left => "LEFT",
                        DiffSide::Right => "RIGHT",
                    };
                    let data: Data = execute(
                        &self.http_client,
                        self.token(),
                        &q::add_review_thread(),
                        serde_json::json!({
                            "pullRequestId": comment.pull_request.node_id,
                            "body": comment.body,
                            "path": path,
                            "line": line,
                            "startLine": start_line,
                            "side": side,
                            "startSide": start_line.map(|_| side),
                            "reviewId": comment.review_id,
                        }),
                    )
                    .await?;
                    Ok(Some(data.add_pull_request_review_thread.thread.into_thread()))
                }
                CommentTarget::Reply { in_reply_to } => {
                    let review_id = match &comment.review_id {
                        Some(id) => id.clone(),
                        None => self.start_review(&comment.pull_request.node_id).await?,
                    };
                    let _: serde_json::Value = execute(
                        &self.http_client,
                        self.token(),
                        &q::add_review_comment(),
                        serde_json::json!({
                            "reviewId": review_id,
                            "body": comment.body,
                            "inReplyTo": in_reply_to,
                        }),
                    )
                    .await?;
                    Ok(None)
                }
            }
        })
    }

    fn edit_comment<'a>(
        &'a self,
        comment_node_id: &'a str,
        body: &'a str,
    ) -> ProviderFuture<'a, ReviewComment> {
        Box::pin(async move {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Data {
                update_pull_request_review_comment: CommentWrap,
            }
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct CommentWrap {
                pull_request_review_comment: GqlComment,
            }
            let data: Data = execute(
                &self.http_client,
                self.token(),
                &q::edit_comment(),
                serde_json::json!({ "id": comment_node_id, "body": body }),
            )
            .await?;
            Ok(data
                .update_pull_request_review_comment
                .pull_request_review_comment
                .into_comment())
        })
    }

    fn submit_review<'a>(
        &'a self,
        review_node_id: &'a str,
        event: ReviewEvent,
        body: &'a str,
    ) -> ProviderFuture<'a, ()> {
        Box::pin(async move {
            let body = if body.is_empty() {
                serde_json::Value::Null
            } else {
                serde_json::Value::String(body.to_string())
            };
            let _: serde_json::Value = execute(
                &self.http_client,
                self.token(),
                q::submit_review(),
                serde_json::json!({
                    "reviewId": review_node_id,
                    "event": event.graphql(),
                    "body": body,
                }),
            )
            .await?;
            Ok(())
        })
    }

    fn delete_review<'a>(&'a self, review_node_id: &'a str) -> ProviderFuture<'a, ()> {
        Box::pin(async move {
            let _: serde_json::Value = execute(
                &self.http_client,
                self.token(),
                q::delete_review(),
                serde_json::json!({ "reviewId": review_node_id }),
            )
            .await?;
            Ok(())
        })
    }

    fn resolve_thread<'a>(&'a self, thread_node_id: &'a str) -> ProviderFuture<'a, ReviewThread> {
        Box::pin(async move {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Data {
                resolve_review_thread: ThreadWrap,
            }
            #[derive(Deserialize)]
            struct ThreadWrap {
                thread: GqlThread,
            }
            let data: Data = execute(
                &self.http_client,
                self.token(),
                &q::resolve_thread(),
                serde_json::json!({ "threadId": thread_node_id }),
            )
            .await?;
            Ok(data.resolve_review_thread.thread.into_thread())
        })
    }

    fn unresolve_thread<'a>(&'a self, thread_node_id: &'a str) -> ProviderFuture<'a, ReviewThread> {
        Box::pin(async move {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Data {
                unresolve_review_thread: ThreadWrap,
            }
            #[derive(Deserialize)]
            struct ThreadWrap {
                thread: GqlThread,
            }
            let data: Data = execute(
                &self.http_client,
                self.token(),
                &q::unresolve_thread(),
                serde_json::json!({ "threadId": thread_node_id }),
            )
            .await?;
            Ok(data.unresolve_review_thread.thread.into_thread())
        })
    }

    fn add_reaction<'a>(
        &'a self,
        subject_node_id: &'a str,
        content: &'a str,
    ) -> ProviderFuture<'a, ()> {
        Box::pin(async move {
            let _: serde_json::Value = execute(
                &self.http_client,
                self.token(),
                q::add_reaction(),
                serde_json::json!({ "subjectId": subject_node_id, "content": content }),
            )
            .await?;
            Ok(())
        })
    }

    fn remove_reaction<'a>(
        &'a self,
        subject_node_id: &'a str,
        content: &'a str,
    ) -> ProviderFuture<'a, ()> {
        Box::pin(async move {
            let _: serde_json::Value = execute(
                &self.http_client,
                self.token(),
                q::remove_reaction(),
                serde_json::json!({ "subjectId": subject_node_id, "content": content }),
            )
            .await?;
            Ok(())
        })
    }

    fn merge_pull_request<'a>(
        &'a self,
        pull_request_node_id: &'a str,
        method: MergeMethod,
        commit_headline: Option<&'a str>,
    ) -> ProviderFuture<'a, ()> {
        Box::pin(async move {
            let _: serde_json::Value = execute(
                &self.http_client,
                self.token(),
                q::merge_pull_request(),
                serde_json::json!({
                    "pullRequestId": pull_request_node_id,
                    "method": method.graphql(),
                    "headline": commit_headline,
                }),
            )
            .await?;
            Ok(())
        })
    }

    fn create_pull_request<'a>(
        &'a self,
        input: &'a CreatePullRequest,
    ) -> ProviderFuture<'a, PullRequestInfo> {
        Box::pin(async move {
            #[derive(Deserialize)]
            struct RepoIdData {
                repository: IdNode,
            }
            #[derive(Deserialize)]
            struct IdNode {
                id: String,
            }
            let repo_id: RepoIdData = execute(
                &self.http_client,
                self.token(),
                q::repository_id(),
                serde_json::json!({ "owner": input.owner, "name": input.repo }),
            )
            .await?;

            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Data {
                create_pull_request: PrWrap,
            }
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct PrWrap {
                pull_request: GqlPrInfo,
            }
            let data: Data = execute(
                &self.http_client,
                self.token(),
                &q::create_pull_request(),
                serde_json::json!({
                    "repositoryId": repo_id.repository.id,
                    "base": input.base_ref,
                    "head": input.head_ref,
                    "title": input.title,
                    "body": input.body,
                    "draft": input.draft,
                }),
            )
            .await?;
            Ok(data
                .create_pull_request
                .pull_request
                .into_info(&input.owner, &input.repo))
        })
    }
}
