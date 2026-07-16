# Gap:P1/P2 工具全缺(browser/多媒体生成/agent_to_agent/session_*)

| 字段 | 值 |
|---|---|
| 类目 | [04-breadth](./_index.md)(生态广度) |
| 优先级 | P2 |
| 工作量 | L(总体;单工具 S-M) |
| 前置依赖 | [plugin-facade](../02-missing/plugin-facade.md);[multi-agent](../02-missing/multi-agent.md)(agent_to_agent);[providers](./providers.md)(image_generate) |
| 关联 PRD | `agent-harness-prd.md` §8 T2/T3(session_* / browser / canvas / nodes_* / 多媒体) |
| 关联参考 | `docs/openclaw_raw/tools.md`(59 tool)、`tools/browser-control.md`、`tools/image-generation.md` |
| 状态 | ✅ 已实施(Phase A+B+C,2026-07-11,见 DEVLOG;Phase D canvas/video/nodes_* 暂不承诺) |

---

## 1. 现状证据

- **仅 10 个基础工具**(`legion-tools/src/registry.rs:52-98`):read/write/edit/apply_patch/exec/web_fetch/web_search/memory_search/memory_get/memory_index。
- **PRD T2 缺**:无 `session_status`/`sessions_list`/`sessions_history`(agent 无法自查/跨会话检索)。
- **PRD T3 全缺**:无 `browser`、`subagent_spawn`(在 [multi-agent](../02-missing/multi-agent.md) 处理)、`canvas`、`nodes_camera`/`nodes_screen`/`nodes_location`、`image_generate`、`video_generate`、`tts`、`agent_to_agent_send`。
- **现有工具是亮点**:apply_patch 手写 diff applier、exec 走 sandbox、web_search 真实 DuckDuckGo 解析、memory 三件套完整——说明 Tool trait + Policy 模式成熟,新增工具成本低。

**结论**:工具基座扎实,但工具种类窄。对照 OpenClaw 59 tool,策略是"优先高价值几个 + 留扩展口"。

---

## 2. 设计目标(对照七条原则)

- **P1 扩展性**:每个新工具 = 实现 `Tool` trait + 注册(Policy/Approval/sandbox),不改核心。
- **P2 安全**:危险工具(image_generate 费用、browser 网络暴露、exec-like)默认 `Approval::Required` 或 sandbox;`agent_to_agent_send` 权限收敛。
- **P3 增量**:新工具可选启用;现有 10 工具行为不变。
- **P4 证据**:现状见 §1;借鉴见 §6。
- **P5 可观测**:每次工具调用记 tool/duration/approval。
- **P6 失败显式**:provider 失败、browser 超时、media 生成失败分类处理。
- **P7 测试**:每个新工具有 happy + 失败 + Policy 测试。

---

## 3. 架构设计

### 3.1 工具优先级矩阵

| 优先级 | 工具 | 价值 | 依赖 |
|---|---|---|---|
| P2-高 | **session_status / sessions_list / sessions_history** | agent 自查会话,实现简单(读 session_store) | session_store |
| P2-高 | **agent_to_agent_send** | 配合 multi-agent 跨 agent 通信 | multi-agent |
| P2-高 | **image_generate** | 多模态生成刚需 | image provider |
| P2-中 | **browser** | agent 上网/抓取,但实现重 | Playwright/CDP |
| P2-中 | **tts** | 语音 channel 场景 | audio provider |
| P3 | canvas / video_generate / nodes_* | 特定场景,长尾 | 各自后端 |

### 3.2 统一实现模板(复用现有 Tool trait)

```rust
// 每个新工具遵循此模板
pub struct XTool { config: XConfig }

#[async_trait]
impl Tool for XTool {
    fn name(&self) -> &str { "x" }
    fn description(&self) -> &str { "..." }
    fn input_schema(&self) -> &serde_json::Value { &SCHEMA }
    fn is_concurrency_safe(&self) -> bool { /* read-only=true */ }
    fn is_read_only(&self) -> bool { ... }
    async fn call(&self, input: Value) -> Result<ToolOutput, ToolError> { ... }
}

// 注册时声明 Policy/Approval/sandbox(沿用现有)
registry.register(Arc::new(XTool { ... }), Policy {
    approval: Approval::Required,  // 危险工具默认
    workspace_only: false,
    ...
});
```

