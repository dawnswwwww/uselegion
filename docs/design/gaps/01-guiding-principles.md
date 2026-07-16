# 指导原则:补齐 legion 差距的设计宪法

> 本文件是 `docs/design/gaps/` 下所有差距方案文档的**共同前提**。它定义:
> (1) 我们从 Claude Code 源码中借鉴什么、舍弃什么;
> (2) 所有方案必须遵守的 Rust 工程基线与横切原则;
> (3) 优先级标尺(P0/P1/P2)的判定标准;
> (4) 文档写作与交叉引用约定。
>
> **阅读顺序**:任何具体 gap 文档之前,先读本文件与 [`00-overview.md`](./00-overview.md)。

---

## 0. 为什么要有这份"宪法"

`claude-code-analysis/` 是对 Claude Code 泄露源码的静态分析,Claude Code 是一个**经过大规模生产验证的本地 coding agent**,在 Memory、Skills、Tool Call、Sandbox、Context/Compaction、Multi-Agent、Session 等子系统上沉淀了大量工程教训。legion 作为同领域(自主 agent)但不同形态(Rust 多通道 gateway)的项目,可以直接汲取这些教训,**避免重走弯路**。

但 Claude Code 用的是 TypeScript + 进程内闭包 + 单机 CLI/TUI 形态;legion 用的是 Rust + trait 抽象 + 长驻 Gateway + 多通道。直接照搬语义会水土不服。因此本文件的第一职责,是**划清"借鉴"与"因地制宜"的边界**。

本文件的第二职责,是让 14 个分散的 gap 方案文档**保持一致**——同样的优先级标尺、同样的错误处理风格、同样的命名约定、同样的测试要求——避免每个文档各说各话,最终拼不出一个连贯的产品。

---

## 1. 设计哲学总纲

一句话:**先做对内核的不变量,再扩展连接面;每个能力都要可插拔、可观测、可回退。**

展开为五条:

1. **内核不变量优先于功能数量**。Agent 系统的核心风险是"失控循环"——agent 在错误的状态下执行危险工具、在越界的上下文里产生幻觉、在压缩后丢失关键能力。Claude Code 的工程深度几乎全部花在**防止这些失败**上(approval 回路、compact 熔断、sandbox 逃逸防护、session orphan 修复)。legion 当前的内核链路可工作,但缺这些"防线",**防线的优先级高于新增 channel/provider**。

2. **扩展性是杠杆**。一个设计良好的插件/Skill/MCP 接口,能让生态以我们写代码赶不上的速度增长。因此**插件化的架构债(plugin-facade)越早还,后续每个能力(新增 channel/provider/tool)的边际成本越低**。

3. **失败必须显式**。Rust 的 `Result` + `thiserror` 天然鼓励显式错误。Agent 系统的失败尤其要分类:哪些是可重试的瞬时错误(provider 超时)、哪些是需人工介入的策略错误(approval 拒绝)、哪些是不可恢复的协议错误(ACP JSON-RPC 格式错)。每个 gap 方案都要给出失败分类。

4. **可观测性是一等公民,不是事后补丁**。无法观测的 agent 是黑箱。Claude Code 的 `dump-prompts`、`/context` token 统计、审计记录、diagnostics flags 都是为了让 agent 行为可解释。legion 当前只有 `tracing` + Prometheus,差距方案都要预留观测钩子。

5. **增量演进,绝不破坏现有 MVP 链路**。legion 的 Gateway→Channel→Runtime→Provider→Tools→Memory→Compaction 主链路已端到端打通且有测试。所有 gap 方案必须**以增量方式接入**——新增 trait 提供默认实现、新增配置提供默认值、新增行为在关闭时等价于现状。禁止"先拆了重建"。

---

## 2. Rust 工程基线(对齐 legion 现有约定)

以下约定来自 `AGENTS.md` 与现有 crate 源码,**所有方案中的代码示例必须遵守**。审阅者据此判断方案是否"像 legion 的代码"。

