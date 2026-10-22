# ACP testing and support matrix

ACP is bidirectional. A useful interoperability test must keep one connection
open while notifications and nested requests arrive; a series of disconnected
JSON lines cannot prove session behavior.

## Verified tests

The `adk-acp` suite connects the official SDK `Client` to the ADK-Rust SDK
`Agent` through an in-memory transport and exercises:

```text
initialize
  → session/new
  → session/prompt
  ← session/update
  ← PromptResponse(end_turn)
  → session/close
  → session/list
  → session/resume
  → session/close
  → session/delete
```

Separate tests cover session cancellation, JSON-RPC request cancellation and
recovery, event mapping, reject-first permission menus, opaque option IDs,
fabricated selections, awaited human decisions, exact-call allow-once behavior,
MCP configuration validation, and secret-redacted debug output. Phase 2 adds
`session/load` replay-ordering tests (the replayed `session/update`s match the
stored events' chronological order), multimodal prompt mapping and rejection
tests (image and audio accepted; unadvertised content rejected), server
permission-bridge approve/deny/cancel tests (cancellation maps to deny,
correlated by function-call id), and client-fidelity tests proving the
`ToolCallUpdate` and `UsageUpdate` surfaces appear without regressing the agent
text surface. Phase 3 adds session-mode and configuration-option tests
(`set_mode` / `set_config_option` record advertised values and reject unknown
ones, and the selection persists across `session/load`), a `session/fork`
isolation test (the fork's history equals the source's and the source is left
unchanged), available-commands and session-info activation tests (updates emitted
only when the agent declares commands or records a title), and a
capability-accuracy test asserting the advertised capabilities correspond exactly
to the registered handlers and enabled content mappings — including that modes
and config options are advertised only when a `SessionControls` provider is
present.

The live MCP lifecycle gate starts a real stdio MCP child, completes the
handshake, and discovers its tools through the same `McpToolset` used by ACP
sessions.

## Run the gates

```bash
cargo test -p adk-acp --all-features
cargo test -p adk-agent --test tool_confirmation_tests
cargo test -p adk-tool --features mcp --test mcp_server_lifecycle_integration_tests test_tool_aggregation -- --ignored --exact

cargo test --manifest-path examples/acp_client_host/Cargo.toml
cargo check --manifest-path examples/acp_kiro/Cargo.toml
cargo check --manifest-path examples/acp_server/Cargo.toml
cargo test --manifest-path examples/acp_full_protocol/Cargo.toml
```

The `acp_full_protocol` gate is the runnable Phase 2 safety net: a no-API-key,
`Runner`-backed `AcpServer` driven through the official SDK over an in-process
channel, validating the full server-direction Phase 2 surface (embedded-resource
and image/audio prompts, the permission bridge, `session/load` replay ordering,
and `UsageUpdate` / `ToolCallUpdate` surfacing) without a subprocess or model
credentials.

## Current support

| Area | Status | Notes |
|---|---|---|
| Stable wire protocol v1 | Implemented | Official Rust SDK 1.2; protocol version negotiated separately |
| Local stdio client transport | Implemented | One-shot, streaming, and persistent sessions |
| Client permissions | Implemented | Deny by default, semantic matching, opaque IDs, sync or async policy |
| Client filesystem callbacks | Implemented API | Read and write advertised independently |
| Client terminal callbacks | Implemented API | Complete create/output/wait/kill/release trait |
| Client-supplied MCP | Implemented | stdio required; HTTP/SSE capability-gated |
| ADK-Rust ACP server | Implemented | New, prompt, load, update, cancel, close, list, resume, fork, set_mode, set_config_option, delete |
| Server session load + replay | Implemented | Reactivates a persisted session and replays stored events in chronological order; `load_session` advertised |
| Server session fork | Implemented | Copies history and relevant state into a new session id, leaving the source unchanged; `fork` advertised |
| Server session modes + config options | Implemented | Provider-gated via `SessionControls`; `set_mode` / `set_config_option` validated and persisted across load/resume/fork; advertised only when declared |
| Server available-commands + session-info | Implemented | Emitted on activation when the agent declares commands or records a title; none otherwise |
| Server plan updates | Dormant | `Plan` `SessionUpdate` mapping exists but is inert until an ADK plan primitive surfaces plan entries |
| Server session MCP | Implemented | stdio, per session, bounded startup and cleanup |
| Text and resource-link prompts | Implemented | Mapped through the shared content module |
| Multimodal prompts (image, audio) | Implemented | Mapped to `Part::InlineData`; `image`/`audio` advertised; unadvertised content rejected |
| Embedded-resource prompts | Implemented | Mapped to `Part::EmbeddedResource`; `embedded_context` advertised |
| Server ADK tool approval to ACP | Implemented | `ToolConfirmationRequest` bridged to `session/request_permission`; allow → approve, deny/cancel → deny, correlated by function-call id |
| Client tool-update and usage fidelity | Implemented | `OutputChunk::ToolUpdate` and `OutputChunk::Usage` surface an External_Agent's `ToolCallUpdate`/`UsageUpdate`; agent text unchanged |
| Client rich prompt content | Implemented | `prompt_agent_content_with_policy` transmits non-text ADK content as the matching ACP block |
| Remote ACP HTTP/WebSocket | Not advertised | The stable implementation is local stdio |
| Experimental protocol features | Not advertised | Add only after implementation and interoperability tests |

## Manual editor test

Build `examples/acp_server`, then configure an ACP client to start that binary
with an absolute manifest path and model credentials. Verify:

1. the initialization response reports protocol version 1;
2. a new session accepts the intended absolute project directory;
3. text appears as live updates before the final response;
4. read tool starts and completions appear in the client;
5. cancellation closes the turn without closing the connection;
6. a later prompt succeeds in the same session;
7. close and resume preserve history when the session service is durable.

Do not use `echo | cargo run` for this test. Each pipe starts a different
process and cannot preserve the connection or session.

## Related examples

- [`acp_client_host`](../../../examples/acp_client_host)
- [`acp_kiro`](../../../examples/acp_kiro)
- [`acp_server`](../../../examples/acp_server)
- [`acp_full_protocol`](../../../examples/acp_full_protocol) — no-API-key, `Runner`-backed Phase 2 server-direction reference with an end-to-end validating test
