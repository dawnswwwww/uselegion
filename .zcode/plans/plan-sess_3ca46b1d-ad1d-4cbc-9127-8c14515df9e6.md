## 目标
支持通过模型名后缀（`minimax/MiniMax-M3[1m]`）或配置 per-model 表覆盖 `context_window`，仅覆盖 `context_window`（不影响 buffer_tokens / max_summary_tokens / threshold_ratio 等）。

## 优先级（高→低）
1. **模型名后缀** `[512k]` / `[1m]` / `[200000]` — 最具体，按轮次
2. **配置 per-model 表** `compaction.context_windows`
3. **全局** `compaction.context_window`（兜底）

（第③层 Provider 目录本次不做，留作后续增强。）

## 改动清单

### 1. `crates/legion-provider/src/model_ref.rs`
- 新增 `parse_context_window_suffix(model_name: &str) -> (String /* cleaned */, Option<usize>)`：
  - 识别结尾的 `[...]`，内部支持纯数字 `128000`、`k`/`K` 后缀 `512k`、`m`/`M` 后缀 `1m`。
  - 返回去掉后缀的干净名字 + 解析出的 token 数；格式非法则忽略后缀（不报错，保留原名，行为等同无后缀）。
- 在 `parse_model_ref` 内对 `model_name` 调用该函数剥离后缀。
- 不改 `ResolvedModelRef` 结构（最简方案：解析出的窗口不随 ref 传递，而是由压缩侧按需从 model_ref 重新解析，见第 4 点）。

> 决策依据：`router.rs:276/340/406` 已把 `candidate.model_name` 直接赋给 `req.model`，所以只要在 `parse_model_ref` 里剥掉 `[...]`，发给 API 的模型名就自动干净，无需改路由。

### 2. `crates/legion-provider/src/types.rs`
- 无需改 `ResolvedModelRef`。

### 3. `crates/legion-core/src/config.rs`（`CompactionConfig`）
- 新增字段 `context_windows: BTreeMap<String, usize>`，`#[serde(default)]` 默认空表。
- 在 `Default` impl 里补 `context_windows: BTreeMap::new()`。
- 已有的 `..Default::default()` 测试会自动兼容（无需逐个改测试）。
- 顶部 `use std::collections::BTreeMap`（当前仅 import 了 HashMap）。

### 4. `crates/legion-runtime/src/compaction.rs`（核心优先级收敛）
- 新增 `fn effective_context_window(&self, model_ref: &str) -> usize`，按优先级链解析（**唯一真相源**）：
  1. 调 `parse_context_window_suffix` 取后缀覆盖；
  2. 查 `self.config.context_windows`（键为去掉后缀的 `provider/model` 形式）；
  3. 回退 `self.config.context_window`。
- `should_compact` 签名增加 `model_ref: &str` 参数，内部把对 `self.config.context_window` 的两处读取改为 `self.effective_context_window(model_ref)`。
- `compact_if_needed` 已接收 `model_ref: &str`（用于 summary 子调用），在调用 `self.should_compact(...)` 时把 `model_ref` 透传即可（签名不变）。
- `TwoPassCompactor::compact_if_needed` 同理透传（它已持有 `model_ref`）。
- `should_compact` 现有 4 处测试调用需补 `model_ref` 实参（如 `""` 或无后缀模型名）。

### 5. `crates/legion-runtime/src/run_loop.rs`
- 无改动：`compact_if_needed` 签名未变，已传 `&request.model_ref`。

### 6. 文档
- `docs/design/gaps/03-shallow/compaction.md` 追加一条：已支持模型名后缀 + per-model 配置表覆盖 `context_window`；Provider 目录（ModelInfo.context_window）接入为后续增强项。

## 测试
- `model_ref.rs`：`[1m]`/`[512k]`/`[200000]` 解析正确；非法后缀（如 `[abc]`、`[1.5m]`）被忽略且名字保留；OpenRouter 多斜杠 `openrouter/x/y[1m]` 仅剥末尾后缀。
- `compaction.rs`：后缀覆盖优先于全局；per-model 表覆盖全局；后缀优先于 per-model 表；无任何覆盖时回退全局。
- 运行 `cargo test -p legion-provider -p legion-runtime -p legion-core`，再跑全工作区 `build`/`clippy`/`test`/`fmt --check` 收口。

## 不做
- 不覆盖 buffer_tokens / max_summary_tokens / threshold_ratio。
- 不接入 Provider 目录（ModelInfo.context_window）。
- 不改 `ResolvedModelRef`、`router.rs`、`run_loop.rs` 签名。