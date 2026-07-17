# Legion TUI 升级整体规划

> 本文档是 Legion TUI 改进的**总进度表**。每个功能点都有独立的设计/实现文档，本表通过链接串联，作为后续开发、进度追踪和验收的唯一入口。

---

## 1. 目标

对标 `grok-build` 的 TUI 体验，在不引入过重依赖、不破坏现有稳定性的前提下，分阶段提升 Legion CLI TUI 的：

1. **视觉质感**：主题、代码高亮、排版细节。
2. **输入体验**：undo/redo、鼠标选择、历史搜索、chips。
3. **输出表现**：ANSI 彩色输出、超链接、消息内选择。
4. **工程可维护性**：拆分臃肿的 `render.rs`、writer thread、PTY 回归测试。

---

## 2. 现状摘要

- **底层库一致**：Legion 与 grok-build 都使用 `ratatui 0.29` + `crossterm 0.28`，版本不是问题。
- **差距在中间件**：grok-build 自研了 `xai-ratatui-textarea`（富输入）、`xai-ratatui-inline`（内联视口）、`xai-grok-pager-render`（主题/高亮/ANSI）等中间层。
- **Legion 当前位置**：
  - 入口：`crates/legion-cli/src/tui.rs`
  - 渲染：`crates/legion-cli/src/tui/render.rs`（1320 行，已偏大）
  - 状态：`crates/legion-cli/src/tui/state.rs`
  - 事件：`crates/legion-cli/src/tui/events.rs`
  - 输入：`crates/legion-cli/src/tui/input.rs`
  - 依赖：`ratatui`、`crossterm`、`unicode-width`、`pulldown-cmark`

---

## 3. 阶段与任务总览

### Phase 1 — 视觉基础（低风险、高视觉回报）

| ID | 功能点 | 状态 | 优先级 | 估算 | 设计文档 |
|----|--------|------|--------|------|----------|
| T1 | 主题系统 + `render.rs` 拆分 | done | P0 | 3-4d | [T1 主题系统](./2026-07-17-legion-tui-theme-system.md) |
| T2 | 代码块语法高亮 | done | P0 | 2-3d | [T2 语法高亮](./2026-07-17-legion-tui-syntax-highlighting.md) |
| T3 | 富输入框（`ratatui-textarea`） | done | P1 | 5-7d | [T3 富输入框](./2026-07-17-legion-tui-rich-textarea.md) |

### Phase 2 — 交互增强（中等风险、高交互回报）

| ID | 功能点 | 状态 | 优先级 | 估算 | 设计文档 |
|----|--------|------|--------|------|----------|
| T4 | 消息内文本选择 | done | P1 | 3-4d | [T4 消息选择](./2026-07-17-legion-tui-message-selection.md) |
| T5 | Prompt 历史模糊搜索 | done | P2 | 2-3d | [T5 历史搜索](./2026-07-17-legion-tui-history-search.md) |
| T6 | 命令输出 ANSI 渲染 | done | P1 | 4-5d | [T6 ANSI 输出](./2026-07-17-legion-tui-ansi-output.md) |

### Phase 3 — 连接与扩展（中长期）

| ID | 功能点 | 状态 | 优先级 | 估算 | 设计文档 |
|----|--------|------|--------|------|----------|
| T7 | 超链接 OSC 8 | done | P2 | 2-3d | [T7 超链接](./2026-07-17-legion-tui-hyperlinks.md) |
| T8 | Writer Thread（非阻塞终端 I/O） | done | P2 | 3-4d | [T8 Writer Thread](./2026-07-17-legion-tui-writer-thread.md) |
| T9 | PTY 集成测试框架 | done | P3 | 5-7d | [T9 PTY 测试](./2026-07-17-legion-tui-pty-testing.md) |
| T10 | Inline / Minimal 视口模式 | in-progress | P3 | 7-10d | [T10 Inline 视口](./2026-07-17-legion-tui-inline-viewport.md) |

---

## 4. 全局约束

- **Rust 版本**：MSRV 1.86，Edition 2024，resolver 3。
- **新增依赖原则**：优先使用 workspace 已有依赖；新增依赖必须已在 `Cargo.lock` 或 grok-build 中验证过兼容性。
- **不允许反向依赖**：保持 `cargo tree -p legion-cli` 不出现 `legion-gateway`。
- **测试要求**：每个改动必须通过 `cargo build/clippy/test/fmt --workspace --all-targets`；新增 UI 行为必须有单元测试或 PTY 测试。
- **TUI 默认行为不变**：所有改进默认开启时不能破坏现有 gateway/local/auto 三种模式的事件循环。
- **不复制 grok-build 源码**：grok-build 的 `xai-*` crate 是定制实现，不能直接复制；优先使用 crates.io 等效库或自研最小实现。

---

## 5. 依赖关系图

```
T1 主题系统 + render.rs 拆分
├── T2 语法高亮（依赖 theme 抽象）
├── T3 富输入框（可并行，但建议 T1 完成后统一风格）
└── T6 ANSI 输出（依赖 render 模块拆分）

T3 富输入框
├── T4 消息内文本选择
└── T5 Prompt 历史搜索

T8 Writer Thread
├── T9 PTY 测试（需要稳定的输出节奏）
└── T10 Inline 视口（需要 writer thread 的 scrollback 写入）
```

---

## 6. 验收总标准

1. `cargo build --workspace --all-targets`、`cargo clippy --workspace --all-targets`、`cargo test --workspace --all-targets`、`cargo fmt -- --check` 全绿。
2. 每个阶段完成后，至少有一名团队成员在真实终端（iTerm2/VS Code Terminal/Windows Terminal）完成一次完整对话不走样。
3. 不增加 CLI 启动耗时 >100ms（本地机器测量）。
4. 不增加 release 二进制大小 >10%（累计）。

---

## 7. 进度更新规则

- 本表的状态字段仅允许：`planned` / `in-progress` / `review` / `done`。
- 每个功能点开始开发时，由开发者在对应设计文档顶部更新 `Status` 和 `Owner`。
- 功能点合并到主分支后，由开发者将本表对应行状态改为 `done`，并补充 `Completed` 日期。

---

## 8. 参考文档

- 当前 TUI 入口：`crates/legion-cli/src/tui.rs`
- 当前渲染实现：`crates/legion-cli/src/tui/render.rs`
- grok-build TUI 分析报告（本分析结论已写入各设计文档背景章节）
