#!/usr/bin/env bash
# Runs the web client's browser scenarios (scripts/web-e2e/scenarios.py) in
# headless Firefox against real `cookie daemon`s in scratch homes: one with
# a scripted model (scripts/web-e2e/mock-model.py) whose
# `[server] allowed_origins` lists the page's origin, and one that allows no
# origin. Run scripts/build-web.sh first so the daemons embed the real page.
#
# Needs Firefox and geckodriver (on PATH, or set GECKODRIVER). With
# SCREENSHOT_DIR set, it saves a screenshot of each step.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

geckodriver="${GECKODRIVER:-$(command -v geckodriver || true)}"
if [[ -z "$geckodriver" ]]; then
    echo "geckodriver is missing; install it or set GECKODRIVER" >&2
    exit 1
fi
if [[ ! -f crates/web_assets/bundle/index.html ]]; then
    echo "no web bundle; run scripts/build-web.sh first" >&2
    exit 1
fi

cargo build --locked -p cookie_agent
cookie="$root/target/debug/cookie"

scratch="$(mktemp -d)"
pids=()
cleanup() {
    for pid in "${pids[@]}"; do
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    done
    rm -rf "$scratch"
}
trap cleanup EXIT

free_port() {
    python3 -I -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])'
}

python3 -I scripts/web-e2e/mock-model.py "$scratch/model.port" &
pids+=("$!")
for _ in $(seq 1 100); do
    [[ -s "$scratch/model.port" ]] && break
    sleep 0.1
done
model_port="$(cat "$scratch/model.port")"

# Starts a daemon in home $1 on port $2 whose config.toml holds stdin after
# its `[server]` table with `allowed_origins = $3`, and sets `link` to what
# `cookie web` prints for it.
start_daemon() {
    local home="$1" port="$2" origins="$3"
    mkdir -p -m 700 "$home/.cookie-agent/agents" "$home/workspace"
    {
        printf '[server]\nport = %s\nallowed_origins = %s\n\n' "$port" "$origins"
        cat
    } >"$home/.cookie-agent/config.toml"
    chmod 600 "$home/.cookie-agent/config.toml"
    (cd "$home/workspace" && HOME="$home" exec "$cookie" daemon >"$home/daemon.out" 2>"$home/daemon.err") &
    pids+=("$!")
    for _ in $(seq 1 200); do
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
mkdir -p -m 700 "$allowing/.cookie-agent/agents"
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
---
Help.
AGENT
start_daemon "$allowing" "$port" "[\"http://127.0.0.1:$port\"]" <<TOML
[session_title]
generate_on_first_turn = false

[providers."custom.local"]
source = "custom"
endpoint = "http://127.0.0.1:$model_port/v1"
adaptor = "openai-compatible"
auth = { method = "no-auth-v1", values = {} }

[providers."custom.local".models.test]
display_name = "Scripted Model"
capabilities = { input = ["text"], output = ["text"], context_tokens = 65536, output_tokens = 2048, tool_calling = true, parallel_tool_calls = false, structured_output = false, reasoning = false, temperature = true, top_p = true, seed = false, native_replay = "unsupported", media = {} }
TOML
allowing_link="$link"
refusing_port="$(free_port)"
start_daemon "$scratch/refusing" "$refusing_port" "[]" </dev/null
refusing_link="$link"

driver_port="$(free_port)"
"$geckodriver" --port "$driver_port" >"$scratch/geckodriver.log" 2>&1 &
pids+=("$!")

python3 -I scripts/web-e2e/scenarios.py "$driver_port" "$allowing_link" "$refusing_link" \
    "$refusing_port" "${SCREENSHOT_DIR:-}"
