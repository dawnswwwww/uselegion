# Agent Harness 功能规格书（对标 OpenClaw）

> 版本：v0.2（草案）  
> 基于 OpenClaw 文档站全量内容整理，供本项目设计参考。  
> 技术栈：Rust 全栈实现；沙箱基于 CubeSandbox；向量检索基于 ZVec；支持 ACP 外部 harness 接入。

---

## 1. 项目目标与定位

### 1.1 一句话定义

一个**自托管、多通道、可扩展的 AI agent harness**：在单台机器上运行一个 Gateway 进程，把用户日常使用的聊天应用（Telegram、Slack、WhatsApp、iMessage 等）桥接到一个可长期运行、具备记忆、工具和自动化能力的个人 AI 助手。

### 1.2 核心设计原则

1. **Self-hosted first**：数据落在用户机器，配置、记忆、会话 transcript 全部本地文件化。
2. **Multi-channel, one Gateway**：一个 Gateway 同时服务多个聊天通道，统一路由、统一会话。
3. **Agent-native**：为 tool-use、长会话、记忆、自动化而设计，而不是简单套壳 LLM。
4. **Extensible by plugins**：通道、工具、记忆、上下文引擎、agent runtime 均可插件化。
5. **Secure by default**：沙箱、allowlist、配对、审批、operator scopes 分层控制。

### 1.3 用户画像

- 开发者和高级用户想要一个 24/7 可消息触达的个人/团队助手。
- 需要 agent 能读写代码、查资料、定时汇报、记住偏好。
- 不愿意把全部上下文交给第三方 SaaS。

---

## 2. 术语表

| 术语 | 定义 |
|---|---|
| Gateway | 长期运行的中央进程，负责通道连接、会话路由、工具执行、客户端通信。 |
| Channel | 消息通道，如 Telegram、WhatsApp、Slack、WebChat。 |
| Agent | 一个具有独立 workspace、auth、模型配置、会话存储的 AI 实体。 |
| Runtime / Harness | 实际执行 agent loop 的组件，可以是内置 runtime，也可以是插件 harness（如 Codex）。 |
| Session | 一次持续的对话上下文，由来源（DM/群组/cron 等）决定隔离粒度。 |
| Binding | 把 `(channel, account, peer)` 路由到某个 `agentId` 的规则。 |
| Skill | 以 Markdown/JSON 描述的可复用 agent 能力，注入到 prompt 和工具集中。 |
| Plugin | 通过 SDK 注册的扩展包，可扩展通道、工具、记忆、上下文引擎、harness 等。 |
| Node | iOS/Android/macOS/headless 客户端，通过 WS 连接到 Gateway，提供设备能力。 |
| Pairing | 新设备/新用户首次连接时的审批流程。 |
| Heartbeat | Gateway 周期性触发的主会话 turns，用于批量检查通知/邮件/日历。 |
| Compaction | 长会话上下文超限时的自动摘要机制。 |

---

## 3. 总体架构

### 3.1 高层组件图

```
┌─────────────────────────────────────────────────────────────┐
│                         Gateway                              │
│  ┌─────────────┐  ┌──────────────┐  ┌─────────────────────┐ │
│  │  Channel    │  │  Agent       │  │  Plugin             │ │
│  │  Manager    │◄─┤  Runtime     │◄─┤  Registry           │ │
│  │             │  │              │  │                     │ │
│  └──────┬──────┘  └──────┬───────┘  └─────────────────────┘ │
│         │                │                                   │
│  ┌──────▼────────────────▼───────┐  ┌─────────────────────┐ │
│  │      Session / Context        │  │  Tool               │ │
│  │      Manager                  │  │  Registry           │ │
│  └──────┬────────────────┬───────┘  └─────────────────────┘ │
│         │                │                                   │
│  ┌──────▼──────┐  ┌──────▼──────┐  ┌─────────────────────┐  │
│  │  Memory     │  │  Automation │  │  Security /         │  │
│  │  Engine     │  │  Scheduler  │  │  Sandbox            │  │
│  └─────────────┘  └─────────────┘  └─────────────────────┘  │
└─────────────────────────────────────────────────────────────┘
         ▲                            ▲
         │ WS / HTTP                  │ WS / HTTP
    ┌────┴────┐                  ┌────┴────┐
    │ Clients │                  │  Nodes  │
    │ CLI/Web │                  │iOS/Android/macOS
    └─────────┘                  └─────────┘
```

### 3.2 通信协议

- **Gateway ↔ Channel**：各通道官方 SDK / API（Telegram Bot API、WhatsApp Baileys、Slack Bolt 等）。
- **Gateway ↔ Clients/Nodes**：WebSocket，文本帧 JSON，三态：
  - `req` / `res`：请求-响应
  - `event`：服务端推送
- **Gateway ↔ external harness**：Agent Connect Protocol（ACP），支持把单次或多次 agent turn 委托给外部 harness（如 Codex、Claude Code）执行。
- **Gateway ↔ Sandbox**：通过 CubeSandbox 的 E2B 兼容 SDK / HTTP API 创建、执行、回收 MicroVM 沙箱。
- **Memory Engine ↔ Vector Store**：通过 ZVec Rust SDK 进行本地向量存储与混合检索。

### 3.3 目录与状态布局

```
~/.legion/
├── legion.json                 # 主配置
├── workspace/                  # 默认 agent workspace
│   ├── AGENTS.md
│   ├── SOUL.md
│   ├── USER.md
│   ├── TOOLS.md
│   ├── MEMORY.md
│   └── skills/
├── agents/
│   └── <agentId>/
│       ├── agent/              # auth-profiles.json, 模型注册表
│       ├── sessions/           # 会话 store + transcripts
│       └── workspace/          # 该 agent 的 workspace（可选）
├── credentials/                # 通道凭证、OAuth token
├── plugins/                    # 已安装插件
├── hooks/                      # 事件钩子脚本
└── logs/                       # 日志
```

### 3.4 Rust Crate 结构

