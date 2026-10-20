"""Browser scenarios for the web client, driven over WebDriver (geckodriver).

Run by scripts/test-web-page.sh:

    python3 -I scenarios.py <driver-port> <allowing-link> <refusing-link> \
        <refusing-port> [screenshot-dir]

The allowing daemon has a scripted model (mock-model.py) and an agent whose
`bash` asks for approval.
"""

import base64
import json
import os
import sys
import time
import urllib.error
import urllib.request

DRIVER = "http://127.0.0.1:%s" % sys.argv[1]
LINK, REFUSING_LINK, REFUSING_PORT = sys.argv[2:5]
SCREENSHOTS = sys.argv[5] if len(sys.argv) > 5 else ""
ELEMENT = "element-6066-11e4-a52e-4f735466cecf"
ENTER, ALT, ESCAPE, NULL = "", "", "", ""


def call(method, path, body=None):
    data = None if body is None else json.dumps(body).encode()
    request = urllib.request.Request(
        DRIVER + path, data=data, method=method, headers={"Content-Type": "application/json"}
    )
    try:
        with urllib.request.urlopen(request, timeout=60) as response:
            return json.load(response)["value"]
    except urllib.error.HTTPError as error:
        raise RuntimeError("%s %s: %s" % (method, path, error.read().decode())) from None


class Browser:
    def __init__(self):
        for _ in range(100):
            try:
                call("GET", "/status")
                break
            except OSError:
                time.sleep(0.1)
        capabilities = {
            "browserName": "firefox",
            "moz:firefoxOptions": {"args": ["-headless"]},
        }
        self.session = call("POST", "/session", {"capabilities": {"alwaysMatch": capabilities}})[
            "sessionId"
        ]

    def path(self, suffix):
        return "/session/%s%s" % (self.session, suffix)

    def go(self, url):
        call("POST", self.path("/url"), {"url": url})

    def script(self, source, *args):
        return call("POST", self.path("/execute/sync"), {"script": source, "args": list(args)})

    def find(self, selector):
        return call("POST", self.path("/element"), {"using": "css selector", "value": selector})[
            ELEMENT
        ]

    def click(self, selector):
        call("POST", self.path("/element/%s/click" % self.find(selector)), {})

    def type(self, selector, text):
        call("POST", self.path("/element/%s/value" % self.find(selector)), {"text": text})

    def text(self, selector="body"):
        return self.script(
            "const e = document.querySelector(arguments[0]); return e ? e.innerText : ''",
            selector,
        )

    def wait(self, what, check, timeout=30):
        deadline = time.time() + timeout
        while time.time() < deadline:
            try:
                if check():
                    return
            except RuntimeError:
                pass
            time.sleep(0.2)
        self.shot("failed-" + what.replace(" ", "-"))
        print("timed out waiting for %s; the page shows:\n%s" % (what, self.text()), file=sys.stderr)
        sys.exit(1)

    def shot(self, name):
        if SCREENSHOTS:
            with open(os.path.join(SCREENSHOTS, name + ".png"), "wb") as file:
                file.write(base64.b64decode(call("GET", self.path("/screenshot"))))

    def resize(self, width, height):
        call("POST", self.path("/window/rect"), {"width": width, "height": height})

    def close(self):
        call("DELETE", self.path(""))


def send(browser, text, keys=ENTER):
    browser.type("#composer", text)
    browser.type("#composer", keys)


def main():
    browser = Browser()
    try:
        browser.resize(1280, 900)
        browser.go(LINK)
        browser.wait("the connection", lambda: browser.text("#connected") != "")
        assert browser.script("return location.hash") == "", "the token stayed in the address bar"
        print("connected:", browser.text("#connected"))

        # A new session from the first message; streamed markdown.
        send(browser, "show me markdown")
        browser.wait("the markdown", lambda: "A heading" in browser.text("#transcript"))
        browser.wait(
            "the highlighted code",
            lambda: browser.script(
                "return document.querySelector('#transcript code.language-rust[data-highlighted]') !== null"
            ),
        )
        assert browser.script("return document.querySelector('#transcript h1') !== null")
        assert browser.script("return document.querySelector('#transcript strong') !== null")
        print("streamed markdown rendered and highlighted")
        browser.shot("markdown")

        # A tool call that asks for approval.
        send(browser, "use the tool")
        browser.wait("the approval", lambda: browser.text("#approvals-badge") != "")
        browser.click("#approvals-badge")
        browser.wait("the approval sheet", lambda: "bash" in browser.text("#sheet").lower() or "printf" in browser.text("#sheet"))
        browser.shot("approval")
        browser.click("#sheet [data-decision=approve_once]")
        browser.wait("the tool's answer", lambda: "The tool ran." in browser.text("#transcript"))
        if "hello from the tool" not in browser.text("#transcript"):
            browser.click("#transcript [data-action=toggle-tool]")
        browser.wait("the tool output", lambda: "hello from the tool" in browser.text("#transcript"))
        print("approved the tool; its terminal output shows")

        # Steer, queue, and cancel a slow run.
        browser.script(
            "const sheet = document.querySelector('#sheet'); if (sheet) sheet.querySelector('button').click()"
        )
        send(browser, "go slow")
        browser.wait("the slow run", lambda: "word3" in browser.text("#transcript"))
        send(browser, "steer this way")
        browser.wait(
            "the steered message",
            lambda: "steer this way" in browser.text("#queue") or "steer this way" in browser.text("#transcript"),
        )
        print("steered the running run")
        send(browser, "queued one", ALT + ENTER + NULL)
        browser.wait("the queued message", lambda: "queued one" in browser.text("#queue"))
        browser.shot("queue")
        print("queued behind the run")
        browser.type("#composer", ESCAPE)
        browser.wait(
            "the cancelled run",
            lambda: "working" not in browser.text("#status-bar")
            and browser.script("return document.querySelector('#composer').value").find("queued one") >= 0,
            timeout=40,
        )
        print("cancelled the run; the queued message came back to the composer")
        browser.shot("cancelled")

        # A phone-sized window: no sideways scroll, the sidebar becomes a sheet.
        browser.resize(375, 800)
        time.sleep(0.5)
        overflow = browser.script(
            "return document.documentElement.scrollWidth - document.documentElement.clientWidth"
        )
        assert overflow <= 0, "the page scrolls sideways by %s px" % overflow
        browser.click("main button")
        browser.wait("the sessions sheet", lambda: browser.text("#sheet") != "")
        browser.shot("narrow-sessions")
        browser.click("#sheet h2 ~ button")
        browser.shot("narrow")
        print("the narrow layout fits and opens the sessions as a sheet")

        # A daemon that allows no origin refuses the page before its handshake.
        browser.resize(1280, 900)
        browser.go(REFUSING_LINK)
        expected = '"http://127.0.0.1:%s"' % REFUSING_PORT
        browser.wait(
            "the origin hint",
            lambda: expected in browser.text() and "allowed_origins" in browser.text(),
        )
        print("the refused page names", expected)
        browser.shot("refused")
    finally:
        browser.close()


main()
