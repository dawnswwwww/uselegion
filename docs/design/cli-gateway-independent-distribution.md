# CLI / Gateway 独立发布、按需安装与兼容性设计

> **状态**：已完成 Phase B/C/D（2026-07-14）  
> **优先级**：P1（建立在 Host / Protocol 分层之后）  
> **依赖**：[Host / Protocol 分层迁移计划](./host-protocol-extraction-plan.md) Phase 0–3 已完成  
> **目标**：`legion-cli` 与 `legion-gateway` 可作为独立产物发布；CLI 在用户明确启动本地 Gateway 时，能够安全地解析、下载、校验、安装、启动并升级兼容的 Gateway。  
> **当前决策**：Phase 0–3（crate 边界拆分）已完成；CLI 通过子进程启动 `legion-gateway` binary，不再链接 Gateway server 代码。Phase B（独立二进制 + 本地发现）、Phase C（签名 manifest + 按需安装）、Phase D（升级回滚 + 迁移 ledger）已落地。

---

## 1. 决策摘要

采用“**独立产物、同一发布列车、显式协议兼容**”的模式：

```text
用户安装 legion CLI
        │
        ├─ local / embedded 模式：仅使用 legion-host，不需要 Gateway
        │
        └─ legion gateway start
              ├─ 找到本地兼容 Gateway → 启动或复用已运行实例
              ├─ 没找到 → 在许可的情况下下载已签名的兼容版本 → 原子安装 → 启动
              └─ 版本不兼容 → 显示结构化原因，要求 upgrade / downgrade / remote 模式选择
```

“独立”不等于任意版本可以互通。每个 release 同时发布 CLI 与 Gateway，并通过协议版本范围、能力声明和签名 manifest 明确它们能否配对。CLI **不得**在用户没有发起 `gateway install` 或 `gateway start --install` 的情况下后台下载或升级 Gateway。

---

## 2. 背景与现状

当前 CLI 同时支持 Gateway 和 embedded 两种模式，但 embedded mode 复用 `legion-gateway::host::AgentHost`：

- `crates/legion-cli/Cargo.toml:12` 直接依赖 `legion-gateway`；
- `crates/legion-cli/src/driver.rs:15-18` 引入 Gateway 的 `WsFrame`、agent RPC 和 `AgentHost`；
- `crates/legion-gateway/src/host.rs:44-64` 实际承担跨 transport 的运行时组合根；
- `crates/legion-gateway/src/gateway.rs:61-174` 才是 HTTP/WS、渠道与自动化服务的生命周期层。

这使 CLI 安装/编译时不可避免地带入 Gateway 的服务端依赖，也使单独发布缺少稳定协议与安装边界。

本设计以 Host / Protocol 分层为前提：

```text
legion-cli ──→ legion-host + legion-protocol
legion-gateway ──→ legion-host + legion-protocol
```

完成该分层后，CLI 不需要链接或携带 Gateway 才能运行 local mode；Gateway 则可作为独立下载的本地服务端二进制发布。

---

## 3. 目标与非目标

### 3.1 目标

1. 用户可只安装 CLI，并使用 embedded/local mode 完成 agent turn。
2. Gateway 作为独立、可签名校验的可执行产物发布。
3. `legion gateway start` 能确定性地选中一个与当前 CLI 兼容的平台产物。
4. CLI 与 Gateway 的协议兼容性由机器可读范围判断，而不是仅打印版本警告。
5. 安装、切换与升级原子化；失败不破坏已知可用版本。
6. 支持离线、受代理限制和受控企业环境下的预安装/内部镜像。
7. Gateway 更新不丢失配置、sessions、SQLite memory、cron/task 数据，且可检测不可逆迁移。

### 3.2 非目标

- 不实现任意远程机器的自动部署；远程 Gateway 由管理员独立部署，CLI 只连接。
- 不实现静默后台自动更新。
- 不把 API key、auth profile 或 `~/.legion` 数据打包进下载产物。
- 不让 CLI 执行来自 manifest 的 shell 命令、安装脚本或任意 post-install hook。
- 不在第一期实现增量/差分更新；完整 artifact 下载优先保证可审计性。

---

## 4. 产物、crate 与包边界

### 4.1 Rust crate 边界

