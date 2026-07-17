# T2 — Legion TUI 代码块语法高亮

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在 TUI 的 fenced code block 中根据语言标签应用语法高亮。

**Architecture:** 引入 `syntect` 作为 `legion-cli` 依赖；在 `markdown.rs` 的代码块渲染路径中，使用 `syntect::easy::HighlightLines` 把源码行转成 `Vec<(syntect::highlighting::Style, &str)>`，再映射为 ratatui `Span`；失败时回退到当前统一背景渲染。

**Tech Stack:** Rust, `ratatui 0.29`, `syntect 5.3`.

---

## Global Constraints

- 新增依赖 `syntect` 必须加入 workspace root `Cargo.toml` 的 `[workspace.dependencies]`，并在 `legion-cli/Cargo.toml` 中引用。
- 不能使用 grok-build 的 `xai-grok-markdown`；自研最小实现。
- 语法高亮必须在 streaming 结束后应用（streaming 阶段仍使用 `plain_lines`）。
- 保持二进制大小可控：`syntect` 默认带所有语法定义，需评估是否过大。

---

## Task 1: 添加 `syntect` 依赖

**Files:**
- Modify: `Cargo.toml`
- Modify: `crates/legion-cli/Cargo.toml`

**Interfaces:**
- Produces: `syntect = "5.3"` in workspace dependencies.

- [ ] **Step 1: 在 workspace root 加入依赖**

```toml
# Cargo.toml
[workspace.dependencies]
# ... existing
syntect = "5.3"
```

- [ ] **Step 2: 在 legion-cli 引用**

```toml
# crates/legion-cli/Cargo.toml
[dependencies]
# ... existing
syntect = { workspace = true }
```

- [ ] **Step 3: 编译检查**

Run: `cargo check -p legion-cli`
Expected: PASS（此时 syntect 已可用）

---

## Task 2: 创建 `syntax.rs` 高亮模块

**Files:**
- Create: `crates/legion-cli/src/tui/syntax.rs`
- Modify: `crates/legion-cli/src/tui/mod.rs`

**Interfaces:**
- Produces: `pub struct Highlighter { ps: syntect::parsing::SyntaxSet, ts: syntect::highlighting::ThemeSet }`
- Produces: `impl Highlighter { pub fn new() -> Self; pub fn highlight_lines(&self, lang: &str, source: &str, theme: &crate::tui::theme::Theme) -> Option<Vec<Line<'static>>> }`

- [ ] **Step 1: 实现 `Highlighter`**

```rust
// crates/legion-cli/src/tui/syntax.rs
use crate::tui::theme::Theme;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, ThemeSet};
use syntect::parsing::SyntaxSet;

pub struct Highlighter {
    ps: SyntaxSet,
    ts: ThemeSet,
}

impl Default for Highlighter {
    fn default() -> Self {
        Self::new()
    }
}

impl Highlighter {
    pub fn new() -> Self {
        Self {
            ps: SyntaxSet::load_defaults_newlines(),
            ts: ThemeSet::load_defaults(),
        }
    }

    pub fn highlight_lines(
        &self,
        lang: &str,
        source: &str,
        _theme: &Theme,
    ) -> Option<Vec<Line<'static>>> {
        let syntax = self.ps.find_syntax_by_token(lang)?;
        let syntect_theme = &self.ts.themes["base16-ocean.dark"];
        let mut h = HighlightLines::new(syntax, syntect_theme);
        let mut lines = Vec::new();
        for line in source.lines() {
            let highlighted = h.highlight_line(line, &self.ps).ok()?;
            let spans: Vec<Span<'static>> = highlighted
                .into_iter()
                .map(|(style, text)| syntect_style_to_span(style, text.to_string()))
                .collect();
            lines.push(Line::from(spans));
        }
        Some(lines)
    }
}

fn syntect_style_to_span(style: syntect::highlighting::Style, text: String) -> Span<'static> {
    let mut ratatui_style = Style::default().fg(syntect_color_to_ratatui(style.foreground));
    if style.font_style.contains(FontStyle::BOLD) {
        ratatui_style = ratatui_style.add_modifier(Modifier::BOLD);
    }
    if style.font_style.contains(FontStyle::ITALIC) {
        ratatui_style = ratatui_style.add_modifier(Modifier::ITALIC);
    }
    if style.font_style.contains(FontStyle::UNDERLINE) {
        ratatui_style = ratatui_style.add_modifier(Modifier::UNDERLINED);
    }
    Span::styled(text, ratatui_style)
}

fn syntect_color_to_ratatui(c: syntect::highlighting::Color) -> Color {
    Color::Rgb(c.r, c.g, c.b)
}
```

