# AGENTS.md — Legion

This file contains the essential conventions AI agents must follow when working on the **Legion** repository. For detailed architecture, CLI usage, and design docs, see `docs/` and `README.md`.

---

## Project essentials

- **Language / toolchain:** Rust (MSRV 1.86), Cargo workspace, Edition 2024, resolver 3.
- **Do not touch `claude-code-analysis/`** — it is an independent repository and is **not** part of this Cargo workspace. Exclude it from all `cargo` commands.
- Prefer workspace dependencies from the root `Cargo.toml`; do not duplicate versions.

## Build, lint, and test

Run these before finishing any change:

```bash
cargo build --workspace --all-targets
cargo clippy --workspace --all-targets
cargo test --workspace --all-targets
cargo fmt -- --check
```

For faster local iteration you may target a single crate, but the full-workspace commands above are the final gate:

```bash
cargo check -p <crate>
cargo test -p <crate>
cargo clippy -p <crate>
```

E2E tests in `crates/legion-gateway/tests/e2e_minimax*.rs` and `crates/legion-provider/tests/integration_test.rs` require a real `MINIMAX_API_KEY`; they are ignored otherwise.

## Code style

- Rust edition 2024.
- Use `thiserror` for error enums and `tracing` for logging.
- Serde config and API fields use `camelCase`.
- Async dynamic dispatch uses `async-trait`.
- Avoid `unwrap`/`expect` in production code; prefer `?` and typed errors.
- Prefer `Arc<dyn Trait>` for shared, plugin-provided components.

## Code quality

- Keep files focused. Avoid pushing a file from under 1 000 lines to over 1 000 lines without a strong reason; prefer decomposition first.
- Avoid ad-hoc "spaghetti" growth: do not bolt random conditionals into unrelated flows. Push special cases into dedicated abstractions, helpers, or policy objects.
- Prefer "code judo": restructure so that branches, wrappers, or whole layers disappear while preserving behavior.
- Keep logic in the canonical layer and reuse existing helpers rather than introducing bespoke one-offs.
- Prefer direct, boring, maintainable code over hacky or magical indirection.
- Question unnecessary optionality or cast-heavy boundaries when a clearer typed contract could exist.
- Prefer subtraction over layering — collapse parallel models into a single source of truth and delete code rather than adding indirection; do not over-engineer.

## Where to make changes

- **Config schema:** `crates/legion-core/src/config.rs`
- **Routing:** `crates/legion-gateway/src/routing.rs`
- **Tools:** register in `crates/legion-tools/src/registry.rs`, add tests in `crates/legion-tools/src/tools.rs`
- **MCP transports:** extend `McpTransport` in `crates/legion-core/src/config.rs`, add client logic in `crates/legion-mcp/src/client.rs`
- **Providers:** `crates/legion-provider/src/router.rs`
- **Session goals:** model/store/turn-end gate in `crates/legion-runtime/src/goal.rs` / `goal_gate.rs`, model-facing tools in `crates/legion-host/src/goal_tools.rs`, `/goal` command in `crates/legion-cli/src/slash_commands.rs`
- **CLI / Gateway distribution:** `crates/legion-cli/src/gateway_manager.rs`, `crates/legion-cli/src/lib.rs`, `crates/legion-cli/src/main.rs`, and `crates/legion-protocol/src/compatibility.rs` / `manifest.rs`
- Keep `cargo tree -p legion-cli` free of `legion-gateway`.

## Testing

- Unit tests live in the same file under `#[cfg(test)]`.
- Integration tests live in `crates/<crate>/tests/`.
- Common test crates: `rstest`, `tempfile`, `temp-env`, `wiremock`, `tokio-test`.
- Tests must exercise the shipped code on the real path. Avoid "test theater": do not hard-code expected values, re-implement the code under test, or start past the thing being tested.
- Run targeted tests after each change; run the full relevant suite before finishing.
- Verify outcomes, not just code. If the task was to deploy, submit, or configure something, confirm the external state actually changed.

## Security

- Gateway defaults to loopback. Reject `auth.mode: none` on non-loopback binds.
- API keys belong in auth profiles or environment variables, never in committed config files.
- Tool approval policies (`off` / `prompt` / `required`) must be respected; unattended sessions deny `prompt`/`required`.
- Channel access control in `legion-channel/src/access.rs` is enforced for every inbound message.
- `exec` sandboxing: restricted profiles must fail explicitly rather than silently fall back to unsandboxed execution, except for the documented `cube` availability fallback.

## Working principles

- Make minimal, scoped changes. Avoid opportunistic reformatting, renames, or unrelated refactors.
- New code should match the surrounding file's naming, comment density, and structural idioms.
- Do not add dependencies unless the project already uses them or the capability is genuinely missing.
- Update this file only when conventions themselves change, not for every feature.
- Treat `docs/design/agent-harness-prd.md` as the functional spec, but verify implementation status in the source.
- Capability gaps and roadmap live in `docs/design/gaps/00-overview.md`; update them when closing a gap.
- Tool-call first, narration second: if you describe an action, pair it with the corresponding tool call in the same turn.
- Do not stop with easy work left undone. Keep going when the next step is clear and unblocked.
- Use a TODO list for multi-step work when it helps, but do not let bookkeeping replace real progress.
- Verify as you go: inspect output, validate state, and capture evidence rather than assuming success.
