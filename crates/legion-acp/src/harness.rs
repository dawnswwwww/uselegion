use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use futures::SinkExt;
use futures::channel::mpsc;
use legion_core::config::Config;
use legion_provider::types::ToolDefinition;
use legion_runtime::{
    Harness, LifecyclePhase, RunEvent, RunRequest, RunStream, RuntimeError, ToolCall, ToolContext,
    ToolRegistry, ToolResult,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

use crate::protocol::{
    AcpEvent, AgentInfo, JsonRpcRequest, JsonRpcResponse, METHOD_AGENTS_RUN, METHOD_TOOLS_RESULT,
    RunParams, RunResult, SessionInfo, ToolResultParams,
};

/// External ACP harness.
///
/// Spawns a configured command and communicates via stdin/stdout JSON-RPC.
/// Tool calls emitted by the harness are forwarded to Legion's `ToolRegistry`
/// and results are sent back.
#[derive(Clone)]
pub struct AcpHarness {
    command: Vec<String>,
    tool_registry: Arc<dyn ToolRegistry>,
    config: Config,
}

impl AcpHarness {
    pub fn new(command: Vec<String>, tool_registry: Arc<dyn ToolRegistry>, config: Config) -> Self {
        Self {
            command,
            tool_registry,
            config,
        }
    }

    fn resolve_workspace(&self, agent_id: &str) -> PathBuf {
        legion_runtime::resolve_workspace(&self.config, agent_id, None)
    }
}

#[async_trait]
impl Harness for AcpHarness {
    fn id(&self) -> &str {
        "acp"
    }

    fn can_handle(&self, model_ref: &str) -> bool {
        model_ref.starts_with("acp:")
    }

    fn run(&self, request: RunRequest) -> Result<RunStream, RuntimeError> {
        let (mut tx, rx) = mpsc::channel::<RunEvent>(128);
        let command = self.command.clone();
        let tool_registry = self.tool_registry.clone();
        let workspace = self.resolve_workspace(&request.agent_id);
        let tool_defs = self.tool_registry.definitions();

        tokio::spawn(async move {
            if let Err(err) = run_acp(
                command,
                request,
                tool_registry,
                workspace,
                tool_defs,
                &mut tx,
            )
            .await
            {
                let _ = tx
                    .send(RunEvent::Lifecycle {
                        phase: LifecyclePhase::Error,
                        error: Some(err.to_string()),
                    })
                    .await;
                tracing::error!(error = %err, "acp harness run failed");
            }
        });

        Ok(Box::pin(rx))
    }
}

async fn run_acp(
    command: Vec<String>,
    request: RunRequest,
    tool_registry: Arc<dyn ToolRegistry>,
    workspace: PathBuf,
    tool_defs: Vec<ToolDefinition>,
    tx: &mut mpsc::Sender<RunEvent>,
) -> Result<(), RuntimeError> {
    send(
        tx,
        RunEvent::Lifecycle {
            phase: LifecyclePhase::Start,
            error: None,
        },
    )
    .await;

    if command.is_empty() {
        return Err(RuntimeError::Context(
            "ACP harness command is empty".to_string(),
        ));
    }

    let mut child = spawn_child(&command)?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| RuntimeError::Io(std::io::Error::other("failed to open child stdin")))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| RuntimeError::Io(std::io::Error::other("failed to open child stdout")))?;
    let mut lines = BufReader::new(stdout).lines();

    let acp_request = build_run_request(&request, &workspace, tool_defs);
    write_jsonrpc(&mut stdin, &acp_request).await?;

    let mut done = false;
    while let Some(line) = lines.next_line().await.map_err(RuntimeError::Io)? {
        let response: JsonRpcResponse<RunResult> = serde_json::from_str(&line)
            .map_err(|e| RuntimeError::Context(format!("invalid ACP response: {e}")))?;

        if let Some(error) = response.error {
            send(
                tx,
                RunEvent::Lifecycle {
                    phase: LifecyclePhase::Error,
                    error: Some(format!("ACP error {}: {}", error.code, error.message)),
                },
            )
            .await;
            return Ok(());
        }

        if let Some(result) = response.result {
            for event in result.events {
                match event {
                    AcpEvent::Text { delta } => {
                        send(tx, RunEvent::AssistantDelta { delta }).await;
                    }
                    AcpEvent::ToolCall { id, tool, params } => {
                        let runtime_call = ToolCall {
                            id: id.clone(),
                            name: tool.clone(),
                            arguments: params.to_string(),
                        };
                        send(
                            tx,
                            RunEvent::ToolStart {
                                tool_call: runtime_call.clone(),
                            },
                        )
                        .await;

                        let result = execute_tool_call(
                            &runtime_call,
                            &workspace,
                            &request.session_id,
                            &request.agent_id,
                            &tool_registry,
                        )
                        .await;

                        send(
                            tx,
                            RunEvent::ToolEnd {
                                tool_call: runtime_call,
                                result: result.clone(),
                            },
                        )
                        .await;

                        let result_value = serde_json::json!({
                            "content": result.content,
                            "is_error": result.is_error,
                        });
                        let notification = JsonRpcRequest::new(
                            id.clone(),
                            METHOD_TOOLS_RESULT,
                            ToolResultParams {
                                id,
                                result: result_value,
                            },
                        );
                        write_jsonrpc(&mut stdin, &notification).await?;
                    }
                    AcpEvent::ToolResult { .. } => {
                        // Tool results are produced by Legion; ignore harness echoes.
                    }
                    AcpEvent::Done => {
                        done = true;
                    }
                    AcpEvent::Error { message } => {
                        send(
                            tx,
                            RunEvent::Lifecycle {
                                phase: LifecyclePhase::Error,
                                error: Some(message),
                            },
                        )
                        .await;
                        return Ok(());
                    }
                }
            }
        }

        if done {
            break;
        }
    }

    send(
        tx,
        RunEvent::Lifecycle {
            phase: LifecyclePhase::End,
            error: None,
        },
    )
    .await;
    Ok(())
}

