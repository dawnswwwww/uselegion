# Gap:Session Resume 缺健壮性(无 boundary 恢复/orphan 修复)

| 字段 | 值 |
|---|---|
| 类目 | [03-shallow](./_index.md)(内核浅化) |
| 优先级 | P2(可恢复性) |
| 工作量 | M(1-2 人周) |
| 前置依赖 | [compaction](./compaction.md)(提供 `BoundaryMark`);[multi-agent](../02-missing/multi-agent.md)(提供 sidechain) |
| 关联 PRD | `agent-harness-prd.md` §15 D2(Transcript) |
| 关联分析 | `claude-code-analysis/analysis/04i-session-storage-resume.md` |

---

## 1. 现状证据

legion 的 session 存储与 resume **基础可用**,但缺健壮性:

- **JSONL transcript 真实**:`legion-gateway/src/session_store.rs:1-191`,路径 `<base>/agents/<agent_id>/sessions/<peer_id>.jsonl`,append-only,有损坏行跳过测试(`:210-303`)。
- **resume 已接入**:`websocket.rs:569` 的 `session_store.load(&session_key)` 把历史塞进 `agent_params.history`,经 `agent_rpc.rs:111` 传入 RunRequest。
- **无 compact boundary 恢复**:transcript 是原始消息流,compaction 只在运行时内存,**结果不写回 store**([compaction](./compaction.md) §1)。resume 时全量加载——**若会话曾 compact,旧消息 + summary 混在一起**,模型上下文混乱。
- **无 orphan tool_result 修复**:并行工具执行若中途中断,resume 后可能出现孤立的 `tool_result`(无对应 `tool_use`),违反 API 不变量。
- **无 sidechain**:`legion-gateway/src/session_store.rs` 无子 agent 独立 transcript(主链/子链混存)。
- **无 lite reader**:`list_sessions`(`:118`)全量 parse 每个 transcript 提取摘要,大量 session 时慢。
- **无 session TTL/归档/删除**。

**结论**:resume 在"短会话、无 compact、无中断"下工作,但长会话/中断/compact 后恢复不可靠。

---

## 2. 设计目标(对照七条原则)

- **P1 扩展性**:`TranscriptLoader` trait 化(供未来多存储后端)。
- **P2 安全**:resume 不注入违反 API 不变量的消息(orphan 修复);损坏行不崩。
- **P3 增量**:无 boundary 的旧 transcript 仍可加载(退化全量)。
- **P4 证据**:现状见 §1;借鉴见 §6。
- **P5 可观测**:resume drift、orphan 修复、boundary 截断产生 `tracing`。
- **P6 失败显式**:文件损坏、boundary 缺失、一致性告警分类处理。
- **P7 测试**:boundary 恢复、orphan 修复、lite reader、损坏行跳过(已有)测试。

---

## 3. 架构设计

### 3.1 借鉴 Claude Code 的"transcript 是可修复图"(04i)

Claude Code 把 transcript 视为**可修复的图结构**而非静态数组。legion 复刻三层处理:

```
load_for_resume(path)
   ▼ (1) boundary 扫描:找最近 BoundaryMark(来自 compaction)
   ▼      只读 post-boundary 有效消息 + 单独扫 pre-boundary metadata
   ▼ (2) orphan 修复: recover_orphaned_tool_results
   ▼      补齐/剔除孤立的并行 tool_result
   ▼ (3) 一致性检查: check_resume_consistency
   ▼      报告 drift(如缺失 tool_use、空白 assistant)
   ▼
ResumedSession { messages, metadata, boundary }
```

### 3.2 boundary 感知恢复(借鉴 04i §6.2 `readTranscriptForLoad`)

```
文件结构(post-compaction):
   [旧消息...] [BoundaryMark entry] [summary + kept messages...] [新消息...]

load:
   - 扫描找到最后一个 BoundaryMark
   - 只加载 boundary 之后的 messages(有效上下文)
   - 单独扫描 boundary 之前的 metadata(title/tag/agent-setting)保留
   - 无 boundary → 退化全量加载(兼容旧 transcript)
```

