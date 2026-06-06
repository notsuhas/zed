//! The "Pull Requests" dock panel.
//!
//! Navigates between a PR list and a PR detail view (overview, changed files
//! with tree/flat layout and server-synced viewed state, and review threads),
//! talking only to a [`PullRequestProvider`].

use crate::github_provider::GitHubProvider;
use crate::github_token::{GitHubTokenSource, resolve_github_token};
use crate::provider::*;
use crate::review_panel_settings::ReviewPanelSettings;
use anyhow::Result;
use gpui::{
    App, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle, Focusable, Pixels, Render,
    SharedString, Subscription, Task, WeakEntity, Window,
};
use crate::diff_position::thread_anchor;
use crate::file_tree::build_tree_rows;
use buffer_diff::BufferDiff;
use editor::display_map::{BlockContext, BlockPlacement, BlockProperties, BlockStyle};
use editor::{Anchor, Editor};
use http_client::HttpClient;
use language::Buffer;
use multi_buffer::MultiBuffer;
use text::Point;
use project::Project;
use project::git_store::{GitStoreEvent, Repository};
use settings::Settings as _;
use std::collections::HashSet;
use std::sync::Arc;
use ui::{
    Button, ButtonStyle, Checkbox, Color, Icon, IconButton, IconName, IconSize, Label, LabelSize,
    ToggleState, Tooltip, prelude::*,
};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};
use zed_actions::pull_request::{AddComment, ToggleFocus};

const PULL_REQUEST_PANEL_KEY: &str = "PullRequestPanel";

/// Which query bucket the list view is showing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ListFilter {
    Open,
    ReviewRequested,
    Assigned,
    Created,
}

impl ListFilter {
    fn label(self) -> &'static str {
        match self {
            ListFilter::Open => "Open",
            ListFilter::ReviewRequested => "To Review",
            ListFilter::Assigned => "Assigned",
            ListFilter::Created => "Created",
        }
    }

    /// GitHub search qualifier appended to the repo scope.
    fn query(self) -> &'static str {
        match self {
            ListFilter::Open => "is:open",
            ListFilter::ReviewRequested => "is:open review-requested:@me",
            ListFilter::Assigned => "is:open assignee:@me",
            ListFilter::Created => "is:open author:@me",
        }
    }

    const ALL: [ListFilter; 4] = [
        ListFilter::Open,
        ListFilter::ReviewRequested,
        ListFilter::Assigned,
        ListFilter::Created,
    ];
}

/// File list presentation.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FileLayout {
    Tree,
    Flat,
}

/// What the shared comment composer is currently targeting.
#[derive(Clone, PartialEq)]
enum ComposerTarget {
    /// Reply to an existing thread.
    Reply(SharedString),
    /// Start a new thread on a file line (right side).
    NewThread { path: SharedString, line: u32 },
    /// Edit an existing comment.
    Edit(SharedString),
}

/// Everything fetched for the selected pull request.
struct LoadedPullRequest {
    detail: PullRequest,
    files: Vec<PullRequestFile>,
    threads: Vec<ReviewThread>,
    timeline: Vec<TimelineItem>,
}

enum ActiveView {
    List,
    Detail,
    Create,
}

pub struct PullRequestPanel {
    // Held for opening file diffs in the workspace (wired by the file view).
    #[allow(dead_code)]
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    active_repository: Option<Entity<Repository>>,
    http_client: Arc<dyn HttpClient>,
    fs: Arc<dyn fs::Fs>,
    focus_handle: FocusHandle,
    width: Option<Pixels>,
    provider: Option<Arc<dyn PullRequestProvider>>,
    auth_source: GitHubTokenSource,
    owner: Option<String>,
    repo: Option<String>,
    active_view: ActiveView,
    filter: ListFilter,
    pull_requests: Vec<PullRequestInfo>,
    list_error: Option<SharedString>,
    list_loading: bool,
    selected: Option<LoadedPullRequest>,
    detail_error: Option<SharedString>,
    detail_loading: bool,
    file_layout: FileLayout,
    hide_viewed: bool,
    collapsed_dirs: HashSet<String>,
    comment_editor: Entity<Editor>,
    composer_target: Option<ComposerTarget>,
    create_title: Entity<Editor>,
    create_body: Entity<Editor>,
    create_base: Entity<Editor>,
    create_head: Entity<Editor>,
    create_draft: bool,
    create_error: Option<SharedString>,
    /// When set, the Create view edits this existing PR's title/body instead of
    /// creating a new one.
    edit_target: Option<SharedString>,
    search_editor: Entity<Editor>,
    _refresh_task: Option<Task<()>>,
    _detail_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

pub fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
        workspace.toggle_panel_focus::<PullRequestPanel>(window, cx);
    });

    // Start a review comment on the current line of the focused diff editor.
    workspace.register_action(|workspace, _: &AddComment, window, cx| {
        let Some(panel) = workspace.panel::<PullRequestPanel>(cx) else {
            return;
        };
        let Some(editor) = workspace
            .active_item(cx)
            .and_then(|item| item.act_as::<Editor>(cx))
        else {
            return;
        };
        let Some((path, line)) = editor.update(cx, |editor, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let head = editor.selections.newest_anchor().head();
            let row = snapshot.summary_for_anchor::<Point>(&head).row;
            let buffer = editor.buffer().read(cx).as_singleton()?;
            let file = buffer.read(cx).file()?;
            let path = SharedString::from(file.path().as_std_path().to_string_lossy().to_string());
            Some((path, row + 1))
        }) else {
            return;
        };
        panel.update(cx, |panel, cx| {
            panel.start_new_thread(path, line, window, cx);
        });
    });
}

