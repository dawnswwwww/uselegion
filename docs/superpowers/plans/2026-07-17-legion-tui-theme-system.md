# T1 — Legion TUI 主题系统 + `render.rs` 拆分

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把当前硬编码的颜色常量收敛成可配置的主题系统，并在此过程中把 1320 行的 `render.rs` 拆分为职责清晰的子模块。

**Architecture:** 新增 `crates/legion-cli/src/tui/theme.rs` 定义 `Theme` struct 与默认主题；将 `render.rs` 拆为 `markdown.rs`、`tool_card.rs`、`widgets.rs`；`render.rs` 保留 `draw_ui` 总入口和布局计算。所有颜色通过 `Theme` 获取，默认保持现有视觉。

**Tech Stack:** Rust, `ratatui 0.29`, `dark-light`（新增，可选阶段 2 接入）.

---

## Global Constraints

- MSRV 1.86，Edition 2024。
- 新增依赖：`dark-light` 仅用于后续系统主题探测；本任务不强制引入。
- 不修改消息渲染的语义（前缀、状态符号、think 块折叠行为）。
- 拆分后 `render.rs` 行数应降至 ≤400 行。

---

## Task 1: 创建 `Theme` 抽象

**Files:**
- Create: `crates/legion-cli/src/tui/theme.rs`
- Modify: `crates/legion-cli/src/tui/mod.rs`（暴露 `theme` 模块）

**Interfaces:**
- Produces: `pub struct Theme { pub user_bar: Color, pub user_bg: Color, pub assistant_bar: Color, pub assistant_bg: Color, pub system_bar: Color, pub tool_bar: Color, pub question_bar: Color, pub status_bg: Color, pub status_fg: Color, pub input_border: Color, pub selected_fg: Color, pub code_bg: Color, pub code_inline_bg: Color, pub link_fg: Color, pub error_fg: Color, pub spinner_fg: Color }`
- Produces: `impl Default for Theme { fn default() -> Self }` — 颜色与当前 `render.rs` 硬编码值完全一致。
- Produces: `impl Theme { pub fn default_dark() -> Self; pub fn default_light() -> Self }` — 先都返回 default，为后续做准备。

- [ ] **Step 1: 编写 `Theme` struct 与默认实现**

```rust
// crates/legion-cli/src/tui/theme.rs
use ratatui::style::Color;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Theme {
    pub user_bar: Color,
    pub user_bg: Color,
    pub assistant_bar: Color,
    pub assistant_bg: Color,
    pub system_bar: Color,
    pub tool_bar: Color,
    pub question_bar: Color,
    pub status_bg: Color,
    pub status_fg: Color,
    pub input_border: Color,
    pub selected_fg: Color,
    pub code_bg: Color,
    pub code_inline_bg: Color,
    pub link_fg: Color,
    pub error_fg: Color,
    pub spinner_fg: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            user_bar: Color::Cyan,
            user_bg: Color::Rgb(45, 45, 55),
            assistant_bar: Color::Green,
            assistant_bg: Color::Rgb(28, 34, 28),
            system_bar: Color::Yellow,
            tool_bar: Color::DarkGray,
            question_bar: Color::Magenta,
            status_bg: Color::Rgb(40, 40, 50),
            status_fg: Color::Gray,
            input_border: Color::Blue,
            selected_fg: Color::Black,
            code_bg: Color::Rgb(30, 30, 30),
            code_inline_bg: Color::Rgb(50, 50, 50),
            link_fg: Color::LightBlue,
            error_fg: Color::Red,
            spinner_fg: Color::Green,
        }
    }
}
```

- [ ] **Step 2: 把 `theme` 模块加入 `tui/mod.rs`**

```rust
// crates/legion-cli/src/tui/mod.rs
pub mod theme;
```

- [ ] **Step 3: 编译检查**

Run: `cargo check -p legion-cli`
Expected: PASS

---

## Task 2: 拆分 `render.rs` — 创建 `markdown.rs`

**Files:**
- Create: `crates/legion-cli/src/tui/markdown.rs`
- Modify: `crates/legion-cli/src/tui/render.rs`（删除 markdown 函数，改为 `use super::markdown::*;`）

**Interfaces:**
- Consumes: `Theme` from `theme.rs`
- Produces: `pub fn markdown_lines(text: &str, theme: &Theme) -> Vec<Line<'static>>`
- Produces: `pub fn plain_lines(text: &str) -> Vec<Line<'static>>`

- [ ] **Step 1: 把 `markdown_lines` 及辅助函数整体迁移到 `markdown.rs`**

