//! The per-run agent loop: turn preparation, the LLM/tool iteration cycle,
//! and run finalization. Extracted from `agent_loop.rs` so the loop body can
//! evolve independently of the `AgentRuntime` wiring.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::channel::mpsc::Sender;
use futures::{SinkExt, StreamExt};

use crate::approval::{ApprovalCtx, ApprovalGate, NoOpApprovalNotifier, PermissionMode};
use crate::auto_extract::AutoExtractor;
use crate::commitments::CommitmentExtractor;
use crate::compaction::TwoPassCompactor;
use crate::context::{
    Filesystem, SessionContext, TokioFs, assemble_system_prompt_report, resolve_workspace,
};
use crate::goal::GoalStore;
use crate::goal_gate::{GoalGate, GoalGateResult};
use crate::memory::{MemoryBackend, RecallContext};
use crate::messenger::AgentMessenger;
use crate::question::{NoOpQuestionNotifier, QuestionCtx, QuestionGate};
use crate::recall_selector::LlmRecallSelector;
use crate::skill_selector::{KeywordSkillSelector, LlmSkillSelector, SkillSelector};
use crate::skills_prompt::skill_body_block;
use crate::subagent::SubagentSpawner;
use crate::surfaced::SurfacedStore;
use crate::swarm::SwarmManager;
use crate::todo_gate::{TodoGate, todo_gate_reminder};
use crate::tool_pipeline::{partition_tool_calls, run_tool_batches};
use crate::tools::{ToolCall, ToolRegistry, ToolResult, build_policy_decider};
use crate::types::{LifecyclePhase, RunEvent, RunRequest, RuntimeError};
use legion_core::config::{Config, RecallConfig};
use legion_provider::router::ProviderRouter;
use legion_provider::types::{
    ChatChunk, ChatMessage, ChatRequest, ChatRole, FinishReason, ProviderError,
    ToolCall as ProviderToolCall, ToolDefinition,
};
use legion_skills::{Skill, SkillRegistry, SkillRegistryImpl};
use legion_telemetry::{SessionMetric, TelemetryClient};

/// Dependencies and per-run inputs of [`run_loop`], bundled so the loop and
/// its helpers do not thread a dozen parameters around. Owned outright: the
/// loop runs inside a spawned task and outlives the caller.
pub(crate) struct RunContext {
    pub(crate) provider_router: Arc<ProviderRouter>,
    pub(crate) tool_registry: Arc<dyn ToolRegistry>,
    pub(crate) memory_backend: Arc<dyn MemoryBackend>,
    pub(crate) compactor: Arc<TwoPassCompactor>,
    pub(crate) config: Config,
    pub(crate) request: RunRequest,
    pub(crate) max_iterations: Option<usize>,
    pub(crate) plugin_skills: Vec<Skill>,
    pub(crate) auto_extractor: Option<Arc<AutoExtractor>>,
    pub(crate) commitment_extractor: Option<Arc<dyn CommitmentExtractor>>,
    pub(crate) recall_config: RecallConfig,
    pub(crate) selector: Option<Arc<LlmRecallSelector>>,
    pub(crate) surfaced: SurfacedStore,
    pub(crate) spawner: Option<Arc<dyn SubagentSpawner>>,
    pub(crate) messenger: Option<Arc<dyn AgentMessenger>>,
    pub(crate) swarm: Option<Arc<SwarmManager>>,
    pub(crate) todo_gate: TodoGate,
    pub(crate) goal_store: GoalStore,
    pub(crate) telemetry: Option<Arc<TelemetryClient>>,
}

/// Per-turn state assembled by [`prepare_turn`] and mutated by
/// [`run_iteration`] as the loop progresses.
struct PreparedTurn {
    workspace: PathBuf,
    todo_store: Option<crate::SharedTodoStore>,
    current_todos: crate::todo::TodoList,
    plan_tracker: Arc<tokio::sync::Mutex<crate::plan_mode::PlanModeTracker>>,
    skill_registry: Option<SkillRegistryImpl>,
    injected_bodies: HashSet<String>,
    goal_gate: GoalGate,
    messages: Vec<ChatMessage>,
    system_prompt: String,
    session_ctx: SessionContext,
    tools: Vec<ToolDefinition>,
    query: String,
    iteration_cap: Option<usize>,
}

/// How a single loop iteration ended.
enum IterationOutcome {
    /// The model produced a final answer and every turn-end gate passed.
    Done,
    /// A tool batch ran or a turn-end gate asked the model to continue.
    Continue,
}

