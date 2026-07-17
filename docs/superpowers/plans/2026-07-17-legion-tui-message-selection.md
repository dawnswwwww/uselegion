# T4 — Legion TUI 消息内文本选择

## Goal
让用户在 scrollback 消息区域内用鼠标拖拽选择文本，并支持复制（通过系统剪贴板或 OSC 52）。当前仅支持 Shift+拖拽触发终端原生选择，在 inline/alternate screen 模式下体验不佳。

## Scope
- 在 `AppState` 中维护一个选择模型：`selection_start: Option<(usize, usize)>`（消息索引, 行内列）和 `selection_end`。
- 支持鼠标拖拽选择多行消息文本。
- 支持双击选词、三击选行（可选，优先级低于基础拖拽）。
- 选择高亮通过反向颜色（`Modifier::REVERSED`）或自定义主题色实现。
- 复制通过 `Ctrl+C` 或右键菜单触发；优先使用 `arboard` 写入系统剪贴板，回退到 OSC 52。

## Files to Touch
- Modify: `crates/legion-cli/src/tui/state.rs` — 添加选择状态字段。
- Modify: `crates/legion-cli/src/tui/events.rs` — 处理鼠标按下/移动/释放，更新选择状态。
- Modify: `crates/legion-cli/src/tui/render.rs` — 在渲染消息行时根据选择区间应用高亮样式。
- Create: `crates/legion-cli/src/tui/selection.rs` — 选择模型与坐标转换。
- Modify: `crates/legion-cli/Cargo.toml` — 新增 `arboard` 依赖（需确认跨平台）。

## Key Interfaces

```rust
// crates/legion-cli/src/tui/selection.rs
use ratatui::layout::Position;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cursor {
    pub message_index: usize,
    pub line_index: usize,      // within rendered (wrapped) lines of the message
    pub grapheme_index: usize,  // within the line
}

pub struct Selection {
    pub anchor: Cursor,
    pub head: Cursor,
}

impl Selection {
    pub fn is_empty(&self) -> bool;
    pub fn normalized(&self) -> (Cursor, Cursor);
    pub fn contains(&self, message_index: usize, line_index: usize, grapheme_index: usize) -> bool;
}

/// Convert a mouse `Position` in the chat area to a `Cursor`.
pub fn position_to_cursor(
    pos: Position,
    chat_area: Rect,
    messages: &[ChatMessage],
    scroll_offset: usize,
    render_cache: &[Option<CachedRender>],
) -> Option<Cursor>;

/// Extract selected text as a plain string.
pub fn selected_text(
    selection: &Selection,
    messages: &[ChatMessage],
    render_cache: &[Option<CachedRender>],
) -> String;
```

## Rendering Changes
在 `render.rs` 渲染每行消息时，若当前 grapheme 位于 `Selection` 区间内，叠加 `Style::default().add_modifier(Modifier::REVERSED)`。

## Dependencies
- `arboard` 或 `clipboard`：系统剪贴板写入。
- 若不允许新增依赖，使用 OSC 52 转义序列写入终端剪贴板。

## Acceptance Criteria
1. 鼠标拖拽可在单条消息内选择文本。
2. 鼠标拖拽可跨消息选择文本。
3. `Ctrl+C` 将选中内容写入系统剪贴板（或 OSC 52）。
4. 选择模型单元测试覆盖 `contains`、`normalized`、`position_to_cursor`。
5. 不选择时 UI 与当前一致。
6. 通过 `cargo test -p legion-cli`。

## Risks
- 与现有鼠标滚轮、think 块点击事件冲突，需要事件分发优先级设计。
- 宽字符（CJK）和 grapheme cluster 边界需要 `unicode-segmentation` 正确处理。