---

## 4. 接口设计(各工具要点)

### 4.1 session_* 工具(`legion-tools`,读 `session_store`)

```rust
// session_status:当前会话元信息(agent/scope/token 用量/compaction 状态)
// sessions_list:列某 agent 下所有 peer session(lite_read,见 session-resume)
// sessions_history:读指定 session 的 transcript(支持行范围/compact boundary)
pub struct SessionsHistoryTool { store: Arc<dyn TranscriptLoader> }
```
**Policy**:`Approval::Off`(只读,workspace 内),`is_read_only: true`,`is_concurrency_safe: true`。

### 4.2 agent_to_agent_send(配合 multi-agent)

```rust
pub struct AgentToAgentSendTool { router: Arc<AgentRouter> }
// input: { to: agent_id, message: string }
// 行为:向目标 agent 投递一条 inbound,触发其 turn;返回投递确认(异步,非阻塞)
// 权限收敛:仅允许向配置允许的 agent_id 集合发送(防滥用)
```
**Policy**:`Approval::Prompt`(跨 agent 动作需确认),`allow_from` 限定可通信 agent。

### 4.3 image_generate(依赖 image provider)

```rust
pub struct ImageGenerateTool { router: Arc<ProviderRouter> }
// input: { prompt, model: "dall-e-3"|"flux"|"...", size, n }
// 走 provider 抽象(OpenAI image / 独立 image provider);返回 image url/path
```
**Policy**:`Approval::Required`(费用 + 内容安全);`workspace_only: false`(产物存 workspace)。
**风险**:内容审核(provider 侧 + 本地关键词预检)。

### 4.4 browser(重,Phase C)

```rust
pub struct BrowserTool { backend: BrowserBackend }  // Playwright/CDP/ego-browser
// input: { action: navigate|click|read|screenshot|extract, url, selector }
// 在 sandbox 内启动 browser;网络受 sandbox allowlist 约束(配合 sandbox-isolation)
```
**Policy**:`Approval::Required`(网络暴露 + 资源);默认在 sandbox 内。
**取舍**:browser 后端重,Phase C 评估 Playwright(wasm) vs CDP vs ego-browser;先做 read-only navigate+extract。

### 4.5 tts(语音 channel 配套)

```rust
pub struct TtsTool { router: Arc<ProviderRouter> }  // audio provider
// input: { text, voice, model }
// 返回 audio bytes,经 channel media_send 发送(配合 channels capabilities)
```
**Policy**:`Approval::Off`(低风险),但受 voice channel capabilities 门控。

---

## 5. 集成点

| 位置 | 改动 |
|---|---|
| `legion-tools/src/registry.rs` | 注册新工具;或走 plugin-facade 独立插件 crate。 |
| `legion-tools/src/tools.rs` | 新增 session_*/image_generate/tts/browser 工具实现(或拆 tools/ 子模块)。 |
| `legion-gateway/src/session_store.rs` | session_* 工具复用 `TranscriptLoader`(见 [session-resume](../03-shallow/session-resume.md))。 |
| `legion-runtime` | agent_to_agent_send 接入 agent router(配合 multi-agent)。 |
| `legion-provider` | image/audio provider 抽象(image_generate/tts 复用)。 |

---

## 6. 风险与权衡

### 6.1 image_generate 的费用与内容安全
AI 生成有费用 + 滥用风险。**缓解**:默认 `Approval::Required`;本地关键词预检(防违规 prompt);per-agent 费用上限(配合 [providers](./providers.md) CostTracker)。

### 6.2 browser 的实现成本与安全
browser 是最重的工具(启动 headless、网络暴露、资源)。**取舍**:
- Phase C 先做轻量版(navigate + extract + screenshot),read-only;
- 在 sandbox 内运行,网络受 allowlist 约束(配合 [sandbox-isolation](../03-shallow/sandbox-isolation.md));
- 后端优先 CDP(Chrome DevTools Protocol)或 ego-browser,Playwright(wasm)留评估。

### 6.3 agent_to_agent_send 的滥用
跨 agent 通信可能被滥用(循环触发、越权)。**缓解**:`allow_from` 限定可通信 agent 集合;复用 [channels](./channels.md) 的 bot-loop-protection 防 agent 循环。

