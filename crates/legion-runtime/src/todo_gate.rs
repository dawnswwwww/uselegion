//! Turn-end gate that prevents the agent from ending a turn while required
//! todo patterns are not yet completed.
//!
//! The gate is model-driven: it inspects the session todo list produced by the
//! `todo_write` tool. When enabled, the agent loop appends a system reminder
//! instead of breaking out of the tool loop, giving the model a chance to
//! finish the remaining work.

use crate::todo::{TodoList, TodoStatus};

/// Result of evaluating the turn-end gate against the current todo list.
#[derive(Debug, Clone, PartialEq)]
pub enum TodoGateResult {
    /// All required patterns are satisfied; the turn may end.
    Pass,
    /// The todo list is empty while the gate requires at least one pattern.
    NoTodos,
    /// One or more required patterns are not matched by completed todos.
    Incomplete { missing: Vec<String> },
}

/// Turn-end gate tied to the session todo list.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TodoGate {
    /// Required substrings that must appear in at least one completed todo item.
    /// When empty, the gate always passes.
    pub required_patterns: Vec<String>,
}

impl TodoGate {
    /// Create a gate with the given required patterns.
    pub fn new(required_patterns: Vec<String>) -> Self {
        Self { required_patterns }
    }

    /// Evaluate the gate against the current todo list and the latest assistant
    /// message.
    ///
    /// The gate passes when:
    /// - no patterns are configured, or
    /// - every configured pattern is found as a substring of at least one
    ///   completed todo item.
    pub fn check(&self, todos: &TodoList, _last_assistant_msg: &str) -> TodoGateResult {
        if self.required_patterns.is_empty() {
            return TodoGateResult::Pass;
        }
        if todos.is_empty() {
            return TodoGateResult::NoTodos;
        }

        let completed: Vec<String> = todos
            .items
            .iter()
            .filter(|t| t.status == TodoStatus::Completed)
            .map(|t| t.content.to_lowercase())
            .collect();

        let mut missing = Vec::new();
        for pattern in &self.required_patterns {
            let needle = pattern.to_lowercase();
            if !completed.iter().any(|c| c.contains(&needle)) {
                missing.push(pattern.clone());
            }
        }

        if missing.is_empty() {
            TodoGateResult::Pass
        } else {
            TodoGateResult::Incomplete { missing }
        }
    }
}

/// Render a system reminder for the model when the gate did not pass.
pub fn todo_gate_reminder(result: &TodoGateResult) -> Option<String> {
    match result {
        TodoGateResult::Pass => None,
        TodoGateResult::NoTodos => Some(
            "The session todo list is empty. Before finishing this turn, create todos for the \
             required steps using the todo_write tool and complete them."
                .to_string(),
        ),
        TodoGateResult::Incomplete { missing } => Some(format!(
            "The following required tasks are not yet marked as completed: {}. \
             Please finish them (and update the todo list via todo_write) before ending this turn.",
            missing.join(", ")
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::todo::{TodoItem, TodoStatus};

    fn make_todo(content: &str, status: TodoStatus) -> TodoItem {
        TodoItem {
            id: content.to_string(),
            content: content.to_string(),
            status,
            active_form: content.to_string(),
        }
    }

    #[test]
    fn empty_gate_always_passes() {
        let gate = TodoGate::default();
        assert_eq!(gate.check(&TodoList::default(), ""), TodoGateResult::Pass);
    }

    #[test]
    fn no_todos_fails_when_patterns_required() {
        let gate = TodoGate::new(vec!["test".to_string()]);
        assert_eq!(
            gate.check(&TodoList::default(), ""),
            TodoGateResult::NoTodos
        );
    }

    #[test]
    fn incomplete_todos_report_missing_patterns() {
        let gate = TodoGate::new(vec!["tests pass".to_string(), "docs updated".to_string()]);
        let todos = TodoList {
            items: vec![make_todo("Tests pass", TodoStatus::Completed)],
        };
        assert_eq!(
            gate.check(&todos, ""),
            TodoGateResult::Incomplete {
                missing: vec!["docs updated".to_string()]
            }
        );
    }

    #[test]
    fn all_patterns_completed_passes() {
        let gate = TodoGate::new(vec!["tests pass".to_string(), "docs updated".to_string()]);
        let todos = TodoList {
            items: vec![
                make_todo("Tests pass", TodoStatus::Completed),
                make_todo("Docs updated", TodoStatus::Completed),
            ],
        };
        assert_eq!(gate.check(&todos, ""), TodoGateResult::Pass);
    }

    #[test]
    fn pending_todo_does_not_satisfy_pattern() {
        let gate = TodoGate::new(vec!["tests pass".to_string()]);
        let todos = TodoList {
            items: vec![make_todo("Tests pass", TodoStatus::Pending)],
        };
        assert_eq!(
            gate.check(&todos, ""),
            TodoGateResult::Incomplete {
                missing: vec!["tests pass".to_string()]
            }
        );
    }

    #[test]
    fn reminder_for_incomplete_lists_missing() {
        let result = TodoGateResult::Incomplete {
            missing: vec!["a".to_string(), "b".to_string()],
        };
        let reminder = todo_gate_reminder(&result).unwrap();
        assert!(reminder.contains("a, b"));
    }
}