### 2.1 语言与工具链
- Rust **edition 2024**,workspace MSRV **1.86**,resolver 3。
- 工作区依赖统一在根 `Cargo.toml` 声明,crate 内用 `workspace = true`,不重复钉版本。
- 异步运行时 `tokio`(full features)。

### 2.2 抽象与动态分发
- 跨 crate 的可插拔组件一律用 **`#[async_trait]` + `Arc<dyn Trait>`**:
  - 已有先例:`Plugin`、`Provider`、`Tool`、`SandboxBackend`、`Harness`、`ChannelProvider`、`MemoryBackend`。
  - 新增的 `SkillRegistry`、`McpClient`、`ApprovalGate`、`ContextEngine`、`SubagentSpawner` 等遵循同一模式。
- 用 `Arc<dyn Trait>` 而非泛型,除非有强性能证据(因为插件/通道在运行时按配置装配,需要动态分发)。
- 共享可变状态用 `Arc<RwLock<T>>` 或 `Arc<Mutex<T>>`;读多写少优先 `RwLock`。

### 2.3 错误处理
- 错误枚举用 **`thiserror`**,每个 crate 一个顶层 `Error`(`legion_core::Error`、`legion_tools::ToolError` 等)。
- 生产代码**禁止 `unwrap`/`expect`**(测试代码可用),用 `?` + 类型化错误。
- 错误要可分类:`thiserror` 的 `#[error("...")]` 配合 `#[from]` 转换。
- 跨边界错误(如序列化、IO)用 `#[from]` 自动转换,避免手动 map。

### 2.4 序列化与配置
- 配置与 API 类型用 **`serde` + `camelCase`**(`#[serde(rename_all = "camelCase")]`)。
- 配置文件支持 `.json` 与 `.json5`,环境变量替换 `${VAR}` / `${VAR:default}`(由 `legion-core` 解析)。
- 新增配置项**必须有默认值**,且默认值等价于"当前行为"或"安全关闭"。

### 2.5 日志与追踪
- 用 **`tracing`**(不用 `println!`/`log`),结构化字段:`tracing::info!(agent_id = %id, event = "skill_loaded", path = %p)`。
- span 层级:`agent_loop` span 内嵌 `tool` span、`provider` span。
- 敏感字段(API key、用户消息全文)默认 **不进日志**,或用 redaction 包装。

### 2.6 测试
- 单元测试与代码同文件,`#[cfg(test)] mod tests`。
- 参数化用 `rstest`,文件系统隔离用 `tempfile`/`temp-env`,HTTP mock 用 `wiremock`,异步用 `tokio-test`。
- 集成测试放 `crates/<crate>/tests/`。
- **每个新 trait/能力必须有:happy path 测试 + 至少一个失败模式测试 + 一个默认行为(关闭时等价现状)测试**。

### 2.7 命名
- 类型 `UpperCamelCase`,函数/变量 `snake_case`,常量 `SCREAMING_SNAKE_CASE`。
- 异步函数不加 `_async` 后缀(由 `async` 关键字表达)。
- Builder 类型用 `XxxBuilder`,`build()` 返回 `Result`。

---

## 3. 从 Claude Code 取舍

下表是所有 gap 方案的"借 vs 舍"总纲。具体子系统在各自文档里展开。

