//! IDE integration server for the interactive Claude Code CLI.
//!
//! Claude Code discovers an IDE through a lock file in `~/.claude/ide/<port>.lock`
//! and the `CLAUDE_CODE_SSE_PORT` / `ENABLE_IDE_INTEGRATION` environment variables
//! of the terminal it runs in. It then opens a loopback WebSocket carrying
//! JSON-RPC (MCP) and calls the tools this server advertises. The wire details
//! here were checked against Claude Code 2.1.322; the protocol is not an
//! officially documented public interface, so expect it to change.

mod diff_view;
mod workspace_tools;

pub use workspace_tools::ClaudeIde;

use anyhow::{Context as _, Result};
use async_tungstenite::tungstenite::{
    Message,
    handshake::server::{ErrorResponse, Request, Response},
    http::{HeaderValue, StatusCode},
};
use futures::{FutureExt as _, StreamExt as _, select_biased};
use gpui::{AsyncApp, Task};
use parking_lot::Mutex;
use serde_json::{Value, json};
use smol::net::{TcpListener, TcpStream};
use std::{path::PathBuf, rc::Rc, sync::Arc};
use util::ResultExt as _;

const AUTH_HEADER: &str = "x-claude-code-ide-authorization";
const SUBPROTOCOL: &str = "mcp";
const DEFAULT_PROTOCOL_VERSION: &str = "2025-11-25";

pub enum ToolOutput {
    /// A string is sent as-is; any other value is serialized, which is how
    /// Claude Code's own IDE extension replies.
    Json(Value),
    /// One text content block per entry.
    Blocks(Vec<String>),
}

pub trait ToolHost: 'static {
    /// MCP tool definitions advertised in response to `tools/list`.
    fn tools(&self) -> Vec<Value>;

    fn call_tool(
        &self,
        name: &str,
        arguments: Value,
        cx: &mut AsyncApp,
    ) -> Task<Result<ToolOutput>>;
}

pub struct ClaudeIdeServer {
    port: u16,
    lock_path: PathBuf,
    clients: Arc<Mutex<Vec<async_channel::Sender<String>>>>,
    _accept_task: Task<()>,
}

impl ClaudeIdeServer {
    pub async fn start(
        workspace_folders: Vec<PathBuf>,
        ide_name: &str,
        host: Rc<dyn ToolHost>,
        cx: &AsyncApp,
    ) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .context("failed to bind the Claude IDE server to loopback")?;
        let port = listener.local_addr()?.port();
        let auth_token = format!("{:032x}", rand::random::<u128>());
        let lock_path = write_lock_file(port, &auth_token, &workspace_folders, ide_name)?;

        let clients = Arc::new(Mutex::new(Vec::new()));
        let accept_task = cx.spawn({
            let clients = clients.clone();
            async move |cx| {
                loop {
                    let stream = match listener.accept().await {
                        Ok((stream, _)) => stream,
                        Err(error) => {
                            log::error!("Claude IDE server stopped accepting connections: {error}");
                            return;
                        }
                    };
                    cx.spawn({
                        let auth_token = auth_token.clone();
                        let host = host.clone();
                        let clients = clients.clone();
                        async move |cx| {
                            serve_connection(stream, auth_token, host, clients, cx)
                                .await
                                .log_err();
                        }
                    })
                    .detach();
                }
            }
        });

        Ok(Self {
            port,
            lock_path,
            clients,
            _accept_task: accept_task,
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Environment variables that make a terminal's `claude` connect to this server.
    pub fn terminal_env(&self) -> [(String, String); 2] {
        [
            ("CLAUDE_CODE_SSE_PORT".to_string(), self.port.to_string()),
            ("ENABLE_IDE_INTEGRATION".to_string(), "true".to_string()),
        ]
    }

    pub fn has_clients(&self) -> bool {
        !self.clients.lock().is_empty()
    }

    pub fn notify(&self, method: &str, params: Value) {
        let message = json!({ "jsonrpc": "2.0", "method": method, "params": params }).to_string();
        self.clients
            .lock()
            .retain(|client| client.try_send(message.clone()).is_ok());
    }
}

impl Drop for ClaudeIdeServer {
    fn drop(&mut self) {
        std::fs::remove_file(&self.lock_path).log_err();
    }
}

fn lock_directory() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| paths::home_dir().join(".claude"))
        .join("ide")
}

