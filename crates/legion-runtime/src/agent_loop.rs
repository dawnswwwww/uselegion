use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use futures::channel::mpsc::Sender;
use futures::{SinkExt, StreamExt};

use crate::approval::{ApprovalCtx, ApprovalGate, NoOpApprovalNotifier, PermissionMode};
use crate::auto_extract::AutoExtractor;
use crate::commitments::CommitmentExtractor;
use crate::compaction::Compactor;
use crate::context::{
    Filesystem, SessionContext, TokioFs, assemble_system_prompt_report, resolve_workspace,
};
use crate::context_engine::{ContextEngine, LegacyContextEngine};
use crate::memory::{MemoryBackend, RecallContext};
use crate::messenger::AgentMessenger;
use crate::question::{NoOpQuestionNotifier, QuestionCtx, QuestionGate};
use crate::recall_selector::LlmRecallSelector;
use crate::skill_selector::{KeywordSkillSelector, LlmSkillSelector, SkillSelector};
use crate::skills_prompt::skill_body_block;
use crate::subagent::SubagentSpawner;
use crate::surfaced::SurfacedStore;
use crate::swarm::SwarmManager;
use crate::tool_pipeline::{partition_tool_calls, run_tool_batches};
use crate::tools::{ToolCall, ToolRegistry, ToolResult, build_policy_decider};
use crate::types::{LifecyclePhase, RunEvent, RunRequest, RunStream, RuntimeError};
use legion_core::config::{Config, RecallConfig};
use legion_provider::router::ProviderRouter;
use legion_provider::types::{
    ChatChunk, ChatMessage, ChatRequest, ChatRole, FinishReason, ProviderError,
    ToolCall as ProviderToolCall,
};
use legion_skills::{Skill, SkillRegistry, SkillRegistryImpl};
use std::time::Duration;

pub const DEFAULT_MAX_ITERATIONS: usize = 10;

/// The built-in agent runtime.
///
/// The runtime holds a provider router per agent so that each agent can use its
/// own `auth-profiles.json` while sharing the same tool and memory backends.
pub struct AgentRuntime {
    provider_routers: HashMap<String, Arc<ProviderRouter>>,
    tool_registry: Arc<dyn ToolRegistry>,
    memory_backend: Arc<dyn MemoryBackend>,
    config: Config,
    /// Runtime-wide default iteration cap. `None` means no limit.
    max_iterations: Option<usize>,
    plugin_skills: Vec<Skill>,
    auto_extractor: Option<Arc<AutoExtractor>>,
    /// Optional background commitment extractor (automation-advanced Phase B).
    /// `None` disables inference of natural-language follow-ups.
    commitment_extractor: Option<Arc<dyn CommitmentExtractor>>,
    recall_config: RecallConfig,
    selector: Option<Arc<LlmRecallSelector>>,
    surfaced: SurfacedStore,
    /// Late-bound sub-agent spawner (multi-agent Phase A). Wired by the gateway
    /// after the `Arc<AgentRuntime>` is constructed to break the spawn/runtime
    /// construction cycle. `None` until `set_spawner` is called.
    spawner: Mutex<Option<Arc<dyn SubagentSpawner>>>,
    /// Late-bound agent messenger (tools-p1p2 Phase B). Same late-binding
    /// pattern as the spawner; `None` until `set_messenger` is called.
    messenger: Mutex<Option<Arc<dyn AgentMessenger>>>,
    /// Late-bound swarm manager (multi-agent Phase D). Same late-binding
    /// pattern as the spawner; `None` until `set_swarm` is called.
    swarm: Mutex<Option<Arc<SwarmManager>>>,
}

impl AgentRuntime {
    pub fn new(
        provider_router: Arc<ProviderRouter>,
        tool_registry: Arc<dyn ToolRegistry>,
        memory_backend: Arc<dyn MemoryBackend>,
        config: Config,
    ) -> Self {
        let mut provider_routers = HashMap::new();
        provider_routers.insert("main".to_string(), provider_router);

        let recall_config = config.memory.recall.clone();
        Self {
            provider_routers,
            tool_registry,
            memory_backend,
            config: config.clone(),
            max_iterations: config.agents.defaults.max_iterations,
            plugin_skills: Vec::new(),
            auto_extractor: None,
            commitment_extractor: None,
            recall_config,
            selector: None,
            surfaced: SurfacedStore::default(),
            spawner: Mutex::new(None),
            messenger: Mutex::new(None),
            swarm: Mutex::new(None),
        }
    }

    /// Register skills provided by plugins so they are merged into the agent's
    /// skill registry for every run.
    pub fn with_plugin_skills(mut self, skills: Vec<Skill>) -> Self {
        self.plugin_skills = skills;
        self
    }

    /// Register a provider router for a non-main agent.
    pub fn with_agent_router(
        mut self,
        agent_id: impl Into<String>,
        router: Arc<ProviderRouter>,
    ) -> Self {
        self.provider_routers.insert(agent_id.into(), router);
        self
    }

    pub fn with_max_iterations(mut self, max_iterations: Option<usize>) -> Self {
        self.max_iterations = max_iterations;
        self
    }

    /// Attach a background auto-extractor (memory-layers Phase B). `None` (the
    /// default) keeps memory fully manual.
    pub fn with_auto_extractor(mut self, extractor: Option<Arc<AutoExtractor>>) -> Self {
        self.auto_extractor = extractor;
        self
    }

    /// Attach a background commitment extractor (automation-advanced Phase B).
    /// `None` (the default) disables inferred commitments.
    pub fn with_commitment_extractor(
        mut self,
        extractor: Option<Arc<dyn CommitmentExtractor>>,
    ) -> Self {
        self.commitment_extractor = extractor;
        self
    }

    /// Attach an optional LLM recall re-ranker (memory-layers Phase C). `None`
    /// (the default) keeps keyword/score ranking from the backend.
    pub fn with_selector(mut self, selector: Option<Arc<LlmRecallSelector>>) -> Self {
        self.selector = selector;
        self
    }

    /// Override the surfaced-ids store used to suppress memories already injected
    /// in earlier turns of the same session (memory-layers Phase C).
    pub fn with_surfaced(mut self, surfaced: SurfacedStore) -> Self {
        self.surfaced = surfaced;
        self
    }

    /// Borrow the resolved configuration (used by the sub-agent spawner to
    /// resolve per-agent models).
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Late-bind the sub-agent spawner (multi-agent Phase A). Called by the
    /// gateway after the `Arc<AgentRuntime>` exists. No-ops if the internal
    /// mutex is poisoned (which cannot happen during single-threaded startup).
    pub fn set_spawner(&self, spawner: Arc<dyn SubagentSpawner>) {
        if let Ok(mut guard) = self.spawner.lock() {
            *guard = Some(spawner);
        }
    }

    fn spawner(&self) -> Option<Arc<dyn SubagentSpawner>> {
        self.spawner.lock().ok().and_then(|g| g.clone())
    }

    /// Late-bind the agent messenger (tools-p1p2 Phase B). Called by the
    /// gateway after the `Arc<AgentRuntime>` exists. No-ops if the internal
    /// mutex is poisoned (which cannot happen during single-threaded startup).
    pub fn set_messenger(&self, messenger: Arc<dyn AgentMessenger>) {
        if let Ok(mut guard) = self.messenger.lock() {
            *guard = Some(messenger);
        }
    }

    fn messenger(&self) -> Option<Arc<dyn AgentMessenger>> {
        self.messenger.lock().ok().and_then(|g| g.clone())
    }

    /// Late-bind the swarm manager (multi-agent Phase D). Called by the
    /// gateway after the `Arc<AgentRuntime>` exists. No-ops if the internal
    /// mutex is poisoned (which cannot happen during single-threaded startup).
    pub fn set_swarm(&self, swarm: Arc<SwarmManager>) {
        if let Ok(mut guard) = self.swarm.lock() {
            *guard = Some(swarm);
        }
    }

    fn swarm(&self) -> Option<Arc<SwarmManager>> {
        self.swarm.lock().ok().and_then(|g| g.clone())
    }

    /// Start an agent run and return a stream of runtime events.
    pub fn run(&self, request: RunRequest) -> Result<RunStream, RuntimeError> {
        let provider_router = self
            .provider_routers
            .get(&request.agent_id)
            .cloned()
            .or_else(|| self.provider_routers.get("main").cloned())
            .ok_or_else(|| {
                RuntimeError::Provider(legion_provider::types::ProviderError::ProviderNotFound(
                    request.agent_id.clone(),
                ))
            })?;

        let context_engine = self.build_context_engine();
        context_engine.run(provider_router, request, self.max_iterations)
    }