pub(crate) async fn run_loop(
    mut ctx: RunContext,
    tx: &mut Sender<RunEvent>,
) -> Result<(), RuntimeError> {
    let mut turn = prepare_turn(&mut ctx, tx).await?;

    let mut iteration = 0usize;
    loop {
        if let Some(limit) = turn.iteration_cap {
            if iteration >= limit {
                return Err(RuntimeError::MaxIterations(limit));
            }
        }
        iteration += 1;
        match run_iteration(&ctx, &mut turn, iteration, tx).await? {
            IterationOutcome::Done => break,
            IterationOutcome::Continue => {}
        }
    }

    finish_run(&ctx, &turn.messages, tx).await;
    Ok(())
}

/// Turn setup: lifecycle/telemetry start events, todo store, plan tracker,
/// skill registry and selection, memory recall, standing orders and system
/// prompt assembly, and the session-goal gate. Returns the mutable per-turn
/// state the iteration loop works on.
async fn prepare_turn(
    ctx: &mut RunContext,
    tx: &mut Sender<RunEvent>,
) -> Result<PreparedTurn, RuntimeError> {
    send(
        tx,
        RunEvent::Lifecycle {
            phase: LifecyclePhase::Start,
            error: None,
        },
    )
    .await;

    if let Some(telemetry) = &ctx.telemetry {
        telemetry
            .log_session_event(SessionMetric::SessionStarted {
                session_id: ctx.request.session_id.clone(),
                agent_id: ctx.request.agent_id.clone(),
                model_ref: ctx.request.model_ref.clone(),
            })
            .await;
    }

    let workspace = resolve_workspace(
        &ctx.config,
        &ctx.request.agent_id,
        ctx.request.workspace_override.as_deref(),
    );
    let fs = TokioFs;

    // Session todo store. Enabled by default; when disabled the store is still
    // created (so the tool can report availability) but no events are emitted.
    let todo_store: Option<crate::SharedTodoStore> = if ctx.config.todos.enabled {
        let base = crate::expand_tilde("~/.legion");
        let path = crate::todo::JsonTodoStore::path_for(
            &base,
            &ctx.request.agent_id,
            &ctx.request.session_id,
        );
        match crate::todo::JsonTodoStore::open_with_event_tx(path, Some(tx.clone())).await {
            Ok(store) => Some(Arc::new(store)),
            Err(err) => {
                tracing::warn!(error = %err, "failed to open todo store; todo_write will be unavailable");
                None
            }
        }
    } else {
        None
    };

    // Snapshot of the todo list for the turn-end gate. Reloaded after each tool
    // batch so the gate sees the latest state written by todo_write.
    let current_todos = if let Some(store) = &todo_store {
        match store.load().await {
            Ok(list) => list,
            Err(err) => {
                tracing::warn!(error = %err, "failed to load todo list");
                crate::todo::TodoList::default()
            }
        }
    } else {
        crate::todo::TodoList::default()
    };

    // Plan-mode tracker. Load persisted state for the session when available;
    // otherwise start fresh in the session directory.
    let plan_tracker = if let Some(tracker) = ctx.request.plan_mode_tracker.clone() {
        tracker
    } else {
        let session_dir =
            crate::expand_tilde(&format!("~/.legion/sessions/{}", ctx.request.session_id));
        match crate::plan_mode::PlanModeTracker::load(&session_dir).await {
            Ok(tracker) => Arc::new(tokio::sync::Mutex::new(tracker)),
            Err(err) => {
                tracing::warn!(error = %err, "failed to load plan mode state; starting fresh");
                Arc::new(tokio::sync::Mutex::new(
                    crate::plan_mode::PlanModeTracker::new(&session_dir),
                ))
            }
        }
    };

    let skills_config = &ctx.config.agents.defaults.skills;
    let skill_registry = if skills_config.enabled {
        let mut skill_dirs = skills_config.dirs.clone();
        for dir in [".agents/skills", ".legion/skills"] {
            let p = workspace.join(dir);
            if fs.exists(&p).await {
                skill_dirs.push(p);
            }
        }

        let mut registry = SkillRegistryImpl::new();
        let report = registry.load(&skill_dirs).await;
        if !report.loaded.is_empty() || !report.failed.is_empty() || !ctx.plugin_skills.is_empty() {
            tracing::info!(
                loaded = report.loaded.len(),
                failed = report.failed.len(),
                plugin_skills = ctx.plugin_skills.len(),
                "loaded skills"
            );
        }
        for skill in ctx.plugin_skills.drain(..) {
            registry.add(skill);
        }
        for (path, err) in &report.failed {
            tracing::warn!(path = %path.display(), error = %err, "failed to load skill");
        }
        Some(registry)
    } else {
        None
    };

    let (skill_block, active_skills, initial_body_block, injected_bodies) =
        if let Some(registry) = skill_registry.as_ref() {
            let block = registry.summary_block(skills_config.max_summary_tokens);
            let names: Vec<String> = registry
                .all()
                .iter()
                .map(|s| s.frontmatter.name.clone())
                .collect();
            let candidates: Vec<&legion_skills::Skill> = registry.all().iter().collect();
            let selector: Arc<dyn SkillSelector> =
                if let Some(model_ref) = &skills_config.selector_model {
                    Arc::new(LlmSkillSelector::new(
                        ctx.provider_router.clone(),
                        model_ref,
                    ))
                } else {
                    Arc::new(KeywordSkillSelector::new())
                };
            let selected = selector
                .select(
                    &ctx.request.user_message,
                    &candidates,
                    skills_config.max_triggered_skills,
                )
                .await;
            let relevant: Vec<&legion_skills::Skill> =
                selected.into_iter().map(|idx| candidates[idx]).collect();
            let body_block = skill_body_block(&relevant, skills_config.max_body_tokens);
            let injected: HashSet<String> = relevant
                .iter()
                .map(|s| s.frontmatter.name.clone())
                .collect();
            (Some(block), names, Some(body_block), injected)
        } else {
            (None, Vec::new(), None, HashSet::new())
        };

    // Phase C: per-turn recall with optional LLM re-ranking and cross-turn
    // de-duplication via the surfaced store. The assembled notes are handed to
    // `assemble_system_prompt` directly, bypassing the legacy MEMORY.md-gated
    // search inside the prompt builder.
    let recalled_notes = if ctx.recall_config.limit == 0 {
        Vec::new()
    } else {
        let already = ctx
            .surfaced
            .load(&ctx.request.agent_id, &ctx.request.session_id)
            .await;
        let recent_tools: Vec<String> = ctx
            .tool_registry
            .definitions()
            .iter()
            .map(|d| d.name.clone())
            .collect();
        let limit = ctx.recall_config.limit.max(1);
        let recall_limit = if ctx.selector.is_some() {
            limit * 3
        } else {
            limit
        };
        let recall_ctx = RecallContext {
            already_surfaced: already,
            recent_tools,
            limit: recall_limit,
        };
        let mut notes = ctx
            .memory_backend
            .recall(&ctx.request.user_message, &recall_ctx)
            .await
            .unwrap_or_default();
        if let Some(sel) = &ctx.selector {
            notes = sel.select(&ctx.request.user_message, notes, limit).await;
        } else {
            notes.truncate(limit);
        }
        let new_ids: Vec<String> = notes.iter().map(|n| n.id.clone()).collect();
        ctx.surfaced
            .append(&ctx.request.agent_id, &ctx.request.session_id, &new_ids)
            .await;
        notes
    };

    let agent_cfg = ctx
        .config
        .agents
        .list
        .iter()
        .find(|a| a.id == ctx.request.agent_id);
    // Standing orders (automation-advanced gap Phase A): merge global
    // (`agents.defaults`) orders first, then the per-agent ones.
    let mut standing_orders = ctx.config.agents.defaults.standing_orders.clone();
    if let Some(cfg) = agent_cfg {
        standing_orders.extend(cfg.standing_orders.iter().cloned());
    }
    let prompt_report = assemble_system_prompt_report(
        &workspace,
        &fs,
        Some(ctx.memory_backend.as_ref()),
        &ctx.request.user_message,
        ctx.request.system_prompt.as_deref(),
        skill_block.as_deref(),
        initial_body_block.as_deref(),
        Some(recalled_notes.as_slice()),
        agent_cfg,
        &standing_orders,
        ctx.config.todos.enabled,
    )
    .await
    .map_err(|e| RuntimeError::Context(e.to_string()))?;

    // Prompt dump (prompt-management Phase C): enabled globally via
    // `promptDump.enabled` or per run via `--dump-prompts`.
    if ctx.config.prompt_dump.enabled || ctx.request.dump_prompts {
        let dump_dir = crate::expand_tilde("~/.legion/dump-prompts");
        match prompt_report.write_dump(&dump_dir, &ctx.request.session_id) {
            Ok(path) => tracing::debug!(path = %path.display(), "wrote prompt dump"),
            Err(e) => tracing::warn!(error = %e, "failed to write prompt dump"),
        }
    }

    let cache_blocks = prompt_report.split_for_prompt_cache(ctx.config.compaction.use_prompt_cache);
    let system_prompt = prompt_report.text;

    // Session goal: the turn-end goal gate keeps the run going while a goal
    // is active. Sub-agent runs share the parent's session key, so the gate
    // only engages for top-level runs (depth 0).
    let goal_gate = if ctx.config.goals.enabled && ctx.request.depth == 0 {
        GoalGate::new(ctx.goal_store.clone(), ctx.request.session_id.clone())
    } else {
        GoalGate::disabled()
    };
    let active_goal = goal_gate.load_active().await;

    let mut messages = Vec::new();
    if !system_prompt.is_empty() {
        // Prompt-cache wiring (providers gap Phase C): when prompt caching is
        // enabled, mark the stable leading prefix as a cache breakpoint so
        // providers that support caching (Anthropic `cache_control`) can reuse
        // it across turns; other providers simply see two system messages.
        for (block, cache_breakpoint) in cache_blocks {
            let msg = ChatMessage::system(block);
            messages.push(if cache_breakpoint {
                msg.with_cache_breakpoint()
            } else {
                msg
            });
        }
    }
    // Inject the active goal as a user-role context line (in-memory only; the
    // transcript stays clean). This replaces the former TUI-side prepend so
    // gateway and channel sessions get the same context.
    let user_message = match &active_goal {
        Some(goal) => format!("{}\n\n{}", goal.context_line(), ctx.request.user_message),
        None => ctx.request.user_message.clone(),
    };
    messages.extend(std::mem::take(&mut ctx.request.history));
    messages.push(ChatMessage::user(&user_message));

    let session_ctx = SessionContext::new(
        active_skills,
        ctx.tool_registry.clone(),
        Some(ctx.memory_backend.clone()),
    );

    let tools: Vec<_> = match &ctx.request.allowed_tools {
        Some(allowed) => ctx
            .tool_registry
            .definitions()
            .into_iter()
            .filter(|d| allowed.iter().any(|a| a == &d.name))
            .collect(),
        None => ctx.tool_registry.definitions(),
    };
    let query = ctx.request.user_message.clone();

    // Resolve the iteration cap: request override > per-agent config > runtime default.
    let base_cap = ctx
        .request
        .max_iterations
        .or_else(|| {
            ctx.config
                .agents
                .list
                .iter()
                .find(|a| a.id == ctx.request.agent_id)
                .and_then(|a| a.max_iterations)
        })
        .or(ctx.max_iterations);
    // While a goal is active the goal itself is the limiter: lift the
    // per-run iteration cap so goal pursuit is not cut short.
    let iteration_cap = if active_goal.is_some() {
        None
    } else {
        base_cap
    };

    Ok(PreparedTurn {
        workspace,
        todo_store,
        current_todos,
        plan_tracker,
        skill_registry,
        injected_bodies,
        goal_gate,
        messages,
        system_prompt,
        session_ctx,
        tools,
        query,
        iteration_cap,
    })
}