fn write_lock_file(
    port: u16,
    auth_token: &str,
    workspace_folders: &[PathBuf],
    ide_name: &str,
) -> Result<PathBuf> {
    use std::io::Write as _;

    let directory = lock_directory();
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("failed to create {}", directory.display()))?;
    let lock_path = directory.join(format!("{port}.lock"));
    let contents = json!({
        "pid": std::process::id(),
        "workspaceFolders": workspace_folders,
        "ideName": ide_name,
        "transport": "ws",
        "authToken": auth_token,
    });

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    // The lock file holds the token that authorizes tool calls, so keep it private.
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options
        .open(&lock_path)
        .with_context(|| format!("failed to create {}", lock_path.display()))?;
    file.write_all(contents.to_string().as_bytes())
        .with_context(|| format!("failed to write {}", lock_path.display()))?;
    Ok(lock_path)
}

enum ConnectionEvent {
    Outgoing(Option<String>),
    Incoming(Option<Result<Message, async_tungstenite::tungstenite::Error>>),
}

#[expect(
    clippy::result_large_err,
    reason = "tungstenite requires the handshake callback to return its large `ErrorResponse`"
)]
async fn serve_connection(
    stream: TcpStream,
    auth_token: String,
    host: Rc<dyn ToolHost>,
    clients: Arc<Mutex<Vec<async_channel::Sender<String>>>>,
    cx: &mut AsyncApp,
) -> Result<()> {
    let mut websocket =
        async_tungstenite::accept_hdr_async(stream, |request: &Request, mut response: Response| {
            let authorized = request
                .headers()
                .get(AUTH_HEADER)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value == auth_token);
            if !authorized {
                let mut rejection = ErrorResponse::new(Some("unauthorized".to_string()));
                *rejection.status_mut() = StatusCode::UNAUTHORIZED;
                return Err(rejection);
            }
            response.headers_mut().insert(
                "sec-websocket-protocol",
                HeaderValue::from_static(SUBPROTOCOL),
            );
            Ok(response)
        })
        .await?;

    let (outgoing_sender, outgoing_receiver) = async_channel::unbounded::<String>();
    clients.lock().push(outgoing_sender.clone());

    let result = async {
        loop {
            let event = select_biased! {
                outgoing = outgoing_receiver.recv().fuse() => ConnectionEvent::Outgoing(outgoing.ok()),
                incoming = websocket.next().fuse() => ConnectionEvent::Incoming(incoming),
            };
            match event {
                ConnectionEvent::Outgoing(Some(text)) => {
                    websocket.send(Message::text(text)).await?;
                }
                ConnectionEvent::Incoming(Some(Ok(Message::Text(text)))) => {
                    handle_message(text.as_str(), &host, &outgoing_sender, cx);
                }
                ConnectionEvent::Incoming(Some(Ok(Message::Close(_))) | None)
                | ConnectionEvent::Outgoing(None) => return Ok(()),
                ConnectionEvent::Incoming(Some(Ok(_))) => {}
                ConnectionEvent::Incoming(Some(Err(error))) => return Err(error.into()),
            }
        }
    }
    .await;

    clients
        .lock()
        .retain(|client| !client.same_channel(&outgoing_sender));
    result
}

fn handle_message(
    text: &str,
    host: &Rc<dyn ToolHost>,
    outgoing: &async_channel::Sender<String>,
    cx: &mut AsyncApp,
) {
    let message: Value = match serde_json::from_str(text) {
        Ok(message) => message,
        Err(error) => {
            log::debug!("ignoring malformed Claude IDE message: {error}");
            return;
        }
    };
    // Notifications such as `notifications/initialized` and `ide_connected`
    // carry no id and expect no reply.
    let Some(id) = message.get("id").cloned() else {
        return;
    };
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let params = message.get("params").cloned().unwrap_or(Value::Null);

    let response = match method {
        "initialize" => {
            let protocol_version = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(DEFAULT_PROTOCOL_VERSION);
            Some(rpc_result(
                &id,
                json!({
                    "protocolVersion": protocol_version,
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "zed", "version": env!("CARGO_PKG_VERSION") },
                }),
            ))
        }
        "ping" => Some(rpc_result(&id, json!({}))),
        "tools/list" => Some(rpc_result(&id, json!({ "tools": host.tools() }))),
        "tools/call" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
            let tool_call = host.call_tool(&name, arguments, cx);
            let outgoing = outgoing.clone();
            cx.spawn(async move |_| {
                let result = match tool_call.await {
                    Ok(ToolOutput::Json(Value::String(text))) => tool_content(vec![text], false),
                    Ok(ToolOutput::Json(value)) => tool_content(vec![value.to_string()], false),
                    Ok(ToolOutput::Blocks(blocks)) => tool_content(blocks, false),
                    Err(error) => tool_content(vec![format!("{error:#}")], true),
                };
                outgoing.try_send(rpc_result(&id, result)).log_err();
            })
            .detach();
            None
        }
        _ => Some(
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("method not found: {method}") },
            })
            .to_string(),
        ),
    };
    if let Some(response) = response {
        outgoing.try_send(response).log_err();
    }
}

