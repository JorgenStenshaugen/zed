use buffer_diff::BufferDiff;
use editor::{Editor, EditorEvent, EditorSettings, MultiBuffer, SplittableEditor};
use futures::channel::oneshot;
use gpui::{
    App, AppContext as _, AsyncApp, Context, Entity, EntityId, EventEmitter, FocusHandle,
    Focusable, IntoElement, Render, Task, WeakEntity, Window,
};
use language::{Buffer, Capability, Point};
use settings::Settings as _;
use std::{any::TypeId, path::Path, sync::Arc};
use ui::{Button, ButtonStyle, Color, Icon, IconName, Label, SharedString, prelude::*};
use util::ResultExt as _;
use workspace::{Item, SaveIntent, Workspace, item::ItemEvent};

pub enum DiffDecision {
    /// Carries the final text, which the user may have edited before accepting.
    Accepted(String),
    Rejected,
}

/// Shows a change proposed by Claude Code next to the current file contents and
/// reports whether the user accepted it. Dropping the view counts as a rejection.
pub struct ClaudeDiffView {
    diff_editor: Entity<SplittableEditor>,
    diff: Entity<BufferDiff>,
    base: Entity<Buffer>,
    proposed: Entity<Buffer>,
    workspace: WeakEntity<Workspace>,
    title: SharedString,
    path: SharedString,
    decision: Option<oneshot::Sender<DiffDecision>>,
    _update_diff_task: Task<()>,
}

impl ClaudeDiffView {
    pub fn open(
        workspace: &Entity<Workspace>,
        file_path: &Path,
        old_text: String,
        new_text: String,
        window: &mut Window,
        cx: &mut App,
    ) -> (Entity<Self>, oneshot::Receiver<DiffDecision>) {
        let project = workspace.read(cx).project().clone();
        let language_registry = project.read(cx).languages().clone();

        let base = cx.new(|cx| {
            let mut buffer = Buffer::local(old_text, cx);
            buffer.set_capability(Capability::ReadOnly, cx);
            buffer
        });
        let proposed = cx.new(|cx| Buffer::local(new_text, cx));
        cx.spawn({
            let base = base.clone();
            let proposed = proposed.clone();
            let file_path = file_path.to_path_buf();
            async move |cx| {
                let Some(language) = language_registry
                    .load_language_for_file_path(&file_path)
                    .await
                    .log_err()
                else {
                    return;
                };
                for buffer in [base, proposed] {
                    buffer.update(cx, |buffer, cx| buffer.set_language(Some(language.clone()), cx));
                }
            }
        })
        .detach();

        let proposed_snapshot = proposed.read(cx).snapshot();
        let diff = cx.new(|cx| {
            BufferDiff::new_with_base_text_buffer(&proposed_snapshot.text, base.clone(), cx)
        });
        let multibuffer = cx.new(|cx| {
            let mut multibuffer = MultiBuffer::new(Capability::ReadWrite);
            multibuffer.set_excerpts_for_buffer(
                proposed.clone(),
                [Point::zero()..proposed_snapshot.max_point()],
                0,
                cx,
            );
            multibuffer.add_diff(diff.clone(), cx);
            multibuffer
        });

        let (sender, receiver) = oneshot::channel();
        let file_name = file_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let view = cx.new(|cx| {
            let diff_editor = cx.new(|cx| {
                SplittableEditor::new(
                    EditorSettings::get_global(cx).diff_view_style,
                    multibuffer,
                    project,
                    workspace.clone(),
                    window,
                    cx,
                )
            });
            cx.subscribe(&proposed, |this: &mut Self, _, event, cx| {
                if matches!(event, language::BufferEvent::Edited { .. }) {
                    this.refresh_diff(cx);
                }
            })
            .detach();
            let mut this = Self {
                diff_editor,
                diff,
                base,
                proposed,
                workspace: workspace.downgrade(),
                title: format!("Claude: {file_name}").into(),
                path: file_path.to_string_lossy().into_owned().into(),
                decision: Some(sender),
                _update_diff_task: Task::ready(()),
            };
            this.refresh_diff(cx);
            this
        });
        (view, receiver)
    }

