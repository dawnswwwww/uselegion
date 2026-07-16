# Gap:Skills 系统(Phase A+B 已完成)

> **实施状态**:Phase A(核心 skill 加载与系统提示摘要注入)、Phase B(paths 条件触发 / 按需召回完整 body + token 上限)、Phase C(plugin 来源)与轻量 LLM 选择器均已完成。Skills gap 全量关闭。

| 字段 | 值 |
|---|---|
| 类目 | [02-missing](./_index.md)(完全缺失) |
| 优先级 | **P0**(高杠杆扩展) |
| 工作量 | M(1-2 人周) |
| 前置依赖 | [plugin-facade](./plugin-facade.md)(skill 可作为插件来源;workspace skill 可独立) |
| 关联 PRD | `agent-harness-prd.md` §8 T1(工具来源含 Skill 声明)、§6 R3(Skills prompt 注入) |
| 关联分析 | `claude-code-analysis/analysis/04c-skills-implementation.md` |

---

## 1. 现状证据

- **配置已落地**:`legion-core/src/config.rs:553-565` 定义 `SkillsConfig { dirs, max_summary_tokens, max_body_tokens, max_triggered_skills, enabled }`,`AgentDefaults.skills` 已替换为 `SkillsConfig`(`:158`),默认 `enabled=false` 以保持无 skill 时行为不变;同时支持旧数组格式(字符串列表)向后兼容解析为 `dirs`。
- **核心 crate 已创建**:`crates/legion-skills/src/lib.rs` 与 `registry.rs` 实现 `Skill`/`SkillFrontmatter`/`SkillRegistry`/`SkillRegistryImpl`、YAML frontmatter 解析、glob paths 索引、按名称/关键词召回、摘要块生成,并覆盖 9 个单元测试。
- **prompt 已集成**:`legion-runtime/src/context.rs:85-142` 的 `assemble_system_prompt` 新增 `skill_summary_block` 与 `skill_body_block` 参数,在 bootstrap/MEMORY/override 之后追加 skill 摘要/完整 body;无 skill 或摘要为空时行为不变。
- **agent_loop 已接入**:`legion-runtime/src/agent_loop.rs:130-161` 在 `run_loop` 开头,当 `skills.enabled=true` 时实例化 `SkillRegistryImpl`,加载 `dirs` 下每个 `<name>/SKILL.md`,生成摘要注入系统提示,并将加载的 skill 名称写入 `SessionContext.active_skills` 用于 compaction 后复灌。
- **按需召回已接入**:`legion-runtime/src/agent_loop.rs:158-165` 在组装系统提示前调用 `SkillRegistry::relevant(&request.user_message)`,将命中的 skill 完整 body 注入初始系统提示。
- **paths 条件触发已接入**:`legion-runtime/src/agent_loop.rs:273-312` 在每轮工具执行后读取 `SessionContext.viewed_files`,转工作区相对路径/basename 后调用 `SkillRegistry::match_paths`,把新命中的 skill body 以独立 system message 追加到下一轮上下文,并用 `HashSet` 去重避免同 run 内重复注入。
- **token 上限保护**:`legion-runtime/src/skills_prompt.rs` 新增 `skill_body_block`,使用 `token_counter::count_tokens` 按 `max_body_tokens` 截断,优先保留完整 skill 边界,单 skill 超限时截断 body 并加 `(truncated)` 提示。

**结论**:Skills 核心加载、按需召回与 paths 条件触发已可用;剩余工作集中在 plugin 来源、轻量 LLM 选择器与 CLI 命令。

---

## 2. 设计目标(对照七条原则)

- **P1 扩展性**:用户在 `~/.legion/skills/<name>/SKILL.md` 放一个 Markdown 文件即可新增领域能力,不改 legion 源码。
- **P2 安全**:skill **不执行内嵌 shell**;skill 声明的 `allowed_tools` 仍经 approval gate;skill body 注入受 token 上限保护。
- **P3 增量**:无 skill 时 `assemble_system_prompt` 行为不变。
- **P4 证据**:现状见 §1;借鉴见 §6。
- **P5 可观测**:skill 加载/触发/注入产生 `tracing` 事件。
- **P6 失败显式**:frontmatter 解析失败、body 超限、循环引用分类报错。
- **P7 测试**:加载、paths 条件触发、按需召回、token 限流各有测试。

---

## 3. 架构设计

### 3.1 Skill 的语义:提示注入,不是代码执行

> **关键取舍**(详见 §6.1):Claude Code 的 skill 支持内嵌 `!command` 在宿主机执行 shell。legion **舍弃此能力**——skill 仅作为"领域能力包":一段 Markdown 正文 + 声明它允许使用哪些工具。模型读取 skill 后,通过 legion 工具体系(经 approval gate)执行。这消除了"skill 文件 = RCE 入口"的风险。

