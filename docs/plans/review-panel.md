# Review Panel — Build Plan

## Overview

A new panel in Zed for PR/MR/Branch review workflows. Works locally first (branch diff viewer), then with GitHub integration. Trait-based provider system for extensibility (GitLab, etc.). Inspired by the Agent panel's architecture.

## Design Decisions

- **Panel location**: `crates/review_ui/`
- **Views**: 4 views via `ActiveView` enum — Empty, PullRequestList, ReviewThread, FileList, Configuration
- **Toolbar**: Matches agent panel pattern — left: "Review" label, right: 3 buttons (Plus, Recent reviews dropdown, Options menu)
- **Menus**: `PopoverMenu` + `ContextMenu` for dropdowns (same pattern as agent panel). Actions dispatched via `window.dispatch_action()` since `ContextMenu::build` doesn't provide `Context<ReviewPanel>`
- **Two modes**:
  - **Local mode**: Compare any two branches. Uses existing `GitRepository`, `TreeDiff`, `BranchDiff`. No remote needed.
  - **Remote mode**: When repo has a remote + user is authenticated. Adds PR metadata, comments, reviews on top of local mode.
- **Comment display**: Panel thread view + editor gutter markers (future)
- **Auth**: `GITHUB_TOKEN` env var (already used by Zed's `HttpClient`) + `CredentialsProvider` for keychain
- **MVP target**: Local mode first, then GitHub read/write

## Existing Zed Infrastructure (reusable)

### Git operations (`crates/git/src/repository.rs`)
- `GitRepository` trait — branches, `diff_tree()` -> `TreeDiff`, `load_commit()` -> `CommitDiff`, `remote_url()`, `default_branch()`
- `Branch` struct — name, upstream, most recent commit summary
- `CommitDiff` / `CommitFile` — per-file old_text/new_text
- `TreeDiff` (`crates/git/src/status.rs:524`) — `HashMap<RepoPath, TreeDiffStatus>` (Added/Modified/Deleted)

### Branch diff (`crates/project/src/git_store/branch_diff.rs`)
- `BranchDiff` entity — manages diff between HEAD and merge-base, subscribes to git status changes
- `DiffBase::Head` vs `DiffBase::Merge { base_ref }` — two comparison modes

### Diff UI (`crates/git_ui/`)
- `ProjectDiff` (`project_diff.rs`) — multi-file diff panel, split editor, file navigation
- `FileDiffView` (`file_diff_view.rs`) — single file side-by-side diff with syntax highlighting
- `BufferDiff` (`crates/buffer_diff/`) — character-level diff computation

### Hosting providers (`crates/git/src/hosting_provider.rs`)
- `GitHostingProvider` trait — `parse_remote_url()` -> owner/repo, `build_permalink()`, `extract_pull_request()`
- `GitHostingProviderRegistry` — global, auto-detects GitHub/GitLab/Gitea/Bitbucket/etc from remote URL

### HTTP & Auth
- `HttpClient` (`crates/http_client/`) — supports `GITHUB_TOKEN` Bearer auth
- `CredentialsProvider` (`crates/credentials_provider/`) — keychain-based credential storage
- GitHub API already used for commit author avatars (`git_hosting_providers/src/providers/github.rs`)

### Gaps to fill
- No PR fetching API (list PRs, get details, reviews, comments)
- No PR/Review/Comment data structures
- No comment thread UI component
- No review state tracking (approved, changes requested)

## Phase 1: Empty Panel Skeleton

- [x] Create `crates/review_ui/` crate with Cargo.toml
- [x] Create `review_ui.rs` (lib root) with `init()` and `register()`
- [x] Create `review_panel.rs` with `ReviewPanel` struct implementing `Panel`, `Render`, `Focusable`, `EventEmitter<PanelEvent>`
- [x] Add `ToggleFocus` action in `zed_actions::review_panel` module
- [x] Register panel in `zed/src/main.rs` and `zed/src/zed.rs` (initialize_panels)
- [x] Panel icon visible in dock, opens on click

## Phase 2: Settings & Dock Switching

- [x] Create `review_panel_settings.rs` with `ReviewPanelSettings` + `RegisterSetting`
- [x] Add `ReviewPanelSettingsContent` to `settings_content` crate
- [x] Add defaults to `assets/settings/default.json` (button=true, dock=right, width=360)
- [x] Fix `vscode_import.rs` with `review_panel: None`
- [x] Panel docks left/right via settings system

## Phase 3: Panel Layout & View Switching

- [x] Add `ActiveView` enum
- [x] Implement `set_active_view()` view switching
- [x] Render toolbar with label and action buttons
- [x] Render placeholder content per view in `Render::render()`

### 3a: Toolbar Buttons & Menus (agent panel pattern)

Toolbar right side has 3 buttons: Plus, Recent dropdown, Options menu.

Reference: `crates/agent_ui/src/agent_panel.rs` lines 2360-2375.

- [x] Wire Plus button `on_click` -> `set_active_view(PullRequestList)`
- [x] Add `options_menu_handle: PopoverMenuHandle<ContextMenu>` + `recent_reviews_menu_handle` fields
- [x] Options button as `PopoverMenu` + `ContextMenu` (render_options_menu)
- [x] Recent reviews dropdown as `PopoverMenu` (render_recent_reviews_menu, placeholder for now)
- [ ] Define actions for menu entries (`OpenConfiguration`, etc.) — needed for ContextMenu callbacks
- [ ] Wire "Configuration" menu entry to switch view via action dispatch
- [x] Populate recent reviews dropdown with recent branch comparisons

## Phase 4: Data Model & Local Review

Local mode — compare two branches using existing git infrastructure. No remote needed.

### Integration path (from ReviewPanel to git data)

```
ReviewPanel
  -> project: Entity<Project>           // store in struct, get from workspace.project() in new()
  -> project.read(cx).git_store()       // &Entity<GitStore>
  -> project.read(cx).active_repository(cx)  // Option<Entity<Repository>>

Repository entity methods (all return oneshot::Receiver<Result<T>>):
  repo.update(cx, |r, cx| r.branches())          -> Vec<Branch>
  repo.update(cx, |r, cx| r.default_branch(false)) -> Option<SharedString>
  repo.update(cx, |r, cx| r.diff_tree(diff_type, cx)) -> TreeDiff

DiffTreeType (crates/git/src/status.rs:496):
  DiffTreeType::MergeBase { base, head }  // diff from merge-base (PR-style)
  DiffTreeType::Since { base, head }      // diff since a ref

TreeDiff (status.rs:524):
  entries: HashMap<RepoPath, TreeDiffStatus>
  TreeDiffStatus::Added | Modified { old: Oid } | Deleted { old: Oid }
```

Subscribe to git changes (same pattern as git_panel.rs:715):
```
cx.subscribe_in(&git_store, window, |this, _store, event, window, cx| {
    match event {
        GitStoreEvent::ActiveRepositoryChanged(_)
        | GitStoreEvent::RepositoryUpdated(..) => { this.refresh(cx); }
        _ => {}
    }
});
```

### 4a: Wire ReviewPanel to git infrastructure

- [x] Add `project: Entity<Project>` field to `ReviewPanel`
- [x] Add `active_repository: Option<Entity<Repository>>` field
- [x] Subscribe to `GitStoreEvent::ActiveRepositoryChanged`
- [x] Add `base_branch: Option<SharedString>` and `head_branch: Option<SharedString>` fields
- [x] `load_branches()` — async load default branch + current HEAD branch
- [x] Add dependencies to Cargo.toml: `project.workspace = true`, `git.workspace = true`

### 4b: Diff loading & FileList rendering

- [x] `load_diff()` — call `repo.diff_tree(DiffTreeType::MergeBase { base, head })`
- [x] Store `tree_diff: Option<TreeDiff>` field
- [x] Auto-switch to FileList view when diff loads
- [x] `render_file_list()` — display changed files from TreeDiff
- [x] Branch header ("main..feature/review_panel")
- [x] File count summary with added/modified/deleted counts
- [x] Group files by status (Added, Modified, Deleted), then by path
- [x] Hover state on file rows
- [x] Auto-refresh on branch switch via `RepositoryEvent::BranchChanged`
- [x] Click file -> open branch diff in `ProjectDiff` multibuffer, navigates to clicked file
- [x] First-click handling: dispatches `BranchDiff` action, retries after 500ms to navigate

### 4b+: File list polish

- [x] Viewed/unviewed file indicators (blue dot for unviewed, muted label for viewed)
- [x] Selected entry highlighting (info color background, follows git panel pattern)
- [x] Keyboard navigation (SelectNext/SelectPrevious/Confirm via on_action)
- [x] Viewed count in summary bar ("5/12 viewed")
- [x] Fixed-width dot indicator + truncating labels for long file names
- [x] `RecentReview` struct tracking base/head/file_count
- [x] Recent reviews dropdown populated with branch comparisons
- [x] Clicking recent review reloads that branch comparison

### 4c: ReviewProvider trait & data model

Provider-agnostic trait for GitHub/GitLab/Gitea extensibility.

- [x] Create `review_provider.rs` — trait + data structs
- [x] `ReviewProvider` trait — `fetch_pull_requests()`, `fetch_pull_request_files()`, `fetch_reviews()`
- [x] Write methods — `submit_comment()`, `submit_review()`, `merge_pull_request()`
- [x] Default (optional) methods — `react_to_comment()`, `mark_file_viewed()`, `request_reviewers()`, `fetch_checks()`, `fetch_mergeable()`, `fetch_labels()`
- [x] `PullRequestInfo` — number, title, author, description, state, base_ref, head_ref, review_status, created_at, updated_at
- [x] `PullRequestDetails` — bundles info + files + comments + checks + mergeable + labels
- [x] `PullRequestFile` — path, status (Added/Modified/Deleted/Renamed), additions, deletions
- [x] `ReviewComment` — id, author, body, created_at, path (optional), line (optional), reply_to (optional)
- [x] `CheckRun` + `CheckStatus` — CI/CD status per check
- [x] `ReviewStatus` enum — Pending, Approved, ChangesRequested, Commented
- [x] `PullRequestState` enum — Open, Closed, Merged, All
- [x] `FileChangeStatus` enum — Added, Modified, Deleted, Renamed { from }
- [x] `MergeMethod` enum — Merge, Squash, Rebase
- [x] Register module in `review_ui.rs`

### 4d: GitHub token resolver (layered auth)

Resolve GitHub token with fallback chain: env var → keychain → `gh` CLI.

- [x] Create `github_token.rs` with `resolve_github_token()` async function
- [x] Check `GITHUB_TOKEN` env var first
- [x] Check keychain via `CredentialsProvider::read_credentials("https://api.github.com")`
- [x] Fallback: shell out to `gh auth token` and capture stdout
- [x] Return `Option<String>` — None means no auth available
- [x] Register module in `review_ui.rs`

### 4e: GitHub provider implementation

Implement `ReviewProvider` for GitHub REST API.

- [x] Create `github_provider.rs` with `GitHubProvider` struct
- [x] Store `HttpClient` + resolved token
- [x] `fetch_pull_requests()` — `GET /repos/{owner}/{repo}/pulls?state={state}`
- [x] `fetch_pull_request_details()` — `GET /repos/{owner}/{repo}/pulls/{number}`
- [x] `fetch_pull_request_files()` — `GET /repos/{owner}/{repo}/pulls/{number}/files`
- [x] `fetch_reviews()` — `GET /repos/{owner}/{repo}/pulls/{number}/reviews` + `/comments`
- [x] `submit_comment()` — `POST /repos/{owner}/{repo}/issues/{number}/comments`
- [x] `submit_review()` — `POST /repos/{owner}/{repo}/pulls/{number}/reviews`
- [x] `merge_pull_request()` — `PUT /repos/{owner}/{repo}/pulls/{number}/merge`
- [x] Map GitHub JSON responses → provider-agnostic structs
- [x] Register module in `review_ui.rs`

## Phase 5: PullRequestList View

- [x] Wire `GitHubProvider` into panel — `initialize_provider()` with async token resolution
- [x] Parse GitHub remote URL (SSH + HTTPS) to extract owner/repo
- [x] Retry provider init on `RepositoryUpdated` (handles timing when snapshot not yet loaded)
- [x] Render list of PRs from `ReviewProvider::fetch_pull_requests()`
- [x] PR entry: `#number title` by author, updated date
- [x] Click PR → set base_ref/head_ref from PR data, load diff
- [x] Filter dropdown (PopoverMenu): Open / Closed / All
- [x] Search input — filter by `#number`, author, or title
- [x] PR caching — keep stale data visible during refresh (no UI blink)
- [x] Refresh via options menu (⋯ → Refresh)
- [x] Review status badge on PR entries (icon + color per ReviewStatus)
- [x] Populate "Recent reviews" dropdown from opened PRs
- [x] Fetch API file list as fallback (`load_pr_api_files`)
- [x] Render API-sourced file list when local diff unavailable (with "(remote)" badge)
- [ ] Handle missing local branch: `git fetch origin {branch}` in background (requires AskPassDelegate, deferred to Phase 8)

## Phase 6: ReviewThread View

- [x] Fetch comments on PR select via `ReviewProvider::fetch_reviews()`
- [x] Thread view with back navigation, PR header (title, author, description)
- [x] Comment cards — author, timestamp, body, file path badge for inline comments
- [x] Reply indentation (left border accent for `reply_to` comments)
- [x] "View changed files" button with file count
- [x] Comment count label with loading state
- [x] Link inline comments to diff — click file path opens diff at that file
- [x] Comment input with submit button (posts via `ReviewProvider::submit_comment`)
- [x] Submit uses `spawn_in` for window access to clear editor after success
- [ ] Markdown rendering for comment body

## Phase 7: Configuration View

- [ ] Auth status display — shows which auth method resolved (env var / keychain / gh CLI / none)
- [ ] Token input field → store via `CredentialsProvider::write_credentials()`
- [ ] "Test connection" button — fetches user info from GitHub API
- [ ] Branch picker for local mode (base/head dropdowns)
- [ ] Remote provider auto-detection via `GitHostingProviderRegistry`
- [ ] Wire Options menu "Configuration" entry to switch to this view

## Phase 8: Component Extraction (Refactor)

Extract sub-views from `ReviewPanel` into separate `Entity<T>` structs with their own `impl Render`. Follows the AgentsPanel v2 pattern — parent stores child entities, renders via `.child(self.sub_view.clone())`. Data flows through constructor injection at `cx.new()` time.

### Target structure

```
crates/review_ui/src/
├── review_ui.rs              (lib root, init/register)
├── review_panel.rs           (ReviewPanel — thin shell, toolbar, view routing)
├── review_panel_settings.rs  (settings)
├── pull_request_list.rs      (PullRequestList entity — PR list + search + filter)
├── review_view.rs            (ReviewView entity — unified PR review: files + comments + input)
├── comment_card.rs           (CommentCard — RenderOnce component for single comment)
├── review_provider.rs        (trait + data structs)
├── github_provider.rs        (GitHub impl)
└── github_token.rs           (token resolver)
```

### Extraction plan

- [ ] **`PullRequestList`** — new `Entity<T>` with `impl Render`
  - Owns: `pr_search_editor`, `pull_requests`, `pr_list_loading`, `pr_filter`, `pr_filter_menu_handle`
  - Emits: `PullRequestListEvent::Selected(PullRequestInfo)` — parent subscribes
  - Methods: `load_pull_requests()`, `set_pr_filter()`, `render()`
  - File: `pull_request_list.rs`

- [ ] **`ReviewView`** — new `Entity<T>` with `impl Render`
  - Owns: `selected_pr`, `pr_comments`, `pr_comments_loading`, `pr_api_files`, `comment_editor`, `comment_submitting`
  - Receives at construction: `provider`, `owner`, `repo`, `tree_diff` (or update methods)
  - Emits: `ReviewViewEvent::OpenFile(RepoPath)`, `ReviewViewEvent::Back`
  - Methods: `set_pull_request()`, `load_pr_comments()`, `submit_comment()`, `render()`
  - File: `review_view.rs`

- [ ] **`CommentCard`** — `RenderOnce` struct (stateless)
  - Fields: `comment: ReviewComment`, theme colors
  - No entity needed — constructed fresh each render
  - File: `comment_card.rs`

- [ ] **`ReviewPanel`** becomes thin orchestrator
  - Stores: `Entity<PullRequestList>`, `Option<Entity<ReviewView>>`, git state
  - Subscribes to child events, routes between views
  - `render()` calls `.child(self.pr_list.clone())` or `.child(review_view.clone())`

### Why this order
Extract `PullRequestList` first (simplest, self-contained). Then `ReviewView` (most complex, depends on provider/git state). `CommentCard` as `RenderOnce` last (lightweight, used by `ReviewView`).

## Phase 9: Diff viewing for remote PRs

Handle reviewing PRs when you're on a different branch.

- [x] Fetch PR ref via `smol::process::Command`: `git fetch origin pull/<N>/head`
- [x] `fetch_pr_ref()` method — spawns background fetch, calls `load_diff()` on completion
- [x] `select_pull_request()` calls `fetch_pr_ref()` instead of `load_diff()` directly
- [x] Store task in `pr_ref_fetch_task` field to prevent cancellation
- [x] While fetching: show file list from GitHub API data (`PullRequestFile`)
- [x] Once fetched: switch to local `ProjectDiff` for full diff experience
- [ ] "Open local file" action — opens the file on current working tree (for editing while reviewing)

## Phase 9a: Fix Recent Reviews

Recent reviews popover is broken for PR-based reviews. Two bugs:

**Bug 1**: `RecentReview` only stores branch names — loses PR number, selected_pr, and SHA context.
**Bug 2**: Clicking a recent review in the popover sets branches and calls `load_diff()` but does NOT restore `selected_pr`, so file diffs open as local branch diffs instead of PR diffs.

- [x] Extend `RecentReview` to store `Option<PullRequestInfo>` (or at minimum PR number + head_sha)
- [x] In `render_recent_reviews_menu` click handler: restore `selected_pr` when PR info is present, call `select_pull_request()` for PR-based reviews (which calls `fetch_pr_ref` + `load_pr_comments` + sets view)
- [x] Distinguish PR reviews vs local reviews in the menu (`"#42 main..feature"` vs `"main..feature"`)
- [x] Cap recent_reviews list size (10 entries via `truncate`)
- [x] Clear `selected_pr` when restoring a local (non-PR) review to prevent stale PR context

## Phase 9b: File List Tree/Flat Toggle

Port the tree/flat view mode from `git_panel.rs` to the review panel. Same pattern: `GitPanelViewMode` enum, `TreeViewState` with `expanded_dirs` + `logical_indices`, popover toggle in toolbar.

Reference: `crates/git_ui/src/git_panel.rs` — `GitPanelViewMode`, `TreeViewState`, `ToggleTreeView` action.

- [ ] Add `ViewMode` enum (Flat / Tree) + `TreeViewState` to `ReviewPanel`
- [ ] Add popover menu in toolbar (or options menu entry) to toggle between Flat and Tree
- [ ] `build_tree_entries()` — group sorted entries by path components into nested tree nodes
- [ ] `flatten_tree()` — produce visible entries list respecting expanded/collapsed dirs
- [ ] Render tree view: directory rows (expandable) + file rows (indented)
- [ ] `ExpandSelectedEntry` / `CollapseSelectedEntry` actions for keyboard nav
- [ ] Apply same tree view to both `render_file_list` (local diff) and `render_review_thread` file section
- [ ] Default to Flat view, persist preference in settings

## Phase 9c: Redesign Comment Display

Current comment rendering is too noisy — full cards inline with files. Redesign to use compact indicators + expandable sections, following agent panel patterns (`Disclosure` component, `HashSet` state tracking).

**Approach** (TBD — choosing between):
- **A) File-level expand in panel** — `Disclosure` chevron on file rows, expands threaded comments below the file in the panel. Uses `expanded_file_comments: HashSet<SharedString>` for state.
- **B) Badge only in panel, comments in editor** — comment count badge per file in panel, actual comments render as inline overlays in editor via existing `stored_review_comments` / `diff_review_overlays` / `Addon` infrastructure.
- **C) Both** — badge + disclosure in panel for scanning, AND inline editor overlays when file is opened.