- [ ] **Step 2: 把 `syntax` 模块加入 `tui/mod.rs`**

```rust
pub mod syntax;
```

- [ ] **Step 3: 编译检查**

Run: `cargo check -p legion-cli`
Expected: PASS

---

## Task 3: 在 `markdown.rs` 中接入高亮

**Files:**
- Modify: `crates/legion-cli/src/tui/markdown.rs`
- Modify: `crates/legion-cli/src/tui/state.rs`（持有 `Highlighter`）

**Interfaces:**
- Consumes: `Highlighter::highlight_lines`
- Produces: `AppState.highlighter: Highlighter`

- [ ] **Step 1: 修改 `emit_code_block` 函数签名，接收 `highlighter` 和 `theme`**

```rust
fn emit_code_block(
    lang: &str,
    content: &str,
    theme: &Theme,
    highlighter: &Highlighter,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    // header line
    lines.push(Line::from(vec![
        Span::styled("─ ".to_string(), Style::default().fg(theme.code_bg)),
        Span::styled(lang.to_string(), Style::default().fg(theme.status_fg).add_modifier(Modifier::BOLD)),
    ]));

    if let Some(highlighted) = highlighter.highlight_lines(lang, content, theme) {
        lines.extend(highlighted);
    } else {
        for line in content.lines() {
            lines.push(Line::from(Span::styled(line.to_string(), Style::default().bg(theme.code_bg))));
        }
    }
    lines
}
```

- [ ] **Step 2: 在 `AppState` 中持有 `Highlighter`**

```rust
// crates/legion-cli/src/tui/state.rs
use crate::tui::syntax::Highlighter;

pub struct AppState {
    // ... existing
    pub highlighter: Highlighter,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            // ...
            highlighter: Highlighter::new(),
        }
    }
}
```

- [ ] **Step 3: 在 `markdown_lines` 调用链中传递 `highlighter` 和 `theme`**

- [ ] **Step 4: 运行 markdown 单元测试**

Run: `cargo test -p legion-cli markdown_lines`
Expected: PASS（未高亮的代码块仍正常显示）

---

## Task 4: 评估二进制大小与性能

**Files:**
- N/A（仅测量）

- [ ] **Step 1: 测量 release 二进制大小变化**

Run before/after:
```bash
cargo build -p legion-cli --release
ls -lh target/release/legion
```

Expected: 记录到本任务 PR 描述中；如果增长 >5MB，考虑 `syntect` 的 `default-syntaxes` feature 裁剪。

- [ ] **Step 2: 测量首次高亮耗时**

Run: 在 TUI 中发送包含 `rust` 代码块的消息，主观观察是否有明显卡顿。

Expected: 首次 ≤200ms，后续缓存后无感知。

---

## Task 5: 单元测试高亮行为

**Files:**
- Modify: `crates/legion-cli/src/tui/render.rs` 测试区（或新建 `syntax.rs` 测试）

- [ ] **Step 1: 添加高亮单元测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::theme::Theme;

    #[test]
    fn highlight_rust_produces_colored_spans() {
        let h = Highlighter::new();
        let theme = Theme::default();
        let lines = h.highlight_lines("rust", "let x = 1;", &theme).unwrap();
        assert!(!lines.is_empty());
        assert!(!lines[0].spans.is_empty());
    }

    #[test]
    fn unknown_language_returns_none() {
        let h = Highlighter::new();
        let theme = Theme::default();
        assert!(h.highlight_lines("not_a_real_lang", "foo", &theme).is_none());
    }
}
```

- [ ] **Step 2: 运行测试**

Run: `cargo test -p legion-cli syntax`
Expected: PASS

---

## Self-Review

- **Spec coverage:** 依赖、Highlighter、markdown 接入、测试、性能评估都有任务。
- **Placeholder scan:** 无 TBD；代码可编译。
- **Type consistency:** `Highlighter` 接口在 Task 2-5 中一致。
