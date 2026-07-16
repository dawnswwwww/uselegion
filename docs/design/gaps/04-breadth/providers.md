# Gap:Provider 仅 OpenAI/Anthropic(缺重试/cache/成本)

| 字段 | 值 |
|---|---|
| 类目 | [04-breadth](./_index.md)(生态广度) |
| 优先级 | P2 |
| 工作量 | M(总体;单 provider S) |
| 前置依赖 | 无(与 [compaction](../03-shallow/compaction.md) 的 prompt cache 协同) |
| 关联 PRD | `agent-harness-prd.md` §6.5 P2/P6(provider 列表/超时) |
| 关联参考 | `docs/openclaw_raw/providers.md`(60+ provider)、`concepts/model-failover.md`、`reference/prompt-caching.md` |
| 状态 | ✅ **已实施**(Phase A+B+C,2026-07-11,见 DEVLOG;Phase D Azure/国内 provider 暂不承诺) |

---

## 1. 现状证据

- **仅 4 种 kind**:`legion-provider/src/router.rs` 的 `from_configs` 支持 `openai`/`generic-openai`/`openrouter`/`anthropic`。
- **缺主流 provider**:无 Google Gemini、无 AWS Bedrock、无 Azure 独立、无 Ollama 原生(Ollama 可经 generic-openai 走 `/v1`,但无模型列表/专用优化)。
- **无单 provider 重试**:`router.rs:97-187` 只有 fallback chain(provider 间切换),**单 provider 内失败不重试**(只 fallback 到下一个)。
- **无速率限制**:无 RPM/TPM 限流,易触发 provider 429。
- **无成本核算**:无 token/费用统计。
- **无请求级 prompt caching**:无 `cache_control`(与 [compaction](../03-shallow/compaction.md) 协同)。
- **`timeout_seconds` 定义未用**:`ProviderConfig.timeout_seconds`(`config.rs`)定义了,但 `ProviderRouter`/`OpenAiProvider` 均未应用——**只有 reqwest client 级超时**,provider 级超时是"声明 vs 事实"又一例。
- **Anthropic 不支持 embed**:`anthropic.rs:273-275` 返回 `EmbeddingNotSupported`,embeddings 只走 OpenAI 兼容端点。

**结论**:provider 路由基础可用(流式 + tool-call 累积 + alias + fallback 是亮点),但缺主流 provider 与运维能力(重试/限流/成本/cache)。

---

## 2. 设计目标(对照七条原则)

- **P1 扩展性**:新 provider = 实现 `Provider` trait + 注册,不改 router 核心。
- **P2 安全**:API key 走 auth profiles / 环境变量(已有);成本核算防失控消费。
- **P3 增量**:新 provider 可选;现有 OpenAI/Anthropic 行为不变。
- **P4 证据**:现状见 §1;借鉴见 §6。
- **P5 可观测**:每次 provider 调用记 model/tokens/latency/cost/retry。
- **P6 失败显式**:429/超时/5xx 分类触发 retry 或 fallback。
- **P7 测试**:retry 退避、rate limit、fallback、各 provider 流式测试。

---

## 3. 架构设计

### 3.1 优先级排序

| 优先级 | Provider | 理由 |
|---|---|---|
| P2-高 | **Google Gemini** | 主流多模态,OpenClaw/市场刚需 |
| P2-高 | **Ollama 原生** | 本地/私有部署,generic-openai 走法缺模型列表 |
| P2-中 | **AWS Bedrock** | 企业合规 |
| P2-中 | **Azure OpenAI(独立)** | 企业 |
| P3 | Qwen/DeepSeek/Moonshot 等 | 国内 provider,可经 generic-openai 兜底 |

### 3.2 router 增强四项运维能力

```
ProviderRouter.chat/embed
   ▼
1. RateLimiter.acquire(rpm/tpm)        // 限流,防 429
   ▼
2. RetryPolicy.attempt(provider)        // 单 provider 内重试(429/5xx/超时)
   ▼
3. provider.chat(带 cache_control)      // prompt cache
   ▼ 失败且 retry 耗尽
4. fallback chain(已有)→ 下一个 provider
   ▼
CostTracker.record(model, tokens)       // 成本核算
```

### 3.3 prompt cache(与 compaction 协同)

Anthropic:请求带 `cache_control` breakpoint(来自 [prompt-management](../03-shallow/prompt-management.md) 的 `cache_breakpoints`)。
OpenAI:自动 cache(prefix 稳定即可)。
收益:长会话重复 system prompt 命中 cache,大幅降本。

### 3.4 成本核算

