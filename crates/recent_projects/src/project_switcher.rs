use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::Result;
use fs::Fs;
use fuzzy_nucleo::{
    MaxResults, StringMatch, StringMatchCandidate, StringMatchOptions, match_strings,
};
use futures::StreamExt;
use gpui::{
    App, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle, Focusable, Pixels, Task,
    TaskExt, WeakEntity, Window, actions, px,
};
use picker::{Picker, PickerDelegate};
use settings::{RegisterSetting, Settings, SettingsStore};
use ui::{HighlightedLabel, ListItem, ListItemSpacing, prelude::*};
use util::ResultExt;
use workspace::{
    MultiWorkspace, OpenMode, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

actions!(
    project_switcher,
    [
        /// Toggles focus on the project switcher panel.
        ToggleFocus
    ]
);

const PROJECT_SWITCHER_PANEL_KEY: &str = "ProjectSwitcherPanel";

#[derive(RegisterSetting)]
pub struct ProjectSwitcherSettings {
    pub folder: Option<PathBuf>,
}

impl Settings for ProjectSwitcherSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        Self {
            folder: content
                .project_switcher
                .as_ref()
                .and_then(|settings| settings.folder.as_deref())
                .map(expand_home),
        }
    }
}

fn expand_home(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(relative) => util::paths::home_dir().join(relative),
        None if path == "~" => util::paths::home_dir().clone(),
        None => PathBuf::from(path),
    }
}

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<ProjectSwitcherPanel>(window, cx);
        });
    })
    .detach();
}

struct ProjectEntry {
    name: String,
    path: PathBuf,
}

pub struct ProjectSwitcherPanel {
    picker: Entity<Picker<ProjectSwitcherDelegate>>,
    fs: Arc<dyn Fs>,
    workspace: WeakEntity<Workspace>,
    reload_task: Task<()>,
    _settings_subscription: gpui::Subscription,
}

impl ProjectSwitcherPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, window, cx| {
            let fs = workspace.app_state().fs.clone();
            let workspace = cx.weak_entity();
            cx.new(|cx| Self::new(fs, workspace, window, cx))
        })
    }

    fn new(
        fs: Arc<dyn Fs>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let delegate = ProjectSwitcherDelegate {
            entries: Vec::new(),
            matches: Vec::new(),
            selected_index: 0,
            folder_configured: false,
        };
        let picker = cx.new(|cx| Picker::uniform_list(delegate, window, cx));

        let mut last_folder = ProjectSwitcherSettings::get_global(cx).folder.clone();
        let _settings_subscription =
            cx.observe_global_in::<SettingsStore>(window, move |this, window, cx| {
                let folder = ProjectSwitcherSettings::get_global(cx).folder.clone();
                if folder != last_folder {
                    last_folder = folder;
                    this.reload(window, cx);
                }
            });

        let mut this = Self {
            picker,
            fs,
            workspace,
            reload_task: Task::ready(()),
            _settings_subscription,
        };
        this.reload(window, cx);
        this
    }

    fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let folder = ProjectSwitcherSettings::get_global(cx).folder.clone();
        let fs = self.fs.clone();
        let workspace = self.workspace.clone();
        self.reload_task = cx.spawn_in(window, async move |this, cx| {
            let entries = match &folder {
                Some(folder) => list_projects(fs.as_ref(), folder)
                    .await
                    .log_err()
                    .unwrap_or_default(),
                None => Vec::new(),
            };
            let current_project = workspace
                .read_with(cx, |workspace, cx| {
                    let project = workspace.project().read(cx);
                    let worktree = project.visible_worktrees(cx).next()?;
                    Some(worktree.read(cx).abs_path())
                })
                .ok()
                .flatten();
            this.update_in(cx, |this, window, cx| {
                let folder_configured = folder.is_some();
                this.picker.update(cx, |picker, cx| {
                    picker.delegate.entries = entries;
                    picker.delegate.folder_configured = folder_configured;
                    picker.refresh(window, cx);
                    let current_index = current_project.and_then(|current| {
                        picker.delegate.matches.iter().position(|hit| {
                            picker
                                .delegate
                                .entries
                                .get(hit.candidate_id)
                                .is_some_and(|entry| entry.path.as_path() == current.as_ref())
                        })
                    });
                    if let Some(index) = current_index {
                        picker.set_selected_index(index, None, true, window, cx);
                    }
                });
            })
            .log_err();
        });
    }
}

