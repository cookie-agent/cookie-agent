"""Photographs the web client for the landing page, driven over WebDriver.

Run by scripts/web-screenshots.sh:

    python3 -I showcase.py <driver-port> <link> <output-dir> [all]

It starts a session, sends one request the scripted model
(showcase-model.py) answers with an edit, a `bash` check, and a markdown
reply, opens both tool calls, and saves `web-light.png` and `web-dark.png`
at 1280×800, and `web-phone-light.png` and `web-phone-dark.png` at 390×844.

With `all`, it also opens every panel, sheet, and menu at both sizes and
saves each in light and dark under `<output-dir>/all/`, then the connect
screen, for reviewing the page's look as a whole.
"""

import base64
import json
import os
import sys
import time
import urllib.error
import urllib.request

DRIVER = "http://127.0.0.1:%s" % sys.argv[1]
LINK, OUT = sys.argv[2:4]
ALL = sys.argv[4:5] == ["all"]
ELEMENT = "element-6066-11e4-a52e-4f735466cecf"
ENTER = "\ue007"  # WebDriver key: Enter
CHROME = 86  # what the headless window keeps above the page
PROMPT = "greet() should say “Hello, Ada!”. Fix it and run the check."


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
        capabilities = {"browserName": "firefox", "moz:firefoxOptions": {"args": ["-headless"]}}
        self.session = call("POST", "/session", {"capabilities": {"alwaysMatch": capabilities}})[
            "sessionId"
        ]

    def path(self, suffix):
        return "/session/%s%s" % (self.session, suffix)

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

    def wait(self, what, check, timeout=60):
        deadline = time.time() + timeout
        while time.time() < deadline:
            try:
                if check():
                    return
            except RuntimeError:
                pass
            time.sleep(0.2)
        sys.exit("timed out waiting for %s; the page shows:\n%s" % (what, self.text()))

    def size(self, width, height):
        """Shows the page at `width`×`height`: Firefox windows are at least
        500 px wide, so a narrower page is a 500 px window zoomed in (needs
        geckodriver --allow-system-access)."""
        zoom = max(1.0, 500 / width)
        call(
            "POST",
            self.path("/window/rect"),
            {"width": max(width, 500), "height": round(height * zoom) + CHROME},
        )
        call("POST", self.path("/moz/context"), {"context": "chrome"})
        try:
            self.script("gBrowser.selectedBrowser.fullZoom = arguments[0]", zoom)
        finally:
            call("POST", self.path("/moz/context"), {"context": "content"})

    def theme(self, name):
        # A script click: on a phone an open sheet covers the header.
        for _ in range(3):
            shown = self.script(
                "const b = document.querySelector('button[aria-label=Theme]');"
                "return b.getAttribute('data-theme') || b.innerText.trim()"
            )
            if shown.endswith(name):
                return
            self.script("document.querySelector('button[aria-label=Theme]').click()")
        sys.exit("the theme button never showed %s" % name)

    def count(self, selector):
        return self.script("return document.querySelectorAll(arguments[0]).length", selector)

    def shot(self, name):
        time.sleep(0.8)
        os.makedirs(os.path.dirname(os.path.join(OUT, name)), exist_ok=True)
        with open(os.path.join(OUT, name), "wb") as file:
            file.write(base64.b64decode(call("GET", self.path("/screenshot"))))
        print("saved", name)


browser = Browser()
browser.size(1280, 800)
call("POST", browser.path("/url"), {"url": LINK})
browser.wait("the connection", lambda: browser.text("#connected") != "")
# Diagnostics at warning and above only, as a user would read the page; the
# page keeps the token it was opened with.
browser.script("localStorage.setItem('cookie.event_level', 'warning')")
call("POST", browser.path("/url"), {"url": LINK})
browser.wait("the connection", lambda: browser.text("#connected") != "")
browser.click("aside [data-new-session]")
browser.wait("the new session", lambda: "New session" in browser.text("#session-title"))
browser.type("#composer", PROMPT)
browser.type("#composer", ENTER)
browser.wait(
    "the answer",
    lambda: "passes" in browser.text("#transcript")
    and "working" not in browser.text("#status-bar"),
)
browser.script(
    "for (const tool of document.querySelectorAll('#transcript [data-action=toggle-tool]'))"
    " tool.click()"
)
browser.wait("the open tool calls", lambda: "ok: greet" in browser.text("#transcript"))
browser.wait("the title", lambda: "Fix the greeting" in browser.text("#session-title"))
browser.script("document.querySelector('#composer').blur()")
for theme in ("light", "dark"):
    browser.theme(theme)
    browser.shot("web-%s.png" % theme)
