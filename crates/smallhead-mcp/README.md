# smallhead-mcp

Explicit MCP client connections using the official `rmcp = 3.5.1` SDK. The
crate declares Rust 1.88 (the SDK's minimum), independently of the workspace's
1.85 metadata; this repository currently pins the Rust 1.99 toolchain.

The manager is constructed synchronously and never starts a server on load,
list, or tool registration. The host should catch `McpManager::load` errors,
display the safe diagnostic, and keep the TUI usable (for example with an empty
manager). Configuration errors are not silently treated as successful loads.

## Configuration

```json
{
  "mcpServers": {
    "local-files": {
      "command": "/absolute/path/to/an/already-installed-mcp-server",
      "args": ["--root", "/workspace"],
      "cwd": "/workspace",
      "env": {"LOG_LEVEL": "warn"},
      "description": "Workspace file tools",
      "disabled": false,
      "connect_timeout_ms": 30000,
      "list_timeout_ms": 30000,
      "call_timeout_ms": 60000
    },
    "remote": {
      "url": "https://example.invalid/mcp",
      "http_headers": {"Authorization": "MY_MCP_AUTHORIZATION"},
      "description": "Remote tools",
      "disabled": true
    }
  }
}
```

Each entry specifies exactly one of `command` or `url`. `http_headers` maps
header names to environment variable **names**. For the example, the environment
variable contains the full value, such as `Bearer ...`. Values are resolved only
when connecting, marked sensitive, and excluded from snapshots/errors. `env`
contains literal child-environment overrides; the child otherwise inherits the
parent environment. No shell interpolation is performed. Relative `cwd` values
are relative to the host process working directory, not the config file.

Names use ASCII letters/digits, `.`, `_`, and `-` (no whitespace). URL userinfo
and fragments are rejected. Descriptions are local display metadata: do not put
credentials in them. Disabled defaults to false; enabled still does not mean
automatically connected. Timeouts must be 1..300000 milliseconds.

## Host API

```rust,ignore
let manager = Arc::new(McpManager::load(config_path)?);
let view = manager.snapshots(); // sync, no I/O
let ready = manager.connect("local-files").await?;
let tools = manager.tools("local-files").await?;
let result = manager.call("local-files", "discovered-tool", json!({})).await?;
manager.disconnect("local-files").await?;
```

- `new(McpConfig) -> Result<McpManager, McpError>`
- `load(impl AsRef<Path>) -> Result<McpManager, McpError>`
- `snapshots() -> Vec<ConnectionSnapshot>`; async `list()` is an alias
- async `connect(name)`, `reconnect(name)`, `disconnect(name)` return
  `Result<ConnectionSnapshot, McpError>`
- async `tools(name) -> Result<Vec<McpToolInfo>, McpError>`
- async `call(name, tool, arguments: Value) -> Result<Value, McpError>`
- async `reload(path) -> Result<(), McpError>` validates the new file before
  disconnecting old clients and replacing configuration; it does not connect
- async `disconnect_all() -> Result<(), McpError>` closes owned transports

Use one `Arc<McpManager>` across UI and tasks. One active operation per server is
allowed; concurrent operations on that server return `Busy` without queuing.
Different servers can progress independently. Reload/disconnect-all require no
active operations, returning `Busy` otherwise. Cancel/finish active workers before
shutdown. Bulk disconnect visits every server even if one close fails, returning
the first error. It is sequential, with a five-second close bound per connection.
If reload cleanup fails, configuration is retained, but some clients may already
be disconnected. Retry is explicit and never replays a tool call.

Snapshots contain only `name`, `transport` (`stdio`/`http`), `status`, `tool_count`,
local `description`, and safe `last_error`. `ready` means initialization and tool
discovery completed; it is separate from an ordinary process being alive. Status
values are `disconnected`, `connecting`, `ready`, `failed`, and `disabled`.
Tool count is the most recent discovered count, not a live capability guarantee.

`McpTool::new(manager)` implements `smallhead_core::tool::ToolImpl`, named `mcp`.
The host registers it through its generic dynamic-tool support. Its actions are:

```json
{"action":"list"}
{"action":"connect","server":"local-files"}
{"action":"tools","server":"local-files"}
{"action":"call","server":"local-files","tool":"discovered-tool","arguments":{}}
{"action":"disconnect","server":"local-files"}
```

Schemas are discovered only for the requested server, never injected wholesale
into global tool registration. Remote descriptions, schemas and results are
untrusted external content. The call result is the SDK result serialized as JSON,
preserving `content`, `structuredContent`, and `isError`. The generic tool sets its
own error flag from `isError`; image/resource blocks remain JSON for core artifact
projection. Resources/prompts listing, subscriptions, sampling, elicitation, and
multi-round tool continuations are future work.

## Lifetimes, Cancellation, And Bounds

The SDK owns stdio children. We set kill-on-drop, pipe stderr, and drain it into a
discard sink with constant buffer space. No stderr text is retained or displayed,
because servers may emit credentials. A guard aborts the drain even if the
initialization future is dropped. Unix launchers run in a new process group via
`process-wrap`; forced SDK shutdown kills that group and reaps its owned child.
Explicit disconnect awaits SDK transport close (grace period three seconds,
outer close bound five seconds). HTTP disconnect only closes the client session.

Drop/abort cleanup is asynchronous, not a synchronous reap guarantee. Use explicit
disconnect for awaited cleanup. Process groups cannot contain children that
deliberately detach into a different group/session. A launcher that exits normally
while leaving background grandchildren can also outlive the SDK's normal-child
wait path; configure foreground MCP servers, not daemonizing launchers. Windows
uses immediate-child cleanup. No external PID-based kill commands are used.

Timeout, caller cancellation, protocol failure or unavailable oversized call
results fail-close the SDK connection and mark it failed. A submitted call may
already have changed external state: inspect before deciding to retry. No calls
are automatically replayed. HTTP redirects, reqwest retries and expired-session
reinitialization/replay are disabled. Normal tool-level `isError` results are
preserved and do not themselves disconnect a healthy transport.

Bounds: 64 servers; 1 MiB config files; 64 KiB per server config; 128 tools and 8
pages per discovery, with repeated cursor/name rejection; 4 MiB aggregate schema
or returned-result JSON; 256 KiB call arguments; one active request per server.
SSE events are limited to 4 MiB. **The SDK decodes stdio messages and ordinary HTTP
JSON responses before our result-size checks**, so these are retention/output
bounds, not a hostile-peer memory-isolation boundary. Configure trusted servers.
The manager does not log raw configuration, headers, URLs, or remote error text.
Hosts should also avoid enabling verbose SDK/HTTP transport tracing around secrets.

## Tests

```sh
cargo test -p smallhead-mcp --features test-server
cargo clippy -p smallhead-mcp --all-targets --features test-server -- -D warnings
```

All fixtures are local: an SDK stdio server and a loopback HTTP protocol fixture.
No real MCP server is installed and no external service receives writes. The
`test-server` feature only enables the fixture binary/server-side SDK code; it is
not needed by the production client. The test-only schemars pin avoids the
registry's currently incomplete 1.2.2/derive pair.