    fn build_context_engine(&self) -> Arc<dyn ContextEngine> {
        match self.config.agent_runtime.context_engine.as_deref() {
            None | Some("legacy") => Arc::new(
                LegacyContextEngine::new(
                    self.tool_registry.clone(),
                    self.memory_backend.clone(),
                    self.config.clone(),
                )
                .with_plugin_skills(self.plugin_skills.clone())
                .with_auto_extractor(self.auto_extractor.clone())
                .with_commitment_extractor(self.commitment_extractor.clone())
                .with_recall_config(self.recall_config.clone())
                .with_selector(self.selector.clone())
                .with_surfaced(self.surfaced.clone())
                .with_spawner(self.spawner())
                .with_messenger(self.messenger())
                .with_swarm(self.swarm()),
            ),
            Some(other) => {
                tracing::warn!(
                    engine = other,
                    "unknown context engine; falling back to legacy"
                );
                Arc::new(
                    LegacyContextEngine::new(
                        self.tool_registry.clone(),
                        self.memory_backend.clone(),
                        self.config.clone(),
                    )
                    .with_plugin_skills(self.plugin_skills.clone())
                    .with_auto_extractor(self.auto_extractor.clone())
                    .with_commitment_extractor(self.commitment_extractor.clone())
                    .with_recall_config(self.recall_config.clone())
                    .with_selector(self.selector.clone())
                    .with_surfaced(self.surfaced.clone())
                    .with_spawner(self.spawner())
                    .with_messenger(self.messenger())
                    .with_swarm(self.swarm()),
                )
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_loop(
    provider_router: Arc<ProviderRouter>,
    tool_registry: Arc<dyn ToolRegistry>,
    memory_backend: Arc<dyn MemoryBackend>,
    compactor: Arc<Compactor>,
    config: Config,
    request: RunRequest,
    max_iterations: Option<usize>,
    plugin_skills: Vec<Skill>,
    auto_extractor: Option<Arc<AutoExtractor>>,
    commitment_extractor: Option<Arc<dyn CommitmentExtractor>>,
    recall_config: RecallConfig,
    selector: Option<Arc<LlmRecallSelector>>,
    surfaced: SurfacedStore,
    spawner: Option<Arc<dyn SubagentSpawner>>,
    messenger: Option<Arc<dyn AgentMessenger>>,
    swarm: Option<Arc<SwarmManager>>,
    tx: &mut Sender<RunEvent>,
) -> Result<(), RuntimeError> {
    send(
        tx,
        RunEvent::Lifecycle {
            phase: LifecyclePhase::Start,
            error: None,
        },
    )
    .await;

    let workspace = resolve_workspace(
        &config,
        &request.agent_id,
        request.workspace_override.as_deref(),
    );
    let fs = TokioFs;

    // Session todo store. Enabled by default; when disabled the store is still
    // created (so the tool can report availability) but no events are emitted.
    let todo_store: Option<crate::SharedTodoStore> = if config.todos.enabled {
        let base = crate::expand_tilde("~/.legion");
        let path =
            crate::todo::JsonTodoStore::path_for(&base, &request.agent_id, &request.session_id);
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

    let skills_config = &config.agents.defaults.skills;
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
        if !report.loaded.is_empty() || !report.failed.is_empty() || !plugin_skills.is_empty() {
            tracing::info!(
                loaded = report.loaded.len(),
                failed = report.failed.len(),
                plugin_skills = plugin_skills.len(),
                "loaded skills"
            );
        }
        for skill in plugin_skills {
            registry.add(skill);
        }
        for (path, err) in &report.failed {
            tracing::warn!(path = %path.display(), error = %err, "failed to load skill");
        }
        Some(registry)
    } else {
        None
    };

    let (skill_block, active_skills, initial_body_block, mut injected_bodies) =
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
                    Arc::new(LlmSkillSelector::new(provider_router.clone(), model_ref))
                } else {
                    Arc::new(KeywordSkillSelector::new())
                };
            let selected = selector
                .select(
                    &request.user_message,
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
    let recalled_notes = if recall_config.limit == 0 {
        Vec::new()
    } else {
        let already = surfaced.load(&request.agent_id, &request.session_id).await;
        let recent_tools: Vec<String> = tool_registry
            .definitions()
            .iter()
            .map(|d| d.name.clone())
            .collect();
        let limit = recall_config.limit.max(1);
        let recall_limit = if selector.is_some() { limit * 3 } else { limit };
        let ctx = RecallContext {
            already_surfaced: already,
            recent_tools,
            limit: recall_limit,
        };
        let mut notes = memory_backend
            .recall(&request.user_message, &ctx)
            .await
            .unwrap_or_default();
        if let Some(sel) = &selector {
            notes = sel.select(&request.user_message, notes, limit).await;
        } else {
            notes.truncate(limit);
        }
        let new_ids: Vec<String> = notes.iter().map(|n| n.id.clone()).collect();
        surfaced
            .append(&request.agent_id, &request.session_id, &new_ids)
            .await;
        notes
    };

    let agent_cfg = config.agents.list.iter().find(|a| a.id == request.agent_id);
    // Standing orders (automation-advanced gap Phase A): merge global
    // (`agents.defaults`) orders first, then the per-agent ones.
    let mut standing_orders = config.agents.defaults.standing_orders.clone();
    if let Some(cfg) = agent_cfg {
        standing_orders.extend(cfg.standing_orders.iter().cloned());
    }
    let prompt_report = assemble_system_prompt_report(
        &workspace,
        &fs,
        Some(memory_backend.as_ref()),
        &request.user_message,
        request.system_prompt.as_deref(),
        skill_block.as_deref(),
        initial_body_block.as_deref(),
        Some(recalled_notes.as_slice()),
        agent_cfg,
        &standing_orders,
        config.todos.enabled,
    )
    .await
    .map_err(|e| RuntimeError::Context(e.to_string()))?;

    // Prompt dump (prompt-management Phase C): enabled globally via
    // `promptDump.enabled` or per run via `--dump-prompts`.
    if config.prompt_dump.enabled || request.dump_prompts {
        let dump_dir = crate::expand_tilde("~/.legion/dump-prompts");
        match prompt_report.write_dump(&dump_dir, &request.session_id) {
            Ok(path) => tracing::debug!(path = %path.display(), "wrote prompt dump"),
            Err(e) => tracing::warn!(error = %e, "failed to write prompt dump"),
        }
    }

    let cache_blocks = prompt_report.split_for_prompt_cache(config.compaction.use_prompt_cache);
    let system_prompt = prompt_report.text;

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
    messages.extend(request.history);
    messages.push(ChatMessage::user(&request.user_message));

    let session_ctx = SessionContext::new(
        active_skills,
        tool_registry.clone(),
        Some(memory_backend.clone()),
    );

    let tools: Vec<_> = match &request.allowed_tools {
        Some(allowed) => tool_registry
            .definitions()
            .into_iter()
            .filter(|d| allowed.iter().any(|a| a == &d.name))
            .collect(),
        None => tool_registry.definitions(),
    };
    let workspace_path: PathBuf = workspace;
    let query = request.user_message.clone();

    // Resolve the iteration cap: request override > per-agent config > runtime default.
    let max_iterations = request
        .max_iterations
        .or_else(|| {
            config
                .agents
                .list
                .iter()
                .find(|a| a.id == request.agent_id)
                .and_then(|a| a.max_iterations)
        })
        .or(max_iterations);

    let mut iteration = 0usize;
    loop {
        if let Some(limit) = max_iterations {
            if iteration >= limit {
                return Err(RuntimeError::MaxIterations(limit));
            }
        }

        if let Some((summary, boundary)) = compactor
            .compact_if_needed(
                &mut messages,
                &system_prompt,
                &provider_router,
                &request.model_ref,
                Some(&session_ctx),
                &query,
            )
            .await?
        {
            // The compacted history minus the leading system prompt (rebuilt
            // from the workspace on resume) is what the transcript must keep
            // after the boundary marker.
            let resume_head: Vec<ChatMessage> = match messages.first() {
                Some(first) if first.role == ChatRole::System => messages[1..].to_vec(),
                _ => messages.clone(),
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
        }

        let mut req = ChatRequest::new(&request.model_ref, Vec::new());
        if !tools.is_empty() {
            req.tools = Some(tools.clone());
        }

        let mut stream =
            chat_with_ptl_retry(&provider_router, &request.model_ref, req, &mut messages).await?;
        let (assistant_text, pending_tool_calls) =
            consume_assistant_stream(&mut stream, tx).await?;

        let mut assistant_msg = ChatMessage::assistant(&assistant_text);
        if !pending_tool_calls.is_empty() {
            assistant_msg.tool_calls = Some(pending_tool_calls.clone());
        }
        messages.push(assistant_msg);

        if pending_tool_calls.is_empty() {
            // Final answer reached.
            break;
        }

        let runtime_calls: Vec<ToolCall> = pending_tool_calls.iter().map(ToolCall::from).collect();
        // Permission narrowing (multi-agent Phase A): calls outside the run's
        // allowed subset are not executed; they get a structured denial result
        // so the model sees an explicit refusal rather than a silent drop.
        let (allowed_calls, denied_calls): (Vec<ToolCall>, Vec<ToolCall>) =
            match &request.allowed_tools {
                Some(allowed) => runtime_calls
                    .into_iter()
                    .partition(|c| allowed.iter().any(|a| a == &c.name)),
                None => (runtime_calls, Vec::new()),
            };
        let batches = partition_tool_calls(tool_registry.as_ref(), &allowed_calls);

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
        let can_use_tool = build_policy_decider(tool_registry.clone());

        // Snapshot the conversation so a Fork sub-agent spawned by a tool in
        // this batch inherits the parent's context up to the tool-call turn.
        let history_snapshot = Arc::new(messages.clone());

        let tool_messages = run_tool_batches(
            batches,
            &workspace_path,
            &request.session_id,
            &request.agent_id,
            request.sender.as_deref(),
            &tool_registry,
            Some(&can_use_tool),
            Some(memory_backend.clone()),
            session_ctx.viewed_files_sink(),
            Some(approval_ctx),
            Some(question_ctx),
            request.allowed_tools.clone(),
            spawner.clone(),
            messenger.clone(),
            swarm.clone(),
            request.depth,
            Some(history_snapshot),
            todo_store.clone(),
            tx,
        )
        .await;
        messages.extend(tool_messages);

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
                },
            )
            .await;
            messages.push(ChatMessage {
                role: ChatRole::Tool,
                content: result.content,
                name: None,
                tool_calls: None,
                tool_call_id: Some(denied.id),
                cache_breakpoint: false,
            });
        }

        if let Some(registry) = skill_registry.as_ref() {
            let viewed_files = session_ctx.viewed_files();
            let touched_files: Vec<String> = {
                let mut set = HashSet::new();
                for path in viewed_files {
                    if let Ok(rel) = path.strip_prefix(&workspace_path) {
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
                .filter(|s| !injected_bodies.contains(&s.frontmatter.name))
                .take(skills_config.max_triggered_skills)
                .collect();

            if !new_matches.is_empty() {
                let names: Vec<String> = new_matches
                    .iter()
                    .map(|s| s.frontmatter.name.clone())
                    .collect();
                let body_block = skill_body_block(&new_matches, skills_config.max_body_tokens);
                if !body_block.trim().is_empty() {
                    messages.push(ChatMessage::system(body_block));
                    for name in &names {
                        injected_bodies.insert(name.clone());
                    }
                    tracing::info!(
                        skill_names = ?names,
                        "injected skill bodies triggered by file paths"
                    );
                }
            }
        }

        iteration += 1;
    }

    if let Some(extractor) = auto_extractor {
        extractor.spawn(
            request.agent_id.clone(),
            request.session_id.clone(),
            messages.clone(),
        );
    }
    if let Some(extractor) = commitment_extractor {
        extractor.spawn_extract(
            request.agent_id.clone(),
            request.session_id.clone(),
            messages.clone(),
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
    Ok(())
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

const MAX_PTL_RETRIES: usize = 3;
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
    use crate::memory::{MemoryBackend, MemoryError, MemoryNote};
    use crate::tools::{
        Approval, Policy, Tool, ToolContext, ToolDefinitionExt, ToolError, ToolRegistry, ToolResult,
    };
    use async_trait::async_trait;
    use futures::StreamExt;

    fn open_policy() -> &'static Policy {
        use std::sync::OnceLock;
        static POLICY: OnceLock<Policy> = OnceLock::new();
        POLICY.get_or_init(|| Policy {
            approval: Approval::Off,
            permission_mode: None,
            allow_from: vec![],
            workspace_only: false,
        })
    }
    use legion_core::config::Config;
    use legion_provider::provider::Provider;
    use legion_provider::router::ProviderRouter;
    use legion_provider::types::ChatRole;
    use legion_provider::types::{
        ChatChunk, ChatRequest, ChatStream, EmbedRequest, Embedding, FinishReason, FunctionCall,
        ModelInfo, ProviderError, ToolCall as ProviderToolCall, ToolDefinition,
    };
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct EchoTool;

    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }

        fn description(&self) -> &str {
            "Echoes the provided message."
        }

        fn schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "msg": { "type": "string" }
                },
                "required": ["msg"]
            })
        }

        fn policy(&self) -> &Policy {
            open_policy()
        }

        async fn execute(
            &self,
            params: serde_json::Value,
            _ctx: ToolContext,
        ) -> Result<ToolResult, ToolError> {
            let msg = params["msg"].as_str().unwrap_or("");
            Ok(ToolResult::ok(format!("echo: {msg}")))
        }
    }

