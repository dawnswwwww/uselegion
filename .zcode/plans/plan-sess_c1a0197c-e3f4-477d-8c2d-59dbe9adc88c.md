# Legion 自动发布管线设计

## 决策摘要(已与用户确认)

| 维度 | 决定 |
|---|---|
| 命名 | crates.io `legion-cli` / Homebrew formula `legion`(tap `dawnswwwww/homebrew-legion`)/ npm `@uselegion/cli` / 二进制 `legion` |
| 发布产物 | `legion` (CLI) + `legion-gateway` (sidecar) 两个二进制 |
| 平台 | macOS aarch64+x86_64、Linux musl aarch64+x86_64、Windows x86_64-msvc(实验性),共 5 目标 |
| Linux 格式 | musl 静态链接,零运行时依赖 |
| npm | 平台子包(1 主包 + 5 平台包) |
| crates.io | 发布完整 11 个 lib crate + legion-cli |
| 管线 | 手写 GitHub Actions,零外部工具 |

## 关键发现(来自代码排查)

1. **SQLite 未 bundled**(`legion-memory/Cargo.toml:21-23`):`sqlx`/`sqlite-vec`/`libsqlite3-sys` 都没开 `bundled` 特性 → 静态 musl 二进制会缺库。**必须改**。
2. **WebSocket 走 native-tls/OpenSSL**(根 `Cargo.toml:62-65`,5 个 crate 消费)。reqwest 已是 rustls(`legion-provider/Cargo.toml:17`)。**改一行 workspace dep 即全部切换**。
3. **gateway 下载器已存在**(`gateway_manager/installer.rs`):Ed25519 签名 manifest + sha256 + target 选择 + 原子指针。但 `STABLE_RELEASE_PUBLIC_KEY` 是测试向量(`manifest.rs:5-14`),`default_manifest_url()` 返回 `None`(`lib.rs:713`)。**管线必须产出真签名密钥 + stable manifest host**。
4. **版本单一来源**:根 `Cargo.toml:24` `version = "0.1.0"`,所有 crate `version.workspace = true`,`legion --version` 经 clap derive 自动取 `CARGO_PKG_VERSION`。改一处即全链路更新。
5. **crates.io 缺元数据**:无 `description`/`repository`/`keywords`(发布必需),且 legion-cli 11 个 path dep 都要按序发布。
6. **无任何现有发布工具**:`.github/` 不存在,无 Makefile/justfile/CHANGELOG/release.toml。

---

## 实施步骤(8 步,按依赖顺序)

### 步骤 1:补全 crates.io 发布元数据

**改根 `Cargo.toml` `[workspace.package]`(第 23-28 行)**,增加发布必需字段:

```toml
[workspace.package]
version = "0.1.0"
edition = "2024"
license = "MIT"
authors = ["Legion Contributors"]
rust-version = "1.86"
description = "Self-hosted, multi-channel AI agent gateway and CLI"
repository = "https://github.com/dawnswwwww/legion"
homepage = "https://github.com/dawnswwwww/legion"
documentation = "https://github.com/dawnswwwww/legion#readme"
readme = "../../README.md"          # 每个 crate 指向根 README
keywords = ["ai", "agent", "llm", "gateway", "cli"]
categories = ["command-line-utilities", "development-tools"]
```

> 注:`readme` 在各 crate Cargo.toml 单独指向根 README 路径(workspace.package 不支持相对路径跨 crate,需逐个写)。legion-cli 的 `description` 可单独覆盖更精准的文案。

### 步骤 2:SQLite bundled 化(静态 musl 前提)

**改 `crates/legion-memory/Cargo.toml:21-23`**:

```toml
sqlx = { version = "0.8", features = ["sqlite", "runtime-tokio", "migrate"] }
sqlite-vec = "0.1"
libsqlite3-sys = { version = "0.30", features = ["bundled"] }
```

只加 `bundled` 特性 → libsqlite3-sys 会从源码编译 SQLite,静态链入二进制,目标机无需系统 SQLite。sqlite-vec 本就是源码编译。验证:`cargo build --workspace --all-targets` + `cargo test -p legion-memory`。

### 步骤 3:WebSocket 切 rustls(去掉 OpenSSL 依赖)

**改根 `Cargo.toml:62-65`**,把 `native-tls` 换成 rustls:

```toml
# `rustls-tls-native-roots` 提供 wss:// 支持,且静态链接(无系统 OpenSSL 依赖)。
# 用 native roots 以兼容企业自签 CA。
tokio-tungstenite = { version = "0.26", features = ["rustls-tls-native-roots"] }
```

