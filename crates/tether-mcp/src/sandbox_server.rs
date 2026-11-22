//! Exposes `tether-sandbox` as the built-in tool server named `sandbox`, with
//! `exec`, `read_file`, `write_file` and `list_dir`.
//!
//! It runs in-process behind [`McpClient`], so it goes through the same
//! registry, policy and trace path as any external server. A command that
//! fails, or arguments that do not fit, come back as a result with `is_error`
//! set, so the agent sees a normal failed call.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tether_sandbox::{Exec, Handle, Sandbox};
use tether_types::tool::Payload;
use tether_types::{ToolCall, ToolResult};

use crate::{McpClient, McpError, ToolSpec};

pub const SERVER: &str = "sandbox";

pub struct SandboxServer {
    sandbox: Arc<dyn Sandbox>,
    handle: Handle,
}

#[derive(Deserialize)]
struct ExecArgs {
    argv: Vec<String>,
    cwd: Option<PathBuf>,
    stdin: Option<String>,
}

#[derive(Deserialize)]
struct ReadArgs {
    path: PathBuf,
}

#[derive(Deserialize)]
struct WriteArgs {
    path: PathBuf,
    content: String,
}

#[derive(Deserialize)]
struct ListArgs {
    #[serde(default)]
    path: Option<String>,
}

fn args<T: serde::de::DeserializeOwned>(call: &ToolCall) -> Result<T, String> {
    // Arguments are named, never positional. serde would take a JSON array
    // for these structs field by field, and a rule that looks for `argv` or
    // `path` by name would see nothing to match on: a way past every rule.
    if !call.arguments.is_object() {
        return Err("bad arguments: expected a JSON object".into());
    }
    serde_json::from_value(call.arguments.clone()).map_err(|e| format!("bad arguments: {e}"))
}

/// The same check the sandbox makes on a file path, for the directory
/// `list_dir` is given: relative, and never climbing.
fn in_workspace(path: &str) -> Result<(), String> {
    use std::path::{Component, Path};
    let stays = Path::new(path)
        .components()
        .all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
    if stays {
        Ok(())
    } else {
        Err(format!("path leaves the workspace: {path}"))
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

impl SandboxServer {
    /// Serve a sandbox that has already been started.
    pub fn new(sandbox: Arc<dyn Sandbox>, handle: Handle) -> Self {
        Self { sandbox, handle }
    }

    async fn exec(&self, cmd: Exec) -> Result<(Value, bool), String> {
        let out = self
            .sandbox
            .exec(&self.handle, cmd)
            .await
            .map_err(|e| e.to_string())?;
        let content = json!({
            "exit_code": out.exit_code,
            "stdout": text(&out.stdout),
            "stderr": text(&out.stderr),
        });
        Ok((content, out.exit_code != 0))
    }

    /// The content of the result and whether it is an error.
    async fn dispatch(&self, call: &ToolCall) -> Result<(Value, bool), String> {
        match call.name.as_str() {
            "exec" => {
                let a: ExecArgs = args(call)?;
                self.exec(Exec {
                    argv: a.argv,
                    cwd: a.cwd,
                    env: vec![],
                    stdin: a.stdin.map(String::into_bytes),
                })
                .await
            }
            "read_file" => {
                let a: ReadArgs = args(call)?;
                let data = self
                    .sandbox
                    .read(&self.handle, &a.path)
                    .await
                    .map_err(|e| e.to_string())?;
                Ok((text(&data).into(), false))
            }
            "write_file" => {
                let a: WriteArgs = args(call)?;
                let bytes = a.content.len();
                self.sandbox
                    .write(&self.handle, &a.path, a.content.into_bytes())
                    .await
                    .map_err(|e| e.to_string())?;
                Ok((json!({ "bytes": bytes }), false))
            }
            "list_dir" => {
                let a: ListArgs = args(call)?;
                let dir = a.path.unwrap_or_else(|| ".".into());
                in_workspace(&dir)?;
                let (out, failed) = self
                    .exec(Exec {
                        argv: vec!["ls".into(), "-1A".into(), "--".into(), dir],
                        cwd: None,
                        env: vec![],
                        stdin: None,
                    })
                    .await?;
                if failed {
                    return Ok((out, true));
                }
                let names: Vec<&str> = out["stdout"].as_str().unwrap_or_default().lines().collect();
                Ok((json!(names), false))
            }
            other => Err(format!("the sandbox server has no tool {other:?}")),
        }
    }
}

fn spec(name: &str, description: &str, input_schema: Value) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        server: SERVER.into(),
        description: description.into(),
        input_schema,
        external: false,
    }
}

#[async_trait]
impl McpClient for SandboxServer {
    async fn list_tools(&self) -> Result<Vec<ToolSpec>, McpError> {
        let string = json!({"type": "string"});
        Ok(vec![
            spec(
                "exec",
                "Run a command in the workspace. Returns exit_code, stdout and stderr.",
                json!({
                    "type": "object",
                    "properties": {
                        "argv": {"type": "array", "items": string},
                        "cwd": string,
                        "stdin": string,
                    },
                    "required": ["argv"],
                }),
            ),
            spec(
                "read_file",
                "Read a text file in the workspace.",
                json!({"type": "object", "properties": {"path": string}, "required": ["path"]}),
            ),
            spec(
                "write_file",
                "Write a text file in the workspace, creating directories as needed.",
                json!({
                    "type": "object",
                    "properties": {"path": string, "content": string},
                    "required": ["path", "content"],
                }),
            ),
            spec(
                "list_dir",
                "List the names in a workspace directory.",
                json!({"type": "object", "properties": {"path": string}}),
            ),
        ])
    }

    async fn call(&self, call: &ToolCall) -> Result<ToolResult, McpError> {
        let (content, is_error) = self
            .dispatch(call)
            .await
            .unwrap_or_else(|message| (message.into(), true));
        Ok(ToolResult {
            id: call.id.clone(),
            content: Payload::Inline(content),
            is_error,
            // Timed by the core through its clock.
            duration_ms: 0,
        })
    }

    fn servers(&self) -> Vec<crate::ServerInfo> {
        vec![crate::ServerInfo {
            id: SERVER.into(),
            transport: "builtin".into(),
            protocol_version: None,
            server_name: None,
            server_version: None,
        }]
    }
}
