use crate::{
    ClaudeIdeServer, ToolHost, ToolOutput,
    diff_view::{ClaudeDiffView, DiffDecision, close_item},
};
use anyhow::{Context as _, Result, anyhow};
use editor::{Editor, EditorEvent};
use futures::{FutureExt as _, future::Shared};
use gpui::{
    AnyWindowHandle, App, AppContext as _, AsyncApp, Context, Entity, Subscription, Task,
    WeakEntity, Window,
};
use language::Buffer;
use serde_json::{Value, json};
use std::{collections::HashMap, path::PathBuf, rc::Rc};
use text::PointUtf16;
use util::ResultExt as _;
use workspace::{OpenOptions, Workspace};

type TerminalEnv = Vec<(String, String)>;

/// Owns the IDE server for one workspace and forwards the active editor's
/// selection to Claude Code, mirroring what the VS Code extension does.
pub struct ClaudeIde {
    server: Option<ClaudeIdeServer>,
    ready: Shared<Task<TerminalEnv>>,
    latest_selection: Option<Value>,
    diffs: HashMap<String, WeakEntity<ClaudeDiffView>>,
    editor_subscription: Option<Subscription>,
    _workspace_subscription: Subscription,
}

impl ClaudeIde {
    pub fn new(workspace: &Entity<Workspace>, window: &Window, cx: &mut App) -> Entity<Self> {
        let window_handle = window.window_handle();
        cx.new(|cx| {
            let host = Rc::new(WorkspaceToolHost {
                ide: cx.weak_entity(),
                workspace: workspace.downgrade(),
                window: window_handle,
            });
            // Workspace reads happen inside the spawned task: the panel is often
            // created while the workspace is mid-update, where reading it would panic.
            let ready = cx
                .spawn({
                    let workspace = workspace.clone();
                    async move |this, cx| {
                        let folders = workspace
                            .read_with(cx, |workspace, cx| workspace_folders(workspace, cx));
                        let server = ClaudeIdeServer::start(folders, "Zed", host, cx)
                            .await
                            .log_err();
                        let env = server
                            .as_ref()
                            .map(|server| server.terminal_env().to_vec())
                            .unwrap_or_default();
                        this.update(cx, |this, cx| {
                            this.server = server;
                            this.track_active_editor(&workspace, cx);
                        })
                        .log_err();
                        env
                    }
                })
                .shared();

            let workspace_subscription =
                cx.subscribe(workspace, |this, workspace, event: &workspace::Event, cx| {
                    if matches!(event, workspace::Event::ActiveItemChanged) {
                        this.track_active_editor(&workspace, cx);
                    }
                });

            Self {
                server: None,
                ready,
                latest_selection: None,
                diffs: HashMap::new(),
                editor_subscription: None,
                _workspace_subscription: workspace_subscription,
            }
        })
    }

    /// Environment that makes a terminal's `claude` connect to this server.
    /// Empty if the server could not start. Resolves once the server is listening.
    pub fn terminal_env(&self) -> impl std::future::Future<Output = TerminalEnv> + use<> {
        self.ready.clone()
    }

    /// Whether a Claude Code session is attached and would receive a mention.
    pub fn is_connected(&self) -> bool {
        self.server.as_ref().is_some_and(ClaudeIdeServer::has_clients)
    }

    /// Inserts `@file#Lstart-end` into Claude's prompt. Rows are zero-based.
    pub fn mention(&self, path: &std::path::Path, start_row: u32, end_row: u32) {
        if let Some(server) = &self.server {
            server.notify(
                "at_mentioned",
                json!({ "filePath": path, "lineStart": start_row, "lineEnd": end_row }),
            );
        }
    }

    fn track_active_editor(&mut self, workspace: &Entity<Workspace>, cx: &mut Context<Self>) {
        let Some(editor) = workspace.read(cx).active_item_as::<Editor>(cx) else {
            self.editor_subscription = None;
            return;
        };
        self.editor_subscription = Some(cx.subscribe(&editor, |this, editor, event, cx| {
            if matches!(event, EditorEvent::SelectionsChanged { .. }) {
                this.selection_changed(&editor, cx);
            }
        }));
        self.selection_changed(&editor, cx);
    }

    fn selection_changed(&mut self, editor: &Entity<Editor>, cx: &mut Context<Self>) {
        let Some(selection) = selection_for_editor(editor, cx) else {
            return;
        };
        if let Some(server) = &self.server {
            let mut params = selection.clone();
            params.as_object_mut().map(|params| params.remove("success"));
            server.notify("selection_changed", params);
        }
        self.latest_selection = Some(selection);
    }
}