```rust
// crates/legion-cli/src/tui/markdown.rs
use crate::tui::theme::Theme;
use pulldown_cmark::{CodeBlockKind, Event as MdEvent, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};

pub fn markdown_lines(text: &str, theme: &Theme) -> Vec<Line<'static>> { /* 从 render.rs 移动 */ }
pub fn plain_lines(text: &str) -> Vec<Line<'static>> { /* 从 render.rs 移动 */ }
```

- [ ] **Step 2: 把 `render.rs` 中 hardcoded 颜色替换为 `theme` 字段**

例如：
- 内联代码背景 `Rgb(50,50,50)` → `theme.code_inline_bg`
- 代码块背景 `Rgb(30,30,30)` → `theme.code_bg`
- 链接颜色 → `theme.link_fg`

- [ ] **Step 3: 更新 `render.rs` 使用 `markdown_lines(text, &state.theme)`**

- [ ] **Step 4: 运行 markdown 相关单元测试**

Run: `cargo test -p legion-cli markdown_lines`
Expected: PASS

---

## Task 3: 拆分 `render.rs` — 创建 `tool_card.rs`

**Files:**
- Create: `crates/legion-cli/src/tui/tool_card.rs`
- Modify: `crates/legion-cli/src/tui/render.rs`

**Interfaces:**
- Consumes: `Theme`
- Produces: `pub fn tool_card_json(state: &str, name: &str, args: Option<&str>, result: Option<&str>) -> String`
- Produces: `pub fn render_tool_card(content: &str, theme: &Theme) -> Vec<Line<'static>>`

- [ ] **Step 1: 迁移 `tool_card_json`、`render_tool_card`、`push_result_lines` 到 `tool_card.rs`**

- [ ] **Step 2: 替换其中的硬编码颜色（done/error/running 标签、分隔线）为 `theme` 字段**

- [ ] **Step 3: 更新 `render.rs` 调用点**

- [ ] **Step 4: 运行 tool card 单元测试**

Run: `cargo test -p legion-cli render_tool_card`
Expected: PASS

---

## Task 4: 拆分 `render.rs` — 创建 `widgets.rs` 与简化 `render.rs`

**Files:**
- Create: `crates/legion-cli/src/tui/widgets.rs`
- Modify: `crates/legion-cli/src/tui/render.rs`

**Interfaces:**
- Consumes: `Theme`
- Produces: `pub fn role_color(role: MessageRole, theme: &Theme) -> Color`
- Produces: `pub fn role_background(role: MessageRole, theme: &Theme) -> Color`
- Produces: `pub fn state_indicator(state: MessageState, theme: &Theme) -> Span<'static>`

- [ ] **Step 1: 迁移 `role_color`、`role_background`、`state_indicator` 到 `widgets.rs`**

- [ ] **Step 2: 在 `render.rs` 中保留 `draw_ui`、`message_lines`、`wrap_and_remap`、`wrap_line_to_width`**

- [ ] **Step 3: 确保 `render.rs` 行数 ≤400**

Run: `wc -l crates/legion-cli/src/tui/render.rs`
Expected: ≤400

---

## Task 5: 在 `AppState` 中持有 `Theme`

**Files:**
- Modify: `crates/legion-cli/src/tui/state.rs`
- Modify: `crates/legion-cli/src/tui/render.rs`

**Interfaces:**
- Produces: `pub theme: Theme` in `AppState`

- [ ] **Step 1: 在 `AppState` 添加 `theme: Theme` 字段**

```rust
// crates/legion-cli/src/tui/state.rs
use crate::tui::theme::Theme;

pub struct AppState {
    // ... existing fields
    pub theme: Theme,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            // ...
            theme: Theme::default(),
        }
    }
}
```

- [ ] **Step 2: 把 `render.rs` 中所有 `Theme::default()` 调用改为 `&state.theme`**

- [ ] **Step 3: 运行完整 TUI 单元测试**

Run: `cargo test -p legion-cli tui::`
Expected: PASS

---

## Task 6: 接入 `/theme` slash 命令（可选）

**Files:**
- Modify: `crates/legion-cli/src/tui/input.rs`
- Modify: `crates/legion-cli/src/tui/events.rs`

**Interfaces:**
- Produces: `/theme dark` / `/theme light` / `/theme default` 切换 `state.theme`

- [ ] **Step 1: 在 `slash_commands` 列表注册 `theme`**

- [ ] **Step 2: 在事件处理中解析 `/theme <name>` 并更新 `state.theme`**

- [ ] **Step 3: 加单元测试：切换主题后 `role_color` 返回不同颜色**

Run: `cargo test -p legion-cli theme`
Expected: PASS

---

## Self-Review

- **Spec coverage:** 主题抽象、render.rs 拆分、颜色替换、slash 命令都有对应任务。
- **Placeholder scan:** 无 TBD/TODO；所有代码块都是可编译的骨架。
- **Type consistency:** `Theme` 字段命名在 Task 1-6 中保持一致。
