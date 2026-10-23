# MCP tool filters

Status: **approved** (2026-10-23), being implemented. Builds on
[permission consolidation](permission-consolidation.md) and the MCP scope
work in the [runtimes spec](runtimes.md).

## 1. Goal and scope

An MCP server publishes every tool it lists, each as `<server>_<tool>`. Two
things are missing:

- **No way to drop a server's tools.** A server that offers 40 tools, of
  which three are useful, costs every agent the other 37 in its tool list.
- **Permissions gate calls, not the tool list.** With
  `mcp: {"my_chrome_*": allow}`, an agent may call only `my_chrome_*`
  tools, but it is offered every MCP tool of every server, because a tool
  is offered when any merged `mcp` rule allows or asks
  (`MergedRules::offers`). Calls to the others are denied, wasting turns.

**Decided 2026-10-23:**

- Each MCP server takes `include_tools` and `exclude_tools`, patterns over
  the server's own tool names with `*` and `?`, applied before its tools are
  published (section 2).
- An MCP tool is offered only when the merged rules do not deny its
  published name (section 3, confirmed 2026-10-23).

**Out of scope:** filtering prompts or resources, renaming tools, per-agent
server lists (permissions cover that, section 3).

## 2. Per-server filters

```toml
[mcp.servers.my_chrome]
url = "https://example.test/mcp"
include_tools = ["browser_*"]          # default: every tool
exclude_tools = ["browser_install"]    # applied after include_tools
```

- **Where:** every place an MCP server is configured: `[mcp.servers.<name>]`,
  `[runtimes.<id>.mcp.servers.<name>]`, and the `[runtimes.local.mcp]`
  servers a dial-in runtime reads from its own machine.
- **What they match:** the tool's name as the server lists it (`browser_click`),
  not the published `<server>_<tool>` name, so a filter survives a server
  rename. Patterns use `*` (any characters) and `?` (one character), as
  permission patterns do; a pattern without either is an exact name.
- **Order:** a tool is published when it matches `include_tools` (every tool
  when the field is absent) and matches no `exclude_tools` pattern.
- **When:** to the server's `tools/list` result and to every refresh after
  `notifications/tools/list_changed`. Filtering happens where the server
  runs (the daemon, a runtime template's runtime, or a dial-in runtime), so
  no protocol carries the filters.
- **Validation:** an empty pattern, or an `include_tools` that is present but
  empty, is a configuration error, so a server cannot silently publish
  nothing.
- **Status:** the server's status reports how many tools were filtered out
  (`12 tools, 30 filtered`). `McpServerStatus` gains an optional
  `filtered_tools` count (client protocol minor, additive).
- **Clients:** the TUI and web MCP server editors gain both fields; the MCP
  sheet shows the filtered count.

## 3. MCP tools offered by their permission

- **The rule:** an MCP tool is offered only when the run's merged rules
  (permission consolidation section 3) do not deny `mcp` for its published
  name, on the tool's runtime. `allow` and `ask` offer it; `deny`, or no
  matching rule, hides it.
- **Example:** `mcp: {"my_chrome_*": allow}` offers only the `my_chrome_*`
  tools; with the default `mcp: {"*": ask}`, every MCP tool is offered and
  asks.
- **Why only MCP:** an MCP tool's permission resource is its own name,
  known when the tool list is built. Other actions' resources (a path, a
  command, a URL) are known only from a call's arguments, so they stay
  offered by action.
- **Grants:** an "allow for this session" grant offers a tool its pattern
  covers, unless the merged rules deny it, as grants do today.
- **Composition:** the per-name check runs when tools are composed for a
  model request, after the per-server filters, so the tool list changes
  when a session override changes.

## 4. Compatibility

- Configuration gains two optional fields per MCP server.
- `McpServerStatus.filtered_tools`: optional, client protocol minor.
- Section 3 is a behaviour change with no wire change: agents with narrow
  `mcp` rules see fewer tools. Noted in the protocol changelog.

## 5. Tests

- Filters: include only, exclude only, both (exclude wins), exact names, `?`,
  a server rename keeping the filter, a list-changed refresh filtered, the
  filtered count, configuration errors; the same for a runtime template's
  server and a dial-in runtime's local server.
- Per-tool offering: narrow allow offers only matching tools; `ask` offers;
  a deny pattern hides one tool of an allowed server; a session override
  changes the list; grants.

## 6. Docs

- `docs/site/extend/mcp.md`: the two fields with an example, and that
  narrow `mcp` rules now also narrow the tool list.
- The configuration reference for the MCP server table.
- The protocol changelog.

## 7. Phases

One phase: config and filtering in `cookie_agent_mcp`, the status count,
per-tool offering in the engine, the clients' editor fields, docs.

## 8. Open questions

None.