每次调用按 `ModelCost`(input/output per 1k token)累计;暴露 `/metrics` + `legion costs` CLI。

---

## 4. 接口设计(Rust)

### 4.1 router 增强(`legion-provider/src/router.rs`)

```rust
pub struct ProviderRouter {
    // 现有:providers, fallback chain, alias
    retry: RetryPolicy,
    rate_limiter: RateLimiter,
    cost: CostTracker,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RetryPolicy {
    pub max_attempts: u8,            // 默认 3
    pub backoff: Backoff,
    pub retryable_errors: Vec<ErrorKind>,  // 429/5xx/timeout
}
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backoff { Exponential { base_ms: u64, max_ms: u64 }, Fixed { ms: u64 } }

pub struct RateLimiter { rpm: Option<usize>, tpm: Option<usize> }  // token bucket

pub struct CostTracker {
    per_model: HashMap<ModelRef, ModelCost>,
    accumulated: HashMap<ModelRef, f64>,
}
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCost { pub input_per_1k: f64, pub output_per_1k: f64 }
```

### 4.2 新 provider(以 Gemini 为例)

```rust
// crates/legion-provider/src/gemini.rs
pub struct GeminiProvider { config: ProviderConfig, client: reqwest::Client }

#[async_trait]
impl Provider for GeminiProvider {
    fn id(&self) -> &str { "gemini" }
    fn supported_models(&self) -> &[&str] { &["gemini-2.0-flash", "gemini-2.5-pro", ...] }
    async fn chat(&self, req: ChatRequest) -> Stream<ChatChunk, ProviderError>;
    async fn embed(&self, req: EmbedRequest) -> Result<EmbedResponse, ProviderError>;
}
// Google Generative API: streamGenerateContent + tool deltas
```

### 4.3 timeout 独立配置生效(修复"声明 vs 事实")

```rust
impl ProviderRouter {
    async fn chat(&self, req) -> Stream {
        let timeout = self.timeout_for(&req.model_ref);  // 应用 ProviderConfig.timeout_seconds
        tokio::time::timeout(timeout, provider.chat(req)).await
    }
}
```

### 4.4 配置 schema

```jsonc
{
  "providers": [{
    "id": "gemini", "kind": "gemini",
    "authProfile": "google-default",
    "timeoutSeconds": 60,
    "retry": { "maxAttempts": 3, "backoff": { "type": "exponential", "baseMs": 500, "maxMs": 8000 } }
  }],
  "models": {
    "costs": { "gemini-2.5-pro": { "inputPer1k": 0.00125, "outputPer1k": 0.005 } }
  }
}
```

---

## 5. 集成点

| 位置 | 改动 |
|---|---|
| `legion-provider/src/router.rs:53-187` | `ProviderRouter` 增 `RetryPolicy`/`RateLimiter`/`CostTracker`;chat/embed 包 retry + rate limit + cost record;应用 `timeout_seconds`。 |
| 新增 `legion-provider/src/gemini.rs`/`ollama.rs`/`bedrock.rs` | 各自 `Provider` 实现;router `from_configs` 增 kind 分支。 |
| `legion-provider/src/anthropic.rs` | 接收 `cache_control` breakpoint(来自 prompt-management)。 |
| `legion-gateway/src/observability/prometheus.rs` | 新增 `provider_tokens_total{model}`、`provider_cost_total{model}`、`provider_retries_total{model}`。 |
| `legion-cli` | `legion models list`(列出已知模型 + cost)、`legion costs`(成本报表)。 |

---

## 6. 风险与权衡

### 6.1 retry 与 fallback 的边界
单 provider 内 retry 处理瞬时错误(429/5xx/超时);retry 耗尽才 fallback 到下一个 provider。避免"一遇错误就切 provider"导致首选 provider 永不恢复。

### 6.2 rate limit 的精度
简单 token bucket(RPM/TPM)对单 Gateway 够用;多 Gateway 共享限流需外部 Redis。**Phase A 单机 token bucket**,多机留 P3。

### 6.3 成本核算的准确性
provider 返回的 usage token 是权威来源。**缓解**:流式响应累计 usage delta;未返回 usage 的 provider 用 tiktoken 估算(已有)并标注 `estimated`。

### 6.4 prompt cache 的 provider 差异
Anthropic 显式 `cache_control`;OpenAI 自动;Gemini 有自有 context caching。**取舍**:router 按 provider kind 决定 cache 策略,`cache_breakpoints` 在 Anthropic/Gemini 用,OpenAI 忽略。