| Claude Code 的做法 | legion 借鉴? | 理由 / 因地制宜 |
|---|---|---|
| 四层 Memory(Auto/Session/Agent/Team)文件化 + 后台 subagent 沉淀 | **借鉴分层思想,简化实现** | 借鉴"分层 + 自动决策 + 检索选择器",但 legion 用 SQLite 后端而非纯文件,且后台 subagent 可用轻量 LLM 调用而非 fork 进程 |
| Skills:Markdown + YAML frontmatter + 内嵌 Bash | **借鉴 frontmatter 格式与渐进披露,舍弃内嵌 shell 执行** | frontmatter 是低门槛扩展的精髓;但 legion 不做本地 coding,内嵌 `!`command` 执行面 RCE 风险,改为声明 `allowed_tools` 由 legion 工具体系执行 |
| Tool Call:PreToolUse Hooks 可改 input、Streaming 状态机 | **借鉴 hooks 注入点与审批回流** | hooks 是动态能力核心;legion 用 trait 对象 + channel 回流实现异步审批,而非 TS 闭包 |
| MCP:四种传输 + 认证雪崩缓存 + 描述截断 | **全部借鉴** | MCP 是标准协议,legion 应尽量兼容;认证缓存、描述截断、并发控制都是通用教训 |
| Sandbox:命令路由 + Git bare repo 逃逸专防 | **借鉴逃逸防护清单,本地隔离改用 OS 原语** | legion local backend 当前零隔离;借鉴 Claude Code 的 denyWrite 清单,并用 Linux namespace/seccomp(macOS 用 sandbox-exec)实现真隔离 |
| Context:Auto-compact 熔断 + 状态复灌 + PTL 防御 | **全部借鉴** | 这三条是 compact 工程化的精华,直接移植为 Rust 实现 |
| Multi-Agent:Coordinator + Swarm Teammates + mailbox | **借鉴 Coordinator 分相与结果回传,简化 Swarm** | legion 已有 task runner + ACP harness;Coordinator 模式可基于现有 task 依赖图实现;Swarm mailbox 暂列为 P2 |
| Session:JSONL + compact boundary 恢复 + orphan 修复 | **借鉴 boundary 恢复与 orphan 修复** | legion 已有 JSONL,补上 compact 边界标记与孤立 tool_result 修复即可 |
| Prompt:分层 section 缓存 + override 优先级 + dump 可观测 | **借鉴分层与可观测,缓存按需** | section 缓存对 Rust 意义不如 TS 大(无 GC 压力),但 override 优先级与 dump 可观测必须做 |

**取舍总原则**:Claude Code 的**安全防线、失败处理、可观测性**几乎全盘借鉴(这些是客观工程教训);**进程模型、异步范式、存储形态**因地制宜(因为语言与部署形态不同)。

---

## 4. 七条横切原则(每个 gap 方案都要对照)

每个 gap 文档的"设计目标"与"验收标准"都要显式回答下列七问。审阅者据此判断方案是否合规。

### P1. 扩展性优先于硬编码
新增能力必须通过 trait + registry 暴露,而非写死在某个 `match`。问:**"第三方能否在不改 legion 源码的前提下扩展这个能力?"** 若否,方案不合格。

### P2. 安全作为不变量
涉及工具执行、文件写、网络、子进程的能力,**默认 deny / 默认 required approval / 默认最小权限**。问:**"这个能力在默认配置下,最坏能造成什么损害?损害是否需要显式授权?"**

### P3. 增量演进,可回退
每个改动必须能在配置里关闭,且关闭后等价于当前行为。问:**"把新配置项设为关闭/默认,系统的行为是否和今天完全一致?"** 据此保证渐进上线与灰度。

### P4. 证据驱动设计
方案中每一个"现状"陈述必须带 `file:line` 证据,每一个"借鉴"必须指向 `claude-code-analysis/analysis/*.md` 的具体章节。问:**"这个判断有代码/文档支撑,还是凭印象?"**

### P5. 可观测性内建
新增能力必须产生 `tracing` 事件,关键路径必须有指标(Prometheus)或审计记录。问:**"这个能力出问题时,运维能否从日志/指标定位?"**

### P6. 失败模式显式分类
方案必须枚举该能力的失败模式,并标注处理策略(`retry` / `fallback` / `surface_to_user` / `abort_turn`)。问:**"这个能力在 provider 超时、磁盘满、用户拒绝审批时分别怎么表现?"**

### P7. 测试即契约
方案必须给出:happy path、主失败模式、默认行为(关闭等价现状)三类测试用例描述。问:**"这个方案能否被自动化验证?"**

---

## 5. 优先级标尺

所有 gap 按下表判定 P0 / P1 / P2。`00-overview.md` 的优先级矩阵据此填充。

| 级别 | 判定标准(满足任一) | 典型例子 |
|---|---|---|
| **P0** | ① 安全关键(默认配置下存在失控/越界风险);② 架构地基(其他多个 gap 依赖它落地);③ 改动集中且性价比极高(小改动解除大阻塞) | approval 人机回路(安全)、plugin-facade(地基)、sandbox 隔离(安全) |
| **P1** | ① 高杠杆扩展(落地后显著放大 agent 能力);② 明显提升内核健壮性;③ 有明确下游依赖者等待 | skills、mcp、memory-layers、compaction |
| **P2** | ① 生态广度(数量型增长);② 锦上添花的健壮性;③ 可延后且不影响主干 | 新增 channel/provider、session 健壮性增强、自动化高级特性 |

**工作量估计口径**(用于路线图,粗略):
- `S`(小):≤ 3 人日,改动集中在 1-2 个文件,无新 trait。
- `M`(中):1-2 人周,涉及新 trait 或跨 2-3 个 crate。
- `L`(大):≥ 3 人周,新子系统或多 crate 协同。

**依赖原则**:被依赖项必须排在依赖项之前;若被依赖项是 P0,依赖项最早 P1。

---

## 6. 文档与交叉引用约定

为保证 18 个文件连贯可读,统一以下约定:

### 6.1 路径与命名
- 文档根目录:`docs/design/gaps/`。
- 每类一个子目录 + `_index.md`:`02-missing/`、`03-shallow/`、`04-breadth/`。
- gap 文件用 kebab-case,名称=子系统名(如 `approval-loop.md`、`memory-layers.md`)。

### 6.2 每个 gap 文档的统一结构
所有 gap 文件遵循同一骨架(详见 [`00-overview.md`](./00-overview.md) §阅读指南):

1. **元信息表**(优先级 / 工作量 / 依赖 / 关联 PRD 章节)
2. **现状证据**(`file:line`)
3. **设计目标**(对照 §4 七条原则)
4. **架构设计**(模块职责、数据流图)
5. **接口设计**(Rust trait / struct / 配置 schema)
6. **集成点**(改动哪些现有 crate / 文件)
7. **风险与权衡**(借鉴 vs 因地制宜的取舍)
8. **实现路线图**(分阶段步骤)
9. **验收标准**(可自动化验证的清单)

### 6.3 交叉引用
- 文档间用相对链接,相对**当前文件所在目录**解析。路径写法示例(仅示路径,非完整链接语法):同目录文件直接写文件名 `01-guiding-principles.md`;进子目录写 `02-missing/skills.md`;子目录内的索引写 `_index.md`;子目录文件回上级写 `../00-overview.md`;子目录间跨类目写 `../03-shallow/approval-loop.md`。
- 引用 legion 源码用 `crate/path/file.rs:line` 格式(可点击)。
- 引用 Claude Code 分析用 `claude-code-analysis/analysis/04f-context-management.md §3`。
- 引用 legion PRD 用 `docs/design/agent-harness-prd.md §7`。

### 6.4 术语表(全文档统一)
| 术语 | 定义 |
|---|---|
| **Harness** | 可替换的 agent 执行后端,实现 `Harness` trait(内置 runtime 或外部 ACP) |
| **Approval Gate** | 工具执行前的权限决策与(可选)人工确认回路 |
| **Compact boundary** | transcript 中 compaction 发生的标记点,resume 时据此截断 |
| **Sidechain** | 子 agent 独立的 transcript 文件,不与主链混写 |
| **Bootstrap 文件** | 注入 system prompt 的 workspace 文件(AGENTS/SOUL/USER/TOOLS.md 等) |
| **Skill** | Markdown + frontmatter 描述的领域能力,按需注入 prompt 与工具 |

---

## 7. 与现有文档的关系

- 本目录是 **`docs/design/agent-harness-prd.md` 的补强**,不替代 PRD。PRD 定义"要做什么",本目录定义"现有差距 + 怎么补"。
- 本目录的"现状证据"以**源码为准**;若与 `AGENTS.md` 声明冲突,以源码为准并在文中标注。
- 实施后,完成的部分应在 `AGENTS.md` 的对应章节更新声明,保持声明与源码同步(参见 `00-overview.md` 的"声明同步"流程)。

---

*最后更新:2026-07-09。当横切原则或优先级标尺发生变化时,更新本文件并在 `00-overview.md` 的变更日志登记。*
