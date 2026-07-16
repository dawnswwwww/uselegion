# Gap:Sandbox 隔离(local backend 零隔离)

> **实施状态**:Phase A(Linux restricted backend / `bwrap` + 逃逸防护清单 + `pre_exec_guard`)与 Phase B(macOS `sandbox-exec` + `sandbox_available` 平台检测)已实现,`mode=off` 等价原有 local 行为。Phase C(Cube 复用/scope/web_fetch allowlist 反推)与 Phase D(Windows)待后续切片。

| 字段 | 值 |
|---|---|
| 类目 | [03-shallow](./_index.md)(内核浅化) |
| 优先级 | **P0**(安全关键) |
| 工作量 | L(≥3 人周,平台分叉) |
| 前置依赖 | 无(与 [approval-loop](./approval-loop.md) 协同) |
| 关联 PRD | `agent-harness-prd.md` §8 T5/T6(沙箱模式/CubeSandbox) |
| 关联分析 | `claude-code-analysis/analysis/04e-sandbox-implementation.md` |

---

## 1. 现状证据

legion 的 sandbox 有两个 backend,但隔离能力严重不对等:

### 1.1 Local backend:零隔离
- `legion-tools/src/sandbox/local.rs:19-53` 直接 `sh -c`(Unix)/ `cmd /C`(Win)在**主机 shell** 执行。
- **无 namespace / seccomp / chroot / cgroup / 文件隔离 / 网络隔离**。任何 `exec` 工具调用(即便经 approval 放行)都能读写主机任意文件、发起任意网络请求。
- 唯一约束是 approval gate(见 [approval-loop](./approval-loop.md))——但放行后的执行**完全裸奔**。

### 1.2 Cube backend:可用但缺高级特性
- `cube.rs:24-342` 实现了完整生命周期(create/exec/kill)+ 手写 Connect Protocol 编解码 + wiremock E2E 测试(这是亮点)。
- 但缺:网络 egress **白名单**、volume 挂载、快照/克隆/回滚、sandbox **复用**(每次 exec 都 create+kill 新 sandbox,无连接池)、不支持压缩帧(`cube.rs:254-258` 直接报错)。

### 1.3 共性缺失
- 无 PRD T5 的 sandbox **scope**(shared/agent/session)。
- 无逃逸防护清单(对比 Claude Code 的 settings denyWrite / git bare repo 专防)。
- 无平台/依赖检测 + fail-if-unavailable(容易"以为开了实际没开")。

**结论**:legion 的"沙箱"目前只是远程 Cube MicroVM,本地执行零隔离。对自托管 gateway(暴露在网络/多用户),这是安全隐患。

---

## 2. 设计目标(对照七条原则)

- **P1 扩展性**:`SandboxBackend` trait 可插拔;新增 OS 原语隔离 backend。
- **P2 安全作为不变量**:**默认 `restricted` profile**:workspace 内可写、workspace 外只读/拒绝、网络默认禁用(可配白名单)、敏感路径强制 deny-write。
- **P3 增量**:`off` profile 等价当前 local 行为;现有 Cube 配置不变。
- **P4 证据**:现状见 §1;借鉴见 §6。
- **P5 可观测**:隔离可用性、违规尝试产生 `tracing` + doctor 展示。
- **P6 失败显式**:平台不支持隔离时 `fail-if-unavailable`(借鉴 Claude Code `getSandboxUnavailableReason`)。
- **P7 测试**:各 profile 隔离边界、逃逸尝试、平台检测各有测试。

---

## 3. 架构设计

### 3.1 三级 profile(借鉴 Claude Code 命令路由 + 因地制宜)

```
SandboxProfile
   ├── Off           — 不隔离(当前 local 行为,需显式 opt-in)
   ├── Restricted    — OS 原语轻量隔离(默认,Linux namespace/macOS sandbox-exec)
   └── Cube          — 远程 MicroVM(已有,强隔离但重)
```

借鉴 Claude Code 04e §3 `shouldUseSandbox()`:全局开关 + 单次 `dangerouslyDisableSandbox` + `excludedCommands`(仅便利,非安全边界)。legion 的路由:`config.tools.exec.sandbox.mode` 全局 + 工具调用级 override。

