# Gap:Plugin Facade(插件系统是空壳)

> **实施状态**:Phase A1(trait 契约扩展 + 系统插件迁移)与 Phase A2(manifest + 用户声明型插件扫描/依赖排序/禁用)已实现。WebChat/Telegram/Tools/ACP 已通过 `Plugin` trait 注册并声明 `Capability`;stub 插件声明空 capabilities;`PluginRegistry` 支持 `load_dir`、`init_all`、`status`。Phase B 动态库加载 + panic 隔离与 Phase C 市场真实化待后续切片。

| 字段 | 值 |
|---|---|
| 类目 | [02-missing](./_index.md)(完全缺失) |
| 优先级 | **P0**(架构地基) |
| 工作量 | L(≥3 人周) |
| 前置依赖 | 无(本项是其他多个 gap 的前置) |
| 解锁 | skills、mcp、channels、tools-p1p2 |
| 关联 PRD | `docs/design/agent-harness-prd.md` §10(Plugin 子系统) |
| 关联分析 | `claude-code-analysis/analysis/04b-tool-call-implementation.md` §3(工具池动态装配);OpenClaw `plugins/` 目录 |

---

## 1. 现状证据

PRD §10 设计了 **7 种插件类型**(channel / tool / harness / context-engine / memory / cli-backend / diagnostics),但源码实现严重缩水:

- **PluginRegistry 仅暴露 `register_channel`**:`legion-plugin-sdk/src/lib.rs:86-115`。无 `register_tool` / `register_harness` / `register_context_engine` / `register_memory` / `register_cli_backend` / `register_diagnostics`。
- **5 个 system plugin 中 4 个是 stub**:`legion-gateway/src/plugins.rs:30-95` 用 `stub_plugin!` 宏生成 `MemoryPlugin` / `ProviderRouterPlugin` / `ContextEnginePlugin` / `AutomationPlugin`,其 `init()` 仅打日志。源码注释直言:*"minimal placeholders until the real crates expose Plugin impls"*。
- **真正接线的只有 channel**:`plugins.rs` 中 `ToolsPlugin` / `WebChatProvider` / `TelegramProvider` / `AcpPlugin` 是真实注册。
- **插件市场是内存态**:`legion-gateway/src/market/mod.rs`(169 行)的 `PluginMarket` 仅用 `HashMap` 标记 `installed=true`,**无真实下载/安装/发布/审计**。CLI `legion market install` 仅调 WebSocket 后空转。
- **无插件包格式**:无 `manifest.json` 解析、无 `plugins/` 目录约定、无版本/依赖声明。
- **无动态加载**:所有"插件"硬编码编译进 Gateway 二进制,无运行时 `.so`/`.dylib` 加载,无 panic 隔离(`catch_unwind`)。

**结论**:Plugin 层当前是 **façade(门面)**——真实能力在各 crate 被 gateway 直接引用,不经插件系统调度。这是 legion 最大的**架构债**。

---

## 2. 设计目标

对照 [指导原则](../01-guiding-principles.md) §4 七条横切原则:

- **P1 扩展性优先于硬编码**:第三方应能不改 legion 源码,新增 channel/tool/harness/memory/context-engine/diagnostics。
- **P2 安全作为不变量**:插件默认最小权限;外部插件(非系统插件)的 tool 执行仍须经 approval gate;插件 panic 不得拖垮 Gateway。
- **P3 增量演进**:系统插件迁移到真实现时,Gateway 行为不变;`register_*` 全部可选。
- **P4 证据驱动**:现状与借鉴均见 §1 与 §7。
- **P5 可观测**:插件加载/失败/panic 都产生 `tracing` 事件。
- **P6 失败显式**:插件加载失败、版本不兼容、依赖缺失分类报错。
- **P7 测试即契约**:每类插件注册有 happy/stub-fail/panic-isolation 测试。

---

## 3. 架构设计

### 3.1 分层:系统插件 vs 用户插件 vs 动态库插件

```
┌─────────────────────────────────────────────────┐
│              Gateway (legion-gateway)            │
│  ┌───────────────────────────────────────────┐  │
│  │         PluginRegistry (核心)              │  │
│  │  - 按能力索引(channel/tool/harness/...)    │  │
│  │  - 生命周期:init() → serve → shutdown()   │  │
│  │  - catch_unwind 包裹每次调用(隔离 panic)  │  │
│  └───────────────────────────────────────────┘  │
│        ▲            ▲            ▲               │
│        │            │            │               │
│  ┌─────┴─────┐ ┌────┴─────┐ ┌────┴──────────┐   │
│  │ 系统插件   │ │ 用户插件  │ │ 动态库插件     │   │
│  │(编译进bin)│ │(manifest)│ │(.so/.dylib)   │   │
│  │ WebChat/  │ │ ~/.legion │ │ libloading    │   │
│  │ Telegram/ │ │ /plugins/ │ │ + extern C ABI│   │
│  │ AcpPlugin │ │           │ │               │   │
│  └───────────┘ └──────────┘ └───────────────┘   │
└─────────────────────────────────────────────────┘
```