```
legion/
├── Cargo.toml
├── crates/
│   ├── legion-core/            # 共享类型、配置、错误定义
│   ├── legion-gateway/         # Gateway 进程、WS/HTTP 服务、配对认证
│   ├── legion-provider/        # LLM Provider 抽象与实现
│   ├── legion-runtime/         # 内置 Agent Runtime
│   ├── legion-channel/         # Channel 抽象 + 内置通道
│   ├── legion-memory/          # Memory 抽象 + SQLite/ZVec 实现
│   ├── legion-tools/           # 工具注册、执行、审批、沙箱调用
│   ├── legion-plugin-sdk/      # Plugin trait 与注册 API
│   ├── legion-automation/      # Cron、Heartbeat、Hooks、Tasks
│   ├── legion-acp/             # ACP 协议实现
│   ├── legion-cli/             # 命令行界面
│   └── legion-web/             # Web Dashboard / WebChat 静态资源
└── plugins/
    ├── system-telegram/
    ├── system-webchat/
    ├── system-tools-core/
    ├── system-memory-sqlite-zvec/
    ├── system-provider-router/
    ├── system-automation-cron/
    └── system-acp-bridge/
```

---

## 4. Gateway 核心子系统

### 4.1 生命周期

- **启动**：加载配置 → 加载插件 → 初始化通道 → 打开 WS/HTTP 服务 → 恢复会话 store。
- **运行**：处理入站消息 → 路由到 session → 调用 agent runtime → 发送 outbound 消息。
- **关闭**：优雅关闭通道连接、保存 session、释放插件。
- **重启**：`legion gateway restart`。

### 4.2 WebSocket 协议

#### 握手

客户端首帧必须发送：

```json
{
  "type": "connect",
  "id": "conn-001",
  "params": {
    "auth": { "token": "<device-token>" },
    "deviceId": "macbook-pro-001",
    "platform": "macOS",
    "deviceFamily": "client",
    "role": "client"
  }
}
```

服务端响应：

```json
{
  "type": "res",
  "id": "conn-001",
  "ok": true,
  "payload": {
    "hello": "ok",
    "gatewayId": "gw-abc",
    "features": {
      "methods": ["health", "status", "send", "agent", "system-presence"],
      "events": ["tick", "agent", "presence", "shutdown"]
    },
    "snapshot": { "presence": {}, "health": {} }
  }
}
```

#### 请求帧

```json
{
  "type": "req",
  "id": "req-001",
  "method": "agent",
  "params": {
    "sessionKey": "agent:main:dm:telegram:12345",
    "message": { "role": "user", "content": "帮我查一下 Rust 异步运行时" },
    "idempotencyKey": "idem-001"
  }
}
```

#### 响应帧

```json
{
  "type": "res",
  "id": "req-001",
  "ok": true,
  "payload": { "runId": "run-001", "acceptedAt": "2026-07-08T10:00:00Z" }
}
```

#### 事件帧

```json
{
  "type": "event",
  "event": "agent",
  "payload": {
    "runId": "run-001",
    "stream": "assistant",
    "delta": "Rust 的异步运行时主要有 ..."
  },
  "seq": 42
}
```

### 4.3 路由与配对

- 所有 WS 连接必须携带 `deviceId`。
- 新 device 需要配对批准；本地回环可自动批准。
- 非本地连接（Tailscale/LAN/公网）必须显式配对。
- 认证模式：
  - `token`：共享 token
  - `password`：密码
  - `trusted-proxy`：反向代理传递身份
  - `none`：仅私有 loopback（默认禁用）

### 4.4 配置项（核心）

```json5
{
  "gateway": {
    "bindHost": "127.0.0.1",
    "port": 18789,
    "auth": {
      "mode": "token",           // token | password | trusted-proxy | none
      "token": "${LEGION_GATEWAY_TOKEN}",
      "allowTailscale": false
    },
    "remote": {
      "tailscale": { "enabled": false },
      "sshTunnel": { "enabled": false }
    }
  }
}
```

---

## 5. Channel 子系统

### 5.1 设计目标

- 一个 Gateway 同时运行多个 channel。
- 每个 channel 独立账户、独立连接状态。
- 入站消息统一转换为内部 `InboundMessage` 结构，再路由到 session。

### 5.2 统一消息结构

```typescript
interface InboundMessage {
  channel: string;            // "telegram" | "whatsapp" | "slack" | ...
  accountId: string;          // 多账户时区分
  peer: Peer;                 // direct | group | thread
  sender: Sender;             // id, displayName, username
  messageId: string;
  text?: string;
  media?: Media[];
  replyTo?: string;
  timestamp: string;
  isMentioned?: boolean;
  ambient?: boolean;          // ambient room event
}

interface Peer {
  kind: "direct" | "group" | "thread";
  id: string;
  groupName?: string;
  threadId?: string;
}
```

### 5.3 通道类型

#### 5.3.1 内置通道（MVP 优先）

| 通道 | 协议/库 | 优先级 | 备注 |
|---|---|---|---|
| WebChat | WebSocket + 静态 UI | P0 | 自带控制面板 |
| Telegram | Bot API (grammY 风格) | P0 | 最简单，token 即可 |
| Slack | Bolt Socket Mode | P1 | 企业场景 |
| WhatsApp | Baileys / WhatsApp Web | P1 | 需要 QR 配对 |
| iMessage | imsg JSON-RPC bridge | P2 | 仅 macOS |

#### 5.3.2 插件通道

其余通道通过插件实现，插件注册 `ChannelProvider`：

```typescript
export default function register(api: PluginAPI) {
  api.registerChannel("discord", {
    async start(config, inboundHandler) { ... },
    async stop() { ... },
    async send(channelAccount, outboundMessage) { ... },
    async getCapabilities() {
      return {
        text: true,
        media: ["image", "audio"],
        group: true,
        thread: true,
        reactions: true,
        typing: true
      };
    }
  });
}
```

### 5.4 访问控制

```json5
{
  "channels": {
    "telegram": {
      "dmPolicy": "pairing",      // open | allowlist | pairing
      "allowFrom": ["tg:123456"],
      "groups": {
        "*": { "requireMention": true },
        "-100123456": { "requireMention": false }
      }
    }
  }
}
```

### 5.5 路由到 Session

入站消息经 `Channel Router` 转换为 `sessionKey`：

```
sessionKey = agent:<agentId>:<scope>:<channel>:<accountId>:<peerKind>:<peerId>
```

默认规则：
- DM → 共享主会话 `agent:main:main`（单用户）或按 `session.dmScope` 隔离。
- 群组 → 每个群一个会话。
- Cron/Webhook → 每个触发一个独立会话。

