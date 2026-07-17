# T6 — Legion TUI 命令输出 ANSI 渲染

## Goal
`exec` 等工具返回的 `stdout`/`stderr` 如果包含 ANSI 颜色、光标控制、进度条，当前会被当成纯文本截断显示，效果很差。本任务让 tool card 能原生渲染 ANSI 输出。

## Scope
- 在 tool card 渲染路径中，检测 `stdout`/`stderr` 是否包含 ANSI 转义序列。
- 若包含，使用 `ansi-to-tui` 或 `vte` 将其转换为 ratatui `Text`/`Line`。
- 对无 ANSI 的输出保持当前纯文本截断展示。
- 仅处理常见的 SGR 颜色、加粗、斜体；不处理复杂的光标移动（避免破坏布局）。

## Files to Touch
- Modify: `crates/legion-cli/src/tui/tool_card.rs`（由 T1 拆分出来）— 在结果解析后加入 ANSI 渲染分支。
- Create: `crates/legion-cli/src/tui/ansi.rs` — ANSI 检测与转换。
- Modify: `crates/legion-cli/Cargo.toml` — 新增 `ansi-to-tui = "7.0"` 依赖。

## Key Interfaces

```rust
// crates/legion-cli/src/tui/ansi.rs
use ratatui::text::Text;

/// Returns true if text contains ANSI escape sequences.
pub fn has_ansi(text: &str) -> bool;

/// Convert ANSI text to ratatui Text.
/// Falls back to plain text if parsing fails.
pub fn ansi_to_text(text: &str) -> Text<'static>;
```

## Rendering Changes
在 `tool_card.rs` 的 `push_result_lines` 中：
1. 若 `stdout` 包含 ANSI，调用 `ansi_to_text(stdout)` 并直接追加其行。
2. 否则保持现有 head/tail 截断逻辑。

## Dependencies
- `ansi-to-tui = "7.0"`：与 grok-build 一致。
- 备选：`vte` + `alacritty_terminal` 若需要更复杂的 PTY 模拟，但本任务不引入。

## Acceptance Criteria
1. 包含 ANSI 颜色的 `exec` 输出在 tool card 中显示彩色文本。
2. 无 ANSI 的输出保持现有行为。
3. 超长的 ANSI 输出仍然会被截断（保留 head + tail）。
4. 畸形 ANSI 不会 panic，回退到纯文本。
5. 单元测试覆盖 `has_ansi` 和常见颜色序列转换。
6. 通过 `cargo test -p legion-cli`。

## Risks
- `ansi-to-tui` 的 `Text` 与当前自定义 wrapping 逻辑可能冲突；需要测试长行 ANSI 的换行。
- 二进制大小增长需评估。