struct WorkspaceToolHost {
    ide: WeakEntity<ClaudeIde>,
    workspace: WeakEntity<Workspace>,
    window: AnyWindowHandle,
}

impl ToolHost for WorkspaceToolHost {
    fn tools(&self) -> Vec<Value> {
        let no_input = json!({ "type": "object", "properties": {}, "additionalProperties": false });
        vec![
            json!({
                "name": "getWorkspaceFolders",
                "description": "Get all workspace folders currently open in the IDE",
                "inputSchema": no_input,
            }),
            json!({
                "name": "getOpenEditors",
                "description": "Get information about currently open editors",
                "inputSchema": no_input,
            }),
            json!({
                "name": "getCurrentSelection",
                "description": "Get the current text selection in the active editor",
                "inputSchema": no_input,
            }),
            json!({
                "name": "getLatestSelection",
                "description": "Get the most recent text selection, even if the editor is not active",
                "inputSchema": no_input,
            }),
            json!({
                "name": "getDiagnostics",
                "description": "Get language diagnostics from the IDE for open files",
                "inputSchema": {
                    "type": "object",
                    "properties": { "uri": { "type": "string", "description": "Optional file URI to get diagnostics for. All open files if omitted." } },
                    "additionalProperties": false,
                },
            }),
            json!({
                "name": "openFile",
                "description": "Open a file in the editor",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "filePath": { "type": "string", "description": "Path to the file to open" },
                    },
                    "required": ["filePath"],
                    "additionalProperties": false,
                },
            }),
            json!({
                "name": "openDiff",
                "description": "Open a diff of a proposed file change and wait for the user to accept or reject it",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "old_file_path": { "type": "string" },
                        "new_file_path": { "type": "string" },
                        "new_file_contents": { "type": "string" },
                        "tab_name": { "type": "string" },
                    },
                    "required": ["old_file_path", "new_file_path", "new_file_contents", "tab_name"],
                    "additionalProperties": false,
                },
            }),
            json!({
                "name": "close_tab",
                "description": "Close a diff tab opened by openDiff",
                "inputSchema": {
                    "type": "object",
                    "properties": { "tab_name": { "type": "string" } },
                    "required": ["tab_name"],
                    "additionalProperties": false,
                },
            }),
            json!({
                "name": "closeAllDiffTabs",
                "description": "Close all diff tabs opened by openDiff",
                "inputSchema": no_input,
            }),
        ]
    }

    fn call_tool(
        &self,
        name: &str,
        arguments: Value,
        cx: &mut AsyncApp,
    ) -> Task<Result<ToolOutput>> {
        let Some(workspace) = self.workspace.upgrade() else {
            return Task::ready(Err(anyhow!("the workspace was closed")));
        };
        match name {
            "openDiff" => self.open_diff(workspace, arguments, cx),
            "close_tab" => {
                let tab_name = arguments
                    .get("tab_name")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                self.close_diffs(tab_name, cx)
            }
            "closeAllDiffTabs" => self.close_diffs(None, cx),
            _ => {
                let task = self.call_json_tool(name, arguments, workspace, cx);
                cx.spawn(async move |_| Ok(ToolOutput::Json(task.await?)))
            }
        }
    }
}