/// One turn of the agent loop: compaction, the LLM call, turn-end gating, and
/// the tool batch (including permission narrowing and skill re-triggering).
async fn run_iteration(
    ctx: &RunContext,
    turn: &mut PreparedTurn,
    iteration: usize,
    tx: &mut Sender<RunEvent>,
) -> Result<IterationOutcome, RuntimeError> {
    let request = &ctx.request;
    let input_tokens =
        crate::token_counter::estimate_total_tokens(&turn.messages, &turn.system_prompt);
    let turn_start = Instant::now();
    if let Some(telemetry) = &ctx.telemetry {
        telemetry
            .log_session_event(SessionMetric::Turn {
                session_id: request.session_id.clone(),
                turn_number: iteration,
                input_tokens,
                model_ref: request.model_ref.clone(),
            })
            .await;
    }

    let tokens_before_compaction =
        crate::token_counter::estimate_total_tokens(&turn.messages, &turn.system_prompt);
    if let Some((summary, boundary)) = ctx
        .compactor
        .compact_if_needed(
            &mut turn.messages,
            &turn.system_prompt,
            &ctx.provider_router,
            &request.model_ref,
            Some(&turn.session_ctx),
            &turn.query,
        )
        .await?
    {
        // The compacted history minus the leading system prompt (rebuilt
        // from the workspace on resume) is what the transcript must keep
        // after the boundary marker.
        let resume_head: Vec<ChatMessage> = match turn.messages.first() {
            Some(first) if first.role == ChatRole::System => turn.messages[1..].to_vec(),
            _ => turn.messages.clone(),
        };
        send(
            tx,
            RunEvent::Compaction {
                summary,
                boundary,
                resume_head,
            },
        )
        .await;

        if let Some(telemetry) = &ctx.telemetry {
            let tokens_after_compaction =
                crate::token_counter::estimate_total_tokens(&turn.messages, &turn.system_prompt);
            telemetry
                .log_session_event(SessionMetric::Compaction {
                    session_id: request.session_id.clone(),
                    turn_number: iteration,
                    tokens_before: tokens_before_compaction,
                    tokens_after: tokens_after_compaction,
                })
                .await;
        }
    }

    let mut req = ChatRequest::new(&request.model_ref, Vec::new());
    if !turn.tools.is_empty() {
        req.tools = Some(turn.tools.clone());
    }

    let mut stream = chat_with_ptl_retry(
        &ctx.provider_router,
        &request.model_ref,
        req,
        &mut turn.messages,
    )
    .await?;
    let (assistant_text, pending_tool_calls) = consume_assistant_stream(&mut stream, tx).await?;

    let mut assistant_msg = ChatMessage::assistant(&assistant_text);
    if !pending_tool_calls.is_empty() {
        assistant_msg.tool_calls = Some(pending_tool_calls.clone());
    }
    turn.messages.push(assistant_msg);

    let output_tokens = crate::token_counter::count_tokens(&assistant_text);
    let turn_duration_ms = turn_start.elapsed().as_millis() as u64;
    if let Some(telemetry) = &ctx.telemetry {
        telemetry
            .log_session_event(SessionMetric::TurnCompleted {
                session_id: request.session_id.clone(),
                turn_number: iteration,
                output_tokens,
                tool_calls: pending_tool_calls.len(),
                duration_ms: turn_duration_ms,
            })
            .await;
    }

    if pending_tool_calls.is_empty() {
        // Turn-end gating: if required todo patterns are not satisfied,
        // remind the model instead of ending the turn.
        match ctx.todo_gate.check(&turn.current_todos, &assistant_text) {
            crate::todo_gate::TodoGateResult::Pass => {
                // Goal gate: keep pursuing an active goal ("goal turns")
                // instead of ending the run.
                match turn.goal_gate.check().await {
                    GoalGateResult::Pass => {
                        // Final answer reached.
                        return Ok(IterationOutcome::Done);
                    }
                    GoalGateResult::Continue { reminder } => {
                        // A goal may also be created mid-run via
                        // create_goal; from here on the goal itself is
                        // the limiter, so lift the iteration cap.
                        turn.iteration_cap = None;
                        turn.messages.push(ChatMessage::system(reminder));
                        return Ok(IterationOutcome::Continue);
                    }
                }
            }
            other => {
                if let Some(reminder) = todo_gate_reminder(&other) {
                    turn.messages.push(ChatMessage::system(reminder));
                }
                return Ok(IterationOutcome::Continue);
            }
        }
    }

    let runtime_calls: Vec<ToolCall> = pending_tool_calls.iter().map(ToolCall::from).collect();
    // Permission narrowing (multi-agent Phase A): calls outside the run's
    // allowed subset are not executed; they get a structured denial result
    // so the model sees an explicit refusal rather than a silent drop.
    let (allowed_calls, denied_calls): (Vec<ToolCall>, Vec<ToolCall>) = match &request.allowed_tools
    {
        Some(allowed) => runtime_calls
            .into_iter()
            .partition(|c| allowed.iter().any(|a| a == &c.name)),
        None => (runtime_calls, Vec::new()),
    };
    let batches = partition_tool_calls(ctx.tool_registry.as_ref(), &allowed_calls);

    // Build the approval gate and policy decider for this run. The gate is
    // scoped to the run so session-level denies do not leak across sessions.
    let approval_gate = request.approval_gate.clone().unwrap_or_else(|| {
        Arc::new(ApprovalGate::new(
            Arc::new(NoOpApprovalNotifier),
            Duration::from_secs(300),
        ))
    });
    let approval_ctx = ApprovalCtx {
        gate: approval_gate,
        interactive: request.interactive,
        permission_mode: PermissionMode::Default,
    };
    let question_gate = request.question_gate.clone().unwrap_or_else(|| {
        Arc::new(QuestionGate::new(
            Arc::new(NoOpQuestionNotifier),
            Duration::from_secs(300),
        ))
    });
    let question_ctx = QuestionCtx {
        gate: question_gate,
        interactive: request.interactive,
    };
    let can_use_tool = build_policy_decider(ctx.tool_registry.clone());

    // Snapshot the conversation so a Fork sub-agent spawned by a tool in
    // this batch inherits the parent's context up to the tool-call turn.
    let history_snapshot = Arc::new(turn.messages.clone());

    let tool_messages = run_tool_batches(
        batches,
        &turn.workspace,
        &request.session_id,
        &request.agent_id,
        request.sender.as_deref(),
        &ctx.tool_registry,
        Some(&can_use_tool),
        Some(ctx.memory_backend.clone()),
        turn.session_ctx.viewed_files_sink(),
        Some(approval_ctx),
        Some(question_ctx),
        request.allowed_tools.clone(),
        ctx.spawner.clone(),
        ctx.messenger.clone(),
        ctx.swarm.clone(),
        request.depth,
        Some(history_snapshot),
        turn.todo_store.clone(),
        None,
        Some(turn.plan_tracker.clone()),
        ctx.telemetry.clone(),
        iteration,
        tx,
    )
    .await;
    turn.messages.extend(tool_messages);

    if let Some(store) = &turn.todo_store {
        match store.load().await {
            Ok(list) => turn.current_todos = list,
            Err(err) => tracing::warn!(error = %err, "failed to reload todo list"),
        }
    }

    // Complete any pending plan-mode exit now that the turn has finished,
    // and persist the tracker state.
    {
        let mut guard = turn.plan_tracker.lock().await;
        guard.finalize_exit_if_pending();
        if let Err(err) = guard.save().await {
            tracing::warn!(error = %err, "failed to save plan mode state");
        }
    }

    for denied in denied_calls {
        send(
            tx,
            RunEvent::ToolStart {
                tool_call: denied.clone(),
            },
        )
        .await;
        let result = ToolResult::error(format!(
            "tool '{}' is not permitted in this sub-agent run",
            denied.name
        ));
        send(
            tx,
            RunEvent::ToolEnd {
                tool_call: denied.clone(),
                result: result.clone(),
                canonical_meta: None,
            },
        )
        .await;
        turn.messages.push(ChatMessage {
            role: ChatRole::Tool,
            content: result.content,
            name: None,
            tool_calls: None,
            tool_call_id: Some(denied.id),
            cache_breakpoint: false,
        });
    }

    if let Some(registry) = turn.skill_registry.as_ref() {
        let skills_config = &ctx.config.agents.defaults.skills;
        let viewed_files = turn.session_ctx.viewed_files();
        let touched_files: Vec<String> = {
            let mut set = HashSet::new();
            for path in viewed_files {
                if let Ok(rel) = path.strip_prefix(&turn.workspace) {
                    let _ = set.insert(rel.to_string_lossy().to_string());
                }
                if let Some(name) = path.file_name() {
                    let _ = set.insert(name.to_string_lossy().to_string());
                }
            }
            set.into_iter().collect()
        };

        let matched = registry.match_paths(&touched_files);
        let new_matches: Vec<&legion_skills::Skill> = matched
            .into_iter()
            .filter(|s| !turn.injected_bodies.contains(&s.frontmatter.name))
            .take(skills_config.max_triggered_skills)
            .collect();

        if !new_matches.is_empty() {
            let names: Vec<String> = new_matches
                .iter()
                .map(|s| s.frontmatter.name.clone())
                .collect();
            let body_block = skill_body_block(&new_matches, skills_config.max_body_tokens);
            if !body_block.trim().is_empty() {
                turn.messages.push(ChatMessage::system(body_block));
                for name in &names {
                    turn.injected_bodies.insert(name.clone());
                }
                tracing::info!(
                    skill_names = ?names,
                    "injected skill bodies triggered by file paths"
                );
            }
        }
    }

    Ok(IterationOutcome::Continue)
}