### 3.3 orphan tool_result 修复(借鉴 04i §8 `recoverOrphanedParallelToolResults`)

并行工具执行 `partition_tool_calls`([详见 approval/tool pipeline])可能产生多个 tool_result。若 turn 中断:

```
resume 前消息:[user, assistant(tool_use A, tool_use B), tool_result A]   // B 缺失
   ▼
检测:tool_result A 有对应 tool_use,但 tool_use B 无 result
   ▼
修复策略(可配):
   - drop_orphan: 删除无 result 的 tool_use + 删除孤立 tool_result
   - synthesize: 为缺失 result 补 "[interrupted]" 占位(保 API 合法)
```

### 3.4 sidechain(配合 multi-agent)

子 agent transcript 写入 `sessions/<peer>/subagents/agent-<id>.jsonl`(见 [multi-agent](../02-missing/multi-agent.md) §4.4),不混主链。resume 时按需加载 sidechain。

### 3.5 lite reader(借鉴 04i §6.1 `LITE_READ_BUF_SIZE = 65536`)

`list_sessions` 只读每个 transcript **头尾 64KB** 提取首条 prompt/title/tag,不全量 parse。

---

## 4. 接口设计(Rust)

### 4.1 TranscriptLoader(`legion-gateway/src/session_store.rs` 扩展)

```rust
use async_trait::async_trait;

#[async_trait]
pub trait TranscriptLoader: Send + Sync {
    /// boundary 感知加载(供 resume)。
    async fn load_for_resume(&self, key: &SessionKey) -> Result<ResumedSession, ResumeError>;

    /// 轻量读取(只读头尾,供 list)。
    async fn lite_read(&self, key: &SessionKey) -> Result<SessionSummary, ResumeError>;

    /// 全量加载(无 boundary 兼容路径)。
    async fn load_full(&self, key: &SessionKey) -> Result<Vec<TranscriptEntry>, ResumeError>;
}

#[derive(Debug, Clone)]
pub struct ResumedSession {
    pub messages: Vec<Message>,          // post-boundary 有效部分
    pub metadata: SessionMetadata,       // title/tag/agent_setting/mode
    pub boundary: Option<BoundaryMark>,  // 来自 compaction
    pub consistency: ConsistencyReport,
}

#[derive(Debug, Clone, Default)]
pub struct SessionMetadata {
    pub title: Option<String>,
    pub tags: Vec<String>,
    pub agent_setting: Option<serde_json::Value>,
    pub mode: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ConsistencyReport {
    pub orphan_tool_uses: usize,
    pub orphan_tool_results: usize,
    pub empty_assistant: usize,
    pub drift: Vec<String>,
}
```

### 4.2 修复函数

```rust
pub enum OrphanPolicy { DropOrphan, Synthesize }

pub fn recover_orphaned_tool_results(
    msgs: &mut Vec<Message>, policy: OrphanPolicy,
) -> RepairReport {
    // 1. 建 tool_use_id → tool_result 映射
    // 2. 找无 result 的 tool_use / 无 use 的 result
    // 3. 按 policy 修复
}

pub fn check_resume_consistency(msgs: &[Message]) -> ConsistencyReport {
    // 检测:孤立 use/result、空白 assistant、中断 turn
}
```

### 4.3 sidechain 路径

```rust
impl SessionStore {
    pub fn subagent_path(&self, parent: &SessionKey, handle_id: &str) -> PathBuf {
        // sessions/<peer>/subagents/agent-<handle_id>.jsonl
    }
    pub fn append_subagent(&self, parent: &SessionKey, id: &str, e: TranscriptEntry) -> Result<()>;
}
```

### 4.4 配置 schema(`legion-core`)

```jsonc
{
  "sessions": {
    "orphanPolicy": "synthesize",      // dropOrphan | synthesize
    "liteReadBufferBytes": 65536,
    "ttlDays": 90,                      // 0 = 永不归档
    "archiveDir": "~/.legion/archive"
  }
}
```

---

## 5. 集成点