async fn list_projects(fs: &dyn Fs, folder: &Path) -> Result<Vec<ProjectEntry>> {
    let mut children = fs.read_dir(folder).await?;
    let mut entries = Vec::new();
    while let Some(child) = children.next().await {
        let path = child?;
        let Some(name) = path.file_name().map(|name| name.to_string_lossy().into_owned()) else {
            continue;
        };
        if name.starts_with('.') || !fs.is_dir(&path).await {
            continue;
        }
        entries.push(ProjectEntry { name, path });
    }
    entries.sort_by_key(|entry| entry.name.to_lowercase());
    Ok(entries)
}

impl EventEmitter<PanelEvent> for ProjectSwitcherPanel {}

impl Focusable for ProjectSwitcherPanel {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for ProjectSwitcherPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("ProjectSwitcherPanel")
            .size_full()
            .child(self.picker.clone())
    }
}

impl Panel for ProjectSwitcherPanel {
    fn persistent_name() -> &'static str {
        "Project Switcher Panel"
    }

    fn panel_key() -> &'static str {
        PROJECT_SWITCHER_PANEL_KEY
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        DockPosition::Left
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        position == DockPosition::Left
    }

    fn set_position(&mut self, _: DockPosition, _: &mut Window, _: &mut Context<Self>) {}

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(240.)
    }

    fn set_active(&mut self, active: bool, window: &mut Window, cx: &mut Context<Self>) {
        if active {
            self.reload(window, cx);
        }
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<ui::IconName> {
        Some(ui::IconName::FolderOpen)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Project Switcher")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        8
    }
}

struct ProjectSwitcherDelegate {
    entries: Vec<ProjectEntry>,
    matches: Vec<StringMatch>,
    selected_index: usize,
    folder_configured: bool,
}

impl PickerDelegate for ProjectSwitcherDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "project switcher"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Search projects…".into()
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
    }

    fn update_matches(
        &mut self,
        query: String,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let query = query.trim();
        if query.is_empty() {
            self.matches = self
                .entries
                .iter()
                .enumerate()
                .map(|(id, entry)| StringMatch {
                    candidate_id: id,
                    score: 0.0,
                    positions: Vec::new(),
                    string: entry.name.clone().into(),
                })
                .collect();
        } else {
            let candidates: Vec<_> = self
                .entries
                .iter()
                .enumerate()
                .map(|(id, entry)| StringMatchCandidate::new(id, &entry.name))
                .collect();
            self.matches = match_strings(
                &candidates,
                query,
                StringMatchOptions {
                    max_results: MaxResults::limit::<100>(),
                    ..Default::default()
                },
            );
        }
        self.selected_index = 0;
        Task::ready(())
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(entry) = self
            .matches
            .get(self.selected_index)
            .and_then(|hit| self.entries.get(hit.candidate_id))
        else {
            return;
        };
        let Some(handle) = window.window_handle().downcast::<MultiWorkspace>() else {
            return;
        };
        let paths = vec![entry.path.clone()];
        cx.defer(move |cx| {
            if let Some(task) = handle
                .update(cx, |multi_workspace, window, cx| {
                    multi_workspace.open_project(paths, OpenMode::Activate, window, cx)
                })
                .log_err()
            {
                task.detach_and_log_err(cx);
            }
        });
    }

    fn dismissed(&mut self, _window: &mut Window, _cx: &mut Context<Picker<Self>>) {}

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        Some(if self.folder_configured {
            "No projects found".into()
        } else {
            "Set `project_switcher.folder` in settings".into()
        })
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let hit = self.matches.get(ix)?;
        let entry = self.entries.get(hit.candidate_id)?;
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(HighlightedLabel::new(
                    entry.name.clone(),
                    hit.positions.clone(),
                )),
        )
    }
}