    /// Tool that always requires approval (used to exercise the approval gate).
    struct PromptTool;

    fn required_policy() -> &'static Policy {
        use std::sync::OnceLock;
        static POLICY: OnceLock<Policy> = OnceLock::new();
        POLICY.get_or_init(|| Policy {
            approval: Approval::Required,
            permission_mode: None,
            allow_from: vec![],
            workspace_only: false,
        })
    }

    #[async_trait]
    impl Tool for PromptTool {
        fn name(&self) -> &str {
            "prompt_tool"
        }

        fn description(&self) -> &str {
            "A tool that requires approval."
        }

        fn schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "msg": { "type": "string" }
                },
                "required": ["msg"]
            })
        }

        fn policy(&self) -> &Policy {
            required_policy()
        }

        async fn execute(
            &self,
            params: serde_json::Value,
            _ctx: ToolContext,
        ) -> Result<ToolResult, ToolError> {
            let msg = params["msg"].as_str().unwrap_or("");
            Ok(ToolResult::ok(format!("prompt_tool: {msg}")))
        }
    }

    struct FakeToolRegistry {
        tools: HashMap<String, Arc<dyn Tool>>,
    }

    impl FakeToolRegistry {
        fn new(tools: Vec<Arc<dyn Tool>>) -> Self {
            let mut map = HashMap::new();
            for t in tools {
                map.insert(t.name().to_string(), t);
            }
            Self { tools: map }
        }
    }

    #[async_trait]
    impl ToolRegistry for FakeToolRegistry {
        fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
            self.tools.get(name).cloned()
        }

        fn definitions(&self) -> Vec<ToolDefinition> {
            self.tools.values().map(|t| t.definition()).collect()
        }
    }

    struct FakeMemoryBackend;

    #[async_trait]
    impl MemoryBackend for FakeMemoryBackend {
        async fn search(
            &self,
            _query: &str,
            _top_k: usize,
        ) -> Result<Vec<MemoryNote>, MemoryError> {
            Ok(Vec::new())
        }

        async fn get(
            &self,
            _path: &str,
            _range: Option<std::ops::Range<usize>>,
        ) -> Result<String, MemoryError> {
            Ok(String::new())
        }

        async fn index(
            &self,
            _id: &str,
            _content: &str,
            _meta: crate::memory::MemoryMeta,
        ) -> Result<(), MemoryError> {
            Ok(())
        }
    }

    /// Provider that always returns the same text response.
    struct TextProvider {
        text: String,
    }

    #[async_trait]
    impl Provider for TextProvider {
        fn id(&self) -> &str {
            "text"
        }

        fn supported_models(&self) -> Vec<ModelInfo> {
            Vec::new()
        }

        async fn chat(&self, _req: ChatRequest) -> Result<ChatStream, ProviderError> {
            let chunk = ChatChunk {
                index: 0,
                delta: self.text.clone(),
                finish_reason: Some(FinishReason::Stop),
                tool_calls: None,
            };
            Ok(Box::pin(futures::stream::iter(vec![Ok(chunk)])))
        }

        async fn embed(&self, _req: EmbedRequest) -> Result<Vec<Embedding>, ProviderError> {
            Ok(Vec::new())
        }
    }

    /// Provider that returns a tool call on the first request and text afterwards.
    struct ToolThenTextProvider;

    #[async_trait]
    impl Provider for ToolThenTextProvider {
        fn id(&self) -> &str {
            "tool-then-text"
        }

        fn supported_models(&self) -> Vec<ModelInfo> {
            Vec::new()
        }

        async fn chat(&self, req: ChatRequest) -> Result<ChatStream, ProviderError> {
            let has_tool_message = req.messages.iter().any(|m| m.role == ChatRole::Tool);

            if has_tool_message {
                let chunk = ChatChunk {
                    index: 0,
                    delta: "done".into(),
                    finish_reason: Some(FinishReason::Stop),
                    tool_calls: None,
                };
                Ok(Box::pin(futures::stream::iter(vec![Ok(chunk)])))
            } else {
                let chunk = ChatChunk {
                    index: 0,
                    delta: String::new(),
                    finish_reason: Some(FinishReason::ToolCalls),
                    tool_calls: Some(vec![ProviderToolCall {
                        id: "call-1".into(),
                        kind: "function".into(),
                        function: FunctionCall {
                            name: "echo".into(),
                            arguments: r#"{"msg":"hello"}"#.into(),
                        },
                    }]),
                };
                Ok(Box::pin(futures::stream::iter(vec![Ok(chunk)])))
            }
        }

        async fn embed(&self, _req: EmbedRequest) -> Result<Vec<Embedding>, ProviderError> {
            Ok(Vec::new())
        }
    }

    /// Provider that calls the `prompt_tool` once and then returns text.
    struct PromptToolProvider;

    #[async_trait]
    impl Provider for PromptToolProvider {
        fn id(&self) -> &str {
            "prompt-tool"
        }

        fn supported_models(&self) -> Vec<ModelInfo> {
            Vec::new()
        }

        async fn chat(&self, req: ChatRequest) -> Result<ChatStream, ProviderError> {
            let has_tool_message = req.messages.iter().any(|m| m.role == ChatRole::Tool);

            if has_tool_message {
                let chunk = ChatChunk {
                    index: 0,
                    delta: "done".into(),
                    finish_reason: Some(FinishReason::Stop),
                    tool_calls: None,
                };
                Ok(Box::pin(futures::stream::iter(vec![Ok(chunk)])))
            } else {
                let chunk = ChatChunk {
                    index: 0,
                    delta: String::new(),
                    finish_reason: Some(FinishReason::ToolCalls),
                    tool_calls: Some(vec![ProviderToolCall {
                        id: "call-1".into(),
                        kind: "function".into(),
                        function: FunctionCall {
                            name: "prompt_tool".into(),
                            arguments: r#"{"msg":"hello"}"#.into(),
                        },
                    }]),
                };
                Ok(Box::pin(futures::stream::iter(vec![Ok(chunk)])))
            }
        }

        async fn embed(&self, _req: EmbedRequest) -> Result<Vec<Embedding>, ProviderError> {
            Ok(Vec::new())
        }
    }

    /// Provider that always returns the same tool call.
    struct LoopingToolProvider;

    #[async_trait]
    impl Provider for LoopingToolProvider {
        fn id(&self) -> &str {
            "looping-tool"
        }

        fn supported_models(&self) -> Vec<ModelInfo> {
            Vec::new()
        }

        async fn chat(&self, _req: ChatRequest) -> Result<ChatStream, ProviderError> {
            let chunk = ChatChunk {
                index: 0,
                delta: String::new(),
                finish_reason: Some(FinishReason::ToolCalls),
                tool_calls: Some(vec![ProviderToolCall {
                    id: "call-loop".into(),
                    kind: "function".into(),
                    function: FunctionCall {
                        name: "echo".into(),
                        arguments: r#"{"msg":"loop"}"#.into(),
                    },
                }]),
            };
            Ok(Box::pin(futures::stream::iter(vec![Ok(chunk)])))
        }

        async fn embed(&self, _req: EmbedRequest) -> Result<Vec<Embedding>, ProviderError> {
            Ok(Vec::new())
        }
    }

    fn runtime_with_provider(provider: Arc<dyn Provider>) -> AgentRuntime {
        runtime_with_config(provider, r#"{ "gateway": { "auth": { "token": "x" } } }"#)
    }

    fn runtime_with_config(provider: Arc<dyn Provider>, config_json: &str) -> AgentRuntime {
        runtime_with_tools(provider, config_json, vec![Arc::new(EchoTool)])
    }

    fn runtime_with_tools(
        provider: Arc<dyn Provider>,
        config_json: &str,
        tools: Vec<Arc<dyn Tool>>,
    ) -> AgentRuntime {
        let mut router = ProviderRouter::new();
        router.register_provider(provider);

        let config = Config::from_json(config_json).unwrap();

        AgentRuntime::new(
            Arc::new(router),
            Arc::new(FakeToolRegistry::new(tools)),
            Arc::new(FakeMemoryBackend),
            config,
        )
    }

    async fn collect_events(mut stream: RunStream) -> Vec<RunEvent> {
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event);
        }
        events
    }

    #[tokio::test]
    async fn plain_text_run_emits_start_delta_end() {
        let runtime = runtime_with_provider(Arc::new(TextProvider {
            text: "hello".into(),
        }));
        let request = RunRequest::new("session-1", "main", "hi", "text/gpt");
        let events = collect_events(runtime.run(request).unwrap()).await;

        assert_eq!(
            events[0],
            RunEvent::Lifecycle {
                phase: LifecyclePhase::Start,
                error: None
            }
        );
        assert_eq!(
            events[1],
            RunEvent::AssistantDelta {
                delta: "hello".into()
            }
        );
        assert_eq!(
            events[2],
            RunEvent::Lifecycle {
                phase: LifecyclePhase::End,
                error: None
            }
        );
    }

    /// Provider that returns a final answer for the main turn and a JSON fact
    /// array when it sees the auto-extractor system prompt.
    struct FactProvider;

    #[async_trait]
    impl Provider for FactProvider {
        fn id(&self) -> &str {
            "fact"
        }
        fn supported_models(&self) -> Vec<ModelInfo> {
            Vec::new()
        }
        async fn chat(&self, req: ChatRequest) -> Result<ChatStream, ProviderError> {
            let is_extract = req
                .messages
                .iter()
                .any(|m| m.role == ChatRole::System && m.content.contains("extract durable"));
            let delta = if is_extract {
                r#"["User prefers Rust"]"#.to_string()
            } else {
                "done".to_string()
            };
            let chunk = ChatChunk {
                index: 0,
                delta,
                finish_reason: Some(FinishReason::Stop),
                tool_calls: None,
            };
            Ok(Box::pin(futures::stream::iter(vec![Ok(chunk)])))
        }
        async fn embed(&self, _req: EmbedRequest) -> Result<Vec<Embedding>, ProviderError> {
            Ok(Vec::new())
        }
    }

    #[derive(Default)]
    struct RecordingMemory {
        indexed: std::sync::Mutex<Vec<(String, String, crate::memory::MemoryMeta)>>,
    }

    impl RecordingMemory {
        fn snapshot(&self) -> Vec<(String, String, crate::memory::MemoryMeta)> {
            self.indexed.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl MemoryBackend for RecordingMemory {
        async fn search(&self, _q: &str, _k: usize) -> Result<Vec<MemoryNote>, MemoryError> {
            Ok(Vec::new())
        }
        async fn get(
            &self,
            _p: &str,
            _r: Option<std::ops::Range<usize>>,
        ) -> Result<String, MemoryError> {
            Ok(String::new())
        }
        async fn index(
            &self,
            id: &str,
            content: &str,
            meta: crate::memory::MemoryMeta,
        ) -> Result<(), MemoryError> {
            self.indexed
                .lock()
                .unwrap()
                .push((id.to_string(), content.to_string(), meta));
            Ok(())
        }
    }

    #[tokio::test]
    async fn auto_extract_persists_fact_after_turn() {
        let router = {
            let mut r = ProviderRouter::new();
            r.register_provider(Arc::new(FactProvider));
            Arc::new(r)
        };
        let memory = Arc::new(RecordingMemory::default());
        let extractor = Arc::new(crate::auto_extract::AutoExtractor::new(
            router.clone(),
            "fact/gpt",
            memory.clone() as Arc<dyn MemoryBackend>,
            20,
            5,
            0,
            std::time::Duration::from_secs(5),
        ));
        let config = Config::from_json(r#"{ "gateway": { "auth": { "token": "x" } } }"#).unwrap();
        let runtime = AgentRuntime::new(
            router,
            Arc::new(FakeToolRegistry::new(vec![Arc::new(EchoTool)])),
            memory.clone() as Arc<dyn MemoryBackend>,
            config,
        )
        .with_auto_extractor(Some(extractor));

        let request = RunRequest::new("session-1", "main", "what do I like?", "fact/gpt");
        let _ = collect_events(runtime.run(request).unwrap()).await;
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;

        let indexed = memory.snapshot();
        assert!(
            indexed
                .iter()
                .any(|(_, c, m)| c == "User prefers Rust" && m.kind.as_deref() == Some("episodic")),
            "expected extracted episodic fact, got {indexed:?}"
        );
    }

    /// Commitment extractor that records every invocation instead of doing
    /// inference, so tests can assert the end-of-turn wiring deterministically.
    type RecordedCommitmentCalls = Arc<std::sync::Mutex<Vec<(String, String, Vec<ChatMessage>)>>>;

    struct RecordingCommitmentExtractor {
        calls: RecordedCommitmentCalls,
    }

    impl CommitmentExtractor for RecordingCommitmentExtractor {
        fn spawn_extract(&self, agent_id: String, session_id: String, messages: Vec<ChatMessage>) {
            self.calls
                .lock()
                .unwrap()
                .push((agent_id, session_id, messages));
        }
    }

    #[tokio::test]
    async fn commitment_extractor_fires_once_per_completed_turn() {
        let calls: RecordedCommitmentCalls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let extractor = Arc::new(RecordingCommitmentExtractor {
            calls: calls.clone(),
        });
        let runtime = runtime_with_provider(Arc::new(TextProvider {
            text: "done".into(),
        }))
        .with_commitment_extractor(Some(extractor));

        let request = RunRequest::new("session-1", "main", "remind me to call mom", "text/gpt");
        let events = collect_events(runtime.run(request).unwrap()).await;

        assert!(matches!(
            events.last(),
            Some(RunEvent::Lifecycle {
                phase: LifecyclePhase::End,
                error: None
            })
        ));

        // `spawn_extract` runs synchronously before the End event, so the
        // recording is complete once the stream is drained (no sleep needed).
        let recorded = calls.lock().unwrap();
        assert_eq!(
            recorded.len(),
            1,
            "commitment extractor should fire exactly once per turn: {recorded:?}"
        );
        let (agent_id, session_id, messages) = &recorded[0];
        assert_eq!(agent_id, "main");
        assert_eq!(session_id, "session-1");
        assert!(
            messages
                .iter()
                .any(|m| m.content == "remind me to call mom"),
            "extractor should receive the turn's user message: {messages:?}"
        );
        assert!(
            messages.iter().any(|m| m.content == "done"),
            "extractor should receive the turn's assistant reply: {messages:?}"
        );
    }

    /// Memory backend that always returns the same static notes (for recall tests).
    struct StaticMemory {
        notes: Vec<MemoryNote>,
    }

    #[async_trait]
    impl MemoryBackend for StaticMemory {
        async fn search(&self, _q: &str, _k: usize) -> Result<Vec<MemoryNote>, MemoryError> {
            Ok(self.notes.clone())
        }
        async fn get(
            &self,
            _p: &str,
            _r: Option<std::ops::Range<usize>>,
        ) -> Result<String, MemoryError> {
            Ok(String::new())
        }
        async fn index(
            &self,
            _id: &str,
            _c: &str,
            _m: crate::memory::MemoryMeta,
        ) -> Result<(), MemoryError> {
            Ok(())
        }
    }

    /// Provider that captures every system prompt it sees and replies "done".
    struct CaptureProvider {
        captured: Arc<std::sync::Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl Provider for CaptureProvider {
        fn id(&self) -> &str {
            "cap"
        }
        fn supported_models(&self) -> Vec<ModelInfo> {
            Vec::new()
        }
        async fn chat(&self, req: ChatRequest) -> Result<ChatStream, ProviderError> {
            // Join all system messages: the runtime splits the assembled
            // prompt at the cache prefix, so recalled memories (uncached)
            // may live in a second system message.
            let sys = req
                .messages
                .iter()
                .filter(|m| m.role == ChatRole::System)
                .map(|m| m.content.clone())
                .collect::<Vec<_>>()
                .join("\n");
            self.captured.lock().unwrap().push(sys);
            let chunk = ChatChunk {
                index: 0,
                delta: "done".to_string(),
                finish_reason: Some(FinishReason::Stop),
                tool_calls: None,
            };
            Ok(Box::pin(futures::stream::iter(vec![Ok(chunk)])))
        }
        async fn embed(&self, _req: EmbedRequest) -> Result<Vec<Embedding>, ProviderError> {
            Ok(Vec::new())
        }
    }

    /// Provider that reorders recall candidates via the selector prompt and
    /// captures the main-turn system prompt for assertions.
    struct SelectorProvider {
        captured: Arc<std::sync::Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl Provider for SelectorProvider {
        fn id(&self) -> &str {
            "sel"
        }
        fn supported_models(&self) -> Vec<ModelInfo> {
            Vec::new()
        }
        async fn chat(&self, req: ChatRequest) -> Result<ChatStream, ProviderError> {
            // Join all system messages: the runtime splits the assembled
            // prompt at the cache prefix, so recalled memories (uncached)
            // may live in a second system message.
            let sys = req
                .messages
                .iter()
                .filter(|m| m.role == ChatRole::System)
                .map(|m| m.content.clone())
                .collect::<Vec<_>>()
                .join("\n");
            let delta = if sys.contains("select the memories most relevant") {
                "[1, 0]".to_string()
            } else {
                self.captured.lock().unwrap().push(sys);
                "done".to_string()
            };
            let chunk = ChatChunk {
                index: 0,
                delta,
                finish_reason: Some(FinishReason::Stop),
                tool_calls: None,
            };
            Ok(Box::pin(futures::stream::iter(vec![Ok(chunk)])))
        }
        async fn embed(&self, _req: EmbedRequest) -> Result<Vec<Embedding>, ProviderError> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn selector_reorders_recalled_memories_in_prompt() {
        let captured: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(SelectorProvider {
            captured: captured.clone(),
        });
        let mut router = ProviderRouter::new();
        router.register_provider(provider);
        let router = Arc::new(router);

        let memory = Arc::new(StaticMemory {
            notes: vec![
                MemoryNote {
                    id: "m0".into(),
                    content: "alpha fact".into(),
                    score: 0.9,
                    kind: None,
                },
                MemoryNote {
                    id: "m1".into(),
                    content: "beta fact".into(),
                    score: 0.8,
                    kind: None,
                },
            ],
        });
        let selector = Arc::new(LlmRecallSelector::new(
            router.clone(),
            "sel/gpt",
            std::time::Duration::from_secs(5),
        ));
        let tmp = tempfile::tempdir().unwrap();
        // Point the workspace at the temp dir: the default
        // `~/.legion/workspace` would load the developer machine's real
        // bootstrap files (e.g. a seeded AGENTS.md) and shift the cache
        // prefix / prompt contents.
        let config = Config::from_json(&format!(
            r#"{{ "gateway": {{ "auth": {{ "token": "x" }} }}, "agents": {{ "defaults": {{ "workspace": "{}" }} }} }}"#,
            tmp.path().display()
        ))
        .unwrap();
        let runtime = AgentRuntime::new(
            router,
            Arc::new(FakeToolRegistry::new(vec![Arc::new(EchoTool)])),
            memory,
            config,
        )
        .with_selector(Some(selector))
        .with_surfaced(SurfacedStore::new(tmp.path()));

        let request = RunRequest::new("session-sel", "main", "tell me", "sel/gpt");
        let _ = collect_events(runtime.run(request).unwrap()).await;

        let prompts = captured.lock().unwrap().clone();
        let main = prompts
            .iter()
            .find(|p| p.contains("Relevant memories"))
            .expect("main-turn prompt with recalled memories was captured");
        let beta = main.find("beta fact").unwrap();
        let alpha = main.find("alpha fact").unwrap();
        assert!(
            beta < alpha,
            "expected selector to reorder beta before alpha; prompt={main:?}"
        );
    }

    #[tokio::test]
    async fn surfaced_suppresses_repeat_injection_across_turns() {
        let captured: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(CaptureProvider {
            captured: captured.clone(),
        });
        let mut router = ProviderRouter::new();
        router.register_provider(provider);
        let router = Arc::new(router);

        let memory = Arc::new(StaticMemory {
            notes: vec![MemoryNote {
                id: "fact-1".into(),
                content: "User likes tea".into(),
                score: 0.95,
                kind: None,
            }],
        });
        let tmp = tempfile::tempdir().unwrap();
        // Isolate from the developer machine's real ~/.legion/workspace;
        // see selector_reorders_recalled_memories_in_prompt for details.
        let config = Config::from_json(&format!(
            r#"{{ "gateway": {{ "auth": {{ "token": "x" }} }}, "agents": {{ "defaults": {{ "workspace": "{}" }} }} }}"#,
            tmp.path().display()
        ))
        .unwrap();
        let runtime = AgentRuntime::new(
            router,
            Arc::new(FakeToolRegistry::new(vec![Arc::new(EchoTool)])),
            memory,
            config,
        )
        .with_surfaced(SurfacedStore::new(tmp.path()));

        let req1 = RunRequest::new("session-dup", "main", "first", "cap/gpt");
        let _ = collect_events(runtime.run(req1).unwrap()).await;
        let req2 = RunRequest::new("session-dup", "main", "second", "cap/gpt");
        let _ = collect_events(runtime.run(req2).unwrap()).await;

        let prompts = captured.lock().unwrap().clone();
        assert_eq!(prompts.len(), 2, "two turns captured: {prompts:?}");
        assert!(
            prompts[0].contains("User likes tea"),
            "first turn should inject the fact: {:?}",
            prompts[0]
        );
        assert!(
            !prompts[1].contains("User likes tea"),
            "second turn must suppress the already-surfaced fact: {:?}",
            prompts[1]
        );
    }

    #[tokio::test]
    async fn large_history_triggers_compaction_event() {
        let runtime = runtime_with_config(
            Arc::new(TextProvider {
                text: "summary".into(),
            }),
            r#"{
                "gateway": { "auth": { "token": "x" } },
                "compaction": {
                    "contextWindow": 100,
                    "thresholdRatio": 0.5,
                    "minMessagesToKeep": 1,
                    "maxSummaryTokens": 32
                }
            }"#,
        );

        let big_history = ChatMessage::user("word ".repeat(200));
        let request = RunRequest::new("session-1", "main", "continue", "text/gpt")
            .with_system_prompt("you are helpful")
            .with_history(vec![big_history]);
        let events = collect_events(runtime.run(request).unwrap()).await;

        assert_eq!(
            events[0],
            RunEvent::Lifecycle {
                phase: LifecyclePhase::Start,
                error: None
            }
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, RunEvent::Compaction { .. })),
            "expected a Compaction event, got {:?}",
            events
        );
        // The event carries the compacted head for transcript persistence:
        // a summary message first, no leading system prompt, kept tail last.
        let resume_head = events
            .iter()
            .find_map(|e| match e {
                RunEvent::Compaction { resume_head, .. } => Some(resume_head),
                _ => None,
            })
            .expect("compaction event carries resume_head");
        assert!(
            resume_head
                .first()
                .is_some_and(|m| m.content.contains("Earlier conversation summary")),
            "resume_head starts with the summary: {resume_head:?}"
        );
        assert!(
            resume_head.iter().all(|m| m.content != "you are helpful"),
            "resume_head excludes the rebuilt system prompt: {resume_head:?}"
        );
        assert_eq!(
            resume_head.last().map(|m| m.content.as_str()),
            Some("continue"),
            "resume_head keeps the current user message at the tail"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, RunEvent::AssistantDelta { .. }))
        );
        assert!(events.iter().any(|e| matches!(
            e,
            RunEvent::Lifecycle {
                phase: LifecyclePhase::End,
                error: None
            }
        )));
    }

    #[tokio::test]
    async fn tool_call_is_executed_and_result_fed_back() {
        let runtime = runtime_with_provider(Arc::new(ToolThenTextProvider));
        let request = RunRequest::new("session-1", "main", "call echo", "tool-then-text/gpt");
        let events = collect_events(runtime.run(request).unwrap()).await;

        assert_eq!(
            events[0],
            RunEvent::Lifecycle {
                phase: LifecyclePhase::Start,
                error: None
            }
        );
        assert!(
            matches!(&events[1], RunEvent::ToolStart { tool_call } if tool_call.name == "echo")
        );
        assert!(
            matches!(&events[2], RunEvent::ToolEnd { tool_call, result } if tool_call.name == "echo" && result.content == "echo: hello")
        );
        assert_eq!(
            events[3],
            RunEvent::AssistantDelta {
                delta: "done".into()
            }
        );
        assert_eq!(
            events[4],
            RunEvent::Lifecycle {
                phase: LifecyclePhase::End,
                error: None
            }
        );
    }

    #[tokio::test]
    async fn max_iterations_is_enforced() {
        let runtime =
            runtime_with_provider(Arc::new(LoopingToolProvider)).with_max_iterations(Some(2));
        let request = RunRequest::new("session-1", "main", "loop", "looping-tool/gpt");
        let events = collect_events(runtime.run(request).unwrap()).await;

        assert_eq!(
            events[0],
            RunEvent::Lifecycle {
                phase: LifecyclePhase::Start,
                error: None
            }
        );
        // First tool call, then second tool call, then error.
        assert!(
            events
                .iter()
                .any(|e| matches!(e, RunEvent::ToolStart { .. }))
        );
        assert!(matches!(
            events.last(),
            Some(RunEvent::Lifecycle {
                phase: LifecyclePhase::Error,
                error: Some(_)
            })
        ));
    }

    #[tokio::test]
    async fn request_max_iterations_overrides_runtime_default() {
        // The runtime default is 5, but the request overrides it to 1, so the
        // looping tool call must run exactly once before the loop bails out.
        let runtime =
            runtime_with_provider(Arc::new(LoopingToolProvider)).with_max_iterations(Some(5));
        let request =
            RunRequest::new("session-1", "main", "loop", "looping-tool/gpt").with_max_iterations(1);
        let events = collect_events(runtime.run(request).unwrap()).await;

        let tool_starts = events
            .iter()
            .filter(|e| matches!(e, RunEvent::ToolStart { .. }))
            .count();
        assert_eq!(
            tool_starts, 1,
            "loop should stop after the overridden iteration count, got {events:?}"
        );
        assert!(
            matches!(
                events.last(),
                Some(RunEvent::Lifecycle {
                    phase: LifecyclePhase::Error,
                    error: Some(msg)
                }) if msg.contains("max iterations (1)")
            ),
            "expected max-iterations error carrying the overridden count, got {events:?}"
        );
    }

    /// Provider that returns tool calls for the first `n` turns, then a final
    /// text answer. Used to verify that `maxIterations: null` does not enforce
    /// an artificial cap.
    struct LoopThenDoneProvider {
        calls: AtomicUsize,
        done_after: usize,
    }

    #[async_trait]
    impl Provider for LoopThenDoneProvider {
        fn id(&self) -> &str {
            "loop-then-done"
        }

        fn supported_models(&self) -> Vec<ModelInfo> {
            Vec::new()
        }

        async fn chat(&self, _req: ChatRequest) -> Result<ChatStream, ProviderError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call >= self.done_after {
                let chunk = ChatChunk {
                    index: 0,
                    delta: "done".into(),
                    finish_reason: Some(FinishReason::Stop),
                    tool_calls: None,
                };
                Ok(Box::pin(futures::stream::iter(vec![Ok(chunk)])))
            } else {
                let chunk = ChatChunk {
                    index: 0,
                    delta: String::new(),
                    finish_reason: Some(FinishReason::ToolCalls),
                    tool_calls: Some(vec![ProviderToolCall {
                        id: format!("call-{call}"),
                        kind: "function".into(),
                        function: FunctionCall {
                            name: "echo".into(),
                            arguments: r#"{"msg":"loop"}"#.into(),
                        },
                    }]),
                };
                Ok(Box::pin(futures::stream::iter(vec![Ok(chunk)])))
            }
        }

        async fn embed(&self, _req: EmbedRequest) -> Result<Vec<Embedding>, ProviderError> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn null_max_iterations_allows_run_to_finish() {
        // A runtime default of `None` must not enforce a cap; the provider is
        // allowed to loop until it produces a final answer.
        let runtime = runtime_with_config(
            Arc::new(LoopThenDoneProvider {
                calls: AtomicUsize::new(0),
                done_after: 3,
            }),
            r#"{ "gateway": { "auth": { "token": "x" } }, "agents": { "defaults": { "maxIterations": null } } }"#,
        );
        let request = RunRequest::new("session-1", "main", "loop", "loop-then-done/gpt");
        let events = collect_events(runtime.run(request).unwrap()).await;

        let tool_starts = events
            .iter()
            .filter(|e| matches!(e, RunEvent::ToolStart { .. }))
            .count();
        assert_eq!(tool_starts, 3, "expected three tool calls, got {events:?}");
        assert!(
            matches!(
                events.last(),
                Some(RunEvent::Lifecycle {
                    phase: LifecyclePhase::End,
                    error: None
                })
            ),
            "expected clean completion with no iteration cap, got {events:?}"
        );
    }

    #[tokio::test]
    async fn per_agent_max_iterations_overrides_defaults() {
        // `agents.list[].maxIterations` should take precedence over defaults.
        let runtime = runtime_with_config(
            Arc::new(LoopingToolProvider),
            r#"{
                "gateway": { "auth": { "token": "x" } },
                "agents": {
                    "defaults": { "maxIterations": 100 },
                    "list": [ { "id": "limited", "maxIterations": 2 } ]
                }
            }"#,
        );
        let request = RunRequest::new("session-1", "limited", "loop", "looping-tool/gpt");
        let events = collect_events(runtime.run(request).unwrap()).await;

        let tool_starts = events
            .iter()
            .filter(|e| matches!(e, RunEvent::ToolStart { .. }))
            .count();
        assert_eq!(
            tool_starts, 2,
            "per-agent cap of 2 should allow exactly two tool calls, got {events:?}"
        );
        assert!(
            matches!(
                events.last(),
                Some(RunEvent::Lifecycle {
                    phase: LifecyclePhase::Error,
                    error: Some(msg)
                }) if msg.contains("max iterations (2)")
            ),
            "expected per-agent max-iterations error, got {events:?}"
        );
    }

    #[tokio::test]
    async fn run_falls_back_to_main_router_for_unknown_agent() {
        // Only the "main" router is registered; a run for an agent without its
        // own router must fall back to it.
        let runtime = runtime_with_provider(Arc::new(TextProvider { text: "hi".into() }));
        let request = RunRequest::new("session-1", "researcher", "hello", "text/gpt");
        let events = collect_events(runtime.run(request).unwrap()).await;

        assert!(
            events.iter().any(|e| matches!(
                e,
                RunEvent::AssistantDelta { delta } if delta == "hi"
            )),
            "unknown agent should run through the main router, got {events:?}"
        );
        assert!(matches!(
            events.last(),
            Some(RunEvent::Lifecycle {
                phase: LifecyclePhase::End,
                error: None
            })
        ));
    }

    #[tokio::test]
    async fn run_without_any_router_returns_provider_not_found() {
        let mut runtime = runtime_with_provider(Arc::new(TextProvider { text: "hi".into() }));
        runtime.provider_routers.clear();
        let request = RunRequest::new("session-1", "ghost", "hello", "text/gpt");
        let err = match runtime.run(request) {
            Err(err) => err,
            Ok(_) => panic!("expected run to fail when no router is registered"),
        };
        assert!(
            matches!(
                err,
                RuntimeError::Provider(ProviderError::ProviderNotFound(ref id)) if id == "ghost"
            ),
            "expected ProviderNotFound carrying the agent id, got {err:?}"
        );
    }

    #[tokio::test]
    async fn unknown_tool_returns_error_result() {
        struct UnknownToolProvider;

        #[async_trait]
        impl Provider for UnknownToolProvider {
            fn id(&self) -> &str {
                "unknown-tool"
            }

            fn supported_models(&self) -> Vec<ModelInfo> {
                Vec::new()
            }

            async fn chat(&self, _req: ChatRequest) -> Result<ChatStream, ProviderError> {
                let chunk = ChatChunk {
                    index: 0,
                    delta: String::new(),
                    finish_reason: Some(FinishReason::ToolCalls),
                    tool_calls: Some(vec![ProviderToolCall {
                        id: "call-unknown".into(),
                        kind: "function".into(),
                        function: FunctionCall {
                            name: "missing".into(),
                            arguments: "{}".into(),
                        },
                    }]),
                };
                Ok(Box::pin(futures::stream::iter(vec![Ok(chunk)])))
            }

            async fn embed(&self, _req: EmbedRequest) -> Result<Vec<Embedding>, ProviderError> {
                Ok(Vec::new())
            }
        }

        // Provider that returns no further response after the unknown tool call.
        // The runtime will loop again and hit max iterations; we just verify the tool result.
        let runtime =
            runtime_with_provider(Arc::new(UnknownToolProvider)).with_max_iterations(Some(1));
        let request = RunRequest::new("session-1", "main", "unknown", "unknown-tool/gpt");
        let events = collect_events(runtime.run(request).unwrap()).await;

        let tool_end = events.iter().find_map(|e| match e {
            RunEvent::ToolEnd { tool_call, result } => Some((tool_call.clone(), result.clone())),
            _ => None,
        });
        assert!(tool_end.is_some());
        let (_, result) = tool_end.unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("not found"));
    }

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

    struct PtlThenTextProvider {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl Provider for PtlThenTextProvider {
        fn id(&self) -> &str {
            "ptl-then-text"
        }

        fn supported_models(&self) -> Vec<ModelInfo> {
            Vec::new()
        }

        async fn chat(&self, _req: ChatRequest) -> Result<ChatStream, ProviderError> {
            let previous = self.calls.fetch_add(1, Ordering::SeqCst);
            if previous < 2 {
                return Err(ProviderError::PromptTooLong);
            }
            Ok(Box::pin(futures::stream::iter(vec![Ok(ChatChunk {
                index: 0,
                delta: "hello".to_string(),
                finish_reason: Some(FinishReason::Stop),
                tool_calls: None,
            })])))
        }

        async fn embed(&self, _req: EmbedRequest) -> Result<Vec<Embedding>, ProviderError> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn ptl_retry_truncates_and_succeeds() {
        let runtime = runtime_with_provider(Arc::new(PtlThenTextProvider {
            calls: AtomicUsize::new(0),
        }));
        let request = RunRequest::new("session-1", "main", "hi", "ptl-then-text/gpt");
        let events = collect_events(runtime.run(request).unwrap()).await;

        assert!(
            events.iter().any(|e| matches!(
                e,
                RunEvent::AssistantDelta {
                    delta,
                } if delta == "hello"
            )),
            "expected final assistant delta after PTL retries, got {:?}",
            events
        );
    }

    /// Provider that always fails with PromptTooLong (PTL-retry exhaustion).
    struct AlwaysPtlProvider {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl Provider for AlwaysPtlProvider {
        fn id(&self) -> &str {
            "always-ptl"
        }

        fn supported_models(&self) -> Vec<ModelInfo> {
            Vec::new()
        }

        async fn chat(&self, _req: ChatRequest) -> Result<ChatStream, ProviderError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(ProviderError::PromptTooLong)
        }

        async fn embed(&self, _req: EmbedRequest) -> Result<Vec<Embedding>, ProviderError> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn ptl_retry_exhaustion_returns_error() {
        let provider = Arc::new(AlwaysPtlProvider {
            calls: AtomicUsize::new(0),
        });
        let runtime = runtime_with_provider(provider.clone());
        let request = RunRequest::new("session-1", "main", "hi", "always-ptl/gpt");
        let events = collect_events(runtime.run(request).unwrap()).await;

        assert!(
            matches!(
                events.last(),
                Some(RunEvent::Lifecycle {
                    phase: LifecyclePhase::Error,
                    error: Some(msg)
                }) if msg.contains("prompt too long")
            ),
            "expected a prompt-too-long lifecycle error, got {events:?}"
        );
        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            MAX_PTL_RETRIES,
            "provider should be called once per retry attempt before giving up"
        );
    }

    /// Provider whose stream is cut short by a content filter.
    struct ContentFilterProvider;

    #[async_trait]
    impl Provider for ContentFilterProvider {
        fn id(&self) -> &str {
            "content-filter"
        }

        fn supported_models(&self) -> Vec<ModelInfo> {
            Vec::new()
        }

        async fn chat(&self, _req: ChatRequest) -> Result<ChatStream, ProviderError> {
            Ok(Box::pin(futures::stream::iter(vec![
                Ok(ChatChunk {
                    index: 0,
                    delta: "partial".to_string(),
                    finish_reason: None,
                    tool_calls: None,
                }),
                Ok(ChatChunk {
                    index: 1,
                    delta: String::new(),
                    finish_reason: Some(FinishReason::ContentFilter),
                    tool_calls: None,
                }),
            ])))
        }

        async fn embed(&self, _req: EmbedRequest) -> Result<Vec<Embedding>, ProviderError> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn content_filter_finish_returns_error() {
        let runtime = runtime_with_provider(Arc::new(ContentFilterProvider));
        let request = RunRequest::new("session-1", "main", "hi", "content-filter/gpt");
        let events = collect_events(runtime.run(request).unwrap()).await;

        assert!(
            events.iter().any(|e| matches!(
                e,
                RunEvent::AssistantDelta { delta } if delta == "partial"
            )),
            "deltas before the filter should still be streamed, got {events:?}"
        );
        assert!(
            matches!(
                events.last(),
                Some(RunEvent::Lifecycle {
                    phase: LifecyclePhase::Error,
                    error: Some(msg)
                }) if msg.contains("content filtered")
            ),
            "expected a content-filter lifecycle error, got {events:?}"
        );
    }

    struct CapturingApprovalNotifier {
        ids: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedSender<String>>,
    }

    #[async_trait]
    impl crate::approval::ApprovalNotifier for CapturingApprovalNotifier {
        async fn notify(&self, _req: &crate::approval::ApprovalRequest, prompt_id: &str) {
            let _ = self.ids.lock().await.send(prompt_id.to_string());
        }
    }

    #[tokio::test]
    async fn non_interactive_run_denies_prompt_tool() {
        let runtime = runtime_with_tools(
            Arc::new(PromptToolProvider),
            r#"{ "gateway": { "auth": { "token": "x" } } }"#,
            vec![Arc::new(PromptTool)],
        );
        let request = RunRequest::new("session-1", "main", "call prompt_tool", "prompt-tool/gpt")
            .with_interactive(false);
        let events = collect_events(runtime.run(request).unwrap()).await;

        assert!(
            events.iter().any(|e| matches!(
                e,
                RunEvent::ToolEnd { result, .. } if result.is_error && result.content.contains("approval denied")
            )),
            "expected tool error for unattended approval, got {:?}",
            events
        );
    }

    #[tokio::test]
    async fn interactive_run_prompts_and_executes_on_approve() {
        let runtime = runtime_with_tools(
            Arc::new(PromptToolProvider),
            r#"{ "gateway": { "auth": { "token": "x" } } }"#,
            vec![Arc::new(PromptTool)],
        );

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let notifier = Arc::new(CapturingApprovalNotifier {
            ids: tokio::sync::Mutex::new(tx),
        });
        let gate = Arc::new(crate::approval::ApprovalGate::new(
            notifier,
            std::time::Duration::from_secs(5),
        ));
        let gate_for_resolve = gate.clone();

        let request = RunRequest::new("session-1", "main", "call prompt_tool", "prompt-tool/gpt")
            .with_approval_gate(gate);

        let handle =
            tokio::spawn(async move { collect_events(runtime.run(request).unwrap()).await });

        let prompt_id = rx.recv().await.expect("notifier should fire");
        gate_for_resolve.resolve(&prompt_id, true).await;

        let events = handle.await.unwrap();

        assert!(
            events.iter().any(|e| matches!(
                e,
                RunEvent::ToolEnd { result, .. } if !result.is_error && result.content.contains("prompt_tool: hello")
            )),
            "expected tool to execute after approval, got {:?}",
            events
        );
    }

    // -----------------------------------------------------------------------
    // Skill Phase B integration tests
    // -----------------------------------------------------------------------

    struct CapturingTextProvider {
        messages: Arc<tokio::sync::Mutex<Vec<ChatMessage>>>,
    }

    #[async_trait]
    impl Provider for CapturingTextProvider {
        fn id(&self) -> &str {
            "capturing-text"
        }

        fn supported_models(&self) -> Vec<ModelInfo> {
            Vec::new()
        }

        async fn chat(&self, req: ChatRequest) -> Result<ChatStream, ProviderError> {
            self.messages.lock().await.extend(req.messages);
            Ok(Box::pin(futures::stream::iter(vec![Ok(ChatChunk {
                index: 0,
                delta: "hello".to_string(),
                finish_reason: Some(FinishReason::Stop),
                tool_calls: None,
            })])))
        }

        async fn embed(&self, _req: EmbedRequest) -> Result<Vec<Embedding>, ProviderError> {
            Ok(Vec::new())
        }
    }

    struct CapturingToolThenTextProvider {
        messages: Arc<tokio::sync::Mutex<Vec<ChatMessage>>>,
    }

    #[async_trait]
    impl Provider for CapturingToolThenTextProvider {
        fn id(&self) -> &str {
            "capturing-tool-then-text"
        }

        fn supported_models(&self) -> Vec<ModelInfo> {
            Vec::new()
        }

        async fn chat(&self, req: ChatRequest) -> Result<ChatStream, ProviderError> {
            self.messages.lock().await.extend(req.messages.clone());

            if req.messages.iter().any(|m| m.role == ChatRole::Tool) {
                Ok(Box::pin(futures::stream::iter(vec![Ok(ChatChunk {
                    index: 0,
                    delta: "done".into(),
                    finish_reason: Some(FinishReason::Stop),
                    tool_calls: None,
                })])))
            } else {
                Ok(Box::pin(futures::stream::iter(vec![Ok(ChatChunk {
                    index: 0,
                    delta: String::new(),
                    finish_reason: Some(FinishReason::ToolCalls),
                    tool_calls: Some(vec![ProviderToolCall {
                        id: "call-1".into(),
                        kind: "function".into(),
                        function: FunctionCall {
                            name: "read".into(),
                            arguments: r#"{"path":"main.tf"}"#.into(),
                        },
                    }]),
                })])))
            }
        }

        async fn embed(&self, _req: EmbedRequest) -> Result<Vec<Embedding>, ProviderError> {
            Ok(Vec::new())
        }
    }

    struct FakeReadTool;

    #[async_trait]
    impl Tool for FakeReadTool {
        fn name(&self) -> &str {
            "read"
        }

        fn description(&self) -> &str {
            "read"
        }

        fn schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            })
        }

        fn policy(&self) -> &Policy {
            open_policy()
        }

        fn is_read_only(&self, _input: &serde_json::Value) -> bool {
            true
        }

        fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
            true
        }

        async fn execute(
            &self,
            params: serde_json::Value,
            ctx: ToolContext,
        ) -> Result<ToolResult, ToolError> {
            let path = params["path"].as_str().unwrap_or("");
            let resolved = ctx.workspace.join(path);
            let content = tokio::fs::read_to_string(&resolved).await.map_err(|e| {
                ToolError::Execution(format!("failed to read '{}': {e}", resolved.display()))
            })?;
            if let Some(sink) = &ctx.viewed_files {
                if let Ok(mut guard) = sink.lock() {
                    guard.insert(resolved);
                }
            }
            Ok(ToolResult::ok(content))
        }
    }

    fn write_skill(dir: &std::path::Path, name: &str, content: &str) {
        let skill_dir = dir.join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), content).unwrap();
    }

    fn json_escape(path: &std::path::Path) -> String {
        path.to_string_lossy().replace('\\', "\\\\")
    }

    #[tokio::test]
    async fn relevant_skill_body_injected_into_system_prompt() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_skill(
            tmp.path(),
            "rust",
            "---\nname: rust\ndescription: Rust help\n---\nYou are a Rust expert.",
        );

        let config_json = format!(
            r#"{{
                "gateway": {{ "auth": {{ "token": "x" }} }},
                "agents": {{
                    "defaults": {{
                        "skills": {{
                            "enabled": true,
                            "dirs": ["{}"],
                            "maxBodyTokens": 500,
                            "maxTriggeredSkills": 3
                        }}
                    }}
                }}
            }}"#,
            json_escape(tmp.path())
        );

        let messages = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let runtime = runtime_with_config(
            Arc::new(CapturingTextProvider {
                messages: messages.clone(),
            }),
            &config_json,
        );

        let request = RunRequest::new("session-1", "main", "write Rust code", "capturing-text/gpt");
        let _events = collect_events(runtime.run(request).unwrap()).await;

        let msgs = messages.lock().await;
        let system_contents: Vec<&str> = msgs
            .iter()
            .filter(|m| m.role == ChatRole::System)
            .map(|m| m.content.as_str())
            .collect();
        assert!(
            system_contents
                .iter()
                .any(|c| c.contains("You are a Rust expert.")),
            "expected relevant rust skill body in system messages, got {:?}",
            system_contents
        );
    }

    #[tokio::test]
    async fn path_triggered_skill_body_injected_after_read() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_skill(
            tmp.path(),
            "terraform",
            "---\nname: terraform\ndescription: Terraform help\npaths:\n  - \"*.tf\"\n---\nYou are a Terraform expert.",
        );

        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();
        std::fs::write(
            workspace_dir.join("main.tf"),
            "resource \"null_resource\" \"test\" {}",
        )
        .unwrap();

        let config_json = format!(
            r#"{{
                "gateway": {{ "auth": {{ "token": "x" }} }},
                "agents": {{
                    "defaults": {{
                        "workspace": "{}",
                        "skills": {{
                            "enabled": true,
                            "dirs": ["{}"],
                            "maxBodyTokens": 500,
                            "maxTriggeredSkills": 3
                        }}
                    }}
                }}
            }}"#,
            json_escape(&workspace_dir),
            json_escape(tmp.path())
        );

        let messages = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let runtime = runtime_with_tools(
            Arc::new(CapturingToolThenTextProvider {
                messages: messages.clone(),
            }),
            &config_json,
            vec![Arc::new(FakeReadTool)],
        );

        let request = RunRequest::new(
            "session-1",
            "main",
            "check this file",
            "capturing-tool-then-text/gpt",
        );
        let _events = collect_events(runtime.run(request).unwrap()).await;

        let msgs = messages.lock().await;
        let has_tool_message = msgs.iter().any(|m| m.role == ChatRole::Tool);
        let system_contents: Vec<&str> = msgs
            .iter()
            .filter(|m| m.role == ChatRole::System)
            .map(|m| m.content.as_str())
            .collect();
        assert!(has_tool_message, "expected a tool result message");
        assert!(
            system_contents
                .iter()
                .any(|c| c.contains("You are a Terraform expert.")),
            "expected terraform skill body after reading main.tf, got {:?}",
            system_contents
        );
    }

    #[tokio::test]
    async fn skill_body_not_injected_twice_when_relevant_and_path_match_overlap() {
        let tmp = tempfile::TempDir::new().unwrap();
        // Description matches "rust" and paths match *.rs.
        write_skill(
            tmp.path(),
            "rust",
            "---\nname: rust\ndescription: Rust help\npaths:\n  - \"*.rs\"\n---\nYou are a Rust expert.",
        );

        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();
        std::fs::write(workspace_dir.join("main.rs"), "fn main() {}").unwrap();

        let config_json = format!(
            r#"{{
                "gateway": {{ "auth": {{ "token": "x" }} }},
                "agents": {{
                    "defaults": {{
                        "workspace": "{}",
                        "skills": {{
                            "enabled": true,
                            "dirs": ["{}"],
                            "maxBodyTokens": 500,
                            "maxTriggeredSkills": 3
                        }}
                    }}
                }}
            }}"#,
            json_escape(&workspace_dir),
            json_escape(tmp.path())
        );

        let messages = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let runtime = runtime_with_tools(
            Arc::new(CapturingToolThenTextProvider {
                messages: messages.clone(),
            }),
            &config_json,
            vec![Arc::new(FakeReadTool)],
        );

        // User message triggers relevant("rust"); reading main.rs also triggers
        // match_paths("*.rs"). The body should appear exactly once.
        let request = RunRequest::new(
            "session-1",
            "main",
            "write Rust code",
            "capturing-tool-then-text/gpt",
        );
        let _events = collect_events(runtime.run(request).unwrap()).await;

        let msgs = messages.lock().await;
        let unique_bodies: HashSet<String> = msgs
            .iter()
            .filter(|m| m.role == ChatRole::System && m.content.contains("You are a Rust expert."))
            .map(|m| m.content.clone())
            .collect();
        assert_eq!(
            unique_bodies.len(),
            1,
            "rust skill body should be injected exactly once across all turns"
        );
    }

    // -----------------------------------------------------------------------
    // Skill Phase C integration tests: plugin-provided skills
    // -----------------------------------------------------------------------

    fn plugin_skill(name: &str, description: &str, body: &str) -> Skill {
        Skill {
            frontmatter: legion_skills::SkillFrontmatter {
                name: name.to_string(),
                description: description.to_string(),
                user_invocable: true,
                ..default_skill_frontmatter()
            },
            body: body.to_string(),
            source: legion_skills::SkillSource::Plugin,
            path: std::path::PathBuf::from(format!("/plugin/{name}/SKILL.md")),
        }
    }

    fn default_skill_frontmatter() -> legion_skills::SkillFrontmatter {
        legion_skills::SkillFrontmatter {
            name: String::new(),
            description: String::new(),
            when_to_use: None,
            allowed_tools: Vec::new(),
            paths: Vec::new(),
            user_invocable: true,
            model: None,
            effort: None,
        }
    }

    #[tokio::test]
    async fn plugin_skill_injected_into_system_prompt() {
        let config_json = r#"{
            "gateway": { "auth": { "token": "x" } },
            "agents": {
                "defaults": {
                    "skills": {
                        "enabled": true,
                        "maxBodyTokens": 500,
                        "maxTriggeredSkills": 3
                    }
                }
            }
        }"#;

        let messages = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let runtime = runtime_with_config(
            Arc::new(CapturingTextProvider {
                messages: messages.clone(),
            }),
            config_json,
        )
        .with_plugin_skills(vec![plugin_skill(
            "plugin-rust",
            "Plugin Rust help",
            "You are a plugin-provided Rust expert.",
        )]);

        let request = RunRequest::new("session-1", "main", "write Rust code", "capturing-text/gpt");
        let _events = collect_events(runtime.run(request).unwrap()).await;

        let msgs = messages.lock().await;
        let system_contents: Vec<&str> = msgs
            .iter()
            .filter(|m| m.role == ChatRole::System)
            .map(|m| m.content.as_str())
            .collect();
        assert!(
            system_contents
                .iter()
                .any(|c| c.contains("You are a plugin-provided Rust expert.")),
            "expected plugin skill body in system messages, got {:?}",
            system_contents
        );
    }

    #[tokio::test]
    async fn plugin_skills_merge_with_workspace_skills() {
        let tmp = tempfile::TempDir::new().unwrap();
        write_skill(
            tmp.path(),
            "workspace-rust",
            "---\nname: workspace-rust\ndescription: Workspace Rust help\n---\nYou are a workspace Rust expert.",
        );

        let config_json = format!(
            r#"{{
                "gateway": {{ "auth": {{ "token": "x" }} }},
                "agents": {{
                    "defaults": {{
                        "skills": {{
                            "enabled": true,
                            "dirs": ["{}"],
                            "maxBodyTokens": 500,
                            "maxTriggeredSkills": 3
                        }}
                    }}
                }}
            }}"#,
            json_escape(tmp.path())
        );

        let messages = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let runtime = runtime_with_config(
            Arc::new(CapturingTextProvider {
                messages: messages.clone(),
            }),
            &config_json,
        )
        .with_plugin_skills(vec![plugin_skill(
            "plugin-rust",
            "Plugin Rust help",
            "You are a plugin-provided Rust expert.",
        )]);

        let request = RunRequest::new("session-1", "main", "write Rust code", "capturing-text/gpt");
        let _events = collect_events(runtime.run(request).unwrap()).await;

        let msgs = messages.lock().await;
        let system_contents: Vec<&str> = msgs
            .iter()
            .filter(|m| m.role == ChatRole::System)
            .map(|m| m.content.as_str())
            .collect();
        assert!(
            system_contents
                .iter()
                .any(|c| c.contains("You are a workspace Rust expert.")),
            "expected workspace skill body in system messages, got {:?}",
            system_contents
        );
        assert!(
            system_contents
                .iter()
                .any(|c| c.contains("You are a plugin-provided Rust expert.")),
            "expected plugin skill body in system messages, got {:?}",
            system_contents
        );
    }

    #[tokio::test]
    async fn workspace_agent_skills_injected_into_system_prompt() {
        let tmp = tempfile::TempDir::new().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        let agent_skills_dir = workspace_dir.join(".agents").join("skills");
        let local_rust_dir = agent_skills_dir.join("local-rust");
        std::fs::create_dir_all(&local_rust_dir).unwrap();
        std::fs::write(
            local_rust_dir.join("SKILL.md"),
            "---\nname: local-rust\ndescription: Local Rust help\n---\nYou are a workspace-local Rust expert.",
        )
        .unwrap();

        let config_json = format!(
            r#"{{
                "gateway": {{ "auth": {{ "token": "x" }} }},
                "agents": {{
                    "defaults": {{
                        "workspace": "{}",
                        "skills": {{
                            "enabled": true,
                            "dirs": [],
                            "maxBodyTokens": 500,
                            "maxTriggeredSkills": 3
                        }}
                    }}
                }}
            }}"#,
            json_escape(&workspace_dir)
        );

        let messages = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let runtime = runtime_with_config(
            Arc::new(CapturingTextProvider {
                messages: messages.clone(),
            }),
            &config_json,
        );

        let request = RunRequest::new("session-1", "main", "write Rust code", "capturing-text/gpt");
        let _events = collect_events(runtime.run(request).unwrap()).await;

        let msgs = messages.lock().await;
        let system_contents: Vec<&str> = msgs
            .iter()
            .filter(|m| m.role == ChatRole::System)
            .map(|m| m.content.as_str())
            .collect();
        assert!(
            system_contents
                .iter()
                .any(|c| c.contains("You are a workspace-local Rust expert.")),
            "expected workspace .agents/skills body in system messages, got {:?}",
            system_contents
        );
    }

    #[tokio::test]
    async fn allowed_tools_denies_calls_outside_subset() {
        // Provider first calls `echo`, then returns text. The run narrows the
        // allowed subset to `read`, so `echo` must be refused without executing.
        let runtime = runtime_with_provider(Arc::new(ToolThenTextProvider));
        let request = RunRequest::new("session-1", "main", "go", "tool-then-text/gpt")
            .with_allowed_tools(vec!["read".to_string()]);
        let events = collect_events(runtime.run(request).unwrap()).await;

        let tool_ends: Vec<&ToolResult> = events
            .iter()
            .filter_map(|e| match e {
                RunEvent::ToolEnd { result, .. } => Some(result),
                _ => None,
            })
            .collect();
        assert_eq!(tool_ends.len(), 1, "echo should be denied exactly once");
        assert!(
            tool_ends[0].content.contains("not permitted"),
            "denied call should carry the structured refusal, got {:?}",
            tool_ends[0].content
        );
        assert!(
            !tool_ends[0].content.contains("echo: hello"),
            "denied tool must not execute, got {:?}",
            tool_ends[0].content
        );

        let final_text: String = events
            .iter()
            .filter_map(|e| match e {
                RunEvent::AssistantDelta { delta } => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            final_text.contains("done"),
            "run should finish after the denial, got {final_text:?}"
        );
    }

    #[tokio::test]
    async fn allowed_tools_permits_calls_inside_subset() {
        // With `echo` in the allowed subset, the same provider call executes.
        let runtime = runtime_with_provider(Arc::new(ToolThenTextProvider));
        let request = RunRequest::new("session-1", "main", "go", "tool-then-text/gpt")
            .with_allowed_tools(vec!["echo".to_string()]);
        let events = collect_events(runtime.run(request).unwrap()).await;

        let contents: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                RunEvent::ToolEnd { result, .. } => Some(result.content.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            contents.iter().any(|c| c.contains("echo: hello")),
            "permitted tool should execute, got {contents:?}"
        );
        assert!(
            !contents.iter().any(|c| c.contains("not permitted")),
            "no denial expected when the tool is allowed, got {contents:?}"
        );
    }

    /// Tool that records the `parent_history` snapshot it receives (used to
    /// verify Fork inheritance wiring through the tool pipeline).
    struct CaptureHistoryTool {
        captured: Arc<std::sync::Mutex<Option<Vec<String>>>>,
    }

    #[async_trait]
    impl Tool for CaptureHistoryTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "Captures the parent history snapshot."
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": { "msg": { "type": "string" } },
                "required": ["msg"]
            })
        }
        fn policy(&self) -> &Policy {
            open_policy()
        }
        async fn execute(
            &self,
            _params: serde_json::Value,
            ctx: ToolContext,
        ) -> Result<ToolResult, ToolError> {
            let contents = ctx
                .parent_history
                .as_ref()
                .map(|h| h.iter().map(|m| m.content.clone()).collect::<Vec<_>>());
            *self.captured.lock().unwrap() = contents;
            Ok(ToolResult::ok("captured"))
        }
    }

    #[tokio::test]
    async fn tool_receives_parent_history_snapshot_for_fork() {
        let captured: Arc<std::sync::Mutex<Option<Vec<String>>>> =
            Arc::new(std::sync::Mutex::new(None));
        let runtime = runtime_with_tools(
            Arc::new(ToolThenTextProvider),
            r#"{ "gateway": { "auth": { "token": "x" } } }"#,
            vec![Arc::new(CaptureHistoryTool {
                captured: captured.clone(),
            })],
        );
        let request = RunRequest::new("session-1", "main", "go", "tool-then-text/gpt");
        let _events = collect_events(runtime.run(request).unwrap()).await;

        let guard = captured.lock().unwrap();
        let contents = guard
            .as_ref()
            .expect("tool should receive a history snapshot");
        assert!(
            contents.iter().any(|c| c == "go"),
            "snapshot should include the current user turn, got {contents:?}"
        );
        assert!(
            contents.len() >= 2,
            "snapshot should include prior system/user context, got {contents:?}"
        );
    }
}
