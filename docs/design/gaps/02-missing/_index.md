# 02 · 完全缺失的子系统

> 类目定义:Claude Code 已有**深度实现**,而 legion **零实现或仅有配置占位**的子系统。这是三类差距中**严重度最高**的一类——不是"做得浅",而是"根本没有"。

本类目含 4 个 gap:

| Gap | 优先级 | 工作量 | 一句话 | 文档 |
|---|---|---|---|---|
| **plugin-facade** | P0 | L | 7 类插件仅 channel 可用,4 个 system plugin 是 stub;插件市场是内存态 | [`plugin-facade.md`](./plugin-facade.md) |
| **skills** | P0 | M | 仅 `config.rs:156` 占位 `Vec<String>`,零加载/注册/执行逻辑 | [`skills.md`](./skills.md) |
| **mcp** | P1 | L | 全仓库零 MCP 客户端,无法接入任何外部工具生态 | [`mcp.md`](./mcp.md) |
| **multi-agent** | P1 | L | `TaskKind::Subagent` 枚举存在但无构造/委派执行路径 | [`multi-agent.md`](./multi-agent.md) |

## 共同特征

这 4 个 gap 有三个共性,决定了它们的设计应**协同推进**:

1. **都依赖插件化地基**。skills/mcp/multi-agent 落地后,理想形态都应是"可插拔组件"而非硬编码。因此 [`plugin-facade`](./plugin-facade.md) 是本类目的前置,应在其他三个之前或并行推进。

2. **都是"扩展性杠杆"**。每补齐一个,agent 的能力边界就外扩一档:skills → 领域知识;MCP → 外部工具生态;multi-agent → 并行与委派;plugin-facade → 让前三个 + 未来所有扩展低成本接入。

3. **借鉴 Claude Code 时都要"取舍"**。Claude Code 是 TS 进程内实现,legion 是 Rust trait 抽象 + 长驻进程。每个文档的"风险与权衡"章节会明确标注借鉴点与因地制宜点。

## 推荐推进顺序

```
plugin-facade (P0, 地基)
     ├──→ skills (P0, 依赖 plugin 注册)
     ├──→ mcp (P1, 依赖 plugin 注册)
     └──→ (未来 channels/tools 也走此路)
multi-agent (P1, 独立,可与 skills 并行)
```

## 阅读建议

- 若做架构决策,先读 [`plugin-facade.md`](./plugin-facade.md) §接口设计,它定义了后续所有插件的 trait 契约。
- 若评估短期 ROI,[`skills.md`](./skills.md) 工作量最小(`M`)且杠杆最大,适合作为 plugin-facade 之后的首个落地。
- [`mcp.md`](./mcp.md) 和 [`multi-agent.md`](./multi-agent.md) 工作量均为 `L`,建议排在 Phase B。

---

*返回总览:[`../00-overview.md`](../00-overview.md)*