### 3.2 三种来源

```
SkillRegistry
   ├── workspace skills   <workspace>/.agent/skills/<name>/SKILL.md  (项目级)
   ├── user-global skills ~/.agent/skills/<name>/SKILL.md            (用户级)
   │                      ~/.legion/skills/<name>/SKILL.md           (向后兼容)
   ├── bundled skills     编译期内置(随 legion 分发的基础 skill)
   └── plugin skills      由 plugin-facade 的 Plugin 提供(capability 含 skill)
```

### 3.3 三种触发方式(渐进式披露)

借鉴 Claude Code `04c-skills-implementation.md` §4/§8:

1. **显式调用**:`user_invocable: true` 的 skill 出现在 REPL/CLI 的 skill 列表,用户 `/skill <name>` 或模型主动调用。
2. **条件触发(paths)**:声明 `paths: ["*.tf"]` 的 skill,当 agent 操作匹配 glob 的文件时**自动注入**(Hook 订阅模式)。
3. **按需召回**:模型根据用户意图,由 registry 用 description 匹配(可选轻量 LLM 选择器)决定是否加载完整 body。

**渐进式披露**:默认只把 skill 的 **name + description**(一行)注入 prompt;只有触发/召回时才注入完整 body。避免所有 skill 全量塞爆上下文。

### 3.4 数据流

```
load(扫描目录) → parse(frontmatter + body) → index(by name / by paths glob)
       │
       ▼
assemble_system_prompt:
   注入 [skill 摘要清单] (name + description,受 token 上限)
       │
       ▼ (条件触发 / 按需召回)
注入 [匹配 skill 的完整 body] + [allowed_tools 声明]
```

---

## 4. 接口设计(Rust)

### 4.1 Skill 类型

```rust
// 新 crate legion-skills,或先放入 legion-runtime/src/skills.rs

use std::path::PathBuf;
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort { Low, Medium, High }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillSource { Workspace, Bundled, Plugin }

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillFrontmatter {
    pub name: String,
    pub description: String,           // 必填:用于摘要注入与召回匹配
    #[serde(default)]
    pub when_to_use: Option<String>,
    #[serde(default)]
    pub allowed_tools: Vec<String>,    // 声明该 skill 可用哪些 legion 工具
    #[serde(default)]
    pub paths: Vec<String>,            // glob,条件触发
    #[serde(default = "default_true")]
    pub user_invocable: bool,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<Effort>,
}
fn default_true() -> bool { true }

pub struct Skill {
    pub frontmatter: SkillFrontmatter,
    pub body: String,                  // markdown 正文(去除 frontmatter)
    pub source: SkillSource,
    pub path: PathBuf,                 // 来源文件,用于错误定位
}
```

### 4.2 SkillRegistry trait

```rust
use async_trait::async_trait;

#[async_trait]
pub trait SkillRegistry: Send + Sync {
    /// 扫描多个目录,解析所有 SKILL.md。返回加载报告(成功/失败列表)。
    async fn load(&self, dirs: &[PathBuf]) -> Result<LoadReport, SkillError>;

    /// workspace 当前操作的文件命中哪些 paths 条件 skill。
    fn match_paths(&self, touched_files: &[String]) -> Vec<&Skill>;

    /// 按用户意图召回(Phase A 用关键词匹配;Phase B 可接轻量 LLM 选择器)。
    fn relevant(&self, intent: &str, limit: usize) -> Vec<&Skill>;

    fn get(&self, name: &str) -> Option<&Skill>;
    fn all(&self) -> &[Skill];

    /// 注入 prompt 的摘要行(name + description),受 max_tokens 截断。
    fn summary_block(&self, max_tokens: usize) -> String;
}

pub struct LoadReport {
    pub loaded: Vec<String>,
    pub failed: Vec<(PathBuf, SkillError)>,   // 解析失败的文件 + 原因
}
```

### 4.3 frontmatter 解析

```rust
// 用 YAML front matter 解析:`---\n<yaml>\n---\n<body>`
pub fn parse_skill_md(content: &str, path: PathBuf, source: SkillSource)
    -> Result<Skill, SkillError>
{
    let (yaml, body) = split_frontmatter(content)?;  // 切分首个 --- 块
    let fm: SkillFrontmatter = serde_yaml::from_str(&yaml)
        .map_err(|e| SkillError::InvalidFrontmatter { path: path.clone(), source: e })?;
    validate_name(&fm.name)?;                          // 禁止路径穿越字符
    Ok(Skill { frontmatter: fm, body: body.trim().to_string(), source, path })
}
```