/// Run finalization: hand the transcript to the background extractors and
/// emit the End lifecycle event.
async fn finish_run(ctx: &RunContext, messages: &[ChatMessage], tx: &mut Sender<RunEvent>) {
    if let Some(extractor) = &ctx.auto_extractor {
        extractor.clone().spawn(
            ctx.request.agent_id.clone(),
            ctx.request.session_id.clone(),
            messages.to_vec(),
        );
    }
    if let Some(extractor) = &ctx.commitment_extractor {
        extractor.spawn_extract(
            ctx.request.agent_id.clone(),
            ctx.request.session_id.clone(),
            messages.to_vec(),
        );
    }

    send(
        tx,
        RunEvent::Lifecycle {
            phase: LifecyclePhase::End,
            error: None,
        },
    )
    .await;
}

async fn consume_assistant_stream(
    stream: &mut legion_provider::types::ChatStream,
    tx: &mut Sender<RunEvent>,
) -> Result<(String, Vec<ProviderToolCall>), RuntimeError> {
    let mut text = String::new();
    let mut pending: Vec<ProviderToolCall> = Vec::new();

    while let Some(item) = stream.next().await {
        let chunk: ChatChunk = item?;

        if !chunk.delta.is_empty() {
            text.push_str(&chunk.delta);
            send(tx, RunEvent::AssistantDelta { delta: chunk.delta }).await;
        }

        if let Some(tcs) = chunk.tool_calls {
            pending.extend(tcs);
        }

        match chunk.finish_reason {
            Some(FinishReason::Stop) | Some(FinishReason::Length) => break,
            Some(FinishReason::ToolCalls) => break,
            Some(FinishReason::ContentFilter) => {
                return Err(RuntimeError::Context("content filtered by provider".into()));
            }
            None => {
                // Keep streaming until the provider signals completion.
            }
        }
    }

    Ok((text, pending))
}

