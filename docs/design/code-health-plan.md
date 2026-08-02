# 代码健康与复用优化计划

> 来源:2026-07-23 全库只读分析(4 路并行:架构分层 / provider+channel 适配层 / 超大文件 / 横切模式)。
> 进度记录同步到 `docs/DEVLOG.md`;完成一项勾掉一项并标注日期。
> 每条发现均附 `文件:行号` 证据;行号以分析时点为准,重构后以实际为准。

## 进度总览

| # | 项目 | 状态 | 完成日期 |
|---|------|------|---------|
| P1 | tools.rs 按域拆分(17 工具 / 5 域,生产 ~2000 行) | ✅ | 2026-07-23 |
| P2 | provider 层去重(SSE 管线 / 错误映射 / tool-call 累加器 / 会话转换 helper / router 骨架) | ✅ | 2026-07-23 |
| P3 | channel 层去重(生命周期骨架 / parse_config / post_json / 重连循环) | ✅ | 2026-07-23 |
| P4 | legion-core 共享工具(atomic_write / expand_tilde / legion_home)+ 调用点迁移 | ✅ | 2026-07-23 |
| P5 | resolve_model 收敛(6 份复制 + 1 处硬编码 → 单一 helper) | ✅ | 2026-07-23 |
| P6 | 依赖管理收敛(async-trait ×13 等 → workspace 继承) | ✅ | 2026-07-23 |
| P7 | channel 入站接入 host prepare/drive 管线(历史 + 持久化 + 模型解析) | ✅ | 2026-07-23 |
| P8 | 协议层对齐(Features 清单过期 / sessions.history 双份 / CLI 握手类型化) | ✅ | 2026-07-23 |
| P9 | session key 单一事实源(parse/build 收敛,修 flow:/a2a: 非法 key) | ✅ | 2026-07-23 |
| P10 | 跨 crate JSON-RPC 三份实现(mcp/lsp/acp)下沉 | ✅ | 2026-07-23 |
| P11 | run_loop god function 拆解(RunContext 打包 + prepare_turn/run_iteration) | ✅ | 2026-07-23 |
| P12 | gateway_manager.rs 拆分(installer vs ops)+ 补测试 | ✅ | 2026-07-23 |
| P13 | async 路径 std::Mutex lock().unwrap()(61 处)治理 | ✅ | 2026-07-23 |

> 全部 13 项已于 2026-07-23 完成。各节保留原始分析作为依据;实际实现摘要见 `docs/DEVLOG.md` 当日条目。

---

## P1 tools.rs 拆分 【证据充分,低风险】

`crates/legion-tools/src/tools.rs` 生产 ~1994 行(总行 3148,其余为内联测试),含 17 个 `Tool` 实现,天然分 5 个不耦合的域:

- 文件系统:`resolve_tool_path`/Read/Write/Edit/ApplyPatch/`apply_unified_diff`(tools.rs:35–545)
- 执行:Exec(546–687)
- Web:WebFetch/WebSearch + `parse_duckduckgo_lite`/`strip_html`(688–848, 1145–1213)
- Memory ×3(849–1144)
- 编排:SpawnSubagent/AgentToAgentSend/RunCoordinator/Swarm ×3(1214–1986)

同 crate 已有"一工具一文件"先例(`grep.rs`/`list_dir.rs`/`ask_user.rs`/`browser.rs`/`scheduler.rs`/`todo.rs`,见 `registry.rs:13–34`),tools.rs 是历史遗留。机械移动,测试随生产代码一并迁移。

## P2 provider 层去重 【~500–650 行】

全部为 `crates/legion-provider/src/` 内部:

- router.rs 的 `chat()`(173–288)与 `embed()`(291–402)retry/timeout/tracing 骨架近乎逐行相同(~70 行 ×2);`generate_image`/`synthesize_speech`/`generate_video`(409–548)三个同构 fallback 循环 → 泛型 helper,~150–160 行
- HTTP 错误映射样板 8 处:openai.rs:175-184/248-254/280-286、anthropic.rs:253-262、gemini.rs:119-125、ollama.rs:160-166、bedrock.rs:151-157/240-246 → `check_status`,~55 行
- SSE → JSON 管线 3 处:openai.rs:186-206、anthropic.rs:264-283、gemini.rs:127-139 → 共享流 helper;顺带统一 gemini 的 JsonParse/SseParse 不一致(gemini.rs:135 vs openai.rs:199),~50 行
- 流式 tool-call 累加器:openai.rs:351-400 与 bedrock.rs:386-465 同构(HashMap<index, Partial>),anthropic.rs:294-430 为退化形态 → 共享累加数据结构,保留各自吐出策略,~60–80 行
- 会话 → wire 转换共享子逻辑:system 合并(anthropic.rs:83-99 / gemini.rs:196-214 / bedrock.rs:265-271)、tool-result 分组(anthropic.rs:108-146 / bedrock.rs:313-330)、tool arguments 解析(anthropic.rs:198 / bedrock.rs:295 / gemini.rs:239 同一行 unwrap_or_else)→ 小 helper,~150–200 行;**保留各家边界差异**(如 anthropic.rs:140 跳过空 content)
- 小件:client 构造 ×3、`build_request` ×2(openai.rs:151 / anthropic.rs:219)、`role_str` ×2(openai.rs:293 / ollama.rs:241)、`is_prompt_too_long` ×2(openai.rs:302 / anthropic.rs:228)、`extra` 合并 ×5 → `ChatRequest::extra.apply_to()`
- **顺手修复**:router timeout 只包到 stream 建立(router.rs:217-229),流中途挂起不受约束 → 在共享 SSE helper 上加 per-chunk idle timeout