### 3.2 平台分叉(Rust 取舍点)

| 平台 | Restricted 实现 | 库 |
|---|---|---|
| **Linux** | `unshare`(mount/pid/net/user namespace)+ bind mount rootfs + seccomp BPF | `nix` crate |
| **macOS** | `sandbox-exec`(系统自带,plist 沙箱配置) | 调系统二进制,无额外依赖 |
| **Windows** | 无轻量原生方案 → fallback `Cube` 或 restricted token(复杂,Phase C) | — |

这是与 Claude Code 的关键差异:Claude Code 依赖外部 sandbox runtime;legion 用 OS 原语实现本地隔离。

### 3.3 逃逸防护清单(全盘借鉴 Claude Code 04e §5,legion 化)

```
强制 denyWrite(任何 profile):
  - ~/.legion/legion.json, auth-profiles.json(配置与凭证)
  - ~/.ssh/, ~/.gnupg/(密钥)
  - workspace 外路径(除显式 writable 白名单)

专项防护:
  - git bare repo 逃逸专防(防 cwd 植入伪造 .git + core.fsmonitor 触发宿主 git)
    → 执行前 scrubBareGitRepoFiles() 检查
  - 网络域名从 web_fetch 权限反推 sandbox allowlist(保持上下一致)
```

### 3.4 平台检测 + fail-if-unavailable(借鉴 04e §7)

```
Gateway 启动 / legion doctor:
  sandbox_available(profile) → Ok | Err(SandboxUnavailableReason)
  例:Linux 无 CAP_SYS_ADMIN、macOS 无 sandbox-exec、Cube 不可达
  → Doctor UI 展示,配置 restricted 但平台不支持 → fail-fast 而非静默降级
```

---

## 4. 接口设计(Rust)

### 4.1 Profile 与能力声明(`legion-tools/src/sandbox`)

```rust
use async_trait::async_trait;
use std::path::PathBuf;

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SandboxMode { Off, Restricted, Cube }

#[derive(Debug, Clone, serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SandboxConfig {
    pub mode: SandboxMode,                // 默认 Restricted
    pub scope: SandboxScope,              // 默认 Shared
    #[serde(default)]
    pub restricted: RestrictedConfig,
    #[serde(default)]
    pub cube: CubeConfig,
}

#[derive(Debug, Clone, Copy, serde::Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SandboxScope { #[default] Shared, PerAgent, PerSession }

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RestrictedConfig {
    pub rootfs: Option<PathBuf>,          // None=使用临时 rootfs
    pub writable_paths: Vec<PathBuf>,     // workspace 白名单
    pub read_only_paths: Vec<PathBuf>,
    pub network: NetworkPolicy,           // 默认 None(禁网)
    pub env_whitelist: Vec<String>,
    pub seccomp: SeccompLevel,            // 默认 Basic
}

#[derive(Debug, Clone, serde::Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum NetworkPolicy { #[default] None, Allowlist(Vec<String>) }

#[derive(Debug, Clone, Copy, serde::Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SeccompLevel { Off, #[default] Basic, Strict }
```

### 4.2 SandboxBackend trait 扩展(能力声明 + 可用性检测)

```rust
#[async_trait]
pub trait SandboxBackend: Send + Sync {
    async fn exec(&self, cmd: &str, opts: ExecOptions) -> Result<ExecResult, SandboxError>;

    /// 声明此 backend 支持哪些隔离维度(供 doctor/路由判断)。
    fn capabilities(&self) -> SandboxCapabilities;
}

#[derive(Debug, Clone, Default)]
pub struct SandboxCapabilities {
    pub filesystem_isolation: bool,
    pub network_isolation: bool,
    pub process_isolation: bool,
    pub reusable: bool,          // 是否支持 sandbox 复用(Cube 应支持)
}

/// 启动期检测:当前平台能否提供该 profile 的隔离。
pub fn sandbox_available(mode: SandboxMode) -> Result<(), SandboxUnavailableReason>;

#[derive(Debug, thiserror::Error)]
pub enum SandboxUnavailableReason {
    #[error("Linux namespace requires CAP_SYS_ADMIN or userns")]
    LinuxNamespaceUnavailable,
    #[error("macOS sandbox-exec not found")]
    MacosSandboxExecMissing,
    #[error("Cube backend unreachable: {0}")]
    CubeUnreachable(String),
    #[error("platform {0} has no native restricted sandbox")]
    UnsupportedPlatform(String),
}
```

