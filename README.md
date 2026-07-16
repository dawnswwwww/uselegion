# Legion

> Self-hosted, multi-channel AI agent gateway written in Rust.

Legion is a single long-running process that bridges chat channels (WebChat,
Telegram, …) to an agent runtime capable of calling tools, managing memory,
streaming replies back to the originating channel, and running unattended jobs
(cron, heartbeat, tasks).

The architecture is inspired by the OpenClaw reference design and targets
Claude Code–level capability depth across tools, memory, compaction, and
sandboxing.

---

## Features

- **Multi-channel gateway** — pluggable channel providers (`ChannelProvider`
  trait); WebChat and Telegram ship in-tree; new channels are a plugin drop-in.
- **Agent runtime** — long-context loop with token-aware compaction, prompt-too-long
  retry, and Anthropic prompt caching.
- **Tool pipeline** — built-in tools (`read`, `write`, `edit`, `apply_patch`,
  `exec`, `web_search`, `web_fetch`, memory tools) with per-tool policy and an
  approval gate that asks the originating user on `prompt` / `required`.
- **Sandbox isolation** — `exec` can run in three profiles: `off` (direct),
  `restricted` (OS-native: Linux `bwrap`, macOS `sandbox-exec`), and `cube`
  (CubeSandbox MicroVM).
- **Skills** — `legion-skills` crate loads `SKILL.md` files with YAML
  frontmatter, injects summaries into the system prompt, and on-demand
  expands bodies when the user intent matches the skill description or the
  agent reads files matching the skill's `paths` globs.
- **Memory** — SQLite + `sqlite-vec` + FTS5 backend at
  `~/.legion/agents/<agentId>/memory/`, exposed via `memory_search`,
  `memory_get`, `memory_index` tools.
- **Automation** — cron scheduler, heartbeat runner, hook runner, and a
  JSONL-backed task ledger.
- **Plugin system** — `Plugin` trait with `capabilities()` / `init(ctx)`,
  manifest-driven user plugins with dependency topological sort.
- **Observability** — `tracing` + `/metrics` endpoint; structured audit for
  tool approval decisions.

---

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│  Channels                                                   │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐                   │
│  │ WebChat  │  │ Telegram │  │   ...    │  (Plugin SDK)     │
│  └────┬─────┘  └────┬─────┘  └────┬─────┘                   │
└───────┼─────────────┼─────────────┼─────────────────────────┘
        │  InboundMessage (channel, account, peer, text)       │
        ▼                                                     
┌─────────────────────────────────────────────────────────────┐
│  Gateway   (legion-gateway)                                 │
│  · HTTP / WebSocket                                         │
│  · Session store + routing + pairing                        │
│  · Approval queue registry                                  │
└───────┬─────────────────────────────────────────────────────┘
        │  RunRequest (agent, scope, interactive, approval_gate)
        ▼
┌─────────────────────────────────────────────────────────────┐
│  Agent Runtime  (legion-runtime)                            │
│  · ContextEngine (pluggable; default = LegacyContextEngine) │
│  · Harness registry  (built-in + ACP harnesses)             │
│  · Tool pipeline  →  Policy  →  Approval gate               │
│  · Compactor  (buffer-aware, circuit-breaker, attachments)  │
└───────┬───────────────────┬──────────────────┬──────────────┘
        │                   │                  │
        ▼                   ▼                  ▼