    fn refresh_diff(&mut self, cx: &mut Context<Self>) {
        let diff = self.diff.clone();
        let base = self.base.clone();
        let proposed = self.proposed.clone();
        // Replacing the task cancels a refresh that an earlier edit started.
        self._update_diff_task = cx.spawn(async move |_, cx| {
            update_diff_buffer(&diff, &proposed, &base, cx).await;
        });
    }

    fn decide(&mut self, accepted: bool, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(sender) = self.decision.take() {
            let decision = if accepted {
                DiffDecision::Accepted(self.proposed.read(cx).text())
            } else {
                DiffDecision::Rejected
            };
            sender.send(decision).ok();
        }
        let item_id = cx.entity_id();
        let workspace = self.workspace.clone();
        // Closing synchronously would re-enter this view while it is being updated.
        cx.spawn_in(window, async move |_, cx| {
            workspace
                .update_in(cx, |workspace, window, cx| {
                    close_item(workspace, item_id, window, cx)
                })
                .log_err();
        })
        .detach();
    }
}

pub fn close_item(
    workspace: &mut Workspace,
    item_id: EntityId,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if let Some(pane) = workspace.pane_for_item_id(item_id) {
        pane.update(cx, |pane, cx| {
            pane.close_item_by_id(item_id, SaveIntent::Skip, window, cx)
                .detach_and_log_err(cx)
        });
    }
}

async fn update_diff_buffer(
    diff: &Entity<BufferDiff>,
    proposed: &Entity<Buffer>,
    base: &Entity<Buffer>,
    cx: &mut AsyncApp,
) {
    let proposed_snapshot = proposed.read_with(cx, |buffer, _| buffer.snapshot());
    let base_snapshot = base.read_with(cx, |buffer, _| buffer.snapshot());
    let base_text = Arc::<str>::from(base_snapshot.text());
    let update = diff
        .update(cx, |diff, cx| {
            diff.update_diff(
                proposed_snapshot.text.clone(),
                &base_snapshot,
                Some(base_text),
                cx,
            )
        })
        .await;
    diff.update(cx, |diff, cx| diff.set_snapshot(update, cx));
}

impl EventEmitter<EditorEvent> for ClaudeDiffView {}

impl Focusable for ClaudeDiffView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.diff_editor.focus_handle(cx)
    }
}

impl Item for ClaudeDiffView {
    type Event = EditorEvent;

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Diff).color(Color::Muted))
    }

    fn tab_content_text(&self, _detail: usize, _: &App) -> SharedString {
        self.title.clone()
    }

    fn tab_tooltip_text(&self, _: &App) -> Option<SharedString> {
        Some(self.path.clone())
    }

    fn to_item_events(event: &EditorEvent, f: &mut dyn FnMut(ItemEvent)) {
        Editor::to_item_events(event, f)
    }

    fn deactivated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.diff_editor
            .update(cx, |editor, cx| editor.deactivated(window, cx));
    }

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a Entity<Self>,
        cx: &'a App,
    ) -> Option<gpui::AnyEntity> {
        if type_id == TypeId::of::<Self>() {
            Some(self_handle.clone().into())
        } else if type_id == TypeId::of::<SplittableEditor>() {
            Some(self.diff_editor.clone().into())
        } else if type_id == TypeId::of::<Editor>() {
            Some(self.diff_editor.read(cx).rhs_editor().clone().into())
        } else {
            None
        }
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.diff_editor.update(cx, |editor, cx| {
            editor.added_to_workspace(workspace, window, cx)
        });
    }
}

impl Render for ClaudeDiffView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_2()
                    .justify_between()
                    .child(Label::new(self.path.clone()).color(Color::Muted))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(Button::new("reject", "Reject").on_click(cx.listener(
                                |this, _, window, cx| this.decide(false, window, cx),
                            )))
                            .child(
                                Button::new("accept", "Accept")
                                    .style(ButtonStyle::Filled)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.decide(true, window, cx)
                                    })),
                            ),
                    ),
            )
            .child(div().flex_1().child(self.diff_editor.clone()))
    }
}
