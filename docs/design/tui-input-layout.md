# TUI 输入区布局契约

> 实现:`crates/legion-cli/src/tui/render.rs` 的 `plan_layout` / `draw_ui`。
> 回归测试:`render.rs` 的 `#[cfg(test)] mod tests` 与 `tui.rs` 的
> `input_box_not_covered_by_status_bar` / `input_box_visible_on_tiny_terminals`。

## 背景

queue 面板改动曾把状态栏渲染到输入区的 chunk 上(`render.rs` 用裸
`chunks[3]` 索引),不透明背景直接盖住输入框。本次重设计把布局计算收敛
为纯函数 `plan_layout`,用命名区域取代裸索引,并把"输入框永远完整可见"
写成可测试的不变量。

## 区域顺序(自上而下)

chat → todo 面板 → queue 面板 → **input** → status(最底)

## 不变量(优先级从高到低)

1. **输入框永远完整可见**:高度 = `clamp(内容可视行数 + 2 边框, 3, 10)`,
   内容超过 8 行在框内滚动、不再长高。任何面板、状态栏、浮层都不得
   遮挡输入区。唯一让步:终端高度 ≥ 5 时要给 chat 保底 1 行,输入框
   高度上限相应收 1 行。
2. **chat 保底 1 行**(终端高度 ≥ 5 时);高度 ≤ 4 时输入框优先,chat
   归零。
3. **status 可被完全隐藏**:2 行(状态 + 快捷键 hints,高度 ≥ 15)→
   1 行 → 0 行,在触碰 chat 保底之前先牺牲自己。
4. **面板优先级最低**:todo 先于 queue 被牺牲;面板只有在"显示它之后
   chat 仍 ≥ 5 行"时才分配高度,否则整体隐藏。
5. **浮层避开输入框**:slash 补全菜单锚定在输入区上边缘向上展开;
   history 搜索弹窗在输入区上方的行范围内居中,空间不足直接不画。

## 降级链(空间不足时依次牺牲)

todo 面板 → queue 面板 → status 第 2 行(hints)→ status 整体 →
chat(至保底 1 行,高度 ≤ 4 时归零)

## 关键档位行为

| 终端高度 | input | status | chat | 面板 |
|---|---|---|---|---|
| 30(宽松) | 3~10 | 2 | 剩余全部 | 按需显示 |
| 15 | 3~10 | 2 | ≥ 5 时面板存活 | todo 先让位 |
| 8 | 3 | 1 | 4 | 全隐藏 |
| 5 | 3 | 1 | 1 | 全隐藏 |
| 4 | 3 | 1 | 0 | 全隐藏 |
| 3 | 3 | 0 | 0 | 全隐藏 |

## 实现要点

- `plan_layout(total_height, input_lines, todos, todo_max_display,
  queued_messages) -> LayoutPlan` 是纯函数,五个区域高度之和恒等于终端
  高度,`draw_ui` 用五个 `Constraint::Length` 精确切分,不再依赖
  ratatui 的 `Min` 求解行为。
- 渲染端一律使用命名区域(`chat_area` / `todo_area` / `queue_area` /
  `input_area` / `status_area`),杜绝裸 chunk 索引错位。
- 输入框内容行数按逻辑行逐行软换行统计(`input::wrap_display_line`,
  与 composer 渲染共用同一实现);tui-textarea 0.7 不支持软换行,故
  `Composer::render` 自绘包裹后的内容、光标与 placeholder,编辑状态仍
  由 tui-textarea 维护。
