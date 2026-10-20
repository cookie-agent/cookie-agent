#!/usr/bin/env bash
# Runs the protocol client's browser tests (crates/protocol/tests/browser.rs)
# in headless Firefox against two real `cookie daemon`s in scratch homes: one
# whose `[server] allowed_origins` is ["*"], and one that allows no origin.
#
# Needs the wasm32-unknown-unknown target, `wasm-bindgen-test-runner` of the
# wasm-bindgen version in Cargo.lock (`cargo install wasm-bindgen-cli
# --version <it> --locked`), Firefox, and geckodriver (on PATH, or set
# GECKODRIVER).
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

wasm_bindgen="$(sed -n '/^name = "wasm-bindgen"$/{n;s/^version = "\(.*\)"$/\1/p;q}' Cargo.lock)"
if ! runner_version="$(wasm-bindgen-test-runner --version 2>/dev/null)"; then
    echo "wasm-bindgen-test-runner is missing: cargo install wasm-bindgen-cli --version $wasm_bindgen --locked" >&2
    exit 1
fi
if [[ "$runner_version" != *" $wasm_bindgen"* ]]; then
    echo "wasm-bindgen-test-runner is $runner_version; Cargo.lock has wasm-bindgen $wasm_bindgen" >&2
    exit 1
fi
export GECKODRIVER="${GECKODRIVER:-$(command -v geckodriver || true)}"
if [[ -z "$GECKODRIVER" ]]; then
    echo "geckodriver is missing; install it or set GECKODRIVER" >&2
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

# Writes a private config.toml for the home $1 with the contents of stdin.
write_config() {
    mkdir -p -m 700 "$1/.cookie-agent"
    cat >"$1/.cookie-agent/config.toml"
    chmod 600 "$1/.cookie-agent/config.toml"
}

# Starts a daemon on a free port in home $1 and sets ready_json to its ready
# line's JSON.
start_daemon() {
    local home="$1" out="$1/daemon.out"
    mkdir -p -m 700 "$home/workspace"
    (cd "$home/workspace" && HOME="$home" exec "$cookie" daemon --port 0 >"$out" 2>"$home/daemon.err") &
    pids+=("$!")
    for _ in $(seq 1 200); do
        if grep -q '^daemon-ready ' "$out" 2>/dev/null; then
            ready_json="$(sed -n 's/^daemon-ready //p' "$out" | head -n 1)"
            return
        fi
        sleep 0.1
    done
    echo "the daemon in $home did not get ready:" >&2
    cat "$home/daemon.err" >&2
    exit 1
}

json_field() {
    python3 -I -c 'import json, sys; print(json.loads(sys.argv[1])[sys.argv[2]])' "$1" "$2"
}

allowing="$scratch/allowing"
write_config "$allowing" <<'EOF'
[server]
allowed_origins = ["*"]

[providers."custom.local"]
source = "custom"
endpoint = "http://127.0.0.1:9/v1"
adaptor = "openai-compatible"
auth = { method = "no-auth-v1", values = {} }

[providers."custom.local".models.test]
display_name = "Browser Test"
capabilities = { input = ["text"], output = ["text"], context_tokens = 65536, output_tokens = 2048, tool_calling = true, parallel_tool_calls = false, structured_output = false, reasoning = false, temperature = true, top_p = true, seed = false, native_replay = "unsupported", media = {} }
EOF
mkdir -p -m 700 "$allowing/.cookie-agent/agents"
cat >"$allowing/.cookie-agent/agents/primary.md" <<'EOF'
---
description: Browser test agent
mode: primary
enabled: true
models:
  - { model: "custom.local/test", variant: null }
---
Help.
EOF

refusing="$scratch/refusing"
write_config "$refusing" <<'EOF'
[server]
allowed_origins = []
EOF

start_daemon "$allowing"
ready="$ready_json"
start_daemon "$refusing"
refusing_ready="$ready_json"

COOKIE_WASM_TEST_URL="$(json_field "$ready" url)" \
COOKIE_WASM_TEST_TOKEN="$(json_field "$ready" token)" \
COOKIE_WASM_TEST_WORKSPACE="$allowing/workspace" \
COOKIE_WASM_TEST_REFUSING_URL="$(json_field "$refusing_ready" url)" \
CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=wasm-bindgen-test-runner \
    cargo test --locked --target wasm32-unknown-unknown -p cookie_agent_protocol \
    --features websocket --test browser "$@"