async fn send(tx: &mut Sender<RunEvent>, event: RunEvent) {
    let _ = tx.send(event).await;
}

pub(crate) const MAX_PTL_RETRIES: usize = 3;
const PTL_TRUNCATE_PCT: f64 = 0.2;

/// Call the provider, automatically truncating the oldest non-system messages
/// and retrying when the provider reports the prompt is too long.
async fn chat_with_ptl_retry(
    provider_router: &ProviderRouter,
    model_ref: &str,
    base_req: ChatRequest,
    messages: &mut Vec<ChatMessage>,
) -> Result<legion_provider::types::ChatStream, RuntimeError> {
    for attempt in 0..MAX_PTL_RETRIES {
        let mut req = base_req.clone();
        req.messages = messages.clone();
        match provider_router.chat(model_ref, req).await {
            Ok(stream) => return Ok(stream),
            Err(ProviderError::PromptTooLong) => {
                tracing::warn!(
                    attempt,
                    "provider returned prompt too long; truncating oldest messages"
                );
                truncate_head_for_ptl(messages);
            }
            Err(e) => return Err(e.into()),
        }
    }
    Err(ProviderError::PromptTooLong.into())
}

/// Remove the oldest 20% of non-system messages (at least one) to recover from
/// a prompt-too-long error.
fn truncate_head_for_ptl(messages: &mut Vec<ChatMessage>) {
    let non_system_indices: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role != ChatRole::System)
        .map(|(i, _)| i)
        .collect();
    let remove_count = ((non_system_indices.len() as f64 * PTL_TRUNCATE_PCT)
        .ceil()
        .max(1.0) as usize)
        .min(non_system_indices.len());
    let remove: HashSet<usize> = non_system_indices.into_iter().take(remove_count).collect();

    let mut i = 0;
    messages.retain(|_| {
        let keep = !remove.contains(&i);
        i += 1;
        keep
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use legion_provider::types::ChatRole;

    #[test]
    fn truncate_head_for_ptl_removes_oldest_non_system_messages() {
        let mut messages = vec![
            ChatMessage::system("sys"),
            ChatMessage::user("old1"),
            ChatMessage::user("old2"),
            ChatMessage::user("old3"),
            ChatMessage::user("old4"),
            ChatMessage::user("recent"),
        ];
        truncate_head_for_ptl(&mut messages);
        // 20% of 5 non-system messages rounded up = 1 removed.
        assert_eq!(messages.len(), 5);
        assert_eq!(messages[0].role, ChatRole::System);
        assert_eq!(messages[1].content, "old2");
    }
}