### 4.4 配置 schema(`legion-core`)

将 `skills: Vec<String>` 占位升级为:

```jsonc
// legion.json
{
  "skills": {
    "dirs": ["~/.legion/skills"],     // 扫描目录(workspace skill 来源)
    "maxSummaryTokens": 800,          // 摘要注入上限
    "enabled": true                   // 全局开关(关闭则不注入任何 skill)
  }
}
```

```rust
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SkillsConfig {
    pub dirs: Vec<PathBuf>,
    pub max_summary_tokens: usize,    // default 800
    pub enabled: bool,                // default true
}
impl Default for SkillsConfig {
    fn default() -> Self {
        Self { dirs: vec![], max_summary_tokens: 800, enabled: true }
    }
}
```

---

## 5. 集成点

| 位置 | 改动 |
|---|---|
| `legion-runtime/src/context.rs:81-131` | `assemble_system_prompt` 末尾追加 `skill_summary_block`(若 `enabled`);触发/召回时追加完整 body。无 skill 时行为不变。 |
| `legion-core/src/config.rs:156` | `skills: Vec<String>` → `skills: SkillsConfig`(向后兼容:旧字符串数组解释为 `dirs`);默认 `dirs` 包含 `~/.agent/skills` 与 `~/.legion/skills`。 |
| `legion-runtime/src/agent_loop.rs` | 循环开始处加载 `skills.dirs`、`<workspace>/.agent/skills`(若存在)与 plugin skills;用最近操作的文件路径调用 `match_paths`,把命中的 skill body 注入当轮上下文。 |
| 新 crate `legion-skills` | `Skill`/`SkillRegistry`/frontmatter 解析/glob 匹配;放独立 crate 便于 plugin 复用。 |
| `legion-plugin-sdk` | 插件通过 `PluginHandles::skills` 提供 `Vec<Skill>`;`ManifestPlugin` 解析 `manifest.skills` 中声明的 SKILL.md;`PluginRegistry` 收集并暴露 `skills()`。 |
| `legion-runtime` | `AgentRuntime::with_plugin_skills` → `LegacyContextEngine::with_plugin_skills` → `run_loop` 用 `SkillRegistry::add` 合并 plugin skills。 |
| `legion-gateway` | 加载 system/user 插件后统一 `init_all`,将 `registry.skills()` 传入 `AgentRuntime`。 |
| `legion-cli` | `legion skills list/reload` 子命令。 |

---

## 6. 风险与权衡

### 6.1 舍弃内嵌 shell 执行(最重要取舍)
- **Claude Code 做法**:`04c-skills-implementation.md` §6 支持 Markdown 内 `!`command`` 与 ``` ! ``` 代码块,在 skill 调用前于宿主机执行。
- **legion 决策**:**不实现**。legion 面向多通道消息驱动,skill 文件若能执行任意 shell 等同 RCE 入口(任何能写 workspace 的人/agent 都可植入恶意 skill)。改由 `allowed_tools` 声明 + legion 工具体系执行,所有执行仍经 approval gate。
- **代价**:失去"skill 自动跑脚本"的便利。**缓解**:需要脚本能力的场景用 `exec` 工具(已有,且经 approval),而非 skill 内嵌。

### 6.2 按需召回的选择器实现
- Claude Code `findRelevantMemories()` 用轻量模型做选择器。
- **legion Phase A**:用 description 关键词匹配(无额外 LLM 调用,零成本)。
- **legion Phase B**:可选接 `model: "cheap-router"` 走轻量 LLM 选择器(借鉴 memory-layers 的召回选择器)。

### 6.3 paths glob 与性能
每次 agent 操作文件都要 `match_paths`,若 skill 数量大可能成瓶颈。**缓解**:glob 编译为 `globset::GlobSet`(编译期构建,O(1) 匹配)。

### 6.4 token 上限保护
借鉴 Claude Code `truncateEntrypointContent()`(200 行/25KB):skill summary 与 body 都按 `max_summary_tokens` 截断,防 prompt 爆炸。

### 6.5 循环/递归 skill
skill body 不应触发加载另一个 skill 的完整 body(避免雪崩)。**约束**:只有顶层 `assemble_system_prompt` 注入 body,skill body 内的 `@skill` 引用仅展开为 description。

---

## 7. 实现路线图

### 阶段 A(Phase A,✅ 已完成):核心 skill 加载与注入
1. ✅ 新建 `legion-skills` crate:`Skill`/`SkillFrontmatter`/`SkillRegistry`/frontmatter 解析/glob 匹配。
2. ✅ `assemble_system_prompt` 接入 summary_block(受 `max_summary_tokens`)。
3. ✅ `SkillsConfig` 替换占位字段(向后兼容旧数组格式)。
4. ✅ `legion skills list/reload` CLI(`crates/legion-cli/src/skills.rs`)。
5. **验收**:放一个 SKILL.md,摘要出现在 system prompt;Phase B 再实现 `/skill <name>` 显式触发完整 body。