## P3 channel 层去重 【~500–600 行】

`crates/legion-channel/src/` 五家 provider(telegram/lark/slack/discord/matrix):

- 生命周期骨架(struct/new/start/stop)×5 → 共享基座;**保留 telegram `handle.await` vs 其余 `abort()` 的差异**(telegram.rs:121),作为策略参数,~150 行
- `parse_config` 双键 fallback(appId/app_id)×5 → `cfg_str` helper,~100 行(lark.rs:229-271 等)
- `send()` POST-查状态样板 ~7 份 → `post_json`;slack 的 `ok` / lark 的 `code` 业务码检查参数化,~120 行
- 固定 5s 重连循环 7 处(lark.rs:594、slack.rs:348、discord.rs:337、matrix.rs:390/395、telegram.rs:324/354)→ 重连+退避 helper,~90 行
- WS read 循环 ×3(lark.rs:600-635、slack.rs:354-392、discord.rs:342-457)→ 薄壳,~60 行
- **不做**:长消息拆分(Telegram 4096 / Discord 2000)是功能缺口非重复,另行立项;live 路径无凭据未 E2E,重构后仅靠单测验证

## P4 legion-core 共享工具 【机械,低风险】

- `atomic_write`(tmp+rename)7 份 → legion-core 提供同步/异步两版:gateway_manager.rs:242-251、provider/ops.rs:353-367、runtime/todo.rs:149-165、runtime/surfaced.rs:82-88、runtime/goal.rs:~190、automation/tasks.rs:~215、automation/cron.rs:~200;另有 provider/auth.rs:176,235 用 NamedTempFile
- `expand_tilde` 3 份且**行为不一致**(telemetry/lib.rs:22 用 `dirs::home_dir()` vs runtime/lib.rs:78 用 `HOME` env;cli/main.rs:524 注释自认复制)→ legion-core 单一实现 + `legion_home()`,统一 HOME 解析散落 7+ 处
- "LLM 调用+timeout+流累积+抠 JSON 数组"模板 4 份 → `chat_text_with_timeout` + `extract_json_array`:recall_selector.rs:66-90/123、skill_selector.rs:135-160/166-167、auto_extract.rs:91-115/176-177、commitments.rs:123/239-240

## P5 resolve_model 收敛 【机械】

同一函数体 6 份:host/turn.rs:147、gateway/gateway.rs:445、automation/cron.rs:588、automation/task_runner.rs:206、automation/flow.rs:268、runtime/subagent.rs:380;另有 channel/lib.rs:218 硬编码 `"openai/gpt-4o"`。归宿:`legion-provider/src/model_ref.rs:41` 已有相邻职能的 `resolve_model_ref`。

## P6 依赖管理收敛 【机械】

- `async-trait = "0.1"` 在 13 个 crate 重复声明且不在 workspace.dependencies → 提升
- `chrono` ×3(automation:20、cli:34、tools:28)、`dirs`(memory:20)、`tokio-tungstenite`(gateway:38 dev)、`tokio-test` ×3 → workspace 继承
- `reqwest`(provider:17)钉版本固化 rustls-tls → 改 workspace 继承 + features 追加(legion-mcp:14 已有正确示范)

## P7 channel 入站接入 host 管线 【价值最高,需先确认意图】

现状:`channel/lib.rs:114-310` 的 `route_inbound_to_runtime` 自行构造 `RunRequest`,不调 `.with_history()`(:238-248)、不写 SessionStore(:268-309 只收集 delta 回发)、模型硬编码(:218 注释自称 "MVP default")。对照 WS `agent` RPC 走 `host/turn.rs` 的 `prepare_run`(load_for_resume + orphan 修复,:25-65)+ `drive_run_stream`(转录落盘 + compaction boundary,:244-293,调用点 ws_rpc.rs:356-386)。

影响面:Telegram/Slack/Discord/Lark/Matrix + webchat dashboard(ws_rpc.rs:292-294 → gateway.rs:101-118)全部无状态。修复后同时解决:无历史、无持久化、模型硬编码(P5 的第 7 处)。