┌───────────────┐  ┌────────────────┐  ┌──────────────────────┐
│ Providers     │  │ Tools          │  │ Skills / Memory      │
│ (legion-      │  │ (legion-tools) │  │ (legion-skills,      │
│  provider)    │  │ + Sandbox      │  │  legion-memory)      │
│ OpenAI /      │  │  backends      │  │ SQLite + vec + FTS5  │
│ Anthropic     │  │                │  │                      │
└───────────────┘  └────────────────┘  └──────────────────────┘
```

The runtime is **channel-agnostic** — the same loop serves interactive
sessions, cron jobs, and heartbeat ticks. Only the `interactive` flag and
the `ApprovalNotifier` differ.

---

## Quick start

### Prerequisites

- Rust **1.86+** (edition 2024)
- On Linux: `bubblewrap` (`bwrap`) for the `restricted` sandbox profile
- On macOS: nothing extra (uses system `sandbox-exec`)

### Build

```bash
cargo build --workspace --all-targets
```

### First-time setup

```bash
cargo run -p legion-cli -- setup
```

The wizard walks you through picking a provider (MiniMax, OpenAI, Anthropic,
Gemini, Ollama, OpenRouter, Bedrock, or a custom OpenAI-compatible endpoint),
entering credentials (masked input; a detected `${PROVIDER_API_KEY}` env var
can be stored as a reference instead of a raw key), choosing a default model,
and optionally testing the connection live before anything is written. After
the gateway bind host/port it optionally onboards chat channels (Telegram,
Slack, Discord, Lark, Matrix — WebChat works out of the box) with their
credentials and a DM allowlist (DMs default to an allowlist policy, so an
empty list means the bot ignores every DM until you add one). It then writes
`~/.legion/legion.json` (validating it first), merges the auth profile into
`~/.legion/agents/main/agent/auth-profiles.json` (mode 0600), and seeds
`~/.legion/workspace/AGENTS.md`. Finally it offers to install the gateway as a
background service (launchd agent on macOS, systemd user unit on Linux, a
logon scheduled task on Windows; `--install-daemon` skips the prompt). Re-running never overwrites silently: it
offers to keep the existing config, **add a provider** (merged into
`models.providers`/`models.aliases` — `agents.defaults.model` is left
unchanged; also available as `--add-provider`), or reconfigure with a
`legion.json.bak` backup (non-interactive runs need `--force`). For scripts:

```bash
cargo run -p legion-cli -- setup --non-interactive \
  --provider openai --api-key "$OPENAI_API_KEY"

# add a second provider to an existing config
cargo run -p legion-cli -- setup --non-interactive --add-provider \
  --provider anthropic --api-key "$ANTHROPIC_API_KEY"
```

### Run the Gateway

The gateway is optional for local use — the TUI and `legion agent` run
embedded (in-process) when it is not running. You only need it for chat
channels, cron/heartbeat automation, and remote WebSocket access:

```bash
# foreground
cargo run -p legion-cli -- gateway start --foreground

# background
cargo run -p legion-cli -- gateway start
```

Default bind address: `127.0.0.1:18789`. Open the dashboard at
<http://127.0.0.1:18789/dashboard>.

### One-off agent turn

```bash
# uses the gateway when reachable, falls back to embedded mode otherwise
cargo run -p legion-cli -- agent "hello"

