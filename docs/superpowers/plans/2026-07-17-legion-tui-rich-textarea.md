# T3 — Legion TUI 富输入框（`ratatui-textarea`）

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 用 `ratatui-textarea` 替换当前手写的 `Paragraph` 输入框，获得 undo/redo、鼠标选择、多行编辑、行号滚动条等能力。

**Architecture:** 引入 `ratatui-textarea` crate；在 `AppState` 中用 `TextArea` 替代 `input: String` + `cursor: usize`；把当前 `events.rs` / `input.rs` 中的按键/鼠标/粘贴逻辑迁移到 `TextArea` 的 API；保持 `OutboundControl::Message` 的接口不变。

**Tech Stack:** Rust, `ratatui 0.29`, `crossterm 0.28`, `ratatui-textarea`（新增）.

---

## Global Constraints

- `ratatui-textarea` 版本必须与 `ratatui 0.29` 兼容；先调研最新版本。
- 不破坏现有 slash 命令补全、`!` shell 模式、历史召回、粘贴折叠（>1000 字符或 >10 行折叠为 placeholder）。
- 不破坏 `Enter` 发送、`Shift+↑/↓` 跨行光标、`Ctrl+Q` 退出等快捷键。
- 保持 `OutboundControl` 接口不变。

---

## Task 1: 调研 `ratatui-textarea` 兼容性与 API

**Files:**
- N/A（调研）

**Interfaces:**
- Unknown until调研完成：需要确认 `ratatui-textarea` 是否支持 `ratatui 0.29`，以及是否支持鼠标选择、undo/redo。

- [ ] **Step 1: 查看 crates.io 版本**

Run: `cargo search ratatui-textarea`
Expected: 返回最新版本号，记录到本任务 PR 描述。

- [ ] **Step 2: 本地快速验证**

Run:
```bash
cd /tmp && cargo new textarea_check && cd textarea_check
cargo add ratatui@0.29
cargo add ratatui-textarea
cargo check
```
Expected: 确认编译通过，并记录 `ratatui-textarea` 版本。

- [ ] **Step 3: 阅读 API 文档**

Fetch: `https://docs.rs/ratatui-textarea/latest/ratatui_textarea/`
Expected: 确认 `TextArea::input`、`TextArea::set_cursor_style`、`TextArea::select_all`、`TextArea::undo`、`TextArea::redo` 等 API 存在。

---

## Task 2: 添加依赖并定义适配层

**Files:**
- Modify: `Cargo.toml`
- Modify: `crates/legion-cli/Cargo.toml`
- Create: `crates/legion-cli/src/tui/composer.rs`

**Interfaces:**
- Produces: `pub struct Composer { textarea: TextArea<'static> }`
- Produces: `impl Composer { pub fn new() -> Self; pub fn input(&mut self, key: KeyEvent); pub fn handle_mouse(&mut self, mouse: MouseEvent, area: Rect); pub fn lines(&self) -> Vec<&str>; pub fn clear(&mut self); pub fn render(&mut self, area: Rect, buf: &mut Buffer); pub fn is_empty(&self) -> bool; pub fn placeholder(&mut self, text: &str); }`

- [ ] **Step 1: 添加 workspace 依赖**

```toml
# Cargo.toml
[workspace.dependencies]
# ... existing
ratatui-textarea = "0.7"  # 以调研结果为准
```

- [ ] **Step 2: 在 legion-cli 引用**

```toml
# crates/legion-cli/Cargo.toml
[dependencies]
# ... existing
ratatui-textarea = { workspace = true }
```

- [ ] **Step 3: 实现 `Composer` 适配层骨架**

```rust
// crates/legion-cli/src/tui/composer.rs
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::{Block, Borders, Widget};
use ratatui_textarea::{Input, Key, TextArea};

pub struct Composer {
    textarea: TextArea<'static>,
}

impl Default for Composer {
    fn default() -> Self {
        Self::new()
    }
}

impl Composer {
    pub fn new() -> Self {
        let mut textarea = TextArea::default();
        textarea.set_block(Block::default().borders(Borders::ALL).title("Input"));
        Self { textarea }
    }

    pub fn input(&mut self, key: crossterm::event::KeyEvent) {
        self.textarea.input(Input::from(key));
    }

    pub fn handle_mouse(&mut self, mouse: crossterm::event::MouseEvent, area: Rect) {
        let _ = (mouse, area); // TODO 根据 ratatui-textarea 鼠标 API 调整
    }

    pub fn lines(&self) -> Vec<&str> {
        self.textarea.lines()
    }

    pub fn join(&self) -> String {
        self.textarea.lines().join("\n")
    }

    pub fn clear(&mut self) {
        self.textarea.select_all();
        self.textarea.input(Input::from(Key::Delete));
    }

    pub fn is_empty(&self) -> bool {
        self.textarea.lines().iter().all(|l| l.is_empty())
    }

    pub fn placeholder(&mut self, text: &str) {
        self.textarea.set_placeholder_text(text);
    }

    pub fn render(&mut self, area: Rect, buf: &mut Buffer) {
        self.textarea.widget().render(area, buf);
    }
}
```

