# Gap:MCP 集成(完全缺失)

| 字段 | 值 |
|---|---|
| 类目 | [02-missing](./_index.md)(完全缺失) |
| 优先级 | P1(高杠杆扩展) |
| 工作量 | L(≥3 人周) |
| 前置依赖 | [plugin-facade](./plugin-facade.md)(MCP server 可作为插件来源) |
| 关联 PRD | `agent-harness-prd.md` §10 PL1(tool 类型插件) |
| 关联分析 | `claude-code-analysis/analysis/04d-mcp-implementation.md` |

---

## 1. 现状证据

- **全仓库零命中**:`grep -ri "mcp\|model context protocol" crates/` 无任何结果。无 MCP client、无 MCP server 注册、无 `mcp__*` 工具命名空间、无外部进程桥接。
- 工具体系完全是内置 `Tool` trait + `CoreToolRegistry`(`legion-tools/src/registry.rs`),无法接入任何第三方 MCP server。

**结论**:legion 无法使用 MCP 生态(数据库工具、API 封装、浏览器控制、文件系统等)。这是限制工具面广度的硬约束。

---

## 2. 设计目标(对照七条原则)

- **P1 扩展性**:用户在配置声明一个 MCP server(本地 stdio 或远程 http),其工具自动出现在 agent 工具池。
- **P2 安全**:MCP 工具默认 `approval: required`(像 `exec` 一样);命名空间 `mcp__<server>__<tool>` 让 approval 可按 server 批量控制;远程 MCP 认证失败不雪崩。
- **P3 增量**:无 MCP server 配置时,工具池与当前一致。
- **P4 证据**:借鉴点均指向 `04d-mcp-implementation.md` 具体章节。
- **P5 可观测**:每个 MCP server 的 connect/list/call/error 产生 `tracing` 事件。
- **P6 失败显式**:传输错误、认证失败、超时、工具不存在分类报错;认证失败走雪崩缓存短路。
- **P7 测试**:每种传输有 mock 测试;认证缓存、描述截断、并发限流有单元测试。

---

## 3. 架构设计

### 3.1 四种传输(全盘借鉴 Claude Code)

| 传输 | 用途 | Rust 实现 |
|---|---|---|
| **stdio** | 本地 MCP server 子进程(JSON-RPC over stdin/stdout) | `tokio::process` + newline-delimited JSON |
| **sse** | 远程 SSE server(+ OAuth) | `reqwest` + `eventsource_stream` |
| **http** (streamable-http) | 远程 HTTP/streamable | `reqwest` |
| **ws** | WebSocket(IDE 集成) | `tokio-tungstenite` |

### 3.2 工具适配层
MCP server 暴露的工具,通过 `McpToolAdapter` 包装为 legion `Tool` trait,命名 `mcp__<server>__<tool>`,合并进 `CoreToolRegistry`(内建同名优先,防覆盖)。

### 3.3 工程化防线(借鉴 Claude Code 04d)

```
McpManager
   ├── 认证雪崩缓存(AuthCache, 15min TTL):某 server 认证失败 → 短路后续请求
   ├── 描述长度截断(MAX_MCP_DESCRIPTION_LENGTH = 2048):防 OpenAPI 衍生超长文档塞满上下文
   ├── 并发连接控制:本地 3 / 远程 20 并发上限,pMap 限流防启动卡死
   ├── session 过期重连:HTTP 404 / JSON-RPC -32001 → 清缓存重连
   └── 超时控制:wrap_fetch_with_timeout(用 tokio::time::timeout)
```

---

## 4. 接口设计(Rust)

### 4.1 新 crate `legion-mcp`

