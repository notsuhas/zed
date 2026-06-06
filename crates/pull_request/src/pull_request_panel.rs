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
use crate::file_tree::build_tree_rows;
use editor::Editor;
use http_client::HttpClient;
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
use zed_actions::pull_request::ToggleFocus;

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
    collapsed_dirs: HashSet<String>,
    comment_editor: Entity<Editor>,
    reply_target: Option<SharedString>,
    _refresh_task: Option<Task<()>>,
    _detail_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

pub fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
        workspace.toggle_panel_focus::<PullRequestPanel>(window, cx);
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
            collapsed_dirs: HashSet::new(),
            comment_editor,
            reply_target: None,
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
        let query = self.filter.query().to_string();
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
        self.reply_target = Some(thread_id);
        let handle = self.comment_editor.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
        cx.notify();
    }

    fn submit_reply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.reply_target.clone() else {
            return;
        };
        let body = self.comment_editor.read(cx).text(cx);
        if body.trim().is_empty() {
            return;
        }
        let (Some(provider), Some(loaded)) = (self.provider.clone(), self.selected.as_ref()) else {
            return;
        };
        let new_comment = NewComment {
            pull_request: loaded.detail.info.id.clone(),
            body: body.into(),
            target: CommentTarget::Reply {
                in_reply_to: target,
            },
            review_id: None,
            commit_sha: loaded.detail.info.head_sha.clone(),
        };
        self.comment_editor
            .update(cx, |editor, cx| editor.clear(window, cx));
        self.reply_target = None;
        cx.notify();

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
        let filters = h_flex().gap_1().p_2().children(ListFilter::ALL.map(|filter| {
            let selected = filter == self.filter;
            Button::new(SharedString::from(filter.label()), filter.label())
                .label_size(LabelSize::Small)
                .style(if selected {
                    ButtonStyle::Filled
                } else {
                    ButtonStyle::Subtle
                })
                .on_click(cx.listener(move |this, _, _window, cx| this.set_filter(filter, cx)))
        }));

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
            .child(Icon::new(IconName::GitBranch).size(IconSize::Small).color(state_color))
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

    fn render_detail(&self, cx: &Context<Self>) -> impl IntoElement {
        let header = h_flex()
            .gap_1()
            .p_2()
            .child(
                IconButton::new("back", IconName::ArrowLeft)
                    .tooltip(Tooltip::text("Back to list"))
                    .on_click(cx.listener(|this, _, _window, cx| this.back_to_list(cx))),
            )
            .child(Label::new("Pull Request").size(LabelSize::Small));

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

        v_flex()
            .gap_1()
            .child(title_row)
            .child(
                Label::new(format!("{} → {}", info.head_ref, info.base_ref))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .when_some(labels, |this, labels| this.child(labels))
            .when_some(reviewers, |this, reviewers| this.child(reviewers))
            .when_some(checks, |this, checks| this.child(checks))
            .when_some(mergeable, |this, mergeable| this.child(mergeable))
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
                    .when(
                        detail.viewer_can_merge
                            && info.state == PullRequestState::Open
                            && !info.is_draft,
                        |this| {
                            this.child(
                                ui::Button::new("merge", "Squash & merge")
                                    .label_size(LabelSize::Small)
                                    .style(ButtonStyle::Filled)
                                    .on_click(cx.listener(|this, _, _window, cx| {
                                        this.merge_selected(MergeMethod::Squash, cx)
                                    })),
                            )
                        },
                    ),
            )
    }

    fn render_files(
        &self,
        loaded: &LoadedPullRequest,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let layout = self.file_layout;
        let header = h_flex()
            .justify_between()
            .child(Label::new(format!("{} files", loaded.files.len())).size(LabelSize::Small))
            .child(
                h_flex()
                    .gap_1()
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
                    loaded
                        .files
                        .iter()
                        .map(|file| self.render_file_row(file, file.path.to_string(), 0, cx)),
                )
                .into_any_element(),
            FileLayout::Tree => {
                let tree_rows = build_tree_rows(&loaded.files, &self.collapsed_dirs);
                v_flex()
                    .children(tree_rows.into_iter().map(|row| {
                        if row.is_dir {
                            self.render_dir_row(row.depth, row.name, row.dir_path, cx)
                                .into_any_element()
                        } else if let Some(file) = row.file_index.and_then(|i| loaded.files.get(i)) {
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
        h_flex()
            .w_full()
            .py_0p5()
            .pl(px(8.0 + depth as f32 * 12.0))
            .pr_2()
            .gap_2()
            .child(
                Checkbox::new(
                    SharedString::from(format!("viewed:{path}")),
                    to_toggle(viewed),
                )
                .on_click(cx.listener(move |this, _, _window, cx| {
                    this.toggle_file_viewed(path.clone(), cx)
                })),
            )
            .child(Label::new(display).size(LabelSize::Small).color(if viewed {
                Color::Muted
            } else {
                Color::Default
            }))
            .child(
                Label::new(format!("+{} -{}", file.additions, file.deletions))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
    }

    fn render_threads(&self, loaded: &LoadedPullRequest, cx: &Context<Self>) -> impl IntoElement {
        v_flex()
            .gap_1()
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
        let is_replying = self.reply_target.as_ref() == Some(&thread.id);

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
            .children(thread.comments.iter().map(render_comment))
            .when(is_replying, |this| {
                this.child(
                    v_flex()
                        .gap_1()
                        .child(self.comment_editor.clone())
                        .child(
                            Button::new("submit-reply", "Comment")
                                .label_size(LabelSize::XSmall)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.submit_reply(window, cx)
                                })),
                        ),
                )
            })
    }
}

fn render_comment(comment: &ReviewComment) -> impl IntoElement {
    let reactions = (!comment.reactions.is_empty()).then(|| {
        h_flex().gap_1().children(
            comment
                .reactions
                .iter()
                .filter(|group| group.count > 0)
                .map(|group| {
                    Label::new(format!("{} {}", reaction_emoji(&group.content), group.count))
                        .size(LabelSize::XSmall)
                        .color(if group.viewer_has_reacted {
                            Color::Accent
                        } else {
                            Color::Muted
                        })
                }),
        )
    });

    v_flex()
        .gap_0p5()
        .py_0p5()
        .child(Label::new(comment.author.login.clone()).size(LabelSize::XSmall))
        .child(Label::new(comment.body.clone()).size(LabelSize::Small))
        .when_some(reactions, |this, reactions| this.child(reactions))
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
        Some(IconName::GitBranch).filter(|_| ReviewPanelSettings::get_global(cx).button)
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
}
