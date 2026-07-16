use legion_acp::protocol::{
    AcpEvent, JsonRpcRequest, JsonRpcResponse, METHOD_AGENTS_RUN, RunParams, RunResult,
};
use serde_json::json;
use std::io::{BufRead, Write};

fn main() {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut stdout_lock = stdout.lock();
    let mut lines = stdin.lock().lines();

    let request_line = match lines.next() {
        Some(Ok(line)) => line,
        _ => return,
    };

    let request: JsonRpcRequest<RunParams> =
        serde_json::from_str(&request_line).expect("mock harness expects agents/run request");
    assert_eq!(request.method, METHOD_AGENTS_RUN);

    let instructions = request.params.instructions;
    let should_call_tool = instructions.contains("call echo");

    if should_call_tool {
        let response = JsonRpcResponse::result(
            request.id.clone(),
            RunResult {
                status: "streaming".to_string(),
                events: vec![
                    AcpEvent::Text {
                        delta: "calling tool".to_string(),
                    },
                    AcpEvent::ToolCall {
                        id: "call-1".to_string(),
                        tool: "echo".to_string(),
                        params: json!({"msg": "hello"}),
                    },
                ],
            },
        );
        writeln!(stdout_lock, "{}", serde_json::to_string(&response).unwrap()).unwrap();
        stdout_lock.flush().unwrap();

        // Wait for the tool result notification from Legion.
        let _tool_result_line = lines.next().unwrap().unwrap();
    }

    let response = JsonRpcResponse::result(
        request.id,
        RunResult {
            status: "streaming".to_string(),
            events: vec![
                AcpEvent::Text {
                    delta: "done".to_string(),
                },
                AcpEvent::Done,
            ],
        },
    );
    writeln!(stdout_lock, "{}", serde_json::to_string(&response).unwrap()).unwrap();
    stdout_lock.flush().unwrap();
}
