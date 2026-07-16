# 04 · 生态广度不足

> 类目定义:对照 OpenClaw(`docs/openclaw_raw/`)与 legion 自身 PRD 的**连接面、工具、自动化数量差距**。不是"做错了",而是"覆盖窄"——核心链路能跑,但接入的渠道、模型、工具、自动化种类少。

本类目含 4 个 gap(均为 P2,可与 Phase B 并行):

| Gap | 优先级 | 工作量 | 一句话 | 文档 |
|---|---|---|---|---|
| **channels** | P2 | L(单 channel S-M) | 仅 Telegram+WebChat;PRD Slack/WhatsApp/iMessage 缺;访问控制配置有但运行时不执行 | [`channels.md`](./channels.md) |
| **providers** | P2 | M(单 provider S) | 仅 OpenAI/Anthropic;缺 Gemini/Bedrock/Ollama 原生 + 重试/cache/成本 | [`providers.md`](./providers.md) |
| **tools-p1p2** | P2 | L(单 tool S-M) | browser/canvas/多媒体生成/agent_to_agent/session_* 全缺 | [`tools-p1p2.md`](./tools-p1p2.md) |
| **automation-advanced** | P2 | M | Standing Orders/Commitments/Task Flow DAG/webhook 缺(基础 cron/hooks 已是亮点) | [`automation-advanced.md`](./automation-advanced.md) |

## 共同特征

1. **都依赖 plugin-facade 降本**。channels/providers/tools 的每个新增,理想形态是"实现一个 trait + 一份配置",而非改核心代码。故本类目推进前,[`02-missing/plugin-facade`](../02-missing/plugin-facade.md) 应已就绪。

2. **策略是"扩展模板 + 优先级排序",而非全抄**。OpenClaw 的广度(27 channel / 60 provider)legion 不应也无法全盘复制。每个 gap 文档给出"优先实现哪几个 + 为什么 + 模板"。

3. **数量型增长,边际价值递减**。前 2-3 个新增(如 Slack、Gemini、image_generate)ROI 高;后续是长尾。本类目建议"做精前几个 + 留扩展口"。

## 推荐推进顺序

```
plugin-facade 就绪后:
   ├── channels   (优先 Slack;配套访问控制引擎)
   ├── providers  (优先 Gemini/Ollama 原生;配套重试/cache/成本)
   ├── tools-p1p2 (优先 agent_to_agent[配合 multi-agent]、image_generate、browser)
   └── automation-advanced (Standing Orders、Task Flow DAG)
```

四个可并行,无相互依赖(各自只依赖 plugin-facade 或现有子系统)。

## 阅读建议

- **若要新增聊天渠道**:读 [`channels.md`](./channels.md),重点是"ChannelProvider 实现模板 + 访问控制引擎"。
- **若要接入新模型商**:读 [`providers.md`](./providers.md),重点是"Provider trait 实现 + router 重试/cache/成本"。
- **若要扩展 agent 工具**:读 [`tools-p1p2.md`](./tools-p1p2.md),重点是"优先级 + Tool trait 模板"。
- **若要做自动化编排**:读 [`automation-advanced.md`](./automation-advanced.md),重点是"复用现有 task_runner 做 DAG"。

---

*返回总览:[`../00-overview.md`](../00-overview.md)*
