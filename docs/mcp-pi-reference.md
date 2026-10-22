# Pi MCP reference for smallhead

Research date: 2026-10-16 (Asia/Shanghai). Research only; no MCP implementation or package installation was performed.

## Result and source provenance

**Current Pi has native MCP support.** It shipped in **0.99.0 on 2026-09-29**. The old "No MCP" statement describes the January checkout, not current upstream. Native support consists of a standalone client package and a built-in coding-agent extension; it is not baked into the provider-independent agent loop.

| Source | Verified revision / observation |
| --- | --- |
| Local `/Users/weykon/Desktop/p/pi-mono` HEAD and cached `origin/main` | `2cee7e17de7cfbefb50d20e7c46bbf6ff6130c2b`, commit date `2026-01-30T03:27:09+01:00` |
| Current upstream main, reported by official GitHub API | `4954d760504c11df9b2cbdc040346107b6a8153d`, commit date `2026-10-15T19:21:57Z` |
| Repository identity | Configured origin is `https://github.com/badlogic/pi-mono.git`; GitHub's API redirects this repository to `earendil-works/pi` |
| External adapter main, reported by its GitHub API | `1313344d3310ee04aad0bdce1620befafb3115ed`, commit date `2026-10-16T05:13:09Z` |
| npm registry `pi-mcp-adapter/latest` | `5.3.1`; repository `nicobailon/pi-mcp-adapter` |
| smallhead HEAD during research | `d68ec0617dd5e086ef1975425f35ac9bf7467e94`; shared worktree also contains uncommitted changes |

Fetch limitation: `git fetch origin` failed with the configured low-speed timeout. A canonical-URL retry failed with an HTTP/2 framing error; an HTTP/1.1 retry failed to connect to GitHub port 443 after 75 seconds. No fetch succeeded, so **local origin/main was not refreshed**. Current findings below use official GitHub API results and raw source URLs pinned to the October hash, not an assertion that the local checkout is current. All commands have finished. No checkout, merge, reset, remote-config change, or edit in pi-mono was made. Its status remained exactly `?? .serena/`.

Primary upstream sources, pinned to the inspected current revision:

