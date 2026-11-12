use crate::branch_diff::BranchDiff;
use git::{PullRequestDetails, PullRequestState};
use gpui::{
    App, Entity, EventEmitter, FocusHandle, Focusable, ScrollHandle, SharedString, WeakEntity,
};
use markdown::{Markdown, MarkdownElement, MarkdownFont, MarkdownStyle};
use project::git_store::Repository;
use ui::{Tooltip, WithScrollbar, prelude::*};
use util::truncate_and_trailoff;
use workspace::{
    Workspace,
    item::{Item, ItemEvent},
};

pub struct PullRequestView {
    details: PullRequestDetails,
    /// The remote the pull request was found on, used to diff against the
    /// remote-tracking base branch rather than a possibly stale local one.
    remote_name: Option<SharedString>,
    repository: WeakEntity<Repository>,
    workspace: WeakEntity<Workspace>,
    body: Entity<Markdown>,
    focus_handle: FocusHandle,
    scroll_handle: ScrollHandle,
}

impl PullRequestView {
    pub fn open(
        details: PullRequestDetails,
        remote_name: Option<SharedString>,
        repository: WeakEntity<Repository>,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let existing = workspace
            .items_of_type::<Self>(cx)
            .find(|item| item.read(cx).details.url == details.url);
        if let Some(existing) = existing {
            existing.update(cx, |view, cx| view.set_details(details, cx));
            workspace.activate_item(&existing, true, true, window, cx);
            return;
        }

        let language_registry = workspace.project().read(cx).languages().clone();
        let workspace_handle = workspace.weak_handle();
        let view = cx.new(|cx| {
            let body =
                cx.new(|cx| Markdown::new(details.body.clone(), Some(language_registry), None, cx));
            Self {
                details,
                remote_name,
                repository,
                workspace: workspace_handle,
                body,
                focus_handle: cx.focus_handle(),
                scroll_handle: ScrollHandle::new(),
            }
        });
        workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
    }

    fn set_details(&mut self, details: PullRequestDetails, cx: &mut Context<Self>) {
        if details.body != self.details.body {
            self.body
                .update(cx, |body, cx| body.replace(details.body.clone(), cx));
        }
        self.details = details;
        cx.emit(ItemEvent::UpdateTab);
        cx.notify();
    }

    fn view_changes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repository) = self.repository.upgrade() else {
            return;
        };
        let base_ref: SharedString = match &self.remote_name {
            Some(remote_name) => format!("{remote_name}/{}", self.details.base_branch).into(),
            None => self.details.base_branch.clone(),
        };
        self.workspace
            .update(cx, |workspace, cx| {
                let project = workspace.project().clone();
                BranchDiff::deploy_branch_diff_with_base_ref(
                    workspace, project, repository, base_ref, None, None, window, cx,
                );
            })
            .ok();
    }
}

pub(crate) fn pull_request_state_label(state: PullRequestState) -> (&'static str, Color) {
    match state {
        PullRequestState::Open => ("Open", Color::Success),
        PullRequestState::Draft => ("Draft", Color::Muted),
        PullRequestState::Closed => ("Closed", Color::Error),
        PullRequestState::Merged => ("Merged", Color::Accent),
    }
}

impl EventEmitter<ItemEvent> for PullRequestView {}

impl Focusable for PullRequestView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for PullRequestView {
    type Event = ItemEvent;

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event)
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::PullRequest).color(Color::Muted))
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        format!(
            "#{} {}",
            self.details.number,
            truncate_and_trailoff(&self.details.title, 30)
        )
        .into()
    }

    fn tab_tooltip_text(&self, _: &App) -> Option<SharedString> {
        Some(self.details.title.clone())
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("Git Pull Request Preview Opened")
    }

    fn show_toolbar(&self) -> bool {
        false
    }
}

impl Render for PullRequestView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let details = &self.details;
        let (state_label, state_color) = pull_request_state_label(details.state);
        let url = details.url.clone();

        let meta = h_flex()
            .gap_1p5()
            .flex_wrap()
            .child(
                h_flex()
                    .gap_1()
                    .px_1p5()
                    .rounded_sm()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        Icon::new(IconName::PullRequest)
                            .size(IconSize::Small)
                            .color(state_color),
                    )
                    .child(
                        Label::new(state_label)
                            .size(LabelSize::Small)
                            .color(state_color),
                    ),
            )
            .when_some(details.author.clone(), |this, author| {
                this.child(Label::new(author).size(LabelSize::Small))
            })
            .child(
                Label::new(format!(
                    "wants to merge {} into {}",
                    details.head_branch, details.base_branch
                ))
                .size(LabelSize::Small)
                .color(Color::Muted),
            );

        let actions = h_flex()
            .gap_1()
            .child(
                Button::new("view-pull-request-changes", "View Changes")
                    .start_icon(Icon::new(IconName::Diff).size(IconSize::Small))
                    .tooltip(Tooltip::text(
                        "Diff this branch against the pull request's base",
                    ))
                    .on_click(cx.listener(|this, _, window, cx| this.view_changes(window, cx))),
            )
            .child(
                Button::new("open-pull-request-in-browser", "Open in Browser")
                    .end_icon(Icon::new(IconName::ArrowUpRight).size(IconSize::Small))
                    .on_click(move |_, _, cx| cx.open_url(url.as_str())),
            );

        let body = if details.body.trim().is_empty() {
            Label::new("No description provided.")
                .color(Color::Muted)
                .into_any_element()
        } else {
            MarkdownElement::new(
                self.body.clone(),
                MarkdownStyle::themed(MarkdownFont::Preview, window, cx),
            )
            .into_any_element()
        };

        div()
            .key_context("PullRequestView")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(
                v_flex()
                    .id("pull-request-view")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll_handle)
                    .p_6()
                    .gap_3()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(Headline::new(details.title.clone()))
                            .child(
                                Headline::new(format!("#{}", details.number)).color(Color::Muted),
                            ),
                    )
                    .child(
                        h_flex()
                            .justify_between()
                            .flex_wrap()
                            .gap_2()
                            .child(meta)
                            .child(actions),
                    )
                    .child(ui::Divider::horizontal())
                    .child(body),
            )
            .vertical_scrollbar_for(&self.scroll_handle, window, cx)
    }
}
