#!/usr/bin/env bash
# Runs the web client's browser scenarios (scripts/web-e2e/scenarios.py) in
# headless Chrome against real `cookie daemon`s in scratch homes: one with
# a scripted model (scripts/web-e2e/mock-model.py), a subagent, a skill, a
# runtime that never starts, the reference plugin, and MCP servers that want
# OAuth (scripts/web-e2e/mock-oauth.py), with, for the runtime_mcp group,
# MCP servers in its `local` runtime, for the memory group, an agent that
# keeps memory, and a second, empty
# one whose page is of another origin. Run scripts/build-web.sh first so the
# daemons embed the real page.
#
# Usage: scripts/test-web-page.sh [scenario ...]
#
# Without arguments it runs every scenario; with names (scenarios.py's
# SCENARIOS) only those and the scenarios they build on. Each group of
# scenarios runs with its own daemons, chromedriver, and browser, side by
# side with the other groups; each group's output follows once it is done.
#
# Needs Chrome or Chromium and a matching chromedriver
# (scripts/web-e2e/chrome.sh finds or fetches them; CHROME and CHROMEDRIVER
# override). With SCREENSHOT_DIR set, it saves a screenshot of each step.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

# shellcheck source=web-e2e/chrome.sh
source scripts/web-e2e/chrome.sh
find_chrome
CHROME_ARGS="$(chrome_args_json)"
export CHROME_ARGS
mapfile -t groups < <(python3 -I scripts/web-e2e/scenarios.py --plan "$@")
((${#groups[@]})) || exit 1
if [[ ! -f crates/web_assets/bundle/index.html ]]; then
    echo "no web bundle; run scripts/build-web.sh first" >&2
    exit 1
fi

cargo build --locked -p cookie_agent
cookie="${CARGO_TARGET_DIR:-$root/target}/debug/cookie"
if [[ -n "${SCREENSHOT_DIR:-}" ]]; then
    mkdir -p "$SCREENSHOT_DIR"
fi

scratch="$(mktemp -d)"
sets=()
cleanup() {
    for pid in "${sets[@]}"; do
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    done
    rm -rf "$scratch"
}
trap cleanup EXIT

free_port() {
    python3 -I -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])'
}

# Stops what run_set started, which it lists in `pids`.
stop_set() {
    for pid in "${pids[@]}"; do
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    done
}

# Runs scenarios $2... in a set of their own under directory $1: the mock
# model, the daemons and their homes, chromedriver, and one browser. Run it
# in a subshell: its exit stops them. (`pids` is not local, as the exit
# trap runs after the function's variables are gone.)
run_set() {
    local scratch="$1"
    shift
    mkdir -p "$scratch"
    pids=()
    trap stop_set EXIT
    trap 'exit 143' TERM

    python3 -I scripts/web-e2e/mock-model.py "$scratch/model.port" &
    pids+=("$!")
    for _ in $(seq 1 100); do
        [[ -s "$scratch/model.port" ]] && break
        sleep 0.1
    done
    model_port="$(cat "$scratch/model.port")"
    python3 -I scripts/web-e2e/mock-oauth.py "$scratch/oauth.port" &
    pids+=("$!")
    for _ in $(seq 1 100); do
        [[ -s "$scratch/oauth.port" ]] && break
        sleep 0.1
    done
    oauth_port="$(cat "$scratch/oauth.port")"

    # Writes home $1's config.toml: a `[server]` table on port $2, then stdin.
    write_config() {
        local home="$1" port="$2"
        mkdir -p -m 700 "$home/.cookie-agent/agents" "$home/workspace"
        {
            printf '[server]\nport = %s\n\n' "$port"
            cat
        } >"$home/.cookie-agent/config.toml"
        chmod 600 "$home/.cookie-agent/config.toml"
    }

    # Starts the daemon of home $1 and sets `link` to what `cookie web` prints
    # for it. A debug daemon compiles the model catalog before it is ready,
    # seconds of CPU, while every group starts its daemons and browser at
    # once: on a cold four-core runner that takes longer than 20 s.
    start_daemon() {
        local home="$1"
        (cd "$home/workspace" && HOME="$home" exec "$cookie" daemon >"$home/daemon.out" 2>"$home/daemon.err") &
        pids+=("$!")
        echo "$!" >"$home/daemon.pid"
        for _ in $(seq 1 600); do
            grep -q '^daemon-ready ' "$home/daemon.out" 2>/dev/null && break
            sleep 0.1
        done
        if ! grep -q '^daemon-ready ' "$home/daemon.out"; then
            echo "the daemon in $home did not get ready:" >&2
            cat "$home/daemon.err" >&2
            exit 1
        fi
        link="$(HOME="$home" "$cookie" web)"
    }

    port="$(free_port)"
    allowing="$scratch/allowing"
    mkdir -p -m 700 "$allowing/.cookie-agent/agents" "$allowing/.cookie-agent/skills/greet" \
        "$allowing/workspace"
    cat >"$allowing/.cookie-agent/agents/primary.md" <<'AGENT'
---
description: Browser scenario agent
mode: primary
enabled: true
models:
  - { model: "custom.local/test", variant: null }
permissions:
  read: allow
  bash: ask
  delegate: allow
  skill: allow
---
Help.
AGENT
    # The memory group's agent keeps memory, each user memory at most 1 KB.
    memory_config=""
    if [[ " $* " == *" memory_changes "* ]]; then
        sed -i 's/^  skill: allow$/  skill: allow\n  memory: allow\n  memory_write: allow/' \
            "$allowing/.cookie-agent/agents/primary.md"
        memory_config="
[memory]
user_max_bytes = 1024"
    fi
    cat >"$allowing/.cookie-agent/agents/helper.md" <<'AGENT'
---
description: Browser scenario subagent
mode: subagent
enabled: true
models:
  - { model: "custom.local/test", variant: null }
permissions:
  read: allow
  delegate: allow
---
Answer briefly.
AGENT
    # A preset with its own root agent, for a project's default agent.
    mkdir -p -m 700 "$allowing/.cookie-agent/agents/review"
    cat >"$allowing/.cookie-agent/agents/review/builder.md" <<'AGENT'
---
description: Browser scenario preset agent
mode: primary
enabled: true
models:
  - { model: "custom.local/test", variant: null }
permissions:
  read: allow
---
Build.
AGENT
    cat >"$allowing/.cookie-agent/skills/greet/SKILL.md" <<'SKILL'
---
name: greet
description: Greet someone by name.
argument-hint: <name>
---
Say hello to $ARGUMENTS.
SKILL
    # A 48×32 PNG the scripted model reads, whose result is a media block.
    python3 -I - "$allowing/workspace/dot.png" <<'PY'
import struct, sys, zlib
def chunk(kind, data):
    return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))