Regardless of approach:
- [ ] Add comment count badge to file rows (e.g. "💬 3" or icon + count)
- [ ] Remove current `render_comment_card` inline dump from `render_review_thread` file list
- [ ] Track expanded state per file via `HashSet<SharedString>`
- [ ] Use `Disclosure` component for expand/collapse (agent panel pattern)
- [ ] Keep general (non-file) comments in a separate collapsible section at bottom

## Phase 10: Editor Integration

- [ ] Gutter markers for inline review comments (uses existing `stored_review_comments` infra)
- [ ] Click gutter marker → jump to comment in panel
- [ ] Inline comment creation from editor (gutter hover → comment form)
- [ ] Reuse `BufferDiff` for in-editor diff highlighting

## Files Created/Modified

### New files
- `crates/review_ui/Cargo.toml`
- `crates/review_ui/src/review_ui.rs`
- `crates/review_ui/src/review_panel.rs`
- `crates/review_ui/src/review_panel_settings.rs`
- `crates/review_ui/src/review_provider.rs` (trait + data structs)
- `crates/review_ui/src/github_token.rs` (layered token resolver)
- `crates/review_ui/src/github_provider.rs` (GitHub ReviewProvider impl)
- `crates/review_ui/src/pull_request_list.rs` (Phase 8 — extracted PR list entity)
- `crates/review_ui/src/review_view.rs` (Phase 8 — extracted unified review entity)
- `crates/review_ui/src/comment_card.rs` (Phase 8 — RenderOnce comment component)

### Modified files
- `Cargo.toml` (workspace members + dependencies)
- `crates/zed_actions/src/lib.rs` (ToggleFocus action)
- `crates/zed/Cargo.toml` (review_ui dependency)
- `crates/zed/src/main.rs` (init call)
- `crates/zed/src/zed.rs` (panel load + test init)
- `crates/settings_content/src/settings_content.rs` (ReviewPanelSettingsContent)
- `crates/settings/src/vscode_import.rs` (review_panel: None)
- `assets/settings/default.json` (review_panel defaults)
