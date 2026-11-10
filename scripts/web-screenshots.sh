#!/usr/bin/env bash
# Photographs the web client for the landing page: a `cookie daemon` in a
# scratch home with a scripted model (scripts/web-e2e/showcase-model.py)
# answers one request with an edit, a `bash` check, and a markdown reply, and
# scripts/web-e2e/showcase.py captures the page in headless Chrome, light
# and dark, at desktop and phone sizes. Without a mode it then replays the
# session in the TUI's `render_preview` test and photographs that too, as
# `tui-light.png` and `tui-dark.png` (scripts/tui_preview_html.py). Run
# scripts/build-web.sh first so the daemon embeds the real page.
#
# Usage: scripts/web-screenshots.sh [--all | --composer | --share] <output-dir>
# With --share it also photographs a reply that attaches a chart and an HTML
# report, the report open beside the chat (share-light.png, share-dark.png).
# With --all it also photographs every panel, sheet, and menu, and the
# connect screen, at both sizes and in both themes, under <output-dir>/all/.
# With --composer it photographs the composer in each of its states, the
# bottom bar in each permission mode, and the tool rows collapsed and open,
# under <output-dir>/composer/.
# Needs Chrome or Chromium and a matching chromedriver
# (scripts/web-e2e/chrome.sh finds or fetches them; CHROME and CHROMEDRIVER
# override).
set -euo pipefail

mode=""
if [[ "${1:-}" == "--all" ]]; then
    mode="all"
    shift
elif [[ "${1:-}" == "--composer" ]]; then
    mode="composer"
    shift
elif [[ "${1:-}" == "--share" ]]; then
    mode="share"
    shift
fi
if (($# != 1)); then
    echo "usage: $0 [--all | --composer] <output-dir>" >&2
    exit 2
fi
mkdir -p "$1"
out="$(cd "$1" && pwd)"

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

# shellcheck source=web-e2e/chrome.sh
source scripts/web-e2e/chrome.sh
find_chrome
CHROME_ARGS="$(chrome_args_json)"
export CHROME_ARGS
if [[ ! -f crates/web_assets/bundle/index.html ]]; then
    echo "no web bundle; run scripts/build-web.sh first" >&2
    exit 1
fi

cargo build --locked -p cookie_agent
cookie="${CARGO_TARGET_DIR:-$root/target}/debug/cookie"

scratch="$(mktemp -d)"
pids=()
cleanup() {
    for pid in "${pids[@]}"; do
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    done
    stop_browsers "$scratch/browsers"
    rm -rf "$scratch"
}
trap cleanup EXIT

free_port() {
    python3 -I -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])'
}

python3 -I scripts/web-e2e/showcase-model.py "$scratch/model.port" &
pids+=("$!")
for _ in $(seq 1 100); do
    [[ -s "$scratch/model.port" ]] && break
    sleep 0.1
done
model_port="$(cat "$scratch/model.port")"

home="$scratch/home"
# Under the home, so its paths show as ~/greeter.
workspace="$home/greeter"
mkdir -p -m 700 "$home/.cookie-agent/agents" "$workspace" "$home/website"
cat >"$home/.cookie-agent/agents/primary.md" <<'AGENT'
---
description: Coding agent
mode: primary
enabled: true
models:
  - { model: "openai/gpt-5", variant: null }
permissions:
  read: allow
  write: allow
  bash: allow
---
Help with the code in this workspace.
AGENT
# More agents for the Agents view: a subagent, and a preset with one.
mkdir -p "$home/.cookie-agent/agents/fast"
cat >"$home/.cookie-agent/agents/reviewer.md" <<'AGENT'
---
description: Reviews a diff for bugs and missing tests
mode: subagent
enabled: true
models:
  - { model: "openai/gpt-5", variant: null }
permissions:
  read: allow
---
Review the change you are given.
AGENT
cat >"$home/.cookie-agent/agents/fast/primary.md" <<'AGENT'
---
description: Quick answers with a small model
mode: primary
enabled: true
models:
  - { model: "openai/gpt-5", variant: null }
permissions:
  read: allow
---
Answer briefly.
AGENT
# Project memories in two folders for the Memory view, and a file in one
# that is not a memory.
memory="$home/.cookie-agent/memory/projects"
mkdir -p "$memory/greeter" "$memory/website"
printf -- '---\ndescription: Run the checks with python3 test_greet.py\nkeywords: [python, test]\n---\nRun `python3 test_greet.py` from the repository root.\n' >"$memory/greeter/checks.md"
printf -- '---\ndescription: Greetings end with an exclamation mark\nkeywords: [style]\n---\nGreet as "Hello, <name>!".\n' >"$memory/greeter/style.md"
printf -- '---\ndescription: Deploys go through the staging site first\nkeywords: [deploy]\n---\nPush to staging, check, then promote.\n' >"$memory/website/deploy.md"
printf 'scratch notes\n' >"$memory/greeter/notes.txt"
cat >"$workspace/greet.py" <<'PY'
def greet(name):
    """Greets `name` by name."""
    return "Hello " + name
