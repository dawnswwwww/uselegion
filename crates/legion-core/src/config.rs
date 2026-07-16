use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::HashMap;
use std::fmt;
use thiserror::Error;

#[derive(Debug, Error, PartialEq)]
pub enum ConfigError {
    #[error("invalid auth mode: {0}")]
    InvalidAuthMode(String),
    #[error("auth mode 'none' is not allowed with public bind host {0}")]
    UnsafeAuthModeNone(String),
    #[error("failed to resolve environment variable: {0}")]
    UnresolvedEnvVar(String),
    #[error("JSON parse error: {0}")]
    ParseError(String),
    #[error("JSON5 parse error: {0}")]
    Json5ParseError(String),
}

impl From<serde_json::Error> for ConfigError {
    fn from(e: serde_json::Error) -> Self {
        ConfigError::ParseError(e.to_string())
    }
}

impl From<serde_json5::Error> for ConfigError {
    fn from(e: serde_json5::Error) -> Self {
        ConfigError::Json5ParseError(e.to_string())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    pub gateway: GatewayConfig,
    #[serde(default)]
    pub agents: AgentsConfig,
    #[serde(default)]
    pub bindings: Vec<Binding>,
    #[serde(default)]
    pub channels: HashMap<String, serde_json::Value>,
    #[serde(default)]
    pub models: ModelsConfig,
    #[serde(default)]
    pub memory: MemoryConfig,
    #[serde(default)]
    pub tools: HashMap<String, ToolConfig>,
    #[serde(default)]
    pub session: SessionConfig,
    #[serde(default)]
    pub compaction: CompactionConfig,
    #[serde(default)]
    pub heartbeat: HeartbeatConfig,
    #[serde(default)]
    pub plugins: PluginsConfig,
    #[serde(default, rename = "agentRuntime")]
    pub agent_runtime: AgentRuntimeConfig,
    #[serde(default)]
    pub acp: AcpConfig,
    #[serde(default)]
    pub nodes: NodesConfig,
    #[serde(default)]
    pub observability: ObservabilityConfig,
    #[serde(default)]
    pub mcp: McpConfig,
    #[serde(default)]
    pub subagents: SubagentConfig,
    #[serde(default, rename = "promptDump")]
    pub prompt_dump: PromptDumpConfig,
    #[serde(default)]
    pub sessions: SessionsConfig,
    /// Session todo list (model-driven task checklist displayed in the TUI).
    #[serde(default)]
    pub todos: TodosConfig,
    /// Inferred commitments (automation-advanced Phase B): natural-language
    /// follow-ups mentioned in conversation are turned into one-shot cron jobs.
    #[serde(default)]
    pub commitments: CommitmentsConfig,
    /// Declarative task flows (automation-advanced Phase C): named steps wired
    /// into a dependency DAG, executed via `legion flows run <id>`.
    #[serde(default)]
    pub flows: Vec<TaskFlow>,
}

impl Config {
    pub fn from_json(json: &str) -> Result<Self, ConfigError> {
        let raw: serde_json::Value = serde_json::from_str(json)?;
        let resolved = resolve_env_vars(&raw)?;
        let mut config: Config = serde_json::from_value(resolved)?;
        config.validate()?;
        Ok(config)
    }

    pub fn from_json5(json5: &str) -> Result<Self, ConfigError> {
        let raw: serde_json::Value = serde_json5::from_str(json5)?;
        let resolved = resolve_env_vars(&raw)?;
        let mut config: Config = serde_json::from_value(resolved)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&mut self) -> Result<(), ConfigError> {
        self.gateway.validate()?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GatewayConfig {
    #[serde(default = "default_bind_host")]
    pub bind_host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub auth: AuthConfig,
}

fn default_bind_host() -> String {
    "127.0.0.1".to_string()
}

fn default_port() -> u16 {
    18789
}

impl GatewayConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        match self.auth.mode.as_str() {
            "token" | "password" | "trusted-proxy" => Ok(()),
            "none" => {
                if self.bind_host == "127.0.0.1" || self.bind_host == "localhost" {
                    Ok(())
                } else {
                    Err(ConfigError::UnsafeAuthModeNone(self.bind_host.clone()))
                }
            }
            other => Err(ConfigError::InvalidAuthMode(other.to_string())),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AuthConfig {
    #[serde(default = "default_auth_mode")]
    pub mode: String,
    pub token: Option<String>,
    pub password: Option<String>,
    #[serde(default)]
    pub allow_tailscale: bool,
}

fn default_auth_mode() -> String {
    "token".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct AgentsConfig {
    #[serde(default)]
    pub defaults: AgentDefaults,
    #[serde(default)]
    pub list: Vec<AgentConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AgentDefaults {
    #[serde(default = "default_workspace")]
    pub workspace: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u64,
    #[serde(default)]
    pub skills: SkillsConfig,
    /// Standing orders that apply to every agent (global scope). Per-agent
    /// scope is expressed by declaring orders on `agents.list[]` instead — the
    /// declaration position is the scope, so there is no separate scope enum.
    #[serde(default)]
    pub standing_orders: Vec<StandingOrder>,
    /// Maximum tool-loop iterations for a single turn. `None` means no limit.
    /// The default is `None` (no cap); set to a number to enforce one.
    #[serde(default = "default_max_iterations")]
    pub max_iterations: Option<usize>,
}

impl Default for AgentDefaults {
    fn default() -> Self {
        Self {
            workspace: default_workspace(),
            model: None,
            timeout_seconds: default_timeout_seconds(),
            standing_orders: Vec::new(),
            skills: SkillsConfig {
                enabled: false,
                dirs: default_skill_dirs(),
                max_summary_tokens: default_skill_max_summary_tokens(),
                max_body_tokens: default_skill_max_body_tokens(),
                max_triggered_skills: default_skill_max_triggered_skills(),
                selector_model: None,
            },
            max_iterations: default_max_iterations(),
        }
    }
}

fn default_workspace() -> String {
    "~/.legion/workspace".to_string()
}

fn default_timeout_seconds() -> u64 {
    172800
}

fn default_max_iterations() -> Option<usize> {
    None
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct AgentConfig {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    /// Replaces the default `Base` prompt section for this agent (not appended).
    #[serde(default)]
    pub custom_system_prompt: Option<String>,
    /// Always appended at the very end of the system prompt.
    #[serde(default)]
    pub append_system_prompt: Option<String>,
    /// Output-style instruction injected as its own section (e.g. `concise`).
    #[serde(default)]
    pub output_style: Option<String>,
    /// Language instruction injected as its own section (e.g. `zh-CN`).
    #[serde(default)]
    pub language: Option<String>,
    /// Agent ids allowed to send messages to this agent via
    /// `agent_to_agent_send` (tools-p1p2 Phase B). Empty means "deny all"
    /// (safe default): cross-agent delivery is opt-in per target agent.
    #[serde(default)]
    pub allow_from: Vec<String>,
    /// Standing orders scoped to this agent only; merged after the global
    /// `agents.defaults.standingOrders` when the prompt is assembled.
    #[serde(default)]
    pub standing_orders: Vec<StandingOrder>,
    /// Per-agent override for the tool-loop iteration cap. `None` falls back to
    /// `agents.defaults.maxIterations`.
    #[serde(default)]
    pub max_iterations: Option<usize>,
}

/// A standing order: a persistent authorization/boundary injected into the
/// system prompt every turn. Source is configuration only — never user
/// messages (prompt-injection hardening, automation-advanced gap §6.1).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct StandingOrder {
    pub id: String,
    pub instruction: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// A declarative task flow: a named DAG of agent steps (automation-advanced
/// Phase C). Linear chains and dependency-parallel execution only; conditional
/// branches and revision loops are deferred to Phase D.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TaskFlow {
    pub id: String,
    #[serde(default = "default_main_agent")]
    pub agent_id: String,
    pub steps: Vec<FlowStep>,
    #[serde(default)]
    pub on_failure: FlowFailurePolicy,
}

fn default_main_agent() -> String {
    "main".to_string()
}

/// A single step in a [`TaskFlow`]. `depends_on` names other steps that must
/// complete before this step starts.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FlowStep {
    pub name: String,
    pub message: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
}

/// What to do with the remaining steps when a flow step fails.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum FlowFailurePolicy {
    /// Stop the flow; every not-yet-started step is skipped.
    #[default]
    Abort,
    /// Skip only steps that transitively depend on the failed step; unrelated
    /// branches keep running.
    Continue,
}

/// A routing rule that maps an inbound message to an agent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Binding {
    #[serde(rename = "agentId")]
    pub agent_id: String,
    #[serde(rename = "match")]
    pub match_: BindingMatch,
}

/// Criteria used to decide whether a [`Binding`] applies to a message.
///
/// Any field that is `None` is treated as a wildcard and matches every message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct BindingMatch {
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(default)]
    pub peer: Option<PeerMatch>,
    #[serde(default)]
    pub guild_id: Option<String>,
    #[serde(default)]
    pub team_id: Option<String>,
}

/// Peer-level matching criteria within a [`BindingMatch`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PeerMatch {
    pub kind: String,
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct ModelsConfig {
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,
    #[serde(default)]
    pub aliases: HashMap<String, String>,
    #[serde(default)]
    pub fallbacks: Vec<String>,
    /// Per-model cost rates (USD per 1k tokens). Keys may be fully qualified
    /// (`"<provider>/<model>"`) or bare (`"<model>"`); the router resolves
    /// fully qualified keys first, then falls back to the bare model name.
    #[serde(default)]
    pub costs: HashMap<String, ModelCost>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProviderConfig {
    pub id: String,
    #[serde(default = "default_provider_kind")]
    pub kind: String,
    #[serde(default)]
    pub base_url: Option<String>,
    pub auth_profile: String,
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
    #[serde(default)]
    pub default_model: Option<String>,
    /// Single-provider retry policy applied before falling back to the next
    /// provider in the chain. Absent means "no retry".
    #[serde(default)]
    pub retry: Option<RetryConfig>,
    /// Optional per-provider RPM/TPM rate limiting (serialized as `rateLimit`).
    #[serde(default)]
    pub rate_limit: Option<RateLimitConfig>,
    #[serde(default)]
    pub extra_headers: HashMap<String, String>,
    #[serde(default)]
    pub extra_params: serde_json::Value,
}

fn default_provider_kind() -> String {
    "openai".to_string()
}

/// Retry policy for a single provider call.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RetryConfig {
    /// Maximum attempts per call, including the first one.
    #[serde(default = "default_retry_max_attempts")]
    pub max_attempts: u8,
    #[serde(default)]
    pub backoff: BackoffConfig,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: default_retry_max_attempts(),
            backoff: BackoffConfig::default(),
        }
    }
}

