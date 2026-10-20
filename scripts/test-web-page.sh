#!/usr/bin/env bash
# Runs the web client's browser scenarios (scripts/web-e2e/scenarios.py) in
# headless Firefox against real `cookie daemon`s in scratch homes: one with
# a scripted model (scripts/web-e2e/mock-model.py), a subagent, and a skill,
# whose `[server] allowed_origins` lists the page's origin, and one that
# allows no origin. Run scripts/build-web.sh first so the daemons embed the real page.
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

# Writes home $1's config.toml: a `[server]` table on port $2 with
# `allowed_origins = $3`, then stdin.
write_config() {
    local home="$1" port="$2" origins="$3"
    mkdir -p -m 700 "$home/.cookie-agent/agents" "$home/workspace"
    {
        printf '[server]\nport = %s\nallowed_origins = %s\n\n' "$port" "$origins"
        cat
    } >"$home/.cookie-agent/config.toml"
    chmod 600 "$home/.cookie-agent/config.toml"
}

# Starts the daemon of home $1 and sets `link` to what `cookie web` prints
# for it.
start_daemon() {
    local home="$1"
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
cat >"$allowing/.cookie-agent/agents/helper.md" <<'AGENT'
---
description: Browser scenario subagent
mode: subagent
enabled: true
models:
  - { model: "custom.local/test", variant: null }
permissions:
  read: allow
---
Answer briefly.
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
write_config "$allowing" "$port" "[\"http://127.0.0.1:$port\"]" <<TOML
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
TOML
# A session made before the daemon starts, by another process: the daemon
# lists it but has not loaded it, so the scenarios can hold its owner lock
# as another cookie process would.
held_session="$(cd "$allowing/workspace" && HOME="$allowing" "$cookie" run --json "hello from another process" |
    python3 -I -c 'import json, sys; print([json.loads(l) for l in sys.stdin if l.strip()][-1]["session_id"])')"
start_daemon "$allowing"
allowing_link="$link"
refusing_port="$(free_port)"
write_config "$scratch/refusing" "$refusing_port" "[]" </dev/null
start_daemon "$scratch/refusing"
refusing_link="$link"

driver_port="$(free_port)"
"$geckodriver" --port "$driver_port" >"$scratch/geckodriver.log" 2>&1 &
pids+=("$!")

python3 -I scripts/web-e2e/scenarios.py "$driver_port" "$allowing_link" "$refusing_link" \
    "$refusing_port" "$allowing" "$held_session" "${SCREENSHOT_DIR:-}"