```rust
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase", tag = "type")]
pub enum McpTransport {
    Stdio { command: String, #[serde(default)] args: Vec<String>, #[serde(default)] env: HashMap<String, String> },
    Sse  { url: String, #[serde(default)] headers: HashMap<String, String> },
    Http { url: String, #[serde(default)] headers: HashMap<String, String> },
    Ws   { url: String, #[serde(default)] headers: HashMap<String, String> },
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerConfig {
    pub name: String,                         // server id → mcp__<name>__<tool>
    pub transport: McpTransport,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub auto_approve: Vec<String>,            // 该 server 哪些 tool 自动放行(默认空)
    #[serde(default)]
    pub connect_timeout_ms: u64,              // 默认 15000
}
fn default_true() -> bool { true }

pub const MAX_MCP_DESCRIPTION_LENGTH: usize = 2048;

#[derive(Debug, Clone)]
pub struct McpToolDesc {
    pub name: String,
    pub description: String,                  // 已截断至 MAX_MCP_DESCRIPTION_LENGTH
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct McpToolResult {
    pub content: serde_json::Value,
    pub is_error: bool,
}

#[async_trait]
pub trait McpClient: Send + Sync {
    fn server_name(&self) -> &str;
    async fn connect(&self) -> Result<(), McpError>;
    async fn list_tools(&self) -> Result<Vec<McpToolDesc>, McpError>;
    async fn call_tool(&self, name: &str, args: serde_json::Value) -> Result<McpToolResult, McpError>;
    async fn close(&self) -> Result<(), McpError>;
}

pub struct McpManager {
    clients: HashMap<String, Arc<dyn McpClient>>,
    auth_cache: AuthCache,                    // server_name → (是否失败, 截止时间)
    concurrency: ConcurrencyLimits,           // 本地3/远程20
}
impl McpManager {
    pub async fn load(&mut self, configs: &[McpServerConfig]) -> LoadReport;
    pub fn tools(&self) -> Vec<Arc<dyn crate::Tool>>;   // 包装为 legion Tool
    pub async fn shutdown_all(&self);
}
```

### 4.2 工具适配(`mcp__<server>__<tool>`)

```rust
// 把 MCP 工具适配为 legion Tool trait
pub struct McpToolAdapter {
    server: String,
    desc: McpToolDesc,
    client: Arc<dyn McpClient>,
}

#[async_trait]
impl Tool for McpToolAdapter {
    fn name(&self) -> &str {
        // 持久化命名,供 approval gate 按 server 粒度匹配
        // 实际用 once_cell 缓存格式化串
    }
    fn description(&self) -> &str { &self.desc.description }
    fn input_schema(&self) -> &serde_json::Value { &self.desc.input_schema }
    fn is_concurrency_safe(&self) -> bool { false }   // 默认串行,安全
    async fn call(&self, input: serde_json::Value) -> Result<ToolOutput, ToolError> {
        let r = self.client.call_tool(&self.desc.name, input).await?;
        if r.is_error { return Err(ToolError::McpToolError(r.content)); }
        Ok(ToolOutput::Json(r.content))
    }
}
```

### 4.3 配置 schema(`legion-core`)

```jsonc
// legion.json
{
  "mcp": {
    "servers": [
      {
        "name": "filesystem",
        "type": "stdio",
        "command": "npx",
        "args": ["-y", "@modelcontextprotocol/server-filesystem", "/workspace"],
        "autoApprove": ["read_file"]
      },
      {
        "name": "github",
        "type": "http",
        "url": "https://mcp.github.example/sse",
        "headers": { "Authorization": "Bearer ${GITHUB_TOKEN}" }
      }
    ]
  }
}
```

---

## 5. 集成点

| 位置 | 改动 |
|---|---|
| 新 crate `legion-mcp` | `McpClient`/`McpManager`/`McpToolAdapter`/四种传输实现/认证缓存/描述截断/并发限流。 |
| `legion-tools/src/registry.rs` | `CoreToolRegistry` 合并 `McpManager.tools()`;同名优先级:内建 > MCP(防覆盖)。 |
| `legion-gateway/src/gateway.rs` | 启动时 `McpManager::load(config.mcp.servers)`,关闭时 `shutdown_all()`。 |
| `legion-core/src/config.rs` | 新增 `mcp: McpConfig`(默认空 servers)。 |
| `legion-tools/src/policy.rs` | `mcp__*` 工具默认 `Approval::Required`;支持 `autoApprove` 白名单放行(仍可被全局 deny 覆盖)。 |

---

## 6. 风险与权衡

### 6.1 认证雪崩(全盘借鉴)
Claude Code 04d §7/§10:某 MCP server 认证失败后,若不缓存,每个请求都会重试,15 分钟内可产生海量失败请求。legion 复刻 15min `AuthCache`,失败一次短路同 server 后续请求。

### 6.2 描述截断(全盘借鉴)
04d §5:OpenAPI 衍生的 MCP 工具描述可达 15-60KB,会塞爆上下文。legion 强制截断到 2048 字符。

### 6.3 命名空间即安全边界
`mcp__<server>__<tool>` 让 approval policy 可用 glob 批量控制(如 `mcp__filesystem__*` 全部 required,`mcp__readonly__*` 放行)。这是借鉴之外、legion 特化的安全设计点。

### 6.4 并发限流(借鉴 + Rust 化)
Claude Code 用 `pMap`;legion 用 `tokio::sync::Semaphore`(本地 3 permit / 远程 20 permit),避免启动时所有 server 并发连接卡死。