- **系统插件**:随二进制编译,注册真实能力(WebChat/Telegram/AcpTools)。这是现有行为的"原地升级"——把 stub 换成真 impl。
- **用户插件**:`~/.legion/plugins/<name>/manifest.json` + (可选)脚本/配置,声明它提供哪类能力。Phase A 只支持"声明型"用户插件(指向已注册的系统能力或脚本 hook);真正的可执行用户插件走动态库。
- **动态库插件**:Rust crate 编译为 `cdylib`,导出 `extern "C" fn legion_plugin_create() -> *mut PluginVtable`。`libloading` 加载,`catch_unwind` 隔离。

### 3.2 能力分发
`PluginRegistry` 不直接持有具体能力,而是按 7 类维护"能力提供者列表",gateway 各子系统按需拉取:

```
ChannelSubsystem  → registry.channels()
ToolSubsystem     → registry.tools()        // 合并入 CoreToolRegistry
HarnessRegistry   → registry.harnesses()
AgentRuntime      → registry.context_engine() / memory_backend()
Diagnostics       → registry.diagnostics()
```

### 3.3 生命周期
```
load_manifest → resolve_dependencies → init(ctx) [catch_unwind]
   → serve (各子系统按需调用) [每次 catch_unwind]
   → shutdown() [catch_unwind]
```

---

## 4. 接口设计(Rust)

### 4.1 核心 trait(`legion-plugin-sdk`)

```rust
// crates/legion-plugin-sdk/src/lib.rs(扩展)

use async_trait::async_trait;
use std::sync::Arc;
use std::path::PathBuf;

/// 所有插件的公共契约。
#[async_trait]
pub trait Plugin: Send + Sync {
    /// 稳定标识,用作 registry key 与 manifest 引用。
    fn id(&self) -> &str;
    /// 语义化版本,用于依赖解析与兼容性检查。
    fn version(&self) -> &str;
    /// 声明本插件提供哪些能力(可多类)。
    fn capabilities(&self) -> &[Capability];

    /// 注册阶段:读取配置、建立连接、返回该插件提供的能力对象。
    async fn init(&self, ctx: &PluginContext) -> Result<PluginHandles, PluginError>;

    /// 优雅关闭:释放资源。
    async fn shutdown(&self) -> Result<(), PluginError> {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    Channel, Tool, Harness, ContextEngine, Memory, CliBackend, Diagnostics,
}

/// 插件能力集合:init 返回,由 registry 分发给各子系统。
pub struct PluginHandles {
    pub channels: Vec<Arc<dyn ChannelProvider>>,
    pub tools: Vec<Arc<dyn Tool>>,
    pub harnesses: Vec<Arc<dyn Harness>>,
    pub context_engine: Option<Arc<dyn ContextEngine>>,
    pub memory_backend: Option<Arc<dyn MemoryBackend>>,
    pub cli_backends: Vec<Arc<dyn CliBackend>>,
    pub diagnostics: Vec<Arc<dyn DiagnosticProbe>>,
}

pub struct PluginContext {
    pub config: serde_json::Value,        // 插件专属配置段
    pub workspace: PathBuf,
    pub shutdown_token: tokio_util::sync::CancellationToken,
    pub agent_id: Option<String>,         // 是否 agent-scoped 插件
}
```

### 4.2 Registry

```rust
pub struct PluginRegistry {
    plugins: Vec<Arc<dyn Plugin>>,
    handles: Vec<PluginHandles>,           // init 后的能力集合
    status: HashMap<String, PluginStatus>,
}

#[derive(Debug, Clone)]
pub enum PluginStatus { Loaded, Initialized, Failed(String), Panicked, Disabled }

impl PluginRegistry {
    pub fn new() -> Self;

    /// 注册系统插件(编译进 bin)。
    pub fn register(&mut self, plugin: Arc<dyn Plugin>);

    /// 从 manifest 加载用户/动态库插件。
    pub fn load_dir(&mut self, dir: &Path) -> Result<()>;

    /// 初始化所有已加载插件(catch_unwind 隔离)。
    pub async fn init_all(&mut self, ctx_factory: impl Fn() -> PluginContext) -> Result<()>;

    // 能力查询(各子系统调用)
    pub fn channels(&self) -> Vec<Arc<dyn ChannelProvider>>;
    pub fn tools(&self) -> Vec<Arc<dyn Tool>>;
    pub fn harnesses(&self) -> Vec<Arc<dyn Harness>>;
    pub fn context_engine(&self) -> Option<Arc<dyn ContextEngine>>;
    pub fn memory_backend(&self) -> Option<Arc<dyn MemoryBackend>>;
    pub fn diagnostics(&self) -> Vec<Arc<dyn DiagnosticProbe>>;

    pub fn status(&self) -> &HashMap<String, PluginStatus>;
}
```