5 个消费者(channel/cli/gateway/mcp/tools)全部自动切换,无需逐个改。验证:`cargo build --workspace` + Lark/Discord 等 wss 通道测试(若本地无 key,至少 `cargo test -p legion-channel`)。

> 步骤 2+3 完成后,`cargo tree -d | grep -E "openssl|native-tls"` 应为空。

### 步骤 4:生成真签名密钥 + 注入公钥

Ed25519 密钥对用于给 release manifest 签名,CLI 用 `STABLE_RELEASE_PUBLIC_KEY` 验签。

- **本地生成密钥对**(一次性,不入库):
  ```bash
  # 生成 32 字节 seed,派生 keypair
  openssl rand -hex 32 > .legion-release-key.seed   # 绝不入 git
  ```
  或用 `ed25519-dalek` 的 `SigningKey::generate(&mut rand)`。
- **公钥(32 字节)**写入 `crates/legion-protocol/src/manifest.rs:11` 替换测试向量,更新注释。
- **私钥/seed**存为 GitHub Actions secret `LEGION_RELEASE_SIGNING_KEY`(hex,64 字符)。密钥文件名加入 `.gitignore`。
- 文档:在 `docs/` 加 `release-signing-key.md` 说明私钥托管与轮换流程(谁持有、如何轮换、用户如何验证)。

> 现有 `installer.rs:449-497` 的测试用测试向量 key,改公钥常量后需同步更新测试 seed(或让测试自派生 keypair,不依赖常量)。

### 步骤 5:CI 工作流 `.github/workflows/ci.yml`(基线质量门)

PR/push 触发,跑 AGENTS.md 的四件套,仅在一个 ubuntu-22.04 runner 上(快):

```yaml
name: ci
on: { push: {branches: [main]}, pull_request: }
jobs:
  check:
    runs-on: ubuntu-22.04
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      - run: cargo fmt -- --check
      - run: cargo build --workspace --all-targets
      - run: cargo clippy --workspace --all-targets -- -D warnings
      - run: cargo test --workspace --all-targets
```

### 步骤 6:发布工作流 `.github/workflows/release.yml`(核心)

**触发**:`git tag v*.*.*` 推送(手动决定版本)。单一工作流串联全部渠道,分阶段 job。

#### Job 结构(8 个 job)

```
release.yml (tag v*.*.* 触发)
├── 1. version-check     # 校验 tag == Cargo.toml version,防漂移
├── 2. build-matrix      # 5 目标并行编译(矩阵),产出 .tar.gz
├── 3. github-release    # 依赖 build-matrix,上传 5 个资产到 Release
├── 4. manifest-sign     # 依赖 github-release,生成签名 manifest
├── 5. publish-crates    # 依赖 github-release,按序发布 11+1 crate
├── 6. publish-homebrew  # 依赖 github-release,更新 tap formula
├── 7. publish-npm       # 依赖 github-release,发布 6 个 npm 包
└── 8. notify            # 全部成功后发 GitHub Release summary
```

#### Job 1 version-check
```yaml
- 解析 tag 名(去 v 前缀)→ expected
- grep 根 Cargo.toml version → actual
- 不等则 fail(防 tag 与代码版本不一致)
```

#### Job 2 build-matrix(关键)
```yaml
strategy:
  fail-fast: false
  matrix:
    include:
      - { target: aarch64-apple-darwin,    os: macos-14,        archive: tar.gz }
      - { target: x86_64-apple-darwin,     os: macos-13,        archive: tar.gz }
      - { target: aarch64-unknown-linux-musl, os: ubuntu-22.04, archive: tar.gz, cross: true }
      - { target: x86_64-unknown-linux-musl,  os: ubuntu-22.04, archive: tar.gz, cross: true }
      - { target: x86_64-pc-windows-msvc,  os: windows-latest,  archive: zip }
steps:
  - rustup target add ${{ matrix.target }}
  - 若 cross: 用 cross 0.2.5 编译 musl 目标(容器内有 musl-gcc + 静态依赖)
  - cargo build --release --target $TARGET -p legion-cli   # 产出 legion 二进制
  - cargo build --release --target $TARGET -p legion-gateway  # 产出 legion-gateway 二进制
  - 打包:
      legion-${VERSION}-${TARGET}.tar.gz 内含 legion + legion-gateway + LICENSE + README
      (windows 用 zip)
  - 计算 sha256
  - upload-artifact 上传压缩包 + .sha256
```

