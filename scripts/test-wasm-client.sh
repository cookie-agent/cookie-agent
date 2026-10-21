#!/usr/bin/env bash
# Runs the protocol client's browser tests (crates/protocol/tests/browser.rs)
# in headless Chrome against a real `cookie daemon` in a scratch home. The
# test page's origin is foreign to the daemon, which takes it like any other.
#
# Needs the wasm32-unknown-unknown target, `wasm-bindgen-test-runner` of the
# wasm-bindgen version in Cargo.lock (`cargo install wasm-bindgen-cli
# --version <it> --locked`), and Chrome or Chromium with a matching
# chromedriver (scripts/web-e2e/chrome.sh finds or fetches them; CHROME and
# CHROMEDRIVER override).
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
# shellcheck source=web-e2e/chrome.sh
source scripts/web-e2e/chrome.sh
find_chrome

cargo build --locked -p cookie_agent
cookie="${CARGO_TARGET_DIR:-$root/target}/debug/cookie"

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

home="$scratch/home"
write_config "$home" <<'EOF'
[providers."custom.local"]
source = "custom"
endpoint = "http://127.0.0.1:9/v1"
adaptor = "openai-compatible"
auth = { method = "no-auth-v1", values = {} }

[providers."custom.local".models.test]
display_name = "Browser Test"
capabilities = { input = ["text"], output = ["text"], context_tokens = 65536, output_tokens = 2048, tool_calling = true, parallel_tool_calls = false, structured_output = false, reasoning = false, temperature = true, top_p = true, seed = false, native_replay = "unsupported", media = {} }
EOF
mkdir -p -m 700 "$home/.cookie-agent/agents"
cat >"$home/.cookie-agent/agents/primary.md" <<'EOF'
---
description: Browser test agent
mode: primary
enabled: true
models:
  - { model: "custom.local/test", variant: null }
---
Help.
EOF

start_daemon "$home"
ready="$ready_json"

# wasm-bindgen-test-runner drives the driver its environment names first
# (CHROMEDRIVER, from find_chrome), the browser webdriver.json names, and
# adds the headless switches itself.
unset GECKODRIVER SAFARIDRIVER
python3 -I -c 'import json, sys; json.dump({"goog:chromeOptions": {"binary": sys.argv[1]}}, sys.stdout)' \
    "$CHROME" >"$scratch/webdriver.json"
export WASM_BINDGEN_TEST_WEBDRIVER_JSON="$scratch/webdriver.json"

COOKIE_WASM_TEST_URL="$(json_field "$ready" url)" \
COOKIE_WASM_TEST_TOKEN="$(json_field "$ready" token)" \
COOKIE_WASM_TEST_WORKSPACE="$home/workspace" \
CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=wasm-bindgen-test-runner \
    cargo test --locked --target wasm32-unknown-unknown -p cookie_agent_protocol \
    --features websocket --test browser "$@"