impl WorkspaceToolHost {
    fn open_diff(
        &self,
        workspace: Entity<Workspace>,
        arguments: Value,
        cx: &mut AsyncApp,
    ) -> Task<Result<ToolOutput>> {
        let argument = |key: &str| arguments.get(key).and_then(Value::as_str).map(str::to_string);
        let (Some(old_path), Some(new_path), Some(new_contents), Some(tab_name)) = (
            argument("old_file_path"),
            argument("new_file_path"),
            argument("new_file_contents"),
            argument("tab_name"),
        ) else {
            return Task::ready(Err(anyhow!(
                "old_file_path, new_file_path, new_file_contents and tab_name are required"
            )));
        };
        let ide = self.ide.clone();
        let window = self.window;
        cx.spawn(async move |cx| {
            let old_text = cx
                .background_spawn(async move {
                    match std::fs::read_to_string(&old_path) {
                        Ok(text) => Ok(text),
                        // A new file has no previous contents to compare against.
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            Ok(String::new())
                        }
                        Err(error) => Err(error).with_context(|| format!("failed to read {old_path}")),
                    }
                })
                .await?;
            let new_path = PathBuf::from(new_path);
            let (view, decision) = window.update(cx, {
                let new_path = new_path.clone();
                move |_, window, cx| {
                    let (view, decision) = ClaudeDiffView::open(
                        &workspace, &new_path, old_text, new_contents, window, cx,
                    );
                    workspace.update(cx, |workspace, cx| {
                        workspace.add_item_to_active_pane(Box::new(view.clone()), None, true, window, cx)
                    });
                    (view, decision)
                }
            })?;
            ide.update(cx, |ide, _| {
                ide.diffs.insert(tab_name.clone(), view.downgrade());
            })?;
            let decision = decision.await;
            ide.update(cx, |ide, _| ide.diffs.remove(&tab_name)).log_err();

            match decision {
                Ok(DiffDecision::Accepted(text)) => {
                    cx.background_spawn({
                        let text = text.clone();
                        async move {
                            if let Some(parent) = new_path.parent() {
                                std::fs::create_dir_all(parent)?;
                            }
                            std::fs::write(&new_path, text)
                                .with_context(|| format!("failed to write {}", new_path.display()))
                        }
                    })
                    .await?;
                    Ok(ToolOutput::Blocks(vec!["FILE_SAVED".to_string(), text]))
                }
                // A dropped sender means the user closed the tab without choosing.
                Ok(DiffDecision::Rejected) | Err(_) => {
                    Ok(ToolOutput::Blocks(vec!["DIFF_REJECTED".to_string(), tab_name]))
                }
            }
        })
    }

    /// Closes the named diff tab, or every diff tab when `tab_name` is `None`.
    fn close_diffs(&self, tab_name: Option<String>, cx: &mut AsyncApp) -> Task<Result<ToolOutput>> {
        let views = self
            .ide
            .update(cx, |ide, _| match &tab_name {
                Some(name) => ide.diffs.get(name).cloned().into_iter().collect::<Vec<_>>(),
                None => ide.diffs.values().cloned().collect(),
            });
        let views = match views {
            Ok(views) => views,
            Err(error) => return Task::ready(Err(error)),
        };
        let workspace = self.workspace.clone();
        let closed = self.window.update(cx, move |_, window, cx| {
            let mut closed = 0;
            for view in views.iter().filter_map(WeakEntity::upgrade) {
                workspace
                    .update(cx, |workspace, cx| {
                        close_item(workspace, view.entity_id(), window, cx)
                    })
                    .log_err();
                closed += 1;
            }
            closed
        });
        Task::ready(closed.map(|closed| {
            ToolOutput::Blocks(vec![match tab_name {
                Some(_) => "TAB_CLOSED".to_string(),
                None => format!("CLOSED_{closed}_DIFF_TABS"),
            }])
        }))
    }

    fn call_json_tool(
        &self,
        name: &str,
        arguments: Value,
        workspace: Entity<Workspace>,
        cx: &mut AsyncApp,
    ) -> Task<Result<Value>> {
        match name {
            "getWorkspaceFolders" => Task::ready(cx.update(|cx| {
                let folders = workspace_folders(workspace.read(cx), cx);
                let entries = folders
                    .iter()
                    .map(|path| {
                        json!({
                            "name": path.file_name().map(|name| name.to_string_lossy().into_owned()),
                            "uri": file_url(path),
                            "path": path,
                        })
                    })
                    .collect::<Vec<_>>();
                Ok(json!({
                    "success": true,
                    "folders": entries,
                    "rootPath": folders.first(),
                }))
            })),
            "getOpenEditors" => Task::ready(cx.update(|cx| {
                let workspace = workspace.read(cx);
                let active = workspace.active_item_as::<Editor>(cx);
                let tabs = workspace
                    .items_of_type::<Editor>(cx)
                    .filter_map(|editor| {
                        let buffer = editor.read(cx).buffer().read(cx).as_singleton()?;
                        let path = buffer_path(&buffer, cx)?;
                        let buffer = buffer.read(cx);
                        Some(json!({
                            "uri": file_url(&path),
                            "isActive": active.as_ref() == Some(&editor),
                            "label": path.file_name().map(|name| name.to_string_lossy().into_owned()),
                            "languageId": buffer.language().map(|language| language.name().as_ref().to_lowercase()),
                            "isDirty": buffer.is_dirty(),
                        }))
                    })
                    .collect::<Vec<_>>();
                Ok(json!({ "tabs": tabs }))
            })),
            "getCurrentSelection" => Task::ready(cx.update(|cx| {
                let selection = workspace
                    .read(cx)
                    .active_item_as::<Editor>(cx)
                    .and_then(|editor| selection_for_editor(&editor, cx));
                Ok(selection.unwrap_or_else(
                    || json!({ "success": false, "message": "No active editor found" }),
                ))
            })),
            "getLatestSelection" => Task::ready(
                self.ide
                    .read_with(cx, |ide, _| {
                        ide.latest_selection.clone().unwrap_or_else(
                            || json!({ "success": false, "message": "No selection available" }),
                        )
                    })
                    .context("the IDE integration was shut down"),
            ),
            "getDiagnostics" => {
                let uri = arguments
                    .get("uri")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                Task::ready(cx.update(|cx| Ok(diagnostics(&workspace, uri.as_deref(), cx))))
            }
            "openFile" => {
                let Some(path) = arguments.get("filePath").and_then(Value::as_str) else {
                    return Task::ready(Err(anyhow!("filePath is required")));
                };
                let path = PathBuf::from(path);
                let open = self.window.update(cx, {
                    let path = path.clone();
                    move |_, window, cx| {
                        workspace.update(cx, |workspace, cx| {
                            workspace.open_abs_path(path, OpenOptions::default(), window, cx)
                        })
                    }
                });
                match open {
                    Ok(open) => cx.spawn(async move |_| {
                        open.await?;
                        Ok(Value::String(format!("Opened file: {}", path.display())))
                    }),
                    Err(error) => Task::ready(Err(error)),
                }
            }
            other => Task::ready(Err(anyhow!("unknown tool: {other}"))),
        }
    }
}

