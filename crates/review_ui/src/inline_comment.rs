use crate::review_provider::ReviewComment;
use editor::Editor;
use editor::display_map::BlockContext;
use gpui::{AnyElement, Entity, SharedString};
use markdown::{Markdown, MarkdownElement, MarkdownFont, MarkdownStyle};
use ui::{
    Button, ButtonLike, ButtonStyle, Color, FluentBuilder, Icon, IconName, IconSize, IntoElement,
    Label, LabelSize, Tooltip, h_flex, prelude::*, v_flex,
};

#[derive(Clone, Debug)]
pub struct SuggestionBlock {
    pub suggested_code: String,
}

/// Extracts ```suggestion fenced blocks from a comment body.
/// Returns the body with suggestion blocks removed and the extracted suggestions.
pub fn parse_suggestions(body: &str) -> (String, Vec<SuggestionBlock>) {
    let mut cleaned_lines: Vec<&str> = Vec::new();
    let mut suggestions: Vec<SuggestionBlock> = Vec::new();
    let mut inside_suggestion = false;
    let mut current_suggestion_lines: Vec<&str> = Vec::new();

    for line in body.lines() {
        if inside_suggestion {
            if line.trim() == "```" {
                suggestions.push(SuggestionBlock {
                    suggested_code: current_suggestion_lines.join("\n"),
                });
                current_suggestion_lines.clear();
                inside_suggestion = false;
            } else {
                current_suggestion_lines.push(line);
            }
        } else {
            let trimmed = line.trim();
            if trimmed.starts_with("```suggestion") {
                inside_suggestion = true;
            } else {
                cleaned_lines.push(line);
            }
        }
    }

    (cleaned_lines.join("\n"), suggestions)
}

