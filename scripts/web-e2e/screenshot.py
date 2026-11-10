"""Photographs one page at a fixed size over WebDriver, for the TUI shots of
scripts/web-screenshots.sh.

    python3 -I screenshot.py <driver-port> <url> <png> <width> <height>

In the environment, `CHROME` and `CHROME_ARGS` (a JSON list) are the browser
and its switches.
"""

import base64
import json
import os
import sys
import time
import urllib.request

DRIVER = "http://127.0.0.1:%s" % sys.argv[1]
URL, PNG = sys.argv[2:4]
WIDTH, HEIGHT = int(sys.argv[4]), int(sys.argv[5])


def call(method, path, body=None):
    data = None if body is None else json.dumps(body).encode()
    request = urllib.request.Request(
        DRIVER + path, data=data, method=method, headers={"Content-Type": "application/json"}
    )
    with urllib.request.urlopen(request, timeout=60) as response:
        return json.load(response)["value"]


for _ in range(100):
    try:
        call("GET", "/status")
        break
    except OSError:
        time.sleep(0.1)
options = {"args": json.loads(os.environ.get("CHROME_ARGS") or '["--headless=new"]') + ["--hide-scrollbars"]}
if os.environ.get("CHROME"):
    options["binary"] = os.environ["CHROME"]
capabilities = {"browserName": "chrome", "goog:chromeOptions": options}
session = "/session/" + call("POST", "/session", {"capabilities": {"alwaysMatch": capabilities}})["sessionId"]
try:
    call(
        "POST",
        session + "/goog/cdp/execute",
        {
            "cmd": "Emulation.setDeviceMetricsOverride",
            "params": {"width": WIDTH, "height": HEIGHT, "deviceScaleFactor": 1, "mobile": False},
        },
    )
    call("POST", session + "/url", {"url": URL})
    call("POST", session + "/execute/async", {"script": "document.fonts.ready.then(arguments[0])", "args": []})
    time.sleep(0.5)
    with open(PNG, "wb") as file:
        file.write(base64.b64decode(call("GET", session + "/screenshot")))
    print("saved", os.path.basename(PNG))
finally:
    call("DELETE", session)
