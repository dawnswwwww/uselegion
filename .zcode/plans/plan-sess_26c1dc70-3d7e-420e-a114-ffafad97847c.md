# 方案:Skill 感知的 Slash Command 体系 + .legion/skills 项目级目录

## 问题

1. TUI 里 skill 不可见、不可交互:`/help` 只列 4 个内置命令,`/<skill-name>` 报 "unknown command",没有 `/skills` 命令
2. 项目级 skill 目录只扫 `.agent/skills`,不扫 `.legion/skills`

## 设计

用户确认:
- `/<skill-name> [args]` → 注入 body 为 system 消息 + 发送用户消息给 agent
- TUI 启动时加载 skill 缓存到 AppState
- 同时支持 `<workspace>/.agent/skills` 和 `<workspace>/.legion/skills` 两个项目级目录

## 改动清单

### 1. 项目级 skill 目录加 `.legion/skills`

`crates/legion-runtime/src/agent_loop.rs:296`
```rust
// 之前:只扫 .agent/skills
let workspace_agent_skills = workspace.join(".agent").join("skills");
if fs.exists(&workspace_agent_skills).await { skill_dirs.push(...); }

// 之后:扫 .agent/skills + .legion/skills
for dir in [".agent/skills", ".legion/skills"] {
    let p = workspace.join(dir);
    if fs.exists(&p).await { skill_dirs.push(p); }
}
```

### 2. SlashCommand 结构体改为动态 + 加 CommandKind

`crates/legion-cli/src/slash_commands.rs`

- `SlashCommand` 字段从 `&'static` 改为 `String`(skill 名是运行时的)
- 加 `CommandKind` enum:`Local`(内置,本地执行) / `Prompt { body }`(skill,注入 body + 发消息)
- 内置命令保留 `run` 回调,skill 命令用 `Prompt { body }`
- `try_execute` → `dispatch`,返回 `CommandResult`:`Handled` / `SendToAgent { message }` / `NotACommand`
- 新增 `/skills` 命令:列出已加载 skill
- `/help` 末尾追加 skill 列表

### 3. AppState 加 loaded_skills + TUI 启动加载

`crates/legion-cli/src/tui.rs`

- `AppState` 加 `loaded_skills: Vec<Skill>`
- `run_tui` 启动时扫描 skill 目录(config dirs + `.agent/skills` + `.legion/skills`),过滤 `user_invocable: true`
- `slash_suggestions()` 合并内置命令 + skill 命令
- Enter 分支处理 `SendToAgent`:注入 skill body 为 system 消息 + 发送 args 给 driver

### 4. Skill 命令触发流程

用户输入 `/deploy staging`:
1. Echo `/deploy staging` 为 User 消息(可见性)
2. 注入 skill body 为 System 消息:`## Skill: deploy\n\n{body}`
3. 返回 `SendToAgent { message: "staging" }`(args 部分;空则发 "follow the skill instructions above")

## 不做的事

- 不做 `command-dispatch: tool`(工具派发模式)
- 不做 skill 热重载(TUI 启动时加载一次)
- 不改 runtime 侧的自动注入逻辑(意图匹配 + 路径匹配不变)
- 不加 `disable-model-invocation` 字段

## 测试

- `slash_commands.rs`:`dispatch` 三种返回值;skill 匹配 + args 解析;`/skills` 输出;`/help` 含 skill
- `tui.rs`:`slash_suggestions` 合并;Enter 触发 Prompt skill;空 skill 列表
- `agent_loop.rs`:`.legion/skills` 扫描(已有测试基础上加一个)

## 文档

- `AGENTS.md`:CLI quick reference 补 `/skills` `/<skill-name>`;skill 目录说明补 `.legion/skills`