### 6.4 nodes_* 工具的长尾
`nodes_camera`/`nodes_screen`/`nodes_location` 依赖原生客户端(iOS/Android/macOS),而 legion 当前无原生客户端(见 [PRD N3](../../agent-harness-prd.md))。**列为 P3**,随原生客户端推进。

### 6.5 因地制宜:工具 vs skill 的边界
某些"工具"可能更适合做 skill(纯提示注入,见 [skills](../02-missing/skills.md))。**原则**:有副作用/外部调用 → 工具;纯领域知识/流程指引 → skill。

### 6.6 session_* 的权限边界
agent 读自己的 session 无风险,但跨 agent/peer 读 session 是越权。**缓解**:`sessions_history` 限定为当前 agent_id 内;跨 agent 走 `agent_to_agent_send` 显式授权。

---

## 7. 实现路线图

### 阶段 A(Phase C,~0.5 人周):session_* 工具 — ✅ 已落地(2026-07-11,见 DEVLOG)
1. `session_status`/`sessions_list`/`sessions_history` 落地于 `legion-gateway/src/session_tools.rs`(新建,~670 行,13 测试)——放在 gateway 而非 legion-tools,因为 `SessionStore` 在 gateway(tools→gateway 无依赖);`CoreToolRegistry` 加 `pub fn register`(重名 warn 不覆盖,与 MCP 冲突处理一致),gateway 在 `AgentRuntime::new` 前注册三工具(共享同一 `Arc<SessionStore>`)。✅
2. `SessionStore` 加 `stats(session_key) -> Option<SessionStats>`(复用私有 `load_entries`,最小改动)与 `transcript_messages`。✅
3. **权限边界(§6.6)**:三工具均无 agent 参数;`session_status` 比对 key 内 agent_id 与 `ctx.agent_id`,跨 agent 拒绝;`sessions_history` 的 peerId 走 `[A-Za-z0-9._-]` 白名单防路径穿越,缺省取当前 session peer;content 截断 2000 字符。✅
4. **验收**:15 个新测试(统计/跨 agent 拒绝/非法 key/排序+limit/切片/穿越四形态/跨 agent 不泄漏/register 语义),全量 26 suite 全绿,clippy/fmt 干净。

### 阶段 B(Phase C,~0.5 人周):agent_to_agent_send + image_generate — ✅ 已落地(2026-07-11,见 DEVLOG)
1. `agent_to_agent_send`:`legion-runtime` 新 `messenger.rs`(`AgentMessenger` trait fire-and-forget + `MessengerError`),`ToolContext` 加 `messenger` 字段(照 spawner 模式经 AgentRuntime→ContextEngine→tool_pipeline 传递,全部构造点更新);`AgentConfig` 加 `allowFrom`(空 = 拒绝所有,安全默认);gateway `RuntimeAgentMessenger`(`check_allowed` 纯函数 → session key `agent:{to}:a2a:{from}` → `tokio::spawn` 后台 turn,`interactive=false`,非阻塞返回确认);工具在 `legion-tools`(self-send 拒绝、未接线报错),`Policy` 默认 `Approval::Prompt`。✅
2. `image_generate`:`legion-provider` 加 `ImageRequest/ImageResponse/GeneratedImage` + `ProviderError::ImageNotSupported`;`Provider` trait 加默认方法 `generate_image`(零破坏);router 加 fallback 循环(无 retry/计费,image 端点无 token 语义,已注明);OpenAI `POST /images/generations`(wiremock 测试);工具在 `legion-gateway/src/image_tool.rs`(持 `Arc<ProviderRouter>`,同 session_tools 模式):关键词预检(小黑名单启发式粗筛)+ b64 落盘 `<workspace>/generated/` + url 透传,`Policy` 默认 `Approval::Required`。✅
3. **验收**:15 个新测试(config 1 + 工具 4 + wiremock 2 + check_allowed 3 + image_tool 5);全量 26 suite 全绿;clippy/fmt 干净。**live API 未 E2E。**
4. 偏差记录:a2a turn 的 model_ref 照 route_inbound 的 MVP 默认硬编码;image 默认 model 用完整形式 `openai/dall-e-3`(裸名过不了 model-ref 解析);image 未接 CostTracker(无 token 语义)。

