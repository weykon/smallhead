# smallhead-tools

External information providers for `smallhead`: web search, wiki search,
code search, news search, and Agent-Reach-backed channels.

## What It Provides

- `web`: CJK-aware general web search fallback chain.
- `wiki`: Chinese or English Wikipedia, selected from query text.
- `code`: GitHub repository search.
- `news`: Hacker News search.
- `agent_reach`: descriptors for 16 Agent-Reach channels, with implemented
  zero-config bridges for search, webpage, RSS, YouTube, and Bilibili where the
  upstream command is available.

The public tool entry point is `web_search`. `smallhead-core` owns the
`ToolKind::WebSearch` marker; this crate registers the real handler.
Agent-Reach also has a separate public tool entry point, `agent_reach`, for
one-shot CLI-backed channel calls.

## Host Registration

Call registration once during host startup, before exposing `web_search` to the
agent:

```rust
smallhead_tools::web_search_tool::register();
smallhead_tools::agent_reach_tool::register();
```

The host can then register:

```rust
smallhead_core::ToolKind::WebSearch(smallhead_core::WebSearchTool)
smallhead_core::ToolKind::AgentReach(smallhead_core::AgentReachTool)
```

If registration is skipped, the tool returns a clear execution error instead of
panicking.

## Tool Input

```json
{
  "query": "rust async runtime",
  "kind": "web",
  "limit": 8
}
```

`kind` must be one of:

- `web`
- `wiki`
- `code`
- `news`

`limit` is optional and is clamped to `1..=30`.

## Network And Proxy

HTTP providers use the process environment for proxy settings:

- `HTTPS_PROXY`
- `HTTP_PROXY`
- `ALL_PROXY`
- lowercase variants supported by the underlying client or upstream CLI

Brave Search is optional and reads `BRAVE_API_KEY` at runtime. Missing optional
credentials should degrade to a provider error or fallback, not a build failure.

## Agent-Reach

Agent-Reach channels shell out to upstream commands such as `mcporter`, `curl`,
or `yt-dlp`. Cookies and tokens live outside this repository, normally under:

```text
~/.agent-reach/
```

Install or refresh the upstream environment with:

```sh
agent-reach install --env=auto --system
```

The bridge also checks common virtualenv paths such as:

```text
~/.agent-reach-venv/bin
```

Stubbed channels intentionally return an install/configuration hint until their
parser and execution path are implemented.

The `agent_reach` tool supports:

- `{"action":"list"}` to inspect channels and upstream CLI availability.
- `{"action":"search","channel":"youtube","query":"rust async","limit":3}`.
- `{"action":"fetch","channel":"webpage","target":"https://example.com"}`.

It is a one-shot CLI tool, not a `ServiceManager` service preset.

## Verification

Offline parser/router tests:

```sh
cargo test -p smallhead-tools
```

Critical smoke commands to run before a PR:

```sh
cargo test -p smallhead-tools web_search_tool
cargo test -p smallhead-tools github
cargo test -p smallhead-tools wiki
cargo test -p smallhead-tools agent_reach
```

For live YouTube coverage, verify the upstream command is available first:

```sh
yt-dlp --dump-json "ytsearch1:rust async runtime"
```

If this command is unavailable, install Agent-Reach or `yt-dlp` before treating
YouTube live smoke as complete.