### 6.5 因地制宜:超时实现
Claude Code 04d §4 用 `setTimeout` 而非 `AbortSignal.timeout()` 规避 Bun 内存泄漏。legion 无此问题,直接用 `tokio::time::timeout`。

### 6.6 stdio 子进程安全
本地 stdio MCP server 是任意可执行文件,**默认 `approval: required`**,且建议在 sandbox 内运行(与 sandbox-isolation 协同,Phase B)。

---

## 7. 实现路线图

### 阶段 A(Phase A,✅ 已完成):stdio + http 传输 + 工具适配
1. ✅ 新建 `legion-mcp` crate:`McpClient` trait + stdio 实现(`tokio::process`)+ http 实现(`reqwest`)。
2. ✅ `McpToolAdapter` 包装为 legion `Tool`(在 `legion-tools/src/mcp.rs`),合并进 registry(内建优先)。
3. ✅ 认证雪崩缓存 + 描述截断 + 并发 Semaphore(本地3/远程20)+ 超时控制。
4. ✅ `McpConfig` 配置;`mcp__*` 默认 required,`autoApprove` 降级为 `Off`。
5. ✅ **验收**:配置一个 http MCP server,其工具出现在 agent 工具池,调用经 approval(wiremock 测试覆盖);stdio 路径经 build_client 构造验证。

### 阶段 B(Phase B,✅ 已完成):sse + ws 传输 + 重连
1. ✅ sse 传输(reqwest + eventsource_stream,标准双通道 GET/POST)+ OAuth step-up 检测(401 + `WWW-Authenticate` 归类并告警,真实刷新留后续)。
2. ✅ ws 传输(tokio-tungstenite,后台 reader 按 id 路由)。
3. ✅ session 过期重连(HTTP 404 / JSON-RPC `-32001` → `connect()` 后重试一次,经 `McpClient::call_tool_resilient` 默认方法)。
4. ✅ **验收**:ws / sse 各有一次 round-trip mock 测试(ws 用 `tokio-tungstenite` 自建 server,sse 用 raw-tokio 双通道 server);session 过期重连与 OAuth 检测经 wiremock 覆盖。

### 阶段 C(Phase C,✅ 已完成):可观测 + CLI
1. ✅ `legion mcp list/tools/reload` CLI(本地,经 `McpManager` 直连配置)。
2. ✅ Prometheus 指标:`mcp_calls_total{server,tool}`、`mcp_errors_total{server,tool}`,经 `McpMetrics` hook 在 `McpToolAdapter::call` 计数,gateway 注入。
3. ✅ **验收**:指标暴露在 `/metrics`(带 `server` / `tool` 标签);observability 标签测试 + gateway bridge 测试覆盖。

---

## 8. 验收标准

- [x] 配置 stdio/http MCP server 后,其工具以 `mcp__<server>__<tool>` 命名出现在工具池。
- [x] MCP 工具调用默认 `approval: required`,`autoApprove` 白名单生效(全局 deny 仍由 pipeline 覆盖)。
- [x] 认证失败的 server 在 15 分钟内被短路(不重复请求)。
- [x] 工具描述超 2048 字符被截断。
- [x] 四种传输(stdio/http/sse/ws)均有 mock 测试:ws 用 `tokio-tungstenite` 自建 server,sse 用 raw-tokio 双通道 server,http 用 wiremock;stdio e2e 待补。
- [x] session 过期(HTTP 404 / JSON-RPC `-32001`)自动重连一次重试(`McpClient::call_tool_resilient`,wiremock 覆盖)。
- [x] 远程 401 + `WWW-Authenticate` 触发 OAuth step-up 检测(warn + 错误分类;真实刷新留后续)。
- [x] 内建工具与 MCP 工具同名时,内建优先(registry 测试)。
- [x] Prometheus 指标 `mcp_calls_total{server,tool}` / `mcp_errors_total{server,tool}` 暴露在 `/metrics`,每次 `tools/call` 计数(adapter + gateway bridge 测试);连接/重连/OAuth 经 `tracing` 记录。
- [x] `legion mcp list/tools/reload` 本地 CLI(list 配置、tools 已发现工具、reload 连通性检查),经 `McpManager` 直连配置,无需 gateway。
- [x] 无 MCP 配置时,工具池与当前一致(回归)。
- [x] `AGENTS.md` 新增 MCP 章节(本批次补充)。

---

*上一篇:[`skills.md`](./skills.md) · 下一个 gap:[`multi-agent.md`](./multi-agent.md) · 返回总览:[`../00-overview.md`](../00-overview.md)*