| crate | 发布角色 | 说明 |
|---|---|---|
| `legion-protocol` | 共享库 | WebSocket DTO、版本/能力协商类型；CLI 和 Gateway 共同依赖。 |
| `legion-host` | 共享库 | runtime composition、session turn service；CLI embedded 与 Gateway 共同依赖。 |
| `legion-cli` | 用户入口二进制 | TUI、命令、embedded mode、Gateway client、安装管理器。 |
| `legion-gateway` | 服务端库 + `legion-gateway` 二进制 | HTTP/WS、channels、automation 生命周期和 daemon server。 |

`legion-cli` 的 Cargo 依赖图终态中不得有 `legion-gateway`。`legion gateway start` 操作的是磁盘上的 Gateway artifact，而不是将 Gateway server 作为 CLI 的库函数启动。

### 4.2 发布物

每个 release channel（`stable`、可选 `beta`、可选 `nightly`）发布：

```text
legion-cli-<version>-<target>.tar.gz / .zip
legion-gateway-<version>-<target>.tar.gz / .zip
manifest-v1.json
manifest-v1.json.sig
checksums.txt              # 仅作人工核验；机器信任以签名 manifest 为准
```

`target` 使用 Rust target triple，例如：

```text
aarch64-apple-darwin
x86_64-apple-darwin
x86_64-unknown-linux-gnu
aarch64-unknown-linux-gnu
x86_64-pc-windows-msvc
```

可提供一个可选的“CLI + 同版本 Gateway”离线 bundle；它只是便利安装包，内部仍按同一安装与校验规则展开。

### 4.3 同一发布列车

CLI 与 Gateway 可以独立安装、独立修补发布，但常规 release 必须从同一 Git tag 构建并使用相同 release id。版本匹配不是唯一规则，真正的连接条件是：

```text
artifact 签名可信
AND target 匹配
AND CLI 支持 Gateway 的 protocol revision
AND Gateway 支持 CLI 的 protocol revision
AND 请求的 capability 已协商
```

---

## 5. 协议兼容性设计

### 5.1 不以 crate 版本代替协议版本

当前 Gateway hello 中包含 crate `version`，CLI 可据此提示 stale Gateway；该机制不足以支持独立发布。新增独立、整数化的 protocol revision 和兼容范围。