| 位置 | 改动 |
|---|---|
| `legion-gateway/src/session_store.rs:118,178-191` | `TranscriptLoader` trait + boundary 感知 `load_for_resume` + `lite_read`;`list_sessions` 改走 lite reader。 |
| `legion-gateway/src/websocket.rs:569` | resume 改用 `load_for_resume`(替代全量 load)。 |
| `legion-runtime/src/compaction.rs` | compact 时写 `BoundaryMark` entry 到 transcript(已在 compaction §4.1 定义)。 |
| 新增 `legion-gateway/src/transcript_repair.rs` | `recover_orphaned_tool_results`/`check_resume_consistency`。 |
| `legion-core/src/config.rs` | `sessions` 配置(orphanPolicy/liteReadBuffer/ttl)。 |

---

## 6. 风险与权衡

### 6.1 boundary 缺失的兼容(增量关键)
旧 transcript 无 `BoundaryMark`。**`load_for_resume` 检测无 boundary → 退化全量加载**(等价当前行为),保证升级不破坏已有 session。

### 6.2 orphan 修复策略选择
- `DropOrphan`:丢弃不完整对,干净但可能丢上下文。
- `Synthesize`:补 `[interrupted]` 占位,保 API 合法但模型可能困惑。
**默认 `Synthesize`**(借鉴 Claude 保 API 合法优先),可配 `DropOrphan`。

### 6.3 sidechain 与 multi-agent 协同
sidechain 路径依赖 [multi-agent](../02-missing/multi-agent.md) 落地。若 multi-agent 未实现,sidechain 路径无写入方,本 gap 仍可独立交付 boundary/orphan/lite 部分。

### 6.4 lite reader 的精度
只读头尾 64KB 可能漏掉首条 prompt(若极长)。**缓解**:首条 prompt 通常在头部 64KB 内;`SessionSummary` 标记 `truncated: true` 提示。

### 6.5 TTL/归档的误删风险
自动归档老 session 可能误删用户重要记录。**缓解**:TTL 默认 0(永不归档);归档而非删除(移到 archiveDir,可恢复);归档前 `tracing::warn`。

### 6.6 远端 ingress 副本(延后)
Claude Code 04i §5 有远端 session 副本(PUT entry + Last-Uuid 乐观并发)。legion 当前单 Gateway 无远端,延后。

---

## 7. 实现路线图

### 阶段 A(Phase B,~0.5 人周):boundary 感知恢复 — ✅ 已落地(2026-07-11,见 DEVLOG)
1. `BoundaryMark` entry 写入 transcript(compaction Phase D 已完成)。✅
2. `RunEvent::Compaction` 携带 `resume_head`(compacted 历史去掉 leading system prompt——resume 时 system prompt 由 workspace 重建);gateway 在 `append_boundary` 后立即把 `resume_head` 持久化到 boundary 之后(transcript 结构:`[旧消息][boundary][summary+reattachments+kept tail][新消息]`,对齐 Claude Code post-compaction 结构)。✅
3. `SessionStore::load_for_resume`:扫描最后一个 boundary,只返回其后的消息;无 boundary 退化全量(旧 transcript 兼容);`websocket.rs` resume 接线改用 `load_for_resume`。✅
4. **验收**:compact 过的会话 resume 只加载 boundary 后有效消息(`load_for_resume_returns_only_post_boundary_messages`);旧 transcript 全量加载(`load_for_resume_without_boundary_loads_all`);多 boundary 取最后(`load_for_resume_uses_last_boundary`);boundary 附近损坏行跳过(`load_for_resume_skips_corrupt_lines_around_boundary`);resume_head 内容断言(summary 打头、无重建 system prompt、kept tail 收尾)。✅