fn default_retry_max_attempts() -> u8 {
    3
}

/// Backoff strategy between retry attempts.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum BackoffConfig {
    Exponential {
        #[serde(default = "default_backoff_base_ms", rename = "baseMs")]
        base_ms: u64,
        #[serde(default = "default_backoff_max_ms", rename = "maxMs")]
        max_ms: u64,
    },
    Fixed {
        ms: u64,
    },
}

impl Default for BackoffConfig {
    fn default() -> Self {
        Self::Exponential {
            base_ms: default_backoff_base_ms(),
            max_ms: default_backoff_max_ms(),
        }
    }
}

fn default_backoff_base_ms() -> u64 {
    500
}

fn default_backoff_max_ms() -> u64 {
    8000
}

/// Per-provider rate limits (requests/tokens per minute).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitConfig {
    #[serde(default)]
    pub rpm: Option<u32>,
    #[serde(default)]
    pub tpm: Option<u32>,
}

/// Cost rate for a model, in USD per 1k tokens.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ModelCost {
    pub input_per_1k: f64,
    pub output_per_1k: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct MemoryConfig {
    #[serde(default = "default_memory_backend")]
    pub backend: String,
    #[serde(default)]
    pub builtin: BuiltinMemoryConfig,
    #[serde(default)]
    pub auto_extract: AutoExtractConfig,
    #[serde(default)]
    pub recall: RecallConfig,
    #[serde(default)]
    pub decay: DecayConfig,
    #[serde(default)]
    pub merge: MergeConfig,
}

fn default_memory_backend() -> String {
    "builtin".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct BuiltinMemoryConfig {
    #[serde(default = "default_memory_engine")]
    pub engine: String,
    #[serde(default)]
    pub embedding_provider: Option<String>,
    #[serde(default)]
    pub collection_path: Option<String>,
    #[serde(default = "default_true")]
    pub fts_enabled: bool,
    #[serde(default = "default_true")]
    pub hybrid_enabled: bool,
    #[serde(default = "default_embedding_dimension")]
    pub embedding_dimension: usize,
}

impl Default for BuiltinMemoryConfig {
    fn default() -> Self {
        Self {
            engine: default_memory_engine(),
            embedding_provider: None,
            collection_path: None,
            fts_enabled: default_true(),
            hybrid_enabled: default_true(),
            embedding_dimension: default_embedding_dimension(),
        }
    }
}

/// Background auto-extraction of durable facts into the Episodic memory layer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AutoExtractConfig {
    /// Master switch. When `false` (default), no background extraction runs and
    /// memory stays fully manual.
    #[serde(default)]
    pub enabled: bool,
    /// Cheap model reference used to extract durable facts (e.g. `openai/gpt-4o-mini`).
    /// Required when `enabled = true`; otherwise extraction is skipped with a warning.
    #[serde(default)]
    pub model: Option<String>,
    /// Number of recent non-system messages fed to the extractor.
    #[serde(default = "default_auto_extract_max_messages")]
    pub max_messages: usize,
    /// Minimum seconds between two extraction runs for the same agent.
    #[serde(default = "default_auto_extract_cooldown_seconds")]
    pub cooldown_seconds: u64,
    /// Cap on facts persisted per turn.
    #[serde(default = "default_auto_extract_max_facts")]
    pub max_facts_per_turn: usize,
    /// Per-call timeout (seconds) for the extractor LLM.
    #[serde(default = "default_auto_extract_timeout_seconds")]
    pub timeout_seconds: u64,
}

impl Default for AutoExtractConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model: None,
            max_messages: default_auto_extract_max_messages(),
            cooldown_seconds: default_auto_extract_cooldown_seconds(),
            max_facts_per_turn: default_auto_extract_max_facts(),
            timeout_seconds: default_auto_extract_timeout_seconds(),
        }
    }
}

fn default_auto_extract_max_messages() -> usize {
    20
}

fn default_auto_extract_cooldown_seconds() -> u64 {
    300
}

fn default_auto_extract_max_facts() -> usize {
    5
}

fn default_auto_extract_timeout_seconds() -> u64 {
    20
}

/// Background inference of commitments (automation-advanced Phase B): a cheap
/// LLM scans the finished turn for natural-language follow-ups the user asked
/// for and schedules one-shot cron jobs for them.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CommitmentsConfig {
    /// Master switch. When `false` (default), no commitment inference runs.
    #[serde(default)]
    pub enabled: bool,
    /// Cheap model reference used to infer commitments (e.g. `openai/gpt-4o-mini`).
    /// Required when `enabled = true`; otherwise inference is skipped with a warning.
    #[serde(default)]
    pub model: Option<String>,
    /// Number of recent non-system messages fed to the extractor.
    #[serde(default = "default_commitments_max_messages")]
    pub max_messages: usize,
    /// Minimum seconds between two extraction runs for the same agent.
    #[serde(default = "default_commitments_cooldown_seconds")]
    pub cooldown_seconds: u64,
    /// Cap on commitments scheduled per turn.
    #[serde(default = "default_commitments_max_per_turn")]
    pub max_per_turn: usize,
    /// Per-call timeout (seconds) for the extractor LLM.
    #[serde(default = "default_commitments_timeout_seconds")]
    pub timeout_seconds: u64,
}

impl Default for CommitmentsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model: None,
            max_messages: default_commitments_max_messages(),
            cooldown_seconds: default_commitments_cooldown_seconds(),
            max_per_turn: default_commitments_max_per_turn(),
            timeout_seconds: default_commitments_timeout_seconds(),
        }
    }
}

fn default_commitments_max_messages() -> usize {
    20
}

fn default_commitments_cooldown_seconds() -> u64 {
    300
}

fn default_commitments_max_per_turn() -> usize {
    3
}

fn default_commitments_timeout_seconds() -> u64 {
    20
}

/// Controls memory recall (Phase C): result limit and optional LLM re-ranking.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RecallConfig {
    /// Maximum number of memories injected per turn (default 5).
    #[serde(default = "default_recall_limit")]
    pub limit: usize,
    /// When `true`, a cheap model re-ranks recalled candidates before injection.
    /// Requires `selector_model`.
    #[serde(default)]
    pub use_llm_selector: bool,
    /// Cheap model reference for the LLM recall selector (e.g. `openai/gpt-4o-mini`).
    #[serde(default)]
    pub selector_model: Option<String>,
}

impl Default for RecallConfig {
    fn default() -> Self {
        Self {
            limit: default_recall_limit(),
            use_llm_selector: false,
            selector_model: None,
        }
    }
}

fn default_recall_limit() -> usize {
    5
}

/// Query-time age decay for the Episodic layer (Phase C). Disabled by default so
/// existing ranking is preserved until explicitly enabled.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DecayConfig {
    /// Master switch. When `false` (default), no decay is applied.
    #[serde(default)]
    pub enabled: bool,
    /// Half-life in days for episodic score decay (`score *= 0.5^(age/halfLife)`).
    #[serde(default = "default_decay_half_life_days")]
    pub half_life_days: f32,
}

impl Default for DecayConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            half_life_days: default_decay_half_life_days(),
        }
    }
}

fn default_decay_half_life_days() -> f32 {
    30.0
}

/// Explicit, operator-triggered merge of near-duplicate Episodic entries
/// (Phase C). Disabled by default; run via `legion memory merge`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MergeConfig {
    /// Master switch. When `false` (default), the merge CLI refuses to run.
    #[serde(default)]
    pub enabled: bool,
    /// Optional cheap model used to synthesise a merged fact per duplicate group.
    /// When `None`, the newest entry of each group is kept verbatim.
    #[serde(default)]
    pub model: Option<String>,
    /// Cosine-similarity threshold for grouping duplicates.
    #[serde(default = "default_merge_similarity_threshold")]
    pub similarity_threshold: f32,
    /// Cap on episodic candidates scanned in one merge run.
    #[serde(default = "default_merge_max_candidates")]
    pub max_candidates: usize,
}

impl Default for MergeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model: None,
            similarity_threshold: default_merge_similarity_threshold(),
            max_candidates: default_merge_max_candidates(),
        }
    }
}

