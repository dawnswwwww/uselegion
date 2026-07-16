//! Interactive multiple-choice question tool.
//!
//! The `ask_user` tool lets the model present structured questions to the user
//! and block until the user answers. It is the Legion equivalent of Claude
//! Code's `AskUserQuestion` tool.

use async_trait::async_trait;
use legion_runtime::{
    AskUserInput, AskUserOutput, QuestionRequest, Tool, ToolContext, ToolError, ToolResult,
};
use serde_json::json;

use crate::policy::{Approval, Policy};

/// A tool that asks the user one or more multiple-choice questions.
#[derive(Debug)]
pub struct AskUserTool {
    policy: Policy,
}

impl AskUserTool {
    pub fn new() -> Self {
        Self {
            policy: Policy {
                approval: Approval::Off,
                permission_mode: None,
                allow_from: Vec::new(),
                workspace_only: false,
            },
        }
    }
}

impl Default for AskUserTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for AskUserTool {
    fn name(&self) -> &str {
        "ask_user"
    }

    fn description(&self) -> &str {
        "Asks the user one or more multiple-choice questions to clarify ambiguity, understand \
         preferences, or make a decision. Use this tool instead of asking open-ended questions \
         in plain text."
    }

    fn policy(&self) -> &Policy {
        &self.policy
    }

    fn schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 4,
                    "description": "Questions to ask the user (1-4 questions).",
                    "items": {
                        "type": "object",
                        "properties": {
                            "question": {
                                "type": "string",
                                "description": "The complete question to ask. Should be clear, specific, and end with a question mark."
                            },
                            "header": {
                                "type": "string",
                                "description": "Very short label displayed as a chip/tag (max 12 chars). Examples: 'Auth method', 'Library', 'Approach'."
                            },
                            "options": {
                                "type": "array",
                                "minItems": 2,
                                "maxItems": 4,
                                "description": "The available choices. Must have 2-4 options. There should be no 'Other' option; it is provided automatically.",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "label": {
                                            "type": "string",
                                            "description": "Concise display text (1-5 words) for the option."
                                        },
                                        "description": {
                                            "type": "string",
                                            "description": "Explanation of what this option means or what will happen if chosen."
                                        },
                                        "preview": {
                                            "type": "string",
                                            "description": "Optional preview content rendered when this option is focused. Use for mockups, code snippets, or visual comparisons."
                                        }
                                    },
                                    "required": ["label", "description"]
                                }
                            },
                            "multiSelect": {
                                "type": "boolean",
                                "default": false,
                                "description": "Set to true to allow the user to select multiple options instead of just one."
                            }
                        },
                        "required": ["question", "header", "options"]
                    }
                }
            },
            "required": ["questions"]
        })
    }

    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        true
    }

    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        true
    }

    fn validate_input(&self, input: &serde_json::Value) -> Result<(), ToolError> {
        let parsed: AskUserInput = serde_json::from_value(input.clone())
            .map_err(|e| ToolError::InvalidParams(format!("invalid ask_user input: {e}")))?;

        let mut seen_questions = std::collections::HashSet::new();
        for q in &parsed.questions {
            if !seen_questions.insert(q.question.clone()) {
                return Err(ToolError::InvalidParams(format!(
                    "duplicate question: {}",
                    q.question
                )));
            }
            let mut seen_labels = std::collections::HashSet::new();
            for opt in &q.options {
                if !seen_labels.insert(opt.label.clone()) {
                    return Err(ToolError::InvalidParams(format!(
                        "duplicate option label '{}' in question '{}'",
                        opt.label, q.question
                    )));
                }
            }
        }
        Ok(())
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let input: AskUserInput = serde_json::from_value(params)
            .map_err(|e| ToolError::InvalidParams(format!("invalid ask_user input: {e}")))?;

        let gate = match ctx.question_gate {
            Some(g) => g,
            None => {
                return Err(ToolError::Execution(
                    "ask_user requires an interactive session".to_string(),
                ));
            }
        };

        let req = QuestionRequest {
            tool: "ask_user".to_string(),
            agent_id: ctx.agent_id,
            session_key: ctx.session_id,
            interactive: true,
        };

        let answer = gate.request(&req, &input.questions).await;
        match answer {
            Some(output) => {
                let text = format_answers(&output);
                Ok(ToolResult::ok(text))
            }
            None => Err(ToolError::Execution(
                "user did not answer the question in time".to_string(),
            )),
        }
    }
}