### 6.5 因地制宜:Ollama 本地
Ollama 无 API key、无成本、无 rate limit(本地)。`OllamaProvider` 跳过 auth/cost,支持 `/api/tags` 模型列表。generic-openai 走法缺这些,故做原生 provider。

### 6.6 国内 provider 兜底
Qwen/DeepSeek/Moonshot 多为 OpenAI 兼容,可经 `generic-openai` 兜底,不必每个做原生。文档给出"兼容端点配置示例"即可。

---

## 7. 实现路线图

### 阶段 A(Phase C,~0.5 人周):router 运维能力 — ✅ 已落地(2026-07-11,见 DEVLOG)
1. `legion-core`:`ProviderConfig` 加 `retry`(`RetryConfig{maxAttempts 默认 3, backoff: exponential baseMs 500/maxMs 8000 | fixed}`)/`rateLimit`(`{rpm, tpm}`);`ModelsConfig` 加 `costs: HashMap<String, ModelCost{inputPer1k, outputPer1k}>`(全限定 `provider/model` 优先于裸名);全 serde default,旧配置零改动兼容。✅
2. `legion-provider/src/ops.rs`(新建,~620 行):`is_retryable`(Http 429/5xx/timeout/connect + Timeout);`RetryPolicy`(指数退避封顶);`RateLimiter`(per-provider token bucket,rpm/tpm,等待超 30s → `ProviderError::RateLimited`);`CostTracker`(calls/tokens/cost 累计,write-through JSON 持久化 + 启动加载);`track_chat_cost`(unfold 状态机,stream 正常结束时 tiktoken cl100k 估算 output tokens 并 record,`estimated=true`;**提前 drop/中途 Err 不记录**,doc 已注明)。✅
3. `router.rs` 改造:每 candidate = acquire 限流 → retry 循环(retryable 且未耗尽 → warn + backoff 重试;耗尽 → "retry exhausted, falling back";非 retryable → 直接 fallback)→ `tokio::time::timeout` 包裹(**`timeout_seconds` 从此真生效**,超时转 `ProviderError::Timeout` 且判 retryable)→ 成功记 `tracing::info!(provider, model, attempt, latency_ms)` + cost 包装。`from_configs` 新签名接 `costs` + `costs_path`。✅
4. gateway 接线:`costs.json` 落 `~/.legion/agents/<agentId>/costs.json`;CLI `legion costs` 跨 agent 聚合报表(model/calls/tokens/cost/estimated + TOTAL)。✅
5. **验收**:26 个新测试(ops 15 含 wiremock 429/500 分类 + pause 时钟限流;router 7 含 retry 边界/timeout 生效/cost 流;config 3;cli 4);全量 26 suite 全绿;clippy/fmt 干净。
6. 未做:Prometheus 指标(provider_tokens_total 等,后续切片)。

### 阶段 B(Phase C,~0.5 人周):Gemini + Ollama 原生 — ✅ 已落地(2026-07-11,见 DEVLOG)
1. `GeminiProvider`(`legion-provider/src/gemini.rs`,新建,~790 行):`streamGenerateContent?alt=sse`(eventsource-stream 解析;`x-goog-api-key` header;空 key → InvalidAuth);纯函数 `to_gemini_request`(System 合并 → systemInstruction;User/Assistant→user/model;assistant tool_calls→functionCall part;Tool 消息→functionResponse,先建 tool_call_id→name 映射);`tools`→functionDeclarations;finishReason STOP/MAX_TOKENS/SAFETY 映射;embed 走 `batchEmbedContents`;静态目录 gemini-2.5-pro/2.5-flash/2.0-flash(1M context + tool_use)+ default 追加去重。✅
2. `OllamaProvider`(`legion-provider/src/ollama.rs`,新建,~600 行):`/api/chat` NDJSON 流(行缓冲解析 + EOF flush 残余行 + done 终止;tool_calls OpenAI 兼容,id 合成);`/api/embed`;`/api/tags` → inherent async `list_models()`(本地部署跳过 auth,空 key 可构造);`supported_models()` 同步返回 default_model 单项。✅
3. router `from_configs` 注册 `gemini`/`ollama` kind 分支(3 新测试)。✅
4. **验收**:27 个新测试(gemini 14:7 纯函数 + wiremock SSE 流/functionCall/finishReason/embed;ollama 13:解析分支 + wiremock NDJSON/EOF flush/embed/list_models;router 3);legion-provider 79 测试全过;全量 26 suite 全绿;clippy/fmt 干净。**live API 未 E2E(无凭据)。**
5. 偏差记录:生产流式路径用内部 `parse_ollama_line_full`(带 done 标志),规格签名的 `parse_ollama_line` 作 `#[cfg(test)]` 薄包装。