fn default_merge_similarity_threshold() -> f32 {
    0.92
}

fn default_merge_max_candidates() -> usize {
    200
}

/// Limits and defaults for sub-agent delegation (multi-agent Phase A). All
/// bounds are defensive defaults so an agent cannot fan out or loop
/// uncontrollably; `spawn_subagent` is the only construction path and honors
/// these values.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SubagentConfig {
    /// Maximum concurrent sub-agent runs per gateway (default 4).
    #[serde(default = "default_subagent_max_concurrent")]
    pub max_concurrent: usize,
    /// Default per-sub-agent timeout in milliseconds (default 120000).
    #[serde(default = "default_subagent_timeout_ms")]
    pub default_timeout_ms: u64,
    /// Default max tool-loop iterations for a sub-agent (default 5).
    #[serde(default = "default_subagent_max_iterations")]
    pub default_max_iterations: usize,
    /// Maximum nesting depth of sub-agents (default 2). A spawn attempted at
    /// `depth >= max_depth` is rejected.
    #[serde(default = "default_subagent_max_depth")]
    pub max_depth: u8,
}

impl Default for SubagentConfig {
    fn default() -> Self {
        Self {
            max_concurrent: default_subagent_max_concurrent(),
            default_timeout_ms: default_subagent_timeout_ms(),
            default_max_iterations: default_subagent_max_iterations(),
            max_depth: default_subagent_max_depth(),
        }
    }
}

fn default_subagent_max_concurrent() -> usize {
    4
}

fn default_subagent_timeout_ms() -> u64 {
    120_000
}

fn default_subagent_max_iterations() -> usize {
    5
}

fn default_subagent_max_depth() -> u8 {
    2
}

/// System-prompt dumping (prompt-management Phase C). When enabled, every
/// agent run appends a JSONL record of the assembled system prompt —
/// per-section tokens, sources, truncation, and cache prefix — to
/// `~/.legion/dump-prompts/<session>.jsonl` (mode 0600). Inspect the latest
/// record with `legion context <session>`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PromptDumpConfig {
    /// Dump the assembled system prompt for every run (default false).
    #[serde(default)]
    pub enabled: bool,
}

/// How to repair tool-call/result mismatches when resuming a transcript that
/// was interrupted mid tool-execution (session-resume Phase B).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum OrphanPolicy {
    /// Delete tool calls without results and tool results without calls.
    DropOrphan,
    /// Synthesize `[interrupted]` placeholder results for tool calls that
    /// never produced one, keeping provider API invariants (default).
    #[default]
    Synthesize,
}

/// Session resume / maintenance settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionsConfig {
    /// Repair strategy for orphaned tool calls/results on resume
    /// (default `synthesize`).
    #[serde(default)]
    pub orphan_policy: OrphanPolicy,
    /// Bytes read from the head of a transcript for lite summaries
    /// (default 65536).
    #[serde(default = "default_lite_read_buffer_bytes")]
    pub lite_read_buffer_bytes: usize,
    /// Archive transcripts idle for more than this many days
    /// (default 0 = never archive).
    #[serde(default)]
    pub ttl_days: u64,
    /// Where archived transcripts are moved (default `~/.legion/archive`).
    /// Archiving moves files instead of deleting them, so they can be
    /// restored by moving them back.
    #[serde(default = "default_archive_dir")]
    pub archive_dir: String,
}

impl Default for SessionsConfig {
    fn default() -> Self {
        Self {
            orphan_policy: OrphanPolicy::default(),
            lite_read_buffer_bytes: default_lite_read_buffer_bytes(),
            ttl_days: 0,
            archive_dir: default_archive_dir(),
        }
    }
}

/// Controls the session todo list displayed during agent execution.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TodosConfig {
    /// Master switch. When `false`, the `todo_write` tool is not registered
    /// and the TUI does not render the todo panel.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Maximum number of todo items shown in the TUI at once (default 10).
    #[serde(default = "default_todos_max_display")]
    pub max_display: usize,
    /// Seconds to keep the list visible after all items are completed.
    /// Set to 0 to disable auto-hide.
    #[serde(default = "default_todos_auto_hide_seconds")]
    pub auto_hide_seconds: u64,
}

impl Default for TodosConfig {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            max_display: default_todos_max_display(),
            auto_hide_seconds: default_todos_auto_hide_seconds(),
        }
    }
}

fn default_todos_max_display() -> usize {
    10
}

fn default_todos_auto_hide_seconds() -> u64 {
    5
}

fn default_lite_read_buffer_bytes() -> usize {
    65_536
}

fn default_archive_dir() -> String {
    "~/.legion/archive".to_string()
}

fn default_memory_engine() -> String {
    "sqlite-zvec".to_string()
}