建议在 `legion-protocol` 定义：

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProtocolCompatibility {
    pub protocol_revision: u32,
    pub min_peer_revision: u32,
    pub max_peer_revision: u32,
    pub product_version: String,
    pub release_id: String,
    pub capabilities: Vec<String>,
}
```

握手中 CLI 发送自身 `ProtocolCompatibility`；Gateway hello 回传自身范围和允许的 capability。双方只有在各自 revision 落入对方范围内时才继续会话。

### 5.2 兼容性规则

| 情况 | 行为 |
|---|---|
| revision 双向兼容，所有基础 capability 可用 | 正常连接。 |
| revision 兼容，但新 capability 未协商 | 基础功能继续；调用该 capability 时返回明确 `capability_not_supported`。 |
| CLI 太旧 | Gateway 返回 `cli_upgrade_required`，包括 `minCliVersion` 与建议安装版本。 |
| Gateway 太旧 | CLI 不发送业务请求；显示 `gateway_upgrade_required`，可提供 `gateway upgrade`。 |
| protocol range 不相交 | 拒绝连接；不尝试“猜测兼容”。 |
| release id 不同但 revision 兼容 | 允许连接，同时在 `legion gateway status` 给出可见提示。 |

兼容性协商只在已完成 transport authentication 后返回详细信息，避免匿名探测暴露部署版本。

### 5.3 Capability 而非版本分支

新 RPC method、event 或可选子系统必须注册 capability，例如：

```text
agent.run.v1
sessions.history.v1
approval.resolve.v1
flows.run.v1
nodes.invoke.v1
```

CLI 在调用前检查 capability。Gateway 也必须在 server 端校验，不能因客户端漏检而执行未协商路径。

### 5.4 协议演进纪律

1. 仅新增 optional 字段、method、event 或 capability 时，可保持 revision 范围兼容。
2. 修改字段语义、删除字段、改变认证或事件顺序时，提升 protocol revision，并明确最低/最高 peer revision。
3. 废弃 capability 至少保留一个 stable release 周期。
4. 每次 protocol 改动必须在 `legion-protocol` 增加 JSON fixture 与旧版兼容测试。

---

## 6. 发行 manifest 与信任链

### 6.1 Manifest 内容

CLI 不得根据“最新版本网页”或重定向 URL 决定下载内容。它只消费签名的、版本化 manifest。

示例（字段名为设计草案）：

```json
{
  "formatVersion": 1,
  "channel": "stable",
  "publishedAt": "2026-07-14T00:00:00Z",
  "releases": [
    {
      "releaseId": "2026.07.14-0.2.0",
      "cliVersionRange": ">=0.2.0 <0.3.0",
      "gatewayVersion": "0.2.0",
      "protocol": { "minPeerRevision": 1, "maxPeerRevision": 1 },
      "artifacts": [
        {
          "target": "aarch64-apple-darwin",
          "url": "https://releases.example/legion-gateway-0.2.0-aarch64-apple-darwin.tar.gz",
          "sha256": "…",
          "sizeBytes": 12345678
        }
      ]
    }
  ]
}
```

实际 manifest 应额外包含 artifact 文件名、压缩格式和最小 CLI 版本；不得包含可执行脚本或命令字段。

### 6.2 签名与密钥轮换

1. 使用 Ed25519 对 manifest 的规范化字节进行签名。
2. CLI 内置 stable channel 的根公钥；下载后先验签 manifest，再核验 artifact SHA-256 和大小。
3. CLI 只允许 HTTPS；拒绝非 HTTPS、跨 host 重定向、超出下载大小上限和未知压缩格式。
4. 公钥轮换通过当前受信任密钥签名的 `nextKeys` 记录完成；旧 key 留足够 overlap 周期。
5. `--manifest-url` / `--channel-url` 仅允许在显式配置的企业镜像场景使用，并在 status 输出其来源；自定义信任根需要单独配置文件与明确用户授权。
6. 下载与校验日志不得包含 token、代理密码、Authorization header 或完整私有 URL query。

### 6.3 下载失败策略

| 失败 | 行为 |
|---|---|
| DNS / 网络 / 代理错误 | 保留当前已安装版本；提示 `gateway install --from <file>` 或企业 mirror 配置。 |
| manifest 签名无效 | 硬失败，不尝试 artifact 下载。 |
| checksum / size 不匹配 | 删除临时文件，硬失败，保留已安装版本。 |
| 当前平台没有 artifact | 硬失败，显示 target triple 与手动安装说明。 |
| 磁盘空间不足 | 安装前预检；不替换 current version。 |

---

## 7. 本地安装布局与原子性

### 7.1 目录布局

所有下载的 Gateway 版本位于用户状态目录，而不是 CLI 自身安装目录：

```text
~/.legion/
├── gateways/
│   ├── 0.2.0/
│   │   └── aarch64-apple-darwin/
│   │       ├── legion-gateway
│   │       └── install.json
│   └── 0.2.1/
│       └── aarch64-apple-darwin/…
├── gateway-current.json
├── downloads/
│   └── *.partial
├── locks/
│   ├── gateway-install.lock
│   └── gateway-daemon.lock
└── agents/ …                # 既有用户数据，永不随升级删除
```

Windows 使用等价的可写 application data 目录和 `.exe` 文件名。路径必须通过平台 API 计算，禁止拼接 shell 命令。

### 7.2 安装事务

```text
resolve manifest
  → verify signature
  → select compatible release + target
  → acquire install lock
  → download to unique .partial
  → verify size + SHA-256
  → unpack into unique staging directory
  → verify expected executable only、权限、版本 self-check
  → atomic rename to version directory
  → atomically replace gateway-current.json
  → retain prior known-good versions