---

## 6. Agent Runtime 子系统

### 6.1 职责

- 把一条用户消息变成一次 agent run。
- 管理工具调用循环、流式输出、上下文组装、compaction。
- 支持多 runtime：内置 runtime + 插件 harness。

### 6.2 Agent Loop 生命周期

```
1. 接收 agent RPC / 入站消息
2. 解析 sessionKey → 定位 session
3. 解析模型、provider、auth profile
4. 加载 skills、bootstrap 文件、记忆
5. 组装 system prompt + context
6. 调用 LLM
7. 解析 assistant 消息 → tool calls / text
8. 执行工具（串行或并行）
9. 把结果写回 context
10. 重复 6-9 直到 run 结束
11. 流式发送最终回复
12. 持久化 transcript
```

### 6.3 Bootstrap 文件

每个 agent workspace 包含以下文件，首次 turn 注入 Project Context：

| 文件 | 用途 |
|---|---|
| `AGENTS.md` | 操作指令、工作流、边界 |
| `SOUL.md` | 人格、语气、emoji |
| `TOOLS.md` | 用户指定的工具使用约定 |
| `USER.md` | 用户画像、偏好称呼 |
| `HEARTBEAT.md` | Heartbeat 专用检查清单 |
| `IDENTITY.md` | Agent 名字/vibe |
| `MEMORY.md` | 长期记忆根文件 |
| `BOOTSTRAP.md` | 首次运行仪式（完成后删除） |

### 6.4 系统提示结构

```
[Base system prompt]
[Skills prompt]
[Bootstrap context]
[Memory additions]
[Per-run overrides]
```

### 6.5 流式事件

```typescript
type AgentEvent =
  | { stream: "lifecycle"; phase: "start" | "end" | "error"; runId: string }
  | { stream: "assistant"; delta: string; runId: string }
  | { stream: "tool"; state: "start" | "update" | "end"; toolCall: ToolCall; result?: any }
  | { stream: "compaction"; summary: string };
```

### 6.6 多 Agent 路由

- `agents.defaults`：默认 agent 配置。
- `agents.list[]`：额外隔离 agent。
- `bindings[]`：把入站消息路由到 agent。

```json5
{
  "agents": {
    "defaults": {
      "workspace": "~/.legion/workspace",
      "model": "anthropic/claude-sonnet-4-6",
      "timeoutSeconds": 172800
    },
    "list": [
      { "id": "work", "workspace": "~/.legion/workspace-work", "model": "openai/gpt-5.5" }
    ]
  },
  "bindings": [
    { "agentId": "main", "match": { "channel": "telegram", "accountId": "default" } },
    { "agentId": "work", "match": { "channel": "slack", "accountId": "work" } }
  ]
}
```

### 6.7 Context Engine（可插拔）

默认 `legacy` engine：
- Ingest：no-op
- Assemble：pass-through
- Compact：内置摘要
- After-turn：no-op

插件 engine 接口：

```typescript
interface ContextEngine {
  info: { id: string; name: string; ownsCompaction: boolean };
  ingest(params: { sessionId; message; isHeartbeat }): Promise<void>;
  assemble(params: { sessionId; messages; tokenBudget; availableTools }): Promise<AssembleResult>;
  compact(params: { sessionId; force }): Promise<CompactResult>;
  afterTurn?(params): Promise<void>;
}
```

---

## 6.5 Provider 模块（LLM 接入层）

### 6.5.1 设计目标

- 统一的 provider 抽象，屏蔽不同 LLM 厂商的 API 差异。
- 支持 35+ 主流 provider（OpenAI、Anthropic、Google、OpenRouter、本地 vLLM/Ollama 等）。
- 支持 provider 级和 model 级配置、auth profile、fallback、failover。
- 模型引用格式：`provider/model`，例如 `anthropic/claude-sonnet-4-6`。

### 6.5.2 核心抽象

```rust
#[async_trait]
pub trait Provider: Send + Sync {
    fn id(&self) -> &str;
    fn supported_models(&self) -> Vec<ModelInfo>;
    async fn chat(&self, req: ChatRequest) -> Result<ChatStream, ProviderError>;
    async fn embed(&self, req: EmbedRequest) -> Result<Vec<Embedding>, ProviderError>;
}

pub struct ProviderConfig {
    pub id: String,
    pub kind: ProviderKind,            // openai | anthropic | google | openrouter | generic-openai | ...
    pub base_url: Option<String>,      // 自定义 endpoint
    pub auth_profile: String,          // 引用 auth-profiles.json 中的 profile
    pub timeout_seconds: Option<u64>,
    pub default_model: Option<String>,
    pub extra_headers: HashMap<String, String>,
    pub extra_params: serde_json::Value,
}
```

### 6.5.3 Auth Profile

每个 agent 独立维护 `auth-profiles.json`：

```json
{
  "profiles": {
    "anthropic-default": { "type": "api_key", "key": "${ANTHROPIC_API_KEY}" },
    "openai-oauth": { "type": "oauth", "client_id": "...", "refresh_token": "..." }
  }
}
```

### 6.5.4 模型解析与 Fallback

1. 解析 `agents.defaults.model` 为 `provider/model`。
2. 若省略 provider，先查 alias，再查唯一匹配 provider。
3. 调用对应 provider；失败时按 `models.fallbacks` 或 `models.providers.<id>.fallback` 切换。
4. 模型空闲超时、provider HTTP 超时独立配置。

### 6.5.5 配置示例

```json5
{
  "models": {
    "providers": {
      "anthropic": {
        "id": "anthropic",
        "kind": "anthropic",
        "authProfile": "anthropic-default",
        "timeoutSeconds": 120
      },
      "openrouter": {
        "id": "openrouter",
        "kind": "openrouter",
        "baseUrl": "https://openrouter.ai/api/v1",
        "authProfile": "openrouter-default"
      },
      "local-ollama": {
        "id": "local-ollama",
        "kind": "generic-openai",
        "baseUrl": "http://localhost:11434/v1",
        "authProfile": "none",
        "timeoutSeconds": 300
      }
    },
    "aliases": {
      "claude": "anthropic/claude-sonnet-4-6",
      "fast": "local-ollama/qwen3:8b"
    },
    "fallbacks": [
      "anthropic/claude-sonnet-4-6",
      "openrouter/moonshotai/kimi-k2"
    ]
  }
}
```