### 阶段 B(Phase B,~0.5 人周):orphan 修复 + 一致性检查 — ✅ 已落地(2026-07-11,见 DEVLOG)
1. 新建 `legion-gateway/src/transcript_repair.rs`:`recover_orphaned_tool_results`(orphan result 两策略皆丢弃;orphan use 按策略——`Synthesize` 在既有 result 后补 `[interrupted]` 占位,`DropOrphan` 剔除未应答 call、空 assistant 一并丢弃)+ `check_resume_consistency`(只读,统计 orphan use/result/empty assistant + drift 描述)。✅
2. resume 前自动修复:`websocket.rs` resume 加载后立即按 `sessions.orphanPolicy`(默认 `synthesize`,legion-core `SessionsConfig`)修复,非 clean 时 `tracing::warn` 记录 drift。✅
3. **验收**:中断的并行 tool turn resume 后无孤立 result(`interrupted_parallel_turn_gets_synthesized_result`);drop 策略剔除 call 与空 assistant;orphan result 双策略丢弃;一致性检查只读统计;配置解析(默认 synthesize / 显式 dropOrphan)。✅

### 阶段 C(Phase C,~0.5 人周):lite reader + TTL/归档 — ✅ 已落地(2026-07-11,见 DEVLOG)
1. `SessionStore::lite_read(agent, peer, buffer_bytes)`:只读 transcript 头部(默认 64 KiB,`sessions.liteReadBufferBytes`)提取首条 user prompt(截 200 字符),文件超 buffer 标 `truncated`;`list_session_summaries` 批量。(注:既有 `list_sessions` 本就只列文件名,无全量 parse 问题。)✅
2. TTL 归档:`sessions.ttlDays`(默认 0=永不)+ `sessions.archiveDir`(默认 `~/.legion/archive`);`SessionStore::archive_expired` 按最后一条 entry 时间戳(只读文件尾 8 KiB)判定,**移动**而非删除到 `<archive>/agents/<agent>/sessions/`,恢复=移回;gateway `start`/`start_bound` 启动时一次性执行,归档数 `info` 日志。✅
3. **验收**:`lite_read_extracts_first_prompt`/`lite_read_marks_truncated_when_file_exceeds_buffer`/`list_session_summaries_covers_all_peers`;`archive_expired_moves_old_transcripts_and_keeps_recent`(含移回恢复)+ `archive_expired_zero_ttl_is_noop`;配置解析 4 项(默认/显式)。✅

### 阶段 D(随 multi-agent):sidechain — ✅ 已落地(随 multi-agent Phase A,2026-07-10)
1. 子 agent transcript 写 sidechain 路径:`legion-runtime/src/subagent.rs` 的 `write_sidechain` 把子 agent 事件流写到 `~/.legion/agents/<child>/sessions/subagent-<handle>.jsonl`,与父链(`<peer>.jsonl`)完全分离。✅
2. resume 按需加载 sidechain:父链 resume 不读 sidechain(隔离即语义),sidechain 供 `legion tasks show` 等按需查阅。✅
3. **验收**:子 agent transcript 不混主链(multi-agent Phase A 测试覆盖,见 subagent.rs `write_sidechain` 测试)。✅

---

## 8. 验收标准

- [x] compact 过的会话 resume 只加载 boundary 后有效消息(boundary 恢复测试)。(Phase A)
- [x] 无 boundary 的旧 transcript 仍全量加载(兼容回归)。(Phase A)
- [x] 中断的并行 tool turn resume 后无孤立 tool_result(orphan 修复测试)。(Phase B)
- [x] resume 前运行 `check_resume_consistency`,drift 记入 `tracing`。(Phase B;`recover` 返回的 report 非 clean 即 warn)
- [x] `list_sessions` 用 lite reader,不全量 parse(性能测试)。(Phase C:`lite_read` 头读 + `list_session_summaries`;`list_sessions` 本就只列文件名)
- [x] 子 agent transcript 写 sidechain,不混主链(随 multi-agent Phase A 已落地:`subagent-<handle>.jsonl` 独立文件)。
- [x] 损坏行跳过不崩(回归已有测试)。
- [x] TTL 归档(默认关)可恢复。(Phase C)
- [x] `AGENTS.md` 更新 session/resume 章节。(Phase A/B/C)

---

*上一篇:[`compaction.md`](./compaction.md) · 下一个 gap:[`prompt-management.md`](./prompt-management.md) · 返回总览:[`../00-overview.md`](../00-overview.md)*
