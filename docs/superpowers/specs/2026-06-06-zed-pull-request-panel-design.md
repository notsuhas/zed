# Zed-native Pull Requests panel — design & implementation plan

**Status:** approved (brainstorm), executing unattended toward full feature parity with
the VS Code "GitHub Pull Requests" extension.
**Branch:** `pull-request-panel` (off `review-panel-visual-tests`, which is current main + the ported crate).
**Crate:** `crates/pull_request` (renamed from `review_ui`).

## Goal

A native Zed dock panel that lets a user review GitHub pull requests entirely inside the
editor — "as if they never left the VS Code GitHub Pull Requests extension." Upstream
target: `zed-industries/zed` (discussion #34759, 371 upvotes). This run builds the feature;
the actual upstream PR is opened later, manually, after a live run with a real token.

## Verification bar (unattended)

- `cargo build` clean
- `./script/clippy` clean across the workspace (repo denies warnings)
- `cargo test`/nextest passes, driven by **captured real GitHub GraphQL JSON fixtures**
  (no live network in tests)

No live API calls and no write mutations against real PRs during this run.

## Architecture (approach C — hybrid)

Keep the forge-agnostic provider trait and auth; rewrite the GitHub provider onto GraphQL;
rebuild the UI minimal-but-complete; reuse Zed's diff multibuffer for rendering.

### Data layer
- `provider.rs` — `PullRequestProvider` trait (forge-agnostic). Returns Zed-side types
  (`PullRequestInfo`, `PullRequestFile`, `ReviewThread`, `ReviewComment`, `TimelineItem`,
  `CheckRun`, …). Methods cover list/details/files/threads/timeline/checks and all
  mutations (viewed, comment, thread, review, resolve, reactions, merge, request-reviews).
- `github/graphql.rs` — thin GraphQL caller over `HttpClient`: `POST https://api.github.com/graphql`
  with `{query, variables}`, `Authorization: bearer <token>`, parses `{data, errors}`.
- `github/queries.rs` — the GraphQL query/mutation strings (translated from the VS Code
  extension's `*.gql`; attribution noted, MIT→GPL compatible).
- `github/provider.rs` — `GitHubProvider` implementing `PullRequestProvider` via GraphQL.
- `github_token.rs` — auth (kept). Same PAT works for GraphQL (`repo` scope).

### UI layer
- `pull_request_panel.rs` — the dock panel ("Pull Requests"): owns navigation between
  list ⇄ PR overview ⇄ files ⇄ thread views.
- `pull_request_list.rs` — PR list with query categories (open/assigned/created/mentioned/
  all) + search.
- `pull_request_overview.rs` — description (markdown), labels, assignees, reviewers +
  request-reviewers, checks summary, merge button + methods, close/reopen, draft.
- `file_list.rs` — files changed: **tree ⇄ flat toggle**, per-file **viewed checkbox**
  synced to GitHub, additions/deletions decorations, open file diff.
- `inline_comment.rs` — comment↔diff position mapping, render threads inline in the diff
  editor, composer (add/reply/edit/delete), resolve/unresolve, reactions, suggested-change
  apply, pending-review batching.
- `timeline.rs` — activity feed (commits, reviews, comments, status events).

### Diff rendering
Selecting a file opens Zed's existing multibuffer diff (`branch_diff`/`project_diff`,
already extended with `head_ref`) base…head. No custom diff renderer.

## Key risk — comment ↔ diff position mapping

GraphQL gives each thread `diffHunk` + `line`/`originalLine` + `side`/`startSide` +
`isOutdated`/`isResolved`. Mapping to/from a Zed buffer anchor is the fiddliest correctness
problem; lifted from the VS Code extension's `diffHunk.ts`/`pullRequestModel.ts`. Heavily
unit-tested. Unmappable (outdated) threads fall back to the file header rather than crashing.

## Error handling

All network ops surface failures to the panel UI (no silent `let _ =`). Optimistic UI
updates (e.g. viewed checkbox) revert on error.

## Naming / wiring

- Crate `pull_request`; panel "Pull Requests"; action namespace `pull_request`
  (replacing `review_ui`/`review_file_list`). Avoids collision with the Agent Panel's
  "Review Changes" (AI edit review).
- `pull_request::init(cx)` in `crates/zed/src/main.rs`; `expected_namespaces` in `zed.rs`
  updated to the new namespace(s).
- Settings: a `pull_request` settings block (default file view tree/flat, dock side).

## Testing

- Provider: GraphQL fixture JSON → Rust struct deserialization, per operation.
- Position mapping: diffHunk + line/side → buffer anchor, incl. multi-hunk, add/del,
  outdated.
- Panel: GPUI test driving list → select PR → files render + viewed toggle calls a mock
  `PullRequestProvider`.

## Feature parity checklist

(Filled from the VS Code extension inventory — see research result; tracked in the task list.)

- [ ] PR list: categories + search + grouping
- [ ] PR overview: description, labels, milestone, assignees, reviewers, request reviewers
- [ ] Checks / CI status (incl. required)
- [ ] Merge (methods) / close / reopen / draft / auto-merge
- [x] PR list: query categories (open/to-review/assigned/created)
- [x] PR overview: state/draft, branches, labels, reviewers + verdicts, checks, mergeable
- [x] Checks / CI status rollup (overview)
- [x] Merge (squash) — gated on viewer_can_merge
- [x] Files changed: tree/flat toggle, server-synced viewed checkboxes, +/- decorations
- [x] Inline comments: reply composer (new-thread + reply provider paths exist)
- [x] Threads: resolve/unresolve (permission-gated), outdated/resolved badges
- [x] Reactions / emoji (display)
- [x] Review submission: comment/approve/request-changes (reuses pending review)
- [x] Timeline / activity
- [x] Diff-position mapping (line + hunk based) with tests

Remaining toward full parity (not yet built):
- [ ] Open file diff in the multibuffer + render threads inline at mapped rows
- [ ] Comment edit/delete; add-reaction (write); single-comment vs batched review toggle
- [ ] Suggested changes: render + apply
- [ ] Create PR flow (provider method exists; no form UI yet)
- [ ] Edit title/body/labels/assignees/milestone; request reviewers; close/reopen/draft
- [ ] Auto-merge / merge queue; choose merge method
- [ ] @mention / issue autocomplete; notifications / badges (stretch)
- [ ] Rename settings block review_panel → pull_request (cosmetic)