---

## 7. Memory 子系统

### 7.1 设计哲学

- 模型只记住写入磁盘的 Markdown 文件。
- 没有隐藏状态。
- 用户可直接编辑 `MEMORY.md`。

### 7.2 文件层级

| 文件 | 内容 |
|---|---|
| `MEMORY.md` | 精选的长期事实、偏好、决策 |
| `memory/YYYY-MM-DD.md` | 每日详细笔记 |
| `DREAMS.md` | Dream Diary 人类可审查摘要 |

### 7.3 记忆工具

- `memory_search(query)`：语义+关键词混合检索。
- `memory_get(path, lineRange)`：读取指定文件或行范围。

### 7.4 记忆后端

| 后端 | 特点 | 阶段 |
|---|---|---|
| `builtin`（SQLite + ZVec） | SQLite 存结构化数据，ZVec 负责稠密/稀疏向量与全文检索 | MVP |
| `qmd` | 本地 sidecar，BM25、rerank、query expansion | P2 |
| `honcho` | AI-native 跨 session 记忆 | P3 |
| `lancedb` | 向量数据库插件 | P3 |

#### 7.4.1 ZVec 集成

- 使用 ZVec Rust SDK，作为进程内库嵌入 Gateway。
- 每个 agent 的 memory collection 存储在 `~/.legion/agents/<agentId>/memory.zvec/`。
- Schema：
  - `id`: 文档 ID（记忆条目）
  - `content`: 文本内容（FTS 索引）
  - `embedding`: 稠密向量（通过 embedding provider 生成）
  - `sparse_embedding`: 可选稀疏向量
  - `meta`: 来源文件、日期、类型等标量字段
- 检索：MultiQuery 同时走 `VectorQuery` + `FullTextQuery` + 标量过滤，融合排序后返回 Top-K。
- 写入：WAL 保证持久化。

### 7.5 Active Memory

- 可选的阻塞式记忆子代理。
- 在交互式聊天 turn 前注入相关记忆。

### 7.6 Dreaming（可选）

- 后台记忆整合。
- 阶段：light → deep → REM。
- 只有超过阈值的项目才晋升到 `MEMORY.md`。

---

## 8. Tools 子系统

### 8.1 工具注册

工具来源：
1. 内置工具（read/exec/write/web_search 等）
2. 插件注册工具
3. Skill 中声明的工具

工具接口：

```typescript
interface Tool {
  name: string;
  description: string;
  parameters: JSONSchema;
  elevated?: boolean;          // 需要额外授权
  sandbox?: "off" | "optional" | "required";
  handler: (params: any, ctx: ToolContext) => Promise<ToolResult>;
}
```

### 8.2 内置工具清单

#### P0（MVP）

| 工具 | 能力 |
|---|---|
| `read` | 读文件、支持行范围 |
| `write` | 写文件 |
| `edit` | 文本替换编辑 |
| `apply_patch` | 应用 unified diff |
| `exec` | 执行 shell 命令（带审批） |
| `web_search` | Web 搜索 |
| `web_fetch` | 抓取单页内容 |
| `memory_search` | 记忆检索 |
| `memory_get` | 读取记忆文件 |
| `session_status` | 当前会话状态 |
| `sessions_list` | 列出会话 |
| `sessions_history` | 会话历史摘要 |

#### P1/P2

| 工具 | 能力 |
|---|---|
| `browser` | 浏览器自动化 |
| `subagent_spawn` | 启动后台子代理 |
| `canvas` | 操作 Canvas 面板 |
| `nodes_camera` | 调用节点相机 |
| `nodes_screen` | 调用节点屏幕 |
| `nodes_location` | 获取节点位置 |
| `image_generate` | 图像生成 |
| `video_generate` | 视频生成 |
| `tts` | 文本转语音 |
| `agent_to_agent_send` | 跨 agent 消息 |

### 8.3 审批策略

```json5
{
  "tools": {
    "exec": {
      "approval": "required",      // off | prompt | required
      "allowFrom": ["local", "tg:123456"]
    },
    "write": {
      "approval": "prompt",
      "workspaceOnly": true
    },
    "elevated": {
      "enabled": true,
      "allowFrom": ["local"]
    }
  }
}
```

### 8.4 沙箱

- `mode`: `off` | `optional` | `all`
- `scope`: `shared` | `agent` | `session`
- 后端：**CubeSandbox**（基于 RustVMM + KVM 的 MicroVM，<60ms 冷启动，<5MB 内存开销，E2B SDK 兼容）

#### CubeSandbox 集成

```
Gateway ──► CubeSandbox API ──► MicroVM (sandbox)
                 ▲
                 │ 返回 stdout/stderr/exit-code/文件变更
```

- 每个 `exec` 工具调用可选择是否进入 CubeSandbox（由 `tools.<name>.sandbox` 与 agent `sandbox.mode` 共同决定）。
- Sandbox 模板预置常用工具链（git、curl、python、node、rust 等）。
- 工作目录通过 volume 挂载 agent workspace 的只读或读写视图。
- 网络策略：默认禁止出站，或按域名/路径/方法配置 egress 白名单；敏感凭据通过 CubeEgress 注入，对沙箱内不可见。
- 状态管理：支持快照、克隆、回滚，适合长任务或探索性执行。

---

## 9. Automation 子系统

### 9.1 自动化机制矩阵

| 机制 | 触发 | 会话 | 用例 |
|---|---|---|---|
| Cron | 精确时间 / 一次性 / webhook | 独立 | 日报、提醒 |
| Heartbeat | 每 N 分钟 | 主会话 | inbox、日历、通知检查 |
| Hooks | 生命周期事件 | 当前 | `/new`、compaction、gateway 启动 |
| Standing Orders | 每次会话注入 | 全部 | 持久授权/边界 |
| Inferred Commitments | 自然语言推断 | 同 agent+channel | 柔性跟进 |
| Background Tasks | detached work | 独立 | subagent、ACP、cron |
| Task Flow | 多步骤编排 | 独立 | 复杂研究流程 |

### 9.2 Cron 调度器

- 支持 cron 表达式、一次性 `--at`、Gmail PubSub、HTTP webhook。
- 每个执行创建 task record。

