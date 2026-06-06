use crate::file_list::{FileList, FileListEvent};
use crate::github_provider::GitHubProvider;
use crate::github_token::{GITHUB_CREDENTIALS_URL, GitHubTokenSource, resolve_github_token};
use crate::inline_comment::{
    AddCommentAtCursor, ApplySuggestion, CancelInlineCommentDraft, ReplyToCommentThread,
    SubmitInlineCommentDraft, SuggestionBlock, ToggleCommentThread, parse_suggestions,
    render_new_comment_block, render_pr_comment_block,
};
use crate::pull_request_list::{PullRequestList, PullRequestListEvent};
use crate::review_panel_settings::ReviewPanelSettings;
use crate::review_provider::{
    PullRequestInfo, PullRequestState, ReviewComment, ReviewCommentTarget, ReviewProvider,
};
use crate::review_view::{ReviewView, ReviewViewEvent};
use anyhow::Result;
use collections::{HashMap, HashSet};
use editor::display_map::{BlockPlacement, BlockProperties, BlockStyle, CustomBlockId};
use editor::{Anchor, Editor};
use fs::Fs;
use git::repository::RepoPath;
use git::status::{DiffTreeType, TreeDiff};
use gpui::{
    Anchor as PopoverAnchor, App, AsyncWindowContext, Context, Entity, EntityId, EventEmitter,
    FocusHandle, Focusable, Pixels, Render, SharedString, Subscription, WeakEntity, Window,
};
use http_client::HttpClient;
use markdown::Markdown;
use project::{
    Project, ProjectItem as _,
    git_store::{GitStoreEvent, Repository, RepositoryEvent},
};
use settings::{self, Settings};
use std::sync::Arc;
use text::{Point, ToPoint as _};
use ui::{
    Button, ButtonSize, ButtonStyle, Color, ContextMenu, DynamicSpacing, IconButton, IconName,
    IconSize, IntoElement, Label, LabelSize, PopoverMenu, PopoverMenuHandle, Tab, TintColor,
    Tooltip, h_flex, prelude::*, v_flex,
};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};
use zed_actions::review_panel::ToggleFocus;

const REVIEW_PANEL_KEY: &str = "ReviewPanel";

#[derive(Clone)]
struct RecentReview {
    base_branch: SharedString,
    head_branch: SharedString,
    file_count: usize,
    pull_request: Option<PullRequestInfo>,
}

enum ActiveView {
    Empty,
    PullRequestList,
    ReviewThread,
    FileList,
    Configuration,
}

enum PendingAction {
    OpenDiff(RepoPath),
    OpenLocal(RepoPath),
    SelectPullRequest(PullRequestInfo),
}

#[derive(Clone)]
enum InlineCommentDraftTarget {
    NewThread { path: SharedString, line: u32 },
    Reply { comment_id: u64 },
}

struct InlineCommentDraft {
    target: InlineCommentDraftTarget,
    editor: Entity<Editor>,
    submitting: bool,
}

pub struct ReviewPanel {
    _workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    active_repository: Option<Entity<Repository>>,
    base_branch: Option<SharedString>,
    head_branch: Option<SharedString>,
    tree_diff: Option<TreeDiff>,
    file_list: Option<(Entity<FileList>, Subscription)>,
    review_view: Option<(Entity<ReviewView>, Subscription)>,
    focus_handle: FocusHandle,
    recent_reviews_menu_handle: PopoverMenuHandle<ContextMenu>,
    options_menu_handle: PopoverMenuHandle<ContextMenu>,
    fs: Arc<dyn Fs>,
    width: Option<Pixels>,
    active_view: ActiveView,
    recent_reviews: Vec<RecentReview>,
    http_client: Arc<dyn HttpClient>,
    pull_request_list: Option<(Entity<PullRequestList>, Subscription)>,
    provider: Option<Arc<dyn ReviewProvider>>,
    remote_owner: Option<String>,
    remote_repo: Option<String>,
    token_editor: Entity<Editor>,
    auth_source: GitHubTokenSource,
    configuration_status: Option<SharedString>,
    configuration_busy: bool,
    selected_pr: Option<PullRequestInfo>,
    pr_ref_fetch_task: Option<gpui::Task<Result<()>>>,
    pending_action: Option<PendingAction>,
    injected_comment_blocks: HashMap<EntityId, (WeakEntity<Editor>, Vec<CustomBlockId>)>,
    collapsed_comment_threads: HashSet<u64>,
    inline_comment_draft: Option<InlineCommentDraft>,
    _workspace_subscription: Option<Subscription>,
}

pub fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
        workspace.toggle_panel_focus::<ReviewPanel>(window, cx);
    });

    workspace.register_action(|workspace, action: &ApplySuggestion, _window, cx| {
        let comment_id = action.comment_id;
        let Some(panel) = workspace.panel::<ReviewPanel>(cx) else {
            return;
        };

        let active_editor = workspace
            .active_item(cx)
            .and_then(|item| item.act_as::<Editor>(cx));

        panel.update(cx, |panel, cx| {
            panel.handle_apply_suggestion(comment_id, active_editor, cx);
        });
    });

    workspace.register_action(|workspace, action: &ToggleCommentThread, _window, cx| {
        let comment_id = action.comment_id;
        let active_editor = workspace
            .active_item(cx)
            .and_then(|item| item.act_as::<Editor>(cx));
        let Some(panel) = workspace.panel::<ReviewPanel>(cx) else {
            return;
        };

        panel.update(cx, |panel, cx| {
            if !panel.collapsed_comment_threads.insert(comment_id) {
                panel.collapsed_comment_threads.remove(&comment_id);
            }

            panel.remove_all_injected_blocks(cx);
            if let Some(active_editor) = active_editor {
                panel.inject_comments_for_editor(active_editor, cx);
            }
        });
    });

    workspace.register_action(|workspace, action: &ReplyToCommentThread, window, cx| {
        let comment_id = action.comment_id;
        let active_editor = workspace
            .active_item(cx)
            .and_then(|item| item.act_as::<Editor>(cx));
        let Some(panel) = workspace.panel::<ReviewPanel>(cx) else {
            return;
        };

        panel.update(cx, |panel, cx| {
            panel.start_inline_comment_draft(
                InlineCommentDraftTarget::Reply { comment_id },
                active_editor,
                window,
                cx,
            );
        });
    });

    workspace.register_action(|workspace, _: &AddCommentAtCursor, window, cx| {
        let active_editor = workspace
            .active_item(cx)
            .and_then(|item| item.act_as::<Editor>(cx));
        let Some(panel) = workspace.panel::<ReviewPanel>(cx) else {
            return;
        };

        panel.update(cx, |panel, cx| {
            panel.start_comment_at_cursor(active_editor, window, cx);
        });
    });

    workspace.register_action(|workspace, _: &SubmitInlineCommentDraft, window, cx| {
        let active_editor = workspace
            .active_item(cx)
            .and_then(|item| item.act_as::<Editor>(cx));
        let Some(panel) = workspace.panel::<ReviewPanel>(cx) else {
            return;
        };

        panel.update(cx, |panel, cx| {
            panel.submit_inline_comment_draft(active_editor, window, cx);
        });
    });

    workspace.register_action(|workspace, _: &CancelInlineCommentDraft, _window, cx| {
        let active_editor = workspace
            .active_item(cx)
            .and_then(|item| item.act_as::<Editor>(cx));
        let Some(panel) = workspace.panel::<ReviewPanel>(cx) else {
            return;
        };

        panel.update(cx, |panel, cx| {
            panel.inline_comment_draft = None;
            panel.refresh_injected_comment_blocks(active_editor, cx);
        });
    });
}