musl 交叉用 `cross`(Docker 容器,内含 musl-gcc)。验证静态:
```bash
file target/*/release/legion        # 应含 "statically linked"
ldd target/*/release/legion 2>&1    # 应 "not a dynamic executable"
```

#### Job 3 github-release
```yaml
- 下载全部 5 个 artifact
- softprops/action-gh-release@v2 上传
- 同时上传源码 LICENSE/README/CHANGELOG(若有)
```

#### Job 4 manifest-sign(网关下载链路)
```yaml
- 下载 5 个 artifact 的 sha256 + size
- 用 Node 脚本(或 Rust xtask)构造 ReleaseManifest JSON:
    {
      formatVersion: 1, channel: "stable", publishedAt: <now>,
      releases: [{
        releaseId: "${VERSION}",
        cliVersionRange: ">=${MAJOR}.${MINOR}.0 <${MAJOR}.${NEXT_MINOR}.0",
        gatewayVersion: "${VERSION}",
        protocol: { minPeerRevision: <from crate>, maxPeerRevision: <from crate> },
        artifacts: [ {target, url, sha256, sizeBytes} ... ]
      }]
    }
  url 指向 https://github.com/dawnswwwww/legion/releases/download/v${VERSION}/legion-${VERSION}-${TARGET}.tar.gz
- 用 LEGION_RELEASE_SIGNING_KEY secret 做 Ed25519 签名 → manifest.json + manifest.json.sig
- 上传 manifest.json + .sig 到 GitHub Release(以及一个稳定路径)
- 稳定 host:用 GitHub Pages 或 release dawnswwwww/legion 仓库的 manifest.json
  → CLI 的 default_manifest_url() 改为返回该稳定 URL
```

**配套代码改动**:
- `crates/legion-cli/src/lib.rs:713` `default_manifest_url()` 返回稳定 URL(如 `https://dawnswwwww.github.io/legion/manifest.json` 或 raw.githubusercontent)。

#### Job 5 publish-crates(按依赖拓扑序)
```yaml
- cargo publish -p legion-core
- cargo publish -p legion-plugin-sdk
- cargo publish -p legion-protocol
- cargo publish -p legion-provider
- cargo publish -p legion-runtime
- cargo publish -p legion-memory
- cargo publish -p legion-tools
- cargo publish -p legion-channel
- cargo publish -p legion-automation
- cargo publish -p legion-skills
- cargo publish -p legion-mcp
- cargo publish -p legion-web
- cargo publish -p legion-acp
- cargo publish -p legion-cli        # 最后,依赖前面全部
- 用 CARGO_REGISTRY_TOKEN secret
- 每个 --token $TOKEN,失败即整体 fail(已发布过的版本会报 already exists,可用 --allow-dirty 或检查)
```

#### Job 6 publish-homebrew
```yaml
- 触发 dawnswwwww/homebrew-legion 仓库的工作流(Repository Dispatch 或直接 push commit)
- formula: brew install dawnswwwww/legion/legion
- 内容:从 GitHub Release 下载 aarch64+x86_64 darwin tar.gz + sha256
- 用 GitHub App token 或 PERSONAL_ACCESS_TOKEN 跨仓 push
```

**需你在 GitHub 创建空仓库 `dawnswwwww/homebrew-legion`**,内含 `Formula/legion.rb`。本仓库管 formula 模板,发布时自动提交更新。

#### Job 7 publish-npm(平台子包模式)
```yaml
- 目录结构(本仓库内 npm/ 子树,发布时发布):
    npm/
      cli/                      # @uselegion/cli (主包,带 postinstall 选择器)
        package.json            # "name": "@uselegion/cli", "bin": {"legion": "run.sh"}
        install.js              # postinstall: 按平台导入对应子包二进制
      platform-darwin-arm64/    # @uselegion/cli-darwin-arm64
        package.json            # optionalDependencies 关系
      platform-darwin-x64/
      platform-linux-arm64-musl/
      platform-linux-x64-musl/
      platform-win32-x64-msvc/
- 从对应 GitHub Release artifact 下载二进制放入各平台包
- npm publish 每个(6 次),用 NPM_TOKEN
- 主包用 "optionalDependencies" 声明 5 个平台包,npm 按当前平台只装一个
```