fn spawn_child(command: &[String]) -> Result<Child, RuntimeError> {
    Command::new(&command[0])
        .args(&command[1..])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(RuntimeError::Io)
}

async fn write_jsonrpc<T: serde::Serialize>(
    stdin: &mut tokio::process::ChildStdin,
    value: &T,
) -> Result<(), RuntimeError> {
    let line = serde_json::to_string(value)
        .map_err(|e| RuntimeError::Context(format!("failed to serialize ACP message: {e}")))?;
    stdin
        .write_all(line.as_bytes())
        .await
        .map_err(RuntimeError::Io)?;
    stdin.write_all(b"\n").await.map_err(RuntimeError::Io)?;
    stdin.flush().await.map_err(RuntimeError::Io)
}

fn build_run_request(
    request: &RunRequest,
    workspace: &Path,
    tool_defs: Vec<ToolDefinition>,
) -> JsonRpcRequest<RunParams> {
    let instructions = request
        .system_prompt
        .clone()
        .unwrap_or_else(|| request.user_message.clone());
    let model = request
        .model_ref
        .strip_prefix("acp:")
        .unwrap_or(&request.model_ref)
        .to_string();

    JsonRpcRequest::new(
        request.session_id.clone(),
        METHOD_AGENTS_RUN,
        RunParams {
            agent: AgentInfo {
                id: request.agent_id.clone(),
                workspace: workspace.to_string_lossy().to_string(),
            },
            session: SessionInfo {
                id: request.session_id.clone(),
                history: request.history.clone(),
            },
            tools: tool_defs.into_iter().map(|d| d.name).collect(),
            instructions,
            model,
        },
    )
}

async fn execute_tool_call(
    tc: &ToolCall,
    workspace: &Path,
    session_id: &str,
    agent_id: &str,
    registry: &Arc<dyn ToolRegistry>,
) -> ToolResult {
    let tool = match registry.get(&tc.name) {
        Some(t) => t,
        None => {
            return ToolResult::error(format!("tool '{}' not found", tc.name));
        }
    };

    let params = match serde_json::from_str::<serde_json::Value>(&tc.arguments) {
        Ok(p) => p,
        Err(err) => {
            return ToolResult::error(format!("invalid tool arguments: {err}"));
        }
    };

    let ctx = ToolContext {
        workspace: workspace.to_path_buf(),
        session_id: session_id.to_string(),
        agent_id: agent_id.to_string(),
        sender: None,
        memory: None,
        viewed_files: None,
        allowed_tools: None,
        spawner: None,
        messenger: None,
        swarm: None,
        depth: 0,
        parent_history: None,
        question_gate: None,
        todo_store: None,
        background_tasks: None,
        plan_mode_tracker: None,
    };

    match tool.execute(params, ctx).await {
        Ok(res) => res,
        Err(err) => ToolResult::error(err.to_string()),
    }
}

async fn send(tx: &mut mpsc::Sender<RunEvent>, event: RunEvent) {
    let _ = tx.send(event).await;
}
