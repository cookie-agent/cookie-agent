# Delegating to runtimes

A session can hand work to a subagent with `delegate_subagent` (see
[Subagents](../concepts/sessions.md) and the
[tools reference](../reference/tools.md#delegate_subagent)). A root session
can also say *where* that subagent works: on which runtime, and in which
workspace. A root with only the `global` runtime attached has no workspace
of its own, so it can orchestrate work across machines and leave the files
and commands to its subagents.

## A session with only `global`

The web client's **New session** starts with only `global` attached (see
[Web](../clients/web.md#sessions-and-chat)); the TUI's plain `cookie`
attaches `global` and `local` at the current directory, and its `/runtime`
panel can detach `local` before the first message. Such a session:

- has memory, MCP servers, plugins, and the other `global` tools, but no
  `read`, `write`, `edit`, or `bash`;
- gets `list_runtimes`, which lists the daemon's machines and runtimes;
- gets a paragraph in its delegation instructions saying that it has no
  workspace and that work needing files or commands goes to a subagent;
- must name where each subagent works: `delegate_subagent`'s
  `capabilities` is required.

Its row in the session list still shows where it works: its badges add the
machines and workspaces its subagents were given (see
[Badges](#badges)).

## `list_runtimes`

`list_runtimes` takes no arguments and reads the daemon's state at every
call, so a runtime that dials in mid-session shows up on the next call. It
prints every machine and its runtimes, each runtime's capabilities with
their descriptions, whether the runtime is connected or `starts_on_use`
(a template the daemon starts when a session needs it), its home directory,
and the calling session's own attachments. Other sessions' instances are
never shown. A runtime that is offline keeps the machine, platform, and home
it was last seen with ([Machines](../concepts/machines.md#offline-runtimes)).
See the [tools reference](../reference/tools.md#list_runtimes) for the
format.

The `list_runtimes` permission controls it. An agent document without a
`list_runtimes` key gets `allow` when its mode is `primary` or `all`, and no
rule when it is `subagent` or `internal`, so subagents do not see the list
unless their document grants it:

```yaml
permissions:
  list_runtimes: allow   # or deny, to hide it from a primary agent
```

## Naming where a subagent works

In a root session that has `global` attached, `delegate_subagent` takes
`capabilities`, a list of the runtime capabilities the child works through.
Each item is the full `<runtime_id>/<capability_id>`, as `list_runtimes`
prints a `<runtime id=…>` and one of its `<capability name=…>`, with a
`workspace` when the capability takes one:

```json
{
  "agent_type": "coder",
  "description": "Fix the API tests",
  "prompt": "Run the API test suite and fix what fails.",
  "capabilities": [
    {"capability": "labpc/coding", "workspace": "~/src/api"}
  ]
}
```

- **When it is there.** Required when the root has only `global`;
  optional when the root has `global` and other attachments, where leaving
  it out lets the child inherit the root's attachments; absent when the root
  has no `global`, and in every subagent: only the root chooses runtimes.
- **What the child gets.** `global`, because the root has it (whether
  children have `global` follows the root, never the model, so
  `global/global` cannot be listed), then each listed capability, in order,
  under the usual attachment names (`labpc_api`, with `_2` for a duplicate).
  The same runtime may be listed twice with different workspaces; two
  workspaces or more give the child's tools prefixes, as in any session.
- **Starting the runtimes.** The daemon starts every listed runtime before
  it creates the child, as `session.create` does: a template runtime
  starts, and a dial-in runtime must be connected. If one cannot start,
  the call fails naming the runtime and the reason, and nothing is created.
- **Refusals.** A runtime the daemon does not have, or a capability the
  runtime does not offer (the error lists the ones it does), refuses the
  call. So does a missing workspace for a capability that takes one, and
  `capabilities` together with `resume_session_id`: a resumed subagent
  keeps its own attachments.
- **Deeper children** inherit their parent's attachments, with `global`
  exactly when the root has it. A runtime attached to or detached from the
  root later reaches only the children created afterwards.

## Limiting where the root delegates

The `delegate` permission accepts a runtime selector, as other actions do
(see [Runtime selectors](../extend/agents.md#runtime-selectors)). Each
runtime a call names in `capabilities` is checked as `delegate@<runtime>`:
a runtime that denies refuses the whole call, and one that asks puts the call
through the usual approval, which lists the runtimes and workspaces. A call
without `capabilities` is checked as plain `delegate`.

```yaml
permissions:
  delegate: allow
  delegate@labpc: ask     # confirm before a subagent works on labpc
  delegate@prod: deny     # never on prod
```

The resource of `delegate` rules is the subagent's agent ID, and the most
specific rule wins: `delegate: {coder: allow}` outranks `delegate@labpc:
deny`, which is `"*": deny`. Write `delegate@labpc: {coder: deny}` to deny
that one. The child's own rules still decide what it can do on the runtime,
including any `bash@labpc`-style selectors.

## Badges

A root session's badges in the session lists count its own attachments
followed by the ones its subagents were given, each runtime, capability,
and workspace once, by the usual badge rules: one machine shows its badge and
workspaces, several machines show their badges. The daemon records them on
the root as each child is created
(`SessionMeta.delegated_attachments`), so the list needs no extra reads, and
they only grow: a child that finishes, or a detach on the root, leaves them.
A subagent's own row shows its own attachments. The TUI's session picker
shows the same places for a root with no workspace of its own.

## Bringing a machine in from a subagent

No tool starts runtimes on other machines, but a coding subagent on the
daemon's machine can bring one in for a while with the tools it has:

1. The root delegates to a coding subagent on `local`.
2. That subagent runs a background `bash` call that dials the remote
   runtime in:

    ```sh
    ssh labpc cookie runtime serve --daemon ws://daemon-host:7419/ws --id labpc --token <token>
    ```

3. The root calls `list_runtimes`, sees `labpc`, and delegates to it with
   `capabilities`.
4. Stopping the background call (`stop_background`) ends the dial-in
   connection, and the runtime shows as disconnected (and stays on its
   machine as an offline runtime until forgotten).

See [Runtimes that dial in](../runtimes/dial-in.md)
for the listener and IDs.

!!! warning "The subagent needs the daemon's token"
    Dialing in takes the daemon's token, `[server] token` in
    `~/.cookie-agent/config.toml`. Any agent that may `read` or `bash` on the
    daemon's machine can already read that file; this pattern gives it no
    new access, but it does put the token in the agent's hands. To prevent
    that, deny reads of the file in that agent's rules, for example
    `read: {"~/.cookie-agent/config.toml": deny, "*": allow}`, and keep
    `bash` from reaching it. Adding a `[runtimes.<id>]` template to
    `config.toml` instead takes effect only after `config.reload` or a
    daemon restart, which a subagent cannot trigger through a tool, so the
    dial-in path is the one to use.