### 4.3 Restricted backend(Linux 示意)

```rust
#[cfg(target_os = "linux")]
pub struct LinuxNamespaceSandbox { cfg: RestrictedConfig }

#[cfg(target_os = "linux")]
#[async_trait]
impl SandboxBackend for LinuxNamespaceSandbox {
    async fn exec(&self, cmd: &str, opts: ExecOptions) -> Result<ExecResult> {
        // 1. unshare(CLONE_NEWNS | CLONE_NEWPID | CLONE_NEWNET | CLONE_NEWUSER)
        // 2. bind mount rootfs,pivot_root
        // 3. 应用 writable/read_only 路径策略
        // 4. 加载 seccomp BPF filter
        // 5. network: Allowlist → 用 netns + iptables/nftables 规则;None → 空 netns
        // 6. exec cmd in namespace
    }
    fn capabilities(&self) -> SandboxCapabilities {
        SandboxCapabilities { filesystem_isolation: true, network_isolation: true,
                              process_isolation: true, reusable: false }
    }
}
```

### 4.4 逃逸防护

```rust
/// 执行前检查:防 git bare repo 逃逸 / 敏感路径写入。
pub fn pre_exec_guard(cmd: &str, cwd: &Path, cfg: &RestrictedConfig) -> Result<(), SandboxError> {
    scrub_bare_git_repo(cwd)?;                       // 借鉴 Claude 04e §5
    deny_sensitive_writes(cmd, cfg)?;                // 凭证/配置路径
    Ok(())
}
```

### 4.5 配置 schema(`legion-core`)

```jsonc
// legion.json
{
  "tools": {
    "exec": {
      "sandbox": {
        "mode": "restricted",            // off | restricted | cube
        "scope": "perAgent",
        "restricted": {
          "writablePaths": ["${WORKSPACE}"],
          "network": { "type": "allowlist", "domains": ["api.example.com"] },
          "seccomp": "basic"
        }
      }
    }
  }
}
```

---

## 5. 集成点

| 位置 | 改动 |
|---|---|
| `legion-tools/src/sandbox/local.rs` | 保留为 `Off` backend;新增 `LinuxNamespaceSandbox`(linux)/ `MacosSandboxExecSandbox`(macOS)作 `Restricted` backend。 |
| `legion-tools/src/sandbox/mod.rs:55-63` | `SandboxBackend` trait 增加 `capabilities()`;新增 `sandbox_available()`。 |
| `legion-tools/src/sandbox/cube.rs` | 增 sandbox 复用(连接池)、网络白名单、volume 挂载、压缩帧支持。 |
| `legion-tools/src/registry.rs:17-43` | sandbox backend 选择按 `mode` + 平台可用性;不可用时 fail-fast。 |
| `legion-cli` | `legion doctor` 展示 sandbox 可用性与 profile。 |
| `legion-core/src/config.rs` | `tools.exec.sandbox` schema 升级(默认 `restricted`)。 |

---

## 6. 风险与权衡

### 6.1 平台分叉的复杂度
Linux namespace + seccomp 用 `nix` crate 实现成本中等;macOS `sandbox-exec` 是调系统二进制 + 写 plist,较简单;Windows 无轻量方案。**取舍**:Phase A 先做 Linux + macOS;Windows fallback 到 `Cube`,Phase C 评估 restricted token。

### 6.2 Restricted vs Cube 的选择
- `Restricted`:本地、快、轻,但共享内核(隔离弱于 MicroVM)。
- `Cube`:强隔离(独立内核),但重、慢、需远程服务。
**默认**:个人/单机用 `Restricted`;多用户/不可信代码用 `Cube`。借鉴 Claude 04e §4 的"语义翻译"——把 permission 路径规则翻译成 sandbox filesystem 限制。