```bash
legion cron add "0 9 * * *" --agent main --message "send daily report"
```

### 9.3 Heartbeat

- 默认 30 分钟一次主会话 turn。
- 读取 `HEARTBEAT.md` 作为检查清单。
- 不创建 task record，不刷新 session idle 计时。

### 9.4 Hooks

- 目录：`~/.legion/hooks/` 或插件注册。
- 事件：`agent:bootstrap`、命令事件、生命周期事件。

### 9.5 Background Tasks

- 所有 detached work 进入 task ledger。
- 可审计、可取消、可查看状态。

```bash
legion tasks list
legion tasks show <taskId>
legion tasks audit
```

---

## 10. Plugin 子系统

### 10.1 插件类型

| 类型 | 注册接口 | 示例 |
|---|---|---|
| channel | `registerChannel` | Discord、WhatsApp |
| tool | `registerTool` | 自定义搜索 |
| harness | `registerHarness` | Codex harness |
| context-engine | `registerContextEngine` | 自定义上下文 |
| memory | `registerMemoryBackend` | LanceDB |
| cli-backend | `registerCliBackend` | 本地 CLI 桥接 |
| diagnostics | `registerDiagnosticsProvider` | Prometheus/OTel |

### 10.2 插件包格式

```
my-plugin/
├── manifest.json          # 插件元数据：id、version、kind、entrypoint
├── Cargo.toml             # Rust crate（进程内动态库或独立可执行）
├── src/
│   └── lib.rs             # 注册入口，实现 Plugin trait
├── skills/
├── prompts/
└── themes/
```

### 10.3 系统插件策略

Phase 0 所有功能都通过**系统插件**实现，以验证插件系统的可靠性：

| 系统插件 | 类型 | 说明 |
|---|---|---|
| `system:webchat` | channel | 内置 WebChat UI |
| `system:telegram` | channel | Telegram Bot API |
| `system:slack` | channel | Slack Socket Mode |
| `system:memory-sqlite-zvec` | memory | SQLite + ZVec 记忆后端 |
| `system:provider-router` | provider | 多模型 provider 路由 |
| `system:tools-core` | tool | read/write/exec/web_search 等 |
| `system:context-legacy` | context-engine | 默认上下文引擎 |
| `system:automation-cron` | automation | Cron + Heartbeat |
| `system:acp-bridge` | harness | ACP 外部 harness 桥接 |

这些插件随 Gateway 二进制一起编译/打包，但通过同一套 Plugin API 注册，可被后续第三方插件替换。

### 10.4 插件加载与隔离

- **进程内动态库**：系统插件默认方式，性能最好，共享 Gateway 内存。
- **独立进程**：可选，通过 stdio/HTTP 与 Gateway 通信，适合不可信插件。
- 插件注册在 Gateway 启动时完成，注册后不可热加载（P0 简化）。
- 插件错误隔离：单个插件 panic/异常不得导致 Gateway 崩溃。

### 10.5 市场（后期）

- 类似 ClawHub：发现、安装、发布、安全审计。
- CLI：`legion plugins search/install/update/publish`。

---

## 10.5 ACP（Agent Connect Protocol）桥接

### 10.5.1 目标

让外部 coding harness（如 Codex CLI、Claude Code、Cursor Agent）作为 Legion 的一个 runtime/harness 接入，复用 Legion 的通道、会话、工具、记忆，但把实际 agent turn 的执行交给外部 harness。

### 10.5.2 接入方式

- **stdio 模式**：Gateway 启动外部 harness 子进程，通过 stdin/stdout 交换 ACP 消息。
- **HTTP 模式**：外部 harness 作为独立服务运行，Gateway 通过 HTTP POST / SSE 与其通信。

### 10.5.3 ACP 消息格式（简化版）

```json
{
  "jsonrpc": "2.0",
  "id": "run-001",
  "method": "agents/run",
  "params": {
    "agent": { "id": "main", "workspace": "~/.legion/workspace" },
    "session": { "id": "sess-001", "history": [...] },
    "tools": ["read", "write", "exec", "web_search"],
    "instructions": "...",
    "model": "anthropic/claude-sonnet-4-6"
  }
}
```

响应流：

```json
{
  "jsonrpc": "2.0",
  "id": "run-001",
  "result": {
    "status": "streaming",
    "events": [
      { "type": "text", "delta": "..." },
      { "type": "tool_call", "tool": "exec", "params": {...} },
      { "type": "tool_result", "result": {...} },
      { "type": "done" }
    ]
  }
}
```

### 10.5.4 能力映射

| Legion 概念 | ACP 映射 |
|---|---|
| Session | `session.id` + `history` |
| Tools | `tools` 列表，harness 可调用 Legion 工具 registry |
| Memory | 通过 `instructions` 注入，或 harness 直接调用 `memory_search` |
| Channel reply | harness 完成后 Legion 把最终消息发回通道 |

### 10.5.5 生命周期

1. Gateway 根据 `agentRuntime.id` 选择 harness。
2. 若 harness 为 ACP 类型，启动/连接 harness。
3. 把当前 session 上下文投影为 ACP 请求。
4. 转发 harness 的事件流到客户端。
5. harness 结束时写回 transcript 并发送最终回复。

---

## 11. Nodes & Clients

### 11.1 客户端类型

| 类型 | 角色 | 能力 |
|---|---|---|
| CLI | client | 全部命令、TUI |
| Web Dashboard | client | 聊天、配置、日志、会话 |
| WebChat | client | 聊天 |
| macOS App | client + node | 菜单栏、Canvas、语音 |
| iOS App | node | 相机、语音、Canvas、定位 |
| Android App | node | 相机、语音、Canvas、设备命令 |
| Headless Node | node | 远程设备能力 |

### 11.2 Node 协议

- 连接时声明 `role: "node"`。
- 注册 capabilities 和 commands。
- 支持命令：`canvas.show`, `camera.capture`, `screen.record`, `location.get`, `notify.send`。

---

## 12. CLI & UI

### 12.1 CLI 命令（核心）

