#!/usr/bin/env bash
# Opens the web page a real `cookie daemon` serves in headless Firefox and
# checks that it connects by itself from a `cookie web` link: the daemon
# runs in a scratch home whose `[server] allowed_origins` lists the page's
# origin. Run scripts/build-web.sh first so the daemon embeds the real page.
#
# Needs Firefox and geckodriver (on PATH, or set GECKODRIVER). With
# SCREENSHOT_DIR set, it also saves a screenshot of each page it checks.
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

# Starts a daemon in home $1 on port $2 whose `[server] allowed_origins` is
# $3, and sets `link` to what `cookie web` prints for it.
start_daemon() {
    local home="$1" port="$2" origins="$3"
    mkdir -p -m 700 "$home/.cookie-agent" "$home/workspace"
    printf '[server]\nport = %s\nallowed_origins = %s\n' "$port" "$origins" \
        >"$home/.cookie-agent/config.toml"
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
start_daemon "$scratch/allowing" "$port" "[\"http://127.0.0.1:$port\"]"
allowing_link="$link"
refusing_port="$(free_port)"
start_daemon "$scratch/refusing" "$refusing_port" "[]"
refusing_link="$link"

driver_port="$(free_port)"
"$geckodriver" --port "$driver_port" >"$scratch/geckodriver.log" 2>&1 &
pids+=("$!")

python3 -I - "$driver_port" "$allowing_link" "$refusing_link" "$refusing_port" "${SCREENSHOT_DIR:-}" <<'PY'
import base64, json, os, sys, time, urllib.request

driver = f"http://127.0.0.1:{sys.argv[1]}"
link, refusing_link, refusing_port, screenshots = sys.argv[2:6]

def call(method, path, body=None):
    data = None if body is None else json.dumps(body).encode()
    request = urllib.request.Request(driver + path, data=data, method=method,
                                     headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(request, timeout=60) as response:
        return json.load(response)["value"]

for _ in range(100):
    try:
        call("GET", "/status")
        break
    except OSError:
        time.sleep(0.1)

session = call("POST", "/session", {"capabilities": {"alwaysMatch": {
    "browserName": "firefox", "moz:firefoxOptions": {"args": ["-headless"]}}}})["sessionId"]
try:
    call("POST", f"/session/{session}/url", {"url": link})
    script = lambda source: call("POST", f"/session/{session}/execute/sync",
                                 {"script": source, "args": []})

    def screenshot(name):
        if screenshots:
            with open(os.path.join(screenshots, name), "wb") as file:
                file.write(base64.b64decode(call("GET", f"/session/{session}/screenshot")))
    deadline = time.time() + 30
    while time.time() < deadline:
        if script("return document.getElementById('connected') !== null"):
            break
        time.sleep(0.2)
    else:
        print("the page did not connect; it shows:", script("return document.body.innerText"),
              file=sys.stderr)
        sys.exit(1)
    text = script("return document.getElementById('connected').innerText")
    fragment = script("return location.hash")
    stored = script("return Object.keys(localStorage).filter(k => k.startsWith('cookie.token:'))")
    assert fragment == "", f"the token stayed in the address bar: {fragment!r}"
    assert stored, "the token was not kept for the daemon"
    print("the page connected:", " ".join(text.split()))
    screenshot("connected.png")

    # A daemon that allows no origin refuses the page before its handshake,
    # and the page names the entry to add.
    call("POST", f"/session/{session}/url", {"url": refusing_link})
    expected = f'"http://127.0.0.1:{refusing_port}"'
    deadline = time.time() + 30
    while time.time() < deadline:
        shown = script("return document.body.innerText")
        if expected in shown and "allowed_origins" in shown:
            break
        time.sleep(0.2)
    else:
        print("the page showed no origin hint; it shows:", shown, file=sys.stderr)
        sys.exit(1)
    print("the refused page names", expected)
    screenshot("refused.png")
finally:
    call("DELETE", f"/session/{session}")
PY