fn default_embedding_dimension() -> usize {
    1536
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct ToolConfig {
    #[serde(default)]
    pub approval: Option<String>,
    #[serde(default)]
    pub allow_from: Vec<String>,
    #[serde(default)]
    pub workspace_only: Option<bool>,
    #[serde(default)]
    pub sandbox: Option<String>,
    /// Opaque backend-specific configuration. Each tool backend may define its
    /// own schema here (e.g. `sandboxConfig` for the CubeSandbox exec backend).
    #[serde(default, flatten)]
    pub extra: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct SessionConfig {
    #[serde(default = "default_dm_scope")]
    pub dm_scope: String,
    #[serde(default)]
    pub reset: Option<ResetConfig>,
    #[serde(default)]
    pub maintenance: MaintenanceConfig,
}

fn default_dm_scope() -> String {
    "main".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CompactionConfig {
    /// Model context window in tokens.
    #[serde(default = "default_context_window")]
    pub context_window: usize,
    /// Fraction of the context window that triggers compaction.
    #[serde(default = "default_compaction_threshold_ratio")]
    pub threshold_ratio: f32,
    /// Minimum number of recent messages to preserve verbatim.
    #[serde(default = "default_min_messages_to_keep")]
    pub min_messages_to_keep: usize,
    /// Maximum tokens allocated to the summary subagent.
    #[serde(default = "default_max_summary_tokens")]
    pub max_summary_tokens: usize,
    /// Number of tokens reserved for the next model turn. When the current
    /// estimated tokens reach `context_window - buffer_tokens`, compaction is
    /// triggered early so the next turn still fits.
    #[serde(default = "default_buffer_tokens")]
    pub buffer_tokens: usize,
    /// Maximum consecutive compaction failures before auto-compaction is
    /// disabled to avoid infinite API retry loops.
    #[serde(default = "default_max_consecutive_failures")]
    pub max_consecutive_failures: u8,
    /// Replace image attachments (data URIs and Markdown images) with a short
    /// placeholder before summarization.
    #[serde(default = "default_strip_images")]
    pub strip_images: bool,
    /// Replace non-image attachments (data URIs) with a short placeholder
    /// before summarization.
    #[serde(default = "default_strip_documents")]
    pub strip_documents: bool,
    /// Enable provider-specific prompt cache breakpoints (Anthropic).
    #[serde(default = "default_use_prompt_cache")]
    pub use_prompt_cache: bool,
    /// Optional model ref used for summary generation. When None, the main
    /// conversation model is used.
    #[serde(default)]
    pub summary_model: Option<String>,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            context_window: default_context_window(),
            threshold_ratio: default_compaction_threshold_ratio(),
            min_messages_to_keep: default_min_messages_to_keep(),
            max_summary_tokens: default_max_summary_tokens(),
            buffer_tokens: default_buffer_tokens(),
            max_consecutive_failures: default_max_consecutive_failures(),
            strip_images: default_strip_images(),
            strip_documents: default_strip_documents(),
            use_prompt_cache: default_use_prompt_cache(),
            summary_model: None,
        }
    }
}

fn default_context_window() -> usize {
    128_000
}

fn default_compaction_threshold_ratio() -> f32 {
    0.75
}

fn default_min_messages_to_keep() -> usize {
    4
}

fn default_max_summary_tokens() -> usize {
    2_048
}

fn default_buffer_tokens() -> usize {
    13_000
}

fn default_max_consecutive_failures() -> u8 {
    3
}

fn default_strip_images() -> bool {
    true
}

fn default_strip_documents() -> bool {
    true
}

fn default_use_prompt_cache() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct ResetConfig {
    #[serde(default = "default_reset_mode")]
    pub mode: String,
    #[serde(default = "default_reset_hour")]
    pub at_hour: u8,
}

fn default_reset_mode() -> String {
    "daily".to_string()
}

fn default_reset_hour() -> u8 {
    4
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct MaintenanceConfig {
    #[serde(default = "default_maintenance_mode")]
    pub mode: String,
    #[serde(default = "default_prune_after")]
    pub prune_after: String,
    #[serde(default = "default_max_entries")]
    pub max_entries: usize,
}

fn default_maintenance_mode() -> String {
    "enforce".to_string()
}

fn default_prune_after() -> String {
    "30d".to_string()
}

fn default_max_entries() -> usize {
    500
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct HeartbeatConfig {
    #[serde(default = "default_heartbeat_enabled")]
    pub enabled: bool,
    #[serde(default = "default_heartbeat_interval")]
    pub interval_minutes: u32,
}

fn default_heartbeat_enabled() -> bool {
    true
}

fn default_heartbeat_interval() -> u32 {
    30
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PluginsConfig {
    #[serde(default)]
    pub slots: HashMap<String, String>,
    #[serde(default)]
    pub entries: HashMap<String, PluginEntryConfig>,
    /// Directories scanned for plugin manifests (`<dir>/<plugin>/manifest.json`).
    #[serde(default = "default_plugin_dirs")]
    pub dirs: Vec<std::path::PathBuf>,
    /// Plugin ids that are explicitly disabled.
    #[serde(default)]
    pub disabled: Vec<String>,
}

fn default_plugin_dirs() -> Vec<std::path::PathBuf> {
    vec![
        dirs::home_dir()
            .map(|h| h.join(".legion").join("plugins"))
            .unwrap_or_else(|| std::path::PathBuf::from(".legion/plugins")),
    ]
}

impl Default for PluginsConfig {
    fn default() -> Self {
        Self {
            slots: HashMap::new(),
            entries: HashMap::new(),
            dirs: default_plugin_dirs(),
            disabled: Vec::new(),
        }
    }
}

fn default_skill_dirs() -> Vec<std::path::PathBuf> {
    let home = dirs::home_dir();
    vec![
        home.as_ref()
            .map(|h| h.join(".agents").join("skills"))
            .unwrap_or_else(|| std::path::PathBuf::from(".agents/skills")),
        home.map(|h| h.join(".legion").join("skills"))
            .unwrap_or_else(|| std::path::PathBuf::from(".legion/skills")),
    ]
}

fn default_skill_max_summary_tokens() -> usize {
    800
}

fn default_skill_max_body_tokens() -> usize {
    2_000
}

fn default_skill_max_triggered_skills() -> usize {
    3
}

/// Configuration for the skill subsystem.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SkillsConfig {
    /// Directories scanned for skill directories containing `SKILL.md`.
    #[serde(default = "default_skill_dirs")]
    pub dirs: Vec<std::path::PathBuf>,
    /// Maximum tokens injected as the skill summary block.
    #[serde(default = "default_skill_max_summary_tokens")]
    pub max_summary_tokens: usize,
    /// Maximum tokens injected as the full skill body block when a skill is
    /// triggered by file paths or recalled by intent.
    #[serde(default = "default_skill_max_body_tokens")]
    pub max_body_tokens: usize,
    /// Maximum number of skills whose full body may be injected in a single
    /// turn.
    #[serde(default = "default_skill_max_triggered_skills")]
    pub max_triggered_skills: usize,
    /// Global switch. When `false`, no skill content is injected.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Optional model reference for the LLM skill selector. When `None`,
    /// keyword matching is used to recall skill bodies.
    #[serde(default)]
    pub selector_model: Option<String>,
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            dirs: default_skill_dirs(),
            max_summary_tokens: default_skill_max_summary_tokens(),
            max_body_tokens: default_skill_max_body_tokens(),
            max_triggered_skills: default_skill_max_triggered_skills(),
            enabled: true,
            selector_model: None,
        }
    }
}

impl<'de> Deserialize<'de> for SkillsConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct SkillsConfigVisitor;

        impl<'de> Visitor<'de> for SkillsConfigVisitor {
            type Value = SkillsConfig;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a skill config object or an array of directory strings")
            }

            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut dirs = Vec::new();
                while let Some(dir) = seq.next_element::<String>()? {
                    dirs.push(std::path::PathBuf::from(dir));
                }
                Ok(SkillsConfig {
                    dirs,
                    max_summary_tokens: default_skill_max_summary_tokens(),
                    max_body_tokens: default_skill_max_body_tokens(),
                    max_triggered_skills: default_skill_max_triggered_skills(),
                    enabled: true,
                    selector_model: None,
                })
            }

            fn visit_map<M>(self, map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                #[derive(Default, Deserialize)]
                #[serde(rename_all = "camelCase", default)]
                struct Raw {
                    dirs: Vec<std::path::PathBuf>,
                    #[serde(default = "default_skill_max_summary_tokens")]
                    max_summary_tokens: usize,
                    #[serde(default = "default_skill_max_body_tokens")]
                    max_body_tokens: usize,
                    #[serde(default = "default_skill_max_triggered_skills")]
                    max_triggered_skills: usize,
                    // Keep parity with the array form (visit_seq) and
                    // `SkillsConfig::default()`, which both default to true.
                    #[serde(default = "default_true")]
                    enabled: bool,
                    #[serde(default)]
                    selector_model: Option<String>,
                }

                let raw = Raw::deserialize(de::value::MapAccessDeserializer::new(map))?;
                Ok(SkillsConfig {
                    dirs: raw.dirs,
                    max_summary_tokens: raw.max_summary_tokens,
                    max_body_tokens: raw.max_body_tokens,
                    max_triggered_skills: raw.max_triggered_skills,
                    enabled: raw.enabled,
                    selector_model: raw.selector_model,
                })
            }
        }

        deserializer.deserialize_any(SkillsConfigVisitor)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct PluginEntryConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub config: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct AgentRuntimeConfig {
    /// Explicitly selected harness id, e.g. `"built-in"` or `"acp"`.
    pub id: Option<String>,
    /// Selected context engine implementation. `None` and `"legacy"` both select
    /// the built-in context engine.
    #[serde(default)]
    pub context_engine: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct AcpConfig {
    /// Command used to spawn the external ACP harness process.
    /// Example: `["codex", "--acp"]`.
    pub command: Option<Vec<String>>,
}

fn default_connect_timeout_ms() -> u64 {
    15_000
}

/// Transport used to reach an MCP server.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", tag = "type")]
pub enum McpTransport {
    /// Spawn a local subprocess and speak JSON-RPC over stdin/stdout.
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: HashMap<String, String>,
    },
    /// POST JSON-RPC requests to an HTTP endpoint.
    Http {
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
    },
    /// Server-Sent Events transport: a long-lived GET stream delivers
    /// responses while requests are POSTed to an endpoint announced over the
    /// stream (MCP dual-channel SSE).
    Sse {
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
    },
    /// WebSocket transport: a single bidirectional connection carries both
    /// requests and responses.
    Ws {
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
    },
}

/// Configuration for a single MCP server.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct McpServerConfig {
    /// Server id, used as the tool namespace prefix (`mcp__<name>__<tool>`).
    pub name: String,
    /// How to reach the server. Flattened so the `type` discriminator sits
    /// next to the other fields.
    #[serde(flatten)]
    pub transport: McpTransport,
    /// When `false`, the server is skipped at load time.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Tool names that bypass the default `required` approval. Names are
    /// matched against the raw MCP tool name (without the `mcp__<server>__`
    /// prefix).
    #[serde(default)]
    pub auto_approve: Vec<String>,
    /// Timeout used when connecting / listing tools. Falls back to 15s.
    #[serde(default = "default_connect_timeout_ms")]
    pub connect_timeout_ms: u64,
}

/// Configuration for the MCP subsystem.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct McpConfig {
    /// Configured MCP servers.
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ObservabilityConfig {
    #[serde(default = "default_observability_enabled")]
    pub enabled: bool,
    #[serde(default = "default_metrics_enabled")]
    pub metrics_enabled: bool,
    #[serde(default = "default_metrics_path")]
    pub metrics_path: String,
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            enabled: default_observability_enabled(),
            metrics_enabled: default_metrics_enabled(),
            metrics_path: default_metrics_path(),
        }
    }
}

fn default_observability_enabled() -> bool {
    true
}

fn default_metrics_enabled() -> bool {
    true
}