PY
cat >"$workspace/test_greet.py" <<'PY'
from greet import greet

assert greet("Ada") == "Hello, Ada!", greet("Ada")
print("ok: greet(\"Ada\") == \"Hello, Ada!\"")
PY
cp scripts/web-e2e/showcase-files/* "$workspace/"
port="$(free_port)"
# Set up for onboarding (a provider and the default, small, and utility
# models), so no first-run sheet covers the page.
cat >"$home/.cookie-agent/config.toml" <<TOML
default_model = "openai/gpt-5"
small_model = "openai/gpt-5"
utility_model = "openai/gpt-5"

[server]
port = $port

# Two more coding runtimes on this machine, started on use; studio is
# pinned to a machine of its own below, for the session rows' badges.
[runtimes.alt]
command = ["$cookie", "runtime", "serve", "--stdio"]

[runtimes.studio]
command = ["$cookie", "runtime", "serve", "--stdio"]

# A runtime on another machine over ssh, never started here.
[runtimes.labpc]
command = ["ssh", "-o", "BatchMode=yes", "labpc", "cookie", "runtime", "serve", "--stdio"]

[providers."openai"]
source = "custom"
endpoint = "http://127.0.0.1:$model_port/v1"
adaptor = "openai-compatible"
auth = { method = "no-auth-v1", values = {} }

[providers."openai".models.gpt-5]
display_name = "GPT-5"
capabilities = { input = ["text"], output = ["text"], context_tokens = 400000, output_tokens = 128000, tool_calling = true, parallel_tool_calls = false, structured_output = false, reasoning = false, temperature = true, top_p = true, seed = false, native_replay = "unsupported", media = {} }
TOML
chmod 600 "$home/.cookie-agent/config.toml"

(cd "$workspace" && HOME="$home" exec "$cookie" daemon >"$home/daemon.out" 2>"$home/daemon.err") &
pids+=("$!")
# The daemon takes connections at `daemon-ready` and serves them once it
# says it is ready.
for _ in $(seq 1 200); do
    grep -q '^cookie daemon ready$' "$home/daemon.out" 2>/dev/null && break
    sleep 0.1
done
if ! grep -q '^cookie daemon ready$' "$home/daemon.out"; then
    echo "the daemon did not get ready:" >&2
    cat "$home/daemon.err" >&2
    exit 1
fi
link="$(HOME="$home" "$cookie" web)"
# A second machine for the Machines view: labpc pinned to it.
HOME="$home" "$cookie" machine assign labpc labpc >/dev/null
HOME="$home" "$cookie" machine assign studio studio >/dev/null

driver_port="$(free_port)"
start_chromedriver "$driver_port" "$scratch/browsers" "$scratch/chromedriver.log"
pids+=("$!")
python3 -I scripts/web-e2e/showcase.py "$driver_port" "$link" "$out" $mode

# The TUI: the same session, replayed up to the end of its first run, in a
# 96×34 terminal. The replacements hide what only the test harness shows: its
# stand-in model, the scratch home, and its closed transport.
if [[ -z "$mode" ]]; then
    events="$(grep -l 'Fix the greeting' "$home"/.cookie-agent/sessions/*/*/events.jsonl | head -n 1)"
    first_run="$(grep -n '"type":"run_finished"' "$events" | head -n 1 | cut -d: -f1)"
    for theme in light dark; do
        dark=()
        [[ "$theme" == dark ]] && dark=(--dark)
        env PREVIEW_EVENTS="$events" PREVIEW_OUT="$scratch/tui-$theme.jsonl" \
            PREVIEW_WIDTH=96 PREVIEW_HEIGHT=34 PREVIEW_EVENT_LIMIT="$first_run" \
            ${dark:+PREVIEW_DARK=1} \
            cargo test --locked -q -p cookie_agent_tui render_preview -- --ignored >/dev/null
        python3 -I scripts/tui_preview_html.py "$scratch/tui-$theme.jsonl" "$scratch/tui-$theme.html" \
            "${dark[@]}" --blank-scrollbar \
            --replace 'Arbitrary Model\[base\] =GPT-5 |─' \
            --replace ' transport closed =|─' \
            --replace "${scratch//./\\.}\\S*=~/greeter" \
            --replace 'events ≥ warning='
        python3 -I scripts/web-e2e/screenshot.py "$driver_port" "file://$scratch/tui-$theme.html" \
            "$out/tui-$theme.png" 888 700
    done
fi