- [Coding-agent MCP documentation](https://github.com/earendil-works/pi/blob/4954d760504c11df9b2cbdc040346107b6a8153d/packages/coding-agent/docs/mcp.md)
- [Coding-agent changelog, including 0.99.0](https://github.com/earendil-works/pi/blob/4954d760504c11df9b2cbdc040346107b6a8153d/packages/coding-agent/CHANGELOG.md)
- [Standalone MCP package README](https://github.com/earendil-works/pi/blob/4954d760504c11df9b2cbdc040346107b6a8153d/packages/mcp/README.md)
- [MCP package changelog](https://github.com/earendil-works/pi/blob/4954d760504c11df9b2cbdc040346107b6a8153d/packages/mcp/CHANGELOG.md)
- [Built-in extension entry point](https://github.com/earendil-works/pi/blob/4954d760504c11df9b2cbdc040346107b6a8153d/packages/coding-agent/src/extensions/mcp/index.ts)
- [Connection/runtime implementation](https://github.com/earendil-works/pi/blob/4954d760504c11df9b2cbdc040346107b6a8153d/packages/coding-agent/src/extensions/mcp/runtime.ts)
- [HTTP cancellation implementation](https://github.com/earendil-works/pi/blob/4954d760504c11df9b2cbdc040346107b6a8153d/packages/mcp/src/transports/streamable-http.ts)
- [SDK opt-in documentation](https://github.com/earendil-works/pi/blob/4954d760504c11df9b2cbdc040346107b6a8153d/packages/coding-agent/docs/sdk.md#codemode-mcp)

## Architecture and package distinctions

1. `packages/mcp` publishes `@earendil-works/pi-mcp`: a standalone, transport-neutral MCP client with stdio, Streamable HTTP, and an in-memory test transport. The README states it does not depend on other Pi packages or the official MCP SDK. Its OAuth implementation credits an adaptation of the MIT-licensed TypeScript SDK v1.29.0. Request correlation, initialization, timeout renewal, cancellation, roots, and protocol helpers live here.
2. `packages/coding-agent/src/extensions/mcp/` owns application integration: config, connection lifecycle, OAuth persistence, tool/resource conversion, logs, and UI. `index.ts` loads connection code through `runtime.lazy.ts` when needed. This lazy module load does **not** mean enabled servers start only on first use.
3. The CLI loads MCP, codemode, and tool-search as built-in extensions. SDK sessions do not: the caller adds `createMcpExtension()`, plus codemode/tool-search factories as appropriate, to `DefaultResourceLoader`, then binds extensions so `session_start` fires.
4. `packages/codemode` supplies script execution; codemode is not the MCP wire protocol. Pi 1.2-era changes explicitly decouple the two through generic tool reachability declarations.
5. `pi-mcp-adapter` is third-party integration, not an upstream package. Current official docs explicitly say an extension registering `/mcp` replaces the built-in MCP integration for sessions. Shell-level `pi mcp` commands still use the built-in implementation. Do not infer native support merely from adapter installation or package-management fixtures.

Current built-in behavior, from official MCP docs and extension source:

- Configuration: `~/.pi/agent/mcp.json` and trusted project `.pi/mcp.json`; extension registrations through `pi.registerMcpServer()`. File configuration takes precedence over the same registered name.
- Transports: stdio and Streamable HTTP; legacy SSE is rejected.
- Lifecycle: enabled servers connect in the background on session start. The first prompt waits up to 10 seconds for direct tools; search, resources, and scripts wait for the servers they need. `/mcp` exposes status, reconnect, authentication, enablement, and exposure controls.
- Names: `mcp__<server>__<tool>` with normalization and collision handling.
- Exposure: default `codemode`, plus `deferred`, `direct`, and `hidden`; per-tool overrides. Deferred tools enter model declarations only after search. Codemode discovers tool details on demand instead of placing all schemas in the prompt.
- Tool execution: every actual call passes through Pi's tool pipeline, including tool-call/result hooks and permission extensions. Nested calls carry `parentToolCallId`.
- Results: preserve `content`, `structuredContent`, and `isError`. Text above 20 KB is truncated for direct model delivery with a full-output artifact; scripts receive the complete result. Structured content can be any JSON value, not only objects.
- Resources: separate list, template-list, and read tools. MCP App UI resources are omitted because Pi does not render them.
- Cancellation/shutdown: request cancellation is distinct from transport lifetime. Stdio shutdown closes stdin, then terminates/kills the process group, including launch wrappers. Sign-in and pending connection attempts also need shutdown handling.
- Retry nuance: docs say tool calls are not retried after transient failures because execution may have happened. `McpServerConnection.withClient()` has a specific one-time expired-session recovery path, with a source comment assuming the server did not execute a request rejected for an unknown session. Do not generalize that exception into retries after timeout, cancellation, or a lost response.

## Relevant upstream commits

Hashes and dates below were verified through official GitHub commit-history API responses. Dates are UTC.

| Date | Commit | Change and relevance |
| --- | --- | --- |
| 2026-09-29 | `8562bcf66a8eeefdf75ddd231ca9d7aa7d64f86e` | `feat(coding-agent): codemode and MCP`; native integration introduction, confirmed by 0.99.0 changelog |
| 2026-10-05 | `8c911797c3b6b5d3b3921ce4d2438e98e1a948a6` | Close MCP connections still connecting during shutdown; avoid orphan transports |
| 2026-10-07 | `f10993bc7f28145df1375f3ff39c7f5c4cfc05f0` | Cancel OAuth sign-ins and stop refreshing on close; shutdown must not trigger new authentication work |
| 2026-10-12 | `24382bedd03c61c0994628907a017b4491f8fa76` | Decouple MCP from codemode; generic `reachesTools` and tool namespace interfaces |
| 2026-10-12 | `f4cca2dc22ee4ce05ad75e185a025f6c664829c8` | Stop listing resources during connection; resource discovery should not block tool availability |
| 2026-10-12 | `67996ab75c2818847423549807eda888db218c1c` | Accept any JSON `structuredContent`, including arrays and null |
| 2026-10-12 | `da0eadf0433d29516287c7e3d7b3db325a446281` | Do not retry canceled HTTP requests after authorization refresh; prevents execution after caller cancellation |

The last fix is particularly important for smallhead's live steering/recovery. In `streamable-http.ts`, the request path checks the request signal after awaiting auth headers, before sending. Checking cancellation only before an asynchronous token refresh is insufficient.

## External adapter evidence and limits

The adapter's own [README](https://github.com/nicobailon/pi-mcp-adapter/blob/1313344d3310ee04aad0bdce1620befafb3115ed/README.md) describes one generic `mcp` proxy tool, on-demand discovery, cached tool metadata, lazy server startup, and a default 10-minute idle shutdown. It also offers optional direct tools and scripts. These are useful alternative design choices, not the built-in Pi default.

Its current README acknowledges native Pi MCP since 0.99 and compares the implementations. Performance/token figures in that comparison are **maintainer claims, not benchmarks reproduced in this research**. Current npm metadata declares dependencies on `@modelcontextprotocol/client` and `@modelcontextprotocol/core` 2.3.1, unlike upstream Pi's standalone client implementation. No adapter code was installed or executed.

Evidence endpoints: [npm latest metadata](https://registry.npmjs.org/pi-mcp-adapter/latest), [upstream main commit](https://api.github.com/repos/earendil-works/pi/commits/main), [adapter main commit](https://api.github.com/repos/nicobailon/pi-mcp-adapter/commits/HEAD). These endpoints move; use the recorded hashes above to reproduce source inspection.

## Recommendations for the selected smallhead design

Main's selected implementation is **rmcp 3.5.1, a separate stdio/HTTP manager, a generic late-discovery `mcp` tool through core `ExternalTool`, and an MCP section in TUI Services**. This research supports that boundary. Dependency availability and SDK compatibility were not verified here; upstream Pi's TypeScript client is architectural evidence, not a Rust dependency recommendation. Implementation details belong in main's `docs/mcp-integration.md`.

- Keep discovery and connection state outside `AgentLoop`. The generic tool can list/search/describe/call; only requested schemas enter the conversation. Prefer this initial scope over implementing codemode solely to match Pi.
- Give every proxied call a concrete server/tool identity in diagnostics, permission checks, and execution records. A generic proxy name alone must not bypass tool-level decisions. Treat server annotations as claims, not authorization.
- Preserve the current sequential tool-boundary steering and no-replay recovery rules. Pass a request cancellation signal through connect/auth/call; recheck after awaits. If a remote call may have executed, return an explicit unknown-side-effect result instead of reconnecting and replaying it.
- Decide lifecycle explicitly. Pi's background-connect-all policy and the adapter's lazy policy are different. Lazy startup with visible connecting/ready/failed states fits a small Services surface; do not claim Pi itself uses that default.
- Separate server restart/reconnect from retrying a tool invocation. Track which child processes belong to the manager; disconnect HTTP sessions without stopping unrelated remote services.
- Preserve structured output and artifacts even when the prompt receives a compact preview. Discovery should handle pagination, tool-list changes, normalized-name collisions, and schema versions without replacing the whole provider tool list mid-request.
- Initially implement tools over stdio/Streamable HTTP. Make resources, prompt templates, sampling, elicitation, MCP Apps, and scripting explicit later capabilities, rather than silently advertising full MCP coverage.
- Add focused tests for cancellation during auth/connect, no replay after a lost result, shutdown while connecting, tool-list replacement, same-batch steering, and structured output arrays/null. These are recommendations; no MCP tests were run in this research task.

## World and memory discussion

MCP is an interface for accessing capabilities and data. It does not define smallhead's authoritative world state or provide durable agent memory merely because a server offers a search tool.

The following table proposes ownership boundaries; it is not a statement that each subsystem is implemented or wired into the current runtime. In particular, an authoritative ECS world is not established by the existing interfaces and checkpoint storage.

| Concern | Proposed owner / current evidence | Relationship to MCP |
| --- | --- | --- |
| Conversation and recovery | core committed history plus session/turn checkpoints | Record calls/results, pending steering, and uncertainty; never replay remote mutations automatically |
| World state | Proposed: `smallhead-ecs`. World-operation execution still contains MVP stubs; checkpoint APIs do not establish a working authoritative runtime world. | External calls would produce observations; a future implemented world layer would validate and commit explicit updates separately. |
| Memory | `smallhead-memory` ingestion/retrieval with provenance and scope | A memory backend may be reached through MCP, but transport does not determine retention, trust, or retrieval policy |
| External capabilities | MCP manager and tool adapter | Own connections, discovery, invocation, authentication, and cancellation |

Local evidence: `crates/smallhead-ecs/src/lib.rs`, `world_op.rs`, and `checkpoint.rs` define a proposed world-operation boundary and atomic/version-checked checkpoint API. However, `world_op.rs` still returns an `accepted: true` result with `note: "MVP stub"` in its execution path. This corroborates Bacon's finding: interface definitions and checkpoint storage must not be presented as implemented authoritative world behavior. `crates/smallhead-memory/src/ingest.rs` contains `IngestPipeline::ingest` and `search`; `store.rs` contains real connection/table APIs. Its `lib.rs` still calls the crate a scaffold and says ingestion/search are missing, so that header must not be used to conclude the implementation is absent. These memory APIs likewise do not prove end-to-end runtime integration, which was not audited here.

For Pi historical context only, the January revision's `packages/mom/README.md` describes global/channel `MEMORY.md`, append-only `log.jsonl`, and compactable `context.jsonl`. That illustrates a useful separation of curated memory, evidence history, and working context, but is **not a claim about the current October package layout or a native ECS world model**. The January coding-agent extension docs also distinguish `appendEntry` persistence from messages visible to the model.

Recommended discussion questions before expanding memory/world features:

1. Which facts are authoritative world state, which are observed external state, and which are user preferences or fallible summaries?
2. What identifies an observation: source server/tool, arguments, execution ID, timestamp, artifact reference, and world version?
3. Which memories may cross session/project boundaries, and how are corrections, deletion, and stale evidence handled?
4. How does a remote mutation's unknown outcome remain visible until inspection resolves it, without turning a model summary into proof of success?

No additional world/memory redesign is required to add the selected MCP adapter. Keep those ownership decisions separate and use provenance-bearing tool results as the bridge.