fn tool_content(blocks: Vec<String>, is_error: bool) -> Value {
    let content = blocks
        .into_iter()
        .map(|text| json!({ "type": "text", "text": text }))
        .collect::<Vec<_>>();
    let mut result = json!({ "content": content });
    if is_error {
        result["isError"] = Value::Bool(true);
    }
    result
}

fn rpc_result(id: &Value, result: Value) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_tungstenite::tungstenite::client::IntoClientRequest as _;
    use gpui::TestAppContext;

    struct EchoHost;

    impl ToolHost for EchoHost {
        fn tools(&self) -> Vec<Value> {
            vec![json!({ "name": "echo", "inputSchema": { "type": "object" } })]
        }

        fn call_tool(
            &self,
            name: &str,
            arguments: Value,
            _cx: &mut AsyncApp,
        ) -> Task<Result<ToolOutput>> {
            Task::ready(match name {
                "echo" => Ok(ToolOutput::Json(arguments)),
                "pair" => Ok(ToolOutput::Blocks(vec!["one".into(), "two".into()])),
                other => Err(anyhow::anyhow!("unknown tool {other}")),
            })
        }
    }

    async fn connect(
        port: u16,
        token: &str,
    ) -> Result<
        async_tungstenite::WebSocketStream<TcpStream>,
        async_tungstenite::tungstenite::Error,
    > {
        let mut request = format!("ws://127.0.0.1:{port}").into_client_request()?;
        request
            .headers_mut()
            .insert(AUTH_HEADER, HeaderValue::from_str(token).unwrap());
        request
            .headers_mut()
            .insert("sec-websocket-protocol", HeaderValue::from_static("mcp"));
        let stream = TcpStream::connect(("127.0.0.1", port)).await?;
        Ok(async_tungstenite::client_async(request, stream).await?.0)
    }

    fn token_from_lock(port: u16) -> String {
        let contents = std::fs::read_to_string(lock_directory().join(format!("{port}.lock")))
            .expect("lock file should exist");
        let lock: Value = serde_json::from_str(&contents).expect("lock file should be JSON");
        assert_eq!(lock["transport"], "ws");
        lock["authToken"].as_str().unwrap().to_string()
    }

    async fn request(
        websocket: &mut async_tungstenite::WebSocketStream<TcpStream>,
        message: Value,
    ) -> Value {
        websocket
            .send(Message::text(message.to_string()))
            .await
            .unwrap();
        loop {
            match websocket.next().await.unwrap().unwrap() {
                Message::Text(text) => return serde_json::from_str(text.as_str()).unwrap(),
                _ => continue,
            }
        }
    }

    #[gpui::test]
    async fn test_handshake_tools_and_auth(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let directory = tempfile_dir();
        // SAFETY: the test owns the process environment for its duration.
        unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", &directory) };

        let server = cx.spawn(|cx| async move {
            ClaudeIdeServer::start(vec![PathBuf::from("/work")], "Zed", Rc::new(EchoHost), &cx).await
        });
        let server = server.await.unwrap();
        let port = server.port();
        let token = token_from_lock(port);

        assert!(connect(port, "wrong-token").await.is_err());

        let mut websocket = connect(port, &token).await.unwrap();
        let initialized = request(
            &mut websocket,
            json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize",
                    "params": { "protocolVersion": "2025-11-25" } }),
        )
        .await;
        assert_eq!(initialized["result"]["protocolVersion"], "2025-11-25");

        let tools = request(
            &mut websocket,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
        )
        .await;
        assert_eq!(tools["result"]["tools"][0]["name"], "echo");

        let called = request(
            &mut websocket,
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                    "params": { "name": "echo", "arguments": { "a": 1 } } }),
        )
        .await;
        assert_eq!(called["result"]["content"][0]["text"], r#"{"a":1}"#);

        let blocks = request(
            &mut websocket,
            json!({ "jsonrpc": "2.0", "id": 4, "method": "tools/call",
                    "params": { "name": "pair", "arguments": {} } }),
        )
        .await;
        assert_eq!(blocks["result"]["content"][0]["text"], "one");
        assert_eq!(blocks["result"]["content"][1]["text"], "two");

        let failed = request(
            &mut websocket,
            json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                    "params": { "name": "missing", "arguments": {} } }),
        )
        .await;
        assert_eq!(failed["result"]["isError"], true);

        drop(server);
        assert!(!lock_directory().join(format!("{port}.lock")).exists());
    }

    fn tempfile_dir() -> PathBuf {
        let directory = std::env::temp_dir().join(format!("claude-ide-test-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }
}