fn default_metrics_path() -> String {
    "/metrics".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct NodesConfig {
    /// Commands that nodes are explicitly allowed to invoke, even if they are
    /// not in the platform defaults.
    #[serde(default)]
    pub allow_commands: Vec<String>,
    /// Commands that are always denied, overriding platform defaults and
    /// allow_commands.
    #[serde(default)]
    pub deny_commands: Vec<String>,
    /// Optional CIDRs from which first-time node pairing requests are
    /// auto-approved.
    #[serde(default)]
    pub auto_approve_cidrs: Vec<String>,
}

impl NodesConfig {
    /// Return true if `command` is allowed under the configured policy.
    pub fn is_command_allowed(&self, platform: &str, command: &str) -> bool {
        if self.deny_commands.iter().any(|c| c == command) {
            return false;
        }
        if self.allow_commands.iter().any(|c| c == command) {
            return true;
        }
        default_node_command_allowed(platform, command)
    }
}

fn default_node_command_allowed(platform: &str, command: &str) -> bool {
    let dangerous = [
        "camera.snap",
        "camera.clip",
        "screen.record",
        "contacts.add",
        "calendar.add",
        "reminders.add",
        "sms.send",
        "sms.search",
    ];
    if dangerous.contains(&command) {
        return false;
    }

    let base = [
        "camera.list",
        "location.get",
        "device.info",
        "device.status",
        "contacts.search",
        "calendar.events",
        "reminders.list",
        "photos.latest",
        "motion.activity",
        "motion.pedometer",
        "system.notify",
    ];
    if base.contains(&command) {
        return true;
    }

    // Canvas commands are allowed on non-Linux platforms.
    if platform != "linux" && command.starts_with("canvas.") {
        return true;
    }

    // Talk commands are allowed when advertised.
    if command.starts_with("talk.") {
        return true;
    }

    false
}

fn resolve_env_vars(value: &serde_json::Value) -> Result<serde_json::Value, ConfigError> {
    match value {
        serde_json::Value::String(s) => {
            if s.starts_with("${") && s.ends_with('}') && s.matches("${").count() == 1 {
                let var_name = &s[2..s.len() - 1];
                let default_value = var_name.split_once(':').map(|(_, v)| v);
                let var_name = var_name.split_once(':').map(|(n, _)| n).unwrap_or(var_name);

                match std::env::var(var_name) {
                    Ok(v) => Ok(serde_json::Value::String(v)),
                    Err(_) => match default_value {
                        Some(d) => Ok(serde_json::Value::String(d.to_string())),
                        None => Err(ConfigError::UnresolvedEnvVar(var_name.to_string())),
                    },
                }
            } else {
                Ok(serde_json::Value::String(s.clone()))
            }
        }
        serde_json::Value::Array(arr) => {
            let mut out = Vec::with_capacity(arr.len());
            for item in arr {
                out.push(resolve_env_vars(item)?);
            }
            Ok(serde_json::Value::Array(out))
        }
        serde_json::Value::Object(obj) => {
            let mut out = serde_json::Map::new();
            for (k, v) in obj {
                out.insert(k.clone(), resolve_env_vars(v)?);
            }
            Ok(serde_json::Value::Object(out))
        }
        other => Ok(other.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use temp_env::with_var;

    #[test]
    fn should_parse_minimal_config() {
        let json = r#"{
            "gateway": {
                "auth": { "token": "secret" }
            }
        }"#;

        let cfg = Config::from_json(json).unwrap();
        assert_eq!(cfg.gateway.bind_host, "127.0.0.1");
        assert_eq!(cfg.gateway.port, 18789);
        assert_eq!(cfg.gateway.auth.mode, "token");
        assert_eq!(cfg.gateway.auth.token, Some("secret".to_string()));
    }

    #[test]
    fn should_resolve_env_var_in_token() {
        with_var("LEGION_GATEWAY_TOKEN", Some("secret123"), || {
            let json = r#"{
                "gateway": {
                    "auth": { "token": "${LEGION_GATEWAY_TOKEN}" }
                }
            }"#;

            let cfg = Config::from_json(json).unwrap();
            assert_eq!(cfg.gateway.auth.token, Some("secret123".to_string()));
        });
    }

    #[test]
    fn should_use_default_for_missing_env_var() {
        with_var("LEGION_GATEWAY_TOKEN", None::<&str>, || {
            let json = r#"{
                "gateway": {
                    "auth": { "token": "${LEGION_GATEWAY_TOKEN:default-token}" }
                }
            }"#;

            let cfg = Config::from_json(json).unwrap();
            assert_eq!(cfg.gateway.auth.token, Some("default-token".to_string()));
        });
    }

    #[test]
    fn should_fail_on_unresolved_env_var() {
        with_var("MISSING_TOKEN", None::<&str>, || {
            let json = r#"{
                "gateway": {
                    "auth": { "token": "${MISSING_TOKEN}" }
                }
            }"#;

            let result = Config::from_json(json);
            assert!(matches!(result, Err(ConfigError::UnresolvedEnvVar(_))));
        });
    }

    #[test]
    fn should_reject_auth_mode_none_with_public_bind() {
        let json = r#"{
            "gateway": {
                "bindHost": "0.0.0.0",
                "auth": { "mode": "none" }
            }
        }"#;

        let result = Config::from_json(json);
        assert_eq!(
            result,
            Err(ConfigError::UnsafeAuthModeNone("0.0.0.0".to_string()))
        );
    }

    #[test]
    fn should_allow_auth_mode_none_on_loopback() {
        let json = r#"{
            "gateway": {
                "bindHost": "127.0.0.1",
                "auth": { "mode": "none" }
            }
        }"#;

        let cfg = Config::from_json(json).unwrap();
        assert_eq!(cfg.gateway.auth.mode, "none");
    }

    #[test]
    fn should_reject_invalid_auth_mode() {
        let json = r#"{
            "gateway": {
                "auth": { "mode": "magic" }
            }
        }"#;

        let result = Config::from_json(json);
        assert_eq!(
            result,
            Err(ConfigError::InvalidAuthMode("magic".to_string()))
        );
    }

    #[test]
    fn should_parse_json5_with_comments_and_trailing_commas() {
        let json5 = r#"{
            // Gateway config
            "gateway": {
                "auth": { "token": "secret" }, // inline comment
            },
        }"#;

        let cfg = Config::from_json5(json5).unwrap();
        assert_eq!(cfg.gateway.auth.token, Some("secret".to_string()));
    }

    #[test]
    fn should_resolve_env_vars_in_nested_objects() {
        with_var("ANTHROPIC_KEY", Some("ak-test"), || {
            let json = r#"{
                "gateway": { "auth": { "token": "secret" } },
                "models": {
                    "providers": {
                        "anthropic": {
                            "id": "anthropic",
                            "authProfile": "anthropic-default",
                            "baseUrl": "${ANTHROPIC_KEY}"
                        }
                    }
                }
            }"#;

            let cfg = Config::from_json(json).unwrap();
            let provider = cfg
                .models
                .providers
                .get("anthropic")
                .expect("anthropic provider should exist");
            assert_eq!(provider.base_url, Some("ak-test".to_string()));
        });
    }

    #[test]
    fn env_var_resolves_inside_arrays() {
        with_var("LEGION_SKILL_DIR", Some("/tmp/legion-skills"), || {
            let json = r#"{
                "gateway": { "auth": { "token": "secret" } },
                "agents": {
                    "defaults": {
                        "skills": { "dirs": ["${LEGION_SKILL_DIR}"] }
                    }
                }
            }"#;

            let cfg = Config::from_json(json).unwrap();
            assert_eq!(
                cfg.agents.defaults.skills.dirs,
                vec![std::path::PathBuf::from("/tmp/legion-skills")]
            );
        });
    }

    #[test]
    fn env_var_set_beats_default() {
        with_var("LEGION_SET_VAR", Some("real-value"), || {
            let json = r#"{
                "gateway": {
                    "auth": { "token": "${LEGION_SET_VAR:fallback-value}" }
                }
            }"#;

            let cfg = Config::from_json(json).unwrap();
            assert_eq!(cfg.gateway.auth.token, Some("real-value".to_string()));
        });
    }

    #[test]
    fn embedded_env_var_is_left_literal() {
        with_var("LEGION_EMBEDDED_VAR", Some("value"), || {
            // Deliberate design: only strings that *exactly* match
            // `${VAR}` / `${VAR:default}` (starts_with + ends_with) are
            // resolved; embedded references stay literal.
            let json = r#"{
                "gateway": {
                    "auth": { "token": "prefix-${LEGION_EMBEDDED_VAR}-suffix" }
                }
            }"#;

            let cfg = Config::from_json(json).unwrap();
            assert_eq!(
                cfg.gateway.auth.token,
                Some("prefix-${LEGION_EMBEDDED_VAR}-suffix".to_string())
            );
        });
    }

    #[test]
    fn skills_config_object_form_enabled_default() {
        // Object form omitting `enabled` must default to true, matching the
        // array form and `SkillsConfig::default()`.
        let json = r#"{
            "gateway": { "auth": { "token": "secret" } },
            "agents": {
                "defaults": { "skills": { "dirs": ["/tmp/x"] } }
            }
        }"#;
        let cfg = Config::from_json(json).unwrap();
        assert!(cfg.agents.defaults.skills.enabled);
        assert_eq!(
            cfg.agents.defaults.skills.dirs,
            vec![std::path::PathBuf::from("/tmp/x")]
        );

        // An explicit `enabled: false` is still honored.
        let json = r#"{
            "gateway": { "auth": { "token": "secret" } },
            "agents": {
                "defaults": { "skills": { "dirs": ["/tmp/x"], "enabled": false } }
            }
        }"#;
        let cfg = Config::from_json(json).unwrap();
        assert!(!cfg.agents.defaults.skills.enabled);
    }

    #[test]
    fn should_apply_agent_defaults() {
        let json = r#"{
            "gateway": { "auth": { "token": "secret" } }
        }"#;

        let cfg = Config::from_json(json).unwrap();
        assert_eq!(cfg.agents.defaults.workspace, "~/.legion/workspace");
        assert_eq!(cfg.agents.defaults.timeout_seconds, 172800);
        assert!(!cfg.agents.defaults.skills.enabled);
        assert_eq!(cfg.agents.defaults.skills.dirs, default_skill_dirs());
    }

    #[rstest::rstest]
    #[case("token", "0.0.0.0", true)]
    #[case("password", "0.0.0.0", true)]
    #[case("trusted-proxy", "0.0.0.0", true)]
    #[case("none", "127.0.0.1", true)]
    #[case("none", "localhost", true)]
    // `::1` is loopback but not in the accepted list — pin the rejection.
    #[case("none", "::1", false)]
    #[case("none", "0.0.0.0", false)]
    fn auth_mode_validation(#[case] mode: &str, #[case] host: &str, #[case] should_pass: bool) {
        let json = format!(
            r#"{{ "gateway": {{ "bindHost": "{}", "auth": {{ "mode": "{}" }} }} }}"#,
            host, mode
        );
        let result = Config::from_json(&json);
        assert_eq!(result.is_ok(), should_pass);
    }

    #[test]
    fn should_parse_agent_runtime_and_acp_config() {
        let json = r#"{
            "gateway": { "auth": { "token": "x" } },
            "agentRuntime": { "id": "acp" },
            "acp": { "command": ["codex", "--acp"] }
        }"#;

        let cfg = Config::from_json(json).unwrap();
        assert_eq!(cfg.agent_runtime.id, Some("acp".to_string()));
        assert_eq!(
            cfg.acp.command,
            Some(vec!["codex".to_string(), "--acp".to_string()])
        );
    }

    #[test]
    fn should_apply_observability_defaults() {
        let json = r#"{
            "gateway": { "auth": { "token": "x" } }
        }"#;

        let cfg = Config::from_json(json).unwrap();
        assert!(cfg.observability.enabled);
        assert!(cfg.observability.metrics_enabled);
        assert_eq!(cfg.observability.metrics_path, "/metrics");
    }

    #[test]
    fn should_parse_bindings() {
        let json = r#"{
            "gateway": { "auth": { "token": "x" } },
            "bindings": [
                { "agentId": "main", "match": { "channel": "telegram", "accountId": "default" } },
                { "agentId": "work", "match": { "channel": "slack", "accountId": "work", "peer": { "kind": "group", "id": "C123" } } }
            ]
        }"#;

        let cfg = Config::from_json(json).unwrap();
        assert_eq!(cfg.bindings.len(), 2);
        assert_eq!(cfg.bindings[0].agent_id, "main");
        assert_eq!(cfg.bindings[0].match_.channel, Some("telegram".into()));
        assert_eq!(cfg.bindings[0].match_.account_id, Some("default".into()));
        assert_eq!(cfg.bindings[1].agent_id, "work");
        assert_eq!(cfg.bindings[1].match_.peer.as_ref().unwrap().kind, "group");
        assert_eq!(cfg.bindings[1].match_.peer.as_ref().unwrap().id, "C123");
    }

    #[test]
    fn compaction_config_uses_new_defaults() {
        let json = r#"{ "gateway": { "auth": { "token": "x" } } }"#;
        let cfg = Config::from_json(json).unwrap();
        assert_eq!(cfg.compaction.context_window, 128_000);
        assert_eq!(cfg.compaction.threshold_ratio, 0.75);
        assert_eq!(cfg.compaction.min_messages_to_keep, 4);
        assert_eq!(cfg.compaction.max_summary_tokens, 2_048);
        assert_eq!(cfg.compaction.buffer_tokens, 13_000);
        assert_eq!(cfg.compaction.max_consecutive_failures, 3);
        assert!(cfg.compaction.strip_images);
        assert!(cfg.compaction.strip_documents);
    }

    #[test]
    fn compaction_config_parses_explicit_values() {
        let json = r#"{
            "gateway": { "auth": { "token": "x" } },
            "compaction": {
                "contextWindow": 100000,
                "thresholdRatio": 0.8,
                "minMessagesToKeep": 2,
                "maxSummaryTokens": 512,
                "bufferTokens": 5000,
                "maxConsecutiveFailures": 5,
                "stripImages": false,
                "stripDocuments": false,
                "usePromptCache": false,
                "summaryModel": "anthropic/claude-3-haiku"
            }
        }"#;
        let cfg = Config::from_json(json).unwrap();
        assert_eq!(cfg.compaction.context_window, 100_000);
        assert_eq!(cfg.compaction.threshold_ratio, 0.8);
        assert_eq!(cfg.compaction.min_messages_to_keep, 2);
        assert_eq!(cfg.compaction.max_summary_tokens, 512);
        assert_eq!(cfg.compaction.buffer_tokens, 5_000);
        assert_eq!(cfg.compaction.max_consecutive_failures, 5);
        assert!(!cfg.compaction.strip_images);
        assert!(!cfg.compaction.strip_documents);
        assert!(!cfg.compaction.use_prompt_cache);
        assert_eq!(
            cfg.compaction.summary_model,
            Some("anthropic/claude-3-haiku".to_string())
        );
    }

    #[test]
    fn compaction_config_uses_new_phase_c_defaults() {
        let json = r#"{ "gateway": { "auth": { "token": "x" } } }"#;
        let cfg = Config::from_json(json).unwrap();
        assert!(cfg.compaction.use_prompt_cache);
        assert_eq!(cfg.compaction.summary_model, None);
    }

    #[test]
    fn plugins_config_uses_default_dir_and_parses_disabled() {
        let json = r#"{
            "gateway": { "auth": { "token": "x" } },
            "plugins": {
                "dirs": ["/tmp/plugins"],
                "disabled": ["experimental"]
            }
        }"#;
        let cfg = Config::from_json(json).unwrap();
        assert_eq!(
            cfg.plugins.dirs,
            vec![std::path::PathBuf::from("/tmp/plugins")]
        );
        assert_eq!(cfg.plugins.disabled, vec!["experimental"]);
    }

    #[test]
    fn skills_config_uses_defaults() {
        let json = r#"{ "gateway": { "auth": { "token": "x" } } }"#;
        let cfg = Config::from_json(json).unwrap();
        assert_eq!(cfg.agents.defaults.skills.max_summary_tokens, 800);
        assert_eq!(cfg.agents.defaults.skills.max_body_tokens, 2_000);
        assert_eq!(cfg.agents.defaults.skills.max_triggered_skills, 3);
        assert!(!cfg.agents.defaults.skills.enabled);
        assert_eq!(cfg.agents.defaults.skills.selector_model, None);
    }

    #[test]
    fn skills_config_parses_explicit_values() {
        let json = r#"{
            "gateway": { "auth": { "token": "x" } },
            "agents": {
                "defaults": {
                    "skills": {
                        "enabled": true,
                        "dirs": ["/tmp/skills"],
                        "maxSummaryTokens": 500,
                        "maxBodyTokens": 1500,
                        "maxTriggeredSkills": 5,
                        "selectorModel": "openai/gpt-4o-mini"
                    }
                }
            }
        }"#;
        let cfg = Config::from_json(json).unwrap();
        assert!(cfg.agents.defaults.skills.enabled);
        assert_eq!(
            cfg.agents.defaults.skills.dirs,
            vec![std::path::PathBuf::from("/tmp/skills")]
        );
        assert_eq!(cfg.agents.defaults.skills.max_summary_tokens, 500);
        assert_eq!(cfg.agents.defaults.skills.max_body_tokens, 1_500);
        assert_eq!(cfg.agents.defaults.skills.max_triggered_skills, 5);
        assert_eq!(
            cfg.agents.defaults.skills.selector_model,
            Some("openai/gpt-4o-mini".to_string())
        );
    }

    #[test]
    fn skills_config_backwards_compatible_with_array() {
        let json = r#"{
            "gateway": { "auth": { "token": "x" } },
            "agents": {
                "defaults": {
                    "skills": ["/opt/skills"]
                }
            }
        }"#;
        let cfg = Config::from_json(json).unwrap();
        assert_eq!(
            cfg.agents.defaults.skills.dirs,
            vec![std::path::PathBuf::from("/opt/skills")]
        );
        assert_eq!(cfg.agents.defaults.skills.max_body_tokens, 2_000);
        assert_eq!(cfg.agents.defaults.skills.max_triggered_skills, 3);
        assert_eq!(cfg.agents.defaults.skills.selector_model, None);
    }

    #[test]
    fn memory_auto_extract_defaults_to_disabled() {
        let json = r#"{ "gateway": { "auth": { "token": "x" } } }"#;
        let cfg = Config::from_json(json).unwrap();
        let ae = &cfg.memory.auto_extract;
        assert!(!ae.enabled);
        assert_eq!(ae.model, None);
        assert_eq!(ae.max_messages, 20);
        assert_eq!(ae.cooldown_seconds, 300);
        assert_eq!(ae.max_facts_per_turn, 5);
        assert_eq!(ae.timeout_seconds, 20);
    }

    #[test]
    fn memory_auto_extract_parses_explicit_values() {
        let json = r#"{
            "gateway": { "auth": { "token": "x" } },
            "memory": {
                "autoExtract": {
                    "enabled": true,
                    "model": "openai/gpt-4o-mini",
                    "maxMessages": 12,
                    "cooldownSeconds": 60,
                    "maxFactsPerTurn": 3,
                    "timeoutSeconds": 10
                }
            }
        }"#;
        let cfg = Config::from_json(json).unwrap();
        let ae = &cfg.memory.auto_extract;
        assert!(ae.enabled);
        assert_eq!(ae.model, Some("openai/gpt-4o-mini".to_string()));
        assert_eq!(ae.max_messages, 12);
        assert_eq!(ae.cooldown_seconds, 60);
        assert_eq!(ae.max_facts_per_turn, 3);
        assert_eq!(ae.timeout_seconds, 10);
    }

    #[test]
    fn commitments_defaults_to_disabled() {
        let json = r#"{ "gateway": { "auth": { "token": "x" } } }"#;
        let cfg = Config::from_json(json).unwrap();
        let c = &cfg.commitments;
        assert!(!c.enabled);
        assert_eq!(c.model, None);
        assert_eq!(c.max_messages, 20);
        assert_eq!(c.cooldown_seconds, 300);
        assert_eq!(c.max_per_turn, 3);
        assert_eq!(c.timeout_seconds, 20);
    }

    #[test]
    fn commitments_parses_explicit_values() {
        let json = r#"{
            "gateway": { "auth": { "token": "x" } },
            "commitments": {
                "enabled": true,
                "model": "openai/gpt-4o-mini",
                "maxMessages": 12,
                "cooldownSeconds": 60,
                "maxPerTurn": 2,
                "timeoutSeconds": 10
            }
        }"#;
        let cfg = Config::from_json(json).unwrap();
        let c = &cfg.commitments;
        assert!(c.enabled);
        assert_eq!(c.model, Some("openai/gpt-4o-mini".to_string()));
        assert_eq!(c.max_messages, 12);
        assert_eq!(c.cooldown_seconds, 60);
        assert_eq!(c.max_per_turn, 2);
        assert_eq!(c.timeout_seconds, 10);
    }

    #[test]
    fn flows_default_to_empty() {
        let json = r#"{ "gateway": { "auth": { "token": "x" } } }"#;
        let cfg = Config::from_json(json).unwrap();
        assert!(cfg.flows.is_empty());
    }

    #[test]
    fn flows_parse_basic_dag() {
        let json = r#"{
            "gateway": { "auth": { "token": "x" } },
            "flows": [
                {
                    "id": "release",
                    "agentId": "work",
                    "onFailure": "continue",
                    "steps": [
                        { "name": "fetch", "message": "fetch data" },
                        { "name": "analyze", "message": "analyze it", "dependsOn": ["fetch"] },
                        { "name": "report", "message": "write report", "dependsOn": ["analyze"] }
                    ]
                }
            ]
        }"#;
        let cfg = Config::from_json(json).unwrap();
        assert_eq!(cfg.flows.len(), 1);
        let flow = &cfg.flows[0];
        assert_eq!(flow.id, "release");
        assert_eq!(flow.agent_id, "work");
        assert_eq!(flow.on_failure, FlowFailurePolicy::Continue);
        assert_eq!(flow.steps.len(), 3);
        assert_eq!(flow.steps[2].depends_on, vec!["analyze".to_string()]);
        assert!(flow.steps[0].depends_on.is_empty());
    }

    #[test]
    fn flows_default_agent_and_abort_policy() {
        let json = r#"{
            "gateway": { "auth": { "token": "x" } },
            "flows": [
                {
                    "id": "nightly",
                    "steps": [{ "name": "only", "message": "do it" }]
                }
            ]
        }"#;
        let cfg = Config::from_json(json).unwrap();
        let flow = &cfg.flows[0];
        assert_eq!(flow.agent_id, "main");
        assert_eq!(flow.on_failure, FlowFailurePolicy::Abort);
    }

    #[test]
    fn memory_recall_decay_merge_defaults() {
        let json = r#"{ "gateway": { "auth": { "token": "x" } } }"#;
        let cfg = Config::from_json(json).unwrap();
        assert_eq!(cfg.memory.recall.limit, 5);
        assert!(!cfg.memory.recall.use_llm_selector);
        assert_eq!(cfg.memory.recall.selector_model, None);
        assert!(!cfg.memory.decay.enabled);
        assert!((cfg.memory.decay.half_life_days - 30.0).abs() < f32::EPSILON);
        assert!(!cfg.memory.merge.enabled);
        assert_eq!(cfg.memory.merge.model, None);
        assert!((cfg.memory.merge.similarity_threshold - 0.92).abs() < f32::EPSILON);
        assert_eq!(cfg.memory.merge.max_candidates, 200);
    }

    #[test]
    fn memory_recall_decay_merge_explicit() {
        let json = r#"{
            "gateway": { "auth": { "token": "x" } },
            "memory": {
                "recall": { "limit": 8, "useLlmSelector": true, "selectorModel": "openai/gpt-4o-mini" },
                "decay": { "enabled": true, "halfLifeDays": 14.5 },
                "merge": { "enabled": true, "model": "openai/gpt-4o-mini", "similarityThreshold": 0.85, "maxCandidates": 50 }
            }
        }"#;
        let cfg = Config::from_json(json).unwrap();
        assert_eq!(cfg.memory.recall.limit, 8);
        assert!(cfg.memory.recall.use_llm_selector);
        assert_eq!(
            cfg.memory.recall.selector_model,
            Some("openai/gpt-4o-mini".to_string())
        );
        assert!(cfg.memory.decay.enabled);
        assert!((cfg.memory.decay.half_life_days - 14.5).abs() < f32::EPSILON);
        assert!(cfg.memory.merge.enabled);
        assert!((cfg.memory.merge.similarity_threshold - 0.85).abs() < f32::EPSILON);
        assert_eq!(cfg.memory.merge.max_candidates, 50);
    }

    #[test]
    fn subagent_config_defaults() {
        let json = r#"{ "gateway": { "auth": { "token": "x" } } }"#;
        let cfg = Config::from_json(json).unwrap();
        assert_eq!(cfg.subagents.max_concurrent, 4);
        assert_eq!(cfg.subagents.default_timeout_ms, 120_000);
        assert_eq!(cfg.subagents.default_max_iterations, 5);
        assert_eq!(cfg.subagents.max_depth, 2);
    }

    #[test]
    fn subagent_config_explicit() {
        let json = r#"{
            "gateway": { "auth": { "token": "x" } },
            "subagents": {
                "maxConcurrent": 8,
                "defaultTimeoutMs": 60000,
                "defaultMaxIterations": 3,
                "maxDepth": 1
            }
        }"#;
        let cfg = Config::from_json(json).unwrap();
        assert_eq!(cfg.subagents.max_concurrent, 8);
        assert_eq!(cfg.subagents.default_timeout_ms, 60_000);
        assert_eq!(cfg.subagents.default_max_iterations, 3);
        assert_eq!(cfg.subagents.max_depth, 1);
    }

    #[test]
    fn agent_max_iterations_defaults_to_none() {
        let json = r#"{ "gateway": { "auth": { "token": "x" } } }"#;
        let cfg = Config::from_json(json).unwrap();
        assert_eq!(cfg.agents.defaults.max_iterations, None);
    }

    #[test]
    fn agent_max_iterations_explicit_and_null() {
        let json = r#"{
            "gateway": { "auth": { "token": "x" } },
            "agents": {
                "defaults": { "maxIterations": null },
                "list": [
                    { "id": "unlimited", "maxIterations": null },
                    { "id": "capped", "maxIterations": 25 }
                ]
            }
        }"#;
        let cfg = Config::from_json(json).unwrap();
        assert_eq!(cfg.agents.defaults.max_iterations, None);
        let unlimited = cfg
            .agents
            .list
            .iter()
            .find(|a| a.id == "unlimited")
            .unwrap();
        assert_eq!(unlimited.max_iterations, None);
        let capped = cfg.agents.list.iter().find(|a| a.id == "capped").unwrap();
        assert_eq!(capped.max_iterations, Some(25));
    }

    #[test]
    fn prompt_dump_config_defaults_off() {
        let json = r#"{ "gateway": { "auth": { "token": "x" } } }"#;
        let cfg = Config::from_json(json).unwrap();
        assert!(!cfg.prompt_dump.enabled);
    }

    #[test]
    fn prompt_dump_config_explicit() {
        let json = r#"{
            "gateway": { "auth": { "token": "x" } },
            "promptDump": { "enabled": true }
        }"#;
        let cfg = Config::from_json(json).unwrap();
        assert!(cfg.prompt_dump.enabled);
    }

    #[test]
    fn sessions_config_defaults_synthesize() {
        let json = r#"{ "gateway": { "auth": { "token": "x" } } }"#;
        let cfg = Config::from_json(json).unwrap();
        assert_eq!(cfg.sessions.orphan_policy, OrphanPolicy::Synthesize);
    }

    #[test]
    fn sessions_config_explicit_drop_orphan() {
        let json = r#"{
            "gateway": { "auth": { "token": "x" } },
            "sessions": { "orphanPolicy": "dropOrphan" }
        }"#;
        let cfg = Config::from_json(json).unwrap();
        assert_eq!(cfg.sessions.orphan_policy, OrphanPolicy::DropOrphan);
    }

    #[test]
    fn sessions_config_maintenance_defaults() {
        let json = r#"{ "gateway": { "auth": { "token": "x" } } }"#;
        let cfg = Config::from_json(json).unwrap();
        assert_eq!(cfg.sessions.lite_read_buffer_bytes, 65_536);
        assert_eq!(cfg.sessions.ttl_days, 0);
        assert_eq!(cfg.sessions.archive_dir, "~/.legion/archive");
    }

    #[test]
    fn sessions_config_maintenance_explicit() {
        let json = r#"{
            "gateway": { "auth": { "token": "x" } },
            "sessions": {
                "liteReadBufferBytes": 4096,
                "ttlDays": 30,
                "archiveDir": "/tmp/legion-archive"
            }
        }"#;
        let cfg = Config::from_json(json).unwrap();
        assert_eq!(cfg.sessions.lite_read_buffer_bytes, 4096);
        assert_eq!(cfg.sessions.ttl_days, 30);
        assert_eq!(cfg.sessions.archive_dir, "/tmp/legion-archive");
    }

    #[test]
    fn agent_config_parses_prompt_overrides() {
        let json = r#"{
            "gateway": { "auth": { "token": "x" } },
            "agents": {
                "list": [
                    {
                        "id": "researcher",
                        "customSystemPrompt": "You are a meticulous researcher.",
                        "appendSystemPrompt": "Always cite sources.",
                        "outputStyle": "concise",
                        "language": "zh-CN"
                    },
                    { "id": "plain" }
                ]
            }
        }"#;
        let cfg = Config::from_json(json).unwrap();
        let researcher = &cfg.agents.list[0];
        assert_eq!(
            researcher.custom_system_prompt.as_deref(),
            Some("You are a meticulous researcher.")
        );
        assert_eq!(
            researcher.append_system_prompt.as_deref(),
            Some("Always cite sources.")
        );
        assert_eq!(researcher.output_style.as_deref(), Some("concise"));
        assert_eq!(researcher.language.as_deref(), Some("zh-CN"));

        let plain = &cfg.agents.list[1];
        assert!(plain.custom_system_prompt.is_none());
        assert!(plain.append_system_prompt.is_none());
        assert!(plain.output_style.is_none());
        assert!(plain.language.is_none());
    }

    #[test]
    fn agent_config_parses_allow_from() {
        let json = r#"{
            "gateway": { "auth": { "token": "x" } },
            "agents": {
                "list": [
                    { "id": "researcher", "allowFrom": ["main", "critic"] },
                    { "id": "plain" }
                ]
            }
        }"#;
        let cfg = Config::from_json(json).unwrap();
        assert_eq!(
            cfg.agents.list[0].allow_from,
            vec!["main".to_string(), "critic".to_string()]
        );
        // Missing allowFrom defaults to empty (deny-all for agent_to_agent_send).
        assert!(cfg.agents.list[1].allow_from.is_empty());
    }

    #[test]
    fn standing_orders_parse_global_and_per_agent() {
        let json = r#"{
            "gateway": { "auth": { "token": "x" } },
            "agents": {
                "defaults": {
                    "standingOrders": [
                        { "id": "no-prod", "instruction": "Never touch production databases." }
                    ]
                },
                "list": [
                    {
                        "id": "researcher",
                        "standingOrders": [
                            { "id": "cite", "instruction": "Always cite sources.", "enabled": false }
                        ]
                    },
                    { "id": "plain" }
                ]
            }
        }"#;
        let cfg = Config::from_json(json).unwrap();
        assert_eq!(cfg.agents.defaults.standing_orders.len(), 1);
        assert_eq!(cfg.agents.defaults.standing_orders[0].id, "no-prod");
        assert!(cfg.agents.defaults.standing_orders[0].enabled);
        assert_eq!(cfg.agents.list[0].standing_orders.len(), 1);
        assert!(!cfg.agents.list[0].standing_orders[0].enabled);
        // Missing standingOrders defaults to an empty vec on both scopes.
        assert!(cfg.agents.list[1].standing_orders.is_empty());
    }

    #[test]
    fn standing_orders_default_to_empty() {
        let json = r#"{ "gateway": { "auth": { "token": "x" } } }"#;
        let cfg = Config::from_json(json).unwrap();
        assert!(cfg.agents.defaults.standing_orders.is_empty());
        assert!(AgentDefaults::default().standing_orders.is_empty());
    }

    #[test]
    fn standing_order_enabled_defaults_to_true() {
        let order: StandingOrder =
            serde_json::from_str(r#"{ "id": "x", "instruction": "Be careful." }"#).unwrap();
        assert!(order.enabled);
    }

    #[test]
    fn mcp_config_defaults_to_empty_servers() {
        let json = r#"{ "gateway": { "auth": { "token": "x" } } }"#;
        let cfg = Config::from_json(json).unwrap();
        assert!(cfg.mcp.servers.is_empty());
    }

    #[test]
    fn mcp_config_parses_stdio_server() {
        let json = r#"{
            "gateway": { "auth": { "token": "x" } },
            "mcp": {
                "servers": [
                    {
                        "name": "filesystem",
                        "type": "stdio",
                        "command": "npx",
                        "args": ["-y", "@mcp/server-filesystem", "/workspace"],
                        "autoApprove": ["read_file"]
                    }
                ]
            }
        }"#;
        let cfg = Config::from_json(json).unwrap();
        assert_eq!(cfg.mcp.servers.len(), 1);
        let server = &cfg.mcp.servers[0];
        assert_eq!(server.name, "filesystem");
        assert!(server.enabled);
        assert_eq!(server.auto_approve, vec!["read_file"]);
        assert_eq!(server.connect_timeout_ms, 15_000);
        match &server.transport {
            McpTransport::Stdio { command, args, env } => {
                assert_eq!(command, "npx");
                assert_eq!(args.len(), 3);
                assert!(env.is_empty());
            }
            other => panic!("expected stdio, got {:?}", other),
        }
    }

    #[test]
    fn mcp_config_parses_http_server() {
        let json = r#"{
            "gateway": { "auth": { "token": "x" } },
            "mcp": {
                "servers": [
                    {
                        "name": "github",
                        "type": "http",
                        "url": "https://mcp.example.com/rpc",
                        "headers": { "Authorization": "Bearer x" }
                    }
                ]
            }
        }"#;
        let cfg = Config::from_json(json).unwrap();
        assert_eq!(cfg.mcp.servers.len(), 1);
        match &cfg.mcp.servers[0].transport {
            McpTransport::Http { url, headers } => {
                assert_eq!(url, "https://mcp.example.com/rpc");
                assert_eq!(headers.get("Authorization").unwrap(), "Bearer x");
            }
            other => panic!("expected http, got {:?}", other),
        }
    }

    #[test]
    fn mcp_config_parses_sse_server() {
        let json = r#"{
            "gateway": { "auth": { "token": "x" } },
            "mcp": {
                "servers": [
                    {
                        "name": "remote",
                        "type": "sse",
                        "url": "https://mcp.example.com/sse",
                        "headers": { "Authorization": "Bearer s" }
                    }
                ]
            }
        }"#;
        let cfg = Config::from_json(json).unwrap();
        assert_eq!(cfg.mcp.servers.len(), 1);
        match &cfg.mcp.servers[0].transport {
            McpTransport::Sse { url, headers } => {
                assert_eq!(url, "https://mcp.example.com/sse");
                assert_eq!(headers.get("Authorization").unwrap(), "Bearer s");
            }
            other => panic!("expected sse, got {:?}", other),
        }
    }

    #[test]
    fn mcp_config_parses_ws_server() {
        let json = r#"{
            "gateway": { "auth": { "token": "x" } },
            "mcp": {
                "servers": [
                    {
                        "name": "ide",
                        "type": "ws",
                        "url": "ws://127.0.0.1:9000/mcp"
                    }
                ]
            }
        }"#;
        let cfg = Config::from_json(json).unwrap();
        assert_eq!(cfg.mcp.servers.len(), 1);
        match &cfg.mcp.servers[0].transport {
            McpTransport::Ws { url, headers } => {
                assert_eq!(url, "ws://127.0.0.1:9000/mcp");
                assert!(headers.is_empty());
            }
            other => panic!("expected ws, got {:?}", other),
        }
    }

    #[test]
    fn provider_config_parses_retry_and_rate_limit() {
        let json = r#"{
            "gateway": { "auth": { "token": "x" } },
            "models": {
                "providers": {
                    "openai": {
                        "id": "openai",
                        "authProfile": "openai-default",
                        "timeoutSeconds": 60,
                        "retry": {
                            "maxAttempts": 5,
                            "backoff": { "type": "exponential", "baseMs": 250, "maxMs": 4000 }
                        },
                        "rateLimit": { "rpm": 60, "tpm": 90000 }
                    }
                }
            }
        }"#;
        let cfg = Config::from_json(json).unwrap();
        let provider = cfg.models.providers.get("openai").unwrap();
        let retry = provider.retry.as_ref().unwrap();
        assert_eq!(retry.max_attempts, 5);
        assert_eq!(
            retry.backoff,
            BackoffConfig::Exponential {
                base_ms: 250,
                max_ms: 4000
            }
        );
        let rate_limit = provider.rate_limit.as_ref().unwrap();
        assert_eq!(rate_limit.rpm, Some(60));
        assert_eq!(rate_limit.tpm, Some(90000));
    }

    #[test]
    fn retry_config_applies_defaults() {
        let json = r#"{
            "gateway": { "auth": { "token": "x" } },
            "models": {
                "providers": {
                    "openai": {
                        "id": "openai",
                        "authProfile": "openai-default",
                        "retry": { "backoff": { "type": "fixed", "ms": 1000 } }
                    }
                }
            }
        }"#;
        let cfg = Config::from_json(json).unwrap();
        let provider = cfg.models.providers.get("openai").unwrap();
        let retry = provider.retry.as_ref().unwrap();
        assert_eq!(retry.max_attempts, 3);
        assert_eq!(retry.backoff, BackoffConfig::Fixed { ms: 1000 });
        assert!(provider.rate_limit.is_none());

        // Providers without a retry section keep the default backoff values.
        let default_backoff = BackoffConfig::default();
        assert_eq!(
            default_backoff,
            BackoffConfig::Exponential {
                base_ms: 500,
                max_ms: 8000
            }
        );
    }

    #[test]
    fn models_config_parses_costs() {
        let json = r#"{
            "gateway": { "auth": { "token": "x" } },
            "models": {
                "costs": {
                    "openai/gpt-4o": { "inputPer1k": 0.005, "outputPer1k": 0.015 },
                    "gpt-4o-mini": { "inputPer1k": 0.00015, "outputPer1k": 0.0006 }
                }
            }
        }"#;
        let cfg = Config::from_json(json).unwrap();
        assert_eq!(cfg.models.costs.len(), 2);
        let qualified = cfg.models.costs.get("openai/gpt-4o").unwrap();
        assert_eq!(qualified.input_per_1k, 0.005);
        assert_eq!(qualified.output_per_1k, 0.015);
        let bare = cfg.models.costs.get("gpt-4o-mini").unwrap();
        assert_eq!(bare.output_per_1k, 0.0006);
    }
}
