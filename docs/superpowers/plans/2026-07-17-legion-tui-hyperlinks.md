# T7 — Legion TUI 超链接 OSC 8

## Goal
让 Markdown 链接、tool card 中的文件路径、URL 支持 OSC 8 超链接协议，用户可在支持的终端中 Ctrl+点击打开。

## Scope
- 在 Markdown 渲染时，把 `[text](url)` 的 URL 以 OSC 8 序列包裹。
- 在 tool card 中，对 `stdout` 里的 URL 和本地路径使用 `linkify` 识别并包裹 OSC 8。
- 不支持鼠标点击检测（grok-build 级别功能），仅输出 OSC 8 让终端处理。

## Files to Touch
- Modify: `crates/legion-cli/src/tui/markdown.rs`（T1 拆分）— 链接 Span 包裹 OSC 8。
- Modify: `crates/legion-cli/src/tui/tool_card.rs` — 结果里的 URL 包裹 OSC 8。
- Create: `crates/legion-cli/src/tui/links.rs` — OSC 8 生成与 URL 识别。
- Modify: `crates/legion-cli/Cargo.toml` — 新增 `linkify` 依赖。

## Key Interfaces

```rust
// crates/legion-cli/src/tui/links.rs
/// Wrap text in an OSC 8 hyperlink sequence.
pub fn osc8_link(url: &str, display: &str) -> String;

/// Extract URLs from plain text using linkify.
pub fn extract_urls(text: &str) -> Vec<(usize, usize, String)>;

/// Apply OSC 8 links to all URLs in a string, returning a ratatui-friendly representation.
pub fn linkify_text(text: &str) -> Vec<Span<'static>>;
```

## Rendering Changes
- Markdown 链接：把当前 `Span::styled(text, link_style)` 改为 content 中包含 OSC 8 转义序列的 `Span`。
- Tool card：对 `stdout`/`stderr` 中的 URL 用 `linkify_text` 拆分后渲染。

## Dependencies
- `linkify`：URL/路径识别。

## Acceptance Criteria
1. Markdown 链接在支持 OSC 8 的终端中可点击。
2. Tool card 中的 URL 可点击。
3. 在不支持 OSC 8 的终端中回退为纯文本显示。
4. 单元测试验证 `osc8_link` 输出包含正确的转义序列。
5. 通过 `cargo test -p legion-cli`。

## Risks
- OSC 8 与宽字符、自定义 wrapping 结合时可能出现显示宽度计算错误。
- 终端兼容性：部分终端（如某些 Windows Terminal 版本）对 OSC 8 支持不完整。
