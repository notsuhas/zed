//! GitHub GraphQL query and mutation strings.
//!
//! Field selections are kept in lock-step with the serde response types in
//! `github_provider.rs`. Operation shapes are adapted from the
//! `microsoft/vscode-pull-request-github` extension's `*.gql` files (MIT
//! licensed; compatible with this crate's GPL-3.0). PR listing is search-based
//! (`type: ISSUE`) because GitHub models PRs as issues in search.
#![allow(dead_code)]

/// Reusable selection for an inline review comment.
const REVIEW_COMMENT_FIELDS: &str = r#"
    id
    databaseId
    author { login avatarUrl }
    body
    diffHunk
    createdAt
    viewerCanUpdate
    viewerCanDelete
    pullRequestReview { databaseId }
    reactionGroups { content viewerHasReacted reactors { totalCount } }
"#;

/// Reusable selection for a review thread.
fn review_thread_fields() -> String {
    format!(
        r#"
        id
        path
        diffSide
        line
        startLine
        originalLine
        originalStartLine
        isResolved
        isOutdated
        viewerCanResolve
        viewerCanUnresolve
        comments(first: 100) {{ nodes {{ {comment} }} }}
    "#,
        comment = REVIEW_COMMENT_FIELDS
    )
}

/// Reusable selection for the summary fields shared by list + detail.
const PR_INFO_FIELDS: &str = r#"
    id
    number
    title
    state
    isDraft
    createdAt
    updatedAt
    additions
    deletions
    url
    author { login avatarUrl }
    baseRefName
    headRefName
    baseRefOid
    headRefOid
    comments { totalCount }
"#;

/// Search-based PR listing. `$query` is GitHub search syntax.
pub fn list_pull_requests() -> String {
    format!(
        r#"
        query ListPullRequests($query: String!) {{
          search(first: 100, type: ISSUE, query: $query) {{
            nodes {{
              ... on PullRequest {{ {info} }}
            }}
          }}
        }}
    "#,
        info = PR_INFO_FIELDS
    )
}

/// Full detail for one pull request.
pub fn pull_request() -> String {
    format!(
        r#"
        query PullRequest($owner: String!, $name: String!, $number: Int!) {{
          repository(owner: $owner, name: $name) {{
            pullRequest(number: $number) {{
              {info}
              body
              mergeable
              mergeStateStatus
              viewerCanUpdate
              viewerCanMergeAsAdmin
              milestone {{ title }}
              labels(first: 50) {{ nodes {{ name }} }}
              assignees(first: 50) {{ nodes {{ login avatarUrl }} }}
              reactionGroups {{ content viewerHasReacted reactors {{ totalCount }} }}
              latestReviews(first: 50) {{ nodes {{ state author {{ login avatarUrl }} }} }}
              reviewRequests(first: 50) {{
                nodes {{ requestedReviewer {{ ... on User {{ login avatarUrl }} ... on Team {{ name: slug }} }} }}
              }}
              commits(last: 1) {{
                nodes {{
                  commit {{
                    statusCheckRollup {{
                      state
                      contexts(first: 100) {{
                        nodes {{
                          ... on CheckRun {{ name conclusion status detailsUrl }}
                          ... on StatusContext {{ context state targetUrl description }}
                        }}
                      }}
                    }}
                  }}
                }}
              }}
            }}
          }}
        }}
    "#,
        info = PR_INFO_FIELDS
    )
}

/// Changed files with per-file viewed state. Paginated by `$after`.
pub fn pull_request_files() -> &'static str {
    r#"
    query PullRequestFiles($owner: String!, $name: String!, $number: Int!, $after: String) {
      repository(owner: $owner, name: $name) {
        pullRequest(number: $number) {
          files(first: 100, after: $after) {
            nodes { path additions deletions changeType viewerViewedState }
            pageInfo { hasNextPage endCursor }
          }
        }
      }
    }
    "#
}

/// Review threads (inline comments). Paginated by `$after`.
pub fn pull_request_threads() -> String {
    format!(
        r#"
        query PullRequestThreads($owner: String!, $name: String!, $number: Int!, $after: String) {{
          repository(owner: $owner, name: $name) {{
            pullRequest(number: $number) {{
              reviewThreads(first: 50, after: $after) {{
                nodes {{ {thread} }}
                pageInfo {{ hasNextPage endCursor }}
              }}
            }}
          }}
        }}
    "#,
        thread = review_thread_fields()
    )
}

/// Activity timeline.
pub fn pull_request_timeline() -> &'static str {
    r#"
    query PullRequestTimeline($owner: String!, $name: String!, $number: Int!) {
      repository(owner: $owner, name: $name) {
        pullRequest(number: $number) {
          timelineItems(last: 150) {
            nodes {
              __typename
              ... on PullRequestCommit { commit { oid messageHeadline author { user { login avatarUrl } } } }
              ... on PullRequestReview { author { login avatarUrl } state body createdAt }
              ... on IssueComment { author { login avatarUrl } body createdAt }
              ... on MergedEvent { actor { login avatarUrl } createdAt }
              ... on ClosedEvent { actor { login avatarUrl } createdAt }
              ... on ReopenedEvent { actor { login avatarUrl } createdAt }
              ... on HeadRefForcePushedEvent { actor { login avatarUrl } createdAt }
            }
          }
        }
      }
    }
    "#
}

/// Id of the viewer's pending review on a PR, if any.
pub fn pending_review_id() -> &'static str {
    r#"
    query PendingReviewId($owner: String!, $name: String!, $number: Int!) {
      repository(owner: $owner, name: $name) {
        pullRequest(number: $number) {
          reviews(first: 1, states: [PENDING]) { nodes { id } }
        }
      }
    }
    "#
}