```
legion onboard                 # 交互式 onboarding
legion gateway <start|stop|restart|status>
legion dashboard               # 打开 Web UI
legion agent <message>         # 单次 agent turn
legion message <channel> ...   # 发送消息/通道操作
legion config <get|set|patch|validate>
legion channels <list|status|login|logout|add>
legion agents <list|add|delete|bindings>
legion cron <list|add|remove|run>
legion tasks <list|show|cancel|audit>
legion memory <status|search|index>
legion plugins <list|install|update|remove|validate>
legion doctor                  # 健康检查与修复
legion security audit          # 安全检查
legion status                  # 诊断快照
```

### 12.2 Web Dashboard

- 路径：`/dashboard`
- 功能：聊天、配置编辑、会话列表、日志 tail、节点管理。

---

## 13. Security 子系统

### 13.1 分层安全模型

1. **网络层**：loopback 默认，Tailscale/SSH 可选，trusted-proxy 支持。
2. **认证层**：token/password/trusted-proxy/none。
3. **配对层**：设备级批准，非本地必须显式。
4. **通道层**：DM/群组 allowlist、pairing、mention gating。
5. **Agent 层**：per-agent workspace、auth、session。
6. **工具层**：allow/deny、approval、elevated、workspace-only。
7. **沙箱层**：optional/required、shared/agent/session scope。

### 13.2 暴露前检查清单

- 是否启用 allowlist/pairing？
- 是否关闭 `auth.mode: none`？
- 是否限制 `tools.exec` 和 elevated？
- 是否有 TLS/WAF？
- 是否运行 `legion security audit`？

---

## 14. Observability

| 能力 | 阶段 |
|---|---|
| 结构化日志（stdout + 文件） | P0 |
| CLI tail：`legion logs` | P0 |
| Health checks | P0 |
| Diagnostics flags / stuck session detection | P1 |
| Audit records（元数据级） | P1 |
| OpenTelemetry export | P2 |
| Prometheus metrics | P2 |

---

## 15. 数据模型

### 15.1 Session Store

```typescript
interface Session {
  sessionId: string;
  agentId: string;
  scope: "direct" | "group" | "thread" | "cron" | "hook" | "webhook";
  channel?: string;
  accountId?: string;
  peerId?: string;
  sessionStartedAt: string;
  lastInteractionAt: string;
  updatedAt: string;
}
```

### 15.2 Transcript

JSONL 文件，每行一个事件：

```json
{"type": "header", "sessionId": "...", "createdAt": "..."}
{"type": "message", "role": "user", "content": "..."}
{"type": "message", "role": "assistant", "content": "..."}
{"type": "tool_call", "tool": "exec", "params": {...}}
{"type": "tool_result", "result": {...}}
```

### 15.3 Task Record

```typescript
interface Task {
  taskId: string;
  kind: "cron" | "subagent" | "acp" | "cli";
  status: "pending" | "running" | "completed" | "failed" | "timeout";
  agentId: string;
  sessionId?: string;
  runId?: string;
  startedAt: string;
  endedAt?: string;
  error?: string;
}
```

---

## 16. 配置 Schema（MVP 子集）

```json5
{
  // Gateway
  "gateway": {
    "bindHost": "127.0.0.1",
    "port": 18789,
    "auth": {
      "mode": "token",
      "token": "${LEGION_GATEWAY_TOKEN}"
    }
  },

  // Agents
  "agents": {
    "defaults": {
      "workspace": "~/.legion/workspace",
      "model": "anthropic/claude-sonnet-4-6",
      "timeoutSeconds": 172800,
      "blockStreamingDefault": "off",
      "skills": ["core"]
    },
    "list": []
  },

  // Bindings
  "bindings": [
    { "agentId": "main", "match": { "channel": "telegram", "accountId": "default" } }
  ],

  // Channels
  "channels": {
    "telegram": {
      "accounts": {
        "default": { "botToken": "${TELEGRAM_BOT_TOKEN}" }
      },
      "dmPolicy": "pairing",
      "groups": { "*": { "requireMention": true } }
    },
    "webchat": { "enabled": true }
  },

  // Models / Providers
  "models": {
    "providers": {
      "anthropic": {
        "id": "anthropic",
        "kind": "anthropic",
        "authProfile": "anthropic-default",
        "timeoutSeconds": 120
      },
      "openrouter": {
        "id": "openrouter",
        "kind": "openrouter",
        "baseUrl": "https://openrouter.ai/api/v1",
        "authProfile": "openrouter-default"
      },
      "local-ollama": {
        "id": "local-ollama",
        "kind": "generic-openai",
        "baseUrl": "http://localhost:11434/v1",
        "authProfile": "none",
        "timeoutSeconds": 300
      }
    },
    "aliases": {
      "claude": "anthropic/claude-sonnet-4-6",
      "fast": "local-ollama/qwen3:8b"
    },
    "fallbacks": [
      "anthropic/claude-sonnet-4-6",
      "openrouter/moonshotai/kimi-k2"
    ]
  },

  // Memory
  "memory": {
    "backend": "builtin",
    "builtin": {
      "engine": "sqlite-zvec",
      "embeddingProvider": "openai/text-embedding-3-small",
      "collectionPath": "~/.legion/agents/<agentId>/memory.zvec",
      "ftsEnabled": true,
      "hybridEnabled": true
    }
  },

  // Tools
  "tools": {
    "exec": {
      "approval": "required",
      "allowFrom": ["local"],
      "sandbox": "optional"
    },
    "write": { "approval": "prompt", "workspaceOnly": true },
    "elevated": { "enabled": false }
  },

  // Sandbox
  "sandbox": {
    "backend": "cubesandbox",
    "cubesandbox": {
      "apiUrl": "${CUBE_SANDBOX_URL}",
      "apiKey": "${CUBE_SANDBOX_API_KEY}",
      "template": "legion-default",
      "defaultTimeoutSeconds": 300,
      "networkPolicy": {
        "defaultEgress": "deny",
        "allowlist": ["github.com", "crates.io", "pypi.org"]
      }
    }
  },

  // Session
  "session": {
    "dmScope": "main",
    "reset": { "mode": "daily", "atHour": 4 },
    "maintenance": { "mode": "enforce", "pruneAfter": "30d", "maxEntries": 500 }
  },

  // Automation
  "heartbeat": { "enabled": true, "intervalMinutes": 30 },

  // Plugins
  "plugins": {
    "slots": {
      "contextEngine": "system:context-legacy",
      "memory": "system:memory-sqlite-zvec"
    },
    "entries": {
      "system:telegram": { "enabled": true },
      "system:webchat": { "enabled": true },
      "system:tools-core": { "enabled": true },
      "system:provider-router": { "enabled": true },
      "system:memory-sqlite-zvec": { "enabled": true },
      "system:automation-cron": { "enabled": true },
      "system:acp-bridge": { "enabled": false }
    }
  }
}
```