```

`gateway-current.json` 是小型指针文件，记录 `version`、`target`、`releaseId`、安装时间和最后一次成功启动状态；使用临时文件 + rename 写入。不能依赖 symlink，因为 Windows 与权限环境的行为不一致。

### 7.3 保留与清理

- 默认保留：current、previous known-good、用户显式 pin 的版本；其余版本最多保留 2 个。
- `legion gateway prune` 只删除未运行、未 pin 的版本。
- 不自动清理失败证据（manifest、失败原因）以外的用户数据。
- 若磁盘空间紧张，先报告可释放版本并要求显式 `prune`，不静默删除回滚点。

---

## 8. CLI 命令与用户体验

### 8.1 建议命令面

```text
legion gateway status
legion gateway install [--version <v>] [--channel stable] [--from <archive>]
legion gateway start [--install] [--version <v>] [--foreground]
legion gateway upgrade [--to <v>] [--restart]
legion gateway rollback [--to <v>]
legion gateway list-versions
legion gateway prune
legion gateway doctor
```

### 8.2 自动下载策略

安全和可预期性优先：

- 交互终端执行 `legion gateway start`，未安装兼容版本时显示目标版本、来源、大小与签名状态，并请求确认；用户确认后下载。
- `legion gateway start --install` 明确授权本次下载。
- 非交互环境（CI、脚本、service installer）默认不下载；必须传 `--install` 或预先执行 `gateway install`。
- 配置项 `gateway.autoInstall` 默认 `false`；管理员可在受控环境显式开启，但 CLI 仍必须输出下载来源与版本。
- CLI 启动时、普通 `legion agent` 或 TUI embedded mode 不触发背景下载。

### 8.3 已运行 Gateway 的处理

`gateway start` 按以下顺序判断：

1. 探测配置地址是否已有 Gateway。
2. 完成认证后的 protocol handshake。
3. 已运行且兼容：复用，输出 PID / version / endpoint。
4. 已运行但不兼容：不覆盖、不强杀；返回升级建议。用户必须执行 `gateway upgrade --restart` 或显式使用另一个 endpoint。
5. 未运行：选择本地兼容已安装版本；缺失时按 8.2 安装；随后启动。

这避免一个新 CLI 无意中中断正在处理工具调用、cron 或 channel 连接的旧 Gateway。

### 8.4 离线与企业网络

| 场景 | 支持方式 |
|---|---|
| 完全离线 | `gateway install --from /path/to/artifact`；artifact 仍需关联已信任 manifest 或随 bundle 带签名 manifest。 |
| 内网镜像 | 管理员配置 manifest URL + 企业信任根，CLI status 明示 mirror。 |
| HTTP(S) 代理 | 使用标准环境变量/显式 proxy 配置；日志脱敏。 |
| 预装系统包 | Gateway 位于受管路径时允许 `gateway.path` 显式配置；仍执行 `--version --json` 和 protocol handshake。 |

---

## 9. Gateway 生命周期、升级与回滚

### 9.1 守护进程所有权

Gateway 自身继续拥有 channels、cron、heartbeat、task runner、MCP connections 的生命周期；CLI 只负责安装、启动请求、状态与受控停止。

需要引入跨平台 daemon metadata（JSON + lock），至少记录：

```text
pid, executable path, gateway version, protocol revision,
started_at, endpoint, config path hash, release id
```

PID 仅是辅助信息；最终健康判断必须以 authenticated protocol handshake 为准，避免 PID 重用问题。

### 9.2 升级流程

```text
install and verify target version
  → verify no incompatible configuration migration is pending
  → request old gateway drain mode
  → stop accepting new agent turns / wait bounded time for active turns
  → stop channels and background loops cleanly
  → start target version
  → authenticated health + protocol check
  → mark target known-good