browser.size(390, 844)
time.sleep(1)
for theme in ("light", "dark"):
    browser.theme(theme)
    browser.script("const t = document.querySelector('#transcript'); t.scrollTop = t.scrollHeight")
    browser.shot("web-phone-%s.png" % theme)

# Every panel, sheet, and menu: a slash command typed in the composer, or a
# click, which works on any version of the page.
COMMANDS = [
    "/sessions", "/agent", "/model", "/preset", "/approvals", "/usage", "/events",
    "/goal", "/background", "/skills", "/permissions", "/runtime", "/mcp", "/connect",
    "/config", "/memory", "/plugins",
]
CLICKS = [
    ("commands", "button[aria-label=Commands]"),
    ("message-menu", "#transcript [data-action=user-menu]"),
    ("send-menu", "button[aria-label='More ways to send']"),
    ("connection", "#connection"),
    ("composer-completions", None),
]


def close_all():
    browser.script(
        "const s = document.querySelector('#sheet h2 ~ button'); if (s) s.click();"
        "const c = document.querySelector('#connection-close'); if (c) c.click();"
        "if (document.querySelector('#queue-send'))"
        "  document.querySelector(\"button[aria-label='More ways to send']\").click();"
    )
    browser.script(
        "const c = document.querySelector('#composer'); c.value = '';"
        "c.dispatchEvent(new Event('input')); c.blur()"
    )
    time.sleep(0.3)


def both_themes(name):
    for theme in ("light", "dark"):
        browser.theme(theme)
        browser.shot("all/%s-%s.png" % (name, theme))


if ALL:
    for size, (width, height) in (("desktop", (1280, 800)), ("phone", (390, 844))):
        browser.size(width, height)
        time.sleep(1)
        close_all()
        both_themes("%s-transcript" % size)
        for command in COMMANDS:
            close_all()
            browser.type("#composer", command)
            browser.type("#composer", ENTER)
            time.sleep(1.2)
            both_themes("%s-%s" % (size, command[1:]))
        close_all()
        browser.type("#composer", "/runtime")
        browser.type("#composer", ENTER)
        time.sleep(1)
        if browser.count("#runtime-tab-machines"):
            browser.script("document.querySelector('#runtime-tab-machines').click()")
            time.sleep(1)
            both_themes("%s-machines" % size)
        for name, selector in CLICKS:
            close_all()
            if selector is None:
                browser.script(
                    "const c = document.querySelector('#composer'); c.value = '/m';"
                    "c.dispatchEvent(new Event('input')); c.focus()"
                )
            elif browser.count(selector):
                browser.script("document.querySelector(arguments[0]).click()", selector)
            else:
                continue
            time.sleep(0.8)
            both_themes("%s-%s" % (size, name))
        close_all()
        # A new session's card.
        if width >= 768:
            browser.script("document.querySelector('aside [data-new-session]').click()")
            time.sleep(1)
            both_themes("%s-new-session" % size)
            browser.script("document.querySelector('aside nav[aria-label=Sessions] button').click()")
            time.sleep(1)
    # The connect screen, once the page holds no token.
    browser.size(1280, 800)
    browser.script("localStorage.clear()")
    call("POST", browser.path("/url"), {"url": LINK.split("#")[0]})
    time.sleep(2)
    both_themes("desktop-connect")
    browser.size(390, 844)
    time.sleep(1)
    both_themes("phone-connect")
call("DELETE", browser.path(""))