---

## 16.5 TDD 与测试策略

### 16.5.1 测试驱动开发流程

本项目采用 **TDD（Test-Driven Development）**：

1. **Red**：先写一个会失败的测试，明确当前要实现的行为。
2. **Green**：写最少量的代码让测试通过。
3. **Refactor**：重构代码，保持测试通过，提升设计质量。

每个 crate 的核心模块都应先有测试，再实现功能。

### 16.5.2 测试金字塔

```
         ┌─────────┐
         │  E2E    │  少量：完整 Gateway + Telegram/WebChat 端到端
         │  Tests  │
        ┌┴─────────┴┐
        │ Integration│  中等：跨 crate 集成（Provider + Runtime + Memory）
        │   Tests   │
       ┌┴───────────┴┐
       │   Unit Tests │  大量：单个函数/结构体行为
       └───────────────┘
```

### 16.5.3 Crate 测试组织

```
crates/
├── legion-core/
│   └── src/lib.rs              # #[cfg(test)] mod tests
├── legion-gateway/
│   ├── src/
│   │   └── lib.rs
│   └── tests/
│       └── ws_handshake.rs     # 集成测试
├── legion-provider/
│   ├── src/
│   │   └── providers/
│   │       └── anthropic.rs
│   └── tests/
│       └── provider_router.rs
├── legion-runtime/
│   └── tests/
│       └── agent_loop.rs
├── legion-memory/
│   └── tests/
│       └── zvec_backend.rs
└── legion-tools/
    └── tests/
        └── exec_approval.rs
```

### 16.5.4 单元测试原则

- 每个 public function / method 都要有对应测试。
- 测试名采用 `should_<行为>_when_<条件>`。
- 使用 `rstest` 做参数化测试。
- 复杂状态用 builder / fixture 构造。

### 16.5.5 关键模块测试设计

#### 1. `legion-core::config`

```rust
#[test]
fn should_resolve_env_var_in_token() {
    temp_env::with_var("LEGION_GATEWAY_TOKEN", Some("secret123"), || {
        let raw = r#"{ "gateway": { "auth": { "token": "${LEGION_GATEWAY_TOKEN}" } } }"#;
        let cfg: GatewayConfig = serde_json::from_str(raw).unwrap();
        assert_eq!(cfg.gateway.auth.token.resolve(), "secret123");
    });
}

#[test]
fn should_reject_auth_mode_none_with_public_bind() {
    let raw = r#"{ "gateway": { "bindHost": "0.0.0.0", "auth": { "mode": "none" } } }"#;
    let result: Result<GatewayConfig, _> = serde_json::from_str(raw);
    assert!(result.is_err());
}
```

#### 2. `legion-plugin-sdk`

```rust
#[test]
fn should_register_channel_plugin() {
    let mut registry = PluginRegistry::new();
    registry.load(Box::new(FakeChannelPlugin::new()));
    
    assert!(registry.channels().contains_key("fake"));
}

#[tokio::test]
async fn should_deliver_inbound_message_to_gateway() {
    let (tx, mut rx) = mpsc::channel(1);
    let plugin = FakeChannelPlugin::with_sender(tx);
    let inbound = InboundMessage::direct("fake", "u1", "hello");
    
    plugin.inject(inbound.clone()).await;
    
    let received = rx.recv().await.unwrap();
    assert_eq!(received.text, "hello");
}
```

#### 3. `legion-provider`

```rust
#[tokio::test]
async fn should_fallback_to_next_provider_on_429() {
    let primary = FakeProvider::failing(StatusCode::TOO_MANY_REQUESTS);
    let fallback = FakeProvider::responding("fallback reply");
    let router = ProviderRouter::new(vec![primary, fallback]);
    
    let response = router.chat(request()).await.unwrap();
    
    assert_eq!(response.text(), "fallback reply");
}

#[test]
fn should_parse_model_ref_provider_model() {
    let model = ModelRef::from_str("anthropic/claude-sonnet-4-6").unwrap();
    assert_eq!(model.provider, "anthropic");
    assert_eq!(model.model, "claude-sonnet-4-6");
}

#[test]
fn should_parse_openrouter_model_with_slashes() {
    let model = ModelRef::from_str("openrouter/moonshotai/kimi-k2").unwrap();
    assert_eq!(model.provider, "openrouter");
    assert_eq!(model.model, "moonshotai/kimi-k2");
}
```

#### 4. `legion-gateway::websocket`

```rust
#[tokio::test]
async fn should_close_connection_on_missing_connect_frame() {
    let (addr, _server) = start_test_gateway().await;
    let mut ws = connect_ws(addr).await;
    ws.send(Message::Text(r#"{ "type": "req", "method": "health" }"#.into())).await.unwrap();
    
    let msg = ws.next().await.unwrap().unwrap();
    assert!(matches!(msg, Message::Close(_)));
}

#[tokio::test]
async fn should_accept_connection_with_valid_token() {
    let (addr, _server) = start_test_gateway_with_token("valid").await;
    let mut ws = connect_ws(addr).await;
    
    ws.send(connect_frame("valid")).await.unwrap();
    let response = recv_json(&mut ws).await;
    
    assert_eq!(response["type"], "res");
    assert_eq!(response["ok"], true);
}
```

#### 5. `legion-runtime`

```rust
#[tokio::test]
async fn should_call_tool_when_assistant_requests_it() {
    let mut runtime = TestRuntime::new()
        .with_tool(FakeEchoTool)
        .with_model(FakeModel::single_tool_call("echo", json!({ "msg": "hi" })));
    
    let result = runtime.run("say hi").await.unwrap();
    
    assert!(result.tool_calls.contains("echo"));
    assert!(result.reply.contains("hi"));
}
```

#### 6. `legion-memory`