rows = b"".join(b"\0" + b"\xc0\x80\x40" * 48 for _ in range(32))
png = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", 48, 32, 8, 2, 0, 0, 0))
png += chunk(b"IDAT", zlib.compress(rows)) + chunk(b"IEND", b"")
open(sys.argv[1], "wb").write(png)
PY
    # The runtime_mcp group's `local` runtime hosts the stdio fixture MCP
    # server, one that cannot start, and the fixture once for the runtime
    # (`scope = "runtime"`), which the runtime panels list.
    runtime_mcp=""
    if [[ " $* " == *" runtime_mcp "* ]]; then
        runtime_mcp="
[runtimes.local.mcp.servers.fixture]
command = \"python3\"
args = [\"$root/crates/mcp/tests/fixtures/mcp_server.py\"]

[runtimes.local.mcp.servers.broken]
command = \"false\"

[runtimes.local.mcp.servers.shared]
command = \"python3\"
args = [\"$root/crates/mcp/tests/fixtures/mcp_server.py\"]
scope = \"runtime\""
    fi
    write_config "$allowing" "$port" <<TOML
[session_title]
generate_on_first_turn = false

[providers."custom.local"]
source = "custom"
endpoint = "http://127.0.0.1:$model_port/v1"
adaptor = "openai-compatible"
auth = { method = "no-auth-v1", values = {} }

[providers."custom.local".models.test]
display_name = "Scripted Model"
capabilities = { input = ["text", "image"], output = ["text"], context_tokens = 65536, output_tokens = 2048, tool_calling = true, parallel_tool_calls = false, structured_output = false, reasoning = false, temperature = true, top_p = true, seed = false, native_replay = "unsupported", media = { image = { mime_types = ["image/png", "image/jpeg"], max_bytes = 200000, max_count = 4 } } }

# A runtime whose instance never starts, for the retry and a refused card.
[runtimes.box]
command = ["false"]

# A second coding runtime on this machine, started on use.
[runtimes.alt]
command = ["$cookie", "runtime", "serve", "--stdio"]

[plugins.reference]
command = ["python3", "$root/examples/plugins/python/reference_plugin.py"]

# Remote MCP servers that want OAuth (mock-oauth.py), one for each way the
# page signs in.
[mcp.servers.oauth-tab]
url = "http://127.0.0.1:$oauth_port/mcp"

[mcp.servers.oauth-same]
url = "http://127.0.0.1:$oauth_port/mcp"

[mcp.servers.oauth-paste]
url = "http://127.0.0.1:$oauth_port/mcp"
$runtime_mcp
$memory_config
TOML
    # A session made before the daemon starts, by another process: the daemon
    # lists it but has not loaded it, so the scenarios can hold its owner lock
    # as another cookie process would.
    held_session="$(cd "$allowing/workspace" && HOME="$allowing" "$cookie" run --json "hello from another process" |
        python3 -I -c 'import json, sys; print([json.loads(l) for l in sys.stdin if l.strip()][-1]["session_id"])')"
    # A memory folder from before projects: the daemon makes it the project
    # `e2e` at start, which the memory scenario writes into.
    mkdir -p "$allowing/.cookie-agent/memory/projects/e2e"
    start_daemon "$allowing"
    allowing_link="$link"
    mkdir -p "$allowing/second" "$allowing/elsewhere"
    # A runtime that dialled in once and went away: its card is offline.
    roamer="$scratch/roamer"
    mkdir -p -m 700 "$roamer"
    (HOME="$roamer" exec "$cookie" runtime serve --daemon "ws://127.0.0.1:$port/ws" --id roamer \
        --token "${allowing_link##*#token=}" >"$roamer/out" 2>"$roamer/err") &
    roamer_pid="$!"
    for _ in $(seq 1 100); do
        grep -qi "connected\|ready" "$roamer/err" "$roamer/out" 2>/dev/null && break
        sleep 0.1
    done
    sleep 1
    kill "$roamer_pid" 2>/dev/null || true
    wait "$roamer_pid" 2>/dev/null || true
    other_port="$(free_port)"
    write_config "$scratch/other" "$other_port" </dev/null
    start_daemon "$scratch/other"
    other_link="$link"

    driver_port="$(free_port)"
    "$CHROMEDRIVER" --port="$driver_port" >"$scratch/chromedriver.log" 2>&1 &
    pids+=("$!")

    COOKIE="$cookie" OAUTH_SERVER="http://127.0.0.1:$oauth_port" python3 -I scripts/web-e2e/scenarios.py "$driver_port" "$allowing_link" "$other_link" \
        "$other_port" "$allowing" "$held_session" "${SCREENSHOT_DIR:-}" "$@"
}

if ((${#groups[@]} == 1)); then
    # shellcheck disable=SC2086  # the group's names
    (run_set "$scratch/set" ${groups[0]})
    exit
fi
for index in "${!groups[@]}"; do
    # shellcheck disable=SC2086
    run_set "$scratch/set$index" ${groups[$index]} >"$scratch/set$index.log" 2>&1 &
    sets+=("$!")
done
failed=()
for index in "${!groups[@]}"; do
    status=0
    wait "${sets[$index]}" || status=$?
    echo "=== ${groups[$index]} (exit status $status)"
    cat "$scratch/set$index.log"
    ((status == 0)) || failed+=("${groups[$index]}")
done
sets=()
if ((${#failed[@]})); then
    printf 'failed: %s\n' "${failed[@]}" >&2
    exit 1
fi
echo "all ${#groups[@]} groups passed"