# force a mode
cargo run -p legion-cli -- agent --local "hello"     # never touches the gateway
cargo run -p legion-cli -- agent --gateway "hello"   # requires the gateway
```

---

## CLI

| Command | What it does |
|---|---|
| `legion [--local\|--gateway]` | Launch the interactive TUI (gateway or embedded mode) |
| `legion setup` | First-time setup wizard |
| `legion gateway start [--foreground] / stop / status / logs` | Gateway lifecycle (channels, cron, remote WS) |
| `legion agent [--local\|--gateway] "<prompt>"` | Send a single agent turn (auto-detects gateway) |
| `legion config get <key>` / `set <key> <value>` / `validate` | Config inspection |
| `legion channels list / status` | Channel providers |
| `legion memory search <query>` | Memory search from CLI |
| `legion skills list / reload` | Workspace skills |
| `legion cron list / add / remove / run` | Cron scheduler |
| `legion tasks list / show / create / run` | Task ledger |
| `legion nodes list / status / invoke` | Node manager |
| `legion market list / install / uninstall` | Plugin market |
| `legion doctor` | Health checks (sandbox availability, …) |

---

## Workspace layout

```
legion/
├── Cargo.toml                # workspace root
├── crates/
│   ├── legion-core           # Config schema, env-var resolution, errors
│   ├── legion-plugin-sdk     # Plugin trait + registry + channel abstractions
│   ├── legion-gateway        # Gateway process, HTTP/WS, routing, pairing
│   ├── legion-provider       # LLM provider abstraction (OpenAI / Anthropic)
│   ├── legion-runtime        # Agent loop, tools, compaction, harness registry
│   ├── legion-channel        # WebChat + Telegram providers
│   ├── legion-memory         # SQLite + sqlite-vec + FTS5 backend
│   ├── legion-tools          # Core tool registry + sandbox backends
│   ├── legion-automation     # Cron, heartbeat, hooks, task ledger
│   ├── legion-acp            # Agent Connect Protocol harness + mock
│   ├── legion-skills         # SKILL.md parsing + registry + prompt injection
│   ├── legion-cli            # `legion` binary + TUI
│   └── legion-web            # Static dashboard served by the Gateway
└── docs/
    ├── DEVLOG.md             # Development log + Gap progress tracker
    └── design/
        ├── agent-harness-prd.md   # Functional PRD (Chinese)
        └── gaps/                  # Capability gap analysis vs PRD/Claude Code
            └── 00-overview.md     # Start here
```

---

## Documentation

| Doc | What it's for |
|---|---|
| [`AGENTS.md`](AGENTS.md) | Build / run / test commands, conventions, security notes. Source of truth for AI coding agents. |
| [`docs/DEVLOG.md`](docs/DEVLOG.md) | Per-session development log + live Gap progress tracker. |
| [`docs/design/agent-harness-prd.md`](docs/design/agent-harness-prd.md) | Functional PRD (Chinese). |
| [`docs/design/gaps/00-overview.md`](docs/design/gaps/00-overview.md) | Gap analysis vs Claude Code + PRD, prioritized roadmap. Start here if you want to contribute to a known gap. |
| [`docs/openclaw_raw/`](docs/openclaw_raw/) | OpenClaw reference design (English). |

---

## Development status

The project tracks **14 capability gaps** against Claude Code + the PRD in
`docs/design/gaps/00-overview.md`. Snapshot:

| Status | Count | Gaps |
|---|---|---|
| ✅ Completed | 5 | `approval-loop`, `sandbox-isolation`, `plugin-facade`, `compaction`, `skills` (Phase A+B) |
| 🚧 In progress | 1 | `skills` (Phase C) |
| ⬜ Not started | 9 | `mcp`, `memory-layers`, `multi-agent`, `prompt-management`, `session-resume`, `channels`, `providers`, `tools-p1p2`, `automation-advanced` |

For details on any gap (current state, design, acceptance criteria) see the
matching `docs/design/gaps/<category>/<gap>.md` file.

### Verify a change

```bash
cargo build --workspace --all-targets
cargo clippy --workspace --all-targets
cargo test --workspace --all-targets
cargo fmt -- --check
```

---

## Security

- Gateway defaults to loopback (`127.0.0.1`) only.
- Auth modes: `token`, `password`, `trusted-proxy`, `none` (loopback only —
  config validator rejects `none` on non-loopback bind).
- Device pairing tracked in `PairingStore`; loopback connections are
  identified but the WebSocket handshake auto-approves them today — verify
  before exposing the Gateway to a network.
- Tool approval: each tool exposes a `Policy` (`off` / `prompt` / `required`).
  `prompt` and `required` ask the originating user; unattended sessions
  (cron, heartbeat) fail closed.
- Sandbox profiles for `exec`: `off`, `restricted`, `cube`. `restricted`
  blocks network by default, denies writes to credential / config paths,
  and scrubs bare git-repo markers. If a `restricted` profile is configured
  but the platform can't provide it, `exec` fails explicitly rather than
  silently falling back to `off`.
- Secrets belong in auth profiles or env vars, never in committed config.

---

## License

[MIT](LICENSE)