fn format_answers(output: &AskUserOutput) -> String {
    let mut lines = Vec::new();
    for q in &output.questions {
        let answer = output.answers.get(&q.question).cloned().unwrap_or_default();
        let annotation = output.annotations.as_ref().and_then(|a| a.get(&q.question));
        let mut parts = vec![format!("Q: {}", q.question), format!("A: {answer}")];
        if let Some(a) = annotation {
            if let Some(notes) = &a.notes {
                parts.push(format!("Notes: {notes}"));
            }
        }
        lines.push(parts.join("\n"));
    }
    lines.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use legion_runtime::{AskUserOption, AskUserQuestion, NoOpQuestionNotifier, QuestionGate};
    use std::sync::Arc;
    use std::time::Duration;

    fn sample_input() -> serde_json::Value {
        json!({
            "questions": [{
                "question": "Which color?",
                "header": "Color",
                "options": [
                    {"label": "Red", "description": "Warm"},
                    {"label": "Blue", "description": "Cool"}
                ]
            }]
        })
    }

    fn ctx_with_gate(gate: Arc<QuestionGate>) -> ToolContext {
        ToolContext {
            workspace: std::path::PathBuf::from("/tmp"),
            session_id: "s1".into(),
            agent_id: "a1".into(),
            sender: None,
            memory: None,
            viewed_files: None,
            allowed_tools: None,
            spawner: None,
            messenger: None,
            swarm: None,
            depth: 0,
            parent_history: None,
            question_gate: Some(gate),
            todo_store: None,
            background_tasks: None,
        }
    }

    #[tokio::test]
    async fn ask_user_blocks_until_answered() {
        let gate = Arc::new(QuestionGate::new(
            Arc::new(NoOpQuestionNotifier),
            Duration::from_secs(5),
        ));
        let gate_for_resolve = gate.clone();
        let tool = AskUserTool::new();
        let ctx = ctx_with_gate(gate);
        let params = sample_input();

        let handle = tokio::spawn(async move { tool.execute(params, ctx).await });

        // Wait a tiny bit to ensure the gate has registered the prompt.
        tokio::time::sleep(Duration::from_millis(10)).await;
        let answer = AskUserOutput {
            questions: vec![AskUserQuestion {
                question: "Which color?".into(),
                header: "Color".into(),
                options: vec![
                    AskUserOption {
                        label: "Red".into(),
                        description: "Warm".into(),
                        preview: None,
                    },
                    AskUserOption {
                        label: "Blue".into(),
                        description: "Cool".into(),
                        preview: None,
                    },
                ],
                multi_select: false,
            }],
            answers: [("Which color?".into(), "Red".into())].into(),
            annotations: None,
        };
        gate_for_resolve.resolve("question-0", answer).await;

        let result = handle.await.unwrap().expect("tool should succeed");
        assert!(!result.is_error);
        assert!(result.content.contains("Which color?"));
        assert!(result.content.contains("Red"));
    }

    #[tokio::test]
    async fn ask_user_fails_when_no_gate() {
        let tool = AskUserTool::new();
        let ctx = ToolContext {
            workspace: std::path::PathBuf::from("/tmp"),
            session_id: "s1".into(),
            agent_id: "a1".into(),
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
        };

        let result = tool.execute(sample_input(), ctx).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("interactive session")
        );
    }

    #[test]
    fn validate_input_rejects_duplicate_questions() {
        let tool = AskUserTool::new();
        let input = json!({
            "questions": [
                {"question": "Same?", "header": "A", "options": [
                    {"label": "Yes", "description": "y"},
                    {"label": "No", "description": "n"}
                ]},
                {"question": "Same?", "header": "B", "options": [
                    {"label": "Maybe", "description": "m"},
                    {"label": "Never", "description": "x"}
                ]}
            ]
        });
        let err = tool.validate_input(&input).unwrap_err().to_string();
        assert!(err.contains("duplicate question"));
    }
}