/// Renders a PR comment thread (parent + replies) as an inline editor block.
/// Each tuple pairs a comment with its pre-created Markdown entity and extracted suggestions.
pub fn render_pr_comment_block(
    comments: Vec<(ReviewComment, Entity<Markdown>, Vec<SuggestionBlock>)>,
    collapsed: bool,
    composer: Option<Entity<Editor>>,
    submitting: bool,
    cx: &mut BlockContext,
) -> AnyElement {
    let colors = cx.theme().colors().clone();
    let style = MarkdownStyle::themed(MarkdownFont::Editor, cx.window, cx.app);
    let thread_id = comments.first().map(|(comment, _, _)| comment.id);
    let comment_count = comments.len();

    let mut comment_body = v_flex().w_full().overflow_x_hidden().gap_1();

    if collapsed {
        comment_body = comment_body.child(
            Label::new(format!("{comment_count} review comment(s)"))
                .size(LabelSize::XSmall)
                .color(Color::Muted),
        );
    }

    let container = h_flex()
        .w_full()
        .max_w(cx.max_width - cx.anchor_x)
        .items_start()
        .overflow_x_hidden()
        .pr_2()
        .py_1()
        .gap_1()
        .border_t_1()
        .border_b_1()
        .border_color(colors.border)
        .bg(colors.editor_background)
        .child(h_flex().w(cx.anchor_x).justify_end().pr_1().when_some(
            thread_id,
            |this, comment_id| {
                this.child(
                    ButtonLike::new(SharedString::from(format!(
                        "toggle_review_comment_{comment_id}"
                    )))
                    .child(
                        Icon::new(IconName::ChevronUp)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .tooltip(Tooltip::text(if collapsed {
                        "Show review comments"
                    } else {
                        "Hide review comments"
                    }))
                    .on_click(move |_event, window, cx| {
                        window.dispatch_action(Box::new(ToggleCommentThread { comment_id }), cx);
                    }),
                )
            },
        ));

    if collapsed {
        return container.child(comment_body).into_any_element();
    }

    for (comment, markdown_entity, suggestions) in &comments {
        let is_reply = comment.reply_to.is_some();

        let mut row = v_flex()
            .px_2()
            .py_1()
            .gap_0p5()
            .rounded_md()
            .when(is_reply, |el| {
                el.ml_4()
                    .border_l_2()
                    .border_color(colors.border_focused)
                    .pl_2()
            })
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        Label::new(comment.author.clone())
                            .size(LabelSize::XSmall)
                            .color(Color::Default),
                    )
                    .child(
                        Label::new(comment.created_at.clone())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
            .child(MarkdownElement::new(markdown_entity.clone(), style.clone()));

        for suggestion in suggestions {
            row = row.child(render_suggestion_block(suggestion, comment.id, cx));
        }

        comment_body = comment_body.child(row);
    }

    if let Some(editor) = composer {
        comment_body = comment_body.child(render_comment_composer(
            editor,
            submitting,
            "Reply",
            "Post reply",
            cx,
        ));
    } else if let Some(comment_id) = thread_id {
        comment_body = comment_body.child(
            h_flex().px_2().pb_1().child(
                Button::new(
                    SharedString::from(format!("reply_to_comment_{comment_id}")),
                    "Reply",
                )
                .style(ButtonStyle::Subtle)
                .label_size(LabelSize::XSmall)
                .on_click(move |_event, window, cx| {
                    window.dispatch_action(Box::new(ReplyToCommentThread { comment_id }), cx);
                }),
            ),
        );
    }

    container.child(comment_body).into_any_element()
}

pub fn render_new_comment_block(
    line: u32,
    editor: Entity<Editor>,
    submitting: bool,
    cx: &mut BlockContext,
) -> AnyElement {
    let colors = cx.theme().colors().clone();

    h_flex()
        .w_full()
        .max_w(cx.max_width - cx.anchor_x)
        .items_start()
        .overflow_x_hidden()
        .pr_2()
        .py_1()
        .gap_1()
        .border_t_1()
        .border_b_1()
        .border_color(colors.border)
        .bg(colors.editor_background)
        .child(
            h_flex().w(cx.anchor_x).justify_end().pr_1().child(
                Icon::new(IconName::Chat)
                    .size(IconSize::Small)
                    .color(Color::Muted),
            ),
        )
        .child(
            v_flex()
                .w_full()
                .overflow_x_hidden()
                .gap_1()
                .px_2()
                .py_1()
                .child(
                    Label::new(format!("Add comment on line {line}"))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .child(render_comment_composer(
                    editor,
                    submitting,
                    "Comment",
                    "Post comment",
                    cx,
                )),
        )
        .into_any_element()
}

fn render_comment_composer(
    editor: Entity<Editor>,
    submitting: bool,
    editor_id: &'static str,
    submit_label: &'static str,
    cx: &mut BlockContext,
) -> impl IntoElement {
    let colors = cx.theme().colors().clone();

    v_flex()
        .id(editor_id)
        .gap_1()
        .p_2()
        .rounded_md()
        .border_1()
        .border_color(colors.border)
        .bg(colors.surface_background)
        .child(editor)
        .child(
            h_flex()
                .justify_end()
                .gap_1()
                .child(
                    Button::new("cancel_inline_comment", "Cancel")
                        .style(ButtonStyle::Subtle)
                        .label_size(LabelSize::XSmall)
                        .disabled(submitting)
                        .on_click(move |_event, window, cx| {
                            window.dispatch_action(Box::new(CancelInlineCommentDraft), cx);
                        }),
                )
                .child(
                    Button::new(
                        "submit_inline_comment",
                        if submitting {
                            "Posting..."
                        } else {
                            submit_label
                        },
                    )
                    .style(ButtonStyle::Tinted(ui::TintColor::Accent))
                    .label_size(LabelSize::XSmall)
                    .disabled(submitting)
                    .on_click(move |_event, window, cx| {
                        window.dispatch_action(Box::new(SubmitInlineCommentDraft), cx);
                    }),
                ),
        )
}

fn render_suggestion_block(
    suggestion: &SuggestionBlock,
    comment_id: u64,
    cx: &mut BlockContext,
) -> impl IntoElement {
    let suggestion_id = SharedString::from(format!("suggestion_{}", comment_id));
    let colors = cx.theme().colors().clone();
    let status = cx.theme().status().clone();

    v_flex()
        .id(suggestion_id)
        .mt_1()
        .p_2()
        .rounded_md()
        .bg(colors.surface_background)
        .border_l_2()
        .border_color(status.created)
        .child(
            h_flex()
                .justify_between()
                .items_center()
                .child(
                    Label::new("Suggested change")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .child(
                    Button::new(
                        SharedString::from(format!("apply_suggestion_{}", comment_id)),
                        "Apply",
                    )
                    .start_icon(Icon::new(IconName::Check).size(ui::IconSize::XSmall))
                    .style(ButtonStyle::Tinted(ui::TintColor::Accent))
                    .label_size(LabelSize::XSmall)
                    .on_click({
                        move |_event, window: &mut gpui::Window, cx: &mut gpui::App| {
                            window.dispatch_action(Box::new(ApplySuggestion { comment_id }), cx);
                        }
                    }),
                ),
        )
        .child(
            v_flex()
                .mt_1()
                .p_1()
                .rounded_sm()
                .bg(colors.editor_background)
                .child(
                    Label::new(suggestion.suggested_code.clone())
                        .size(LabelSize::XSmall)
                        .color(Color::Default)
                        .buffer_font(cx.app),
                ),
        )
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, schemars::JsonSchema, gpui::Action)]
pub struct ApplySuggestion {
    pub comment_id: u64,
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, schemars::JsonSchema, gpui::Action)]
pub struct ToggleCommentThread {
    pub comment_id: u64,
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, schemars::JsonSchema, gpui::Action)]
pub struct ReplyToCommentThread {
    pub comment_id: u64,
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, schemars::JsonSchema, gpui::Action)]
pub struct AddCommentAtCursor;

#[derive(Clone, Debug, PartialEq, serde::Deserialize, schemars::JsonSchema, gpui::Action)]
pub struct SubmitInlineCommentDraft;

#[derive(Clone, Debug, PartialEq, serde::Deserialize, schemars::JsonSchema, gpui::Action)]
pub struct CancelInlineCommentDraft;
