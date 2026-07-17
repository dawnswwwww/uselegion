# T9 — Legion TUI PTY 集成测试框架

## Goal
建立一套基于 PTY 的端到端测试框架，能够启动真实的 `legion` 二进制、发送按键/鼠标事件、捕获屏幕状态，从而回归测试 TUI 的输入、滚动、选择、渲染等行为。

## Scope
- 使用 `portable-pty` 在测试中启动 `legion` 子进程，分配 PTY。
- 使用 `alacritty_terminal` 或自研 VTE 解析器把 PTY 输出解析为屏幕状态。
- 提供测试辅助 API：`send_key`、`send_text`、`wait_for_text`、`screenshot`、`assert_screen_contains`。
- 首批覆盖场景：启动、发送消息、历史召回、tool card 显示、退出。

## Files to Touch
- Create: `crates/legion-cli/tests/pty_harness/mod.rs` — PTY 启动与屏幕捕获。
- Create: `crates/legion-cli/tests/pty_harness/screen.rs` — 屏幕解析。
- Create: `crates/legion-cli/tests/tui_pty.rs` — 首批 PTY 测试。
- Modify: `crates/legion-cli/Cargo.toml` — 新增 dev-dependencies：`portable-pty`、`alacritty_terminal`、`tempfile`。

## Key Interfaces

```rust
// crates/legion-cli/tests/pty_harness/mod.rs
pub struct TuiPty {
    pty: Box<dyn portable_pty::MasterPty>,
    child: Box<dyn portable_pty::Child>,
    parser: alacritty_terminal::vte::ansi::Processor,
    screen: Screen,
}

impl TuiPty {
    pub fn spawn(binary: &Path) -> anyhow::Result<Self>;
    pub fn send_key(&mut self, key: KeyCode, modifiers: KeyModifiers);
    pub fn send_text(&mut self, text: &str);
    pub fn wait_for_text(&mut self, text: &str, timeout: Duration) -> anyhow::Result<()>;
    pub fn screen_string(&self) -> String;
}
```

## Dependencies
- `portable-pty = "0.9"`
- `alacritty_terminal = "0.26"`
- `anyhow`（若 legion-cli 未引入则作为 dev-dependency）

## Acceptance Criteria
1. `cargo test -p legion-cli --test tui_pty` 能在本地运行。
2. 至少覆盖 3 个核心场景：启动显示欢迎语、发送消息、退出。
3. 测试在 CI 中可运行（Linux/macOS；Windows 可标记为 `#[cfg(unix)]`）。
4. 不依赖外部 API key（使用 `--yolo` 和 local 模式或 mock driver）。

## Risks
- PTY 测试不稳定（时序、终端尺寸、字体宽度）。
- 需要构建 release/debug 二进制，增加 CI 时间。
- `alacritty_terminal` 与 `ratatui` 输出之间的兼容性需要调试。