```rust
#[tokio::test]
async fn should_find_relevant_memory_by_vector() {
    let backend = ZVecMemoryBackend::open(temp_dir()).await.unwrap();
    backend.index("doc1", "I love Rust").await.unwrap();
    backend.index("doc2", "Python is nice").await.unwrap();
    
    let results = backend.search("favorite programming language", 2).await.unwrap();
    
    assert_eq!(results[0].id, "doc1");
}
```

### 16.5.6 集成测试策略

| 场景 | 方式 |
|---|---|
| Gateway + Provider + Runtime | 在内存中启动 Gateway，使用 FakeChannel + FakeProvider |
| 跨 crate 配置加载 | 临时配置文件 + `assert_cmd` 调用 CLI |
| Plugin 加载 | 在 `tests/` 目录编译示例插件，动态加载验证 |
| ACP 桥接 | 启动一个 fake ACP harness 子进程，验证 JSON-RPC 交互 |

### 16.5.7 Mocking 方案

- **HTTP Provider**：使用 `wiremock` 或 `mockito` 模拟 LLM API。
- **外部服务**：CubeSandbox、ZVec 在集成测试中启动容器或本地实例；单元测试用 trait mock。
- **时间**：使用 `tokio::time::pause` 或注入 `Clock` trait。
- **文件系统**：单元测试用 `tempfile`；需要隔离时用 `vfs` 或自定义 `Fs` trait。

### 16.5.8 E2E / Live 测试

```bash
cargo test --test e2e --features e2e-tests -- --nocapture
```

- 启动真实 Gateway 进程。
- 使用 Telegram test bot 或 WebChat 发送消息。
- 验证端到端回复。
- 默认 CI 不跑，需显式开启。

### 16.5.9 测试基础设施

- **`rstest`**：参数化测试。
- **`tokio::test`**：异步测试。
- **`tempfile` / `temp_env`**：临时目录和环境变量。
- **`wiremock`**：模拟 HTTP 服务。
- **`serial_test`**：需要串行执行的测试（如端口占用）。
- **`insta`**：快照测试，用于复杂 JSON / 错误消息回归。
- **`criterion`**：性能基准（可选）。

### 16.5.10 CI 检查

```yaml
- cargo fmt --check
- cargo clippy --all-targets --all-features -- -D warnings
- cargo test --all-features
- cargo test --test e2e --features e2e-tests  # 手动触发或夜间跑
```

### 16.5.11 TDD 节奏建议

每个 crate 按此顺序推进：

1. 定义核心类型和 trait（先不实现）。
2. 写该 trait 的最小单元测试。
3. 实现最小代码让测试变绿。
4. 写集成测试描述该模块如何与上下游协作。
5. 补全实现。
6. 重构并确保测试仍绿。

---

## 17. 实现路线图

### Phase 0：MVP（Rust 全栈，1-2 个月）

- [ ] Rust 工程结构 + crate 拆分（gateway、runtime、provider、memory、channel、tools、plugin-sdk、cli）
- [ ] Plugin SDK + 系统插件注册机制
- [ ] Gateway 进程 + WS 协议 + 配对/认证
- [ ] Provider 模块：OpenAI / Anthropic / generic-openai 接入 + auth profile + fallback
- [ ] WebChat 系统插件（channel）
- [ ] Telegram 系统插件（channel）
- [ ] 内置 agent runtime（tool loop + streaming）
- [ ] 核心工具系统插件：read / write / edit / exec / web_search
- [ ] Memory 系统插件：SQLite + ZVec 混合检索
- [ ] Session 管理 + compaction
- [ ] CLI：`gateway`、`agent`、`config`、`channels`、`memory`、`doctor`
- [ ] Web Dashboard（基础聊天 + 配置）

### Phase 1：扩展（2-3 个月）

- [ ] Slack / WhatsApp 系统插件通道
- [ ] 多 agent 路由 + bindings
- [ ] Cron + Heartbeat + Hooks 系统插件
- [ ] Background Tasks + Task Flow
- [ ] CubeSandbox 集成（exec 工具沙箱化）
- [ ] ACP 桥接系统插件（接入 Codex/Claude Code）
- [ ] Audit + diagnostics

### Phase 2：平台化（3-6 个月）

- [ ] iOS/Android nodes + macOS app
- [ ] Plugin 市场（ClawHub-like）
- [ ] 更多模型 provider + auth profile 体系
- [ ] 可插拔 context engine、memory backend
- [ ] OpenTelemetry / Prometheus
- [ ] 多 Gateway / 高可用

---

## 18. 已确认技术决策

| # | 决策项 | 结论 | 说明 |
|---|---|---|---|
| 1 | 实现语言 | **Rust 全栈** | Gateway、Runtime、CLI、Plugins 全部用 Rust 实现。 |
| 2 | LLM 接入层 | **独立 Provider 模块** | 支持灵活配置各家 provider（OpenAI、Anthropic、OpenRouter、本地 generic-openai 等），支持 auth profile、alias、fallback。 |
| 3 | 插件机制 | **系统插件先行** | 先设计好 Plugin SDK，所有功能以系统插件形式实现，验证扩展性；系统插件随 Gateway 一起编译，可选进程内动态库或独立进程。 |
| 4 | 沙箱后端 | **CubeSandbox** | 基于 RustVMM + KVM 的 MicroVM，E2B SDK 兼容，<60ms 冷启动，<5MB 内存开销。 |
| 5 | 向量检索 | **SQLite + ZVec** | SQLite 存结构化记忆数据，ZVec（Rust 进程内向量库）负责稠密/稀疏向量与全文检索。 |
| 6 | 外部 harness | **支持 ACP** | 通过 Agent Connect Protocol 桥接 Codex、Claude Code 等外部 harness。 |

### 18.1 关键外部依赖

- **CubeSandbox**：<https://github.com/TencentCloud/CubeSandbox>，<https://cubesandbox.com/>
- **ZVec**：<https://github.com/alibaba/zvec>，<https://zvec.org/en/>
- **ACP（Agent Connect Protocol）**：参考 Anthropic / OpenAI 相关规范实现。

---

## 19. 参考资料

- 已下载的 OpenClaw 完整文档：`docs/openclaw_raw/`
- 索引：`docs/openclaw_raw/docs_map.md`
- 抓取摘要：`docs/openclaw_raw/_fetch_summary.json`

---

*本 PRD 为草案，下一步可针对任一模块展开详细设计或开始编码。*
