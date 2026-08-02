# Gap:MCP 集成(已实施;2026-07-31 协议升级)

| 字段 | 值 |
|---|---|
| 类目 | [02-missing](./_index.md)(完全缺失) |
| 状态 | ✅ 已实施(初版 Phase A–C 2026-07-10;协议升级 2026-07-31) |
| 优先级 | P1(高杠杆扩展) |
| 关联 PRD | `agent-harness-prd.md` §10 PL1(tool 类型插件) |
| 关联分析 | `claude-code-analysis/analysis/04d-mcp-implementation.md` |

---

## 1. 现状(2026-07-31 协议升级后)

`legion-mcp` 是完整的 MCP client:四种传输、协议版本协商、列表分页、resources/prompts 内省,以及 CLI/TUI 管理面。以源码为准。

- **传输**:stdio(本地子进程)、http(Streamable HTTP)、sse(2024-11-05 双通道,上游已废弃、12 个月 offramp,仍支持)、ws。
- **协议协商**(`version.rs`):`SUPPORTED_VERSIONS` 新→旧排列,`initialize` 按链回退;server 返回的版本宽松采纳,server capabilities 存储;配置 `protocolVersion` pin 时跳过回退链。
- **2026-07-28 stateless 模式**:不发 `notifications/initialized`;`_meta` 携带 `io.modelcontextprotocol/clientInfo` + client 能力;HTTP 请求带 `MCP-Protocol-Version` / `Mcp-Method` / `Mcp-Name` 头;`initialize` 返回 -32601 时回退 `server/discover`。
- **Streamable HTTP**(http 传输):`Accept: application/json, text/event-stream`,SSE 帧解析;2025-xx 版本捕获并重发 `Mcp-Session-Id`(2026-07-28 不用);≥2025-03-26 在 initialize 后带 `MCP-Protocol-Version` 头。
- **分页**:tools/list、resources/list、prompts/list 支持 `cursor`/`nextCursor`,100 页上限。
- **工具面**:`McpToolDesc` 携带 `annotations` + `outputSchema`;tool 结果携带 `structuredContent`(存在时附加进 legion 工具输出)。
- **内省 API**(UI/自查用,**不是** agent 工具):client 与 `McpManager` 的 `list_resources`/`read_resource`/`list_prompts`/`get_prompt`,按 server capabilities 门控(能力未知时宽松放行);`McpManager::server_status()` 快照(协议版本、capabilities、工具数)。
- **安全/工程防线**(初版沿用):`mcp__<server>__<tool>` 命名空间适配进 `CoreToolRegistry`(内建同名优先),默认 `Approval::Required` + `autoApprove` 白名单;认证雪崩缓存(15min);描述截断 2048;并发限流(本地 3 / 远程 20);session 过期(404 / -32001)重连一次;Prometheus 指标 `mcp_calls_total{server,tool}` / `mcp_errors_total{server,tool}`。

### 协议版本矩阵

| 版本 | 支持 | 要点 |
|---|---|---|
| 2026-07-28 | ✅ | stateless core:无 `notifications/initialized`;`_meta` clientInfo + 能力;`Mcp-Method`/`Mcp-Name`/`MCP-Protocol-Version` 头;-32601 → `server/discover` 回退 |
| 2025-11-25 | ✅ | 协商链中间版本 |
| 2025-06-18 | ✅ | 协商链中间版本 |
| 2025-03-26 | ✅ | Streamable HTTP profile:SSE 帧响应、`Mcp-Session-Id` 会话、initialize 后版本头 |
| 2024-11-05 | ✅ | legacy sse 双通道(GET 流 + POST endpoint);上游 deprecated(12 个月 offramp) |

---

## 2. 剩余差距

- **OAuth flow**:401 + `WWW-Authenticate` 已检测并报告 `oauth step-up required`,但无 token 获取 / PKCE / CIMD。
- **MRTR / elicitation / sampling**:2026-07-28 的 Multi Round-Trip Requests 未实现(roots/sampling/logging 上游已废弃,不计划)。
- **`notifications/tools/list_changed` 实时刷新**:server→client 通知仍被丢弃,工具在连接时一次性列举。
- **列表缓存提示**:2026-07-28 的 `ttlMs`/`cacheScope` 未解析。
- **Tasks extension、subscriptions/listen**:未实现。
- **配置热加载**:MCP 配置修改须重启 gateway/host 生效,无运行中热加载。

---

## 3. 配置与操作入口

- **配置字段**(`crates/legion-core/src/config.rs`,`McpServerConfig`,camelCase):新增 `protocolVersion`(`Option<String>`,pin 协议版本跳过协商链)、`toolTimeoutMs`(默认 60000,所有传输的 per-request `tools/call` 超时);既有 `enabled`/`autoApprove`/`connectTimeoutMs` 不变。
- **CLI**(`crates/legion-cli/src/mcp.rs`):
  - `legion mcp add`(claude-code 对齐 flag:`--transport stdio|http|sse|ws`、`--env KEY=VALUE`、`--header "Name: value"`、`--auto-approve`、`--protocol-version`、`--connect-timeout-ms`、`--tool-timeout-ms`,stdio 命令放 `--` 之后)
  - `legion mcp remove <name>` / `legion mcp get <name>`
  - `legion mcp status`(live 探测:协商版本 + 工具数,不可达标 `✗`)
  - `legion mcp list`(纯配置视图:enabled 状态 + 传输摘要);`tools`/`reload` 不变
- **TUI slash 命令**(`crates/legion-cli/src/mcp_cmd.rs`):`/mcp`(列出配置)、`/mcp status [name]`、`/mcp tools|resources|prompts <name>`、`/mcp enable|disable <name>`、`/mcp add <name> stdio <cmd...>` / `/mcp add <name> <http|sse|ws> <url>`、`/mcp remove <name>`。配置编辑持久化到 legion.json(备份 + schema 校验),下次 gateway/host 重启生效;异步查询结果经 local-notice 通道送回聊天。

---

*上一篇:[`skills.md`](./skills.md) · 下一个 gap:[`multi-agent.md`](./multi-agent.md) · 返回总览:[`../00-overview.md`](../00-overview.md)*
