# T8 — Legion TUI Writer Thread（非阻塞终端 I/O）

## Goal
把当前在 async 事件循环中直接调用 `terminal.draw(...)` 的同步写终端操作，改为通过独立 writer thread 异步写入，避免慢终端（SSH、WSL、tmux）阻塞事件循环。

## Scope
- 在 `tui.rs` 中创建 frame 写入通道：`mpsc::channel<Vec<u8>>`。
- 创建独立 OS thread（或 blocking tokio task）从通道读取 frame bytes 并写入 stdout。
- `tui_loop` 中把 `terminal.draw` 的输出通过通道发送，而不是直接写 stdout。
- 退出时优雅等待 writer thread 排空。

## Files to Touch
- Modify: `crates/legion-cli/src/tui.rs` — 创建 writer thread 和通道，改造 `run_terminal` / `tui_loop`。
- Create: `crates/legion-cli/src/tui/writer.rs` — `TermWriter`、`WriterThread`。
- Modify: `crates/legion-cli/src/tui/render.rs` — 若 `draw_ui` 直接持有 backend，需要改为接收 writer。

## Key Interfaces

```rust
// crates/legion-cli/src/tui/writer.rs
use std::io::Write;
use std::sync::mpsc::{Sender, channel};

pub struct TermWriter {
    tx: Sender<Vec<u8>>,
}

impl Write for TermWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize>;
    fn flush(&mut self) -> std::io::Result<()>;
}

pub struct WriterThread;

impl WriterThread {
    /// Spawn a thread that reads from rx and writes to `out`.
    pub fn spawn<W: Write + Send + 'static>(out: W) -> TermWriter;
    
    /// Wait until all pending bytes are drained.
    pub fn drain(tx: &Sender<Vec<u8>>) -> std::io::Result<()>;
}
```

## Architecture
参考 grok-build 的 `xai-grok-pager-render/src/render/draw.rs:115-246`：
1. `run_terminal` 创建 `TermWriter` 包装 `stdout`。
2. `CrosstermBackend::new(term_writer)` 作为 backend。
3. `terminal.draw` 把 frame 写入 `TermWriter` 的内部 buffer，然后一次性 send 到 writer thread。
4. Writer thread 在循环中 `rx.recv()` → `stdout.write_all(bytes)` → `stdout.flush()`。

## Dependencies
- 不引入新依赖。

## Acceptance Criteria
1. `cargo test -p legion-cli` 通过。
2. 在本地运行 TUI 对话无异常。
3. 退出时不丢失未写入的 frame。
4. writer thread 能被正确 join，不泄漏。
5. 通过 `strace`/dtruss 或日志确认 terminal bytes 由独立线程写入（可选验证）。

## Risks
- `ratatui::Terminal` 的 backend 需要 `Write + Flush`，`TermWriter` 必须正确实现。
- 错误处理：writer thread panic 不能导致主线程静默卡死。
- 与 T10（inline viewport）强相关；T10 依赖 writer thread 的 scrollback 写入能力。