### 6.3 fail-if-unavailable(安全核心)
借鉴 Claude 04e §7:配置 `restricted` 但平台不支持时,**fail-fast 而非静默降级到 Off**。否则用户以为开了隔离实际没开(典型 security footgun)。`legion doctor` 显式展示。

### 6.4 excludedCommands 非安全边界(借鉴 04e §3)
Claude Code 明确 `excludedCommands` 只是便利(让某些命令绕过 sandbox),**不是安全边界**。legion 文档须同样警示:绕过 sandbox 的命令承担全部风险。

### 6.5 sandbox 复用(Cube 性能)
当前每次 exec 都 create+kill 新 sandbox,开销大。借鉴连接池:per-session 复用一个 sandbox,session 结束才 kill。`capabilities().reusable` 标记。

### 6.6 网络白名单与 web_fetch 权限反推(借鉴 04e §4.5)
sandbox 网络白名单应与 `web_fetch` 的域名权限一致——否则 agent 能通过 `exec curl` 绕过 `web_fetch` 的域名限制。启动时从 `web_fetch` 权限反推 sandbox allowlist。

---

## 7. 实现路线图

### 阶段 A(Phase A,~1.5 人周):Restricted backend(Linux)
1. `SandboxMode`/`SandboxCapabilities`/`sandbox_available()` 类型。
2. `LinuxNamespaceSandbox`(unshare + bind mount + seccomp basic)。
3. 逃逸防护清单(敏感路径 deny-write、git bare repo scrub)。
4. `off` = 当前 local 行为;默认 `restricted`。
5. **验收**:`exec` 在 restricted 下无法写 workspace 外路径、无网络(隔离测试)。

### 阶段 B(Phase A,~1 人周):macOS + doctor
1. `MacosSandboxExecSandbox`(sandbox-exec + plist 生成)。
2. `legion doctor` 展示 sandbox 可用性;fail-if-unavailable。
3. 网络白名单(Allowlist)。
4. **验收**:macOS restricted 隔离生效;doctor 正确报告不可用原因。

### 阶段 C(Phase B,~1 人周):Cube 增强 + scope
1. Cube sandbox 复用(连接池)、volume 挂载、压缩帧。
2. `SandboxScope`(perAgent/perSession)复用语义。
3. `web_fetch` 权限反推 sandbox allowlist。
4. **验收**:Cube exec 性能因复用提升;scope 隔离正确。

### 阶段 D(Phase C,研究):Windows
- 评估 Windows restricted token / WSL 隔离方案。暂不承诺。

---

## 8. 验收标准

- [x] 默认 `mode=restricted` 时,本地 `exec` 无法写 workspace 外文件(Linux `bwrap` / macOS `sandbox-exec` 隔离,回归测试通过;实际隔离测试需在对应 OS 环境运行)。
- [x] 默认 `network=None` 时,`exec` 无法发起网络请求(profile 级别已禁用;实际隔离测试需在对应 OS 环境运行)。
- [x] 凭证路径(`~/.legion/legion.json`、`auth-profiles.json`、`~/.ssh`)强制 deny-write(`pre_exec_guard` 拒绝 + 后端隔离)。
- [x] git bare repo 逃逸防护生效(scrub 测试)。
- [x] 配置 `restricted` 但平台不支持时显式失败(通过 `UnavailableBackend` 在 exec 时返回错误,非静默降级)。
- [x] `legion doctor` 展示 sandbox 可用性与原因(`legion-cli` 已接入 `sandbox_available`)。
- [x] `mode=off` 等价当前 local 行为(回归测试通过)。
- [ ] Cube backend 支持 sandbox 复用、网络白名单、volume 挂载(Phase C)。
- [ ] `web_fetch` 域名权限与 sandbox allowlist 一致(反推测试,Phase C)。
- [x] 违规尝试有 `tracing` 事件。
- [x] `AGENTS.md` 更新沙箱章节。

---

*上一篇:[`approval-loop.md`](./approval-loop.md) · 下一个 gap:[`memory-layers.md`](./memory-layers.md) · 返回总览:[`../00-overview.md`](../00-overview.md)*
