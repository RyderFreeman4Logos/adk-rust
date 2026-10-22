# ACP full-protocol reference (server direction)

End-to-end, **no-API-key** coverage of the stable ACP v1 server-direction
features that `adk-acp` gained in Phase 2. A real ADK-Rust
[`Runner`](https://docs.rs/adk-runner)-backed `AcpServer` exposes a small,
deterministic agent, and an integration test drives it through the official
`agent-client-protocol` SDK over an in-process channel — no subprocess, no model
credentials.

```text
SDK Client  ──initialize · session/new · session/prompt · session/load──►  AcpServer
            ◄──session/update (message · tool call · tool update · usage)──   │
            ◄──session/request_permission───────────────────────────────     │
                                                                              ▼
                                                              adk-runner Runner
                                                                              ▼
                                                    ScriptedAgent + delete_file tool
```

## What it exercises

| Feature | How |
|---------|-----|
| Embedded-resource prompts | The agent echoes an `EmbeddedResource` block's uri/text, proving it arrived as `Part::EmbeddedResource`. |
| Multimodal prompts | Image and audio blocks are accepted and reach the agent as `Part::InlineData` (echoed with mime type + byte length). |
| Permission bridge | The confirmation-gated `delete_file` tool pauses the turn; the client's `session/request_permission` answer (allow / deny) decides whether it runs. The outer prompt completes either way. |
| `session/load` replay | After a turn, `session/load` replays the stored updates in chronological order before the response returns. |
| Usage & tool updates | A rich turn emits a `UsageUpdate` and an enriched `ToolCallUpdate` (content + file location + kind). |

## Why the agent is deterministic

The [`ScriptedAgent`](src/lib.rs) does not call an LLM, so the protocol behavior
is fully reproducible. It models the same pause/resume tool-confirmation flow an
[`LlmAgent`](https://docs.rs/adk-agent) produces with `require_tool_confirmation`
— emitting `event.actions.tool_confirmation`, which the server's permission
bridge maps to `session/request_permission`. The `Runner` and `AcpServer` are
the real ones.

## Run the reference server

```bash
cd examples/acp_full_protocol
cargo run
```

The server listens on stdin for ACP messages and writes protocol JSON to
stdout. Diagnostics go to stderr. Connect an ACP client or SDK over stdio.

## Run the validating test

```bash
cargo test --manifest-path examples/acp_full_protocol/Cargo.toml
```

The test in [`tests/protocol.rs`](tests/protocol.rs) builds the server from the
public `AcpSessionHandler`, pairs it with the SDK `Client` via
`Channel::duplex()`, and asserts every feature above.

## No configuration required

There are no environment variables to set — see [`.env.example`](.env.example).
The agent is deterministic and never contacts a model provider.
