# T10 — Legion TUI Inline / Minimal 视口模式

- Status: in-progress
- Owner: agent

## Goal
实现 grok-build 的 "minimal" 模式： finalized 对话内容写入终端原生 scrollback，只有 prompt 和当前运行中的 turn 占用一个小的 inline ratatui 视口。这样用户可以用终端原生滚动查看历史，而不是被锁在 alternate screen。

## Scope
- 新增 `/mode fullscreen` 和 `/mode inline` slash 命令。
- Inline 模式下：
  - 使用 ratatui 内置的 `Viewport::Inline` 渲染底部 live 区域，不引入额外依赖。
  - 完成一轮对话后，把消息渲染为纯文本并通过 T8 的 writer thread 写入 scrollback。
  - prompt 和 streaming 内容固定在底部 inline 区域。
- Fullscreen 模式保持当前 alternate screen 行为。
- 默认仍使用 fullscreen。

## Files to Touch
- Modify: `crates/legion-cli/src/tui.rs` — 根据模式选择 terminal 初始化。
- Create: `crates/legion-cli/src/tui/inline.rs` — inline viewport 管理、scrollback 写入。
- Modify: `crates/legion-cli/src/tui/render.rs` — 提供 finalized 消息的纯文本版本用于 scrollback 输出。
- Modify: `crates/legion-cli/src/tui/events.rs` — 处理 `/mode` 命令。
- Modify: `crates/legion-cli/Cargo.toml` — 视实现而定，可能新增 `ratatui-inline` 或保持无新增。

## Key Interfaces

```rust
// crates/legion-cli/src/tui/inline.rs
pub enum ScreenMode {
    Fullscreen,
    Inline,
}

pub struct InlineViewport {
    height: u16,
    backend: CrosstermBackend<io::Stdout>,
}

impl InlineViewport {
    pub fn new(height: u16) -> io::Result<Self>;
    /// Emit finalized messages to native scrollback.
    pub fn emit_to_scrollback(&mut self, messages: &[ChatMessage]) -> io::Result<()>;
    /// Resize the live viewport, scrolling covered rows into history.
    pub fn set_height(&mut self, height: u16) -> io::Result<()>;
}
```

## Dependencies
- 调研 crates.io 上是否有 `ratatui-inline` 可用；若无，自研最小 inline terminal。
- 依赖 T8（Writer Thread），因为 inline 模式需要异步写入 scrollback。

## Acceptance Criteria
1. `/mode inline` 切换到 inline 模式，不进入 alternate screen。
2. 用户发送消息后，消息出现在终端原生 scrollback。
3. 完成一轮 assistant/tool 回合后，该回合内容被写入 scrollback。
4. `/mode fullscreen` 切换回当前行为。
5. 切换模式不丢失当前会话状态。
6. 通过 `cargo test -p legion-cli`。

## Risks
- Inline viewport 实现复杂，涉及终端光标位置、清除行、resize 重排。
- 与 T8 强耦合，必须等 writer thread 完成后才能稳定实现。
- 不同终端对 inline 行为差异大，测试成本高。