主包 `install.js` 逻辑(已验证为 esbuild/biome/turbo 标准模式):
```js
const { exitCode } = await import(`@uselegion/cli-${process.platform}-${arch}`)
  // 平台包 package.json 的 "bin" 指向内置二进制,主包 symlink/转发
```

#### Job 8 notify
- 在 GitHub Release body 写入全部安装方式 + manifest 指针

### 步骤 7:配置 GitHub Secrets / 仓库

需创建的 secret(在 `dawnswwwww/legion` 仓库 Settings → Secrets):
| Secret | 用途 |
|---|---|
| `CARGO_REGISTRY_TOKEN` | crates.io 发布 |
| `NPM_TOKEN` | npm 发布(automation token) |
| `LEGION_RELEASE_SIGNING_KEY` | manifest 签名私钥(hex) |
| `HOMEBREW_TAP_TOKEN` | 跨仓推 homebrew-legion(PAT 或 GitHub App) |

需创建的外部仓库:
- `dawnswwwww/homebrew-legion`(Homebrew tap,初始含 Formula/legion.rb 骨架)

需在 npm 注册的 org:
- `uselegion`(先到 npmjs.com 创建 org)

### 步骤 8:本地 helper 脚本 `scripts/release.sh`(可选便捷)

封装"改版本号 → 打 tag → 推"的手动动作,降低出错:
```bash
#!/usr/bin/env bash
# scripts/release.sh <version>
# 1. sed 根 Cargo.toml version = "<version>"
# 2. cargo build --workspace (确认编译)
# 3. git commit -m "release: v<version>"
# 4. git tag v<version>
# 5. git push origin main --tags
# 工作流接管后续全自动发布
```

---

## 产物清单(实施后新增/修改的文件)

**修改:**
- `Cargo.toml`(元数据 + tokio-tungstenite rustls)
- `crates/legion-memory/Cargo.toml`(SQLite bundled)
- `crates/legion-protocol/src/manifest.rs`(真公钥 + 测试更新)
- `crates/legion-cli/src/lib.rs`(default_manifest_url 稳定 URL)
- `.gitignore`(忽略 release key seed)

**新增:**
- `.github/workflows/ci.yml`
- `.github/workflows/release.yml`
- `npm/`(6 个子目录:cli + 5 平台包)
- `Formula/legion.rb`(formula 模板,发布时同步到 homebrew-legion 仓)
- `scripts/release.sh`
- `scripts/sign-manifest.mjs`(或 Rust xtask,Job 4 用)
- `docs/release-signing-key.md`(密钥托管说明)

---

## 验证方式

1. **先合步骤 1-3**(代码改动),跑 `cargo build --workspace --all-targets && cargo clippy --workspace --all-targets && cargo test --workspace --all-targets` 全绿,且 `cargo tree | grep openssl` 为空。
2. **本地试编译 musl**:`rustup target add x86_64-unknown-linux-musl && cross build -p legion-cli --target ...`,确认静态。
3. **CI 工作流**先在 PR 上跑通 ci.yml。
4. **首个 release**:打 `v0.1.0` tag(或 `v0.1.0-rc1`),观察 release.yml 全 8 job 通过,然后在干净机器验证 `brew install`、`npm i -g @uselegion/cli`、`cargo install legion-cli`、`legion gateway install` 四条路径。
5. **端到端**:发布后 `legion setup` + `legion gateway install`(触发 manifest 下载)在无网络预装的干净 Linux/macos 上可用。

---

## 注意点与风险

- **token 兼容性**:`cliVersionRange` 用 semver range,需与 `semver` crate 解析一致(空格分隔 `>=0.1.0 <0.2.0` → installer.rs:247-252 已有 normalize 逻辑)。
- **crates.io 重发**:同版本号不可重发。首次发布失败需 bump 版本或 `cargo publish --allow-dirty`。建议先发一个 `0.1.0-rc.1` 演练全链路。
- **gateway 协议版本**:`ProtocolRange.min/maxPeerRevision` 需从 `legion-protocol` 实际值填,Job 4 脚本要读这个常量。
- **musl + sqlite-vec**:`sqlite-vec` 编译时可能用 SSE/AVX,某些 musl cross 容器需确认 CPU 特性不影响。已在 cross 镜像里验证过 bundled sqlite 是标准做法。
- **Homebrew tap formula 签名**:Homebrew 对 tap formula 无强制签名,但 sha256 必须对。
- **npm 平台包 optionalDependencies**:确保主包不把 5 个平台二进制都拉下来(用 optionalDependencies 而非 dependencies)。