impl ReviewPanel {
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
        cx.subscribe_in(
            &git_store,
            window,
            |this, _store, event, _window, cx| match event {
                GitStoreEvent::ActiveRepositoryChanged(_) => {
                    this.active_repository = this.project.read(cx).active_repository(cx);
                    this.initialize_provider(cx);
                    this.load_branches(cx);
                    cx.notify();
                }
                GitStoreEvent::RepositoryUpdated(
                    _,
                    RepositoryEvent::HeadChanged | RepositoryEvent::BranchListChanged,
                    _,
                ) => {
                    this.load_branches(cx);
                }
                GitStoreEvent::RepositoryUpdated(..) => {
                    if this.provider.is_none() {
                        this.initialize_provider(cx);
                    }
                }
                _ => {}
            },
        )
        .detach();

        let workspace_entity = weak_workspace.upgrade();
        let workspace_subscription = workspace_entity.map(|ws| {
            cx.subscribe(&ws, |this: &mut Self, workspace, event, cx| {
                if let workspace::Event::ActiveItemChanged = event {
                    this.on_active_item_changed(&workspace, cx);
                }
            })
        });

        let token_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("GitHub personal access token", window, cx);
            editor
        });

        let mut this = Self {
            _workspace: weak_workspace,
            project,
            active_repository,
            base_branch: None,
            head_branch: None,
            tree_diff: None,
            file_list: None,
            review_view: None,
            focus_handle: cx.focus_handle(),
            fs,
            width: None,
            recent_reviews_menu_handle: PopoverMenuHandle::default(),
            options_menu_handle: PopoverMenuHandle::default(),
            active_view: ActiveView::Empty,
            recent_reviews: Vec::new(),
            http_client: workspace.client().http_client(),
            pull_request_list: None,
            provider: None,
            remote_owner: None,
            remote_repo: None,
            token_editor,
            auth_source: GitHubTokenSource::None,
            configuration_status: None,
            configuration_busy: false,
            selected_pr: None,
            pr_ref_fetch_task: None,
            pending_action: None,
            injected_comment_blocks: HashMap::default(),
            collapsed_comment_threads: HashSet::default(),
            inline_comment_draft: None,
            _workspace_subscription: workspace_subscription,
        };
        this.initialize_provider(cx);
        this.load_branches(cx);
        this
    }

    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, window, cx| {
            let weak_workspace = workspace.weak_handle();
            cx.new(|cx| ReviewPanel::new(workspace, weak_workspace, window, cx))
        })
    }

    fn set_active_view(&mut self, new_view: ActiveView, cx: &mut Context<Self>) {
        self.active_view = new_view;
        cx.notify();
    }

    fn refresh_pull_requests(&mut self, cx: &mut Context<Self>) {
        if let Some((pr_list, _)) = &self.pull_request_list {
            pr_list.update(cx, |list, cx| list.refresh(cx));
        }
    }

    fn render_recent_reviews_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let recent = self.recent_reviews.clone();
        let weak_panel = cx.weak_entity();

        PopoverMenu::new("review-nav-menu")
            .trigger_with_tooltip(
                IconButton::new("review-nav-menu", IconName::MenuAltTemp)
                    .icon_size(IconSize::Small),
                Tooltip::text("Recent Reviews"),
            )
            .anchor(PopoverAnchor::TopRight)
            .with_handle(self.recent_reviews_menu_handle.clone())
            .menu(move |window, cx| {
                let recent = recent.clone();
                let weak_panel = weak_panel.clone();
                Some(ContextMenu::build(
                    window,
                    cx,
                    move |mut menu, _window, _cx| {
                        if recent.is_empty() {
                            return menu.entry("No recent reviews", None, |_window, _cx| {});
                        }

                        menu = menu.header("Recent");
                        for entry in &recent {
                            let label = if let Some(pr) = &entry.pull_request {
                                format!(
                                    "#{} {}..{} ({} files)",
                                    pr.number,
                                    entry.base_branch,
                                    entry.head_branch,
                                    entry.file_count
                                )
                            } else {
                                format!(
                                    "{}..{} ({} files)",
                                    entry.base_branch, entry.head_branch, entry.file_count
                                )
                            };
                            let base = entry.base_branch.clone();
                            let head = entry.head_branch.clone();
                            let pull_request = entry.pull_request.clone();
                            let weak_panel = weak_panel.clone();
                            menu = menu.entry(label, None, move |_window, cx| {
                                weak_panel
                                    .update(cx, |this, cx| {
                                        this.base_branch = Some(base.clone());
                                        this.head_branch = Some(head.clone());
                                        if let Some(pr) = &pull_request {
                                            this.select_pull_request(pr, cx);
                                        } else {
                                            this.selected_pr = None;
                                            this.review_view = None;
                                            this.load_diff(cx);
                                        }
                                    })
                                    .ok();
                            });
                        }
                        menu
                    },
                ))
            })
    }

    fn render_options_menu(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let weak_panel = cx.weak_entity();
        let show_tree_toggle = matches!(
            self.active_view,
            ActiveView::FileList | ActiveView::ReviewThread
        );
        let is_tree_view = match &self.active_view {
            ActiveView::FileList => self
                .file_list
                .as_ref()
                .map(|(fl, _)| fl.read(cx).is_tree_view())
                .unwrap_or(false),
            ActiveView::ReviewThread => self
                .review_view
                .as_ref()
                .map(|(rv, _)| rv.read(cx).is_tree_view())
                .unwrap_or(false),
            _ => false,
        };
        let is_review_thread = matches!(self.active_view, ActiveView::ReviewThread);

        PopoverMenu::new("review-options-menu")
            .trigger_with_tooltip(
                IconButton::new("review-options-menu", IconName::Ellipsis)
                    .icon_size(IconSize::Small),
                Tooltip::text("Options"),
            )
            .anchor(PopoverAnchor::TopRight)
            .with_handle(self.options_menu_handle.clone())
            .menu(move |window, cx| {
                let weak_panel = weak_panel.clone();
                Some(ContextMenu::build(window, cx, move |menu, _window, _| {
                    menu.when(show_tree_toggle, |menu| {
                        let weak_panel = weak_panel.clone();
                        menu.entry(
                            if is_tree_view {
                                "Flat View"
                            } else {
                                "Tree View"
                            },
                            None,
                            {
                                let weak_panel = weak_panel;
                                move |window, cx| {
                                    weak_panel
                                        .update(cx, |this, cx| {
                                            if is_review_thread {
                                                if let Some((review_view, _)) = &this.review_view {
                                                    review_view.update(cx, |rv, cx| {
                                                        rv.toggle_tree_view(cx);
                                                    });
                                                }
                                            } else if let Some((file_list, _)) = &this.file_list {
                                                file_list.update(cx, |fl, cx| {
                                                    fl.toggle_tree_view(window, cx);
                                                });
                                            }
                                        })
                                        .ok();
                                }
                            },
                        )
                        .separator()
                    })
                    .when(is_review_thread, |menu| {
                        let weak_panel = weak_panel.clone();
                        menu.entry("Add Comment at Cursor", None, move |window, cx| {
                            weak_panel
                                .update(cx, |this, cx| {
                                    let active_editor =
                                        this._workspace.upgrade().and_then(|workspace| {
                                            workspace
                                                .read(cx)
                                                .active_item(cx)
                                                .and_then(|item| item.act_as::<Editor>(cx))
                                        });
                                    this.start_comment_at_cursor(active_editor, window, cx);
                                })
                                .ok();
                        })
                        .separator()
                    })
                    .entry("Configuration", None, {
                        let weak_panel = weak_panel.clone();
                        move |_window, cx| {
                            weak_panel
                                .update(cx, |this, cx| {
                                    this.set_active_view(ActiveView::Configuration, cx);
                                })
                                .ok();
                        }
                    })
                    .separator()
                    .entry("Full Screen", None, |_window, _cx| {
                        // TODO: dispatch ToggleZoom action
                    })
                    .separator()
                    .entry("Refresh", None, {
                        let weak_panel = weak_panel;
                        move |_window, cx| {
                            weak_panel
                                .update(cx, |this, cx| {
                                    this.refresh_pull_requests(cx);
                                })
                                .ok();
                        }
                    })
                }))
            })
    }

    fn render_toolbar(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .id("review-panel-toolbar")
            .h(Tab::container_height(cx))
            .max_w_full()
            .flex_none()
            .justify_between()
            .gap_2()
            .bg(cx.theme().colors().tab_bar_background)
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                h_flex()
                    .size_full()
                    .gap(DynamicSpacing::Base04.rems(cx))
                    .pl(DynamicSpacing::Base04.rems(cx))
                    .child(Label::new("Review").size(LabelSize::Small)),
            )
            .child(
                h_flex()
                    .flex_none()
                    .gap(DynamicSpacing::Base02.rems(cx))
                    .pr(DynamicSpacing::Base06.rems(cx))
                    .child(
                        IconButton::new("new-review", IconName::Plus)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("New Review"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.show_pull_request_list(window, cx);
                            })),
                    )
                    .child(self.render_recent_reviews_menu(cx))
                    .child(self.render_options_menu(window, cx)),
            )
    }

    fn save_github_token(&mut self, cx: &mut Context<Self>) {
        let token = self.token_editor.read(cx).text(cx).trim().to_string();
        if token.is_empty() {
            self.configuration_status = Some("Enter a GitHub token first".into());
            cx.notify();
            return;
        }

        let credentials_provider = zed_credentials_provider::global(cx);
        self.configuration_busy = true;
        self.configuration_status = Some("Saving GitHub token...".into());
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = credentials_provider
                .write_credentials(GITHUB_CREDENTIALS_URL, "Bearer", token.as_bytes(), cx)
                .await;

            this.update(cx, |this, cx| {
                this.configuration_busy = false;
                match result {
                    Ok(()) => {
                        this.auth_source = GitHubTokenSource::Keychain;
                        this.configuration_status = Some("Saved GitHub token to keychain".into());
                        this.initialize_provider(cx);
                    }
                    Err(error) => {
                        this.configuration_status =
                            Some(format!("Failed to save token: {error}").into());
                    }
                }
                cx.notify();
            })?;

            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn test_github_connection(&mut self, cx: &mut Context<Self>) {
        let Some(provider) = self.provider.clone() else {
            self.configuration_status = Some("No GitHub provider is configured".into());
            cx.notify();
            return;
        };
        let Some(owner) = self.remote_owner.clone() else {
            self.configuration_status = Some("No GitHub remote owner detected".into());
            cx.notify();
            return;
        };
        let Some(repo) = self.remote_repo.clone() else {
            self.configuration_status = Some("No GitHub remote repository detected".into());
            cx.notify();
            return;
        };

        self.configuration_busy = true;
        self.configuration_status = Some("Testing GitHub connection...".into());
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = provider
                .fetch_pull_requests(&owner, &repo, PullRequestState::Open)
                .await;

            this.update(cx, |this, cx| {
                this.configuration_busy = false;
                this.configuration_status = Some(match result {
                    Ok(pull_requests) => {
                        format!("Connected to GitHub ({} open PRs)", pull_requests.len()).into()
                    }
                    Err(error) => format!("GitHub connection failed: {error}").into(),
                });
                cx.notify();
            })?;

            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn render_configuration(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let remote = match (&self.remote_owner, &self.remote_repo) {
            (Some(owner), Some(repo)) => format!("{owner}/{repo}"),
            _ => "No GitHub remote detected".to_string(),
        };
        let auth_source = self.auth_source.label();
        let status = self.configuration_status.clone();
        let busy = self.configuration_busy;

        v_flex()
            .id("review-configuration")
            .size_full()
            .overflow_scroll()
            .p_3()
            .gap_3()
            .child(Label::new("GitHub Configuration").size(LabelSize::Small))
            .child(
                v_flex()
                    .gap_1()
                    .child(Label::new(format!("Remote: {remote}")).size(LabelSize::Small))
                    .child(
                        Label::new(format!("Authentication: {auth_source}"))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        Label::new("Token")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        div()
                            .id("github-token-editor")
                            .w_full()
                            .p_1()
                            .rounded_md()
                            .border_1()
                            .border_color(cx.theme().colors().border)
                            .child(self.token_editor.clone()),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("save-github-token", "Save Token")
                            .size(ButtonSize::Compact)
                            .style(ButtonStyle::Tinted(TintColor::Accent))
                            .loading(busy)
                            .disabled(busy)
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.save_github_token(cx);
                            })),
                    )
                    .child(
                        Button::new("test-github-connection", "Test Connection")
                            .size(ButtonSize::Compact)
                            .disabled(busy)
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.test_github_connection(cx);
                            })),
                    ),
            )
            .when_some(status, |this, status| {
                this.child(
                    Label::new(status)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
            })
    }

    fn initialize_provider(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.active_repository.clone() else {
            log::info!("review_panel: no active repository");
            return;
        };

        let remote_url = repo.read(cx).default_remote_url();
        log::info!("review_panel: remote_url = {:?}", remote_url);
        let Some(remote_url) = remote_url else {
            log::info!("review_panel: no remote URL found");
            return;
        };

        let Ok((owner, repo_name)) = parse_github_remote(&remote_url) else {
            log::info!("review_panel: failed to parse remote URL: {}", remote_url);
            return;
        };
        log::info!("review_panel: parsed {}/{}", owner, repo_name);

        let http_client = self.http_client.clone();
        let credentials_provider = zed_credentials_provider::global(cx);

        self.remote_owner = Some(owner);
        self.remote_repo = Some(repo_name);

        cx.spawn(async move |this, cx| {
            let resolved_token = resolve_github_token(credentials_provider, cx).await;
            let token = resolved_token.token;
            let auth_source = resolved_token.source;

            let provider: Arc<dyn ReviewProvider> =
                Arc::new(GitHubProvider::new(http_client, token));

            this.update(cx, |this, cx| {
                this.provider = Some(provider.clone());
                this.auth_source = auth_source;
                this.configuration_status =
                    Some(format!("GitHub authentication: {}", auth_source.label()).into());
                if let (Some((pr_list, _)), Some(owner), Some(repo)) = (
                    &this.pull_request_list,
                    this.remote_owner.clone(),
                    this.remote_repo.clone(),
                ) {
                    pr_list.update(cx, |list, cx| {
                        list.set_provider(provider, owner, repo, cx);
                    });
                }
                cx.notify();
            })?;

            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn select_pull_request(&mut self, pr: &PullRequestInfo, cx: &mut Context<Self>) {
        self.remove_all_injected_blocks(cx);
        self.selected_pr = Some(pr.clone());
        self.base_branch = Some(pr.base_ref.clone());
        self.head_branch = Some(pr.head_ref.clone());

        self.fetch_pr_ref(pr.number, cx);
        self.pending_action = Some(PendingAction::SelectPullRequest(pr.clone()));
        self.set_active_view(ActiveView::ReviewThread, cx);
    }

    fn create_review_view(
        &mut self,
        pr: &PullRequestInfo,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let review_view = cx.new(|cx| {
            ReviewView::new(
                self.provider.clone(),
                self.remote_owner.clone(),
                self.remote_repo.clone(),
                pr.clone(),
                window,
                cx,
            )
        });
        let subscription = cx.subscribe_in(
            &review_view,
            window,
            |this, _view, event, window, cx| match event {
                ReviewViewEvent::OpenFileDiff(path) => {
                    this.open_file_diff(path.clone(), cx);
                }
                ReviewViewEvent::Back => {
                    this.selected_pr = None;
                    this.review_view = None;
                    this.remove_all_injected_blocks(cx);
                    this.show_pull_request_list(window, cx);
                }
            },
        );
        self.review_view = Some((review_view, subscription));
    }

    fn fetch_pr_ref(&mut self, pr_number: u32, cx: &mut Context<Self>) {
        let Some(repo) = self.active_repository.clone() else {
            return;
        };

        let work_dir = repo.read(cx).snapshot().work_directory_abs_path;
        let refspec = format!("pull/{}/head", pr_number);

        self.pr_ref_fetch_task = Some(cx.spawn(async move |this, cx| {
            let output = smol::process::Command::new("git")
                .current_dir(work_dir.as_ref())
                .args(["fetch", "origin", &refspec])
                .output()
                .await?;

            if output.status.success() {
                log::info!("review_panel: fetched PR #{} ref", pr_number);
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                log::warn!(
                    "review_panel: PR #{} ref fetch failed: {}",
                    pr_number,
                    stderr
                );
            }

            this.update(cx, |this, cx| {
                this.load_diff(cx);
            })?;
            anyhow::Ok(())
        }));
    }

    fn load_branches(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.active_repository.clone() else {
            return;
        };

        let default_branch_rx = repo.update(cx, |repo, _cx| repo.default_branch(false));
        let branches_rx = repo.update(cx, |repo, _cx| repo.branches());

        cx.spawn(async move |this, cx| {
            if let Ok(Some(default)) = default_branch_rx.await? {
                this.update(cx, |this, cx| {
                    this.base_branch = Some(default);
                    cx.notify();
                })?;
            }

            if let Ok(branches) = branches_rx.await? {
                let head = branches
                    .branches
                    .iter()
                    .find(|b| b.is_head)
                    .map(|b| b.ref_name.clone());
                if let Some(head) = head {
                    this.update(cx, |this, cx| {
                        this.head_branch = Some(head);
                        this.load_diff(cx);
                        cx.notify();
                    })?;
                }
            }

            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn load_diff(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.active_repository.clone() else {
            return;
        };

        let Some(base) = self.base_branch.clone() else {
            return;
        };

        let Some(head) = self.head_branch.clone() else {
            return;
        };

        let diff_rx = repo.update(cx, |repo, cx| {
            repo.diff_tree(DiffTreeType::MergeBase { base, head }, cx)
        });

        cx.spawn(async move |this, cx| {
            let tree_diff = diff_rx.await??;
            this.update(cx, |this, cx| {
                this.tree_diff = Some(tree_diff);

                let file_count = this
                    .tree_diff
                    .as_ref()
                    .map(|d| d.entries.len())
                    .unwrap_or(0);

                if let (Some(base), Some(head)) =
                    (this.base_branch.clone(), this.head_branch.clone())
                {
                    let pull_request = this.selected_pr.clone();
                    this.recent_reviews
                        .retain(|r| !(r.base_branch == base && r.head_branch == head));
                    this.recent_reviews.insert(
                        0,
                        RecentReview {
                            base_branch: base,
                            head_branch: head,
                            file_count,
                            pull_request,
                        },
                    );
                    this.recent_reviews.truncate(10);
                }

                if let Some((review_view, _)) = &this.review_view {
                    review_view.update(cx, |view, cx| {
                        view.set_tree_diff(this.tree_diff.as_ref(), cx);
                    });
                }

                if !matches!(this.active_view, ActiveView::ReviewThread) && file_count > 0 {
                    this.show_file_list(cx);
                }
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn show_file_list(&mut self, cx: &mut Context<Self>) {
        let file_list = cx.new(|cx| {
            FileList::new(
                self.base_branch.clone(),
                self.head_branch.clone(),
                self.tree_diff.as_ref(),
                cx,
            )
        });
        let subscription = cx.subscribe(&file_list, |this, _file_list, event, cx| match event {
            FileListEvent::OpenFileDiff(path) => {
                this.open_file_diff(path.clone(), cx);
            }
            FileListEvent::OpenLocalFile(path) => {
                this.open_local_file_by_path(path.clone(), cx);
            }
        });
        self.file_list = Some((file_list, subscription));
        self.active_view = ActiveView::FileList;
        cx.notify();
    }

    fn show_pull_request_list(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pull_request_list.is_none() {
            let pr_list = cx.new(|cx| {
                PullRequestList::new(
                    self.provider.clone(),
                    self.remote_owner.clone(),
                    self.remote_repo.clone(),
                    window,
                    cx,
                )
            });
            let subscription = cx.subscribe(&pr_list, |this, _pr_list, event, cx| match event {
                PullRequestListEvent::Selected(pr) => {
                    this.select_pull_request(&pr.clone(), cx);
                }
            });
            self.pull_request_list = Some((pr_list, subscription));
        }

        if let Some((pr_list, _)) = &self.pull_request_list {
            pr_list.update(cx, |list, cx| list.load_if_empty(cx));
        }

        self.active_view = ActiveView::PullRequestList;
        cx.notify();
    }

    fn open_file_diff(&mut self, path: RepoPath, cx: &mut Context<Self>) {
        if let Some((file_list, _)) = &self.file_list {
            file_list.update(cx, |fl, cx| {
                fl.mark_viewed(path.clone(), cx);
                fl.select_path(&path, cx);
            });
        }

        self.pending_action = Some(PendingAction::OpenDiff(path));
        cx.notify();
    }

    fn open_local_file_by_path(&mut self, path: RepoPath, cx: &mut Context<Self>) {
        self.pending_action = Some(PendingAction::OpenLocal(path));
        cx.notify();
    }

    fn on_active_item_changed(&mut self, workspace: &Entity<Workspace>, cx: &mut Context<Self>) {
        if self.selected_pr.is_none() {
            return;
        }
        if self.review_view.is_none() {
            return;
        }

        let active_item = workspace.read(cx).active_item(cx);
        let Some(item) = active_item else {
            return;
        };
        let Some(editor) = item.act_as::<Editor>(cx) else {
            return;
        };

        self.inject_comments_for_editor(editor, cx);
    }

    fn inject_comments_for_editor(&mut self, editor: Entity<Editor>, cx: &mut Context<Self>) {
        let editor_id = editor.entity_id();
        if self.injected_comment_blocks.contains_key(&editor_id) {
            return;
        }

        let Some((review_view, _)) = &self.review_view else {
            return;
        };

        let project_path = editor
            .read(cx)
            .buffer()
            .read(cx)
            .as_singleton()
            .and_then(|buffer| buffer.read(cx).project_path(cx));
        if let Some(project_path) = project_path {
            let file_path = SharedString::from(
                project_path
                    .path
                    .as_ref()
                    .as_std_path()
                    .to_string_lossy()
                    .to_string(),
            );
            let comments = review_view.read(cx).comments_for_file(&file_path);
            if !comments.is_empty() {
                self.inject_pr_comments_into_editor(&editor, &file_path, &comments, cx);
            } else {
                self.inject_new_comment_draft_into_editor(&editor, &file_path, cx);
            }
            return;
        }

        self.inject_for_multibuffer_editor(&editor, &review_view.clone(), cx);
    }

    fn new_inline_comment_editor(window: &mut Window, cx: &mut Context<Self>) -> Entity<Editor> {
        cx.new(|cx| {
            let mut editor = Editor::auto_height(2, 6, window, cx);
            editor.set_placeholder_text("Leave a comment...", window, cx);
            editor.set_show_gutter(false, cx);
            editor.set_show_wrap_guides(false, cx);
            editor.set_show_indent_guides(false, cx);
            editor.set_use_autoclose(false);
            editor
        })
    }

    fn start_inline_comment_draft(
        &mut self,
        target: InlineCommentDraftTarget,
        active_editor: Option<Entity<Editor>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let editor = Self::new_inline_comment_editor(window, cx);
        window.focus(&editor.focus_handle(cx), cx);
        self.inline_comment_draft = Some(InlineCommentDraft {
            target,
            editor,
            submitting: false,
        });
        self.refresh_injected_comment_blocks(active_editor, cx);
    }

    fn start_comment_at_cursor(
        &mut self,
        active_editor: Option<Entity<Editor>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(editor) = active_editor.clone() else {
            return;
        };
        let Some((path, line)) = Self::active_editor_comment_location(&editor, cx) else {
            return;
        };

        let target = self
            .review_view
            .as_ref()
            .and_then(|(review_view, _)| {
                review_view
                    .read(cx)
                    .comments_for_file(&path)
                    .into_iter()
                    .find(|comment| comment.reply_to.is_none() && comment.line == Some(line))
            })
            .map(|comment| InlineCommentDraftTarget::Reply {
                comment_id: comment.id,
            })
            .unwrap_or(InlineCommentDraftTarget::NewThread { path, line });

        self.start_inline_comment_draft(target, active_editor, window, cx);
    }

    fn active_editor_comment_location(
        editor: &Entity<Editor>,
        cx: &mut Context<Self>,
    ) -> Option<(SharedString, u32)> {
        let editor = editor.read(cx);
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let (buffer_anchor, _) =
            snapshot.anchor_to_buffer_anchor(editor.selections.newest_anchor().head())?;
        let buffer_snapshot = snapshot.buffer_for_id(buffer_anchor.buffer_id)?;
        let path_key = snapshot.path_for_buffer(buffer_anchor.buffer_id)?;
        let point = buffer_anchor.to_point(buffer_snapshot);
        Some((
            SharedString::from(path_key.path.as_std_path().to_string_lossy().to_string()),
            point.row + 1,
        ))
    }

    fn refresh_injected_comment_blocks(
        &mut self,
        active_editor: Option<Entity<Editor>>,
        cx: &mut Context<Self>,
    ) {
        self.remove_all_injected_blocks(cx);
        if let Some(active_editor) = active_editor {
            self.inject_comments_for_editor(active_editor, cx);
        }
        cx.notify();
    }

    #[cfg(feature = "test-support")]
    pub fn set_visual_test_state(
        &mut self,
        state: crate::test_support::VisualReviewPanelState,
        active_editor: Option<Entity<Editor>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pull_request = PullRequestInfo {
            number: state.pull_request.number,
            title: state.pull_request.title,
            author: state.pull_request.author,
            description: "Review panel visual test pull request".into(),
            state: PullRequestState::Open,
            base_ref: state.pull_request.base_ref,
            head_ref: state.pull_request.head_ref,
            base_sha: "base-sha".into(),
            head_sha: state.pull_request.head_sha,
            created_at: "2026-04-26T17:00:00Z".into(),
            updated_at: "2026-04-26T17:30:00Z".into(),
            review_status: crate::review_provider::ReviewStatus::Commented,
        };

        self.remove_all_injected_blocks(cx);
        self.provider = None;
        self.remote_owner = Some("zed-industries".to_string());
        self.remote_repo = Some("zed".to_string());
        self.selected_pr = Some(pull_request.clone());
        self.base_branch = Some(pull_request.base_ref.clone());
        self.head_branch = Some(pull_request.head_ref.clone());
        self.collapsed_comment_threads = state.collapsed_threads.into_iter().collect();
        self.inline_comment_draft = state.draft.map(|draft| {
            let (target, body) = match draft {
                crate::test_support::VisualInlineCommentDraft::NewThread { path, line, body } => {
                    (InlineCommentDraftTarget::NewThread { path, line }, body)
                }
                crate::test_support::VisualInlineCommentDraft::Reply { comment_id, body } => {
                    (InlineCommentDraftTarget::Reply { comment_id }, body)
                }
            };
            let editor = Self::new_inline_comment_editor(window, cx);
            editor.update(cx, |editor, cx| {
                editor.set_text(body.to_string(), window, cx);
            });
            InlineCommentDraft {
                target,
                editor,
                submitting: false,
            }
        });

        self.create_review_view(&pull_request, window, cx);
        if let Some((review_view, _)) = &self.review_view {
            review_view.update(cx, |review_view, cx| {
                review_view.set_visual_test_data(state.files, state.comments, state.tree_view, cx);
            });
        }

        self.set_active_view(ActiveView::ReviewThread, cx);
        if let Some(active_editor) = active_editor {
            self.inject_comments_for_editor(active_editor, cx);
        }
    }

    fn reply_composer_for(&self, thread_id: Option<u64>) -> Option<Entity<Editor>> {
        let thread_id = thread_id?;
        let draft = self.inline_comment_draft.as_ref()?;
        match draft.target {
            InlineCommentDraftTarget::Reply { comment_id } if comment_id == thread_id => {
                Some(draft.editor.clone())
            }
            _ => None,
        }
    }

    fn new_thread_composer_for(
        &self,
        file_path: &SharedString,
    ) -> Option<(u32, Entity<Editor>, bool)> {
        let draft = self.inline_comment_draft.as_ref()?;
        match &draft.target {
            InlineCommentDraftTarget::NewThread { path, line } if path == file_path => {
                Some((*line, draft.editor.clone(), draft.submitting))
            }
            _ => None,
        }
    }

    fn inline_comment_draft_submitting(&self) -> bool {
        self.inline_comment_draft
            .as_ref()
            .map(|draft| draft.submitting)
            .unwrap_or(false)
    }

    fn submit_inline_comment_draft(
        &mut self,
        active_editor: Option<Entity<Editor>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(provider) = self.provider.clone() else {
            return;
        };
        let Some(owner) = self.remote_owner.clone() else {
            return;
        };
        let Some(repo) = self.remote_repo.clone() else {
            return;
        };
        let Some(selected_pr) = self.selected_pr.clone() else {
            return;
        };
        let Some((review_view, _)) = &self.review_view else {
            return;
        };
        let review_view = review_view.clone();
        let Some(draft) = self.inline_comment_draft.as_mut() else {
            return;
        };

        let body = draft.editor.read(cx).text(cx);
        if body.trim().is_empty() {
            return;
        }

        let target = draft.target.clone();
        let provider_target = match &target {
            InlineCommentDraftTarget::NewThread { path, line } => ReviewCommentTarget::NewThread {
                path: path.clone(),
                line: *line,
                commit_sha: selected_pr.head_sha.clone(),
            },
            InlineCommentDraftTarget::Reply { comment_id } => ReviewCommentTarget::Reply {
                in_reply_to: *comment_id,
            },
        };

        draft.submitting = true;
        self.refresh_injected_comment_blocks(active_editor.clone(), cx);

        cx.spawn_in(window, async move |this, cx| {
            let result = provider
                .submit_comment(&owner, &repo, selected_pr.number, &body, provider_target)
                .await;

            this.update_in(cx, |this, _window, cx| {
                match result {
                    Ok(mut new_comment) => {
                        match &target {
                            InlineCommentDraftTarget::NewThread { path, line } => {
                                new_comment.path.get_or_insert_with(|| path.clone());
                                new_comment.line.get_or_insert(*line);
                            }
                            InlineCommentDraftTarget::Reply { comment_id } => {
                                new_comment.reply_to.get_or_insert(*comment_id);
                                if new_comment.path.is_none() || new_comment.line.is_none() {
                                    if let Some(parent_comment) = review_view
                                        .read(cx)
                                        .pr_comments()
                                        .iter()
                                        .find(|comment| comment.id == *comment_id)
                                    {
                                        if new_comment.path.is_none() {
                                            new_comment.path = parent_comment.path.clone();
                                        }
                                        if new_comment.line.is_none() {
                                            new_comment.line = parent_comment.line;
                                        }
                                    }
                                }
                            }
                        }

                        review_view.update(cx, |review_view, cx| {
                            review_view.push_comment(new_comment, "Comment posted", cx);
                        });
                        this.inline_comment_draft = None;
                    }
                    Err(error) => {
                        if let Some(draft) = this.inline_comment_draft.as_mut() {
                            draft.submitting = false;
                        }
                        review_view.update(cx, |review_view, cx| {
                            review_view
                                .set_status_message(format!("Failed to post comment: {error}"), cx);
                        });
                    }
                }

                this.refresh_injected_comment_blocks(active_editor, cx);
            })?;

            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn inject_for_multibuffer_editor(
        &mut self,
        editor: &Entity<Editor>,
        review_view: &Entity<ReviewView>,
        cx: &mut Context<Self>,
    ) {
        let multibuffer = editor.read(cx).buffer().clone();
        let snapshot = multibuffer.read(cx).snapshot(cx);
        let mut blocks: Vec<BlockProperties<Anchor>> = Vec::new();

        for excerpt in snapshot.excerpts() {
            let buffer_id = excerpt.context.start.buffer_id;
            let Some(buffer_snapshot) = snapshot.buffer_for_id(buffer_id) else {
                continue;
            };
            let Some(path_key) = snapshot.path_for_buffer(buffer_id) else {
                continue;
            };

            let file_path =
                SharedString::from(path_key.path.as_std_path().to_string_lossy().to_string());
            let comments = review_view.read(cx).comments_for_file(&file_path);
            if comments.is_empty() {
                continue;
            }

            for (line, thread_comments) in Self::build_comment_threads(&comments, cx) {
                let row = line.saturating_sub(1);
                if row > buffer_snapshot.max_point().row {
                    continue;
                }

                let text_anchor = buffer_snapshot.anchor_before(Point::new(row, 0));
                if !excerpt.contains(&text_anchor, buffer_snapshot) {
                    continue;
                }

                let Some(anchor) = snapshot.anchor_in_excerpt(text_anchor) else {
                    continue;
                };

                let thread_id = thread_comments.first().map(|(comment, _, _)| comment.id);
                let collapsed = thread_id
                    .map(|thread_id| self.collapsed_comment_threads.contains(&thread_id))
                    .unwrap_or(false);
                let composer = self.reply_composer_for(thread_id);
                let submitting = self.inline_comment_draft_submitting();
                let height = Self::estimate_block_height(&thread_comments)
                    + composer.as_ref().map(|_| 6).unwrap_or(0);
                let height = if collapsed { 2 } else { height };
                let thread_clone = thread_comments.clone();
                blocks.push(BlockProperties {
                    placement: BlockPlacement::Below(anchor),
                    height: Some(height),
                    style: BlockStyle::Flex,
                    render: Arc::new(move |cx| {
                        render_pr_comment_block(
                            thread_clone.clone(),
                            collapsed,
                            composer.clone(),
                            submitting,
                            cx,
                        )
                    }),
                    priority: 0,
                });
            }

            if let Some((line, editor_entity, submitting)) =
                self.new_thread_composer_for(&file_path)
            {
                let row = line.saturating_sub(1);
                if row <= buffer_snapshot.max_point().row {
                    let text_anchor = buffer_snapshot.anchor_before(Point::new(row, 0));
                    if excerpt.contains(&text_anchor, buffer_snapshot) {
                        if let Some(anchor) = snapshot.anchor_in_excerpt(text_anchor) {
                            blocks.push(BlockProperties {
                                placement: BlockPlacement::Below(anchor),
                                height: Some(6),
                                style: BlockStyle::Flex,
                                render: Arc::new(move |cx| {
                                    render_new_comment_block(
                                        line,
                                        editor_entity.clone(),
                                        submitting,
                                        cx,
                                    )
                                }),
                                priority: 1,
                            });
                        }
                    }
                }
            }
        }

        if blocks.is_empty() {
            return;
        }

        let editor_id = editor.entity_id();
        let block_ids = editor.update(cx, |editor, cx| editor.insert_blocks(blocks, None, cx));
        self.injected_comment_blocks
            .insert(editor_id, (editor.downgrade(), block_ids));
    }

    fn inject_pr_comments_into_editor(
        &mut self,
        editor: &Entity<Editor>,
        file_path: &SharedString,
        comments: &[ReviewComment],
        cx: &mut Context<Self>,
    ) {
        let threads = Self::build_comment_threads(comments, cx);

        let editor_id = editor.entity_id();
        let block_ids = editor.update(cx, |editor, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let max_row = snapshot.max_point().row;

            let mut blocks: Vec<_> = threads
                .into_iter()
                .filter_map(|(line, thread_comments)| {
                    let row = line.saturating_sub(1);
                    if row > max_row {
                        return None;
                    }
                    let anchor = snapshot.anchor_before(Point::new(row, 0));
                    let thread_id = thread_comments.first().map(|(comment, _, _)| comment.id);
                    let collapsed = thread_id
                        .map(|thread_id| self.collapsed_comment_threads.contains(&thread_id))
                        .unwrap_or(false);
                    let composer = self.reply_composer_for(thread_id);
                    let submitting = self.inline_comment_draft_submitting();
                    let height = Self::estimate_block_height(&thread_comments)
                        + composer.as_ref().map(|_| 6).unwrap_or(0);
                    let height = if collapsed { 2 } else { height };
                    let thread_clone = thread_comments;
                    Some(BlockProperties {
                        placement: BlockPlacement::Below(anchor),
                        height: Some(height),
                        style: BlockStyle::Flex,
                        render: Arc::new(move |cx| {
                            render_pr_comment_block(
                                thread_clone.clone(),
                                collapsed,
                                composer.clone(),
                                submitting,
                                cx,
                            )
                        }),
                        priority: 0,
                    })
                })
                .collect();

            if let Some((line, editor_entity, submitting)) = self.new_thread_composer_for(file_path)
            {
                let row = line.saturating_sub(1);
                if row <= max_row {
                    let anchor = snapshot.anchor_before(Point::new(row, 0));
                    blocks.push(BlockProperties {
                        placement: BlockPlacement::Below(anchor),
                        height: Some(6),
                        style: BlockStyle::Flex,
                        render: Arc::new(move |cx| {
                            render_new_comment_block(line, editor_entity.clone(), submitting, cx)
                        }),
                        priority: 1,
                    });
                }
            }

            editor.insert_blocks(blocks, None, cx)
        });

        self.injected_comment_blocks
            .insert(editor_id, (editor.downgrade(), block_ids));
    }

    fn inject_new_comment_draft_into_editor(
        &mut self,
        editor: &Entity<Editor>,
        file_path: &SharedString,
        cx: &mut Context<Self>,
    ) {
        let Some((line, editor_entity, submitting)) = self.new_thread_composer_for(file_path)
        else {
            return;
        };

        let editor_id = editor.entity_id();
        let block_ids = editor.update(cx, |editor, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let max_row = snapshot.max_point().row;
            let row = line.saturating_sub(1);
            if row > max_row {
                return Vec::new();
            }

            let anchor = snapshot.anchor_before(Point::new(row, 0));
            editor.insert_blocks(
                vec![BlockProperties {
                    placement: BlockPlacement::Below(anchor),
                    height: Some(6),
                    style: BlockStyle::Flex,
                    render: Arc::new(move |cx| {
                        render_new_comment_block(line, editor_entity.clone(), submitting, cx)
                    }),
                    priority: 1,
                }],
                None,
                cx,
            )
        });

        self.injected_comment_blocks
            .insert(editor_id, (editor.downgrade(), block_ids));
    }

    fn remove_all_injected_blocks(&mut self, cx: &mut Context<Self>) {
        let entries: Vec<_> = self.injected_comment_blocks.drain().collect();
        for (_editor_id, (weak_editor, block_ids)) in entries {
            if let Some(editor) = weak_editor.upgrade() {
                let block_id_set = block_ids.into_iter().collect();
                editor.update(cx, |editor, cx| {
                    editor.remove_blocks(block_id_set, None, cx);
                });
            }
        }
    }

    fn build_comment_threads(
        comments: &[ReviewComment],
        cx: &mut App,
    ) -> Vec<(
        u32,
        Vec<(ReviewComment, Entity<Markdown>, Vec<SuggestionBlock>)>,
    )> {
        let mut threads: Vec<(
            u32,
            Vec<(ReviewComment, Entity<Markdown>, Vec<SuggestionBlock>)>,
        )> = Vec::new();

        for comment in comments {
            if comment.reply_to.is_none() {
                if let Some(line) = comment.line {
                    let (cleaned_body, suggestions) = parse_suggestions(&comment.body);
                    let md = cx
                        .new(|cx| Markdown::new(SharedString::from(cleaned_body), None, None, cx));
                    threads.push((line, vec![(comment.clone(), md, suggestions)]));
                }
            }
        }

        for comment in comments {
            if let Some(reply_to) = comment.reply_to {
                for (_, thread) in &mut threads {
                    if thread.first().map(|(c, _, _)| c.id) == Some(reply_to) {
                        let (cleaned_body, suggestions) = parse_suggestions(&comment.body);
                        let md = cx.new(|cx| {
                            Markdown::new(SharedString::from(cleaned_body), None, None, cx)
                        });
                        thread.push((comment.clone(), md, suggestions));
                        break;
                    }
                }
            }
        }

        threads
    }

    fn estimate_block_height(
        thread: &[(ReviewComment, Entity<Markdown>, Vec<SuggestionBlock>)],
    ) -> u32 {
        let mut total: u32 = 0;
        for (comment, _, suggestions) in thread {
            let line_count = comment.body.lines().count().max(1) as u32;
            total += 1 + line_count + 1;
            for suggestion in suggestions {
                let suggestion_lines = suggestion.suggested_code.lines().count().max(1) as u32;
                total += 2 + suggestion_lines + 1;
            }
        }
        total.max(3)
    }

    fn handle_apply_suggestion(
        &mut self,
        comment_id: u64,
        active_editor: Option<Entity<Editor>>,
        cx: &mut Context<Self>,
    ) {
        let Some((review_view, _)) = &self.review_view else {
            log::warn!("apply_suggestion: no review view");
            return;
        };

        let comment = review_view
            .read(cx)
            .pr_comments()
            .iter()
            .find(|c| c.id == comment_id)
            .cloned();

        let Some(comment) = comment else {
            log::warn!("apply_suggestion: comment {} not found", comment_id);
            return;
        };

        let (_, suggestions) = parse_suggestions(&comment.body);
        let Some(suggestion) = suggestions.into_iter().next() else {
            log::warn!("apply_suggestion: no suggestion in comment {}", comment_id);
            return;
        };

        let Some(line) = comment.line else {
            log::warn!("apply_suggestion: comment {} has no line", comment_id);
            return;
        };

        let Some(editor) = active_editor else {
            log::warn!("apply_suggestion: no active editor");
            return;
        };

        editor.update(cx, |editor, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let max_row = snapshot.max_point().row;
            let row = line.saturating_sub(1);
            if row > max_row {
                log::warn!(
                    "apply_suggestion: row {} exceeds buffer max {}",
                    row,
                    max_row
                );
                return;
            }

            let line_start = Point::new(row, 0);
            let line_end = if row < max_row {
                Point::new(row + 1, 0)
            } else {
                snapshot.max_point()
            };

            let replacement = if row < max_row {
                format!("{}\n", suggestion.suggested_code)
            } else {
                suggestion.suggested_code.clone()
            };

            editor.edit([(line_start..line_end, replacement.as_str())], cx);
        });
    }

    fn flush_pending_action(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(action) = self.pending_action.take() else {
            return;
        };
        match action {
            PendingAction::OpenDiff(path) => {
                let Some(workspace) = self._workspace.upgrade() else {
                    return;
                };
                let Some(active_repo) = self.active_repository.as_ref() else {
                    return;
                };
                let Some(project_path) = active_repo.read(cx).repo_path_to_project_path(&path, cx)
                else {
                    return;
                };

                if let Some(pr) = self.selected_pr.as_ref() {
                    let base_ref = pr.base_ref.clone();
                    let head_ref = Some(pr.head_sha.clone());
                    workspace.update(cx, |workspace, cx| {
                        git_ui::project_diff::ProjectDiff::deploy_merge_diff(
                            workspace,
                            base_ref,
                            head_ref,
                            Some(project_path),
                            window,
                            cx,
                        );
                    });
                } else {
                    window.dispatch_action(Box::new(git_ui::project_diff::BranchDiff), cx);
                }
            }
            PendingAction::OpenLocal(path) => {
                let Some(active_repo) = self.active_repository.as_ref() else {
                    return;
                };
                let Some(project_path) = active_repo.read(cx).repo_path_to_project_path(&path, cx)
                else {
                    return;
                };
                let Some(workspace) = self._workspace.upgrade() else {
                    return;
                };
                workspace.update(cx, |workspace, cx| {
                    workspace
                        .open_path_preview(project_path, None, true, false, true, window, cx)
                        .detach_and_log_err(cx);
                });
            }
            PendingAction::SelectPullRequest(pr) => {
                self.create_review_view(&pr, window, cx);
            }
        }
    }
}

impl Render for ReviewPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.flush_pending_action(window, cx);
        v_flex()
            .id("review_panel")
            .track_focus(&self.focus_handle)
            .size_full()
            .child(self.render_toolbar(window, cx))
            .map(|parent| match &self.active_view {
                ActiveView::Empty => parent.child(
                    v_flex()
                        .size_full()
                        .justify_center()
                        .items_center()
                        .child(Label::new("No review selected").color(Color::Muted)),
                ),
                ActiveView::PullRequestList => {
                    if let Some((pr_list, _)) = &self.pull_request_list {
                        parent.child(pr_list.clone())
                    } else {
                        parent.child(
                            v_flex()
                                .size_full()
                                .justify_center()
                                .items_center()
                                .child(Label::new("Loading...").color(Color::Muted)),
                        )
                    }
                }
                ActiveView::ReviewThread => {
                    if let Some((review_view, _)) = &self.review_view {
                        parent.child(review_view.clone())
                    } else {
                        parent.child(
                            v_flex()
                                .size_full()
                                .justify_center()
                                .items_center()
                                .child(Label::new("Loading...").color(Color::Muted)),
                        )
                    }
                }
                ActiveView::FileList => {
                    if let Some((file_list, _)) = &self.file_list {
                        parent.child(file_list.clone())
                    } else {
                        parent.child(
                            v_flex()
                                .size_full()
                                .justify_center()
                                .items_center()
                                .child(Label::new("Loading...").color(Color::Muted)),
                        )
                    }
                }
                ActiveView::Configuration => parent.child(self.render_configuration(cx)),
            })
    }
}

impl Focusable for ReviewPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for ReviewPanel {}

impl Panel for ReviewPanel {
    fn persistent_name() -> &'static str {
        "ReviewPanel"
    }

    fn panel_key() -> &'static str {
        REVIEW_PANEL_KEY
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

    fn icon(&self, _window: &Window, cx: &App) -> Option<ui::IconName> {
        Some(ui::IconName::PullRequest).filter(|_| ReviewPanelSettings::get_global(cx).button)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Review Panel")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        10
    }
}

fn parse_github_remote(url: &str) -> anyhow::Result<(String, String)> {
    if let Some(rest) = url.strip_prefix("git@github.com:") {
        let rest = rest.trim_end_matches(".git");
        let parts: Vec<&str> = rest.splitn(2, '/').collect();
        if parts.len() == 2 {
            return Ok((parts[0].to_string(), parts[1].to_string()));
        }
    }

    if let Some(rest) = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))
    {
        let rest = rest.trim_end_matches(".git");
        let parts: Vec<&str> = rest.splitn(2, '/').collect();
        if parts.len() == 2 {
            return Ok((parts[0].to_string(), parts[1].to_string()));
        }
    }

    anyhow::bail!("Could not parse GitHub owner/repo from remote URL: {}", url)
}
