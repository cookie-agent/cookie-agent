#!/usr/bin/env bash
# Photographs the web client for the landing page: a `cookie daemon` in a
# scratch home with a scripted model (scripts/web-e2e/showcase-model.py)
# answers one request with an edit, a `bash` check, and a markdown reply, and
# scripts/web-e2e/showcase.py captures the page in headless Firefox, light
# and dark, at desktop and phone sizes. Run scripts/build-web.sh first so the
# daemon embeds the real page.
#
# Usage: scripts/web-screenshots.sh [--all | --composer] <output-dir>
# With --all it also photographs every panel, sheet, and menu, and the
# connect screen, at both sizes and in both themes, under <output-dir>/all/.
# With --composer it photographs the composer in each of its states, the
# bottom bar in each permission mode, and the tool rows collapsed and open,
# under <output-dir>/composer/.
# Needs Firefox and geckodriver (on PATH, or set GECKODRIVER).
set -euo pipefail

mode=""
if [[ "${1:-}" == "--all" ]]; then
    mode="all"
    shift
elif [[ "${1:-}" == "--composer" ]]; then
    mode="composer"
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
port="$(free_port)"
cat >"$home/.cookie-agent/config.toml" <<TOML
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
for _ in $(seq 1 200); do
    grep -q '^daemon-ready ' "$home/daemon.out" 2>/dev/null && break
    sleep 0.1
done
if ! grep -q '^daemon-ready ' "$home/daemon.out"; then
    echo "the daemon did not get ready:" >&2
    cat "$home/daemon.err" >&2
    exit 1
fi
link="$(HOME="$home" "$cookie" web)"
# A second machine for the Machines view: labpc pinned to it.
HOME="$home" "$cookie" machine assign labpc labpc >/dev/null
HOME="$home" "$cookie" machine assign studio studio >/dev/null

driver_port="$(free_port)"
# System access lets the driver zoom the page to a phone's width.
"$geckodriver" --port "$driver_port" --allow-system-access >"$scratch/geckodriver.log" 2>&1 &
pids+=("$!")
python3 -I scripts/web-e2e/showcase.py "$driver_port" "$link" "$out" $mode
