# 03 · 内核浅化

> 类目定义:legion **有基础实现**且可工作,但缺 Claude Code 沉淀的**工程化深度防线**。不是"没有",而是"防线缺失"——在正常路径上工作,但在失败/长会话/越界场景下暴露问题。

本类目含 7 个 gap:

| Gap | 优先级 | 工作量 | 一句话 | 文档 |
|---|---|---|---|---|
| **approval-loop** | **P0** | M | `Approval::Prompt≡Required`,主循环 `can_use_tool=None`,无人工确认回路 | [`approval-loop.md`](./approval-loop.md) |
| **sandbox-isolation** | **P0** | L | local backend 零隔离(直接 `sh -c`),Cube 缺网络白名单/快照/复用 | [`sandbox-isolation.md`](./sandbox-isolation.md) |
| **memory-layers** | P1 | L | 单层 + 全手动,无自动决策/分层/召回选择器 | [`memory-layers.md`](./memory-layers.md) |
| **compaction** | P1 | M | 无熔断/状态复灌/PTL 防御/prompt cache | [`compaction.md`](./compaction.md) |
| **prompt-management** | P1 | M | 固定拼装,无分层 section/override 优先级/dump 可观测 | [`prompt-management.md`](./prompt-management.md) |
| **session-resume** | P2 | M | 无 compact boundary 恢复/orphan tool_result 修复 | [`session-resume.md`](./session-resume.md) |
| **session-loop** | P2 | S-M | ✅ Phase A 已实施:local TUI `/loop` 可用,与 global gateway loop 分层 | [`session-loop.md`](./session-loop.md) |

## 共同特征

这 6 个 gap 都属于"**内核已有,防线缺失**",有三个共性:

1. **都涉及失败模式处理**。Claude Code 的工程深度几乎全花在"出错时怎么办"——approval 被拒、compact 失败、session 损坏、context 爆炸。legion 当前只覆盖 happy path。

2. **都借鉴 Claude Code 的具体防线**(非架构重写)。多数是"在现有实现上补一段防护逻辑",工作量集中在 `M`,性价比高。

3. **相互有依赖**。`compaction` 的 boundary 标记是 `session-resume` 的前提;`memory-layers` 的 session memory 可作 `compaction` 断点;`prompt-management` 是 `skills` 注入的载体。

## 推荐推进顺序

```
approval-loop (P0, 安全关键,最先)
sandbox-isolation (P0, 安全关键,可与 approval 并行)
   ↓
memory-layers (P1) ──┐
compaction (P1) ─────┼── session-resume (P2, 依赖 compaction boundary)
prompt-management (P1, 是 skills 注入载体)
session-loop (P2, 依赖 automation-advanced cron)
```

两个 P0(approval-loop、sandbox-isolation)是 Phase A 的安全地基,优先于一切。

## 阅读建议

- **若关注安全**:先读 [`approval-loop.md`](./approval-loop.md) 与 [`sandbox-isolation.md`](./sandbox-isolation.md),它们解决"默认配置下工具执行/命令执行的失控风险"。
- **若关注长会话稳定性**:读 [`compaction.md`](./compaction.md) 与 [`memory-layers.md`](./memory-layers.md),它们解决"长对话崩溃/能力丢失"。
- **若关注可恢复性**:读 [`session-resume.md`](./session-resume.md),它解决"compact 后 resume 丢上下文"。
- **若关注本地 TUI 的循环任务体验**:读 [`session-loop.md`](./session-loop.md),它解决"local 模式 `/loop` 必须开 gateway"的 UX 痛点。

---

*返回总览:[`../00-overview.md`](../00-overview.md)*
