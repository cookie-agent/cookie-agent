# Machines

A *machine* is the daemon's name for a computer its runtimes run on. Several
runtimes can share one: the daemon's `local` runtime, a `cookie runtime serve`
started over ssh, and a TUI's `client_<host>` runtime on the same laptop. The
daemon groups them so that you, and the agents' [memory](memory.md), can talk
about the computer rather than each connection to it.

## How a runtime finds its machine

A runtime reports a machine ID when it connects. cookie's own runtimes report
a hash of the operating system's machine ID (`/etc/machine-id` or
`/var/lib/dbus/machine-id`), or of a random ID kept in
`~/.cookie-agent/machine-id` where there is none, together with the host name.
The daemon then places the runtime, first match first:

1. on the machine the runtime ID is pinned to;
2. on the machine that already lists the reported machine ID;
3. on a new machine named after the host name;
4. without a reported ID, on a new machine named after the runtime ID, which
   pins it there.

A runtime's machine is worked out each time from the machines, which are
stored in `~/.cookie-agent/machines.json`; only the daemon writes it.

## Offline runtimes

The registry also remembers where each runtime ran: when a template or
dial-in runtime connects, the daemon records the machine ID, platform, and
home directory it reported, and when it last saw the runtime connect or
disconnect. A runtime that is not connected stays on its pin's machine, else
on the machine its record names, shown as offline with when it was last
seen, across daemon restarts too. Only a runtime that has never connected is
on no machine. So an ssh template's sessions keep their machine badges and
`~/…` paths while it is down, and `list_runtimes` still places it.

A record goes away with **forget** on the offline runtime
(`cookie machine forget-runtime <runtime>`, or **Forget** in the web
client's Machines view), with its machine when the machine is forgotten,
and, for a template, when `config.toml` no longer has its
`[runtimes.<id>]` table at the next reload or restart. A dial-in runtime's
record stays until it is forgotten.

## Regrouping

Two IDs can be one computer (a container and its host), or one ID can be the
wrong place for a runtime. `cookie machine`, the **Machines** tab of the
TUI's `/runtime`, and the web client's **Machines & runtimes** view (where
a runtime moves by dragging it onto another machine) change the registry:

- **rename** a machine, which moves its memory folder;
- **assign** a runtime ID to a machine, pinning it, or **unassign** it so it
  follows the ID it reports;
- **merge** one machine into another, moving its IDs, pins, and memories;
- **forget** a machine that has no connected runtime and no memories (its
  offline runtimes' records go with it);
- **forget** an offline runtime, so the machine no longer lists it.

See [`cookie machine`](../clients/cli.md#cookie-machine) and
[Machines](../daemon/runtimes.md#machines).

## Memory per machine

The `machine` memory layer keeps notes per machine:
`machine://<machine>/<name>`, such as where a toolchain lives or which ports are
free on it. Renaming or merging a machine carries its memories along. See
[Memory](memory.md).