### 4.3 插件包格式(manifest.json)

```jsonc
// ~/.legion/plugins/<name>/manifest.json
{
  "id": "acme-voice",
  "version": "0.2.0",
  "name": "Acme Voice Channel",
  "capabilities": ["channel"],
  // 动态库路径(相对 manifest)。系统插件省略此字段。
  "library": "libacme_voice.dylib",
  // 依赖的其他插件 id(用于解析加载顺序)
  "dependsOn": ["acme-auth"],
  "minLegionVersion": "0.1.0",
  "config": {
    // 用户可编辑的插件配置,运行时透传给 PluginContext.config
    "apiKey": "${ACME_VOICE_KEY}"
  }
}
```

```rust
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginManifest {
    pub id: String,
    pub version: String,
    pub name: String,
    pub capabilities: Vec<Capability>,
    #[serde(default)]
    pub library: Option<PathBuf>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    pub min_legion_version: Option<String>,
    #[serde(default)]
    pub config: serde_json::Value,
}
```

### 4.4 动态库 ABI(Phase B,可选)

```rust
// 动态库导出的 C ABI(插件侧)
#[no_mangle]
pub extern "C" fn legion_plugin_create() -> *mut dyn Plugin { ... }

// registry 侧(libloading 加载)
unsafe {
    let lib = libloading::Library::new(&path)?;
    let ctor: libloading::Symbol<extern "C" fn() -> *mut dyn Plugin> =
        lib.get(b"legion_plugin_create")?;
    let plugin = Box::from_raw(ctor());
    registry.register(Arc::from(plugin));
}
```

> **安全约束**:动态库插件与 Gateway 同进程,`unsafe` 边界必须收敛在加载层;每次调用插件方法用 `catch_unwind` 包裹(见 §5.3)。

### 4.5 配置 schema(`legion-core`)

```jsonc
// legion.json
{
  "plugins": {
    "dirs": ["~/.legion/plugins"],          // 扫描目录
    "enableDynamicLibraries": false,         // 默认关闭(P2 安全)
    "disabled": ["experimental-foo"],        // 显式禁用清单
    "perPlugin": {                           // 覆盖单插件配置
      "acme-voice": { "config": { "region": "us" } }
    }
  }
}
```

---

## 5. 集成点

| 改动位置 | 改动内容 |
|---|---|
| `legion-plugin-sdk/src/lib.rs` | 新增 `Plugin`/`Capability`/`PluginHandles`/`PluginContext`/`PluginRegistry`/`PluginManifest`;保留旧 `register_channel` 作为兼容入口(内部转 `register`)。 |
| `legion-gateway/src/plugins.rs` | 用真实 impl 替换 4 个 `stub_plugin!`;`load_system_plugins` 改为构造 `PluginRegistry` 并 init_all。 |
| `legion-gateway/src/gateway.rs:54-313` | 启动流程接入 `registry.init_all()`;关闭流程接入 `registry.shutdown_all()`。 |
| `legion-tools/src/registry.rs` | `CoreToolRegistry` 合并 `registry.tools()`(含优先级:内建同名优先,防恶意覆盖,借鉴 Claude Code `uniqBy` 内建优先)。 |
| `legion-runtime` | context engine / memory backend 从 registry 取(有则用插件提供者,无则默认)。 |
| `legion-gateway/src/market/mod.rs` | 重写为真实下载/安装(Phase B);Phase A 仅保留 manifest 索引。 |
| `legion-cli` | `legion plugins list/enable/disable/install/uninstall` 子命令(§8 路线图)。 |

---

## 6. 风险与权衡

### 6.1 动态加载方案:libloading vs WASM

| 方案 | 优点 | 缺点 | 取舍 |
|---|---|---|---|
| **libloading(原生 cdylib)** | 无性能开销;可直接用 legion trait;实现简单 | panic 可传染(需 catch_unwind);ABI 稳定性要求高;跨平台符号差异 | **Phase B 采用,默认 `enableDynamicLibraries: false`** |
| **wasmtime(WASM 插件)** | 强隔离(无法 panic 主进程);跨平台一致 | ABI 复杂(需 wit-bindgen);性能开销;无法直接共享 legion `Arc<dyn Trait>` | 暂不采用,留作未来研究 |
| **纯进程内 trait(无动态库)** | 最简单;零 unsafe | 无法加载第三方二进制 | **Phase A 默认形态**:系统插件 + 声明型用户插件 |