### 阶段 C(Phase C,~1 人周):browser(轻量) + tts — ✅ 已落地(2026-07-11,见 DEVLOG)
1. `BrowserTool`(`legion-tools/src/browser.rs`,新建,~620 行,12 测试):**CDP 轻量后端**——配置经 `tools.browser.cdpUrl`/`timeoutSeconds`(走 `ToolConfig.extra`,未改 schema);每次调用一次性 WS 连接(不池化):`Target.createTarget` → `attachToTarget{flatten}` → `Page.enable` → `Page.navigate`(sleep 500ms 简化加载等待,事件等待留后续)→ `Runtime.evaluate`(read 截 8000 字符,selector JSON 转义)/`Page.captureScreenshot`(png 落 `<workspace>/generated/`);结束尽力 closeTarget。`build_cdp_command`/`parse_cdp_response` 纯函数单测;navigate/read 标 read-only;默认 `Approval::Required`。**设计变更(相对 §4.4/§6.2)**:browser 跑在用户自供的 CDP 端点(外部 headless Chrome),网络隔离由该浏览器部署方负责,**不在 legion sandbox 内**——sandbox 内嵌浏览器后端留后续。**CDP WS 全链路本环境无 server 未实测**,仅编解码/参数分支有单测。✅
2. `TtsTool`(`legion-gateway/src/tts_tool.rs`,新建,~250 行,5 测试):`Provider` trait 加默认方法 `synthesize_speech`(`SpeechNotSupported`,照 generate_image 模式);router fallback 循环;OpenAI `POST /audio/speech`(voice 默认 alloy、format 默认 mp3,2 wiremock 测试);产物落 `<workspace>/generated/tts-<millis>.<format>`(format 字符白名单防注入)返路径;默认 `Approval::Off`。**voice channel capabilities 门控/channel 投递未做**(后续切片)。✅
3. **验收**:21 个新测试(browser 12 + registry 2 + tts 5 + openai 2);全量 26 suite 全绿;clippy/fmt 干净。**live CDP/TTS API 未 E2E。**

### 阶段 D(P3):canvas / video_generate / nodes_*
- 随各自后端/原生客户端推进。暂不承诺(不阻塞本 gap 收官)。
- 遗留可选切片(不属验收):browser 事件等待(替代 sleep 500ms)/sandbox 内嵌后端/会话池化;tts 的 voice channel capabilities 门控与 channel 投递;image/tts 费用核算;a2a bot-loop 防护接入。

---

## 8. 验收标准

- [x] `session_status`/`sessions_list`/`sessions_history` 可用,限定当前 agent_id(权限边界测试:跨 agent 拒绝 + peerId 白名单防穿越 + 跨 agent transcript 不泄漏,15 测试)。(Phase A)
- [x] `agent_to_agent_send` 仅向 `allowFrom` 集合投递(空=全拒;`check_allowed` 3 测试 + 工具 4 测试)。(Phase B)
- [x] `image_generate` 默认 `Approval::Required` + 关键词预检;b64 落盘 workspace(费用记录:router 注明 image 端点无 token 语义,未接 CostTracker)。(Phase B)
- [x] `browser` 轻量 CDP 后端落地(设计变更:跑外部 CDP 端点而非 sandbox 内嵌;网络隔离由浏览器部署方负责,WS 全链路未 E2E;navigate/read 标 read-only,默认 Required,12 测试)。(Phase C)
- [x] `tts` 产物落 `<workspace>/generated/` 返路径(voice channel capabilities 门控留后续切片;默认 Off,router fallback,wiremock 2 测试)。(Phase C)
- [x] 每个新工具有 happy + 失败 + Policy 测试(session_* 15、a2a/image 15、browser/tts 21)。
- [x] 新工具 = 实现 `Tool` trait + 注册,不改核心(plugin-facade 验证:全部经 `CoreToolRegistry::register`/registry 钩子接入)。
- [x] 危险工具默认 Required/Prompt(image/browser=Required,a2a=Prompt)。
- [x] 工具调用记 tool/duration/approval(`tracing`;browser/tts 已补 `tracing::info!`)。
- [x] `AGENTS.md` 更新工具清单(§5 Tools 段加 browser/tts)。

---

*上一篇:[`providers.md`](./providers.md) · 下一个 gap:[`automation-advanced.md`](./automation-advanced.md) · 返回总览:[`../00-overview.md`](../00-overview.md)*