- [ ] **Step 4: 编译检查**

Run: `cargo check -p legion-cli`
Expected: PASS（允许 TODO 存在，但接口必须稳定）

---

## Task 3: 在 `AppState` 中替换输入存储

**Files:**
- Modify: `crates/legion-cli/src/tui/state.rs`
- Modify: `crates/legion-cli/src/tui/render.rs`

**Interfaces:**
- Consumes: `Composer`
- Produces: `pub composer: Composer` in `AppState`，替代 `input: String` + `cursor: usize`

- [ ] **Step 1: 修改 `AppState` 字段**

```rust
// crates/legion-cli/src/tui/state.rs
use crate::tui::composer::Composer;

pub struct AppState {
    // 删除：pub input: String,
    // 删除：pub cursor: usize,
    pub composer: Composer,
    // ... existing
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            composer: Composer::new(),
            // ...
        }
    }
}
```

- [ ] **Step 2: 在 `render.rs` 的 `draw_ui` 中使用 `Composer::render` 替代 `Paragraph` 输入框**

- [ ] **Step 3: 编译修复**

Run: `cargo check -p legion-cli`
Expected: PASS 或明确列出需要迁移的调用点

---

## Task 4: 迁移按键与鼠标事件处理

**Files:**
- Modify: `crates/legion-cli/src/tui/events.rs`
- Modify: `crates/legion-cli/src/tui/input.rs`

**Interfaces:**
- Consumes: `Composer::input`, `Composer::handle_mouse`
- Produces: 保持 `OutboundControl::Message` / `ShellCommand` 行为不变

- [ ] **Step 1: 把 `handle_key_event` 中所有直接读写 `state.input` / `state.cursor` 的逻辑改为调用 `state.composer.input(key)`**

- [ ] **Step 2: 保留以下特殊键的自定义处理（在交给 Composer 之前拦截）：**
  - `Enter`：发送消息（Composer 不处理）
  - `↑/↓`：历史召回（Composer 不处理）
  - `Tab`：slash 补全（Composer 不处理）
  - `Ctrl+Q`：退出

- [ ] **Step 3: 把 `handle_mouse_event` 中输入区鼠标事件转发给 `Composer::handle_mouse`**

- [ ] **Step 4: 运行事件相关单元测试**

Run: `cargo test -p legion-cli tui::events`
Expected: PASS（测试需要同步更新为 Composer API）

---

## Task 5: 迁移历史、粘贴、slash 补全

**Files:**
- Modify: `crates/legion-cli/src/tui/input.rs`
- Modify: `crates/legion-cli/src/tui/events.rs`

- [ ] **Step 1: 修改 `input.rs` 中的历史加载函数，从 `state.composer.join()` 读取当前输入，用 `state.composer.clear()` + 逐行插入恢复历史**

- [ ] **Step 2: 修改粘贴折叠逻辑：仍由 `input.rs::handle_paste` 检测大文本，生成 placeholder 存入 `paste_store`，并把折叠提示写入 `Composer`（例如 `state.composer.set_lines(&["[paste:1]"])`）**

- [ ] **Step 3: 修改 slash 补全：从 `state.composer.join()` 获取当前输入以匹配命令**

- [ ] **Step 4: 运行完整 TUI 测试**

Run: `cargo test -p legion-cli tui::`
Expected: PASS

---

## Task 6: 回归测试与手动验证

**Files:**
- N/A

- [ ] **Step 1: 运行完整 workspace 检查**

Run:
```bash
cargo build --workspace --all-targets
cargo clippy --workspace --all-targets
cargo test --workspace --all-targets
cargo fmt -- --check
```
Expected: 全绿

- [ ] **Step 2: 手动验证清单**

- [ ] 多行输入与发送
- [ ] `↑/↓` 历史召回
- [ ] 鼠标拖拽选择输入文本
- [ ] `Ctrl+Z` / `Ctrl+Y` undo/redo
- [ ] 粘贴 >1000 字符自动折叠
- [ ] `/help` slash 命令补全
- [ ] `!ls` shell 模式
- [ ] `Ctrl+Q` 退出

---

## Self-Review

- **Spec coverage:** 依赖调研、适配层、状态替换、事件迁移、历史/粘贴/补全、回归测试都有任务。
- **Placeholder scan:** Task 1 调研结果会影响版本号，但代码骨架使用占位版本；需要在 Task 1 后回填准确版本。
- **Type consistency:** `Composer` 接口在 Task 2-5 中一致。