impl PullRequestPanel {
    pub fn new(
        workspace: &Workspace,
        weak_workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let fs = workspace.app_state().fs.clone();
        let project = workspace.project().clone();
        let active_repository = project.read(cx).active_repository(cx);
        let git_store = project.read(cx).git_store().clone();

        let subscription =
            cx.subscribe_in(&git_store, window, |this, _store, event, _window, cx| {
                match event {
                    GitStoreEvent::ActiveRepositoryChanged(_) => {
                        this.active_repository = this.project.read(cx).active_repository(cx);
                        this.initialize_provider(cx);
                        cx.notify();
                    }
                    GitStoreEvent::RepositoryUpdated(..) => {
                        if this.provider.is_none() {
                            this.initialize_provider(cx);
                        }
                    }
                    _ => {}
                }
            });

        let comment_editor = cx.new(|cx| {
            let mut editor = Editor::auto_height(1, 6, window, cx);
            editor.set_placeholder_text("Reply…", window, cx);
            editor
        });
        let create_title = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Pull request title", window, cx);
            editor
        });
        let create_body = cx.new(|cx| {
            let mut editor = Editor::auto_height(2, 10, window, cx);
            editor.set_placeholder_text("Description", window, cx);
            editor
        });
        let create_base = cx.new(|cx| Editor::single_line(window, cx));
        let create_head = cx.new(|cx| Editor::single_line(window, cx));
        let search_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Search (e.g. is:merged author:me)", window, cx);
            editor
        });

        let mut this = Self {
            workspace: weak_workspace,
            project,
            active_repository,
            http_client: workspace.client().http_client(),
            fs,
            focus_handle: cx.focus_handle(),
            width: None,
            provider: None,
            auth_source: GitHubTokenSource::None,
            owner: None,
            repo: None,
            active_view: ActiveView::List,
            filter: ListFilter::Open,
            pull_requests: Vec::new(),
            list_error: None,
            list_loading: false,
            selected: None,
            detail_error: None,
            detail_loading: false,
            file_layout: FileLayout::Tree,
            hide_viewed: false,
            collapsed_dirs: HashSet::new(),
            comment_editor,
            composer_target: None,
            create_title,
            create_body,
            create_base,
            create_head,
            create_draft: false,
            create_error: None,
            edit_target: None,
            search_editor,
            _refresh_task: None,
            _detail_task: None,
            _subscriptions: vec![subscription],
        };
        this.initialize_provider(cx);
        this
    }

    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, window, cx| {
            let weak_workspace = workspace.weak_handle();
            cx.new(|cx| PullRequestPanel::new(workspace, weak_workspace, window, cx))
        })
    }

    /// Resolve the GitHub remote + token and build the provider, then refresh.
    fn initialize_provider(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.active_repository.clone() else {
            return;
        };
        let Some(remote_url) = repo.read(cx).default_remote_url() else {
            return;
        };
        let Ok((owner, repo_name)) = parse_github_remote(&remote_url) else {
            return;
        };

        self.owner = Some(owner);
        self.repo = Some(repo_name);

        let http_client = self.http_client.clone();
        let credentials_provider = zed_credentials_provider::global(cx);

        cx.spawn(async move |this, cx| {
            let resolved = resolve_github_token(credentials_provider, cx).await;
            let provider: Arc<dyn PullRequestProvider> =
                Arc::new(GitHubProvider::new(http_client, resolved.token));
            this.update(cx, |this, cx| {
                this.provider = Some(provider);
                this.auth_source = resolved.source;
                this.refresh_list(cx);
            })
            .ok();
        })
        .detach();
    }

    fn refresh_list(&mut self, cx: &mut Context<Self>) {
        let (Some(provider), Some(owner), Some(repo)) =
            (self.provider.clone(), self.owner.clone(), self.repo.clone())
        else {
            return;
        };
        let search = self.search_editor.read(cx).text(cx);
        let query = if search.trim().is_empty() {
            self.filter.query().to_string()
        } else {
            format!("{} {}", self.filter.query(), search.trim())
        };
        self.list_loading = true;
        self.list_error = None;
        cx.notify();

        self._refresh_task = Some(cx.spawn(async move |this, cx| {
            let result = provider.list_pull_requests(&owner, &repo, &query).await;
            this.update(cx, |this, cx| {
                this.list_loading = false;
                match result {
                    Ok(prs) => this.pull_requests = prs,
                    Err(error) => this.list_error = Some(error.to_string().into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn set_filter(&mut self, filter: ListFilter, cx: &mut Context<Self>) {
        if self.filter != filter {
            self.filter = filter;
            self.refresh_list(cx);
        }
    }

    fn select_pull_request(&mut self, info: PullRequestInfo, cx: &mut Context<Self>) {
        let Some(provider) = self.provider.clone() else {
            return;
        };
        let owner = info.id.owner.to_string();
        let repo = info.id.repo.to_string();
        let number = info.id.number;
        self.active_view = ActiveView::Detail;
        self.detail_loading = true;
        self.detail_error = None;
        self.selected = None;
        cx.notify();

        self._detail_task = Some(cx.spawn(async move |this, cx| {
            let detail = provider.fetch_pull_request(&owner, &repo, number).await;
            let files = provider.fetch_files(&owner, &repo, number).await;
            let threads = provider.fetch_review_threads(&owner, &repo, number).await;
            // Timeline is non-critical; an error here shouldn't block the view.
            let timeline = provider
                .fetch_timeline(&owner, &repo, number)
                .await
                .unwrap_or_default();
            this.update(cx, |this, cx| {
                this.detail_loading = false;
                match (detail, files, threads) {
                    (Ok(detail), Ok(files), Ok(threads)) => {
                        this.selected = Some(LoadedPullRequest {
                            detail,
                            files,
                            threads,
                            timeline,
                        });
                    }
                    (detail, files, threads) => {
                        let error = detail
                            .err()
                            .or(files.err())
                            .or(threads.err())
                            .map(|error| error.to_string())
                            .unwrap_or_default();
                        this.detail_error = Some(error.into());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn back_to_list(&mut self, cx: &mut Context<Self>) {
        self.active_view = ActiveView::List;
        self.selected = None;
        cx.notify();
    }

    fn start_create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Prefill head with the current branch.
        let head = self
            .active_repository
            .as_ref()
            .and_then(|repo| repo.read(cx).branch.as_ref().map(|b| b.ref_name.clone()))
            .map(|name| name.trim_start_matches("refs/heads/").to_string())
            .unwrap_or_default();
        self.create_head
            .update(cx, |editor, cx| editor.set_text(head, window, cx));
        self.create_title
            .update(cx, |editor, cx| editor.set_text("", window, cx));
        self.create_body
            .update(cx, |editor, cx| editor.set_text("", window, cx));
        self.edit_target = None;
        self.create_error = None;
        self.active_view = ActiveView::Create;
        cx.notify();
    }

    /// Edit the selected PR's title/body, reusing the Create form.
    fn start_edit_pr(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(loaded) = self.selected.as_ref() else {
            return;
        };
        let title = loaded.detail.info.title.to_string();
        let body = loaded.detail.body.to_string();
        self.edit_target = Some(loaded.detail.info.id.node_id.clone());
        self.create_title
            .update(cx, |editor, cx| editor.set_text(title, window, cx));
        self.create_body
            .update(cx, |editor, cx| editor.set_text(body, window, cx));
        self.create_error = None;
        self.active_view = ActiveView::Create;
        cx.notify();
    }

    fn submit_create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(provider) = self.provider.clone() else {
            return;
        };
        // Edit mode updates the existing PR's title/body.
        if let Some(node_id) = self.edit_target.clone() {
            let title = self.create_title.read(cx).text(cx);
            let body = self.create_body.read(cx).text(cx);
            self.edit_target = None;
            self.active_view = ActiveView::Detail;
            cx.notify();
            cx.spawn(async move |this, cx| {
                let result = provider
                    .update_pull_request(&node_id, Some(&title), Some(&body))
                    .await;
                this.update(cx, |this, cx| {
                    if let Err(error) = result {
                        this.detail_error = Some(error.to_string().into());
                    }
                    this.refresh_detail(cx);
                })
                .ok();
            })
            .detach();
            return;
        }

        let (Some(owner), Some(repo)) = (self.owner.clone(), self.repo.clone()) else {
            return;
        };
        let title = self.create_title.read(cx).text(cx);
        let base = self.create_base.read(cx).text(cx);
        let head = self.create_head.read(cx).text(cx);
        if title.trim().is_empty() || base.trim().is_empty() || head.trim().is_empty() {
            self.create_error = Some("Title, base, and head are required".into());
            cx.notify();
            return;
        }
        let input = CreatePullRequest {
            owner: owner.into(),
            repo: repo.into(),
            base_ref: base.trim().to_string().into(),
            head_ref: head.trim().to_string().into(),
            title: title.into(),
            body: self.create_body.read(cx).text(cx).into(),
            draft: self.create_draft,
        };
        let _ = window;
        cx.spawn(async move |this, cx| {
            let result = provider.create_pull_request(&input).await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(_) => {
                        this.active_view = ActiveView::List;
                        this.refresh_list(cx);
                    }
                    Err(error) => this.create_error = Some(error.to_string().into()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn toggle_dir(&mut self, dir_path: String, cx: &mut Context<Self>) {
        if !self.collapsed_dirs.remove(&dir_path) {
            self.collapsed_dirs.insert(dir_path);
        }
        cx.notify();
    }

    /// Re-fetch the selected PR's review threads (after a thread mutation).
    fn refresh_threads(&mut self, cx: &mut Context<Self>) {
        let (Some(provider), Some(loaded)) = (self.provider.clone(), self.selected.as_ref()) else {
            return;
        };
        let id = loaded.detail.info.id.clone();
        cx.spawn(async move |this, cx| {
            let threads = provider
                .fetch_review_threads(&id.owner, &id.repo, id.number)
                .await;
            this.update(cx, |this, cx| {
                if let (Ok(threads), Some(loaded)) = (threads, this.selected.as_mut()) {
                    loaded.threads = threads;
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn start_reply(
        &mut self,
        thread_id: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_composer(ComposerTarget::Reply(thread_id), window, cx);
    }

    /// Begin a new inline comment on a file line (invoked from the diff via the
    /// `AddComment` action).
    pub fn start_new_thread(
        &mut self,
        path: SharedString,
        line: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.active_view = ActiveView::Detail;
        self.set_composer(ComposerTarget::NewThread { path, line }, window, cx);
    }

    fn start_edit(
        &mut self,
        comment_id: SharedString,
        body: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.comment_editor
            .update(cx, |editor, cx| editor.set_text(body.to_string(), window, cx));
        self.set_composer(ComposerTarget::Edit(comment_id), window, cx);
    }

    fn set_composer(&mut self, target: ComposerTarget, window: &mut Window, cx: &mut Context<Self>) {
        self.composer_target = Some(target);
        let handle = self.comment_editor.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
        cx.notify();
    }

    fn submit_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.composer_target.clone() else {
            return;
        };
        let body = self.comment_editor.read(cx).text(cx);
        if body.trim().is_empty() {
            return;
        }
        let (Some(provider), Some(loaded)) = (self.provider.clone(), self.selected.as_ref()) else {
            return;
        };
        let pull_request = loaded.detail.info.id.clone();
        let commit_sha = loaded.detail.info.head_sha.clone();
        self.comment_editor
            .update(cx, |editor, cx| editor.clear(window, cx));
        self.composer_target = None;
        cx.notify();

        // Editing routes to a different mutation than posting.
        if let ComposerTarget::Edit(comment_id) = target {
            cx.spawn(async move |this, cx| {
                let result = provider.edit_comment(&comment_id, &body).await;
                this.update(cx, |this, cx| {
                    if let Err(error) = result {
                        this.detail_error = Some(error.to_string().into());
                    }
                    this.refresh_threads(cx);
                })
                .ok();
            })
            .detach();
            return;
        }

        let comment_target = match target {
            ComposerTarget::Reply(thread_id) => CommentTarget::Reply {
                in_reply_to: thread_id,
            },
            ComposerTarget::NewThread { path, line } => CommentTarget::NewThread {
                path,
                side: DiffSide::Right,
                line,
                start_line: None,
            },
            ComposerTarget::Edit(_) => unreachable!(),
        };
        let new_comment = NewComment {
            pull_request,
            body: body.into(),
            target: comment_target,
            review_id: None,
            commit_sha,
        };
        cx.spawn(async move |this, cx| {
            let result = provider.add_comment(&new_comment).await;
            this.update(cx, |this, cx| {
                if let Err(error) = result {
                    this.detail_error = Some(error.to_string().into());
                }
                this.refresh_threads(cx);
            })
            .ok();
        })
        .detach();
    }

    fn toggle_reaction(
        &mut self,
        subject_id: SharedString,
        content: SharedString,
        reacted: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(provider) = self.provider.clone() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = if reacted {
                provider.remove_reaction(&subject_id, &content).await
            } else {
                provider.add_reaction(&subject_id, &content).await
            };
            this.update(cx, |this, cx| {
                if let Err(error) = result {
                    this.detail_error = Some(error.to_string().into());
                }
                this.refresh_threads(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Apply a ```suggestion block to the working-tree file over the thread's
    /// line range. Requires the PR branch to be checked out locally.
    fn apply_suggestion(
        &mut self,
        path: SharedString,
        start_line: u32,
        end_line: u32,
        suggestion: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(loaded) = self.selected.as_ref() else {
            return;
        };
        let head_ref = loaded.detail.info.head_ref.to_string();
        let Some(root) = self.active_repository.as_ref().and_then(|repo| {
            let repo = repo.read(cx);
            let branch = repo.branch.as_ref()?;
            (branch.ref_name.trim_start_matches("refs/heads/") == head_ref)
                .then(|| repo.work_directory_abs_path.clone())
        }) else {
            self.detail_error =
                Some("Apply needs the PR branch checked out locally".into());
            cx.notify();
            return;
        };
        let abs_path = root.join(std::path::Path::new(path.as_ref()));
        let project = self.project.clone();
        cx.spawn_in(window, async move |this, cx| {
            let project_path = project.read_with(cx, |project, cx| {
                project.project_path_for_absolute_path(&abs_path, cx)
            });
            let Some(project_path) = project_path else {
                return anyhow::Ok(());
            };
            let buffer = project
                .update(cx, |project, cx| project.open_buffer(project_path, cx))
                .await?;
            buffer.update(cx, |buffer, cx| {
                let start = Point::new(start_line.saturating_sub(1), 0);
                let end_row = end_line.saturating_sub(1).min(buffer.max_point().row);
                let end = Point::new(end_row, buffer.line_len(end_row));
                buffer.edit([(start..end, suggestion)], None, cx);
            });
            this.update(cx, |_this, cx| cx.notify()).ok();
            anyhow::Ok(())
        })
        .detach();
    }

    fn delete_comment(&mut self, comment_database_id: u64, cx: &mut Context<Self>) {
        let (Some(provider), Some(loaded)) = (self.provider.clone(), self.selected.as_ref()) else {
            return;
        };
        let owner = loaded.detail.info.id.owner.to_string();
        let repo = loaded.detail.info.id.repo.to_string();
        cx.spawn(async move |this, cx| {
            let result = provider
                .delete_comment(&owner, &repo, comment_database_id)
                .await;
            this.update(cx, |this, cx| {
                if let Err(error) = result {
                    this.detail_error = Some(error.to_string().into());
                }
                this.refresh_threads(cx);
            })
            .ok();
        })
        .detach();
    }

    fn set_thread_resolved(
        &mut self,
        thread_id: SharedString,
        resolved: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(provider) = self.provider.clone() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = if resolved {
                provider.resolve_thread(&thread_id).await
            } else {
                provider.unresolve_thread(&thread_id).await
            };
            this.update(cx, |this, cx| {
                match result {
                    Ok(updated) => {
                        if let Some(loaded) = this.selected.as_mut() {
                            if let Some(thread) =
                                loaded.threads.iter_mut().find(|t| t.id == updated.id)
                            {
                                *thread = updated;
                            }
                        }
                    }
                    Err(error) => this.detail_error = Some(error.to_string().into()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Toggle a file's viewed state, optimistically updating then syncing.
    fn toggle_file_viewed(&mut self, path: SharedString, cx: &mut Context<Self>) {
        let (Some(provider), Some(loaded)) = (self.provider.clone(), self.selected.as_mut()) else {
            return;
        };
        let pr_node_id = loaded.detail.info.id.node_id.to_string();
        let Some(file) = loaded.files.iter_mut().find(|file| file.path == path) else {
            return;
        };
        let was_viewed = file.viewed_state == ViewedState::Viewed;
        file.viewed_state = if was_viewed {
            ViewedState::Unviewed
        } else {
            ViewedState::Viewed
        };
        cx.notify();

        let path_string = path.to_string();
        cx.spawn(async move |this, cx| {
            let result = if was_viewed {
                provider.unmark_file_viewed(&pr_node_id, &path_string).await
            } else {
                provider.mark_file_viewed(&pr_node_id, &path_string).await
            };
            if result.is_err() {
                // Revert the optimistic toggle on failure.
                this.update(cx, |this, cx| {
                    if let Some(loaded) = this.selected.as_mut() {
                        if let Some(file) =
                            loaded.files.iter_mut().find(|file| file.path == path)
                        {
                            file.viewed_state = if was_viewed {
                                ViewedState::Viewed
                            } else {
                                ViewedState::Unviewed
                            };
                        }
                    }
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    /// Open a file's diff in the center pane.
    ///
    /// Matching the VS Code extension: when the PR's head branch is the branch
    /// checked out locally, open the real working-tree file (editable) diffed
    /// against the PR base content (so edits save to disk and the diff spans
    /// base → working tree). Otherwise open a read-only base↔head diff of the
    /// API-fetched blob text (works for forks / un-fetched heads).
    fn open_file_diff(&mut self, path: SharedString, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(provider), Some(loaded)) = (self.provider.clone(), self.selected.as_ref()) else {
            return;
        };
        let info = &loaded.detail.info;
        let owner = info.id.owner.to_string();
        let repo = info.id.repo.to_string();
        let base_sha = info.base_sha.to_string();
        let head_sha = info.head_sha.to_string();
        let head_ref = info.head_ref.to_string();
        let project = self.project.clone();
        let workspace = self.workspace.clone();
        let languages = self.project.read(cx).languages().clone();
        // Threads on this file, shown inline in the diff.
        let file_threads: Vec<ReviewThread> = loaded
            .threads
            .iter()
            .filter(|thread| thread.path == path)
            .cloned()
            .collect();

        // Is the PR's head branch the one checked out in the local repo? If so,
        // resolve its working directory so we can open the real file.
        let local_root = self.active_repository.as_ref().and_then(|repo| {
            let repo = repo.read(cx);
            let branch = repo.branch.as_ref()?;
            let current = branch.ref_name.trim_start_matches("refs/heads/");
            (current == head_ref).then(|| repo.work_directory_abs_path.clone())
        });

        cx.spawn_in(window, async move |_this, cx| {
            let base = provider
                .fetch_file_content(&owner, &repo, &format!("{base_sha}:{path}"))
                .await
                .unwrap_or(None)
                .unwrap_or_default();

            // Try the editable local-file path when the PR is checked out.
            if let Some(root) = local_root {
                let abs_path = root.join(std::path::Path::new(path.as_ref()));
                let project_path = project.read_with(cx, |project, cx| {
                    project.project_path_for_absolute_path(&abs_path, cx)
                });
                if let Some(project_path) = project_path {
                    let buffer = project
                        .update(cx, |project, cx| project.open_buffer(project_path, cx))
                        .await?;
                    workspace
                        .update_in(cx, |workspace, window, cx| {
                            open_diff_item(
                                buffer,
                                base.as_ref(),
                                false,
                                file_threads.clone(),
                                &project,
                                workspace,
                                window,
                                cx,
                            );
                        })
                        .ok();
                    return anyhow::Ok(());
                }
            }

            // Fallback: read-only diff of fetched head content.
            let head = provider
                .fetch_file_content(&owner, &repo, &format!("{head_sha}:{path}"))
                .await
                .unwrap_or(None)
                .unwrap_or_default();
            let language = languages
                .load_language_for_file_path(std::path::Path::new(path.as_ref()))
                .await
                .ok();
            workspace
                .update_in(cx, |workspace, window, cx| {
                    let head_buffer = cx.new(|cx| {
                        let mut buffer = Buffer::local(head.to_string(), cx);
                        buffer.set_language(language.clone(), cx);
                        buffer
                    });
                    open_diff_item(
                        head_buffer,
                        base.as_ref(),
                        true,
                        file_threads,
                        &project,
                        workspace,
                        window,
                        cx,
                    );
                })
                .ok();
            anyhow::Ok(())
        })
        .detach();
    }

    /// Whether the selected PR's head branch is the locally checked-out branch.
    fn pr_is_checked_out(&self, cx: &App) -> bool {
        let (Some(repo), Some(loaded)) = (self.active_repository.as_ref(), self.selected.as_ref())
        else {
            return false;
        };
        let head_ref = &loaded.detail.info.head_ref;
        repo.read(cx)
            .branch
            .as_ref()
            .map(|branch| branch.ref_name.trim_start_matches("refs/heads/") == head_ref.as_ref())
            .unwrap_or(false)
    }

    fn checkout_pr(&mut self, cx: &mut Context<Self>) {
        let (Some(repo), Some(loaded)) = (self.active_repository.clone(), self.selected.as_ref())
        else {
            return;
        };
        let head = loaded.detail.info.head_ref.to_string();
        cx.spawn(async move |this, cx| {
            let receiver = repo.update(cx, |repo, _cx| repo.change_branch(head));
            let result = receiver.await.unwrap_or_else(|_| Ok(()));
            this.update(cx, |this, cx| {
                if let Err(error) = result {
                    this.detail_error = Some(error.to_string().into());
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn close_or_reopen(&mut self, cx: &mut Context<Self>) {
        let (Some(provider), Some(loaded)) = (self.provider.clone(), self.selected.as_ref()) else {
            return;
        };
        let is_open = loaded.detail.info.state == PullRequestState::Open;
        let node_id = loaded.detail.info.id.node_id.to_string();
        cx.spawn(async move |this, cx| {
            let result = if is_open {
                provider.close_pull_request(&node_id).await
            } else {
                provider.reopen_pull_request(&node_id).await
            };
            this.update(cx, |this, cx| {
                if let Err(error) = result {
                    this.detail_error = Some(error.to_string().into());
                }
                this.refresh_detail(cx);
            })
            .ok();
        })
        .detach();
    }

    fn toggle_draft(&mut self, cx: &mut Context<Self>) {
        let (Some(provider), Some(loaded)) = (self.provider.clone(), self.selected.as_ref()) else {
            return;
        };
        let is_draft = loaded.detail.info.is_draft;
        let node_id = loaded.detail.info.id.node_id.to_string();
        cx.spawn(async move |this, cx| {
            let result = if is_draft {
                provider.mark_ready_for_review(&node_id).await
            } else {
                provider.convert_to_draft(&node_id).await
            };
            this.update(cx, |this, cx| {
                if let Err(error) = result {
                    this.detail_error = Some(error.to_string().into());
                }
                this.refresh_detail(cx);
            })
            .ok();
        })
        .detach();
    }

    fn merge_selected(&mut self, method: MergeMethod, cx: &mut Context<Self>) {
        let (Some(provider), Some(loaded)) = (self.provider.clone(), self.selected.as_ref()) else {
            return;
        };
        let node_id = loaded.detail.info.id.node_id.to_string();
        cx.spawn(async move |this, cx| {
            let result = provider.merge_pull_request(&node_id, method, None).await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(()) => this.refresh_detail(cx),
                    Err(error) => this.detail_error = Some(error.to_string().into()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Re-fetch the full detail for the selected PR (after a state change).
    fn refresh_detail(&mut self, cx: &mut Context<Self>) {
        if let Some(loaded) = self.selected.as_ref() {
            self.select_pull_request(loaded.detail.info.clone(), cx);
        }
    }

    fn submit_review(&mut self, event: ReviewEvent, cx: &mut Context<Self>) {
        let (Some(provider), Some(loaded)) = (self.provider.clone(), self.selected.as_ref()) else {
            return;
        };
        let info = loaded.detail.info.clone();
        let owner = info.id.owner.to_string();
        let repo = info.id.repo.to_string();
        let node_id = info.id.node_id.to_string();
        let number = info.id.number;
        cx.spawn(async move |this, cx| {
            // Reuse a pending review if one exists, otherwise start a fresh one.
            let review_id = match provider.pending_review_id(&owner, &repo, number).await {
                Ok(Some(id)) => Ok(id),
                Ok(None) => provider.start_review(&node_id).await,
                Err(error) => Err(error),
            };
            let result = match review_id {
                Ok(review_id) => provider.submit_review(&review_id, event, "").await,
                Err(error) => Err(error),
            };
            this.update(cx, |this, cx| {
                if let Err(error) = result {
                    this.detail_error = Some(error.to_string().into());
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

impl PullRequestPanel {
    fn render_list(&self, cx: &Context<Self>) -> impl IntoElement {
        let filters = h_flex()
            .gap_1()
            .p_2()
            .justify_between()
            .child(
                h_flex()
                    .gap_1()
                    .children(ListFilter::ALL.map(|filter| {
                        let selected = filter == self.filter;
                        Button::new(SharedString::from(filter.label()), filter.label())
                            .label_size(LabelSize::Small)
                            .style(if selected {
                                ButtonStyle::Filled
                            } else {
                                ButtonStyle::Subtle
                            })
                            .on_click(
                                cx.listener(move |this, _, _window, cx| this.set_filter(filter, cx)),
                            )
                    })),
            )
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        IconButton::new("create-pr", IconName::Plus)
                            .tooltip(Tooltip::text("New pull request"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.start_create(window, cx)
                            })),
                    )
                    .child(
                        IconButton::new("refresh-list", IconName::RotateCw)
                            .tooltip(Tooltip::text("Refresh"))
                            .on_click(cx.listener(|this, _, _window, cx| this.refresh_list(cx))),
                    ),
            );

        let search_row = h_flex()
            .px_2()
            .pb_1()
            .gap_1()
            .child(div().flex_1().child(self.search_editor.clone()))
            .child(
                IconButton::new("search-go", IconName::MagnifyingGlass)
                    .tooltip(Tooltip::text("Search"))
                    .on_click(cx.listener(|this, _, _window, cx| this.refresh_list(cx))),
            );

        let auth_status = h_flex().px_2().pb_1().child(
            Label::new(format!("Auth: {}", self.auth_source.label()))
                .size(LabelSize::XSmall)
                .color(Color::Muted),
        );

        let body = if self.list_loading {
            v_flex()
                .p_4()
                .child(Label::new("Loading pull requests…").color(Color::Muted))
                .into_any_element()
        } else if let Some(error) = &self.list_error {
            v_flex()
                .p_4()
                .child(Label::new(error.clone()).color(Color::Error))
                .into_any_element()
        } else if self.pull_requests.is_empty() {
            v_flex()
                .p_4()
                .child(Label::new("No pull requests.").color(Color::Muted))
                .into_any_element()
        } else {
            v_flex()
                .id("pr-list")
                .overflow_y_scroll()
                .children(
                    self.pull_requests
                        .iter()
                        .cloned()
                        .map(|pr| self.render_pr_row(pr, cx)),
                )
                .into_any_element()
        };

        v_flex()
            .size_full()
            .child(filters)
            .child(search_row)
            .child(auth_status)
            .child(body)
    }

    fn render_pr_row(&self, pr: PullRequestInfo, cx: &Context<Self>) -> impl IntoElement {
        let number = pr.id.number;
        let state_color = match pr.state {
            PullRequestState::Open => Color::Success,
            PullRequestState::Merged => Color::Accent,
            PullRequestState::Closed => Color::Error,
        };
        h_flex()
            .id(("pr-row", number))
            .w_full()
            .px_2()
            .py_1p5()
            .gap_2()
            .items_start()
            .hover(|style| style.bg(cx.theme().colors().element_hover))
            .cursor_pointer()
            .child(Icon::new(IconName::PullRequest).size(IconSize::Small).color(state_color))
            .child(
                v_flex()
                    .gap_0p5()
                    .child(Label::new(pr.title.clone()).size(LabelSize::Small))
                    .child(
                        Label::new(format!("#{} by {}", number, pr.author.login))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
            .on_click(cx.listener(move |this, _, _window, cx| {
                this.select_pull_request(pr.clone(), cx)
            }))
    }

    fn render_create(&self, cx: &Context<Self>) -> impl IntoElement {
        let is_edit = self.edit_target.is_some();
        let header = h_flex()
            .gap_1()
            .p_2()
            .child(
                IconButton::new("create-back", IconName::ArrowLeft)
                    .tooltip(Tooltip::text("Back"))
                    .on_click(cx.listener(|this, _, _window, cx| {
                        if this.edit_target.take().is_some() {
                            this.active_view = ActiveView::Detail;
                            cx.notify();
                        } else {
                            this.back_to_list(cx);
                        }
                    })),
            )
            .child(
                Label::new(if is_edit {
                    "Edit pull request"
                } else {
                    "New pull request"
                })
                .size(LabelSize::Small),
            );

        v_flex()
            .size_full()
            .child(header)
            .child(
                v_flex()
                    .gap_2()
                    .p_2()
                    .when_some(self.create_error.clone(), |this, error| {
                        this.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
                    })
                    .child(Label::new("Title").size(LabelSize::XSmall).color(Color::Muted))
                    .child(self.create_title.clone())
                    .when(!is_edit, |this| {
                        this.child(
                            h_flex()
                                .gap_2()
                                .child(
                                    v_flex()
                                        .gap_0p5()
                                        .child(
                                            Label::new("Base")
                                                .size(LabelSize::XSmall)
                                                .color(Color::Muted),
                                        )
                                        .child(self.create_base.clone()),
                                )
                                .child(
                                    v_flex()
                                        .gap_0p5()
                                        .child(
                                            Label::new("Head")
                                                .size(LabelSize::XSmall)
                                                .color(Color::Muted),
                                        )
                                        .child(self.create_head.clone()),
                                ),
                        )
                    })
                    .child(Label::new("Description").size(LabelSize::XSmall).color(Color::Muted))
                    .child(self.create_body.clone())
                    .when(!is_edit, |this| {
                        this.child(
                            Checkbox::new("create-draft", to_toggle(self.create_draft))
                                .label("Create as draft")
                                .on_click(cx.listener(|this, state: &ToggleState, _window, cx| {
                                    this.create_draft = *state == ToggleState::Selected;
                                    cx.notify();
                                })),
                        )
                    })
                    .child(
                        Button::new(
                            "create-submit",
                            if is_edit { "Save" } else { "Create pull request" },
                        )
                        .style(ButtonStyle::Filled)
                        .on_click(cx.listener(|this, _, window, cx| this.submit_create(window, cx))),
                    ),
            )
    }

    fn render_detail(&self, cx: &Context<Self>) -> impl IntoElement {
        let header = h_flex()
            .gap_1()
            .p_2()
            .justify_between()
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        IconButton::new("back", IconName::ArrowLeft)
                            .tooltip(Tooltip::text("Back to list"))
                            .on_click(cx.listener(|this, _, _window, cx| this.back_to_list(cx))),
                    )
                    .child(Label::new("Pull Request").size(LabelSize::Small)),
            )
            .child(
                IconButton::new("refresh-detail", IconName::RotateCw)
                    .tooltip(Tooltip::text("Refresh"))
                    .on_click(cx.listener(|this, _, _window, cx| this.refresh_detail(cx))),
            );

        let body = if self.detail_loading {
            v_flex()
                .p_4()
                .child(Label::new("Loading…").color(Color::Muted))
                .into_any_element()
        } else if let Some(error) = &self.detail_error {
            v_flex()
                .p_4()
                .child(Label::new(error.clone()).color(Color::Error))
                .into_any_element()
        } else if let Some(loaded) = &self.selected {
            v_flex()
                .id("pr-detail")
                .overflow_y_scroll()
                .gap_2()
                .p_2()
                .child(self.render_overview(loaded, cx))
                .child(self.render_files(loaded, cx))
                .child(self.render_threads(loaded, cx))
                .child(self.render_timeline(loaded))
                .into_any_element()
        } else {
            v_flex().into_any_element()
        };

        v_flex().size_full().child(header).child(body)
    }

    fn render_overview(
        &self,
        loaded: &LoadedPullRequest,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let detail = &loaded.detail;
        let info = &detail.info;
        let is_checked_out = self.pr_is_checked_out(cx);

        let state_label = match info.state {
            PullRequestState::Open if info.is_draft => ("Draft", Color::Muted),
            PullRequestState::Open => ("Open", Color::Success),
            PullRequestState::Merged => ("Merged", Color::Accent),
            PullRequestState::Closed => ("Closed", Color::Error),
        };

        let title_row = h_flex()
            .gap_2()
            .child(Label::new(state_label.0).size(LabelSize::Small).color(state_label.1))
            .child(Label::new(format!("#{}", info.id.number)).size(LabelSize::Small).color(Color::Muted))
            .child(Label::new(info.title.clone()));

        let labels = (!detail.labels.is_empty()).then(|| {
            h_flex().gap_1().flex_wrap().children(detail.labels.iter().map(|label| {
                Label::new(label.clone()).size(LabelSize::XSmall).color(Color::Accent)
            }))
        });

        let reviewers = (!detail.reviewers.is_empty()).then(|| {
            h_flex().gap_2().flex_wrap().children(detail.reviewers.iter().map(|reviewer| {
                let (icon, color) = match reviewer.verdict {
                    Some(ReviewVerdict::Approved) => (IconName::Check, Color::Success),
                    Some(ReviewVerdict::ChangesRequested) => (IconName::XCircle, Color::Error),
                    Some(ReviewVerdict::Commented) => (IconName::QueueMessage, Color::Muted),
                    _ => (IconName::CircleHelp, Color::Muted),
                };
                h_flex()
                    .gap_0p5()
                    .child(Icon::new(icon).size(IconSize::XSmall).color(color))
                    .child(
                        Label::new(reviewer.actor.login.clone())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
            }))
        });

        let checks = detail.check_rollup.map(|rollup| {
            let (icon, color, text) = check_presentation(rollup);
            h_flex()
                .gap_1()
                .child(Icon::new(icon).size(IconSize::XSmall).color(color))
                .child(
                    Label::new(format!("Checks: {} ({})", text, detail.checks.len()))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
        });

        let mergeable = detail.mergeable.map(|mergeable| {
            Label::new(if mergeable {
                "Mergeable"
            } else {
                "Conflicts"
            })
            .size(LabelSize::XSmall)
            .color(if mergeable { Color::Success } else { Color::Warning })
        });

        let meta = (!detail.assignees.is_empty() || detail.milestone.is_some()).then(|| {
            h_flex()
                .gap_2()
                .flex_wrap()
                .when(!detail.assignees.is_empty(), |this| {
                    this.child(
                        Label::new(format!(
                            "Assignees: {}",
                            detail
                                .assignees
                                .iter()
                                .map(|a| a.login.to_string())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                    )
                })
                .when_some(detail.milestone.clone(), |this, milestone| {
                    this.child(
                        Label::new(format!("Milestone: {milestone}"))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                })
        });

        let body = (!detail.body.is_empty()).then(|| {
            v_flex().gap_0p5().p_1().child(
                Label::new(detail.body.clone())
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
        });

        v_flex()
            .gap_1()
            .child(title_row)
            .child(
                Label::new(format!("{} → {}", info.head_ref, info.base_ref))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .when_some(labels, |this, labels| this.child(labels))
            .when_some(meta, |this, meta| this.child(meta))
            .when_some(reviewers, |this, reviewers| this.child(reviewers))
            .when_some(checks, |this, checks| this.child(checks))
            .when_some(mergeable, |this, mergeable| this.child(mergeable))
            .when_some(body, |this, body| this.child(body))
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        ui::Button::new("approve", "Approve")
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.submit_review(ReviewEvent::Approve, cx)
                            })),
                    )
                    .child(
                        ui::Button::new("request-changes", "Request changes")
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.submit_review(ReviewEvent::RequestChanges, cx)
                            })),
                    )
                    .child(
                        ui::Button::new("comment", "Comment")
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.submit_review(ReviewEvent::Comment, cx)
                            })),
                    )
                    .when(!is_checked_out, |this| {
                        this.child(
                            Button::new("checkout", "Checkout")
                                .label_size(LabelSize::Small)
                                .on_click(
                                    cx.listener(|this, _, _window, cx| this.checkout_pr(cx)),
                                ),
                        )
                    })
                    .when(
                        detail.viewer_can_merge
                            && info.state == PullRequestState::Open
                            && !info.is_draft,
                        |this| {
                            this.child(
                                Button::new("merge-merge", "Merge")
                                    .label_size(LabelSize::Small)
                                    .style(ButtonStyle::Filled)
                                    .on_click(cx.listener(|this, _, _window, cx| {
                                        this.merge_selected(MergeMethod::Merge, cx)
                                    })),
                            )
                            .child(
                                Button::new("merge-squash", "Squash")
                                    .label_size(LabelSize::Small)
                                    .style(ButtonStyle::Filled)
                                    .on_click(cx.listener(|this, _, _window, cx| {
                                        this.merge_selected(MergeMethod::Squash, cx)
                                    })),
                            )
                            .child(
                                Button::new("merge-rebase", "Rebase")
                                    .label_size(LabelSize::Small)
                                    .style(ButtonStyle::Filled)
                                    .on_click(cx.listener(|this, _, _window, cx| {
                                        this.merge_selected(MergeMethod::Rebase, cx)
                                    })),
                            )
                        },
                    ),
            )
            .when(detail.viewer_can_update, |this| {
                let is_open = info.state == PullRequestState::Open;
                let is_draft = info.is_draft;
                this.child(
                    h_flex()
                        .gap_1()
                        .child(
                            Button::new("edit-pr", "Edit")
                                .label_size(LabelSize::Small)
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.start_edit_pr(window, cx)),
                                ),
                        )
                        .child(
                            Button::new(
                                "close-reopen",
                                if is_open { "Close" } else { "Reopen" },
                            )
                            .label_size(LabelSize::Small)
                            .on_click(
                                cx.listener(|this, _, _window, cx| this.close_or_reopen(cx)),
                            ),
                        )
                        .when(is_open, |this| {
                            this.child(
                                Button::new(
                                    "draft-toggle",
                                    if is_draft { "Ready for review" } else { "Convert to draft" },
                                )
                                .label_size(LabelSize::Small)
                                .on_click(
                                    cx.listener(|this, _, _window, cx| this.toggle_draft(cx)),
                                ),
                            )
                        }),
                )
            })
    }

    fn render_files(
        &self,
        loaded: &LoadedPullRequest,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let layout = self.file_layout;
        let hide_viewed = self.hide_viewed;
        let visible: Vec<PullRequestFile> = loaded
            .files
            .iter()
            .filter(|file| !hide_viewed || file.viewed_state != ViewedState::Viewed)
            .cloned()
            .collect();
        let header = h_flex()
            .justify_between()
            .child(Label::new(format!("{} files", loaded.files.len())).size(LabelSize::Small))
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        IconButton::new("hide-viewed", IconName::Eye)
                            .toggle_state(hide_viewed)
                            .tooltip(Tooltip::text("Hide viewed files"))
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.hide_viewed = !this.hide_viewed;
                                cx.notify();
                            })),
                    )
                    .child(
                        IconButton::new("layout-tree", IconName::ListTree)
                            .toggle_state(layout == FileLayout::Tree)
                            .tooltip(Tooltip::text("Tree view"))
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.file_layout = FileLayout::Tree;
                                cx.notify();
                            })),
                    )
                    .child(
                        IconButton::new("layout-flat", IconName::ListCollapse)
                            .toggle_state(layout == FileLayout::Flat)
                            .tooltip(Tooltip::text("Flat view"))
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.file_layout = FileLayout::Flat;
                                cx.notify();
                            })),
                    ),
            );

        let rows = match layout {
            FileLayout::Flat => v_flex()
                .children(
                    visible
                        .iter()
                        .map(|file| self.render_file_row(file, file.path.to_string(), 0, cx)),
                )
                .into_any_element(),
            FileLayout::Tree => {
                let tree_rows = build_tree_rows(&visible, &self.collapsed_dirs);
                v_flex()
                    .children(tree_rows.into_iter().map(|row| {
                        if row.is_dir {
                            self.render_dir_row(row.depth, row.name, row.dir_path, cx)
                                .into_any_element()
                        } else if let Some(file) = row.file_index.and_then(|i| visible.get(i)) {
                            self.render_file_row(file, row.name, row.depth, cx)
                                .into_any_element()
                        } else {
                            div().into_any_element()
                        }
                    }))
                    .into_any_element()
            }
        };

        v_flex().gap_0p5().child(header).child(rows)
    }

    fn render_dir_row(
        &self,
        depth: usize,
        name: String,
        dir_path: String,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let collapsed = self.collapsed_dirs.contains(&dir_path);
        let icon = if collapsed {
            IconName::ChevronRight
        } else {
            IconName::ChevronDown
        };
        h_flex()
            .id(SharedString::from(format!("dir:{dir_path}")))
            .w_full()
            .py_0p5()
            .pl(px(8.0 + depth as f32 * 12.0))
            .gap_1()
            .cursor_pointer()
            .hover(|style| style.bg(cx.theme().colors().element_hover))
            .child(Icon::new(icon).size(IconSize::XSmall).color(Color::Muted))
            .child(Label::new(name).size(LabelSize::Small).color(Color::Muted))
            .on_click(cx.listener(move |this, _, _window, cx| {
                this.toggle_dir(dir_path.clone(), cx)
            }))
    }

    fn render_file_row(
        &self,
        file: &PullRequestFile,
        display: String,
        depth: usize,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let viewed = file.viewed_state == ViewedState::Viewed;
        let path = file.path.clone();
        let open_path = file.path.clone();
        let github_path = file.path.clone();
        let (badge, badge_color) = file_status_badge(&file.status);
        h_flex()
            .w_full()
            .py_0p5()
            .pl(px(8.0 + depth as f32 * 12.0))
            .pr_2()
            .gap_1()
            .child(
                Checkbox::new(
                    SharedString::from(format!("viewed:{path}")),
                    to_toggle(viewed),
                )
                .on_click(cx.listener(move |this, _, _window, cx| {
                    this.toggle_file_viewed(path.clone(), cx)
                })),
            )
            .child(Label::new(badge).size(LabelSize::XSmall).color(badge_color))
            .child(
                div()
                    .id(SharedString::from(format!("open:{open_path}")))
                    .flex_1()
                    .cursor_pointer()
                    .child(Label::new(display).size(LabelSize::Small).color(if viewed {
                        Color::Muted
                    } else {
                        Color::Default
                    }))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_file_diff(open_path.clone(), window, cx)
                    })),
            )
            .child(
                Label::new(format!("+{} -{}", file.additions, file.deletions))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .child(
                IconButton::new(
                    SharedString::from(format!("gh:{github_path}")),
                    IconName::Github,
                )
                .icon_size(IconSize::XSmall)
                .tooltip(Tooltip::text("Open on GitHub"))
                .on_click(cx.listener(move |this, _, _window, cx| {
                    this.open_file_on_github(github_path.clone(), cx)
                })),
            )
    }

    fn open_file_on_github(&self, path: SharedString, cx: &mut Context<Self>) {
        let Some(loaded) = self.selected.as_ref() else {
            return;
        };
        let info = &loaded.detail.info;
        let url = format!(
            "https://github.com/{}/{}/blob/{}/{}",
            info.id.owner, info.id.repo, info.head_sha, path
        );
        cx.open_url(&url);
    }

    fn render_threads(&self, loaded: &LoadedPullRequest, cx: &Context<Self>) -> impl IntoElement {
        let new_thread = if let Some(ComposerTarget::NewThread { path, line }) = &self.composer_target
        {
            Some(
                v_flex()
                    .gap_0p5()
                    .p_1()
                    .child(
                        Label::new(format!("New comment on {path}:{line}"))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(self.render_composer(cx)),
            )
        } else {
            None
        };

        v_flex()
            .gap_1()
            .when_some(new_thread, |this, new_thread| this.child(new_thread))
            .when(!loaded.threads.is_empty(), |this| {
                this.child(Label::new("Review comments").size(LabelSize::Small))
            })
            .children(
                loaded
                    .threads
                    .iter()
                    .map(|thread| self.render_thread(thread, cx)),
            )
    }

    fn render_thread(&self, thread: &ReviewThread, cx: &Context<Self>) -> impl IntoElement {
        let thread_id = thread.id.clone();
        // Suggestions parsed from this thread's comments (right-side only — they
        // apply to the working tree).
        let suggestion_texts: Vec<String> = if thread.diff_side == DiffSide::Right {
            thread
                .comments
                .iter()
                .flat_map(|comment| parse_suggestions(&comment.body))
                .collect()
        } else {
            Vec::new()
        };
        let suggestion_path = thread.path.clone();
        let end_line = thread.line.unwrap_or(1);
        let start_line = thread.start_line.unwrap_or(end_line);
        let suggestions = (!suggestion_texts.is_empty()).then(|| {
            let suggestion_thread_id = thread_id.clone();
            let suggestion_path = suggestion_path.clone();
            v_flex()
                .gap_1()
                .children(suggestion_texts.iter().cloned().enumerate().map(
                    move |(index, suggestion)| {
                        let path = suggestion_path.clone();
                        let text = suggestion.clone();
                        v_flex()
                            .gap_0p5()
                            .p_1()
                            .child(
                                Label::new(suggestion)
                                    .size(LabelSize::Small)
                                    .color(Color::Created),
                            )
                            .child(
                                Button::new(
                                    SharedString::from(format!(
                                        "apply:{suggestion_thread_id}:{index}"
                                    )),
                                    "Apply suggestion",
                                )
                                .label_size(LabelSize::XSmall)
                                .style(ButtonStyle::Filled)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.apply_suggestion(
                                        path.clone(),
                                        start_line,
                                        end_line,
                                        text.clone(),
                                        window,
                                        cx,
                                    )
                                })),
                            )
                    },
                ))
        });
        let location = format!(
            "{}{}",
            thread.path,
            thread.line.map(|line| format!(":{line}")).unwrap_or_default()
        );
        let resolve_label = if thread.is_resolved {
            "Unresolve"
        } else {
            "Resolve"
        };
        let resolved = thread.is_resolved;
        let can_toggle = if resolved {
            thread.viewer_can_unresolve
        } else {
            thread.viewer_can_resolve
        };
        let is_replying =
            matches!(&self.composer_target, Some(ComposerTarget::Reply(id)) if id == &thread.id);

        let header = h_flex()
            .gap_2()
            .justify_between()
            .child(
                h_flex()
                    .gap_1()
                    .child(Label::new(location).size(LabelSize::XSmall).color(Color::Muted))
                    .when(thread.is_resolved, |this| {
                        this.child(
                            Label::new("resolved")
                                .size(LabelSize::XSmall)
                                .color(Color::Success),
                        )
                    })
                    .when(thread.is_outdated, |this| {
                        this.child(
                            Label::new("outdated")
                                .size(LabelSize::XSmall)
                                .color(Color::Warning),
                        )
                    }),
            )
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Button::new(SharedString::from(format!("reply:{thread_id}")), "Reply")
                            .label_size(LabelSize::XSmall)
                            .on_click(cx.listener({
                                let thread_id = thread_id.clone();
                                move |this, _, window, cx| {
                                    this.start_reply(thread_id.clone(), window, cx)
                                }
                            })),
                    )
                    .when(can_toggle, |this| {
                        this.child(
                            Button::new(
                                SharedString::from(format!("resolve:{thread_id}")),
                                resolve_label,
                            )
                            .label_size(LabelSize::XSmall)
                            .on_click(cx.listener({
                                let thread_id = thread_id.clone();
                                move |this, _, _window, cx| {
                                    this.set_thread_resolved(thread_id.clone(), !resolved, cx)
                                }
                            })),
                        )
                    }),
            );

        v_flex()
            .gap_0p5()
            .p_1()
            .child(header)
            .children(thread.comments.iter().map(|comment| self.render_comment(comment, cx)))
            .when_some(suggestions, |this, suggestions| this.child(suggestions))
            .when(is_replying, |this| this.child(self.render_composer(cx)))
    }

    fn render_comment(&self, comment: &ReviewComment, cx: &Context<Self>) -> impl IntoElement {
        let is_editing =
            matches!(&self.composer_target, Some(ComposerTarget::Edit(id)) if id == &comment.id);
        let id = comment.id.clone();
        let body = comment.body.clone();
        let database_id = comment.database_id;
        let reactions = h_flex()
            .gap_1()
            // Existing reaction groups toggle on click.
            .children(
                comment
                    .reactions
                    .iter()
                    .filter(|group| group.count > 0)
                    .map(|group| {
                        let reacted = group.viewer_has_reacted;
                        let subject = comment.id.clone();
                        let content = group.content.clone();
                        Button::new(
                            SharedString::from(format!("react:{}:{}", comment.id, group.content)),
                            format!("{} {}", reaction_emoji(&group.content), group.count),
                        )
                        .label_size(LabelSize::XSmall)
                        .style(if reacted {
                            ButtonStyle::Filled
                        } else {
                            ButtonStyle::Subtle
                        })
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            this.toggle_reaction(subject.clone(), content.clone(), reacted, cx)
                        }))
                    }),
            )
            // Quick "add 👍".
            .child({
                let subject = comment.id.clone();
                let already = comment
                    .reactions
                    .iter()
                    .any(|g| g.content.as_ref() == "THUMBS_UP" && g.viewer_has_reacted);
                IconButton::new(
                    SharedString::from(format!("react-add:{}", comment.id)),
                    IconName::QueueMessage,
                )
                .tooltip(Tooltip::text("React 👍"))
                .on_click(cx.listener(move |this, _, _window, cx| {
                    this.toggle_reaction(subject.clone(), "THUMBS_UP".into(), already, cx)
                }))
            });

        v_flex()
            .gap_0p5()
            .py_0p5()
            .child(
                h_flex()
                    .justify_between()
                    .child(Label::new(comment.author.login.clone()).size(LabelSize::XSmall))
                    .child(
                        h_flex()
                            .gap_1()
                            .when(comment.viewer_can_update, |this| {
                                this.child(
                                    Button::new(
                                        SharedString::from(format!("edit:{id}")),
                                        "Edit",
                                    )
                                    .label_size(LabelSize::XSmall)
                                    .on_click(cx.listener({
                                        let id = id.clone();
                                        let body = body.clone();
                                        move |this, _, window, cx| {
                                            this.start_edit(id.clone(), body.clone(), window, cx)
                                        }
                                    })),
                                )
                            })
                            .when(comment.viewer_can_delete && database_id.is_some(), |this| {
                                let database_id = database_id.unwrap_or_default();
                                this.child(
                                    Button::new(
                                        SharedString::from(format!("delete:{id}")),
                                        "Delete",
                                    )
                                    .label_size(LabelSize::XSmall)
                                    .on_click(cx.listener(move |this, _, _window, cx| {
                                        this.delete_comment(database_id, cx)
                                    })),
                                )
                            }),
                    ),
            )
            .child(Label::new(comment.body.clone()).size(LabelSize::Small))
            .child(reactions)
            .when(is_editing, |this| this.child(self.render_composer(cx)))
    }

    /// The shared comment composer (editor + submit/cancel), used for replies
    /// and new threads.
    fn render_composer(&self, cx: &Context<Self>) -> impl IntoElement {
        v_flex()
            .gap_1()
            .p_1()
            .child(self.comment_editor.clone())
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Button::new("submit-comment", "Comment")
                            .label_size(LabelSize::XSmall)
                            .style(ButtonStyle::Filled)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.submit_comment(window, cx)),
                            ),
                    )
                    .child(
                        Button::new("make-suggestion", "Suggest")
                            .label_size(LabelSize::XSmall)
                            .tooltip(Tooltip::text("Wrap the comment in a suggestion block"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.wrap_suggestion(window, cx)
                            })),
                    )
                    .child(
                        Button::new("cancel-comment", "Cancel")
                            .label_size(LabelSize::XSmall)
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.composer_target = None;
                                cx.notify();
                            })),
                    ),
            )
    }

    /// Wrap the composer's current text in a ```suggestion fence.
    fn wrap_suggestion(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let body = self.comment_editor.read(cx).text(cx);
        let wrapped = format!("```suggestion\n{}\n```", body.trim_end());
        self.comment_editor
            .update(cx, |editor, cx| editor.set_text(wrapped, window, cx));
        cx.notify();
    }
}

/// Extract the contents of ```suggestion fenced blocks from a comment body.
fn parse_suggestions(body: &str) -> Vec<String> {
    let mut suggestions = Vec::new();
    let mut lines = body.lines();
    while let Some(line) = lines.next() {
        if line.trim_start().starts_with("```suggestion") {
            let mut block = Vec::new();
            for line in lines.by_ref() {
                if line.trim_start().starts_with("```") {
                    break;
                }
                block.push(line);
            }
            suggestions.push(block.join("\n"));
        }
    }
    suggestions
}

/// Map a GitHub reaction content enum to an emoji.
fn reaction_emoji(content: &str) -> &'static str {
    match content {
        "THUMBS_UP" => "👍",
        "THUMBS_DOWN" => "👎",
        "LAUGH" => "😄",
        "HOORAY" => "🎉",
        "CONFUSED" => "😕",
        "HEART" => "❤️",
        "ROCKET" => "🚀",
        "EYES" => "👀",
        _ => "•",
    }
}

impl PullRequestPanel {
    fn render_timeline(&self, loaded: &LoadedPullRequest) -> impl IntoElement {
        v_flex()
            .gap_0p5()
            .when(!loaded.timeline.is_empty(), |this| {
                this.child(Label::new("Activity").size(LabelSize::Small))
            })
            .children(loaded.timeline.iter().map(|item| {
                let (icon, text) = timeline_presentation(item);
                h_flex()
                    .gap_1()
                    .child(Icon::new(icon).size(IconSize::XSmall).color(Color::Muted))
                    .child(Label::new(text).size(LabelSize::XSmall).color(Color::Muted))
            }))
    }
}

/// Icon + one-line summary for a timeline entry.
fn timeline_presentation(item: &TimelineItem) -> (IconName, String) {
    match item {
        TimelineItem::Commit { message, author, .. } => (
            IconName::GitBranch,
            format!(
                "{} committed: {}",
                author.as_ref().map(|a| a.login.as_ref()).unwrap_or("someone"),
                message
            ),
        ),
        TimelineItem::Review { author, verdict, .. } => {
            let verb = match verdict {
                ReviewVerdict::Approved => "approved",
                ReviewVerdict::ChangesRequested => "requested changes",
                ReviewVerdict::Dismissed => "dismissed a review",
                _ => "reviewed",
            };
            (IconName::Check, format!("{} {}", author.login, verb))
        }
        TimelineItem::Comment { author, .. } => {
            (IconName::QueueMessage, format!("{} commented", author.login))
        }
        TimelineItem::Merged { actor, .. } => (
            IconName::GitBranch,
            format!("{} merged", actor.as_ref().map(|a| a.login.as_ref()).unwrap_or("someone")),
        ),
        TimelineItem::Closed { actor, .. } => (
            IconName::XCircle,
            format!("{} closed", actor.as_ref().map(|a| a.login.as_ref()).unwrap_or("someone")),
        ),
        TimelineItem::Reopened { actor, .. } => (
            IconName::CircleHelp,
            format!("{} reopened", actor.as_ref().map(|a| a.login.as_ref()).unwrap_or("someone")),
        ),
        TimelineItem::HeadRefForcePushed { actor, .. } => (
            IconName::GitBranch,
            format!(
                "{} force-pushed",
                actor.as_ref().map(|a| a.login.as_ref()).unwrap_or("someone")
            ),
        ),
        TimelineItem::Other { kind, .. } => (IconName::CircleHelp, kind.to_string()),
    }
}

fn to_toggle(checked: bool) -> ToggleState {
    if checked {
        ToggleState::Selected
    } else {
        ToggleState::Unselected
    }
}

/// Build a base↔buffer diff editor and add it to the active pane. The buffer
/// is the modified (right) side; `base_text` is the read-only original (left).
#[allow(clippy::too_many_arguments)]
fn open_diff_item(
    buffer: Entity<Buffer>,
    base_text: &str,
    read_only: bool,
    threads: Vec<ReviewThread>,
    project: &Entity<Project>,
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let snapshot = buffer.read(cx).snapshot();
    let language = buffer.read(cx).language().cloned();
    let diff = cx.new(|cx| BufferDiff::new(&snapshot.text, cx));
    diff.update(cx, |diff, cx| {
        // Diff computes asynchronously; the editor re-renders when it lands.
        let _ = diff.set_base_text(
            Some(std::sync::Arc::from(base_text)),
            language,
            snapshot.text.clone(),
            cx,
        );
    });
    let multibuffer = cx.new(|cx| {
        let mut multibuffer = MultiBuffer::singleton(buffer, cx);
        multibuffer.add_diff(diff, cx);
        multibuffer
    });
    let editor = cx.new(|cx| {
        let mut editor = Editor::for_multibuffer(multibuffer, Some(project.clone()), window, cx);
        if read_only {
            editor.set_read_only(true);
        }
        editor
    });
    inject_thread_blocks(&editor, threads, cx);
    workspace.add_item_to_active_pane(Box::new(editor), None, true, window, cx);
}

/// Insert a read-only block under each thread's mapped line, showing its
/// comments inline in the diff (the panel's thread list keeps the interactive
/// reply/resolve actions).
fn inject_thread_blocks(
    editor: &Entity<Editor>,
    threads: Vec<ReviewThread>,
    cx: &mut Context<Workspace>,
) {
    editor.update(cx, |editor, cx| {
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let max_row = snapshot.max_point().row;
        let mut blocks: Vec<BlockProperties<Anchor>> = Vec::new();
        for thread in threads {
            let Some(thread_anchor) = thread_anchor(&thread) else {
                continue;
            };
            let row = thread_anchor.end_row;
            if row > max_row {
                continue;
            }
            let anchor = snapshot.anchor_before(Point::new(row, 0));
            let count = thread.comments.len().max(1) as u32;
            blocks.push(BlockProperties {
                placement: BlockPlacement::Below(anchor),
                height: Some((count * 2 + 1).min(24)),
                style: BlockStyle::Flex,
                render: Arc::new(move |cx| render_inline_thread(&thread, cx)),
                priority: 0,
            });
        }
        if !blocks.is_empty() {
            editor.insert_blocks(blocks, None, cx);
        }
    });
}

fn render_inline_thread(thread: &ReviewThread, cx: &mut BlockContext) -> gpui::AnyElement {
    let colors = cx.theme().colors().clone();
    v_flex()
        .w_full()
        .gap_0p5()
        .pl(cx.anchor_x)
        .pr_2()
        .py_1()
        .border_t_1()
        .border_color(colors.border)
        .bg(colors.editor_background)
        .when(thread.is_resolved, |this| {
            this.child(
                Label::new("resolved")
                    .size(LabelSize::XSmall)
                    .color(Color::Success),
            )
        })
        .children(thread.comments.iter().map(|comment| {
            v_flex()
                .child(Label::new(comment.author.login.clone()).size(LabelSize::XSmall))
                .child(Label::new(comment.body.clone()).size(LabelSize::Small))
        }))
        .into_any_element()
}

/// One-letter status badge + color for a changed file.
fn file_status_badge(status: &FileChangeStatus) -> (&'static str, Color) {
    match status {
        FileChangeStatus::Added => ("A", Color::Success),
        FileChangeStatus::Modified => ("M", Color::Warning),
        FileChangeStatus::Deleted => ("D", Color::Error),
        FileChangeStatus::Renamed { .. } => ("R", Color::Accent),
        FileChangeStatus::Copied { .. } => ("C", Color::Accent),
    }
}

/// Icon/color/label for an aggregate CI status.
fn check_presentation(status: CheckStatus) -> (IconName, Color, &'static str) {
    match status {
        CheckStatus::Success => (IconName::Check, Color::Success, "passing"),
        CheckStatus::Failure | CheckStatus::Error => (IconName::XCircle, Color::Error, "failing"),
        CheckStatus::Cancelled => (IconName::XCircle, Color::Muted, "cancelled"),
        CheckStatus::Pending => (IconName::Warning, Color::Warning, "pending"),
        CheckStatus::Neutral | CheckStatus::Skipped => (IconName::CircleHelp, Color::Muted, "neutral"),
    }
}

impl Render for PullRequestPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match self.active_view {
            ActiveView::List => self.render_list(cx).into_any_element(),
            ActiveView::Detail => self.render_detail(cx).into_any_element(),
            ActiveView::Create => self.render_create(cx).into_any_element(),
        };
        v_flex()
            .key_context("PullRequestPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(content)
    }
}

impl Focusable for PullRequestPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for PullRequestPanel {}

impl Panel for PullRequestPanel {
    fn persistent_name() -> &'static str {
        "PullRequestPanel"
    }

    fn panel_key() -> &'static str {
        PULL_REQUEST_PANEL_KEY
    }

    fn position(&self, _window: &Window, cx: &App) -> DockPosition {
        ReviewPanelSettings::get_global(cx).dock
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(
        &mut self,
        position: DockPosition,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        settings::update_settings_file(self.fs.clone(), cx, move |settings, _| {
            settings.review_panel.get_or_insert_default().dock = Some(position.into())
        });
    }

    fn default_size(&self, _window: &Window, cx: &App) -> Pixels {
        self.width
            .unwrap_or_else(|| ReviewPanelSettings::get_global(cx).default_width)
    }

    fn icon(&self, _window: &Window, cx: &App) -> Option<IconName> {
        Some(IconName::PullRequest).filter(|_| ReviewPanelSettings::get_global(cx).button)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Pull Requests")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        10
    }
}

/// Parse a GitHub `owner/repo` from a git remote URL (SSH or HTTPS).
fn parse_github_remote(url: &str) -> anyhow::Result<(String, String)> {
    let rest = url
        .strip_prefix("git@github.com:")
        .or_else(|| url.strip_prefix("https://github.com/"))
        .or_else(|| url.strip_prefix("http://github.com/"));
    if let Some(rest) = rest {
        let rest = rest.trim_end_matches(".git");
        if let Some((owner, repo)) = rest.split_once('/') {
            return Ok((owner.to_string(), repo.to_string()));
        }
    }
    anyhow::bail!("Could not parse GitHub owner/repo from remote URL: {url}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ssh_and_https_remotes() {
        assert_eq!(
            parse_github_remote("git@github.com:zed-industries/zed.git").unwrap(),
            ("zed-industries".to_string(), "zed".to_string())
        );
        assert_eq!(
            parse_github_remote("https://github.com/zed-industries/zed").unwrap(),
            ("zed-industries".to_string(), "zed".to_string())
        );
        assert!(parse_github_remote("git@gitlab.com:foo/bar.git").is_err());
    }

    #[test]
    fn parses_suggestion_blocks() {
        let body = "Looks good but:\n```suggestion\nlet x = 1;\nlet y = 2;\n```\nthanks";
        let suggestions = parse_suggestions(body);
        assert_eq!(suggestions, vec!["let x = 1;\nlet y = 2;".to_string()]);

        assert!(parse_suggestions("no suggestion here").is_empty());

        let multi = "```suggestion\na\n```\nand\n```suggestion\nb\n```";
        assert_eq!(parse_suggestions(multi), vec!["a".to_string(), "b".to_string()]);
    }
}