**开放问题**:MVP 权宜还是缺陷(gaps 文档未记载);channel 无状态是否影响并发语义(同一会话多条入站消息的串行化)。

## P8 协议层对齐

- `protocol/websocket.rs:145-157` `Features::default()` 只列 11 个方法,ws_rpc.rs:176-203 实际分发 27 个 → 补齐
- `sessions.history` 逻辑双份:ws_rpc.rs:539-598 vs cli/driver.rs:413-430(注释都成对)→ 提取为 legion-host 公共函数
- CLI 握手裸 `json!` 拼帧(cli/lib.rs:224-258)而非已定义的 `WsFrame::Connect`/`ConnectParams` → 类型化
- 小件:`iso_now()` ×3、`uuid_like()` ×3、JSON5/JSON config 加载 ×3、ws_rpc 蛇形字段名(:391-394)与 camelCase 约定不一致

## P9 session key 单一事实源

已完成(2026-07-23):权威 parse/build 收敛到 `legion-plugin-sdk/src/session_key.rs`(`SessionKeyParts`/`parse_session_key`/`build_session_key`/`direct_session_key`/`is_safe_segment`;归属 plugin-sdk 因其拥有 `PeerKind` 且 runtime/host/automation/cli/channel 均已依赖它,core 反向依赖不成立)。host/routing、turn、channel_inbound、session、session_tools、goal_tools、runtime/goal、subagent、automation/cron/flow/heartbeat/task_runner、cli/lib/tui 全部复用。flow(5 段)与 a2a(4 段)非法 key 改为合法 7 段(`agent:<a>:flow:flow:default:direct:<flow>-<step>`、`agent:<to>:a2a:a2a:default:direct:<from>`),未扩展格式;a2a 模型硬编码 `openai/gpt-4o` 改 `resolve_agent_model`;cron 本地 atomic_write 迁移 `legion_core::fs::atomic_write_async`。

遗留(有意不做):持久化布局分裂 —— plan-mode 在 `~/.legion/sessions/`(agent_loop.rs:379)而非 `~/.legion/agents/`,归档清理(session.rs:367)覆盖不到;涉及磁盘数据迁移,另行立项。

## P10 跨 crate JSON-RPC 【中优先级】

三份独立实现:mcp/client.rs:97-143(pending map + oneshot 分派)、tools/lsp.rs:154-230(与 MCP stdio 几乎逐行相同,仅错误类型与行尾 \r\n 不同)、acp/protocol.rs:6-38。可下沉极小共享模块;另 MCP 内部 list_tools/call_tool 在 4 个 transport 逐字相同(client.rs:368-383/460-473/626-639/793-806)→ trait 默认方法 + 统一 request 原语。

## P11 run_loop 拆解

`runtime/agent_loop.rs:294-911` 单函数 ~618 行、20 参数(带 `#[allow(clippy::too_many_arguments)]`),顺序堆砌 8 个阶段;todo/goal 双 gate 嵌套 match 且 goal gate 中途改写 iteration_cap(594-598、712)。→ `RunContext` 打包参数;拆 `prepare_turn()`(344–560)与 `run_iteration()`。小异味:`build_context_engine`(245–290)同一 16 行 builder 链复制两遍。

## P12 gateway_manager.rs 拆分 + 补测试

生产 ~1444 行、测试仅 ~130 行,**风险/测试比全库最失衡**(含签名校验 :633/:1424、自动回滚 :1240-1263)。单一 `impl GatewayManager`(155–1404)装两簇不耦合功能:发布/安装管线(extract/fetch/verify/download/install,447–893)与运维命令(status/upgrade/rollback/doctor,894–1404)。`upgrade()`(1144–1291)内三重 `CurrentPointer` 字面量近重复。→ 拆 installer.rs + ops.rs,补签名校验/回滚测试。

## P13 async 路径锁治理

生产 unwrap/expect 共 101 处,核心 crate 接近零;但 61 处是 async 路径 std::Mutex `lock().unwrap()`(gateway 生产 unwrap 100% 属此类:nodes/manager.rs:114,128、registry.rs、pairing.rs、market/mod.rs;cli/tui.rs:211-562 共 25 处、driver.rs:241,367,375)。Mutex 中毒 → 网关后续所有 lock 直接 panic。先跑 clippy `await_holding_lock` 排查跨 await 持锁,再决策换 tokio::sync::Mutex 或消息传递。

---

## 明确不做 / 另行立项

- 长消息拆分(channel 出站):功能缺口,非重复 → 入 gaps/04-breadth/channels.md
- channel 出站重试:需先把 retry/backoff 下沉 legion-core(纯计算),再接线
- host `SessionAccumulator` 与 runtime 消息构造的双份构建:事件溯源固有代价,优先级低
- legion-tools → legion-automation 反向分层(scheduler.rs:11):需引入存储 trait,随 P10/后续