```

若 drain 超时，不默认强杀；CLI 显示活动运行数并要求 `--force`。`--force` 必须记录 audit/tracing 事件，因其可能中断 agent turn 或工具执行。

### 9.3 启动失败与回滚

- 新版本启动、health 或 handshake 失败：保留失败日志，将 `gateway-current.json` 回指 previous known-good，尝试启动旧版本一次。
- 旧版本同样失败：不循环重试；输出诊断文件位置和 `gateway doctor` 建议。
- 手动 `gateway rollback` 只能切换已安装、兼容当前 CLI 的版本；不下载未知旧版。

---

## 10. 用户数据与 schema 迁移

Gateway binary 与用户状态分离。升级时必须兼容或显式迁移以下数据：

```text
~/.legion/legion.json / .json5
~/.legion/agents/*/sessions/*.jsonl
~/.legion/agents/*/memory/* (SQLite / sqlite-vec)
~/.legion/agents/*/agent/auth-profiles.json
~/.legion/automation/* (cron/task JSONL)
```

### 10.1 迁移规则

1. 每个可持久化 schema 增加显式版本或 migration ledger；不能仅靠文件存在与字段猜测。
2. 可逆、向后兼容变更可在启动时完成。
3. 不可逆迁移启动前必须创建限定范围备份，并写入 migration ledger。
4. Gateway 应在 hello/status 声明其可读取的数据 schema 范围；CLI 的 `gateway upgrade` 在停止旧 daemon 前做预检。
5. 降级到不支持当前 schema 的 Gateway 必须被拒绝，除非用户通过明确的 restore/rollback 数据操作完成回退。
6. 不迁移或复制 API key；文件权限继续遵循现有 0600 约束。

### 10.2 并发保护

安装锁与 daemon 锁分离：下载可互斥，数据 migration/daemon start 必须全局串行。CLI 和已运行 Gateway 不得同时对同一 memory SQLite 或 JSONL schema 做升级写入。

---

## 11. 可观测性与错误模型

### 11.1 事件与状态

安装管理器记录结构化事件：

```text
gateway_manifest_verified
gateway_artifact_selected
gateway_download_started / completed / failed
gateway_install_committed
gateway_daemon_started / drained / stopped
gateway_protocol_incompatible
gateway_rollback_completed
```

字段包括 release id、version、target、channel、耗时与错误分类；不含密钥、用户 prompt、auth header。

### 11.2 错误分类

| 分类 | 用户动作 |
|---|---|
| `ManifestUntrusted` | 检查 channel / 企业信任根；不可绕过为普通下载。 |
| `ArtifactIntegrityFailed` | 重新下载或切换镜像；保留已知可用版本。 |
| `PlatformUnsupported` | 手动安装匹配 artifact 或使用 local mode。 |
| `OfflineOrProxy` | 预安装、离线 bundle 或配置 mirror。 |
| `ProtocolIncompatible` | 升级 CLI 或 Gateway，不能强行连接。 |
| `DataMigrationBlocked` | 备份/完成迁移，或继续使用旧 Gateway。 |
| `DaemonBusy` | 等待、drain 或显式 `--force`。 |

`legion gateway doctor` 汇总 installed versions、current pointer、运行实例、protocol range、数据 schema、磁盘空间、下载源和最近一次失败原因。

---

## 12. 实施阶段

### Phase A：解除编译期耦合

依赖 [Host / Protocol 分层迁移计划](./host-protocol-extraction-plan.md)：

1. 提取 `legion-protocol`，固定 WebSocket DTO 与 protocol compatibility 类型。
2. 提取 `legion-host`，让 embedded CLI 不引用 Gateway 实现。
3. 使 `cargo tree -p legion-cli` 不包含 `legion-gateway`。
4. 为 CLI embedded / Gateway 的 event 与 transcript 等价性建立回归测试。

**完成条件**：可单独构建 CLI 与 Gateway artifact；尚不下载。

### Phase B：独立 Gateway 二进制与本地发现 ✅ 已落地

1. 提供 `legion-gateway` standalone binary，支持 `--version --json`、`--config`、foreground/daemon 所需最小参数。
2. CLI 实现 `gateway.path`、版本探测、authenticated handshake、`status` 和 `list-versions`。
3. 添加 `ProtocolCompatibility` 协商；当前“版本不一致只警告”变为按 range 的可操作错误。
4. 支持用户手动将 Gateway 放入版本目录或通过 `gateway install --from` 导入。

**完成条件**：完全离线的独立安装与兼容启动可用。  
**实现位置**：`crates/legion-protocol/src/compatibility.rs`、`crates/legion-gateway/src/main.rs`、`crates/legion-gateway/src/websocket.rs`、`crates/legion-cli/src/gateway_manager.rs`、`crates/legion-cli/src/lib.rs`、`crates/legion-cli/src/main.rs`。

### Phase C：签名 manifest 与按需安装 ✅ 已落地

1. 建立 release pipeline，生成 manifest、signature、checksums 和每平台 artifact。
2. 实现 CLI manifest fetch、Ed25519 验签、artifact SHA-256/size 校验、staging + atomic rename。
3. 实现交互确认、`--install`、non-interactive fail-closed、企业 mirror。
4. 实现安装锁、known-good pointer 与 `doctor`。

**完成条件**：新机器仅安装 CLI 后，可由用户明确触发下载并启动兼容 Gateway。  
**实现位置**：`crates/legion-protocol/src/manifest.rs`、`crates/legion-cli/src/gateway_manager.rs`（`fetch_verified_manifest`、`download_artifact`、`install_from_manifest`、`verify_manifest_signature`）。

### Phase D：受控升级、回滚与数据迁移 ✅ 已落地（MVP）

1. 实现 Gateway drain/health lifecycle API 与 active turn 计数。
2. 实现 `upgrade`、`rollback`、`prune`，以及失败时自动回到 previous known-good。
3. 为 JSONL/SQLite/config 引入 schema/migration ledger 与备份预检。
4. 补齐稳定/beta channel、密钥轮换和 release incident 操作手册。

**完成条件**：兼容升级可自动恢复；不兼容数据迁移明确阻止而非静默损坏。  
**实现位置**：`crates/legion-cli/src/gateway_manager.rs`（`upgrade`、`rollback`、`prune`、`running_gateway_info`、`doctor`、`migration.jsonl`）。当前 migration compatibility 检查为 MVP（schema 恒为 1，视为可逆），真实不可逆迁移检测待后续数据 schema 版本化后补强。

---

## 13. 测试与验收矩阵

| 场景 | 测试类型 | 断言 |
|---|---|---|
| 旧/新 protocol JSON | `legion-protocol` fixture | serde 兼容、range 判断、capability gate。 |
| Manifest 被篡改 | 单元/集成 | 验签失败后不创建任何安装目录。 |
| Artifact checksum 错误 | 集成 | `.partial` 被删除，current pointer 不变。 |
| 下载中断 | 集成 | 下次可重试；不执行半下载文件。 |
| 两个 CLI 并发安装 | 集成 | 单一 winner，目录与 pointer 一致。 |
| 已运行兼容 Gateway | CLI 集成 | 不重启，复用现有 daemon。 |
| 已运行不兼容 Gateway | CLI 集成 | 无强杀、无业务请求，给出 upgrade 动作。 |
| 新 Gateway 启动失败 | 集成 | 自动回滚 previous known-good 一次。 |
| offline bundle | 集成 | 无网络访问也可验签并安装。 |
| 企业 mirror / 自定义信任根 | 集成 | 只信任显式配置，status 展示来源。 |
| 数据迁移拒绝降级 | 集成 | schema 不兼容时拒绝启动旧版本。 |
| CLI embedded 与 Gateway mode | 端到端 | agent event JSON 与 transcript 语义一致。 |

完成命令：

```bash
cargo fmt -- --check
cargo clippy --workspace --all-targets
cargo test --workspace --all-targets
cargo build --workspace --all-targets
cargo tree -p legion-cli | rg 'legion-gateway'
# 无输出
```

---

## 14. 风险与取舍

| 取舍 | 选择 | 原因 |
|---|---|---|
| CLI 首次 `gateway start` 是否静默下载 | 否，要求确认或 `--install` | 网络下载和二进制执行是显著权限边界。 |
| “最新版本”选择 | 签名 manifest 中与 CLI 匹配的 release | 避免不兼容版本和供应链劫持。 |
| 不兼容运行中的 Gateway | 不强杀、不自动替换 | 保护正在执行的 agent/tool/channel 工作。 |
| 更新方式 | 全 artifact + 原子切换 | 第一版更易审计和回滚，牺牲下载量。 |
| 用户数据位置 | 版本目录外、跨版本共享 | 升级不丢数据，但必须有 schema migration 防线。 |
| release independence | 独立 artifact，保持同一 release train | 允许快速 server patch，同时控制兼容组合数量。 |

---

## 15. 完成定义

以下条件全部满足后，此设计视为完成：

- CLI 和 Gateway 是分别可下载、可校验、可运行的 artifact；
- CLI 的 Cargo 依赖树中没有 `legion-gateway`；
- `legion gateway start` 可发现、校验并在明确授权后安装兼容 Gateway；
- protocol range 与 capability 协商替代仅版本警告；
- 下载产物与 manifest 都经过签名/完整性校验，失败不破坏 known-good 版本；
- 离线导入、企业镜像、升级、回滚和 doctor 有可测试路径；
- Gateway 仍独占渠道与自动化后台生命周期；
- 用户数据 migration、降级限制和备份策略已经落地并通过自动化测试。