fn workspace_folders(workspace: &Workspace, cx: &App) -> Vec<PathBuf> {
    workspace
        .visible_worktrees(cx)
        .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
        .collect()
}

fn file_url(path: &std::path::Path) -> Option<String> {
    url::Url::from_file_path(path).ok().map(String::from)
}

fn buffer_path(buffer: &Entity<Buffer>, cx: &App) -> Option<PathBuf> {
    let file = buffer.read(cx).file()?.as_local()?;
    Some(file.abs_path(cx))
}

/// Positions use zero-based lines and UTF-16 columns, as the VS Code extension reports them.
fn selection_for_editor(editor: &Entity<Editor>, cx: &App) -> Option<Value> {
    let editor = editor.read(cx);
    let path = buffer_path(&editor.buffer().read(cx).as_singleton()?, cx)?;
    let snapshot = editor.buffer().read(cx).snapshot(cx);
    let selection = editor.selections.newest_anchor();
    let start: PointUtf16 = snapshot.summary_for_anchor(&selection.start);
    let end: PointUtf16 = snapshot.summary_for_anchor(&selection.end);
    let text = snapshot
        .text_for_range(selection.start..selection.end)
        .collect::<String>();
    Some(json!({
        "success": true,
        "text": text,
        "filePath": path,
        "fileUrl": file_url(&path),
        "selection": {
            "start": { "line": start.row, "character": start.column },
            "end": { "line": end.row, "character": end.column },
            "isEmpty": start == end,
        },
    }))
}

fn diagnostics(workspace: &Entity<Workspace>, uri: Option<&str>, cx: &App) -> Value {
    let wanted_path = uri
        .and_then(|uri| url::Url::parse(uri).ok())
        .and_then(|uri| uri.to_file_path().ok());
    let project = workspace.read(cx).project().clone();
    let files = project
        .read(cx)
        .opened_buffers(cx)
        .into_iter()
        .filter_map(|buffer| {
            let path = buffer_path(&buffer, cx)?;
            if wanted_path.as_ref().is_some_and(|wanted| *wanted != path) {
                return None;
            }
            let snapshot = buffer.read(cx).snapshot();
            let entries = snapshot
                .diagnostics_in_range::<_, PointUtf16>(0..snapshot.len(), false)
                .map(|entry| {
                    json!({
                        "message": entry.diagnostic.message,
                        "severity": severity_name(entry.diagnostic.severity),
                        "range": {
                            "start": { "line": entry.range.start.row, "character": entry.range.start.column },
                            "end": { "line": entry.range.end.row, "character": entry.range.end.column },
                        },
                        "source": entry.diagnostic.source,
                    })
                })
                .collect::<Vec<_>>();
            (!entries.is_empty()).then(|| json!({ "uri": file_url(&path), "diagnostics": entries }))
        })
        .collect::<Vec<_>>();
    Value::Array(files)
}

fn severity_name(severity: lsp::DiagnosticSeverity) -> &'static str {
    match severity {
        lsp::DiagnosticSeverity::ERROR => "Error",
        lsp::DiagnosticSeverity::WARNING => "Warning",
        lsp::DiagnosticSeverity::INFORMATION => "Information",
        _ => "Hint",
    }
}
