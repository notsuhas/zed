//! Native Pull Requests panel: browse, review, and act on GitHub pull
//! requests without leaving the editor.
//!
//! - `provider` — forge-agnostic review API (trait + domain types).
//! - `github_*` — the GitHub GraphQL implementation of that API.
//! - `pull_request_panel` — the dock panel and its sub-views.

mod diff_position;
mod github_graphql;
mod github_provider;
mod github_queries;
mod github_token;
mod provider;
mod pull_request_panel;
mod review_panel_settings;

pub use pull_request_panel::PullRequestPanel;

use gpui::App;
use workspace::Workspace;

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        pull_request_panel::register(workspace);
    })
    .detach();
}
