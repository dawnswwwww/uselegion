# T5 — Legion TUI Prompt 历史模糊搜索

## Goal
把当前的 `↑/↓` 线性历史回退升级为 `/history` 或 `Ctrl+R` 触发的模糊搜索弹窗，支持按关键词快速定位并复用历史消息。

## Scope
- 保留现有 `↑/↓` 历史行为。
- 新增 `Ctrl+R` 快捷键打开历史搜索弹窗。
- 弹窗中显示过滤后的历史列表，支持 `↑/↓` 选择、`Enter` 确认、`Esc` 关闭。
- 过滤使用简单子串匹配即可，无需引入 `nucleo`/`fuzzy-matcher` 等重型库；若后续需要再升级。

## Files to Touch
- Modify: `crates/legion-cli/src/tui/state.rs` — 添加历史搜索状态：`history_search_open: bool`, `history_query: String`, `history_selected: usize`。
- Modify: `crates/legion-cli/src/tui/events.rs` — 处理 `Ctrl+R`、弹窗内按键、确认/取消。
- Modify: `crates/legion-cli/src/tui/render.rs` — 在历史搜索打开时渲染弹窗覆盖层。
- Create: `crates/legion-cli/src/tui/history_search.rs` — 过滤与选择逻辑。

## Key Interfaces

```rust
// crates/legion-cli/src/tui/history_search.rs
pub struct HistorySearch {
    pub query: String,
    pub selected: usize,
}

impl HistorySearch {
    pub fn new() -> Self;
    pub fn reset(&mut self);
    pub fn filtered<'a>(&'a self, history: &'a [String]) -> Vec<(usize, &'a String)>;
    pub fn move_up(&mut self);
    pub fn move_down(&mut self, count: usize);
    pub fn selected_index(&self) -> Option<usize>;
}
```

## UI Design
- 弹窗宽度 80%，高度 60%，居中。
- 顶部一个输入框显示当前 query。
- 下方 `List` 显示匹配结果，每行显示历史消息前 80 个字符。
- 选中行高亮。

## Rendering Changes
在 `draw_ui` 中，若 `state.history_search_open` 为真，先绘制聊天区域，再用 `Clear` + 弹窗块覆盖。

## Dependencies
- 不引入新依赖；使用现有 `ratatui` List/Paragraph。

## Acceptance Criteria
1. `Ctrl+R` 打开历史搜索弹窗。
2. 输入 query 实时过滤历史消息。
3. `Enter` 将选中的历史消息填入输入框并关闭弹窗。
4. `Esc` 关闭弹窗且不修改输入。
5. 弹窗打开时不影响后台消息接收。
6. 单元测试覆盖 `HistorySearch::filtered`、`move_up`、`move_down`。
7. 通过 `cargo test -p legion-cli`。

## Risks
- 若 T3（富输入框）先完成，需要确认 `ratatui-textarea` 与弹窗输入框的兼容性。
- 历史消息可能包含多行，过滤和展示需要考虑换行。