pub fn mark_file_as_viewed() -> &'static str {
    r#"
    mutation MarkFileAsViewed($pullRequestId: ID!, $path: String!) {
      markFileAsViewed(input: { pullRequestId: $pullRequestId, path: $path }) {
        pullRequest { id }
      }
    }
    "#
}

pub fn unmark_file_as_viewed() -> &'static str {
    r#"
    mutation UnmarkFileAsViewed($pullRequestId: ID!, $path: String!) {
      unmarkFileAsViewed(input: { pullRequestId: $pullRequestId, path: $path }) {
        pullRequest { id }
      }
    }
    "#
}

pub fn start_review() -> &'static str {
    r#"
    mutation StartReview($pullRequestId: ID!) {
      addPullRequestReview(input: { pullRequestId: $pullRequestId }) {
        pullRequestReview { id }
      }
    }
    "#
}

/// Add a new inline review thread.
pub fn add_review_thread() -> String {
    format!(
        r#"
        mutation AddReviewThread(
          $pullRequestId: ID!, $body: String!, $path: String!,
          $line: Int!, $startLine: Int, $side: DiffSide!, $startSide: DiffSide,
          $reviewId: ID
        ) {{
          addPullRequestReviewThread(input: {{
            pullRequestId: $pullRequestId, body: $body, path: $path,
            line: $line, startLine: $startLine, side: $side, startSide: $startSide,
            pullRequestReviewId: $reviewId
          }}) {{
            thread {{ {thread} }}
          }}
        }}
    "#,
        thread = review_thread_fields()
    )
}

/// Reply to an existing thread / add a comment to a pending review.
pub fn add_review_comment() -> String {
    format!(
        r#"
        mutation AddReviewComment($reviewId: ID!, $body: String!, $inReplyTo: ID) {{
          addPullRequestReviewComment(input: {{
            pullRequestReviewId: $reviewId, body: $body, inReplyTo: $inReplyTo
          }}) {{
            comment {{ {comment} }}
          }}
        }}
    "#,
        comment = REVIEW_COMMENT_FIELDS
    )
}

pub fn edit_comment() -> String {
    format!(
        r#"
        mutation EditComment($id: ID!, $body: String!) {{
          updatePullRequestReviewComment(input: {{ pullRequestReviewCommentId: $id, body: $body }}) {{
            pullRequestReviewComment {{ {comment} }}
          }}
        }}
    "#,
        comment = REVIEW_COMMENT_FIELDS
    )
}

pub fn submit_review() -> &'static str {
    r#"
    mutation SubmitReview($reviewId: ID!, $event: PullRequestReviewEvent!, $body: String) {
      submitPullRequestReview(input: { pullRequestReviewId: $reviewId, event: $event, body: $body }) {
        pullRequestReview { id state }
      }
    }
    "#
}

pub fn delete_review() -> &'static str {
    r#"
    mutation DeleteReview($reviewId: ID!) {
      deletePullRequestReview(input: { pullRequestReviewId: $reviewId }) {
        pullRequestReview { id }
      }
    }
    "#
}

pub fn resolve_thread() -> String {
    format!(
        r#"
        mutation ResolveThread($threadId: ID!) {{
          resolveReviewThread(input: {{ threadId: $threadId }}) {{ thread {{ {thread} }} }}
        }}
    "#,
        thread = review_thread_fields()
    )
}

pub fn unresolve_thread() -> String {
    format!(
        r#"
        mutation UnresolveThread($threadId: ID!) {{
          unresolveReviewThread(input: {{ threadId: $threadId }}) {{ thread {{ {thread} }} }}
        }}
    "#,
        thread = review_thread_fields()
    )
}

pub fn add_reaction() -> &'static str {
    r#"
    mutation AddReaction($subjectId: ID!, $content: ReactionContent!) {
      addReaction(input: { subjectId: $subjectId, content: $content }) { reaction { content } }
    }
    "#
}

pub fn remove_reaction() -> &'static str {
    r#"
    mutation RemoveReaction($subjectId: ID!, $content: ReactionContent!) {
      removeReaction(input: { subjectId: $subjectId, content: $content }) { reaction { content } }
    }
    "#
}

pub fn merge_pull_request() -> &'static str {
    r#"
    mutation MergePullRequest($pullRequestId: ID!, $method: PullRequestMergeMethod!, $headline: String) {
      mergePullRequest(input: { pullRequestId: $pullRequestId, mergeMethod: $method, commitHeadline: $headline }) {
        pullRequest { id state }
      }
    }
    "#
}

/// Create a pull request.
pub fn create_pull_request() -> String {
    format!(
        r#"
        mutation CreatePullRequest(
          $repositoryId: ID!, $base: String!, $head: String!,
          $title: String!, $body: String, $draft: Boolean
        ) {{
          createPullRequest(input: {{
            repositoryId: $repositoryId, baseRefName: $base, headRefName: $head,
            title: $title, body: $body, draft: $draft
          }}) {{
            pullRequest {{ {info} }}
          }}
        }}
    "#,
        info = PR_INFO_FIELDS
    )
}

/// Fetch a file's text content at a commit. `$expression` is `<oid>:<path>`.
/// `object` is null when the path doesn't exist at that revision (e.g. an
/// added file has no base content).
pub fn file_content() -> &'static str {
    r#"
    query FileContent($owner: String!, $name: String!, $expression: String!) {
      repository(owner: $owner, name: $name) {
        object(expression: $expression) {
          ... on Blob { text isBinary }
        }
      }
    }
    "#
}

/// Resolve a repository's node id (needed for createPullRequest).
pub fn repository_id() -> &'static str {
    r#"
    query RepositoryId($owner: String!, $name: String!) {
      repository(owner: $owner, name: $name) { id }
    }
    "#
}