**决策**:Phase A 只做"进程内 trait 插件 + manifest 声明";动态库加载列为 Phase B 且默认关闭;不引入 WASM。

### 6.2 借鉴 Claude Code:工具池同名优先级
Claude Code `assembleToolPool()` 用 `uniqBy` 保证**内建工具同名优先于 MCP/插件工具**,防恶意插件覆盖 `read`/`exec`。legion 的 `CoreToolRegistry` 合并插件工具时须复刻此规则。

### 6.3 ABI 稳定性
动态库插件要求 legion 暴露稳定的 trait 定义。Phase B 引入动态库前,需冻结 `Plugin`/`Tool`/`ChannelProvider` 的方法签名,并通过 `min_legon_version` 做兼容性门控。

### 6.4 因地制宜:Claude Code 无此问题
Claude Code 是单进程 TS,所有"扩展"在编译期已知;legion 是长驻 Gateway + 多 agent,需额外考虑:**agent-scoped 插件**(某插件只对特定 agent 生效)、**插件 panic 不影响其他 agent 的会话**。这两点用 `catch_unwind` + per-agent 插件上下文解决。

---

## 7. 实现路线图

### 阶段 A1(Phase A,~1 人周):trait 契约 + 系统插件迁移
1. 在 `legion-plugin-sdk` 定义 `Plugin`/`Capability`/`PluginHandles`/`PluginContext`/`PluginRegistry`。
2. 把现有 `WebChatProvider`/`TelegramProvider`/`AcpTools` 迁移为 `Plugin` 实现,通过 `capabilities()` 声明。
3. 替换 `plugins.rs` 的 4 个 stub(若对应 crate 暂无真 impl,保留 stub 但标注 `Capability` 为空,不再伪装接线)。
4. `Gateway::start` 接入 `registry.init_all()` / `shutdown_all()`。
5. **验收**:现有 channel/tool/harness 行为不变;`registry.status()` 正确反映加载状态。

### 阶段 A2(Phase A,~1.5 人周):manifest + 用户声明型插件
1. 定义 `PluginManifest` schema + `load_dir` 扫描。
2. 实现依赖解析(`depends_on` 拓扑排序,检测环)。
3. `plugins.dirs` / `plugins.disabled` 配置生效。
4. `legion plugins list/enable/disable` CLI。
5. **验收**:在 `~/.legion/plugins/` 放一个声明型插件,能被加载、init、shutdown;禁用清单生效。

### 阶段 B(Phase B,~1.5 人周):动态库加载 + panic 隔离
1. `libloading` 加载 cdylib + `legion_plugin_create` ABI。
2. `catch_unwind` 包裹所有插件方法调用;panic → `PluginStatus::Panicked` + `tracing::error` + 禁用该插件。
3. 同名工具优先级规则(内建 > 系统插件 > 动态库插件)。
4. **验收**:一个故意 panic 的动态库插件被加载后,触发 panic 时 Gateway 不崩,该插件被隔离禁用。

### 阶段 C(Phase C,~1 人周):插件市场真实化
1. `market/mod.rs` 重写:从 registry 索引 + (可选)远程 ClawHub 式 registry 下载。
2. `legion market install/uninstall` 真实下载到 `~/.legion/plugins/`。
3. **验收**:`market install <name>` 后插件出现在 `plugins list` 且可启用。

---

## 8. 验收标准

实施完成的判定清单(可直接转 issue acceptance criteria):

- [ ] `legion plugins list` 列出所有已加载插件及其 `PluginStatus` 与 `Capability`(CLI 子命令待补;Gateway 侧能力已就绪)。
- [ ] `legion plugins enable/disable <id>` 生效,重启后持久(写回 `legion.json`)(CLI 子命令待补;`plugins.disabled` 配置已生效)。
- [x] 系统插件(WebChat/Telegram/AcpTools)通过 `Plugin` trait 注册,行为与改动前一致(回归测试通过)。
- [x] 第三方可通过实现 `Plugin` trait + manifest 新增声明型插件,无需改 legion 源码(提供 `PluginManifest`/`ManifestPlugin` + `load_dir` 拓扑排序测试)。
- [ ] 动态库插件 panic 不影响 Gateway 与其他 agent 会话(`catch_unwind` 隔离测试,Phase B)。
- [ ] 同名工具冲突时,内建工具优先(防恶意覆盖测试,Phase B/C)。
- [x] 插件加载/init/shutdown 全程有 `tracing` 结构化日志。
- [x] `AGENTS.md` 更新插件章节,声明与源码同步。

---

*上一篇:[`_index.md`](./_index.md) · 下一个 gap:[`skills.md`](./skills.md) · 返回总览:[`../00-overview.md`](../00-overview.md)*