### 阶段 C(Phase C,~0.5 人周):prompt cache + Bedrock — ✅ 已落地(2026-07-11,见 DEVLOG)
1. **prompt cache 接线**:`BuiltPrompt::split_for_prompt_cache(use_prompt_cache)`(纯函数,3 测试)把 system prompt 按 `cache_prefix_len` 切成 `(稳定前缀, cache_breakpoint=true) + 动态后缀`;`agent_loop.rs` 用 `config.compaction.use_prompt_cache`(默认 true)门控;Anthropic `cache_breakpoint` → `cache_control: {type: ephemeral}` 的 provider 侧支持此前已存在(`anthropic.rs:94`,有测试),本次完成 runtime 侧接线。✅
2. `BedrockProvider`(`legion-provider/src/bedrock.rs`,新建,~910 行):**ConverseStream** API;新建 `sigv4.rs`(SigV4 纯函数签名:canonical request/HMAC 密钥链/Howard Hinnant 日期算法,不引 chrono;签名 known-answer 用 Python 独立计算硬编码)与 `eventstream.rs`(手写 IEEE CRC32 const 建表 + 帧解码,半帧 Ok(None)/CRC 校验);`AuthProfile::AwsSigv4{access_key, secret_key, session_token?, region}` 新 variant;纯函数 `to_converse_request`/`converse_event_to_chunk`(toolUse input 跨帧累积,contentBlockStop 产出完整 ToolCall;exception 帧 → StreamAborted);embed 走 Titan invoke(用 `req.model`,空则回退 titan);sha2/hmac 加 workspace 依赖(与 lockfile 对齐)。✅
3. router `from_configs` 注册 `bedrock` kind(非 sigv4 profile → InvalidAuth)。✅
4. **验收**:36+3 个新测试(sigv4 7 含 CRC32 向量 0xCBF43926 与签名 known-answer;eventstream 7;bedrock 15 含 wiremock 流式验证 authorization/x-amz-date header;auth 4;router 2;split_for_prompt_cache 3);legion-provider 115 测试全过;全量 26 suite 全绿;clippy/fmt 干净。**live AWS 未 E2E(无凭据)。**

### 阶段 D(P3):Azure + 国内 provider
- Azure OpenAI 独立 provider;国内 provider 配置示例(走 generic-openai)。暂不承诺原生(不阻塞本 gap 收官)。
- 遗留可选切片(不属验收):Prometheus provider 指标(provider_tokens_total/provider_cost_total/provider_retries_total);Bedrock 非流式 Converse/Guardrails;Gemini context caching。

---

## 8. 验收标准

- [x] Gemini + Ollama 原生 provider 流式 chat + tool-call(wiremock 集成测试 27 个;live API 无凭据未 E2E)。(Phase B)
- [x] 单 provider 内 429/5xx/超时触发 retry(退避测试:指数/fixed backoff + retry 边界,router 7 个新测试)。(Phase A)
- [x] `ProviderConfig.timeoutSeconds` 真生效(`tokio::time::timeout` 包裹每次 attempt,`timeout_seconds_applies_and_retries_then_falls_back`)。(Phase A)
- [x] RateLimiter 限 RPM/TPM(token bucket,等待超 30s → RateLimited;pause 时钟测试)。(Phase A)
- [x] CostTracker 累计 per-model 成本(write-through JSON 持久化);`legion costs` 跨 agent 聚合报表;Prometheus 指标留后续切片。(Phase A)
- [x] Anthropic 请求带 `cache_control` breakpoint(provider 侧早有;runtime 侧 `split_for_prompt_cache` 接线本次落地,3 测试)。(Phase C)
- [x] retry 耗尽才 fallback(边界测试);fallback 链现有行为不破坏(原有 router 测试保留通过)。(Phase A)
- [x] 新 provider = 实现 `Provider` trait + kind 注册,不改 router 核心(Gemini/Ollama/Bedrock 验证)。(Phase B+C)
- [x] Bedrock ConverseStream + SigV4 + event-stream 解码(36 新测试含签名 known-answer 与 CRC32 向量;wiremock 流式)。(Phase C)
- [x] provider 调用记 model/attempt/latency(`tracing::info!`;tokens/cost 由 CostTracker 累计)。(Phase A)
- [x] `AGENTS.md` 更新 provider 章节(声明新 provider + 运维能力)。(Phase A+B+C)

---

*上一篇:[`channels.md`](./channels.md) · 下一个 gap:[`tools-p1p2.md`](./tools-p1p2.md) · 返回总览:[`../00-overview.md`](../00-overview.md)*