### 阶段 B(Phase B,✅ 已完成):paths 条件触发 + 按需召回
1. ✅ `agent_loop` 接入 `match_paths`:用工具回写的 `viewed_files` 转相对路径/basename 后匹配 skill 的 `paths` glob,命中时自动注入完整 body。
2. ✅ 按需召回的关键词匹配实现:根据用户意图调用 `SkillRegistry::relevant`,把命中的 skill body 注入初始系统提示。
3. ✅ `SkillsConfig` 新增 `max_body_tokens` / `max_triggered_skills`;新增 `legion-runtime/src/skills_prompt.rs` 做 body block 渲染与 token 截断。
4. **验收**:操作 `.tf` 文件时,声明 `paths: ["*.tf"]` 的 skill 自动注入 body;输入"帮我写 Rust"时 `rust` skill body 被召回。

### 阶段 C(Phase C,✅ 已完成):plugin skill 来源 + 轻量 LLM 召回
1. ✅ `legion-plugin-sdk` 的 `PluginHandles` 增加 `skills: Vec<Skill>`;`Capability`/`PluginKind` 新增 `Skill` variant;`ManifestPlugin` 支持通过 `manifest.skills` 声明 SKILL.md 路径并在 `init` 时解析。
2. ✅ `PluginRegistry::init_all` 收集所有 plugin 返回的 skill,暴露 `skills()` 查询接口。
3. ✅ `AgentRuntime` 与 `LegacyContextEngine` 增加 `with_plugin_skills`,`run_loop` 在加载 workspace skills 后用 `SkillRegistry::add` 合并 plugin skills。
4. ✅ `legion-gateway` 在加载 system/user 插件并调用 `init_all` 后,把 `registry.skills()` 注入 `AgentRuntime`。
5. ✅ 按需召回接入轻量 LLM 选择器:`SkillsConfig.selector_model` 配置 cheap model 时,`KeywordSkillSelector` 粗排 + `LlmSkillSelector` 精排;未配置时保持关键词匹配(零成本、向后兼容)。
6. **验收**:插件提供的 skill 被发现并注入;配置 `selector_model` 后 LLM 选择器生效且失败降级。

---

## 8. 验收标准

- [x] `legion skills list` 列出所有已加载 skill(`~/.agent/skills`、`~/.legion/skills`、`<workspace>/.agent/skills`、配置目录、plugin 来源)。
- [x] `legion skills reload` 重新扫描 skill 目录并报告解析错误。
- [x] 在 `~/.legion/skills/<name>/SKILL.md` 放 Markdown 文件,无需改源码即可被加载并注入摘要。
- [x] 用户意图命中 skill description/名称关键词时,对应 skill 完整 body 注入初始系统提示(按需召回)。
- [x] 配置 `skills.selector_model` 后,轻量 LLM 从候选 skill 中精选最相关的 body 注入;未配置时等价关键词匹配(回归)。
- [x] LLM 选择器超时/解析失败/provider 错误时降级为空选择,不阻塞主 turn。
- [x] frontmatter 解析失败或 name 非法时,`load` 报明确错误且不影响其他 skill。
- [x] summary 注入受 `max_summary_tokens` 限制;skill body 注入受 `max_body_tokens` 限制并按 token 截断。
- [x] `paths: ["*.tf"]` 的 skill 在 agent 操作 `.tf` 文件时自动注入完整 body(Phase B 集成测试)。
- [x] manifest 插件声明 `skills: ["SKILL.md"]` 后,对应 skill 被解析并随 `PluginRegistry::skills()` 注入 runtime(Phase C 集成测试)。
- [x] `<workspace>/.agent/skills` 存在时被自动扫描,与 `~/.agent/skills`、`~/.legion/skills` 及配置目录合并加载。
- [x] skill 的 `allowed_tools` 声明的工具调用仍经 approval gate(legion 工具自带 policy,skill 不绕过)。
- [x] skill **不执行内嵌 shell**(无 `!command` 语义;这是 legion 的明确安全取舍)。
- [x] 无 skill / `skills.enabled=false` 时,`assemble_system_prompt` 与之前行为一致(回归测试通过)。
- [x] `AGENTS.md` 新增 Skills 章节并补充 `selector_model` 说明。

---

*上一篇:[`plugin-facade.md`](./plugin-facade.md) · 下一个 gap:[`mcp.md`](./mcp.md) · 返回总览:[`../00-overview.md`](../00-overview.md)*
