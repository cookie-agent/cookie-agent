"""Browser scenarios for the web client, driven over WebDriver (chromedriver,
headless Chrome).

Run by scripts/test-web-page.sh:

    python3 -I scenarios.py <driver-port> <allowing-link> <other-link> \
        <other-port> <allowing-home> <held-session> <screenshot-dir|""> \
        [scenario ...]

runs the scenarios named (all, in order, when none is) in one browser, and

    python3 -I scenarios.py --plan [scenario ...]

prints the groups of scenarios to run for those named, one group per line,
each in its own process against its own daemons (see SCENARIOS).

The allowing daemon has a scripted model (mock-model.py) that takes images,
an agent whose `bash` asks for approval, a `helper` subagent, and a `greet`
skill. `held-session` is a session another process made before the daemon
started, which the daemon lists but has not loaded. The other daemon is
empty; its page is of another origin. In the environment, `COOKIE` is the
`cookie` binary, for the machine commands the badges need, and `CHROME`
and `CHROME_ARGS` (a JSON list) the browser and its switches.
"""

import atexit
import base64
import fcntl
import functools
import hashlib
import http.server
import json
import os
import random
import re
import shutil
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
import zlib

PLAN = sys.argv[1:2] == ["--plan"]
if not PLAN:
    DRIVER = "http://127.0.0.1:%s" % sys.argv[1]
    LINK, OTHER_LINK, OTHER_PORT, HOME, HELD_SESSION, SCREENSHOTS = sys.argv[2:8]
else:
    DRIVER = LINK = OTHER_LINK = OTHER_PORT = HOME = HELD_SESSION = SCREENSHOTS = ""
# The other daemon's page, without a token.
OTHER_PAGE = OTHER_LINK.split("#")[0]
# A daemon URL nothing listens on (port 9, discard): the connection fails
# before its handshake.
UNREACHABLE = "ws://127.0.0.1:9/ws"
# Points the page at daemon URL `arguments[0]` with token `arguments[1]`, as
# a past connection there would have.
REMEMBER_DAEMON = (
    "localStorage.setItem('cookie.daemon_url', arguments[0]);"
    "localStorage.setItem('cookie.token:' + arguments[0], arguments[1])"
)
COOKIE = os.environ.get("COOKIE", "cookie")
# The sessions runtime_cards makes, by what they attach, for the badges.
MADE = {}
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


# What in a browser's log is an uncaught error or a panic: a Rust panic is
# a WebAssembly trap Chrome logs as "Uncaught RuntimeError: unreachable",
# after the page's panic hook logged "panicked at" with its message; an
# unhandled rejection is "Uncaught (in promise)".
PAGE_ERROR = re.compile(r"Uncaught|panicked at|RuntimeError")
# The scenario running, which a browser's errors are reported under.
SCENARIO = ["connect"]


# What a desktop with a mouse reports, which headless Chrome does not:
# Tailwind's hover: styles apply only under (hover: hover).
DESKTOP_POINTER = (
    "--blink-settings=primaryHoverType=2,availableHoverTypes=2,primaryPointerType=4,availablePointerTypes=4"
)


class Browser:
    def __init__(self, user_agent=None, touch=False, switches=()):
        """A new headless Chrome; `touch` makes it a touch screen, whose
        pointer is coarse and cannot hover. `switches` are more of
        Chrome's command-line switches."""
        for _ in range(100):
            try:
                call("GET", "/status")
                break
            except OSError:
                time.sleep(0.1)
        args = json.loads(os.environ.get("CHROME_ARGS") or '["--headless=new"]')
        args += ["--window-size=1280,900", "--hide-scrollbars", DESKTOP_POINTER]
        if user_agent:
            args.append("--user-agent=" + user_agent)
        args += list(switches)
        options = {"args": args}
        if os.environ.get("CHROME"):
            options["binary"] = os.environ["CHROME"]
        capabilities = {
            "browserName": "chrome",
            "goog:chromeOptions": options,
            # The console and uncaught errors, for check().
            "goog:loggingPrefs": {"browser": "ALL"},
        }
        self.session = call("POST", "/session", {"capabilities": {"alwaysMatch": capabilities}})[
            "sessionId"
        ]
        if touch:
            self.cdp("Emulation.setTouchEmulationEnabled", {"enabled": True, "maxTouchPoints": 5})
        # Chrome's own install prompt comes when it pleases and moves the
        # page; the install scenarios hand the page theirs (FAKE_PROMPT).
        self.cdp(
            "Page.addScriptToEvaluateOnNewDocument",
            {
                "source": "addEventListener('beforeinstallprompt',"
                " (e) => { if (e.isTrusted) e.stopImmediatePropagation(); }, true)"
            },
        )

    def errors(self):
        """The uncaught errors and panics the browser logged since the last
        call, which takes them from its log."""
        entries = call("POST", self.path("/se/log"), {"type": "browser"})
        return [
            entry["message"]
            for entry in entries
            if entry.get("level") == "SEVERE" and PAGE_ERROR.search(entry["message"])
        ]

    def check(self):
        """Fails when a page of this browser had an uncaught error or a
        panic since the last check, naming the scenario that ran."""
        errors = self.errors()
        if errors:
            self.shot("errors-" + SCENARIO[0])
            raise AssertionError(self.failure(errors))

    @staticmethod
    def failure(errors):
        return "the page had %d uncaught error(s) or panic(s) during %s:\n%s" % (
            len(errors),
            SCENARIO[0],
            "\n".join(errors),
        )

    def cdp(self, command, params):
        """Runs a Chrome DevTools Protocol command in the page."""
        return call("POST", self.path("/goog/cdp/execute"), {"cmd": command, "params": params})

    def path(self, suffix):
        return "/session/%s%s" % (self.session, suffix)

    def go(self, url):
        call("POST", self.path("/url"), {"url": url})

    def script(self, source, *args):
        return call("POST", self.path("/execute/sync"), {"script": source, "args": list(args)})

    def script_async(self, source, *args):
        """Runs `source`, which calls its last argument when it is done."""
        return call("POST", self.path("/execute/async"), {"script": source, "args": list(args)})

    def find(self, selector):
        return call("POST", self.path("/element"), {"using": "css selector", "value": selector})[
            ELEMENT
        ]

    def on_element(self, selector, action, body):
        """Clicks or types into `selector`, for up to 2 s while the page is
        not ready for it: the page can re-render the element between
        finding it and acting on it (the selector then names its
        replacement), a sheet can still be sliding in, and Chrome scrolls a
        target only just into view, where a sticky header can cover it."""
        for attempt in range(10):
            try:
                return call("POST", self.path("/element/%s/%s" % (self.find(selector), action)), body)
            except RuntimeError as error:
                retried = ("stale element reference", "element not interactable", "element click intercepted")
                if attempt == 9 or not any(reason in str(error) for reason in retried):
                    raise
                if "element click intercepted" in str(error):
                    self.script(
                        "document.querySelector(arguments[0]).scrollIntoView({block: 'center'})", selector
                    )
                time.sleep(0.2)

    def click(self, selector):
        self.on_element(selector, "click", {})

    def type(self, selector, text):
        self.on_element(selector, "value", {"text": text})

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
        self.cdp("Emulation.clearDeviceMetricsOverride", {})
        call("POST", self.path("/window/rect"), {"width": width, "height": height})

    def phone(self, width, height):
        """Shows the page in a `width`×`height` viewport, as on a phone, by
        Chrome's device emulation (resize ends it). It stays a desktop
        browser's page, as the layout does not ask what the device is."""
        self.cdp(
            "Emulation.setDeviceMetricsOverride",
            {"width": width, "height": height, "deviceScaleFactor": 0, "mobile": False},
        )
        size = self.script("return [innerWidth, innerHeight]")
        assert abs(size[0] - width) <= 3, "the page is %s px wide, not %s" % (size[0], width)

    def close(self):
        """Closes the browser, failing if it logged an uncaught error or a
        panic; when the scenario already fails, it only prints them."""
        failing = sys.exc_info()[1] is not None
        try:
            errors = self.errors()
        finally:
            call("DELETE", self.path(""))
        if errors and failing:
            print(self.failure(errors), file=sys.stderr)
        elif errors:
            raise AssertionError(self.failure(errors))


def send(browser, text, keys=ENTER):
    # A new session with only `global` gets `local` at the daemon's
    # workspace first, where the scripted agent works.
    if count(browser, "#new-session") == 1:
        show_cards(browser)
        if selected_cards(browser) == ["global/global"]:
            select_local(browser)
    browser.type("#composer", text)
    browser.type("#composer", keys)


def png(path, width, height, noisy=False):
    """Writes a `width`×`height` RGB PNG; a noisy one does not compress."""
    rows = bytearray()
    for _ in range(height):
        rows.append(0)
        if noisy:
            rows.extend(random.randbytes(width * 3))
        else:
            rows.extend(b"\x30\x90\xc0" * width)

    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))

    data = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
    data += chunk(b"IDAT", zlib.compress(bytes(rows))) + chunk(b"IEND", b"")
    with open(path, "wb") as file:
        file.write(data)
    return path


def count(browser, selector):
    return browser.script("return document.querySelectorAll(arguments[0]).length", selector)


# The daemon's own workspace, where the scripted agent reads dot.png.
WORKSPACE = os.path.join(HOME, "workspace")


def card(key):
    return '#new-session [data-card="%s"]' % key


def field(key, index=0):
    return card(key) + ' [data-workspace-field="%s"]' % index


def show_cards(browser):
    """Waits for the new session's runtime cards, above its composer."""
    # A folded machine group shows no cards: its header is enough.
    browser.wait(
        "the runtime cards",
        lambda: count(browser, "#new-session [data-group]") > 0 and count(browser, "#composer") == 1,
    )


def select_local(browser, workspace=None):
    """Selects `local` on the new session's cards, at `workspace` (the
    daemon's own workspace) when its field is empty: the first message
    creates the session there."""
    show_cards(browser)
    browser.wait("the runtime cards", lambda: count(browser, card("local/coding")) == 1)
    if browser.script("return document.querySelector(arguments[0]).dataset.selected", card("local/coding")) != "true":
        browser.click(card("local/coding") + " [data-card-toggle]")
    browser.wait("the workspace field", lambda: count(browser, field("local/coding")) == 1)
    if browser.script("return document.querySelector(arguments[0]).value", field("local/coding")) == "" or workspace:
        set_text(browser, field("local/coding"), workspace or WORKSPACE)


def new_session(browser):
    browser.click("aside [data-new-session]")
    browser.wait("the new session", lambda: "New session" in browser.text("#session-title"))
    select_local(browser)


def answers(browser):
    return len(re.findall(r"\bok\b", browser.text("#transcript")))


def clear_composer(browser):
    browser.script(
        "const c = document.querySelector('#composer'); c.value = '';"
        "c.dispatchEvent(new Event('input'))"
    )


# Whether the nested subagents follow the open session's row.
NESTED_UNDER_OPEN_ROW = """
const group = document.querySelector('aside [data-subagents]');
let e = group && group.previousElementSibling;
while (e && !e.matches('[data-session-row]')) e = e.previousElementSibling;
return !!e && e.querySelector('[aria-current=true]') !== null;
"""


def composer_height(browser):
    return browser.script("return document.querySelector('#composer').getBoundingClientRect().height")


def buttons(browser):
    """The composer's buttons, by id, and whether each is enabled."""
    return browser.script(
        "return [...document.querySelectorAll('[data-composer-actions] button')]"
        ".map((b) => [b.id, !b.disabled])"
    )


def composer(browser):
    """The message box: one border, one line that grows to five and
    shrinks back, and its buttons by the session's state."""
    new_session(browser)
    # One border: the box's. The text field and what holds the box draw none.
    borders = browser.script(
        "const t = getComputedStyle(document.querySelector('#composer'));"
        "const b = getComputedStyle(document.querySelector('#composer-box'));"
        "const outer = getComputedStyle(document.querySelector('#composer-box').parentElement.parentElement);"
        "const shadowless = (style) => style.boxShadow.split(/,(?![^(]*\\))/)"
        "  .every((shadow) => shadow.trim() === 'none' || shadow.includes('rgba(0, 0, 0, 0)'));"
        "return [t.borderTopWidth, t.borderLeftWidth, t.outlineStyle, shadowless(t) ? 'none' : t.boxShadow,"
        "        b.borderTopWidth, b.borderLeftWidth, outer.borderTopWidth]"
    )
    assert borders == ["0px", "0px", "none", "none", "1px", "1px", "0px"], borders
    print("the message box has one border")
    # One line to start; five at most, then it scrolls; back to one.
    one = composer_height(browser)
    assert 40 <= one <= 48, "the empty box is %s px tall" % one
    set_text(browser, "#composer", "a\nb\nc")
    time.sleep(0.3)
    three = composer_height(browser)
    assert abs(three - (one + 40)) <= 2, (one, three)
    set_text(browser, "#composer", "\n".join("line %d" % n for n in range(1, 10)))
    time.sleep(0.3)
    five = composer_height(browser)
    assert abs(five - (one + 80)) <= 2, (one, five)
    scrolls = browser.script(
        "const c = document.querySelector('#composer');"
        "return [getComputedStyle(c).overflowY, c.scrollHeight > c.clientHeight]"
    )
    assert scrolls == ["auto", True], scrolls
    browser.shot("composer-five-lines")
    # Text never runs under the buttons.
    clear = browser.script(
        "const c = document.querySelector('#composer');"
        "const a = document.querySelector('[data-composer-actions]').getBoundingClientRect();"
        "return c.getBoundingClientRect().right - parseFloat(getComputedStyle(c).paddingRight) <= a.left"
    )
    assert clear, "the text runs under the buttons"
    print("the box grows from one line to five, then scrolls")
    # The state table: idle and empty, idle with text.
    clear_composer(browser)
    time.sleep(0.3)
    assert buttons(browser) == [["send", False]], buttons(browser)
    browser.type("#composer", "go slow")
    assert buttons(browser) == [["send", True]], buttons(browser)
    browser.click("#send")
    browser.wait("the slow run", lambda: "word3" in browser.text("#transcript"))
    assert abs(composer_height(browser) - one) <= 1, "the box kept its height after sending"
    # Running and empty: Stop; with text: Queue and Steer.
    browser.wait("stop", lambda: buttons(browser) == [["stop", True]])
    browser.shot("composer-running-empty")
    browser.type("#composer", "steer this way")
    assert buttons(browser) == [["queue-send", True], ["steer", True]], buttons(browser)
    browser.shot("composer-running-text")
    browser.click("#steer")
    browser.wait(
        "the steered message",
        lambda: "steer this way" in browser.text("#queue") or "steer this way" in browser.text("#transcript"),
    )
    browser.type("#composer", "queued by the button")
    browser.click("#queue-send")
    browser.wait("the queued message", lambda: "queued by the button" in browser.text("#queue"))
    print("Steer and Queue send while the run goes")
    browser.click("#stop")
    browser.wait(
        "the stopped run",
        lambda: "working" not in browser.text("#status-bar")
        and "queued by the button" in browser.script("return document.querySelector('#composer').value"),
        timeout=40,
    )
    browser.wait("send again", lambda: buttons(browser) == [["send", True]])
    clear_composer(browser)
    print("the composer's buttons follow the run: Send, Stop, Queue and Steer")


# How far the newest user bubble's right edge is from the right edge of the
# transcript's content column, and how wide it is against that column.
BUBBLE = """
const bubbles = document.querySelectorAll('#transcript .user-message');
if (!bubbles.length) return null;
const bubble = bubbles[bubbles.length - 1].getBoundingClientRect();
const column = bubbles[bubbles.length - 1].closest('.transcript-item').parentElement.getBoundingClientRect();
return [Math.round(column.right - bubble.right), bubble.width / column.width];
"""


def bubble_aligned(browser):
    """Whether the newest user bubble ends at the content column's right
    edge, no wider than about 80 % of it."""
    placed = browser.script(BUBBLE)
    return placed is None or (abs(placed[0]) <= 1 and placed[1] <= 0.81)


def expanded(browser, row):
    return browser.script(
        "const r = document.querySelector(arguments[0]); return r ? r.dataset.expanded : null", row
    )


def tool_rows(browser):
    """Tool calls start collapsed and open on a click, kept per session; a
    running call shows its progress collapsed; long output stops at the
    row budget."""
    new_session(browser)
    send(browser, "print many lines")
    browser.wait("the approval", lambda: count(browser, "#transcript [data-action=approvals]") == 1)
    browser.click("#transcript [data-action=approvals]")
    browser.wait("the approval sheet", lambda: count(browser, "#sheet [data-decision=approve_once]") > 0)
    browser.click("#sheet [data-decision=approve_once]")
    close_sheet(browser)
    browser.wait("the tool's answer", lambda: "The tool ran." in browser.text("#transcript"), timeout=40)
    assert bubble_aligned(browser), "the user's bubble is not at the right: %s" % browser.script(BUBBLE)
    row = "#transcript [data-tool]"
    assert expanded(browser, row) == "false", "the row starts open"
    assert count(browser, row + " [data-tool-body]") == 0
    browser.click(row + " [data-action=toggle-tool]")
    browser.wait("the open row", lambda: expanded(browser, row) == "true")
    browser.wait("the budget", lambda: "36 more lines (click to expand)" in browser.text(row))
    assert "\n70\n" not in browser.text(row), "output past the budget shows"
    browser.shot("tool-budget")
    browser.click(row + " [data-more-lines]")
    browser.wait("every line", lambda: "100" in browser.text(row) and "click to collapse" in browser.text(row))
    print("a call starts collapsed; its output stops at the budget until clicked")
    # Kept for this session across a switch.
    title = browser.text("#session-title")
    click_row(browser, "aside nav[aria-label=Sessions] [data-session]:not([aria-current])")
    browser.wait("another session", lambda: browser.text("#session-title") != title)
    browser.script(
        "[...document.querySelectorAll('aside nav[aria-label=Sessions] [data-session]')]"
        ".find((b) => b.innerText.includes(arguments[0])).click()",
        title,
    )
    browser.wait("the session again", lambda: browser.text("#session-title") == title)
    browser.wait("the kept open row", lambda: expanded(browser, row) == "true")
    browser.click(row + " [data-action=toggle-tool]")
    browser.wait("the closed row", lambda: expanded(browser, row) == "false")
    print("a row's state is kept per session and it closes again")
    # A running call shows its progress while collapsed.
    send(browser, "be chatty")
    browser.wait("the approval", lambda: browser.text("#approvals-badge") != "")
    browser.click("#approvals-badge")
    browser.wait("the approval sheet", lambda: count(browser, "#sheet [data-decision=approve_once]") > 0)
    browser.click("#sheet [data-decision=approve_once]")
    close_sheet(browser)
    browser.wait(
        "the progress",
        lambda: browser.script(
            "const rows = document.querySelectorAll('#transcript [data-tool]');"
            "const r = rows[rows.length - 1]; const p = r.querySelector('[data-progress]');"
            "return r.dataset.expanded === 'false' && !!p && p.innerText.includes('bash')"
        ),
        timeout=20,
    )
    browser.shot("tool-progress")
    print("a running call shows its progress collapsed:", browser.script(
        "const p = document.querySelectorAll('#transcript [data-progress]'); return p[p.length - 1].innerText"))
    browser.wait("the chatty run's end", lambda: "working" not in browser.text("#status-bar"), timeout=40)
    # A long message of several lines: its bubble still ends at the right.
    long = "\n".join(["A long message about the greeting, " * 4] * 3)
    set_text(browser, "#composer", long)
    browser.type("#composer", ENTER)
    browser.wait("the long message", lambda: "about the greeting" in browser.text("#transcript"))
    assert bubble_aligned(browser), "a long bubble is not at the right: %s" % browser.script(BUBBLE)
    print("user bubbles end at the content column's right edge:", browser.script(BUBBLE))
    browser.wait("its answer", lambda: "working" not in browser.text("#status-bar"), timeout=40)


# The transcript's scroll position: how far its end is below the box,
# its scroll top and content height, and whether the jump-to-latest
# button shows.
SCROLL_STATE = """
const t = document.querySelector('#transcript');
return {gap: t.scrollHeight - t.scrollTop - t.clientHeight, top: t.scrollTop,
    height: t.scrollHeight, jump: !!document.querySelector('[data-jump-latest]')};
"""

# Counts the page's writes to the transcript's scroll position from now on
# (window.__pins), as a pin makes them.
COUNT_PINS = """
const t = document.querySelector('#transcript');
const property = Object.getOwnPropertyDescriptor(Element.prototype, 'scrollTop');
window.__pins = 0;
Object.defineProperty(t, 'scrollTop', {
    configurable: true,
    get() { return property.get.call(this); },
    set(value) { window.__pins += 1; property.set.call(this, value); },
});
"""

# Presses the toggle of tool row `arguments[0]` with real mouse events held
# for `arguments[1]` ms (in which a stream redraws the page many times), and
# reports the ms from the release until the row shows itself toggled, or
# null when it never does within a second.
TIMED_TOGGLE = """
const [call, hold, done] = arguments;
const row = () => document.querySelector('#transcript [data-tool="' + call + '"]');
const before = row().dataset.expanded;
const box = row().querySelector('[data-action=toggle-tool]').getBoundingClientRect();
window.__toggle = {x: box.left + 24, y: box.top + box.height / 2, before};
done(window.__toggle);
"""

TOGGLE_LATENCY = """
const [call, done] = arguments;
const row = () => document.querySelector('#transcript [data-tool="' + call + '"]');
const released = window.__released;
const check = () => {
    if (row().dataset.expanded !== window.__toggle.before) {
        done(performance.now() - released);
    } else if (performance.now() - released > 1000) {
        done(null);
    } else {
        requestAnimationFrame(check);
    }
};
check();
"""


def mouse(browser, kind, x, y, **extra):
    browser.cdp(
        "Input.dispatchMouseEvent",
        dict({"type": kind, "x": x, "y": y, "button": "left", "clickCount": 1}, **extra),
    )


def wheel(browser, delta):
    """A mouse wheel turned over the transcript's middle: up when `delta`
    is negative."""
    box = browser.script(
        "const b = document.querySelector('#transcript').getBoundingClientRect();"
        "return [b.left + b.width / 2, b.top + b.height / 2]"
    )
    browser.cdp(
        "Input.dispatchMouseEvent",
        {"type": "mouseWheel", "x": box[0], "y": box[1], "deltaX": 0, "deltaY": delta},
    )


def stays_put(browser, what, seconds=1.5):
    """Asserts the transcript keeps its scroll position for `seconds` while
    the reply grows below it, and offers the jump to the latest."""
    time.sleep(0.4)
    before = browser.script(SCROLL_STATE)
    time.sleep(seconds)
    after = browser.script(SCROLL_STATE)
    assert after["height"] > before["height"], "%s: the reply stopped growing: %s" % (what, after)
    assert abs(after["top"] - before["top"]) <= 1, "%s: the view moved: %s -> %s" % (what, before, after)
    assert after["jump"], "%s: no jump to the latest: %s" % (what, after)


def follows(browser, what, seconds=1.0):
    """Asserts the transcript follows its growing end for `seconds`."""
    before = browser.script(SCROLL_STATE)
    time.sleep(seconds)
    after = browser.script(SCROLL_STATE)
    assert after["height"] > before["height"], "%s: the reply stopped growing: %s" % (what, after)
    assert after["gap"] <= 2 and not after["jump"], "%s: not at the end: %s" % (what, after)


def idle(browser, what, text):
    """Waits until the open session's run is over: `text` shows, the
    status line is not working, and its row in the list says so too, so
    what follows starts from an idle session."""
    row = "aside [data-session-row]:has([aria-current=true])"
    browser.wait(
        what,
        lambda: text in browser.text("#transcript")
        and "working" not in browser.text("#status-bar")
        and count(browser, row) == 1
        and "running" not in browser.text(row),
        timeout=90,
    )


def streaming_scroll(browser):
    """While a reply streams, the transcript follows its end only when the
    content grows, any scroll up stops that at once until the user comes
    back to the end or jumps to the latest, and a row opened or closed in
    the streaming reply responds within a frame or two."""
    new_session(browser)
    send(browser, "stream only, label alpha")
    browser.wait("the long reply", lambda: "alpha 60 " in browser.text("#transcript"), timeout=30)
    follows(browser, "the stream")
    # A wheel up stops following at once, whatever streams in below.
    wheel(browser, -100)
    stays_put(browser, "a wheel up")
    # Back at the end, following resumes.
    for _ in range(5):
        wheel(browser, 4000)
        time.sleep(0.2)
    browser.wait("the end", lambda: browser.script(SCROLL_STATE)["gap"] <= 2)
    follows(browser, "back at the end")
    # A small scroll up, as the scrollbar or a flick's momentum makes it.
    browser.script("document.querySelector('#transcript').scrollTop -= 30")
    stays_put(browser, "30 px up")
    browser.click("[data-jump-latest]")
    follows(browser, "the jump to the latest")
    print("a scroll up during a stream stays put; the end or the jump follows again")
    # A message sent while scrolled up jumps to the end and follows again.
    wheel(browser, -400)
    stays_put(browser, "a wheel up before sending")
    send(browser, "noted, thanks")
    follows(browser, "the message sent")
    print("a message sent while scrolled up follows the end again")
    idle(browser, "the long reply's end", "alpha 599 ")

    # A folded call at the end gets progress: nothing grows, nothing pins.
    send(browser, "stream long, label beta")
    browser.wait("the approval", lambda: browser.text("#approvals-badge") != "")
    browser.click("#approvals-badge")
    browser.wait("the approval sheet", lambda: count(browser, "#sheet [data-decision=approve_once]") > 0)
    browser.click("#sheet [data-decision=approve_once]")
    close_sheet(browser)
    last_tool = "const rows = document.querySelectorAll('#transcript [data-tool]'); const r = rows[rows.length - 1];"
    browser.wait(
        "the progress",
        lambda: browser.script(last_tool + "return r.dataset.expanded === 'false' && !!r.querySelector('[data-progress]')"),
        timeout=20,
    )
    time.sleep(0.3)
    browser.script(COUNT_PINS)
    time.sleep(1)
    pins = browser.script("return window.__pins")
    still_running = browser.script(last_tool + "return !!r.querySelector('[data-progress]')")
    assert still_running, "the call ended before its progress was watched"
    assert pins == 0, "progress in a folded call pinned the view %d times" % pins
    print("progress in a folded call at the end pins nothing")
    # A row of the streaming reply opens and closes promptly.
    browser.wait("the answer after the call", lambda: "beta 30 " in browser.text("#transcript"), timeout=30)
    call = browser.script(last_tool + "r.scrollIntoView({block: 'center'}); return r.dataset.tool")
    latencies = []
    for _ in range(4):
        spot = browser.script_async(TIMED_TOGGLE, call, 150)
        top = browser.script("return document.querySelector('#transcript [data-tool=\"' + arguments[0] + '\"]').getBoundingClientRect().top", call)
        mouse(browser, "mousePressed", spot["x"], spot["y"])
        time.sleep(0.15)
        browser.script("window.__released = performance.now()")
        mouse(browser, "mouseReleased", spot["x"], spot["y"])
        latency = browser.script_async(TOGGLE_LATENCY, call)
        assert latency is not None, "a click on the streaming reply's call did nothing"
        latencies.append(latency)
        moved = browser.script("return document.querySelector('#transcript [data-tool=\"' + arguments[0] + '\"]').getBoundingClientRect().top", call)
        assert abs(moved - top) <= 1, "the toggled row moved from %s to %s" % (top, moved)
        time.sleep(0.2)
    assert "beta 500 " not in browser.text("#transcript"), "the reply ended before the toggles"
    print("toggles during the stream applied in %s ms" % ", ".join("%.0f" % ms for ms in latencies))
    assert max(latencies) < 250, "a toggle took %.0f ms" % max(latencies)
    browser.click("[data-jump-latest]")
    follows(browser, "the jump after the toggles")
    idle(browser, "the reply's end", "beta 599 ")

    # A phone: a finger dragging the page down stops following.
    session = browser.script(
        "return document.querySelector('aside [data-session-row] [aria-current=true]').dataset.session"
    )
    phone = Browser(touch=True)
    try:
        phone.go(LINK)
        phone.wait("the connection", lambda: phone.text("#connected") != "")
        phone.phone(390, 844)
        phone.click("main [data-sidebar-toggle]")
        phone.wait("the drawer", lambda: count(phone, '#drawer [data-session="%s"]' % session) > 0)
        phone.script("document.querySelector(arguments[0]).click()", '#drawer [data-session="%s"]' % session)
        phone.wait("the session", lambda: "beta 599 " in phone.text("#transcript"), timeout=30)
        send(browser, "stream only, label gamma")
        phone.wait("the long reply", lambda: "gamma 60 " in phone.text("#transcript"), timeout=30)
        follows(phone, "the phone's stream")
        origin = {ELEMENT: phone.find("#transcript")}
        perform(phone, [{
            "type": "pointer", "id": "finger", "parameters": {"pointerType": "touch"}, "actions": [
                {"type": "pointerMove", "duration": 0, "origin": origin, "x": 0, "y": -100},
                {"type": "pointerDown", "button": 0},
                {"type": "pointerMove", "duration": 300, "origin": "pointer", "x": 0, "y": 200},
                {"type": "pointerUp", "button": 0},
            ],
        }])
        stays_put(phone, "a finger dragging down")
        phone.click("[data-jump-latest]")
        follows(phone, "the phone's jump to the latest")
        print("phone: a finger dragging the page down stays put; the jump follows again")
    finally:
        phone.close()
    idle(browser, "the last reply's end", "gamma 599 ")


def pinned_rows(browser):
    """The system prompt and the tools the latest request offered, pinned
    folded at the top of the transcript as in the TUI; each opens to the
    prompt, whitespace kept, and the tool names as chips."""
    prompt = "#transcript [data-pinned=system-prompt]"
    tools = "#transcript [data-pinned=model-tools]"
    browser.wait("the pinned rows", lambda: count(browser, prompt) == 1 and count(browser, tools) == 1)
    assert browser.script(
        "return document.querySelector('#transcript .transcript-item').querySelector('[data-pinned=system-prompt]') !== null"
    ), "the system prompt is not the transcript's first row"
    title = browser.text(prompt)
    assert re.search(r"system prompt · primary \(last run\) \(\d+ lines\)", title), title
    assert re.fullmatch(r"tools · \d+ available \(last request\)", browser.text(tools).strip()), browser.text(tools)
    assert count(browser, "[data-system-prompt], [data-model-tools]") == 0, "a pinned row starts open"
    # Folded, they share one compact line.
    tops, height = browser.script(
        "const b = [...document.querySelectorAll('#transcript [data-pinned] > button')];"
        "return [b.map((e) => Math.round(e.getBoundingClientRect().top)),"
        "  document.querySelector('#transcript [data-pinned-rows]').getBoundingClientRect().height]"
    )
    assert len(set(tops)) == 1 and height <= 32, (tops, height)
    browser.click(prompt + " button")
    browser.wait("the system prompt", lambda: count(browser, "[data-system-prompt]") == 1)
    body = browser.script("return document.querySelector('[data-system-prompt]').textContent")
    assert "Help." in body, body
    style = browser.script(
        "const s = getComputedStyle(document.querySelector('[data-system-prompt]'));"
        "return [s.whiteSpace, s.fontFamily]"
    )
    assert style[0] == "pre-wrap" and "Mono" in style[1], style
    browser.click(tools + " button")
    browser.wait("the tools", lambda: count(browser, "[data-model-tools] li") > 0)
    names = browser.script("return [...document.querySelectorAll('[data-model-tools] li')].map((l) => l.textContent)")
    offered = int(re.search(r"(\d+) available", browser.text(tools)).group(1))
    assert len(names) == offered and "bash" in names, names
    browser.shot("pinned-rows")
    print("the pinned system prompt and tools rows open:", offered, "tools")
    browser.click(prompt + " button")
    browser.click(tools + " button")
    browser.wait("the folded rows", lambda: count(browser, "[data-system-prompt], [data-model-tools]") == 0)


def session_depth(browser):
    """Subagents, history edits, goals, background calls, media, skills, and
    permission overrides."""
    # A delegation whose subagent answers slowly. Collapsed (the default),
    # the open session's row shows the subagent while it runs, and its
    # delegate call says what it does.
    new_session(browser)
    send(browser, "delegate slowly")
    nested = "aside [data-subagents] [data-tree-session]"
    browser.wait("the running subagent under its session", lambda: count(browser, nested) == 1, timeout=60)
    assert tree_state(browser) == "collapsed", tree_state(browser)
    assert browser.script(NESTED_UNDER_OPEN_ROW), "the subagents are not under the open session's row"
    assert count(browser, "aside [data-subagents] [data-delete]") == 0, "a subagent offers deleting"
    assert "helper" in browser.text("aside [data-subagents]"), browser.text("aside [data-subagents]")
    browser.wait(
        "the subagent's activity",
        lambda: browser.text("#transcript [data-delegate-activity]") != "",
        timeout=30,
    )
    print("a running subagent shows under its session:", browser.text("#transcript [data-delegate-activity]"))
    browser.shot("subagent-live")
    browser.wait("the delegation", lambda: "The tool ran." in browser.text("#transcript"), timeout=90)
    # Nothing runs: the collapsed tree shows nothing.
    browser.wait("the empty collapsed tree", lambda: count(browser, nested) == 0)
    print("collapsed, the tree shows only running subagents")
    # A click on the open session's row expands it: the whole hierarchy.
    click_row(browser, open_row(browser))
    browser.wait("the expanded tree", lambda: tree_state(browser) == "expanded" and count(browser, nested) == 1)
    assert browser.script("return document.querySelector(arguments[0]).getAttribute('aria-expanded')", open_row(browser)) == "true"
    # A finished subagent shows no status mark, as a finished session does.
    assert count(browser, nested + " svg") == 0, "a finished subagent shows a status mark"
    browser.shot("subagent-tree")
    root_title = browser.text("#session-title")
    root = browser.script("return document.querySelector(arguments[0]).dataset.session", open_row(browser))
    # A subagent opens from the nested tree; the breadcrumb leads back.
    browser.click(nested)
    browser.wait("the child session", lambda: "child task" in browser.text("#transcript"))
    assert "helper" in browser.text("#session-title"), browser.text("#session-title")
    browser.wait("the breadcrumb", lambda: shown(browser, "main [data-crumb]"))
    assert root_title in browser.text("main nav[aria-label=Path]"), browser.text("main nav[aria-label=Path]")
    # With the subagent open, the root's row keeps its chevron, which folds
    # the tree on its own; rows without subagents never show one.
    chevron = 'aside [data-session="%s"] [data-tree-chevron]' % root
    assert count(browser, chevron) == 1, "the root lost its chevron while a subagent is open"
    assert count(browser, "aside [data-session] [data-tree-chevron]") == 1, "a session without subagents shows a chevron"
    browser.click(chevron)
    browser.wait("the folded tree", lambda: tree_state(browser) == "collapsed")
    browser.click(chevron)
    browser.wait("the unfolded tree", lambda: tree_state(browser) == "expanded")
    assert "helper" in browser.text("#session-title"), "the chevron left the open subagent"
    browser.shot("subagent")
    browser.click("main [data-crumb]")
    browser.wait("back at the root", lambda: browser.text("#session-title") == root_title)
    print("a subagent opens from the nested tree; the breadcrumb leads back")
    # Its delegate call's session reference opens it too.
    browser.click("#transcript [data-action=toggle-tool]")
    browser.wait(
        "the session reference",
        lambda: count(browser, "#transcript [data-action=open-session]") > 0,
    )
    browser.click("#transcript [data-action=open-session]")
    browser.wait("the child session", lambda: "child task" in browser.text("#transcript"))
    browser.click("main [data-crumb]")
    browser.wait("back at the root", lambda: browser.text("#session-title") == root_title)
    # The browser remembers the expanded tree, and the click that collapses it.
    def reopen():
        browser.go(LINK.split("#")[0])
        browser.wait("the connection", lambda: browser.text("#connected") != "")
        browser.wait("the session row", lambda: count(browser, 'aside [data-session="%s"]' % root) == 1)
        if count(browser, 'aside [data-session="%s"][aria-current=true]' % root) == 0:
            click_row(browser, 'aside [data-session="%s"]' % root)
        browser.wait("the session again", lambda: browser.text("#session-title") == root_title)
    reopen()
    browser.wait("the remembered expanded tree", lambda: tree_state(browser) == "expanded" and count(browser, nested) == 1)
    click_row(browser, open_row(browser))
    browser.wait("the collapsed tree", lambda: tree_state(browser) == "collapsed" and count(browser, nested) == 0)
    reopen()
    browser.wait("the remembered collapsed tree", lambda: tree_state(browser) == "collapsed" and count(browser, nested) == 0)
    print("a click on the open session's row expands or collapses its tree; the browser remembers it")

    # A subagent that runs under one that finished: collapsed, the finished
    # one shows dimmed to place the running one; other sessions show no tree.
    new_session(browser)
    send(browser, "delegate deeply")
    browser.wait(
        "the running grandchild under its dimmed parent",
        lambda: count(browser, nested) == 2
        and count(browser, "aside [data-subagents] [data-dimmed]") == 1
        and "grandchild task" in browser.text("aside [data-subagents]"),
        timeout=90,
    )
    rows = browser.script(
        "return [...document.querySelectorAll('aside [data-subagents] [data-tree-session]')]"
        ".map((b) => [b.innerText, b.parentElement.dataset.dimmed === 'true'])"
    )
    assert rows[0][1] and not rows[1][1], rows
    assert "grandchild task" in rows[1][0], rows
    assert count(browser, "aside [data-subagents]") == 1, "another session shows a tree"
    deep = browser.script("return document.querySelector(arguments[0]).dataset.session", open_row(browser))
    browser.shot("tree-collapsed-running")
    click_row(browser, open_row(browser))
    browser.wait(
        "the expanded tree",
        lambda: tree_state(browser) == "expanded" and count(browser, "aside [data-subagents] [data-dimmed]") == 0,
    )
    assert count(browser, nested) == 2
    browser.shot("tree-expanded")
    click_row(browser, open_row(browser))
    browser.wait("the collapsed tree", lambda: tree_state(browser) == "collapsed")
    # Another session opened: the running grandchild's session shows no tree.
    click_row(browser, 'aside [data-session="%s"]' % root)
    browser.wait("another session", lambda: browser.text("#session-title") == root_title)
    time.sleep(1)
    assert count(browser, nested) == 0, "a session other than the open one shows its tree"
    assert count(browser, "aside [data-subagents]") <= 1
    assert count(browser, "aside [data-subagents]") == 0 or browser.script(NESTED_UNDER_OPEN_ROW)
    print("a running subagent sits under its dimmed parent; only the open session has a tree")
    click_row(browser, 'aside [data-session="%s"]' % deep)
    browser.wait("the deep session", lambda: count(browser, 'aside [data-session="%s"][aria-current=true]' % deep) == 1)
    browser.wait("the grandchild's end", lambda: count(browser, nested) == 0, timeout=90)

    # Fork at a message, then revert one.
    new_session(browser)
    send(browser, "hello there")
    browser.wait("the first answer", lambda: answers(browser) >= 1)
    send(browser, "second message")
    browser.wait("the second answer", lambda: answers(browser) >= 2)
    first = browser.text("#session-title")
    browser.click("#transcript [data-action=user-menu]")
    browser.wait("the message menu", lambda: "Fork" in browser.text("#sheet"))
    browser.shot("message-menu")
    browser.click("#message-fork")
    browser.wait(
        "the fork",
        lambda: browser.text("#session-title") not in (first, "New session")
        and "hello there" in browser.text("#transcript")
        and "second message" not in browser.text("#transcript"),
    )
    print("forked the session at its first message")
    browser.shot("fork")
    send(browser, "third message")
    browser.wait("the third answer", lambda: answers(browser) >= 1)
    browser.script(
        "const menus = document.querySelectorAll('#transcript [data-action=user-menu]');"
        "menus[menus.length - 1].click()"
    )
    browser.wait("the message menu", lambda: "Revert" in browser.text("#sheet"))
    browser.click("#message-revert")
    browser.wait("the revert guard", lambda: "Revert to before" in browser.text("#sheet"))
    browser.click("#revert-confirm")
    browser.wait(
        "the revert",
        lambda: "third message" not in browser.text("#transcript")
        and browser.script("return document.querySelector('#composer').value") == "third message",
    )
    print("reverted the last message; its text is back in the composer")
    browser.shot("revert")
    clear_composer(browser)

    # Text that starts with a slash is a message: there are no slash
    # commands, and no menu pops up while typing one.
    browser.type("#composer", "/comp")
    time.sleep(0.4)
    assert count(browser, "#sheet, [data-command]") == 0, "typing a slash opened a menu"
    clear_composer(browser)
    sent = answers(browser)
    send(browser, "/compact")
    browser.wait("the slash message's answer", lambda: answers(browser) > sent)
    assert browser.script(
        "return [...document.querySelectorAll('#transcript .user-message')].some((m) => m.innerText.includes('/compact'))"
    ), "the text was not sent as a message"
    assert count(browser, "#sheet") == 0, "a sheet opened for a slash text"
    print("typing /compact sends it as a message")

    # Compact the context from the ⋮ menu, with a focus, then without.
    for focus in ("keep the greetings", ""):
        menu(browser, "compact")
        browser.wait("the compact sheet", lambda: count(browser, "#compact-run") == 1)
        if focus:
            browser.type("#compact-focus", focus)
            browser.shot("compact-sheet")
        browser.click("#compact-run")
        browser.wait(
            "the compaction",
            lambda: any(
                word in browser.text("#compact-outcome")
                for word in ("context compacted", "checkpoint", "compaction failed")
            ),
            timeout=60,
        )
        print("compact (focus %r): %s" % (focus, browser.text("#compact-outcome")))
        close_sheet(browser)
    browser.shot("compact")

    # A goal, paused while its run streams. The engine tells the agent of a
    # pause only while the session runs (an idle one has no one to tell), so
    # the objective makes the scripted model answer slowly, and the pause
    # waits for that answer to stream: pausing between the goal's runs would
    # send no control message.
    menu(browser, "goal")
    browser.wait("the goal sheet", lambda: count(browser, "#goal-input") == 1)
    browser.type("#goal-input", "finish the scenario slowly")
    browser.shot("goal-sheet")
    browser.click("#goal-set")
    browser.wait("the goal bar", lambda: "finish the scenario" in browser.text("#goal-bar"))
    browser.shot("goal")
    browser.wait("the pause control", lambda: count(browser, "#goal-bar [data-goal-action=pause]") == 1)
    browser.wait("the goal run's answer", lambda: "word3" in browser.text("#transcript"))
    browser.click("#goal-bar [data-goal-action=pause]")
    browser.wait("the paused goal", lambda: count(browser, "#goal-bar [data-goal-action=resume]") == 1)
    print("set a goal and paused it")
    browser.shot("goal-paused")
    browser.wait("the goal run's end", lambda: "working" not in browser.text("#status-bar"), timeout=60)
    # The pause's control message rode along with the goal's run.
    browser.wait("the goal control row", lambda: "Goal paused" in browser.text("#transcript"))
    assert "[engine goal control]" not in browser.text("#transcript"), "a producer message shows unfolded"
    browser.script(
        "const rows = [...document.querySelectorAll('#transcript [data-action=toggle-block]')]"
        ".filter((row) => row.innerText.includes('Goal paused'));"
        "rows[rows.length - 1].click()"
    )
    browser.wait("the opened goal control row", lambda: "[engine goal control]" in browser.text("#transcript"))
    print("goal control messages fold under their summary and open on a click")
    # A cancelled goal leaves the bar; its details stay in the goal sheet.
    browser.click("#goal-bar [data-goal-action=cancel]")
    browser.wait("the bar gone with the cancelled goal", lambda: count(browser, "#goal-bar") == 0)
    menu(browser, "goal")
    browser.wait("the cancelled goal's sheet", lambda: "cancelled" in browser.text("#sheet"))
    close_sheet(browser)
    print("a cancelled goal leaves the bar and keeps its details")

    # Move a slow call to the background, list it, and stop it.
    send(browser, "be sleepy")
    browser.wait("the approval", lambda: browser.text("#approvals-badge") != "")
    browser.click("#approvals-badge")
    browser.wait("the approval sheet", lambda: count(browser, "#sheet [data-decision=approve_once]") > 0)
    browser.click("#sheet [data-decision=approve_once]")
    browser.click("#sheet h2 ~ button")
    browser.wait("the background control", lambda: count(browser, "#transcript [data-action=background]") > 0)
    browser.click("#transcript [data-action=background]")
    browser.wait("the background bar", lambda: "in background" in browser.text("#background-bar"))
    browser.shot("background")
    # Once its run ends, the session waits on the call: its row has a clock,
    # and its tooltip says what it waits on.
    waiting_row = "aside [data-session-row] [aria-current=true]"
    browser.wait(
        "the waiting row",
        lambda: count(browser, waiting_row + ' [data-session-activity="waiting"]') == 1,
        timeout=40,
    )
    tooltip = browser.script("return document.querySelector(arguments[0]).title", waiting_row)
    assert "waiting on: background call" in tooltip, tooltip
    print("a session waiting on a background call shows a clock:", tooltip.splitlines()[-1])
    menu(browser, "background")
    browser.wait("the background sheet", lambda: count(browser, "#sheet [data-stop]") > 0)
    browser.shot("background-sheet")
    browser.click("#sheet [data-stop]")
    browser.wait("the stopped call", lambda: count(browser, "#background-bar") == 0, timeout=40)
    print("moved a call to the background, listed it, and stopped it")
    browser.wait(
        "the background report row",
        lambda: browser.script(
            "return [...document.querySelectorAll('#transcript [data-action=toggle-block]')]"
            ".some((row) => /background/i.test(row.innerText))"
        ),
        timeout=40,
    )
    assert "<background_result" not in browser.text("#transcript"), "a background report shows unfolded"
    browser.shot("background-report")
    browser.wait("the run's end", lambda: "working" not in browser.text("#status-bar"), timeout=60)

    # Attach an image; one too large is offered downscaled.
    uploads = tempfile.mkdtemp(prefix="cookie-web-e2e-")
    atexit.register(shutil.rmtree, uploads, True)
    small = png(os.path.join(uploads, "small.png"), 64, 48)
    browser.type("#attach-input", small)
    browser.wait("the small chip", lambda: "small.png" in browser.text("#draft-files"))
    large = png(os.path.join(uploads, "large.png"), 600, 400, noisy=True)
    browser.type("#attach-input", large)
    browser.wait("the downscale offer", lambda: "Downscale" in browser.text("#downscale-offer"))
    browser.shot("downscale-offer")
    browser.click("#downscale-accept")
    browser.wait("the downscaled chip", lambda: "large.jpg" in browser.text("#draft-files"), timeout=40)
    browser.shot("attachments")
    send(browser, "look at these")
    browser.wait(
        "the attached images",
        lambda: count(browser, "#transcript .user-message .media img") >= 2,
    )
    print("attached an image and a downscaled one; both show in the message")
    browser.shot("attached")

    # A tool's media block shows inline.
    send(browser, "show the picture")
    browser.wait(
        "the read",
        lambda: "Read dot.png" in browser.text("#transcript")
        and "working" not in browser.text("#status-bar"),
    )
    browser.script(
        "const tools = document.querySelectorAll('#transcript [data-action=toggle-tool]');"
        "tools[tools.length - 1].click()"
    )
    browser.wait(
        "the media block",
        lambda: count(browser, "#transcript [data-tool] .media img") >= 1,
    )
    print("a read's media block shows inline")
    browser.shot("media-block")

    # Run a skill.
    menu(browser, "skills")
    browser.wait("the skill search", lambda: count(browser, "#skill-search") == 1)
    browser.type("#skill-search", "gre")
    browser.wait("the skills", lambda: count(browser, "#sheet [data-skill=greet]") == 1)
    browser.click("#sheet [data-skill=greet]")
    browser.type("#skill-args", "Ada")
    browser.shot("skills")
    browser.click("#skill-run")
    browser.wait("the skill", lambda: "greet" in browser.text("#transcript").lower() and "skill" in browser.text("#transcript").lower())
    print("ran the greet skill")
    browser.wait("the skill run's end", lambda: "working" not in browser.text("#status-bar"), timeout=60)

    # A permission override, then its removal.
    menu(browser, "permissions")
    browser.wait("the permissions", lambda: count(browser, "#sheet .permission-row") > 0)
    set_text(browser, "#permission-action", "bash")
    browser.type("#permission-pattern", "git *")
    browser.click("#sheet [data-new-effect=allow]")
    browser.click("#permission-add")
    browser.wait(
        "the override",
        lambda: count(browser, '#sheet [data-rule="bash git *"]') == 1
        and "session" in browser.text('#sheet [data-rule="bash git *"]'),
    )
    browser.shot("permissions")
    browser.click('#sheet [data-remove="bash git *"]')
    browser.wait("the removed override", lambda: count(browser, '#sheet [data-rule="bash git *"]') == 0)
    print("added a session permission override and removed it")
    browser.click("#sheet h2 ~ button")

    # A new session's draft rules are previewed against its agent.
    new_session(browser)
    menu(browser, "permissions")
    browser.wait("the draft preview", lambda: "Rules for the new session" in browser.text("#sheet"))
    browser.wait("the previewed rules", lambda: count(browser, "#sheet .permission-row") > 0)
    set_text(browser, "#permission-action", "read")
    browser.type("#permission-pattern", "secrets/*")
    browser.click("#sheet [data-new-effect=deny]")
    browser.click("#permission-add")
    browser.wait("the draft rule", lambda: count(browser, '#sheet [data-rule="read secrets/*"]') == 1)
    print("previewed a draft rule for a new session")
    browser.shot("permissions-draft")
    browser.click("#sheet h2 ~ button")



def rule_effect(browser, rule):
    """The effect the row of `rule` (its `data-rule`) has pressed, if any."""
    return browser.script(
        "const b = document.querySelector('#sheet [data-rule=\"' + arguments[0] + '\"] [aria-pressed=true]');"
        "return b ? b.dataset.effect : null",
        rule,
    )


def add_rule(browser, action, runtime, pattern, effect):
    """Fills the permission form and adds its rule."""
    set_text(browser, "#permission-action", action)
    set_text(browser, "#permission-runtime", runtime)
    set_text(browser, "#permission-pattern", pattern)
    browser.click("#sheet [data-new-effect=%s]" % effect)
    browser.click("#permission-add")


def permission_rules(browser):
    """The permission form takes any action a tool declares, suggests the
    session's actions and the daemon's runtimes, and limits a rule to one
    runtime as `action@runtime`, which `message` and `skill` take none of.
    A rule on `local` alone outranks the same rule everywhere."""
    new_session(browser)
    send(browser, "hello")
    browser.wait("the answer", lambda: answers(browser) >= 1 and "working" not in browser.text("#status-bar"))
    menu(browser, "permissions")
    browser.wait("the permissions", lambda: count(browser, "#sheet .permission-row") > 0)
    options = browser.script(
        "return [...document.querySelectorAll(arguments[0])].map(o => o.value)",
        "#permission-action-options option",
    )
    for action in ("read", "bash", "mcp", "list_runtimes"):
        assert action in options, options
    runtimes = browser.script(
        "return [...document.querySelectorAll(arguments[0])].map(o => o.value)",
        "#permission-runtime-options option",
    )
    assert "local" in runtimes and "alt" in runtimes, runtimes

    # `message` and `skill` take no runtime: the field turns off and says why.
    set_text(browser, "#permission-runtime", "local")
    set_text(browser, "#permission-action", "message")
    browser.wait("the disabled runtime", lambda: browser.script("return document.querySelector('#permission-runtime').disabled"))
    assert browser.script("return document.querySelector('#permission-runtime').value") == ""
    assert "no runtime" in browser.script("return document.querySelector('#permission-runtime').title")
    set_text(browser, "#permission-action", "delegate")
    browser.wait("the enabled runtime", lambda: not browser.script("return document.querySelector('#permission-runtime').disabled"))
    # An action that is no valid name is refused before it is sent.
    set_text(browser, "#permission-action", "Deploy")
    browser.wait("the refused action", lambda: "invalid permission action" in browser.text("#sheet [data-key-error]"))
    assert browser.script("return document.querySelector('#permission-add').disabled")
    set_text(browser, "#permission-action", "bash")
    set_text(browser, "#permission-runtime", "Lab-PC")
    browser.wait("the refused runtime", lambda: "invalid runtime selector" in browser.text("#sheet [data-key-error]"))
    print("the form refuses what the daemon would and turns the runtime off for message")

    # `bash` allowed everywhere: a call runs unasked.
    browser.click('#sheet [data-rule="bash *"] [data-effect=allow]')
    browser.wait("bash allowed", lambda: rule_effect(browser, "bash *") == "allow")
    browser.click("#sheet h2 ~ button")
    send(browser, "use a tool")
    browser.wait("the unasked tool", lambda: browser.text("#transcript").count("The tool ran.") == 1, timeout=40)
    browser.wait("the run's end", lambda: "working" not in browser.text("#status-bar"), timeout=40)
    assert browser.text("#approvals-badge") == "", "an allowed call asked"

    # Then asked on `local` alone.
    menu(browser, "permissions")
    browser.wait("the permissions", lambda: rule_effect(browser, "bash *") == "allow")
    add_rule(browser, "bash", "local", "*", "ask")
    browser.wait(
        "the rule on local",
        lambda: rule_effect(browser, "bash@local *") == "ask"
        and "session" in browser.text('#sheet [data-rule="bash@local *"]'),
    )
    assert "bash@local" in browser.text('#sheet [data-rule="bash@local *"]')
    # A runtime's own action, typed: it gets its own `*` row and the rule.
    add_rule(browser, "deploy", "", "prod *", "deny")
    browser.wait("the custom action's rule", lambda: rule_effect(browser, "deploy prod *") == "deny")
    assert count(browser, '#sheet [data-rule="deploy *"]') == 1
    print("added bash@local and a custom action's rule through the form")
    if SCREENSHOTS:
        set_text(browser, "#permission-action", "bash")
        set_text(browser, "#permission-runtime", "alt")
        set_text(browser, "#permission-pattern", "git *")
        for scheme in ("light", "dark"):
            browser.cdp("Emulation.setEmulatedMedia", {"features": [{"name": "prefers-color-scheme", "value": scheme}]})
            for device in ("desktop", "phone"):
                if device == "desktop":
                    browser.resize(1280, 900)
                else:
                    browser.phone(390, 844)
                browser.script("document.querySelector(arguments[0]).scrollIntoView({block: 'center'})", '#sheet [data-rule="bash@local *"]')
                settle(browser)
                browser.shot("permission-rules-%s-%s" % (device, scheme))
                browser.script("document.querySelector('#permission-form').scrollIntoView({block: 'end'})")
                settle(browser)
                browser.shot("permission-form-%s-%s" % (device, scheme))
        browser.cdp("Emulation.setEmulatedMedia", {"features": []})
        browser.resize(1280, 900)
        set_text(browser, "#permission-runtime", "")
        set_text(browser, "#permission-pattern", "")
    browser.click("#sheet h2 ~ button")

    # The rule on `local` outranks `bash` everywhere: the call asks.
    send(browser, "use a tool")
    browser.wait("the approval", lambda: browser.text("#approvals-badge") != "", timeout=40)
    browser.click("#approvals-badge")
    browser.wait("the approval sheet", lambda: count(browser, "#sheet [data-decision=approve_once]") > 0)
    browser.click("#sheet [data-decision=approve_once]")
    close_sheet(browser)
    browser.wait("the tool's answer", lambda: browser.text("#transcript").count("The tool ran.") == 2, timeout=40)
    browser.wait("the run's end", lambda: "working" not in browser.text("#status-bar"), timeout=40)
    print("bash@local: ask outranks bash: allow")

    # An edited rule keeps its runtime; the custom one goes.
    menu(browser, "permissions")
    browser.wait("the rule on local", lambda: rule_effect(browser, "bash@local *") == "ask")
    browser.click('#sheet [data-edit="bash@local *"]')
    browser.wait("the filled form", lambda: browser.script("return document.querySelector('#permission-runtime').value") == "local")
    assert "Edit the rule" in browser.text("#permission-form")
    set_text(browser, "#permission-pattern", "git *")
    browser.click("#sheet [data-new-effect=allow]")
    browser.click("#permission-add")
    browser.wait(
        "the edited rule",
        lambda: rule_effect(browser, "bash@local git *") == "allow"
        and count(browser, '#sheet [data-rule="bash@local *"]') == 0,
    )
    browser.click('#sheet [data-remove="deploy prod *"]')
    browser.wait("the removed rule", lambda: count(browser, '#sheet [data-rule="deploy prod *"]') == 0)
    browser.click('#sheet [data-remove="bash@local git *"]')
    browser.wait("the removed rule on local", lambda: count(browser, '#sheet [data-rule="bash@local git *"]') == 0)
    print("edited the rule on local, keeping its runtime, and removed the rules")
    browser.click("#sheet h2 ~ button")

def set_text(browser, selector, text):
    browser.script(
        "const e = document.querySelector(arguments[0]); e.value = arguments[1];"
        "e.dispatchEvent(new Event('input'))",
        selector,
        text,
    )


def close_sheet(browser):
    """Closes an open sheet and drawer, and goes back to the chat view."""
    if count(browser, "#sheet") > 0:
        browser.click("#sheet h2 ~ button")
    browser.script(
        "const d = document.querySelector('[data-drawer-backdrop]'); if (d) d.click();"
        "if (document.querySelector('#page'))"
        "  document.querySelector('[data-rail=primary] [data-view=chat]').click()"
    )
    browser.wait("the chat view", lambda: count(browser, "#page") == 0)
    # A phone's rail opens Chat on its sessions, in the drawer.
    browser.script("const d = document.querySelector('[data-drawer-backdrop]'); if (d) d.click()")
    browser.wait("the closed drawer", lambda: count(browser, "#drawer") == 0)


# The usage sheet's figures for the session (its first list), by name.
USAGE_FIGURES = """
const figures = {};
for (const dt of document.querySelector('#sheet dl').querySelectorAll('dt'))
  figures[dt.innerText] = dt.nextElementSibling.innerText;
return figures;
"""


def cache_hit_rate(browser):
    """Checks the usage sheet's hit rate: every scripted reply read 800 of
    its 1,000 prompt tokens from the cache, so a session with requests
    shows 80.0 % and one without a dash, never 0 %."""
    browser.wait("the usage figures", lambda: count(browser, "#sheet [data-cache-hit=session]") == 1)
    figures = browser.script(USAGE_FIGURES)
    expected = "80.0%" if figures["requests"] != "0" else "—"
    assert figures["cache hit rate"] == expected, figures
    detail = browser.text("#sheet [data-cache-detail=session]")
    if expected == "80.0%":
        assert "input tokens read from the prompt cache" in detail, detail
        assert browser.script(
            "return document.querySelector('#sheet [data-cache-hit=session]').title"
        ) == detail, "the rate's tooltip is not its figures"
        # The context line groups thousands like the figures below it; the
        # scripted prompts are 1,000 tokens, so a bare 1000 is the bug.
        context = browser.text("#sheet #usage-context")
        assert re.fullmatch(r"Context: \d{1,3}(,\d{3})* tokens in the last request", context), context
        assert "1000" not in context, context
    else:
        assert detail == "No input tokens yet.", detail
    return figures


def view(browser, name):
    """Opens a view from the rail."""
    browser.click("[data-rail=primary] [data-view=%s]" % name)
    browser.wait("the %s view" % name, lambda: browser.script(
        "const p = document.querySelector('#page'); return p ? p.dataset.view : 'chat'") == name)


# How far the composer's text starts right of its attach buttons, and ends
# left of its action buttons; `null` until the attach button shows.
COMPOSER_CLEARANCE = """
const t = document.querySelector('#composer');
if (!t || !document.querySelector('#attach')) return null;
const box = t.getBoundingClientRect(), style = getComputedStyle(t);
const visible = (selector) => [...document.querySelectorAll(selector)].filter((b) => b.getClientRects().length);
const left = Math.max(...visible('[data-composer-left] button').map((b) => b.getBoundingClientRect().right));
const right = Math.min(...visible('[data-composer-actions] button').map((b) => b.getBoundingClientRect().left));
return [box.left + parseFloat(style.paddingLeft) - left, right - (box.right - parseFloat(style.paddingRight))];
"""


def composer_clear(browser, what):
    """Asserts, with no interaction since the composer first showed, that
    its text keeps clear of the buttons in its box. A fresh page starts on
    the new session's composer."""
    browser.wait("the page", lambda: count(browser, "#composer") > 0)
    browser.wait("the attach button", lambda: browser.script(COMPOSER_CLEARANCE) is not None)
    time.sleep(0.5)
    left, right = browser.script(COMPOSER_CLEARANCE)
    assert left >= 0 and right >= 0, "%s: the composer's text overlaps its buttons by %s, %s px" % (what, left, right)
    assert browser.script("return document.querySelector('#composer').placeholder") == "Message"


def hover_at(browser, selector, dx=0):
    """Moves a mouse to `dx` px right of `selector`'s centre."""
    origin = {ELEMENT: browser.find(selector)}
    perform(browser, [{
        "type": "pointer", "id": "mouse", "parameters": {"pointerType": "mouse"},
        "actions": [{"type": "pointerMove", "duration": 0, "origin": origin, "x": dx, "y": 0}],
    }])


# The opacity of the newest user message's ⋯, and whether the bubble moved
# since `arguments[0]` (its left edge then).
MESSAGE_MENU = """
const rows = document.querySelectorAll('#transcript .user-row');
const row = rows[rows.length - 1];
const button = row.querySelector('[data-action=user-menu]');
const style = getComputedStyle(button);
return [style.display, parseFloat(style.opacity), Math.round(row.querySelector('[data-bubble]').getBoundingClientRect().left)];
"""


def message_menu(browser):
    """A user message's ⋯ shows only while a mouse is over its row, the
    whole band at the bubble's height, or the keyboard is in it; the bubble
    never moves for it."""
    browser.wait("a user message", lambda: count(browser, "#transcript .user-row") > 0)
    rows = "#transcript .user-row"
    newest = "document.querySelectorAll('%s')[document.querySelectorAll('%s').length - 1]" % (rows, rows)
    browser.script("%s.scrollIntoView({block: 'center'})" % newest)
    hover(browser, "#composer")
    browser.wait("the hidden ⋯", lambda: browser.script(MESSAGE_MENU)[1] == 0)
    display, _, left = browser.script(MESSAGE_MENU)
    assert display != "none", "the ⋯ takes no room"
    browser.shot("message-menu-hidden")
    # The far left of the row's band, well away from the bubble.
    width = browser.script("return %s.getBoundingClientRect().width" % newest)
    browser.script("%s.id = 'newest-user-row'" % newest)
    hover_at(browser, "#newest-user-row", -round(width / 2) + 8)
    browser.wait("the ⋯ over the row's band", lambda: browser.script(MESSAGE_MENU)[1] == 1)
    assert browser.script(MESSAGE_MENU)[2] == left, "the bubble moved for the ⋯"
    browser.shot("message-menu-hovered")
    hover(browser, "#newest-user-row [data-action=user-menu]")
    assert browser.script(MESSAGE_MENU)[1] == 1, "the ⋯ hid on the way to it"
    hover(browser, "#composer")
    browser.wait("the hidden ⋯ again", lambda: browser.script(MESSAGE_MENU)[1] == 0)
    # Keyboard focus in the row shows it too.
    browser.script("document.querySelector('#newest-user-row [data-action=user-menu]').focus()")
    browser.wait("the focused ⋯", lambda: browser.script(MESSAGE_MENU)[1] == 1)
    browser.script("document.activeElement.blur(); document.querySelector('#newest-user-row').removeAttribute('id')")
    browser.wait("the blurred ⋯", lambda: browser.script(MESSAGE_MENU)[1] == 0)
    print("a message's ⋯ shows only over its row or with focus in it, and the bubble stays put")


def touch_message_menu():
    """On a touch screen the ⋯ is gone: a long press on the bubble opens
    the message's menu."""
    # A coarse pointer that cannot hover: a phone's.
    phone = Browser(touch=True)
    try:
        phone.go(LINK)
        phone.wait("the connection", lambda: phone.text("#connected") != "")
        phone.phone(390, 844)
        assert phone.script("return matchMedia('(pointer: coarse)').matches"), "the pointer is not coarse"
        phone.click("main [data-sidebar-toggle]")
        phone.wait("the drawer", lambda: count(phone, "#drawer nav[aria-label=Sessions] [data-session]") > 0)
        phone.script(
            "const rows = [...document.querySelectorAll('#drawer nav[aria-label=Sessions] [data-session]')];"
            "const other = rows.find((r) => r.getAttribute('aria-current') !== 'true');"
            "if (other) other.click(); else document.querySelector('[data-drawer-backdrop]').click();"
        )
        phone.wait("a user message", lambda: count(phone, "#transcript .user-row") > 0, timeout=30)
        phone.script(
            "const rows = document.querySelectorAll('#transcript .user-row');"
            "const row = rows[rows.length - 1]; row.id = 'pressed-row'; row.scrollIntoView({block: 'center'})"
        )
        assert phone.script(MESSAGE_MENU)[0] == "none", "a touch screen shows the ⋯"
        # A scroll cancels the press; a held finger opens the menu.
        long_press(phone, "#pressed-row [data-bubble]", drag=40)
        time.sleep(0.3)
        assert count(phone, "#sheet") == 0, "a scroll opened the message menu"
        long_press(phone, "#pressed-row [data-bubble]")
        phone.wait("the message menu", lambda: "Fork" in phone.text("#sheet") and "Copy" in phone.text("#sheet"))
        phone.shot("phone-message-long-press")
        small = phone.script(SMALL_TARGETS)
        assert not small, "small targets in the message menu: %s" % small
        print("phone: no ⋯ on a touch screen; a long press on the bubble opens the message menu")
    finally:
        phone.close()


def menu(browser, command):
    """Runs `command` (its `data-command`) from the session's ⋮ menu."""
    browser.script("document.querySelector('main button[aria-label=Commands]').click()")
    browser.wait("the session menu", lambda: count(browser, "#sheet [data-command=%s]" % command) == 1)
    browser.click("#sheet [data-command=%s]" % command)


def open_section(browser, name, section=None):
    """Opens a view from the rail, and a Settings section in it."""
    view(browser, name)
    if section:
        browser.click("#page [data-section=%s]" % section)
        browser.wait(
            "the %s section" % section,
            lambda: count(browser, "#page [data-section-body=%s]" % section) == 1,
        )


def open_row(browser):
    return "aside [data-session-row] [data-session][aria-current=true]"


def tree_state(browser):
    return browser.script(
        "const t = document.querySelector('aside [data-subagents]'); return t ? t.dataset.tree : ''"
    )


def click_row_button(browser, rows, text, button):
    """Clicks `button` inside the first of `rows` whose text has `text`."""
    clicked = browser.script(
        "for (const row of document.querySelectorAll(arguments[0])) {"
        "  if (row.innerText.includes(arguments[1])) {"
        "    row.querySelector(arguments[2]).click(); return true; } }"
        "return false",
        rows,
        text,
        button,
    )
    assert clicked, "no %s row has %r" % (rows, text)


def management(browser):
    """The runtime, machine, MCP, provider, configuration, memory, and plugin
    panels."""
    # A new session's runtimes are its cards: the runtimes command points
    # at them instead of a sheet.
    new_session(browser)
    menu(browser, "runtime")
    browser.wait("the pointer to the cards", lambda: "cards above the composer" in browser.text("#status"))
    assert count(browser, "#sheet") == 0 and count(browser, "#new-session") == 1
    print("a new session's runtimes command points at its cards")
    send(browser, "hello there")
    browser.wait("the session", lambda: browser.text("#session-title") != "New session" and answers(browser) >= 1, timeout=60)

    # A session's attachment detached and attached again.
    click_row(browser, "aside nav[aria-label=Sessions] [data-session]")
    browser.wait("an open session", lambda: browser.text("#session-title") != "New session")
    menu(browser, "runtime")
    browser.wait("the attachments", lambda: "local/coding" in browser.text("#sheet"))
    click_row_button(browser, "#sheet .runtime-row", "local/coding", "[data-detach]")
    browser.wait("the detached runtime", lambda: "local/coding" not in browser.text("#sheet .runtime-row:last-of-type") and "detached" in browser.text("#status"))
    browser.click("#runtime-attach")
    browser.type("#runtime-search", "coding")
    browser.wait("the coding choice", lambda: count(browser, '#sheet [data-choice="local/coding"]') == 1)
    browser.click('#sheet [data-choice="local/coding"]')
    browser.type("#runtime-workspace", os.path.join(HOME, "workspace"))
    browser.click("#runtime-attach-confirm")
    browser.wait("the attached runtime", lambda: "attached runtime" in browser.text("#status"))
    browser.wait("its row", lambda: "local/coding" in browser.text("#sheet"))
    browser.shot("runtimes")
    print("detached a session's runtime and attached it again")

    # Rename a machine, in the Machines & runtimes view the sheet links to.
    browser.click("#runtime-open-machines")
    browser.wait("the machines", lambda: count(browser, "#page .machine-row") > 0)
    assert count(browser, "#sheet") == 0, "the sheet stayed over the view"
    browser.wait("the runtime registry", lambda: count(browser, '#page [data-runtime="local"]') == 1)
    browser.click('#page .machine-row [data-machine-action="rename"]')
    browser.type("#machine-name", "renamed")
    browser.click("#machine-apply")
    browser.wait("its row", lambda: count(browser, '#page [data-machine-entry^="renamed"]') == 1)
    browser.shot("machines")
    print("renamed a machine")
    close_sheet(browser)

    # An MCP server that cannot start: added, disabled, removed.
    open_section(browser, "settings", "mcp")
    browser.wait("the MCP panel", lambda: count(browser, "#mcp-new") == 1)
    browser.click("#mcp-new")
    browser.type("#mcp-name", "broken")
    browser.type("#mcp-endpoint", "false")
    browser.click("#mcp-save")
    browser.wait("the new server", lambda: count(browser, '#page [data-server="broken"]') == 1)
    browser.wait(
        "its failure",
        lambda: browser.text('#page [data-server="broken"] .mcp-state') in ("failed", "reconnecting", "disconnected"),
        timeout=40,
    )
    browser.shot("mcp")
    browser.click('#page [data-server="broken"] [data-mcp-toggle]')
    browser.wait("the disabled server", lambda: browser.text('#page [data-server="broken"] .mcp-state') == "disabled")
    browser.click('#page [data-server="broken"] [data-mcp-remove]')
    browser.wait("the removed server", lambda: count(browser, '#page [data-server="broken"]') == 0)
    print("added an MCP server that cannot start, disabled it, and removed it")
    close_sheet(browser)

    # Connect a provider with an API key, then disconnect it.
    open_section(browser, "settings", "providers")
    browser.type("#provider-search", "openai")
    browser.wait("the provider", lambda: count(browser, '#page [data-provider="openai"]') == 1)
    browser.click('#page [data-provider="openai"]')
    browser.wait("the connect form", lambda: count(browser, "#credential-0") == 1)
    browser.type("#credential-0", "sk-e2e-not-a-real-key")
    browser.shot("connect-form")
    browser.click("#provider-connect")
    browser.wait("the connection", lambda: "Connected provider openai" in browser.text("#status"), timeout=60)
    assert "sk-e2e" not in browser.text(), "the key shows on the page"
    set_text(browser, "#provider-search", "openai")
    browser.wait("the provider", lambda: "connected" in browser.text('#page [data-provider="openai"]'))
    browser.click('#page [data-provider="openai"]')
    browser.click("#provider-disconnect")
    browser.click("#provider-disconnect-confirm")
    close_sheet(browser)
    browser.wait("the disconnection", lambda: "Disconnected provider openai" in browser.text("#status"), timeout=60)
    print("connected and disconnected a provider")

    # Edit config.toml: an unknown key is refused and stays to be fixed; a
    # valid edit applies.
    open_section(browser, "settings", "configuration")
    browser.wait("the documents", lambda: count(browser, '#page [data-config-path="config.toml"]') == 1)
    browser.click('#page [data-config-path="config.toml"] button')
    browser.wait("the editor", lambda: count(browser, "#config-text") == 1)
    original = browser.script("return document.querySelector('#config-text').value")
    set_text(browser, "#config-text", original + "\ncolour = 1\n")
    browser.click("#config-apply")
    browser.wait("the refusal", lambda: "was not changed" in browser.text("#config-error"))
    browser.shot("config-error")
    set_text(browser, "#config-text", original + "\n# edited from the page\n")
    browser.click("#config-apply")
    browser.wait("the closed editor", lambda: count(browser, "#config-text") == 0)
    close_sheet(browser)
    browser.wait("the applied edit", lambda: "Applied config.toml" in browser.text("#status"))
    print("a config edit with an unknown key was refused, then a valid one applied")
    close_sheet(browser)

    # Memory: an invalid one is refused, then created, edited over a change
    # made meanwhile, and deleted, in the project `e2e` the daemon adopted
    # from its memory folder at start.
    open_section(browser, "memory")
    browser.wait("the memories", lambda: count(browser, "#memory-new") == 1)
    set_text(browser, "#memory-new-url", "project://e2e/notes")
    browser.click("#memory-new")
    browser.wait("the editor", lambda: count(browser, "#memory-text") == 1)
    set_text(browser, "#memory-text", "---\ndescription: \n---\nfirst\n")
    browser.click("#memory-save")
    browser.wait("the invalid memory", lambda: "not a valid memory" in browser.text("#memory-pending"))
    browser.click("#memory-edit-again")
    set_text(browser, "#memory-text", "---\ndescription: Browser scenario notes\n---\nfirst\n")
    browser.click("#memory-save")
    # Its project's folder starts folded.
    browser.wait("its folder", lambda: count(browser, '#page [data-project="e2e"]') == 1)
    browser.click('#page [data-project="e2e"] [data-project-toggle]')
    browser.wait("its row", lambda: count(browser, '#page [data-memory="project://e2e/notes"]') == 1)
    browser.click('#page [data-memory="project://e2e/notes"] button')
    browser.wait("the editor", lambda: "first" in browser.script("const e = document.querySelector('#memory-text'); return e ? e.value : ''"))
    path = os.path.join(HOME, ".cookie-agent", "memory", "projects", "e2e", "notes.md")
    with open(path, "w") as file:
        file.write("---\ndescription: Browser scenario notes\n---\nchanged on disk\n")
    set_text(browser, "#memory-text", "---\ndescription: Browser scenario notes\n---\nsecond\n")
    browser.click("#memory-save")
    browser.wait("the conflict", lambda: "changed since it was opened" in browser.text("#memory-pending"))
    browser.shot("memory-conflict")
    browser.click("#memory-overwrite")
    browser.wait("the overwrite", lambda: count(browser, "#memory-text") == 0)
    # The editor closes when the page has the answer; the file may land a
    # moment later, so wait for it rather than read it once.
    def overwritten():
        with open(path, "rb") as file:
            return b"second" in file.read()
    browser.wait("the overwritten file", overwritten)
    # The trash deletes the revision listed, so wait for the overwrite's: the
    # first 16 hex digits of the SHA-256 of the file. Any revision but the
    # first is not enough, because the list the conflict refreshed shows the
    # revision written on disk above until the overwrite's refresh lands, and
    # a delete at that revision is refused as changed.
    with open(path, "rb") as file:
        overwrite = hashlib.sha256(file.read()).hexdigest()[:16]
    browser.wait("the overwrite listed", lambda: browser.script(
        "return document.querySelector('#page [data-memory=\"project://e2e/notes\"]')?.dataset.revision") == overwrite)
    browser.click('#page [data-memory="project://e2e/notes"] [data-memory-delete]')
    browser.click("#memory-delete-confirm")
    browser.wait("the deleted memory", lambda: count(browser, '#page [data-memory="project://e2e/notes"]') == 0)
    print("a memory was refused, created, overwritten over a conflict, and deleted")
    close_sheet(browser)

    # Restart the reference plugin.
    open_section(browser, "settings", "plugins")
    browser.wait("the plugin", lambda: count(browser, '#page [data-plugin="reference"]') == 1)
    browser.wait("its start", lambda: browser.text('#page [data-plugin="reference"] .plugin-state') == "connected", timeout=40)
    browser.shot("plugins")
    browser.click('#page [data-plugin="reference"] [data-plugin-restart]')
    close_sheet(browser)
    browser.wait("the restart", lambda: "is restarting" in browser.text("#status"))
    print("restarted the reference plugin")


def drag(browser, source, target, touch=False, dx=0):
    """Drags `source` onto `target`: a mouse at once, a finger after a long
    press. `dx` moves the press off the source's middle."""
    pointer = "touch" if touch else "mouse"
    steps = [
        {"type": "pointerMove", "duration": 0, "origin": {ELEMENT: browser.find(source)}, "x": dx, "y": 0},
        {"type": "pointerDown", "button": 0},
    ]
    if touch:
        steps.append({"type": "pause", "duration": 800})
    steps += [
        {"type": "pointerMove", "duration": 150, "origin": "pointer", "x": 0, "y": 12},
        {"type": "pointerMove", "duration": 300, "origin": {ELEMENT: browser.find(target)}, "x": 0, "y": 0},
        {"type": "pointerUp", "button": 0},
    ]
    perform(browser, [{"type": "pointer", "id": pointer, "parameters": {"pointerType": pointer}, "actions": steps}])


def drag_row(browser, source, target, touch=False):
    """Drags a session row by its title: over a hovered row, its buttons
    cover the middle."""
    width = browser.script("return document.querySelector(arguments[0]).getBoundingClientRect().width", source)
    drag(browser, source, target, touch=touch, dx=int(-width / 2 + 40))


def under(browser, machine, runtime):
    return count(browser, '#page [data-machine-drop="%s"] [data-runtime="%s"]' % (machine, runtime)) == 1


def views_2026(browser):
    """The Agents view's sidebar, the machines' drag and drop, Move to…,
    and Add a runtime, and the Memory view's project folders and agent
    files."""
    # Agents: the documents beside the open one; New agent, then Delete.
    view(browser, "agents")
    browser.wait("the agents", lambda: count(browser, '#agents-sidebar [data-config-path="agents/primary.md"]') == 1)
    assert shown(browser, "#agents-sidebar"), "a desktop does not pin the agents"
    assert count(browser, "#agents-sidebar [data-config-path] svg") >= 2, "the agents have no icons"
    browser.click('#agents-sidebar [data-config-path="agents/helper.md"] button')
    browser.wait("the agent's editor", lambda: browser.text("#agent-name") == "helper")
    assert count(browser, '#agents-sidebar [data-config-path="agents/helper.md"] [aria-current=true]') == 1
    browser.shot("agents-open")
    browser.click("#page [data-sidebar-toggle]")
    browser.wait("the hidden agents", lambda: not shown(browser, "#agents-sidebar"))
    browser.click("#page [data-sidebar-toggle]")
    browser.wait("the agents again", lambda: shown(browser, "#agents-sidebar"))
    browser.click("#agents-sidebar [data-new-agent]")
    set_text(browser, "#agents-sidebar #config-new-path", "agents/e2e.md")
    browser.click("#agents-sidebar #config-new")
    browser.wait("the new agent's editor", lambda: browser.text("#agent-name") == "e2e")
    set_text(browser, "#config-text", "---\ndescription: Browser scenario agent\nmode: subagent\nenabled: true\n"
             "models:\n  - { model: \"custom.local/test\", variant: null }\npermissions: {}\n---\nHelp.\n")
    browser.click("#config-apply")
    browser.wait("the new agent's row", lambda: count(browser, '#agents-sidebar [data-config-path="agents/e2e.md"]') == 1)
    assert os.path.exists(os.path.join(HOME, ".cookie-agent", "agents", "e2e.md"))
    browser.click('#agents-sidebar [data-config-path="agents/e2e.md"] button')
    browser.wait("its editor", lambda: browser.text("#agent-name") == "e2e")
    browser.click("#page [data-config-delete]")
    browser.click("#config-delete-confirm")
    browser.wait("the deleted agent", lambda: count(browser, '#agents-sidebar [data-config-path="agents/e2e.md"]') == 0)
    assert not os.path.exists(os.path.join(HOME, ".cookie-agent", "agents", "e2e.md"))
    print("agents: the sidebar opens one beside the others; one was created and deleted")

    # Machines: `box` has never connected, so it is on no machine. A mouse
    # drags it onto one, Move to… takes it to a new one, a finger drags it
    # back after a long press, and a long press let go offers Move to….
    view(browser, "machines")
    browser.wait("box on no machine", lambda: count(browser, '#page [data-unplaced] [data-runtime="box"]') == 1)
    machine = browser.script("return document.querySelector('#page [data-machine-drop]').dataset.machineDrop")
    browser.shot("machines-cards")
    drag(browser, '#page [data-runtime="box"]', '#page [data-machine-drop="%s"]' % machine)
    browser.wait("box dropped on %s" % machine, lambda: under(browser, machine, "box"))
    print("machines: a mouse dragged a runtime onto a machine")
    browser.click('#page [data-runtime="box"] [data-machine-action="move to machine"]')
    browser.wait("Move to…", lambda: count(browser, '#page [data-move-picker="box"]') == 1)
    browser.shot("machines-move")
    browser.type("#machine-name", "e2e-lab")
    browser.click("#machine-apply")
    browser.wait("box on e2e-lab", lambda: under(browser, "e2e-lab", "box"))
    print("machines: Move to… took a runtime to a new machine")
    # The page is unselectable while the finger is down (a phone would
    # select the words around it), and selectable again once it lifts.
    browser.script("""window.e2ePressed = false;
        new MutationObserver(() => {
          if (document.documentElement.classList.contains('pressing')) window.e2ePressed = true;
        }).observe(document.documentElement, { attributes: true });""")
    drag(browser, '#page [data-runtime="box"]', '#page [data-machine-drop="%s"]' % machine, touch=True)
    browser.wait("box dragged back by a finger", lambda: under(browser, machine, "box"))
    assert browser.script("return window.e2ePressed"), "a held finger left the page selectable"
    assert not browser.script("return document.documentElement.classList.contains('pressing')"), \
        "the page stays unselectable after the finger lifted"
    assert browser.script("return String(window.getSelection())") == "", "the drag selected text"
    print("machines: a finger dragged a runtime after a long press, selecting no text")
    long_press(browser, '#page [data-runtime="box"]')
    browser.wait("Move to… after a long press", lambda: count(browser, '#page [data-move-picker="box"]') == 1)
    click_row_button(browser, '#page [data-move-picker="box"] div', "Cancel", "button:last-child")
    browser.wait("Move to… closed", lambda: count(browser, '#page [data-move-picker="box"]') == 0)
    browser.click('#page [data-runtime="box"] [data-machine-action="unpin"]')
    browser.wait("box unpinned", lambda: count(browser, '#page [data-unplaced] [data-runtime="box"]') == 1)
    browser.click('#page [data-machine-drop="e2e-lab"] [data-machine-action="forget"]')
    browser.wait("e2e-lab forgotten", lambda: count(browser, '#page [data-machine-drop="e2e-lab"]') == 0)
    # `roamer` dialed in once and left: its machine remembers it, dimmed,
    # with when it was last seen, until Forget.
    roamer = '#page [data-machine-drop] [data-runtime="roamer"]'
    browser.wait("roamer remembered", lambda: count(browser, roamer + "[data-offline=true]") == 1)
    detail = browser.text(roamer)
    assert "last seen" in detail and "not connected" in detail, detail
    opacity = browser.script("return getComputedStyle(document.querySelector(arguments[0])).opacity", roamer)
    assert float(opacity) < 1, opacity
    browser.script("document.querySelector(arguments[0]).scrollIntoView({block: 'center'})", roamer)
    browser.shot("machines-remembered")
    print("machines: an offline runtime stays on its machine, dimmed:", detail.replace("\n", " / "))
    browser.click(roamer + ' [data-machine-action="forget-runtime"]')
    browser.wait("roamer forgotten", lambda: count(browser, roamer) == 0)
    assert count(browser, '#page [data-unplaced] [data-runtime="roamer"]') == 1
    print("machines: Forget leaves an offline runtime on no machine")

    # Add a runtime: an ssh template into config.toml, the dial-in
    # command, and the guides.
    icon = "return document.querySelector('#machines-add svg').innerHTML"
    plus = browser.script(icon)
    browser.click("#machines-add")
    browser.wait("the add panel", lambda: count(browser, "#add-runtime") == 1)
    # Open, its button shows − instead of +.
    browser.wait("the − icon", lambda: browser.script(icon) != plus)
    # A click and then keys, as a person types: the field keeps the focus
    # and its caret after the mouse button comes up. (A release once cleared
    # the page's selection, which in Chrome also takes a focused field's
    # caret, so the keys typed nothing.)
    browser.click("#ssh-id")
    keys = [step for key in "e2essh" for step in ({"type": "keyDown", "value": key}, {"type": "keyUp", "value": key})]
    perform(browser, [{"type": "key", "id": "keyboard", "actions": keys}])
    assert browser.script("return document.activeElement.id") == "ssh-id", "the clicked field lost the focus"
    assert browser.script("return document.querySelector('#ssh-id').value") == "e2essh", "keys after a click typed nothing"
    browser.type("#ssh-host", "me@localhost")
    browser.wait("the template", lambda: "[runtimes.e2essh]" in browser.text("#ssh-preview"))
    browser.shot("machines-add")
    browser.click("#ssh-add")
    browser.wait("the template written", lambda: "Added [runtimes.e2essh]" in browser.text("#ssh-outcome"))
    with open(os.path.join(HOME, ".cookie-agent", "config.toml")) as file:
        written = file.read()
    assert "[runtimes.e2essh]" in written and '"me@localhost"' in written, written
    browser.click("#ssh-add")
    browser.wait("the second add refused", lambda: "already" in browser.text("#ssh-outcome"))
    # The command carries the token this page connected with, hidden on
    # screen until revealed.
    token = LINK.split("#token=", 1)[1]
    command = browser.script("return document.querySelector('#dial-in-command').value")
    assert command.startswith("cookie runtime serve --daemon ws://<daemon host>:"), command
    assert token not in command and "••••" in command, command
    browser.click("#dial-in-reveal")
    browser.wait("the revealed token", lambda: token in browser.script("return document.querySelector('#dial-in-command').value"))
    browser.click("#dial-in-reveal")
    guides = browser.script("return [...document.querySelectorAll('#add-runtime [data-guide]')].map((a) => a.href)")
    assert len(guides) == 5 and all(g.startswith("https://cookie-agent.github.io/doc/latest/") for g in guides), guides
    print("machines: Add a runtime wrote an ssh template and shows the dial-in command and the guides")

    # Memory: a project's folder folds until opened, the browser keeps it
    # open, and deleting the project keeps its other files.
    folder = os.path.join(HOME, ".cookie-agent", "memory", "projects", "e2eproj")
    os.makedirs(folder, exist_ok=True)
    for name in ("build", "test"):
        with open(os.path.join(folder, name + ".md"), "w") as file:
            file.write("---\ndescription: How to %s\nkeywords: [e2e]\n---\n%s it.\n" % (name, name))
    with open(os.path.join(folder, "notes.txt"), "w") as file:
        file.write("not a memory\n")
    view(browser, "memory")
    browser.wait("the project folder", lambda: count(browser, '#page [data-project="e2eproj"]') == 1)
    assert count(browser, '#page [data-memory^="project://e2eproj/"]') == 0, "a project starts open"
    browser.click('#page [data-project="e2eproj"] [data-project-toggle]')
    browser.wait("the open folder", lambda: count(browser, '#page [data-memory^="project://e2eproj/"]') == 2)
    browser.go(LINK)
    browser.wait("the connection", lambda: browser.text("#connected") != "")
    view(browser, "memory")
    browser.wait("the folder after a reload", lambda: count(browser, '#page [data-memory^="project://e2eproj/"]') == 2)
    browser.shot("memory-project-open")
    browser.click('#page [data-project="e2eproj"] [data-project-delete]')
    browser.wait("the confirmation", lambda: "1 file that is not a memory" in browser.text("#page [data-project-confirm]"))
    browser.shot("memory-project-delete")
    browser.click("#project-delete-confirm")
    browser.wait("the deleted project", lambda: count(browser, '#page [data-memory^="project://e2eproj/"]') == 0)
    assert sorted(os.listdir(folder)) == ["notes.txt"], os.listdir(folder)
    print("memory: a project folder stays open across a reload; deleting it kept its other file")

    # An agent's memory: the daemon's description and keywords are fixed.
    browser.wait("the agent's memory", lambda: count(browser, '#page [data-memory="agent://helper"]') == 1)
    browser.click('#page [data-memory="agent://helper"] button')
    browser.wait("the agent memory editor", lambda: count(browser, "#memory-description") == 1)
    fixed = browser.script("return [...document.querySelectorAll('[data-fixed-keyword]')].map((k) => k.innerText.trim())")
    assert fixed == ["helper", "subagent"], fixed
    assert "Browser scenario subagent" in browser.text("#memory-description"), browser.text("#memory-description")
    browser.shot("memory-agent")
    set_text(browser, "#memory-keywords", "e2e-keyword")
    set_text(browser, "#memory-text", "Prefers short answers.\n")
    browser.click("#memory-save")
    browser.wait("the saved agent memory", lambda: count(browser, "#memory-description") == 0)
    with open(os.path.join(HOME, ".cookie-agent", "memory", "agents", "helper.md")) as file:
        saved = file.read()
    assert "e2e-keyword" in saved and "Prefers short answers." in saved and "subagent" in saved, saved
    print("memory: an agent's file keeps the daemon's description and keywords and takes the user's")
    close_sheet(browser)


def shown(browser, selector):
    """Whether `selector` matches an element the page lays out."""
    return browser.script(
        "const e = document.querySelector(arguments[0]);"
        "return !!e && e.getClientRects().length > 0",
        selector,
    )


def memory_changes(browser):
    """A model's memory change is one transcript line that opens the Memory
    view at that memory, its row marked, with when it was modified and its
    size against its limit; the editor shows its stamps read-only and leaves
    their lines out of the text, and a save keeps when it was created. A
    memory over its limit is marked, and a line about a deleted memory says
    it is gone. test-web-page.sh gives this group's agent memory, each user
    memory at most 1 KB."""
    browser.resize(1280, 900)
    url = "user://e2e-build"
    path = os.path.join(HOME, ".cookie-agent", "memory", "user", "e2e-build.md")
    row = '#page [data-memory="%s"]' % url
    new_session(browser)
    send(browser, "remember how the build works")
    browser.wait("the change line", lambda: count(browser, "#transcript [data-memory-change]") == 1, timeout=40)
    browser.wait("the run's end", lambda: "working" not in browser.text("#status-bar"), timeout=40)
    line = browser.text("#transcript [data-memory-change]")
    assert re.fullmatch(r"\s*Saved\s+user://e2e-build\s*", line), line
    # The line sits between the call that made the change and the answer
    # after it.
    order = browser.script(
        "const line = document.querySelector('#transcript [data-memory-change]');"
        " const tool = [...document.querySelectorAll('#transcript [data-tool]')]"
        "   .find(e => e.innerText.includes('Write memory user://e2e-build'));"
        " const answer = [...document.querySelectorAll('#transcript .markdown')]"
        "   .find(e => e.innerText.includes('The tool ran.'));"
        " const after = (a, b) => !!a && !!b && !!(a.compareDocumentPosition(b) & Node.DOCUMENT_POSITION_FOLLOWING);"
        " return [after(tool, line), after(line, answer)]"
    )
    assert order == [True, True], order
    browser.shot("memory-change-line")

    browser.click("#transcript [data-memory-change] button")
    browser.wait("the Memory view at the memory", lambda: page_view(browser) == "memory"
                 and count(browser, row + "[data-memory-focused]") == 1)
    stamps = browser.text(row + " [data-memory-stamps]")
    assert re.fullmatch(r"modified less than a minute ago · \d+ bytes of 1 KB", stamps.strip()), stamps
    exact = browser.script("return document.querySelector(arguments[0]).title", row + " [data-memory-stamps]")
    assert re.search(r"^created: \d{4}-\d\d-\d\d \d\d:\d\d:\d\d UTC$", exact, re.M), exact
    assert count(browser, row + "[data-memory-over]") == 0
    browser.shot("memory-focused")

    # The editor: the stamps read-only above the text, which has none.
    with open(path) as file:
        created = [line for line in file.read().splitlines() if line.startswith("created: ")]
    assert len(created) == 1, created
    browser.click(row + " button")
    browser.wait("the editor", lambda: count(browser, "#memory-stamps") == 1)
    assert re.fullmatch(
        r"created less than a minute ago · modified less than a minute ago · \d+ bytes of 1 KB",
        browser.text("#memory-stamps").strip(),
    ), browser.text("#memory-stamps")
    text = browser.script("return document.querySelector('#memory-text').value")
    assert "description: The browser scenario builds with trunk" in text, text
    assert "created:" not in text and "modified:" not in text, text
    browser.shot("memory-editor-stamps")
    set_text(browser, "#memory-text", text.replace("with trunk.", "with trunk and tailwind."))
    browser.click("#memory-save")
    browser.wait("the saved memory", lambda: count(browser, "#memory-text") == 0)
    with open(path) as file:
        saved = file.read()
    assert "trunk and tailwind" in saved, saved
    assert created[0] in saved.splitlines(), saved
    print("memory: a change line opens the memory, whose stamps show and survive a save")

    # A memory over its limit is marked.
    big = os.path.join(HOME, ".cookie-agent", "memory", "user", "e2e-big.md")
    with open(big, "w") as file:
        file.write("---\ndescription: A memory over its limit\n---\n" + "x" * 1500 + "\n")
    view(browser, "chat")
    view(browser, "memory")
    big_row = '#page [data-memory="user://e2e-big"]'
    browser.wait("the memory over its limit", lambda: count(browser, big_row + "[data-memory-over]") == 1)
    assert "over its limit" in browser.text(big_row), browser.text(big_row)
    assert count(browser, "#page [data-memory-focused]") == 0, "the rail kept the mark"
    browser.shot("memory-over-limit")
    os.remove(big)

    # A line about a deleted memory says it is gone.
    view(browser, "chat")
    send(browser, "forget how the build works")
    browser.wait("the delete line", lambda: count(browser, "#transcript [data-memory-change]") == 2, timeout=40)
    browser.wait("the run's end", lambda: "working" not in browser.text("#status-bar"), timeout=40)
    assert not os.path.exists(path)
    browser.script("const l = document.querySelectorAll('#transcript [data-memory-change] button'); l[l.length - 1].click()")
    browser.wait("the gone note", lambda: page_view(browser) == "memory"
                 and "user://e2e-build is not in memory now." in browser.text("#page [data-memory-gone]"))
    print("memory: a line about a deleted memory says it is gone")


def cron_reading(browser, text):
    """Waits for the form's plain reading of the schedule to say `text`."""
    browser.wait("the reading %r" % text, lambda: browser.text("#cron-reading").strip() == text)


def cron_row(name):
    """The Scheduled prompts row whose title is `name`."""
    return '#page [data-cron-job][data-title="%s"]' % name


def cron_jobs(browser):
    """Scheduled prompts: the ⋮ menu's "Schedule a prompt" opens the job
    form for the open session, its plain reading following what is typed;
    the job lists its schedule, reading, next fire, target, state, and last
    outcome; run now records a fire, disable and enable change its state,
    and an edit renames it. A new-session job's run session shows a clock
    in the session list that opens its job, and deleting the job keeps the
    session. The in-session job stays, for the phone audit."""
    browser.resize(1280, 900)
    new_session(browser)
    send(browser, "a session to schedule into")
    browser.wait("the run's end", lambda: "working" not in browser.text("#status-bar"), timeout=40)
    browser.wait("an open session", lambda: browser.text("#session-title") != "New session")
    session_title = browser.text("#session-title").strip()

    menu(browser, "schedule")
    browser.wait("the job form", lambda: count(browser, "#cron-form") == 1)
    assert page_view(browser) == "cron"
    assert browser.script(
        "return document.querySelector('#cron-target-session').getAttribute('aria-checked')"
    ) == "true", "the form does not target the open session"
    assert "this session" in browser.text("#cron-target-session"), browser.text("#cron-target-session")
    set_text(browser, "#cron-name", "E2E nightly")
    set_text(browser, "#cron-prompt", "Check the build.")
    set_text(browser, "#cron-schedule", "0 3 * * *")
    cron_reading(browser, "every day at 03:00, once")
    set_text(browser, "#cron-max-runs", "unlimited")
    cron_reading(browser, "every day at 03:00")
    set_text(browser, "#cron-schedule", "0 3 * *")
    browser.wait("the refusal", lambda: "five cron fields" in browser.text("#cron-reading"))
    assert browser.script("return document.querySelector('#cron-reading').dataset.valid") == "false"
    set_text(browser, "#cron-schedule", "0 9 * * 1-5")
    cron_reading(browser, "every Monday to Friday at 09:00")
    browser.shot("cron-form")
    browser.click("#cron-save")
    nightly = cron_row("E2E nightly")
    browser.wait("the job", lambda: count(browser, "#cron-form") == 0 and count(browser, nightly) == 1)
    text = browser.text(nightly)
    for part in ("0 9 * * 1-5", "every Monday to Friday at 09:00", "this session", "enabled", "last: never"):
        assert part in text, (part, text)
    assert re.search(r"next \d{4}-\d\d-\d\d 09:00 \(in \d+ (hour|day)s?\)", text), text
    # The new job is selected: its details, actions, and empty history.
    browser.wait("its details", lambda: count(browser, nightly + " [data-cron-details]") == 1)
    assert browser.text(nightly + " [data-cron-detail=delivery]") == "queue"
    assert "It has not fired yet." in browser.text(nightly + " [data-cron-history]")
    browser.shot("cron-view")
    print("cron: the ⋮ menu schedules a prompt into the open session, read plainly")

    # Run now queues the prompt into the session, recorded as a run now.
    browser.click(nightly + " [data-cron-action=run]")
    browser.wait("the run now", lambda: "run now · delivered" in browser.text(nightly + " [data-cron-history]"), timeout=40)
    assert count(browser, nightly + " [data-cron-fire-session]") == 1
    browser.click(nightly + " [data-cron-action=disable]")
    browser.wait("the disabled job", lambda: browser.text(nightly + " [data-cron-state]") == "disabled by you")
    assert "no next fire" in browser.text(nightly)
    browser.click(nightly + " [data-cron-action=enable]")
    browser.wait("the enabled job", lambda: browser.text(nightly + " [data-cron-state]") == "enabled")
    browser.click(nightly + " [data-cron-action=edit]")
    browser.wait("the edit form", lambda: count(browser, "#cron-form") == 1)
    assert browser.script("return document.querySelector('#cron-schedule').value") == "0 9 * * 1-5"
    assert "this session" in browser.text("#cron-target") or session_title in browser.text("#cron-target")
    set_text(browser, "#cron-name", "E2E weekdays")
    browser.click("#cron-save")
    weekdays = cron_row("E2E weekdays")
    browser.wait("the renamed job", lambda: count(browser, weekdays) == 1 and count(browser, nightly) == 0)
    print("cron: run now, disable, enable, and edit")

    # A job that starts a new session each run: its run session's clock
    # opens it.
    browser.click("#cron-new")
    browser.wait("the job form", lambda: count(browser, "#cron-form") == 1)
    browser.click("#cron-target-new")
    browser.wait("the template's fields", lambda: shown(browser, "#cron-agent") and not shown(browser, "#cron-delivery"))
    set_text(browser, "#cron-name", "E2E fresh")
    set_text(browser, "#cron-prompt", "Start somewhere new.")
    set_text(browser, "#cron-schedule", "in 2h")
    cron_reading(browser, "once in 2h")
    browser.click("#cron-save")
    fresh = cron_row("E2E fresh")
    browser.wait("the new-session job", lambda: count(browser, fresh + " [data-cron-details]") == 1)
    assert "new session · " in browser.text(fresh + " [data-cron-target]"), browser.text(fresh)
    browser.click(fresh + " [data-cron-action=run]")
    browser.wait("the run session", lambda: count(browser, fresh + " [data-cron-fire-session]") == 1, timeout=40)
    run_session = browser.script(
        "return document.querySelector(arguments[0]).dataset.cronFireSession", fresh + " [data-cron-fire-session]"
    )
    view(browser, "chat")
    clock = 'aside [data-session-row="%s"] [data-session-cron]' % run_session
    browser.wait("the clock mark", lambda: count(browser, clock) == 1, timeout=40)
    title = browser.script("return document.querySelector(arguments[0]).title", clock)
    assert title == "Started by the scheduled prompt E2E fresh", title
    browser.shot("cron-clock")
    browser.click(clock)
    browser.wait("the job from its clock", lambda: page_view(browser) == "cron"
                 and count(browser, fresh + " [data-cron-details]") == 1)
    print("cron: a run session's clock opens its job")

    # Deleting the job keeps its session.
    browser.click(fresh + " [data-cron-action=delete]")
    browser.wait("the confirmation", lambda: count(browser, "#cron-delete-confirm") == 1)
    browser.click("#cron-delete-confirm")
    browser.wait("the deleted job", lambda: count(browser, fresh) == 0)
    assert count(browser, weekdays) == 1
    view(browser, "chat")
    browser.wait("the kept session", lambda: count(browser, 'aside [data-session-row="%s"]' % run_session) == 1)
    print("cron: deleting a job keeps the sessions it started")


def page_view(browser):
    return browser.script("const p = document.querySelector('#page'); return p ? p.dataset.view : 'chat'")


def perform(browser, sources):
    """Runs WebDriver input actions, then releases what they hold."""
    call("POST", browser.path("/actions"), {"actions": sources})
    call("DELETE", browser.path("/actions"))


def hover(browser, selector):
    origin = {ELEMENT: browser.find(selector)}
    perform(browser, [{
        "type": "pointer", "id": "mouse", "parameters": {"pointerType": "mouse"},
        "actions": [{"type": "pointerMove", "duration": 0, "origin": origin, "x": 0, "y": 0}],
    }])


def click_row(browser, selector):
    """Clicks a session row near its left end, by its title: over a
    hovered row, its three buttons cover the middle. The list can redraw
    the row between finding it and clicking it; it is found again then."""
    for attempt in range(5):
        width = browser.script("return document.querySelector(arguments[0]).getBoundingClientRect().width", selector)
        origin = {ELEMENT: browser.find(selector)}
        try:
            perform(browser, [{
                "type": "pointer", "id": "mouse", "parameters": {"pointerType": "mouse"},
                "actions": [
                    {"type": "pointerMove", "duration": 0, "origin": origin, "x": int(-width / 2 + 40), "y": 0},
                    {"type": "pointerDown", "button": 0},
                    {"type": "pointerUp", "button": 0},
                ],
            }])
            return
        except RuntimeError as error:
            if "stale element reference" not in str(error) or attempt == 4:
                raise
            time.sleep(0.2)


def long_press(browser, selector, hold=1200, drag=0):
    """A finger held on `selector` for `hold` ms, moved `drag` px down
    first (a scroll). Chrome takes a touch held under a second for a tap,
    whose click lands on what the press opened."""
    origin = {ELEMENT: browser.find(selector)}
    steps = [
        {"type": "pointerMove", "duration": 0, "origin": origin, "x": 0, "y": 0},
        {"type": "pointerDown", "button": 0},
    ]
    if drag:
        steps.append({"type": "pointerMove", "duration": 100, "origin": "pointer", "x": 0, "y": drag})
    steps += [{"type": "pause", "duration": hold}, {"type": "pointerUp", "button": 0}]
    perform(browser, [{
        "type": "pointer", "id": "finger", "parameters": {"pointerType": "touch"}, "actions": steps,
    }])


# A session row's middle and its trash's, the trash's size, and the
# background of each.
TRASH_GEOMETRY = """
const row = document.querySelector(arguments[0]);
const open = row.querySelector('[data-session]').getBoundingClientRect();
const trash = row.querySelector('[data-delete]');
const box = trash.getBoundingClientRect();
return [open.top + open.height / 2, box.top + box.height / 2, box.width, box.height,
    getComputedStyle(row.querySelector('[data-session]')).backgroundColor,
    getComputedStyle(trash).backgroundColor];
"""


def deleting(browser):
    """Deleting a session from its row: the trash on hover, the inline
    confirmation naming it, a running session refused with the daemon's
    message, and the open session gone for a new one."""
    new_session(browser)
    send(browser, "go slow")
    browser.wait("the slow run", lambda: "word3" in browser.text("#transcript"))
    current = browser.script(
        "return document.querySelector('aside [data-session-row] [aria-current=true]').dataset.session"
    )
    row = 'aside [data-session-row="%s"]' % current
    title = browser.text("#session-title")
    # The pointer away from the list: the row it last clicked may be this one.
    hover(browser, "#transcript")
    assert not shown(browser, row + " [data-delete]"), "the trash shows without a hover"
    hover(browser, row)
    browser.wait("the trash on hover", lambda: shown(browser, row + " [data-delete]"))
    # It is centred on the row's two lines, a full touch target, and at rest
    # in the hovered row's colour.
    middle, trash_middle, width, height, row_colour, trash_colour = browser.script(TRASH_GEOMETRY, row)
    assert abs(middle - trash_middle) <= 1, "the trash is %s px off the row's middle" % (trash_middle - middle)
    assert width >= 44 and height >= 44, "the trash is %sx%s px" % (width, height)
    assert row_colour == trash_colour, "the trash is %s over a %s row" % (trash_colour, row_colour)
    browser.click(row + " [data-delete]")
    browser.wait("the confirmation", lambda: title in browser.text("aside [data-delete-confirm-box]"))
    browser.shot("delete-confirm")
    browser.click('aside [data-delete-confirm="%s"]' % current)
    browser.wait("the refusal", lambda: browser.text("aside [data-delete-error]") != "")
    print("deleting a running session is refused:", browser.text("aside [data-delete-error]"))
    browser.shot("delete-refused")
    assert count(browser, row) == 1
    browser.type("#composer", ESCAPE)
    browser.wait("the cancelled run", lambda: "working" not in browser.text("#status-bar"), timeout=40)
    hover(browser, row)
    browser.click(row + " [data-delete]")
    browser.click('aside [data-delete-confirm="%s"]' % current)
    browser.wait("the deleted row", lambda: count(browser, row) == 0, timeout=30)
    browser.wait("a new session", lambda: browser.text("#session-title") == "New session")
    print("deleted the open session from its row; the page went to a new session")


def field_values(browser, key):
    return browser.script(
        "return [...document.querySelectorAll(arguments[0] + ' [data-workspace-field]')].map((f) => f.value)",
        card(key),
    )


def open_cards(browser):
    browser.click("aside [data-new-session]")
    show_cards(browser)


def selected_cards(browser):
    return browser.script(
        "return [...document.querySelectorAll('#new-session [data-card][data-selected=true]')].map((c) => c.dataset.card)"
    )


def runtime_cards(browser):
    """The new session's runtime cards: a new session shows them above its
    composer with only `global` selected and no Start button, groups by
    machine that fold and are remembered, two runtimes at their own
    workspaces, one runtime at two, a refusal on its card, an offline card,
    global deselected, no coding card at all, no selection remembered, and
    every path under its runtime's home."""
    # A new session shows its cards with only `global`, above the composer.
    open_cards(browser)
    assert count(browser, "#start-session") == 0 and count(browser, "#runtimes-chip") == 0, "a Start button or chip"
    assert selected_cards(browser) == ["global/global"], selected_cards(browser)
    assert browser.text("#draft-places [data-draft-places]") == "global only: no workspace"
    browser.shot("new-session-cards")
    print("a new session shows its cards with only global, above the composer")
    groups = browser.script(
        "return [...document.querySelectorAll('#new-session [data-group]')].map((g) => [g.dataset.group, g.dataset.open])"
    )
    print("card groups:", groups)
    daemon_group = groups[0][0]
    assert groups[0][1] == "true", "this daemon's machine is not open: %s" % groups
    assert count(browser, card("global/global")) == 1 and count(browser, card("roamer/coding")) == 1, browser.text("#new-session")
    # The offline card cannot be selected.
    assert browser.script("return document.querySelector(arguments[0]).dataset.state", card("roamer/coding")) == "offline"
    browser.script("document.querySelector(arguments[0]).click()", card("roamer/coding"))
    time.sleep(0.3)
    assert "roamer/coding" not in selected_cards(browser), "an offline card was selected"
    # A machine group folds to its summary, and the browser remembers it.
    summary = browser.text('#new-session [data-group="%s"] [data-group-summary]' % daemon_group)
    assert daemon_group in summary and "selected" in summary and "ready" in summary, summary
    browser.click('#new-session [data-group-toggle="%s"]' % daemon_group)
    browser.wait("the folded group", lambda: count(browser, card("local/coding")) == 0)
    browser.shot("cards-collapsed")
    browser.go(LINK.split("#")[0])
    browser.wait("the connection", lambda: browser.text("#connected") != "")
    open_cards(browser)
    browser.wait("the cards again", lambda: count(browser, '#new-session [data-group="%s"]' % daemon_group) == 1)
    time.sleep(0.5)
    assert count(browser, card("local/coding")) == 0, "the folded group opened again after a reload"
    browser.click('#new-session [data-group-toggle="%s"]' % daemon_group)
    browser.wait("the open group", lambda: count(browser, card("local/coding")) == 1)
    print("machine groups fold to their summary, and the browser remembers it:", summary)
    # Runtimes the daemon starts on use report their machine once they ran.
    for name, is_open in browser.script(
        "return [...document.querySelectorAll('#new-session [data-group]')].map((g) => [g.dataset.group, g.dataset.open])"
    ):
        if is_open != "true":
            browser.click('#new-session [data-group-toggle="%s"]' % name)
    browser.wait("every group open", lambda: count(browser, '#new-session [data-group][data-open="false"]') == 0)

    # Two runtimes at their own workspaces, one of them at two: the note
    # says tools get prefixes, and the session attaches all three.
    second = os.path.join(HOME, "second")
    elsewhere = os.path.join(HOME, "elsewhere")
    if "local/coding" not in selected_cards(browser):
        browser.click(card("local/coding") + " [data-card-toggle]")
    set_text(browser, field("local/coding"), WORKSPACE)
    browser.click(card("local/coding") + " [data-add-workspace]")
    browser.wait("a second field", lambda: count(browser, field("local/coding", 1)) == 1)
    # An empty field holds the message back and says why.
    browser.wait("the blocker", lambda: "enter a workspace" in browser.text("#start-blocker"))
    browser.type("#composer", "too early")
    browser.type("#composer", ENTER)
    browser.wait("the held message", lambda: "enter a workspace" in browser.text("#draft-error"))
    assert browser.script("return document.querySelector('#composer').value") == "too early"
    clear_composer(browser)
    set_text(browser, field("local/coding", 1), "~/second")
    browser.wait("the cleared notice", lambda: count(browser, "#draft-error") == 0)
    # `alt` has one card, `alt` until it ran and `alt/coding` after.
    alt = "alt" if count(browser, card("alt")) else "alt/coding"
    browser.click(card(alt) + " [data-card-toggle]")
    browser.wait("the alt field", lambda: count(browser, field(alt)) == 1)
    set_text(browser, field(alt), elsewhere)
    browser.wait("the prefix note", lambda: "local_workspace_bash" in browser.text("#prefix-note"))
    assert "3 workspaces" in browser.text("#prefix-note"), browser.text("#prefix-note")
    places = browser.text("#draft-places [data-draft-places]")
    assert places.startswith("~/workspace, ~/second, alt:") and places.endswith("elsewhere"), places
    browser.shot("cards-two-runtimes")
    # A refusal shows on its card, nothing is created, and the message comes
    # back.
    sessions_before = count(browser, "aside [data-session-row]")
    browser.click(card("box") + " [data-card-toggle]")
    set_text(browser, field("box"), WORKSPACE)
    send(browser, "hello from three workspaces")
    browser.wait("the refusal", lambda: browser.text(card("box") + " [data-card-error]") != "", timeout=40)
    browser.wait("the message back", lambda: browser.script("return document.querySelector('#composer').value") == "hello from three workspaces")
    assert "box" in browser.text("#draft-error"), browser.text("#draft-error")
    browser.script("document.querySelector(arguments[0]).scrollIntoView({block: 'center'})", card("box"))
    browser.shot("cards-refusal")
    assert count(browser, "#new-session") == 1 and count(browser, "aside [data-session-row]") == sessions_before
    assert browser.text("#session-title") == "New session"
    print("a refused workspace shows on its card:", browser.text(card("box") + " [data-card-error]"))
    browser.click(card("box") + " [data-card-toggle]")
    browser.wait("the cleared notice", lambda: count(browser, "#draft-error") == 0)
    browser.type("#composer", ENTER)
    browser.wait("the session", lambda: browser.text("#session-title") != "New session" and answers(browser) >= 1, timeout=60)
    assert "hello from three workspaces" in browser.text("#transcript")
    row = OPEN_SESSION_ROW
    # Both runtimes are on this daemon's machine: it, then the workspaces.
    browser.wait("the row's badges", lambda: badge_kinds(browser, row) == ["machine", "workspace"])
    shown_badges = badges(browser, row)
    assert shown_badges[0][1] == daemon_group, shown_badges
    assert shown_badges[1][1] == "~/workspace, ~/second, ~/elsewhere", shown_badges
    assert "~/workspace" in browser.script(TOOLTIP, row) and "~/elsewhere" in browser.script(TOOLTIP, row)
    MADE["one machine"] = open_session_id(browser)
    browser.shot("cards-session-places")
    print("two runtimes at three workspaces on one machine:", shown_badges)

    # Nothing is remembered: the next new session starts with `global`
    # alone again, whatever the open session has.
    open_cards(browser)
    for name, is_open in browser.script(
        "return [...document.querySelectorAll('#new-session [data-group]')].map((g) => [g.dataset.group, g.dataset.open])"
    ):
        if is_open != "true":
            browser.click('#new-session [data-group-toggle="%s"]' % name)
    alt = "alt/coding" if count(browser, card("alt/coding")) else "alt"
    assert selected_cards(browser) == ["global/global"], selected_cards(browser)
    print("a new session starts with global alone, not the open session's runtimes")

    # One runtime alone: its machine, and its workspace under its home.
    browser.click(card(alt) + " [data-card-toggle]")
    browser.wait("the alt field", lambda: count(browser, field(alt)) == 1)
    set_text(browser, field(alt), "~/elsewhere")
    send(browser, "hello from elsewhere")
    browser.wait("the session", lambda: browser.text("#session-title") != "New session" and answers(browser) >= 1, timeout=60)
    browser.wait("its row", lambda: badge_kinds(browser, row) == ["machine", "workspace"])
    assert badges(browser, row)[1][1] == "~/elsewhere", badges(browser, row)
    MADE["one"] = open_session_id(browser)

    open_cards(browser)
    # Global deselected, and no coding card: a session with no workspace.
    browser.wait("the preselected cards", lambda: "global/global" in selected_cards(browser))
    for key in selected_cards(browser):
        browser.click(card(key) + " [data-card-toggle]")
    assert selected_cards(browser) == [], selected_cards(browser)
    assert count(browser, "#start-blocker") == 0
    assert browser.text("#draft-places [data-draft-places]") == "no workspace"
    send(browser, "hello from nowhere")
    browser.wait("the session", lambda: browser.text("#session-title") != "New session" and answers(browser) >= 1, timeout=60)
    browser.wait("its row", lambda: browser.text(row + " [data-session-title]") != "" and badges(browser, row) == [])
    MADE["none"] = open_session_id(browser)
    menu(browser, "runtime")
    browser.wait("the runtimes sheet", lambda: count(browser, "#sheet") == 1)
    time.sleep(1)
    assert count(browser, '#sheet .runtime-row') == 0, browser.text("#sheet")
    close_sheet(browser)
    print("a session without global and without a coding runtime works nowhere")

    # Back to a new session in the daemon's own workspace for what follows.
    open_cards(browser)
    if "global/global" not in selected_cards(browser):
        browser.click(card("global/global") + " [data-card-toggle]")
    select_local(browser, WORKSPACE)


def draft_first_message(browser):
    """A new session is drafted on its composer, under its cards: a file
    attached and a runtime that cannot start keep the draft, show the
    error on the runtime's card and beside the composer, and give the
    message and its file back; the next send creates the session, which
    shows the message and the file."""
    browser.resize(1280, 900)
    open_cards(browser)
    assert count(browser, "#start-session") == 0, "a Start button"
    uploads = tempfile.mkdtemp(prefix="cookie-web-e2e-")
    atexit.register(shutil.rmtree, uploads, True)
    picture = png(os.path.join(uploads, "draft.png"), 64, 48)
    browser.type("#attach-input", picture)
    browser.wait("the draft's chip", lambda: "draft.png" in browser.text("#draft-files"))
    sessions_before = count(browser, "aside [data-session-row]")
    assert browser.text("#session-title") == "New session", "attaching a file created the session"
    # `box` never starts: the daemon refuses the session.
    for name, is_open in browser.script(
        "return [...document.querySelectorAll('#new-session [data-group]')].map((g) => [g.dataset.group, g.dataset.open])"
    ):
        if is_open != "true":
            browser.click('#new-session [data-group-toggle="%s"]' % name)
    browser.wait("the box card", lambda: count(browser, card("box")) == 1)
    browser.click(card("box") + " [data-card-toggle]")
    set_text(browser, field("box"), WORKSPACE)
    browser.type("#composer", "the first message")
    browser.type("#composer", ENTER)
    browser.wait("the refusal on its card", lambda: browser.text(card("box") + " [data-card-error]") != "", timeout=40)
    browser.wait("the message back", lambda: browser.script("return document.querySelector('#composer').value") == "the first message")
    assert "draft.png" in browser.text("#draft-files"), browser.text("#draft-files")
    assert "box" in browser.text("#draft-error") and "see its card" in browser.text("#draft-error"), browser.text("#draft-error")
    assert browser.text("#session-title") == "New session" and count(browser, "#new-session") == 1
    assert count(browser, "aside [data-session-row]") == sessions_before, "a refused draft made a session"
    assert "box" in selected_cards(browser), selected_cards(browser)
    browser.shot("draft-refused")
    print("a runtime that cannot start keeps the draft and gives the message back:", browser.text(card("box") + " [data-card-error]"))
    # Without it, the same message creates the session, and shows with its
    # file.
    browser.click(card("box") + " [data-card-toggle]")
    select_local(browser)
    browser.type("#composer", ENTER)
    browser.wait("the session", lambda: browser.text("#session-title") != "New session" and answers(browser) >= 1, timeout=60)
    assert count(browser, "#new-session") == 0
    browser.wait("the message", lambda: "the first message" in browser.text("#transcript .user-message"))
    browser.wait("its picture", lambda: count(browser, "#transcript .user-message .media img") >= 1)
    assert count(browser, "#draft-files") == 0 and count(browser, "#draft-error") == 0
    assert count(browser, "aside [data-session-row]") == sessions_before + 1
    browser.shot("draft-created")
    print("the first message created the session and shows with its file")


# The open session's row.
OPEN_SESSION_ROW = "aside [data-session-row]:has([aria-current=true])"
# A row's tooltip: where it runs, with every path.
TOOLTIP = "return document.querySelector(arguments[0] + ' [data-session]').title"


def open_session_id(browser):
    return browser.script(
        "return document.querySelector('aside [data-session-row] [aria-current=true]').dataset.session"
    )


def badges(browser, row):
    """A session row's badges, as [kind, text] pairs."""
    return browser.script(
        "return [...document.querySelectorAll(arguments[0] + ' [data-session-badges] [data-badge]')]"
        ".filter((b) => !b.hidden).map((b) => [b.dataset.badge, b.textContent.trim()])",
        row,
    )


def badge_kinds(browser, row):
    return [kind for kind, _ in badges(browser, row)]


# The text colour of a row's badge for `arguments[1]`, and of the bottom
# bar's dot for it.
# Whether every shown machine badge of a row shows its whole name.
MACHINES_WHOLE = """
return [...document.querySelectorAll(arguments[0] + ' [data-badge=machine]')]
    .filter((b) => !b.hidden)
    .every((b) => b.scrollWidth <= b.clientWidth + 1 && !b.textContent.includes('…'));
"""
# A row's line at the sidebar's width now: whether its shown machine
# badges are whole and inside the line, whether a workspace is cut, and
# whether the time is whole.
LINE_STATE = """
const row = arguments[0];
const line = document.querySelector(row + ' [data-session-badges]');
const time = document.querySelector(row + ' [data-session-time]');
const edge = line.getBoundingClientRect().right;
const whole = [...line.querySelectorAll('[data-badge=machine]')]
    .filter((b) => !b.hidden)
    .every((b) => b.scrollWidth <= b.clientWidth + 1 && b.getBoundingClientRect().right <= edge + 1);
const pathCut = [...line.querySelectorAll('[data-badge=workspace]')]
    .some((e) => e.scrollWidth > e.clientWidth + 1);
return [whole, pathCut, time.scrollWidth <= time.clientWidth];
"""
BADGE_COLOUR = """
const badge = [...document.querySelectorAll(arguments[0] + ' [data-badge]')]
    .find((b) => b.textContent.trim().startsWith(arguments[1]));
return badge ? getComputedStyle(badge).color : null;
"""


def session_badges(browser):
    """Session rows say where they run as machine badges: none without a
    workspace, one machine and its workspaces, and one badge per machine
    once a runtime moves to another machine; a machine keeps its colour in
    every row, and its runtimes take it in the bottom bar and the runtime
    cards; and the badges truncate before the time does."""
    rows = {name: 'aside [data-session-row="%s"]' % session for name, session in MADE.items()}
    assert badge_kinds(browser, rows["none"]) == []
    assert badge_kinds(browser, rows["one"]) == ["machine", "workspace"]
    machine, _ = badges(browser, rows["one machine"])
    # The time is on the title's line; the badges have the second to themselves.
    assert browser.script(
        "const r = document.querySelector(arguments[0]);"
        "return r.querySelector('[data-session-time]').getBoundingClientRect().top"
        "  < r.querySelector('[data-session-badges]').getBoundingClientRect().top",
        rows["one machine"],
    )
    # The same machine, the same colour, in any row.
    desk_colour = browser.script(BADGE_COLOUR, rows["one"], machine[1])
    assert desk_colour and desk_colour == browser.script(BADGE_COLOUR, rows["one machine"], machine[1]), desk_colour
    # A runtime pinned to another machine: the session spans two.
    subprocess.run([COOKIE, "machine", "assign", "alt", "e2e-studio"], check=True, env=dict(os.environ, HOME=HOME),
                   stdout=subprocess.DEVNULL)
    browser.wait(
        "a badge per machine",
        lambda: badges(browser, rows["one machine"]) == [["machine", machine[1]], ["machine", "e2e-studio"]],
    )
    # Too narrow for both machines: the first stays whole, and +1 stands for
    # the other, which its tooltip names.
    hover(browser, "#transcript")
    browser.script("document.querySelector('#sidebar').style.width = '150px'")
    browser.wait("the +N badge", lambda: browser.script(
        "const m = document.querySelector(arguments[0] + ' [data-badge=more]');"
        "return !!m && !m.hidden ? [m.textContent, m.title] : null", rows["one machine"]) is not None)
    more, more_title = browser.script(
        "const m = document.querySelector(arguments[0] + ' [data-badge=more]'); return [m.textContent, m.title]",
        rows["one machine"],
    )
    assert re.fullmatch(r"\+[12]", more) and "e2e-studio" in more_title, (more, more_title)
    assert browser.script(MACHINES_WHOLE, rows["one machine"]), "a machine badge is cut"
    browser.shot("badges-more")
    browser.script("document.querySelector('#sidebar').style.width = ''")
    print("a line too narrow for its machines shows only whole ones, then", more, "for", more_title)
    # One attachment shows its machine, now the studio, and its workspace.
    browser.wait("alt on the studio", lambda: badges(browser, rows["one"]) == [["machine", "e2e-studio"], ["workspace", "~/elsewhere"]])
    alt_colour = browser.script(BADGE_COLOUR, rows["one"], "e2e-studio")
    assert alt_colour, alt_colour
    tooltip = browser.script(TOOLTIP, rows["one machine"])
    assert "e2e-studio · alt" in tooltip and "~/elsewhere" in tooltip, tooltip
    assert "%s · local · ~/workspace" % machine[1] in tooltip, tooltip
    # Machine badges carry the machine's OS icon.
    assert count(browser, rows["one machine"] + " [data-badge=machine] svg") == 2
    browser.shot("badges")
    # The bottom bar and the runtime cards mark alt in its colour.
    click_row(browser, rows["one"] + " [data-session]")
    browser.wait("the alt session", lambda: count(browser, '#places [data-runtime-dot="alt"]') == 1)
    dot = browser.script(
        "return getComputedStyle(document.querySelector('#places [data-runtime-dot=alt]')).backgroundColor"
    )
    assert dot == alt_colour, (dot, alt_colour)
    open_cards(browser)
    for name, is_open in browser.script(
        "return [...document.querySelectorAll('#new-session [data-group]')].map((g) => [g.dataset.group, g.dataset.open])"
    ):
        if is_open != "true":
            browser.click('#new-session [data-group-toggle="%s"]' % name)
    browser.wait("alt's card dot", lambda: count(browser, '#new-session [data-runtime-dot="alt"]') == 1)
    assert browser.script(
        "return getComputedStyle(document.querySelector('#new-session [data-runtime-dot=alt]')).backgroundColor"
    ) == alt_colour
    browser.shot("badges-cards")
    close_sheet(browser)
    click_row(browser, rows["one machine"] + " [data-session]")
    browser.wait("the session", lambda: count(browser, OPEN_SESSION_ROW) == 1)
    subprocess.run([COOKIE, "machine", "unassign", "alt"], check=True, env=dict(os.environ, HOME=HOME),
                   stdout=subprocess.DEVNULL)
    browser.wait("alt back on this machine", lambda: badge_kinds(browser, rows["one machine"]) == ["machine", "workspace"])
    # A narrowing sidebar: the workspaces give way first, down to "…";
    # a machine's name is never cut, and the time stays whole.
    hover(browser, "#transcript")
    cut_at = None
    for width in range(288, 119, -12):
        browser.script("document.querySelector('#sidebar').style.width = arguments[0] + 'px'", width)
        time.sleep(0.25)
        whole, path_cut, time_whole = browser.script(LINE_STATE, rows["one machine"])
        assert whole and time_whole, (width, whole, time_whole)
        if path_cut and cut_at is None:
            cut_at = width
            browser.shot("badges-narrow")
    assert cut_at is not None, "the workspaces were never cut"
    print("a narrowing sidebar cuts the workspaces from %d px; machine names and the time stay whole" % cut_at)
    browser.script("document.querySelector('#sidebar').style.width = ''")
    subprocess.run([COOKIE, "machine", "forget", "e2e-studio"], check=True, env=dict(os.environ, HOME=HOME),
                   stdout=subprocess.DEVNULL)
    print("badges: none, one machine and its workspaces, two machines; a machine keeps its colour everywhere; names stay whole")


# A row's pencil and trash: the row's middle and each button's, their
# sizes, the row's background and theirs, and the gap between them.
ROW_BUTTONS = """
const row = document.querySelector(arguments[0]);
const open = row.querySelector('[data-session]').getBoundingClientRect();
const pencil = row.querySelector('[data-rename]'), trash = row.querySelector('[data-delete]');
const p = pencil.getBoundingClientRect(), t = trash.getBoundingClientRect();
return [open.top + open.height / 2, p.top + p.height / 2, p.width, p.height, t.left - p.right,
    getComputedStyle(row.querySelector('[data-session]')).backgroundColor,
    getComputedStyle(pencil).backgroundColor];
"""


def renaming(browser):
    """Renaming a session: the title bar's title edits in place (Enter
    saves, Escape keeps it), ⋮ › Rename does the same, the row's pencil
    beside its trash turns the row's title into a field, and an emptied
    field offers the automatic title."""
    session = open_session_id(browser)
    row = 'aside [data-session-row="%s"]' % session
    before = browser.text("#session-title")
    # The title is a button; a click makes it a field holding the title.
    browser.click("#session-title [data-title-edit]")
    browser.wait("the title field", lambda: count(browser, "#session-title-field") == 1)
    # It holds the title; an untitled session's is empty.
    assert browser.script("return document.querySelector('#session-title-field').value") in (before, "")
    # The field takes focus a frame after it appears.
    browser.wait("the title field's focus", lambda: browser.script(
        "return document.activeElement.id") == "session-title-field")
    browser.type("#session-title-field", "Scratch" + ESCAPE)
    browser.wait("the title again", lambda: count(browser, "#session-title-field") == 0)
    assert browser.text("#session-title") == before
    browser.click("#session-title [data-title-edit]")
    browser.wait("the title field", lambda: count(browser, "#session-title-field") == 1)
    set_text(browser, "#session-title-field", "Badges and titles")
    browser.shot("rename-title-bar")
    browser.type("#session-title-field", ENTER)
    browser.wait("the new title", lambda: browser.text("#session-title") == "Badges and titles")
    browser.wait("the row's new title", lambda: browser.text(row + " [data-session-title]") == "Badges and titles")
    print("renamed the session from its title bar")
    # ⋮ › Rename opens the same field; emptied, it offers the automatic title.
    menu(browser, "rename")
    browser.wait("the title field", lambda: count(browser, "#session-title-field") == 1)
    assert count(browser, "[data-title-reset]") == 0
    set_text(browser, "#session-title-field", "")
    browser.wait("the automatic title button", lambda: count(browser, "[data-title-reset]") == 1)
    browser.shot("rename-title-bar-empty")
    browser.click("[data-title-reset]")
    browser.wait("the automatic title", lambda: browser.text("#session-title") not in ("Badges and titles", ""))
    print("⋮ › Rename and the automatic title:", browser.text("#session-title"))

    # The row's pencil sits left of its trash, centred, in the hovered
    # row's colour.
    hover(browser, "#transcript")
    assert not shown(browser, row + " [data-rename]"), "the pencil shows without a hover"
    hover(browser, row)
    browser.wait("the pencil on hover", lambda: shown(browser, row + " [data-rename]"))
    middle, pencil_middle, width, height, gap, row_colour, pencil_colour = browser.script(ROW_BUTTONS, row)
    assert abs(middle - pencil_middle) <= 1, "the pencil is %s px off the row's middle" % (pencil_middle - middle)
    assert width >= 44 and height >= 44, "the pencil is %sx%s px" % (width, height)
    assert 0 <= gap <= 8, "the pencil is %s px from the trash" % gap
    assert row_colour == pencil_colour, "the pencil is %s over a %s row (%s)" % (
        pencil_colour, row_colour, browser.script("return document.querySelector(arguments[0] + ' [data-session]').className", row))
    original = browser.text(row + " [data-session-title]")
    browser.click(row + " [data-rename]")
    field = row + " [data-rename-field]"
    browser.wait("the row's field", lambda: count(browser, field) == 1)
    browser.wait("the field's focus", lambda: browser.script(
        "return document.activeElement === document.querySelector(arguments[0])", field))
    browser.type(field, "Nope" + ESCAPE)
    browser.wait("the row again", lambda: count(browser, field) == 0)
    assert browser.text(row + " [data-session-title]") == original
    hover(browser, row)
    browser.wait("the pencil on hover", lambda: shown(browser, row + " [data-rename]"))
    browser.click(row + " [data-rename]")
    browser.wait("the row's field", lambda: count(browser, field) == 1)
    set_text(browser, field, "Renamed from its row")
    browser.shot("rename-row")
    browser.type(field, ENTER)
    browser.wait("the row's new title", lambda: browser.text(row + " [data-session-title]") == "Renamed from its row")
    browser.wait("the title bar's", lambda: browser.text("#session-title") == "Renamed from its row")
    # Emptied, the row offers the automatic title.
    hover(browser, row)
    browser.wait("the pencil on hover", lambda: shown(browser, row + " [data-rename]"))
    browser.click(row + " [data-rename]")
    browser.wait("the row's field", lambda: count(browser, field) == 1)
    set_text(browser, field, "")
    browser.wait("the automatic title button", lambda: count(browser, row + " [data-rename-reset]") == 1)
    browser.shot("rename-row-empty")
    browser.click(row + " [data-rename-reset]")
    browser.wait("the automatic title", lambda: browser.text(row + " [data-session-title]") != "Renamed from its row")
    print("renamed the session from its row's pencil, and back to its automatic title")


def desktop_layout(browser):
    """The rail's views, the session menu, the sidebar toggle, the theme
    toggle, and the bottom bar's pickers."""
    # The rail switches views and marks the one shown.
    for name in ("agents", "settings", "machines", "memory", "cron", "chat"):
        view(browser, name)
        current = browser.script(
            "return document.querySelector('[data-rail=primary] [aria-current=page]').dataset.view"
        )
        assert current == name, (current, name)
        assert shown(browser, "#composer") == (name == "chat"), "the composer outside Chat"
    unnamed = browser.script(
        "return [...document.querySelectorAll('[data-rail=primary] button')]"
        ".filter((b) => !b.getAttribute('aria-label') || !(b.dataset.tip || b.title)).length"
    )
    assert unnamed == 0, "%s rail buttons lack a name or a tooltip" % unnamed
    print("the rail switches views and marks the current one")
    browser.shot("rail-settings")

    # The ⋮ menu: every session command by name, icon, and description, no
    # slash syntax; each opens a sheet.
    new_session(browser)
    browser.click("button[aria-label=Commands]")
    browser.wait("the session menu", lambda: count(browser, "#sheet [data-command]") > 0)
    entries = browser.script(
        "return [...document.querySelectorAll('#sheet [data-command]')].map((b) =>"
        " [b.dataset.command, b.innerText, b.querySelector('svg') !== null])"
    )
    names = [entry[0] for entry in entries]
    assert names == [
        "rename", "compact", "goal", "schedule", "skills", "background", "runtime", "permissions", "usage",
        "events", "preset",
    ], names
    for name, text, icon in entries:
        assert icon and "/" not in text and "\n" in text, (name, text, icon)
    browser.shot("session-menu")
    close_sheet(browser)
    click_row(browser, "aside nav[aria-label=Sessions] [data-session]")
    browser.wait("an open session", lambda: browser.text("#session-title") != "New session")
    # Rename edits the title in place (renaming), and Schedule a prompt
    # opens the job form (cron); the others open sheets.
    for name in [name for name in names[1:] if name != "schedule"]:
        menu(browser, name)
        browser.wait("the %s sheet" % name, lambda: count(browser, "#sheet [data-command]") == 0 and count(browser, "#sheet") == 1)
        assert page_view(browser) == "chat"
        if name == "usage":
            figures = cache_hit_rate(browser)
            browser.shot("usage-sheet")
            print("the usage sheet shows the prompt-cache hit rate:", figures["cache hit rate"])
        close_sheet(browser)
    print("the ⋮ menu lists the session's commands by name, and each opens its sheet")

    # The sidebar toggle hides the sessions on a desktop, and the browser
    # remembers it.
    assert shown(browser, "#sidebar")
    browser.click("main [data-sidebar-toggle]")
    browser.wait("the hidden sidebar", lambda: not shown(browser, "#sidebar"))
    browser.go(LINK)
    browser.wait("the connection", lambda: browser.text("#connected") != "")
    time.sleep(0.5)
    assert not shown(browser, "#sidebar"), "the hidden sidebar came back on a reload"
    assert browser.script("return document.querySelector('main [data-sidebar-toggle]').getAttribute('aria-expanded')") == "false"
    browser.shot("sidebar-hidden")
    browser.click("main [data-sidebar-toggle]")
    browser.wait("the sidebar", lambda: shown(browser, "#sidebar"))
    browser.go(LINK)
    browser.wait("the connection", lambda: browser.text("#connected") != "")
    browser.wait("the sidebar after a reload", lambda: shown(browser, "#sidebar"))
    print("the sidebar toggles and the choice outlasts a reload")

    # The theme button at the rail's foot cycles system, light, and dark.
    theme = "[data-rail=primary] button[aria-label=Theme]"
    assert browser.script(
        "const r = document.querySelector('[data-rail=primary]').getBoundingClientRect();"
        "const t = document.querySelector(arguments[0]).getBoundingClientRect();"
        "return r.bottom - t.bottom < 24",
        theme,
    ), "the theme button is not at the rail's foot"
    seen = []
    for _ in range(3):
        browser.click(theme)
        name = browser.script("return document.querySelector(arguments[0]).dataset.theme", theme)
        dark = browser.script("return document.documentElement.classList.contains('dark')")
        if name in ("light", "dark"):
            assert dark == (name == "dark"), (name, dark)
        seen.append(name)
    assert sorted(seen) == ["dark", "light", "system"], seen
    assert count(browser, "header button[aria-label=Theme]") == 0, "the header kept a theme button"
    print("the rail's theme button cycles", " → ".join(seen))

    # The bottom bar's pickers.
    click_row(browser, "aside nav[aria-label=Sessions] [data-session]")
    browser.wait("an open session", lambda: browser.text("#session-title") != "New session")
    browser.click("#agent-chip")
    browser.wait("the agent picker", lambda: "primary" in browser.text("#sheet"))
    browser.shot("agent-picker")
    browser.click("#sheet h2 ~ button")
    browser.click("#model-chip")
    browser.wait("the model picker", lambda: "Scripted Model" in browser.text("#sheet"))
    click_row_button(browser, "#sheet .flex.flex-col", "Scripted Model", "button")
    browser.wait("the picked model", lambda: count(browser, "#sheet") == 0)
    assert "Scripted Model" in browser.text("#model-chip"), browser.text("#model-chip")
    # The mode chip opens a picker of every mode; a click sets one.
    assert count(browser, "#permission-mode svg") == 2, "the mode chip has no chevron"
    for mode in ("auto-approve-n", "auto-approve-y", "ask", "yolo", "auto-approve"):
        browser.click("#permission-mode")
        browser.wait("the mode picker", lambda: count(browser, "#sheet [data-mode]") == 5)
        assert "loop guard" in browser.text("#sheet"), browser.text("#sheet")
        if mode == "auto-approve-n":
            browser.shot("mode-picker")
        browser.click('#sheet [data-mode="%s"]' % mode)
        browser.wait("the %s mode" % mode, lambda: count(browser, "#sheet") == 0)
        browser.wait(
            "the chip's %s" % mode,
            lambda: browser.script("return document.querySelector('#permission-mode').getAttribute('aria-label')")
            == "Permission mode: %s" % mode,
        )
        browser.click("#permission-mode")
        browser.wait("the picker again", lambda: count(browser, "#sheet [data-mode]") == 5)
        assert browser.script(
            "return document.querySelector('#sheet [data-mode=\"' + arguments[0] + '\"]').getAttribute('aria-pressed')", mode
        ) == "true", "%s is not checked" % mode
        close_sheet(browser)
    assert count(browser, "#mode-toast") == 0
    print("the bottom bar's agent, model, and mode pickers work")


def phone_layout():
    """The phone's drawer, Back, the bottom sheets, the mode picker, and the
    bottom bar that gives way to the keyboard."""
    phone = Browser()
    try:
        phone.go(LINK)
        phone.wait("the connection", lambda: phone.text("#connected") != "")
        phone.phone(390, 844)
        composer_clear(phone, "phone")
        phone.shot("phone-fresh-connect")
        assert not shown(phone, "[data-rail=primary]"), "a phone shows the rail outside the drawer"
        # The drawer: the rail beside the sessions, over the dimmed chat.
        phone.click("main [data-sidebar-toggle]")
        phone.wait("the drawer", lambda: shown(phone, "#drawer [data-rail=drawer]"))
        width = phone.script("return document.querySelector('#drawer').getBoundingClientRect().width")
        assert 300 <= width <= 340, "the drawer is %s px wide" % width
        phone.shot("phone-drawer")
        phone.script("history.back()")
        phone.wait("Back closing the drawer", lambda: count(phone, "#drawer") == 0)

        # A rail button whose view has a list, its second level, shows the
        # list first: the agents and the memories fill the page, Chat's
        # sessions stay in the drawer. Choosing an item opens its detail,
        # whose back arrow (and browser Back) returns to the list.
        agent = '#agents-sidebar [data-config-path="agents/helper.md"]'
        phone.click("main [data-sidebar-toggle]")
        phone.click("#drawer [data-view=agents]")
        phone.wait(
            "the agents",
            lambda: page_view(phone) == "agents" and count(phone, "#drawer") == 0 and shown(phone, agent),
        )
        assert count(phone, "#config-text") == 0 and not shown(phone, "[data-agents-empty]"), "the rail opened an agent"
        assert phone.text("#page-title") == "Agents" and not shown(phone, "[data-page-back]")
        phone.shot("phone-rail-agents")
        phone.click(agent + " button")
        phone.wait("the agent", lambda: shown(phone, "#config-text") and not shown(phone, agent))
        assert shown(phone, "[data-page-back]"), "the agent has no back arrow"
        phone.shot("phone-rail-agents-open")
        phone.script("history.back()")
        phone.wait("Back to the agents", lambda: count(phone, "#config-text") == 0 and shown(phone, agent))
        phone.click(agent + " button")
        phone.wait("the agent again", lambda: shown(phone, "#config-text"))
        phone.click("[data-page-back]")
        phone.wait("the back arrow to the agents", lambda: count(phone, "#config-text") == 0 and shown(phone, agent))

        phone.click("#page [data-sidebar-toggle]")
        phone.click("#drawer [data-view=memory]")
        phone.wait(
            "the memories",
            lambda: page_view(phone) == "memory"
            and count(phone, "#drawer") == 0
            and "Reading memories" not in phone.text("#page"),
        )
        assert count(phone, "#memory-text") == 0, "the rail opened a memory"
        phone.shot("phone-rail-memory")
        # Run on its own, the daemon may have no memory to open yet.
        if count(phone, "#page .memory-row button") > 0:
            phone.click("#page .memory-row button")
            phone.wait("the memory", lambda: count(phone, "#memory-text") == 1)
            phone.click("[data-page-back]")
            phone.wait("Back to the memories", lambda: count(phone, "#memory-text") == 0 and count(phone, "#page .memory-row") > 0)

        # Machines has no list: its page at once.
        phone.click("#page [data-sidebar-toggle]")
        phone.click("#drawer [data-view=machines]")
        phone.wait("the machines", lambda: page_view(phone) == "machines" and count(phone, "#drawer") == 0)

        phone.click("#page [data-sidebar-toggle]")
        phone.click("#drawer [data-view=chat]")
        phone.wait(
            "the sessions",
            lambda: page_view(phone) == "chat" and count(phone, "#drawer nav[aria-label=Sessions] [data-session]") > 0,
        )
        phone.shot("phone-rail-chat")
        phone.script("history.back()")
        phone.wait("Back closing the sessions", lambda: count(phone, "#drawer") == 0 and page_view(phone) == "chat")
        print("phone: a rail button shows its view's list first; Back from an item returns to it")

        # Settings' list is its page too: no section until one is chosen.
        phone.click("main [data-sidebar-toggle]")
        phone.click("#drawer [data-view=settings]")
        phone.wait("the settings view", lambda: page_view(phone) == "settings" and count(phone, "#drawer") == 0)
        assert shown(phone, "[data-section=providers]") and count(phone, "[data-section-body]") == 0, "the rail opened a section"
        # List, then detail; Back closes the drawer, then leaves the detail.
        phone.click("#page [data-section=configuration]")
        phone.wait("the section", lambda: shown(phone, "[data-section-body=configuration]"))
        assert not shown(phone, "[data-section=providers]"), "the list stays beside the detail"
        assert phone.text("#page-title") == "Configuration", phone.text("#page-title")
        phone.click("#page [data-sidebar-toggle]")
        phone.wait("the drawer", lambda: count(phone, "#drawer") == 1)
        phone.script("history.back()")
        phone.wait("Back closing the drawer", lambda: count(phone, "#drawer") == 0)
        assert shown(phone, "[data-section-body=configuration]"), "Back left the section with the drawer"
        phone.script("history.back()")
        phone.wait("Back leaving the section", lambda: shown(phone, "[data-section=providers]"))
        # The title bar's back arrow does the same; an editor fills the
        # screen with its actions at the foot.
        phone.click("#page [data-section=configuration]")
        phone.click('#page [data-config-path="config.toml"] button')
        phone.wait("the editor", lambda: count(phone, "#config-text") == 1)
        bottom = phone.script("return innerHeight - document.querySelector('#config-apply').getBoundingClientRect().bottom")
        assert 0 <= bottom < 70, "the editor's actions are %s px above its foot" % bottom
        phone.shot("phone-editor")
        phone.click("[data-page-back]")
        phone.wait("the closed editor", lambda: count(phone, "#config-text") == 0)
        phone.click("[data-page-back]")
        phone.wait("the section list", lambda: shown(phone, "[data-section=providers]"))
        print("phone: the drawer opens and switches views; Back closes it, then leaves a detail")

        # A subagent's title bar: ‹ goes back to its parent.
        phone.click("#page [data-sidebar-toggle]")
        phone.click("#drawer [data-new-session]")
        phone.wait("the new session", lambda: page_view(phone) == "chat" and count(phone, "#drawer") == 0)
        # The cards stack in one column above the composer, which stays at
        # the foot.
        phone.wait("the cards", lambda: count(phone, "#new-session [data-card]") > 1 and count(phone, "#composer") == 1)
        phone.click(card("local/coding") + " [data-card-toggle]")
        phone.wait("the workspace field", lambda: count(phone, card("local/coding") + " [data-workspace-field]") == 1)
        lefts = phone.script(
            "return [...new Set([...document.querySelectorAll('#new-session [data-card]')]"
            ".map((c) => Math.round(c.getBoundingClientRect().left)))]"
        )
        assert len(lefts) == 1, "the cards are not one column: %s" % lefts
        foot = phone.script(
            "return innerHeight - document.querySelector('#composer-box').getBoundingClientRect().bottom"
        )
        assert 0 <= foot < 120, "the composer is %s px above the foot" % foot
        heights = phone.script(
            "return [...document.querySelectorAll('#new-session [data-workspace-field]')].map((f) => f.getBoundingClientRect().height)"
        )
        assert heights and min(heights) >= 44, heights
        phone.shot("phone-cards")
        print("phone: the cards stack in one column above the composer")
        select_local(phone)
        send(phone, "delegate the work")
        phone.wait("the delegation", lambda: "The tool ran." in phone.text("#transcript"), timeout=60)
        root_title = phone.text("#session-title")
        phone.click("#transcript [data-delegate] [data-action=open-session]")
        phone.wait("the child", lambda: "helper" in phone.text("#session-title"))
        assert shown(phone, "[data-crumb-back]") and not shown(phone, "main nav[aria-label=Path]")
        phone.shot("phone-subagent")
        phone.click("[data-crumb-back]")
        phone.wait("the parent", lambda: phone.text("#session-title") == root_title)
        print("phone: a subagent's ‹ goes back to its parent")

        # A long press on a session row offers to delete it, and never opens
        # it; a scroll cancels the press.
        phone.click("main [data-sidebar-toggle]")
        phone.click("#drawer [data-new-session]")
        select_local(phone)
        send(phone, "hello there")
        phone.wait("the answer", lambda: answers(phone) >= 1)
        doomed = phone.script(
            "return document.querySelector('aside [data-session-row] [aria-current=true]').dataset.session"
        )
        phone.click("main [data-sidebar-toggle]")
        click_row(phone, "#drawer nav[aria-label=Sessions] [data-session]:not([aria-current])")
        phone.wait("another session", lambda: count(phone, "#drawer") == 0 and phone.text("#transcript") != "")
        opened = phone.text("#session-title")
        phone.click("main [data-sidebar-toggle]")
        row = '#drawer [data-session="%s"]' % doomed
        phone.wait("the drawer", lambda: count(phone, row) == 1)
        assert not shown(phone, '#drawer [data-delete="%s"]' % doomed), "the hover trash shows on a phone"
        long_press(phone, row, drag=40)
        time.sleep(0.3)
        assert count(phone, "#drawer [data-delete-actions]") == 0, "a scroll still revealed the delete"
        long_press(phone, row)
        phone.wait("the delete action", lambda: count(phone, "#drawer [data-delete-actions]") == 1)
        assert count(phone, "#drawer") == 1 and phone.text("#session-title") == opened, "the long press opened the session"
        phone.shot("phone-delete-revealed")
        # The long press offers Rename too, which makes the title a field.
        phone.click("#drawer [data-rename-reveal]")
        phone.wait("the row's field", lambda: count(phone, '#drawer [data-rename-field="%s"]' % doomed) == 1)
        phone.shot("phone-rename-row")
        phone.type('#drawer [data-rename-field="%s"]' % doomed, "Doomed" + ENTER)
        phone.wait("the renamed row", lambda: phone.text('#drawer [data-session-row="%s"] [data-session-title]' % doomed) == "Doomed")
        long_press(phone, row)
        phone.wait("the delete action", lambda: count(phone, "#drawer [data-delete-actions]") == 1)
        phone.click("#drawer [data-delete-reveal]")
        phone.wait("the confirmation", lambda: count(phone, "#drawer [data-delete-confirm-box]") == 1)
        phone.shot("phone-delete-confirm")
        phone.click('#drawer [data-delete-confirm="%s"]' % doomed)
        phone.wait("the deleted row", lambda: count(phone, row) == 0, timeout=30)
        print("phone: a long press offers the delete without opening the session; a scroll cancels it")
        phone.script("document.querySelector('[data-drawer-backdrop]').click()")

        # Back to Chat and the latest session, from the drawer. A tap on the
        # open session's own row switches its subagent tree instead, and
        # leaves the drawer open.
        phone.click("main [data-sidebar-toggle]")
        phone.wait("the drawer", lambda: count(phone, "#drawer nav[aria-label=Sessions] [data-session]") > 0)
        latest = "#drawer nav[aria-label=Sessions] [data-session]"
        if phone.script("return document.querySelector(arguments[0]).getAttribute('aria-current')", latest) == "true":
            phone.script("document.querySelector('[data-drawer-backdrop]').click()")
        else:
            click_row(phone, latest)
        phone.wait("the transcript", lambda: page_view(phone) == "chat" and phone.text("#transcript") != "")
        phone.wait("the closed drawer", lambda: count(phone, "#drawer") == 0)
        # One row of 44 px targets.
        tops = phone.script(
            "return [...document.querySelectorAll('#status-bar > *')]"
            ".filter((e) => e.getClientRects().length).map((e) => Math.round(e.getBoundingClientRect().top))"
        )
        assert len(set(tops)) == 1, "the bottom bar wraps: %s" % tops
        # A picker rises from the bottom.
        phone.click("#model-chip")
        phone.wait("the model sheet", lambda: "Scripted Model" in phone.text("#sheet"))
        rect = phone.script(
            "const r = document.querySelector('#sheet').getBoundingClientRect(); return [r.top, r.bottom, innerHeight]"
        )
        assert rect[0] > 40 and abs(rect[1] - rect[2]) <= 2, "the sheet is not a bottom sheet: %s" % rect
        phone.shot("phone-bottom-sheet")
        close_sheet(phone)
        # The mode chip is a shield that opens a bottom sheet of the modes.
        phone.click("#permission-mode")
        phone.wait("the mode sheet", lambda: count(phone, "#sheet [data-mode]") == 5)
        rect = phone.script(
            "const r = document.querySelector('#sheet').getBoundingClientRect(); return [r.top, r.bottom, innerHeight]"
        )
        assert abs(rect[1] - rect[2]) <= 2, "the mode picker is not a bottom sheet: %s" % rect
        phone.shot("phone-mode-picker")
        phone.click('#sheet [data-mode="ask"]')
        phone.wait("the ask mode", lambda: "ask" in phone.script("return document.querySelector('#permission-mode').getAttribute('aria-label')"))
        phone.click("#permission-mode")
        phone.click('#sheet [data-mode="auto-approve"]')
        phone.wait("the default mode", lambda: count(phone, "#sheet") == 0)
        # The ⋮ menu and its sheets rise from the bottom too.
        for name, field in (("compact", "#compact-focus"), ("goal", "#goal-input")):
            menu(phone, name)
            phone.wait("the %s sheet" % name, lambda: count(phone, field) == 1)
            rect = phone.script(
                "const r = document.querySelector('#sheet').getBoundingClientRect(); return [r.top, r.bottom, innerHeight]"
            )
            assert abs(rect[1] - rect[2]) <= 2, "the %s sheet is not a bottom sheet: %s" % (name, rect)
            phone.shot("phone-%s-sheet" % name)
            close_sheet(phone)
        menu(phone, "usage")
        cache_hit_rate(phone)
        phone.shot("phone-usage-sheet")
        close_sheet(phone)
        print("phone: the bottom bar is one row; pickers and the menu's sheets are bottom sheets")
        # The keyboard's room: the bar hides while the composer has focus.
        phone.script("document.querySelector('#composer').focus()")
        phone.wait("the hidden bar", lambda: not shown(phone, "#status-bar"))
        phone.script("document.querySelector('#composer').blur()")
        phone.wait("the bar again", lambda: shown(phone, "#status-bar"))
        print("phone: the bottom bar gives way to the composer's keyboard")
    finally:
        phone.close()


PHONES = [("phone-360", 360, 640), ("phone-390", 390, 844), ("phone-landscape", 844, 390)]

# Every interactive element the page shows (except text links inside
# rendered text) that the screen's edge cuts off or that is under the 44 px
# a finger needs.
SMALL_TARGETS = """
const small = [];
for (const element of document.querySelectorAll(
    'button, select, textarea, input:not([type=hidden]):not([type=file]), summary, a[href]')) {
  if (element.closest('.markdown') || element.offsetParent === null) continue;
  const rect = element.getBoundingClientRect();
  if (rect.width === 0 || rect.height === 0) continue;
  // A control smaller than a finger may reach further with an invisible
  // ::before (.hit-area): measure what a finger hits.
  let hit = rect;
  if (element.classList.contains('hit-area')) {
    const before = getComputedStyle(element, '::before');
    const reach = (side) => Math.max(0, -parseFloat(before[side]) || 0);
    hit = {
      width: rect.width + reach('left') + reach('right'),
      height: rect.height + reach('top') + reach('bottom'),
    };
  }
  // Cut off by the edge of the screen, which `overflow: hidden` hides.
  if (rect.right > innerWidth + 1 || rect.left < -1) {
    small.push('cut off: ' + element.tagName.toLowerCase() + ' "'
      + (element.innerText || element.getAttribute('aria-label') || '').slice(0, 30) + '"');
    continue;
  }
  const tooShort = hit.height < 44;
  const tooNarrow = element.tagName === 'BUTTON' && hit.width < 44;
  if (tooShort || tooNarrow) {
    small.push(element.tagName.toLowerCase() + (element.id ? '#' + element.id : '') + ' "'
      + (element.innerText || element.value || element.getAttribute('aria-label') || '').slice(0, 30)
      + '" ' + Math.round(hit.width) + 'x' + Math.round(hit.height));
  }
}
return [...new Set(small)];
"""

OVERFLOW = """
const page = document.documentElement.scrollWidth - document.documentElement.clientWidth;
let inner = 0;
for (const id of ['#sheet', '#page [data-page-body]', '#drawer nav[aria-label=Sessions]']) {
  for (const e of document.querySelectorAll(id)) inner = Math.max(inner, e.scrollWidth - e.clientWidth);
}
return Math.max(page, inner);
"""

# What the audit opens at each size: the chat's sheets by their commands,
# the ⋮ menu and the drawer by a click, then every view, Settings section,
# and editor by a script click (the rail is in the drawer on a phone).
SHEETS = [
    ("agent", ["#agent-chip"]),
    ("model", ["#model-chip"]),
    ("modes", ["#permission-mode"]),
] + [
    (name, ["main button[aria-label=Commands]", "#sheet [data-command=%s]" % name])
    for name in ("compact", "goal", "skills", "background", "runtime", "permissions", "usage", "events", "preset")
]
SCRIPTED = [
    ("view-%s" % name, ["[data-rail=primary] [data-view=%s]" % name])
    for name in ("agents", "settings", "machines", "memory", "cron")
] + [
    ("cron-form", ["[data-rail=primary] [data-view=cron]", "#cron-new"]),
] + [
    ("new-session-cards", ["[data-new-session]"]),
] + [
    ("settings-%s" % section, ["[data-rail=primary] [data-view=settings]", "[data-section=%s]" % section])
    for section in ("configuration", "providers", "mcp", "plugins", "browser")
] + [
    ("editor-config", [
        "[data-rail=primary] [data-view=settings]", "[data-section=configuration]",
        '[data-config-path="config.toml"] button',
    ]),
    ("editor-agent", ["[data-rail=primary] [data-view=agents]", '[data-config-path^="agents/"] button']),
    ("editor-memory", ["[data-rail=primary] [data-view=memory]", "#page .memory-row button"]),
    ("agents-open", ["[data-rail=primary] [data-view=agents]", '[data-config-path="agents/helper.md"] button']),
    ("agents-drawer", ["[data-rail=primary] [data-view=agents]", "#page [data-sidebar-toggle]"]),
    ("machines-add", ["[data-rail=primary] [data-view=machines]", "#machines-add"]),
    ("machines-move", ["[data-rail=primary] [data-view=machines]", '#page [data-machine-action="move to machine"]']),
    ("memory-agent", ["[data-rail=primary] [data-view=memory]", '[data-memory="agent://helper"] button']),
]


# Whether the page has settled: no finite animation runs and nothing in the
# DOM changed for 300 ms (the first call starts watching).
SETTLED = """
if (window.__lastChange === undefined) {
    window.__lastChange = performance.now();
    new MutationObserver(() => { window.__lastChange = performance.now(); })
        .observe(document, {subtree: true, childList: true, attributes: true, characterData: true});
}
const moving = document.getAnimations().some((a) =>
    a.playState === 'running' && a.effect && a.effect.getComputedTiming().iterations !== Infinity);
return !moving && performance.now() - window.__lastChange >= 300;
"""


def settle(browser, timeout=5):
    """Waits at least 0.3 s, then until the page has settled (SETTLED), for
    at most `timeout` s."""
    time.sleep(0.3)
    deadline = time.time() + timeout
    while not browser.script(SETTLED) and time.time() < deadline:
        time.sleep(0.1)


def phone_audit():
    """Every view, sheet, editor, and the drawer at phone sizes: no
    sideways scroll and touch targets of at least 44 px. Returns what falls
    short."""
    problems = []
    phone = Browser()
    try:
        for name, width, height in PHONES:
            phone.go(LINK)
            phone.wait("the connection", lambda: phone.text("#connected") != "")
            phone.phone(width, height)
            # The most recent session, with tool calls, media, and producers,
            # from the drawer (a phone's) or the sessions over the transcript
            # (a phone on its side).
            phone.click("main [data-sidebar-toggle]")
            phone.wait("the drawer", lambda: count(phone, "#drawer nav[aria-label=Sessions] [data-session]") > 0)
            latest = "#drawer nav[aria-label=Sessions] [data-session]"
            if phone.script("return document.querySelector(arguments[0]).getAttribute('aria-current')", latest) == "true":
                # Already open: a tap would switch its tree.
                phone.script("document.querySelector('[data-drawer-backdrop]').click()")
            else:
                click_row(phone, latest)
            phone.wait("the transcript", lambda: phone.text("#transcript") != "")
            settle(phone)
            views = [("transcript", None)] + SHEETS
            views += [("commands", "menu"), ("drawer", "drawer"), ("subagents", "subagents")] + SCRIPTED
            for view, command in views:
                close_sheet(phone)
                if command == "menu":
                    phone.click("button[aria-label=Commands]")
                elif command == "drawer":
                    phone.click("main [data-sidebar-toggle]")
                elif command == "subagents":
                    # The drawer with the open session's tree expanded.
                    phone.click("main [data-sidebar-toggle]")
                    settle(phone)
                    phone.script(
                        "const row = document.querySelector('#drawer [data-session][aria-current=true]');"
                        "if (row && row.getAttribute('aria-expanded') !== 'true') row.click()"
                    )
                elif isinstance(command, list):
                    for selector in command:
                        settle(phone)
                        phone.script("document.querySelector(arguments[0]).click()", selector)
                elif command:
                    send(phone, command)
                settle(phone)
                overflow = phone.script(OVERFLOW)
                small = phone.script(SMALL_TARGETS)
                if view == "transcript" and not bubble_aligned(phone):
                    problems.append("%s: the user's bubble is not at the right: %s" % (name, phone.script(BUBBLE)))
                if overflow > 0:
                    problems.append("%s %s: scrolls sideways by %s px" % (name, view, overflow))
                for target in small:
                    problems.append("%s %s: small target %s" % (name, view, target))
                phone.shot("%s-%s" % (name, view))
            close_sheet(phone)
    finally:
        phone.close()
    return problems


# Counts the page's popstate listeners as they come and go from now on.
COUNT_POPSTATE = """
window.popstateListeners = 0;
const add = window.addEventListener.bind(window);
const remove = window.removeEventListener.bind(window);
window.addEventListener = (type, ...rest) => {
  if (type === 'popstate') window.popstateListeners += 1;
  return add(type, ...rest);
};
window.removeEventListener = (type, ...rest) => {
  if (type === 'popstate') window.popstateListeners -= 1;
  return remove(type, ...rest);
};
"""


def reconnects():
    """The connected page remounts with every reconnection; its browser
    listeners must go with it. After several reconnections the page has
    no more popstate listeners than before, and Back moves exactly one
    step: from an agent to the agents' list, not further."""
    phone = Browser()
    try:
        phone.go(LINK)
        phone.wait("the connection", lambda: phone.text("#connected") != "")
        phone.phone(390, 844)
        with open(os.path.join(HOME, "daemon.pid")) as file:
            daemon = int(file.read())
        visibility = (
            "Object.defineProperty(document, 'visibilityState', {configurable: true, get: () => arguments[0]});"
            "document.dispatchEvent(new Event('visibilitychange'))"
        )
        phone.script(COUNT_POPSTATE)
        for round in range(1, 4):
            os.kill(daemon, signal.SIGSTOP)
            try:
                phone.script(visibility, "hidden")
                phone.script(visibility, "visible")
                phone.wait("the reconnect", lambda: "stopped answering" in phone.text("#reconnecting"), timeout=20)
            finally:
                os.kill(daemon, signal.SIGCONT)
            phone.wait("the reconnection", lambda: phone.text("#connected") != "", timeout=60)
            print("reconnection", round)
        listeners = phone.script("return window.popstateListeners")
        assert listeners == 0, "%s popstate listeners were added and not removed" % listeners
        agent = '#agents-sidebar [data-config-path="agents/helper.md"]'
        phone.click("main [data-sidebar-toggle]")
        phone.click("#drawer [data-view=agents]")
        phone.wait("the agents", lambda: page_view(phone) == "agents" and shown(phone, agent))
        phone.click(agent + " button")
        phone.wait("the agent", lambda: shown(phone, "#config-text") and not shown(phone, agent))
        phone.script("history.back()")
        phone.wait("Back to the agents", lambda: count(phone, "#config-text") == 0 and shown(phone, agent))
        time.sleep(1)
        assert page_view(phone) == "agents" and shown(phone, agent), "Back went past the agents' list"
        phone.check()
        print("after reconnections, Back moves one step and no listener is left behind")
    finally:
        phone.close()
    return []


def suspended_tab(browser):
    """A phone suspends a background tab; when it shows again the page
    checks its connection. A daemon that stopped answering (here, paused)
    is reconnected to once it answers again."""
    with open(os.path.join(HOME, "daemon.pid")) as file:
        daemon = int(file.read())
    visibility = (
        "Object.defineProperty(document, 'visibilityState', {configurable: true, get: () => arguments[0]});"
        "document.dispatchEvent(new Event('visibilitychange'))"
    )
    os.kill(daemon, signal.SIGSTOP)
    try:
        browser.script(visibility, "hidden")
        browser.script(visibility, "visible")
        browser.wait("the reconnect", lambda: "stopped answering" in browser.text("#reconnecting"), timeout=20)
        browser.shot("suspended-reconnecting")
    finally:
        os.kill(daemon, signal.SIGCONT)
    browser.wait("the reconnection", lambda: browser.text("#connected") != "", timeout=60)
    browser.wait("the restored session", lambda: "reconnected to the daemon" in browser.text("#status"), timeout=30)
    print("a tab shown again after the daemon stopped answering reconnects")


# Clicks each of `arguments[0]` (selectors) in turn, the first at the
# start of an animation frame and each next one a task later, so all land
# before the next frame: what the earlier clicks drew and scheduled for
# that frame is torn down by the later ones before it comes.
CLICKS_IN_ONE_FRAME = """
const [selectors, done] = arguments;
requestAnimationFrame(() => {
  const next = (i) => {
    if (i === selectors.length) return setTimeout(done, 300);
    document.querySelector(selectors[i]).click();
    setTimeout(() => next(i + 1), 0);
  };
  next(0);
});
"""


def torn_down(browser):
    """What a part of the page left for later after it was taken away:
    a timer or an animation frame that reads its state. The page must not
    panic when such a callback finds the state disposed: the composer's
    blur, the phone's status line, a session row's badges fitted on the
    next frame, and the transcript following its end."""
    browser.resize(1280, 900)
    # A status message keeps the phone's status line up for a few
    # seconds; leaving for a new session takes the line away meanwhile.
    # So does the composer's blur, which waits a moment to bring back the
    # bottom bar.
    if count(browser, "#composer") == 0:
        click_row(browser, "aside [data-session-row] [data-session]")
        browser.wait("a session's composer", lambda: count(browser, "#composer") == 1)
    assert browser.text("#status") != "nothing is running", "the status would not change"
    browser.type("#composer", ESCAPE)
    browser.wait("the status", lambda: browser.text("#status") == "nothing is running")
    assert browser.script("return document.activeElement.id") == "composer"
    browser.click("aside [data-new-session]")
    browser.wait("the new session's composer", lambda: count(browser, "#new-session") == 1 and count(browser, "#composer") == 1)
    time.sleep(1)
    browser.check()
    print("the composer's blur outlives the composer quietly")
    time.sleep(8)
    browser.check()
    print("the status line's timer outlives the status line quietly")
    # A section opened and folded again within one frame: its rows fit
    # their badges on the frame after they were drawn, when they are gone.
    open_section = "aside [data-project-section][data-open=true]:has([data-session-row])"
    browser.wait("an open section with sessions", lambda: count(browser, open_section) >= 1)
    key = browser.script("return document.querySelector(arguments[0]).dataset.projectSection", open_section)
    toggle = 'aside [data-project-section="%s"] [data-section-toggle]' % key
    for _ in range(3):
        browser.script_async(CLICKS_IN_ONE_FRAME, [toggle, toggle, toggle, toggle])
    browser.check()
    print("rows drawn and folded away within a frame fit nothing")
    # A session opened and left for a new one within one frame: the
    # transcript scrolls to its end on the frame after it changed.
    for _ in range(3):
        browser.script_async(
            CLICKS_IN_ONE_FRAME, ["aside [data-session-row] [data-session]", "aside [data-new-session]"]
        )
    browser.check()
    print("a transcript drawn and taken away within a frame scrolls nothing")


def long_lived(browser):
    """A page left open while things change under it: a reply streams
    while the user goes through every view (the Machines view polls its
    runtimes each second), opens and closes a sheet, and switches
    sessions back and forth. Nothing on it may panic meanwhile."""
    browser.resize(1280, 900)
    close_sheet(browser)
    new_session(browser)
    send(browser, "go slow")
    browser.wait("the slow run", lambda: "word3" in browser.text("#transcript"))
    streaming = open_session_id(browser)
    for name in ("agents", "machines", "memory", "settings"):
        view(browser, name)
        time.sleep(3 if name == "machines" else 0.5)
    close_sheet(browser)
    browser.click("main button[aria-label=Commands]")
    browser.wait("the commands", lambda: count(browser, "#sheet") == 1)
    close_sheet(browser)
    other = 'aside [data-session-row]:not([data-session-row="%s"]) [data-session]' % streaming
    mine = 'aside [data-session-row="%s"] [data-session]' % streaming
    for _ in range(3):
        click_row(browser, other)
        time.sleep(0.3)
        click_row(browser, mine)
        time.sleep(0.3)
    browser.wait("the streaming session", lambda: open_session_id(browser) == streaming)
    browser.check()
    browser.type("#composer", ESCAPE)
    browser.wait("the cancelled run", lambda: "working" not in browser.text("#status-bar"), timeout=40)
    print("a reply streamed through every view, a sheet, and switched sessions")


def home_screen():
    """The page names a web app manifest that meets Chromium's install
    criteria, whose icons and screenshots the daemon serves."""
    base = LINK.split("#")[0]
    with urllib.request.urlopen(base + "manifest.webmanifest") as response:
        assert response.headers["Content-Type"] == "application/manifest+json"
        manifest = json.load(response)
    assert manifest["name"] and manifest["short_name"], manifest
    # Relative, so the page installs from any path.
    for field in ("id", "start_url", "scope"):
        assert manifest[field] == "./", (field, manifest[field])
    assert manifest["display"] == "standalone"
    assert re.fullmatch(r"#[0-9a-f]{6}", manifest["background_color"])
    assert re.fullmatch(r"#[0-9a-f]{6}", manifest["theme_color"])
    assert not manifest.get("prefer_related_applications")
    icons = set()
    for icon in manifest["icons"]:
        with urllib.request.urlopen(base + icon["src"]) as response:
            assert response.headers["Content-Type"] == icon["type"], icon
            assert response.read(), icon
        icons.add((icon["sizes"], icon["purpose"]))
    for size in ("192x192", "512x512"):
        for purpose in ("any", "maskable"):
            assert (size, purpose) in icons, (size, purpose)
    form_factors = set()
    for shot in manifest["screenshots"]:
        with urllib.request.urlopen(base + shot["src"]) as response:
            assert response.headers["Content-Type"] == shot["type"], shot
            assert response.read(), shot
        form_factors.add(shot["form_factor"])
    assert form_factors == {"narrow", "wide"}, form_factors
    print("the manifest, its %d icons, and its %d screenshots are served"
          % (len(manifest["icons"]), len(manifest["screenshots"])))


# The page's service worker registration: its scope, its script, and that
# script's state.
REGISTRATION = """
return navigator.serviceWorker.getRegistration().then((r) =>
    r && r.active ? [r.scope, r.active.scriptURL, r.active.state] : null);
"""


def controlled(browser):
    return browser.script("return navigator.serviceWorker.controller !== null")


def service_worker(browser):
    """The service worker registers on the 127.0.0.1 page (a secure
    context), takes control of it, and caches nothing; the daemon sends it
    to be revalidated, under a policy that allows it. Reopening the page
    without the token, as the home screen does, connects with the stored
    one over the WebSocket the worker leaves alone."""
    base = LINK.split("#")[0]
    browser.wait("the service worker", lambda: controlled(browser))
    scope, script, state = browser.script(REGISTRATION)
    assert scope == base and script == base + "sw.js", (scope, script)
    assert state == "activated", state
    assert browser.script("return caches.keys()") == [], "the service worker cached something"
    with urllib.request.urlopen(base + "sw.js") as response:
        headers = response.headers
        body = response.read().decode()
    assert headers["Content-Type"] == "text/javascript; charset=utf-8", headers["Content-Type"]
    assert headers["Cache-Control"] == "no-cache", headers["Cache-Control"]
    assert headers["Service-Worker-Allowed"] is None
    assert "worker-src 'self';" in headers["Content-Security-Policy"]
    assert re.search(r"^// build: [0-9a-f]{16}$", body, re.M), "sw.js carries no build stamp"
    # A fetch that fails (the daemon restarting) ends as a network error, not
    # an uncaught rejection in the worker's console.
    assert "fetch(request).catch(() => Response.error())" in body, "sw.js leaves failed fetches uncaught"
    browser.go(base)
    browser.wait("the connection from the start URL", lambda: browser.text("#connected") != "")
    assert controlled(browser), "the reloaded page is not the service worker's"
    assert browser.script("return caches.keys()") == []
    print("the service worker controls the page, caches nothing, and the page connects under it")


def daemon_change():
    """The worker is network-only: with the other daemon's page under
    its control, the daemon stops and a reload fails, then a different
    build on the same port shows on the next reload."""
    browser = Browser()
    try:
        changed_daemon(browser)
    finally:
        browser.close()


def changed_daemon(browser):
    base = OTHER_PAGE
    browser.go(OTHER_LINK)
    browser.wait("the other page's service worker", lambda: controlled(browser))
    with open(os.path.join(os.path.dirname(HOME), "other", "daemon.pid")) as file:
        daemon = int(file.read())
    os.kill(daemon, signal.SIGTERM)
    deadline = time.time() + 20
    while time.time() < deadline:
        with socket.socket() as probe:
            if probe.connect_ex(("127.0.0.1", int(OTHER_PORT))) != 0:
                break
        time.sleep(0.1)
    try:
        browser.go(base)
        loaded = browser.script("return document.querySelector('link[rel=manifest]') !== null")
    except RuntimeError:
        loaded = False
    assert not loaded, "the page loaded with its daemon gone: something cached it"
    build = tempfile.mkdtemp()
    with open(os.path.join(build, "index.html"), "w") as file:
        file.write("<!doctype html><title>cookie</title><p id=build>a new build</p>")
    with open("crates/web_assets/bundle/sw.js") as source, open(os.path.join(build, "sw.js"), "w") as copy:
        copy.write(source.read())
    handler = functools.partial(QuietFiles, directory=build)
    server = http.server.ThreadingHTTPServer(("127.0.0.1", int(OTHER_PORT)), handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        browser.go(base)
        browser.wait("the new build", lambda: browser.text("#build") == "a new build")
    finally:
        server.shutdown()
        server.server_close()
    print("a reload after the daemon changed shows the new build at once")


def restart_daemon(browser):
    """A change that needs a restart shows the banner; its button and
    Settings → Configuration's both open the confirmation, which offers a
    restart when idle or now; when idle restarts the daemon, the page
    reconnects, and the banner is gone."""
    config = os.path.join(HOME, ".cookie-agent", "config.toml")
    with open(config, "a") as file:
        file.write('\n[runtimes.later]\ncommand = ["false"]\n')
    open_section(browser, "settings", "configuration")
    browser.click("#config-reload")
    browser.wait("the restart banner", lambda: "runtimes" in browser.text("#restart-needed"))
    # Only `runtimes`: the token the daemon wrote to `[server]` at its first
    # start is the one it serves with.
    needed = browser.text("#restart-needed")
    assert needed == "Restart needed to apply: runtimes", needed
    browser.shot("restart-banner")
    browser.click("#restart-banner-button")
    browser.wait("the confirmation", lambda: shown(browser, "#restart-dialog"))
    assert browser.text("#restart-idle") == "Restart when idle", browser.text("#restart-idle")
    assert browser.text("#restart-now").startswith("Restart now"), browser.text("#restart-now")
    browser.click("#restart-cancel")
    browser.wait("the confirmation closed", lambda: not shown(browser, "#restart-dialog"))
    print("the banner names what waits for a restart and its button asks first")

    # A phone: the confirmation is a sheet with thumb-sized buttons.
    browser.resize(390, 844)
    browser.click("#restart-banner-button")
    browser.wait("the phone confirmation", lambda: shown(browser, "#restart-dialog"))
    heights = browser.script(
        "return ['#restart-idle', '#restart-now', '#restart-cancel']"
        ".map((s) => document.querySelector(s).getBoundingClientRect().height)"
    )
    assert all(height >= 44 for height in heights), heights
    fits = browser.script(
        "const r = document.querySelector('#restart-dialog').getBoundingClientRect();"
        "return r.left >= 0 && r.right <= innerWidth && r.bottom <= innerHeight + 1"
    )
    assert fits, "the confirmation does not fit a phone"
    browser.shot("restart-confirm-phone")
    browser.click("#restart-cancel")
    browser.resize(1280, 900)
    print("on a phone the confirmation fits and its buttons are 44 px tall")

    # Settings → Configuration: Restart daemon beside Reload from disk,
    # highlighted while a change waits for it; when idle.
    open_section(browser, "settings", "configuration")
    assert count(browser, "#restart-daemon[data-restart-needed]") == 1, "the restart button is not highlighted"
    beside = browser.script(
        "const r = document.querySelector('#config-reload'), b = document.querySelector('#restart-daemon');"
        "return r.parentElement === b.parentElement && r.nextElementSibling === b"
    )
    assert beside, "Restart daemon is not next to Reload from disk"
    browser.click("#restart-daemon")
    browser.wait("the confirmation", lambda: shown(browser, "#restart-dialog"))
    with open(os.path.join(HOME, "daemon.pid")) as file:
        pid = int(file.read())
    browser.click("#restart-idle")
    browser.wait(
        "the daemon restarting",
        lambda: count(browser, "#reconnecting") == 1 or count(browser, "#restart-banner[data-restarting]") == 1,
    )
    browser.wait("the reconnected page", lambda: count(browser, "#reconnecting") == 0 and browser.text("#connected") != "", timeout=60)
    browser.wait("the banner gone", lambda: count(browser, "#restart-banner") == 0)
    open_section(browser, "settings", "configuration")
    browser.wait(
        "the plain restart button",
        lambda: count(browser, "#restart-daemon") == 1 and count(browser, "#restart-daemon[data-restart-needed]") == 0,
    )
    os.kill(pid, 0)
    with open(os.path.join(HOME, "daemon.out")) as file:
        readies = sum(1 for line in file if line.startswith("daemon-ready "))
    assert readies == 2, "the daemon did not restart in place: %d ready lines" % readies
    print("Settings restarts the daemon when idle; the page reconnects to the same process")


class QuietFiles(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *_):
        pass


# A Chromium install prompt, as `beforeinstallprompt` hands it: `prompt()`
# counts its calls, and the user's choice is `arguments[0]`. Returns whether
# the page kept it from the browser's mini-infobar.
FAKE_PROMPT = """
const event = new Event('beforeinstallprompt', {cancelable: true});
event.prompt = () => { window.promptCalls = (window.promptCalls || 0) + 1; return Promise.resolve(); };
event.userChoice = Promise.resolve({outcome: arguments[0], platform: 'web'});
dispatchEvent(event);
return event.defaultPrevented;
"""

IPHONE = (
    "Mozilla/5.0 (iPhone; CPU iPhone OS 18_1 like Mac OS X) AppleWebKit/605.1.15 "
    "(KHTML, like Gecko) Version/18.1 Mobile/15E148 Safari/604.1"
)


def offer(browser):
    return browser.script("const e = document.querySelector('#install'); return e ? e.dataset.installOffer : ''")


def browser_section(browser):
    """Opens Settings › This browser by script clicks (a phone's rail is in
    its drawer)."""
    for selector in ("[data-rail=primary] [data-view=settings]", "[data-section=browser]"):
        browser.script("document.querySelector(arguments[0]).click()", selector)
        time.sleep(0.4)
    browser.wait("the install section", lambda: offer(browser) != "")


def fits(browser, what):
    """Asserts the page does not scroll sideways and its targets take a
    finger."""
    overflow = browser.script(OVERFLOW)
    small = browser.script(SMALL_TARGETS)
    assert overflow <= 0 and not small, "%s: overflow %s, small %s" % (what, overflow, small)


def install_prompt():
    """Chromium's prompt, handed to the page by a script (Browser keeps
    Chrome's own from the page): Settings offers Install app, which opens
    it once; a dismissed prompt leaves the browser's menu, and an install
    hides the action."""
    browser = Browser()
    try:
        browser.resize(1280, 900)
        browser.go(LINK)
        browser.wait("the connection", lambda: browser.text("#connected") != "")
        browser_section(browser)
        # No prompt yet: the browser's menu.
        assert offer(browser) == "browser-menu", offer(browser)
        assert browser.script(FAKE_PROMPT, "dismissed"), "the mini-infobar was not prevented"
        browser.wait("the install action", lambda: offer(browser) == "prompt")
        assert browser.text("#install [data-install-action]") == "Install app"
        browser.shot("install-settings-prompt")
        browser.click("#install [data-install-action]")
        browser.wait("the used prompt", lambda: offer(browser) == "browser-menu")
        assert browser.script("return window.promptCalls") == 1
        browser.script(FAKE_PROMPT, "accepted")
        browser.wait("the second prompt", lambda: offer(browser) == "prompt")
        browser.script("dispatchEvent(new Event('appinstalled'))")
        browser.wait("the installed page", lambda: offer(browser) == "installed")
        assert count(browser, "[data-install-action]") == 0
        browser.shot("install-settings-installed")
        print("Chromium's prompt: Install app opens it once, and an install hides it")
    finally:
        browser.close()


def install_on_phones():
    """The install offer on a phone, in both themes: Chromium's prompt (from
    a script) and iOS's steps (Chrome posing as Safari on an iPhone), in
    the banner, Settings › This browser, and the connect screen."""
    for platform, agent in (("android", None), ("ios", IPHONE)):
        phone = Browser(user_agent=agent)
        try:
            for theme in ("light", "dark"):
                phone.go(LINK)
                phone.wait("the connection", lambda: phone.text("#connected") != "")
                phone.script("localStorage.setItem('cookie.theme', arguments[0])", theme)
                phone.go(LINK.split("#")[0])
                phone.wait("the connection", lambda: phone.text("#connected") != "")
                phone.phone(390, 844)
                if agent is None:
                    phone.script(FAKE_PROMPT, "dismissed")
                phone.wait("the install banner", lambda: count(phone, "#install-banner") == 1)
                if agent:
                    assert "Add to Home Screen" in phone.text("#install-banner [data-install-steps]")
                else:
                    assert phone.text("#install-banner [data-install-action]") == "Install app"
                fits(phone, "%s banner" % platform)
                phone.shot("phone-install-banner-%s-%s" % (platform, theme))
                browser_section(phone)
                assert offer(phone) == ("ios" if agent else "prompt"), offer(phone)
                fits(phone, "%s settings" % platform)
                phone.shot("phone-install-settings-%s-%s" % (platform, theme))
                # The connect screen, on another port (another origin, its
                # own theme choice), of a page pointed at no daemon.
                phone.go(OTHER_PAGE)
                phone.script(REMEMBER_DAEMON, UNREACHABLE, "token")
                phone.script("localStorage.setItem('cookie.theme', arguments[0])", theme)
                phone.go(OTHER_PAGE)
                phone.wait("the connect screen", lambda: "Check the daemon URL" in phone.text())
                phone.phone(390, 844)
                if agent is None:
                    phone.script(FAKE_PROMPT, "dismissed")
                phone.wait("the connect screen's action", lambda: count(phone, "main [data-install-action]") == 1)
                if agent:
                    phone.click("main [data-install-action]")
                    phone.wait("iOS's steps", lambda: "Add to Home Screen" in phone.text("main [data-install-steps]"))
                fits(phone, "%s connect screen" % platform)
                phone.script("document.querySelector('main [data-install-action]').scrollIntoView({block: 'center'})")
                phone.shot("phone-install-connect-%s-%s" % (platform, theme))
            # The banner goes for good once dismissed.
            phone.go(LINK)
            phone.wait("the connection", lambda: phone.text("#connected") != "")
            if agent is None:
                phone.script(FAKE_PROMPT, "dismissed")
            phone.wait("the install banner", lambda: count(phone, "#install-banner") == 1)
            phone.click("#install-banner [data-install-dismiss]")
            phone.wait("the dismissed banner", lambda: count(phone, "#install-banner") == 0)
            phone.go(LINK.split("#")[0])
            phone.wait("the connection", lambda: phone.text("#connected") != "")
            if agent is None:
                phone.script(FAKE_PROMPT, "dismissed")
            time.sleep(0.5)
            assert count(phone, "#install-banner") == 0, "the dismissed banner came back"
            print("phone (%s): the banner, Settings, and the connect screen offer the install" % platform)
        finally:
            phone.close()


def session_dir(session_id):
    for directory, children, _ in os.walk(HOME):
        if session_id in children:
            return os.path.join(directory, session_id)
    raise RuntimeError("no directory for session %s under %s" % (session_id, HOME))


def foreign_session(browser):
    """A session another process holds opens as a read-only snapshot. The
    holder here takes the tree's owner lock as a second cookie process
    would."""
    with open(os.path.join(session_dir(HELD_SESSION), "owner.lock"), "a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        browser.wait(
            "the held session in the list",
            lambda: count(browser, 'aside [data-session="%s"]' % HELD_SESSION) == 1,
        )
        click_row(browser, 'aside [data-session="%s"]' % HELD_SESSION)
        browser.wait("the read-only snapshot", lambda: "read-only snapshot" in browser.text("main"))
        browser.wait(
            "the snapshot's transcript",
            lambda: "hello from another process" in browser.text("#transcript"),
        )
        assert browser.script("return document.querySelector('#composer').disabled")
        print("a session another process holds opens as a read-only snapshot")
        browser.shot("read-only")


def section(project):
    """The sidebar section of `project` (an ID, or `none`)."""
    key = "none" if project == "none" else "project:" + project
    return 'aside [data-project-section="%s"]' % key


def header(project):
    return 'aside [data-project-header="%s"]' % project


def section_of(browser, session):
    """The section key a session's row is listed under."""
    return browser.script(
        "const r = document.querySelector('aside [data-session-row=\"' + arguments[0] + '\"]');"
        "const s = r && r.closest('[data-project-section]'); return s ? s.dataset.projectSection : null",
        session,
    )


def project_on_disk(project):
    """A project's `project.toml` and `AGENTS.md` (or None)."""
    folder = os.path.join(HOME, ".cookie-agent", "projects", project)
    with open(os.path.join(folder, "project.toml")) as file:
        toml = file.read()
    agents = os.path.join(folder, "AGENTS.md")
    if not os.path.exists(agents):
        return toml, None
    with open(agents) as file:
        return toml, file.read()


def sheet_card(key):
    return '#sheet [data-card="%s"]' % key


def click_centred(browser, selector):
    """Scrolls `selector` to the middle of its scroller first, clear of a
    sheet's pinned foot, then clicks it."""
    browser.script("document.querySelector(arguments[0]).scrollIntoView({block: 'center'})", selector)
    browser.click(selector)


# A row's title width and its shown badges, which a hover must not move.
ROW_LAYOUT = """
const row = document.querySelector(arguments[0]);
const title = row.querySelector('[data-session-title]').getBoundingClientRect();
return [Math.round(title.width), [...row.querySelectorAll('[data-session-badges] [data-badge]')]
    .filter((b) => !b.hidden).map((b) => [b.textContent.trim(), Math.round(b.getBoundingClientRect().left)])];
"""


def projects(browser):
    """Projects in the sidebar: sections instead of the workspace filter,
    a project made in the editor with an emoji, default attachments, a
    default agent, and an AGENTS.md; a new session in it from its header's
    +, prefilled; sessions moved by mouse and by finger onto a header and
    with the row's ⋮ menu's Move to project…, which Escape and a click
    outside close; folding remembered; the editor's unsaved-edit
    guard, its rename, and its conflict; and the delete, refused while a
    session runs, then confirmed with what goes."""
    close_sheet(browser)
    assert count(browser, "#place-filter") == 0, "the workspace filter stayed"
    browser.wait("the sections", lambda: count(browser, section("none")) == 1)
    # The adopted project from the memory scenario's folder is a section;
    # "No project" is last and has no pencil or trash.
    keys = browser.script(
        "return [...document.querySelectorAll('aside#sidebar [data-project-section]')].map((s) => s.dataset.projectSection)"
    )
    assert keys[-1] == "none", keys
    assert count(browser, header("none") + " [data-header-edit]") == 0
    print("the sidebar lists project sections, No project last:", keys)

    # A new project from the sidebar's button.
    browser.click("#new-project")
    browser.wait("the editor", lambda: count(browser, "#project-editor") == 1)
    assert browser.text("#sheet h2") == "New project"
    set_text(browser, "#project-name", "e2e-web")
    click_centred(browser, "#project-emoji")
    browser.wait("the emoji picker", lambda: count(browser, "#emoji-picker") == 1)
    browser.shot("project-emoji-picker")
    set_text(browser, "#emoji-search", "cookie")
    browser.wait("the search", lambda: count(browser, '#emoji-picker [data-emoji]') >= 1)
    assert count(browser, '#emoji-picker [data-emoji="🍪"]') == 1
    # Typed text is checked as the daemon checks it.
    set_text(browser, "#emoji-typed", "ab")
    browser.wait("the refusal", lambda: "Not one emoji" in browser.text("[data-emoji-error]"))
    assert browser.script("return document.querySelector('#emoji-use').disabled")
    set_text(browser, "#emoji-typed", "🍪🍪")
    browser.wait("the refusal", lambda: "more than one" in browser.text("[data-emoji-error]"))
    browser.click('#emoji-picker [data-emoji="🍪"]')
    browser.wait("the chosen emoji", lambda: browser.text("[data-chosen-emoji]") == "🍪")
    # Default attachments: global and local at the daemon's workspace.
    browser.wait("the editor's cards", lambda: count(browser, sheet_card("local/coding")) == 1)
    click_centred(browser, sheet_card("local/coding") + " [data-card-toggle]")
    browser.wait("its field", lambda: count(browser, sheet_card("local/coding") + " [data-workspace-field]") == 1)
    assert "Enter a workspace" in browser.text("[data-editor-blocker]") or browser.script(
        "return document.querySelector('#project-save').disabled"
    )
    set_text(browser, sheet_card("local/coding") + ' [data-workspace-field="0"]', "~/workspace")
    click_centred(browser, "#project-agent")
    browser.wait("the agents", lambda: count(browser, "[data-agent-choices]") == 1)
    click_centred(browser, '[data-agent-choice="builder · review"]')
    browser.wait("the default agent", lambda: browser.text("[data-project-agent]") == "builder · review")
    set_text(browser, "#project-agents-md", "Answer in one line.\n")
    browser.shot("project-editor-new")
    browser.click("#project-save")
    browser.wait("the saved project", lambda: count(browser, "#sheet") == 0)
    project = browser.script(
        "const h = [...document.querySelectorAll('aside#sidebar [data-project-header]')]"
        ".find((h) => h.innerText.includes('e2e-web')); return h ? h.dataset.projectHeader : null"
    )
    assert project, "no e2e-web header"
    assert browser.text(header(project) + " [data-project-emoji]") == "🍪"
    toml, agents = project_on_disk(project)
    assert 'emoji = "🍪"' in toml and 'agent = "builder"' in toml and 'preset = "review"' in toml, toml
    assert "~/workspace" in toml or "workspace" in toml, toml
    assert agents == "Answer in one line.\n", agents
    # Its header shows where it runs, as the rows do.
    header_badges = browser.script(
        "return [...document.querySelectorAll(arguments[0] + ' [data-badge]')]"
        ".filter((b) => !b.hidden).map((b) => b.textContent.trim())",
        header(project),
    )
    assert len(header_badges) == 2 and header_badges[1] == "~/workspace", header_badges
    print("made project e2e-web with an emoji, defaults, and AGENTS.md; its header shows", header_badges)

    # A new session in it from its +: the cards and the agent prefilled.
    browser.click(header(project) + " [data-header-new]")
    # The cards have the project's defaults prefilled.
    show_cards(browser)
    browser.wait("the prefilled places", lambda: browser.text("#draft-places [data-draft-places]") == "~/workspace")
    browser.wait("the new session", lambda: count(browser, "#new-session-project") == 1)
    assert browser.script("return document.querySelector('#new-session-project').value") == project
    browser.wait("the prefilled card", lambda: field_values(browser, "local/coding") == ["~/workspace"])
    assert "global/global" in selected_cards(browser)
    browser.shot("project-new-session")
    browser.wait("the default agent", lambda: "builder" in browser.text("#agent-chip"))
    send(browser, "hello from the project")
    browser.wait("the session", lambda: browser.text("#session-title") != "New session" and answers(browser) >= 1, timeout=60)
    made = open_session_id(browser)
    browser.wait("its row in the section", lambda: section_of(browser, made) == "project:" + project)
    print("a new session from the header's + starts in the project with its runtimes and agent")

    # Moving: a mouse drags a row from No project onto the header.
    loose = browser.script(
        "return [...document.querySelectorAll(arguments[0] + ' [data-session-row]')].map((r) => r.dataset.sessionRow)",
        section("none"),
    )
    assert len(loose) >= 2, loose
    first, second = loose[0], loose[1]
    drag_row(browser, 'aside [data-session-row="%s"] [data-session]' % first, header(project) + " [data-section-toggle]")
    browser.wait("the dragged session", lambda: section_of(browser, first) == "project:" + project)
    print("a mouse drag moved a session into the project")
    # A finger lifts the row after a long press, then drags it.
    drag_row(browser, 'aside [data-session-row="%s"] [data-session]' % second, header(project) + " [data-section-toggle]", touch=True)
    browser.wait("the finger-dragged session", lambda: section_of(browser, second) == "project:" + project)
    print("a finger drag moved a session into the project")
    # The row's ⋮ menu's Move to project… sends one back out.
    row = 'aside [data-session-row="%s"]' % second
    hover(browser, "#transcript")
    before = browser.script(ROW_LAYOUT, row)
    hover(browser, row)
    browser.wait("the ⋮", lambda: shown(browser, row + " [data-row-menu]"))
    # The buttons lie over the row's end: its title, time, and badges keep
    # their layout.
    time.sleep(0.3)
    assert browser.script(ROW_LAYOUT, row) == before, (before, browser.script(ROW_LAYOUT, row))
    order = browser.script(
        "return [...document.querySelectorAll(arguments[0] + ' [data-row-actions] button')].map((b) => b.getAttribute('aria-label'))",
        row,
    )
    assert order[:2] == ["More actions", "Rename the session"], order
    browser.shot("project-row-buttons")
    opened = open_session_id(browser)
    menu = "aside [data-row-menu-list]"
    # A click opens the menu under the ⋮, right edges level, without
    # opening the session; Escape closes it and gives the ⋮ the focus.
    browser.click(row + " [data-row-menu]")
    browser.wait("the row's menu", lambda: count(browser, menu) == 1)
    items = browser.script(
        "return [...document.querySelectorAll(arguments[0] + ' [role=menuitem]')].map((b) => b.innerText.trim())", menu
    )
    assert items == ["Move to project…"], items
    trigger, box = browser.script(
        "const r = (s) => document.querySelector(s).getBoundingClientRect();"
        "const t = r(arguments[0]), m = r(arguments[1]);"
        "return [[Math.round(t.bottom), Math.round(t.right)], [Math.round(m.top), Math.round(m.right)]]",
        row + " [data-row-menu]", menu,
    )
    assert box[0] >= trigger[0] and abs(box[1] - trigger[1]) <= 1, (trigger, box)
    assert browser.script("return document.activeElement.dataset.menuItem") == "move"
    assert browser.script("return document.querySelector(arguments[0]).getAttribute('aria-expanded')", row + " [data-row-menu]") == "true"
    browser.shot("project-row-menu")
    browser.type(menu + " [data-menu-item=move]", ESCAPE)
    browser.wait("the closed menu", lambda: count(browser, menu) == 0)
    assert browser.script("return document.activeElement.dataset.rowMenu") == second, "the ⋮ lost the focus"
    assert open_session_id(browser) == opened, "the ⋮ opened the session"
    # Enter on the focused ⋮ opens it again; a click outside closes it.
    browser.type(row + " [data-row-menu]", ENTER)
    browser.wait("the row's menu by keyboard", lambda: count(browser, menu) == 1)
    browser.click("aside [data-row-menu-backdrop]")
    browser.wait("the menu closed by a click outside", lambda: count(browser, menu) == 0)
    assert open_session_id(browser) == opened, "the click outside opened the session"
    hover(browser, row)
    browser.wait("the ⋮", lambda: shown(browser, row + " [data-row-menu]"))
    browser.click(row + " [data-row-menu]")
    browser.wait("the row's menu", lambda: count(browser, menu) == 1)
    browser.click(menu + " [data-menu-item=move]")
    browser.wait("the picker", lambda: count(browser, "#sheet [data-move-to]") >= 2)
    assert count(browser, menu) == 0, "the menu stayed open"
    browser.shot("project-move-picker")
    browser.click('#sheet [data-move-to="none"]')
    browser.wait("the moved session", lambda: section_of(browser, second) == "none")
    print("the ⋮ menu's Move to project… moved a session out of its project; Escape and a click outside close it")

    # The header's pencil and trash show on hover, left of its +.
    hover(browser, "#transcript")
    assert count(browser, header(project) + " [data-header-edit]") == 0
    hover(browser, header(project))
    browser.wait("the header's buttons", lambda: count(browser, header(project) + " [data-header-edit]") == 1)
    labels = browser.script(
        "return [...document.querySelectorAll(arguments[0] + ' [data-header-actions] button')].map((b) => b.getAttribute('aria-label'))",
        header(project),
    )
    assert labels == ["Edit the project", "Delete the project", "New session in this project"], labels
    # Its buttons are clear, so they take the header's colour, as No
    # project's + does: the same buttons, styled the same.
    width = browser.script("return document.querySelector(arguments[0]).getBoundingClientRect().width", header(project))
    perform(browser, [{
        "type": "pointer", "id": "mouse", "parameters": {"pointerType": "mouse"},
        "actions": [{"type": "pointerMove", "duration": 0, "origin": {ELEMENT: browser.find(header(project))},
                     "x": int(-width / 2 + 30), "y": -8}],
    }])
    time.sleep(0.3)
    for button in ("[data-header-edit]", "[data-header-delete]", "[data-header-new]"):
        colour = browser.script(
            "return getComputedStyle(document.querySelector(arguments[0])).backgroundColor",
            header(project) + " " + button,
        )
        assert colour in ("rgba(0, 0, 0, 0)", "transparent"), (button, colour)
    assert browser.script(
        "return document.querySelector(arguments[0]).className === document.querySelector(arguments[1]).className",
        header(project) + " [data-header-new]",
        header("none") + " [data-header-new]",
    ), "a project's + is styled unlike No project's"
    browser.shot("project-header-hover")

    # Folding is remembered across a reload.
    click_row(browser, header(project) + " [data-section-toggle]")
    browser.wait("the folded section", lambda: browser.script(
        "return document.querySelector(arguments[0]).dataset.open", section(project)) == "false")
    browser.shot("project-folded")
    browser.go(LINK.split("#")[0])
    browser.wait("the connection", lambda: browser.text("#connected") != "")
    browser.wait("the sections", lambda: count(browser, section(project)) == 1)
    assert browser.script("return document.querySelector(arguments[0]).dataset.open", section(project)) == "false"
    click_row(browser, header(project) + " [data-section-toggle]")
    browser.wait("the open section", lambda: browser.script(
        "return document.querySelector(arguments[0]).dataset.open", section(project)) == "true")
    print("a folded section stays folded after a reload")

    # The editor: closing with unsaved edits asks first; a rename saves.
    hover(browser, header(project))
    browser.wait("the pencil", lambda: count(browser, header(project) + " [data-header-edit]") == 1)
    browser.click(header(project) + " [data-header-edit]")
    browser.wait("the editor", lambda: browser.script(
        "const e = document.querySelector('#project-agents-md'); return !!e && !e.disabled && e.value.includes('one line')"))
    assert browser.text("#sheet h2") == "Project"
    set_text(browser, "#project-name", "e2e-renamed")
    browser.click("#sheet h2 ~ button")
    browser.wait("the question", lambda: count(browser, "[data-discard-box]") == 1)
    browser.shot("project-discard")
    browser.click("#sheet [data-discard-box] button:not([data-discard])")
    browser.wait("the editor kept", lambda: count(browser, "[data-discard-box]") == 0)
    assert browser.script("return document.querySelector('#project-name').value") == "e2e-renamed"
    browser.click("#project-save")
    browser.wait("the renamed header", lambda: "e2e-renamed" in browser.text(header(project)))
    print("the editor asks before dropping edits, and renamed the project")

    # A change made elsewhere meanwhile: the save is refused and reloads.
    hover(browser, header(project))
    browser.wait("the pencil", lambda: count(browser, header(project) + " [data-header-edit]") == 1)
    browser.click(header(project) + " [data-header-edit]")
    browser.wait("the editor", lambda: browser.script(
        "const e = document.querySelector('#project-agents-md'); return !!e && !e.disabled"))
    with open(os.path.join(HOME, ".cookie-agent", "projects", project, "AGENTS.md"), "w") as file:
        file.write("Changed on disk.\n")
    set_text(browser, "#project-agents-md", "Mine.\n")
    browser.click("#project-save")
    browser.wait("the conflict", lambda: count(browser, "[data-editor-reload]") == 1)
    assert "changed since" in browser.text("[data-editor-error]"), browser.text("[data-editor-error]")
    browser.shot("project-conflict")
    browser.click("[data-editor-reload]")
    browser.wait("the reloaded editor", lambda: browser.script(
        "return document.querySelector('#project-agents-md').value") == "Changed on disk.\n")
    browser.click("#sheet h2 ~ button")
    browser.wait("the closed editor", lambda: count(browser, "#sheet") == 0)
    print("a project changed meanwhile refuses the save and reloads")

    # Delete: refused while a session of the project runs, naming it.
    click_row(browser, 'aside [data-session-row="%s"] [data-session]' % made)
    browser.wait("the session", lambda: open_session_id(browser) == made)
    send(browser, "go slow")
    browser.wait("the slow run", lambda: "word3" in browser.text("#transcript"))
    hover(browser, header(project))
    browser.wait("the trash", lambda: count(browser, header(project) + " [data-header-delete]") == 1)
    browser.click(header(project) + " [data-header-delete]")
    browser.wait("the confirmation", lambda: count(browser, "aside [data-project-confirm-box]") == 1)
    question = browser.text("aside [data-project-confirm-box] p")
    assert "e2e-renamed" in question and "2 sessions" in question and "memor" in question, question
    browser.shot("project-delete-confirm")
    browser.click("aside [data-project-delete-confirm]")
    browser.wait("the refusal", lambda: "running" in browser.text("aside [data-project-error]"))
    print("a project with a running session is not deleted:", browser.text("aside [data-project-error]"))
    browser.type("#composer", ESCAPE)
    browser.wait("the cancelled run", lambda: "working" not in browser.text("#status-bar"), timeout=40)
    time.sleep(1)
    hover(browser, header(project))
    browser.wait("the trash", lambda: count(browser, header(project) + " [data-header-delete]") == 1)
    browser.click(header(project) + " [data-header-delete]")
    browser.wait("the confirmation", lambda: count(browser, "aside [data-project-confirm-box]") == 1)
    browser.click("aside [data-project-delete-confirm]")
    browser.wait("the deleted project", lambda: count(browser, section(project)) == 0)
    assert not os.path.exists(os.path.join(HOME, ".cookie-agent", "projects", project))
    assert count(browser, 'aside [data-session-row="%s"]' % made) == 0
    print("deleted the project with its sessions:", browser.text("#status"))


def phone_section_of(phone, session):
    """The section key a session's row is listed under in the drawer."""
    return phone.script(
        "const r = document.querySelector('#drawer [data-session-row=\"' + arguments[0] + '\"]');"
        "return r && r.closest('[data-project-section]').dataset.projectSection",
        session,
    )


def projects_phone():
    """Projects on a phone: a long press on a header shows its buttons, a
    long press on a row offers its ⋮, Rename, and Delete, a finger drags a
    row onto a header, and the ⋮'s sheet moves it back out."""
    phone = Browser()
    try:
        phone.phone(375, 800)
        phone.go(LINK)
        phone.wait("the connection", lambda: phone.text("#connected") != "")
        phone.click("main [data-sidebar-toggle]")
        phone.wait("the drawer", lambda: count(phone, "#drawer [data-project-section]") >= 2)
        project = phone.script(
            "const h = document.querySelector('#drawer [data-project-header]:not([data-project-header=none])');"
            "return h ? h.dataset.projectHeader : null"
        )
        assert project, "no project section"
        target = '#drawer [data-project-header="%s"]' % project
        long_press(phone, target + " [data-section-toggle]")
        phone.wait("the header's buttons", lambda: count(phone, target + " [data-header-edit]") == 1)
        phone.shot("phone-project-header")
        row = phone.script(
            "const r = document.querySelector('#drawer [data-project-section=none] [data-session-row]');"
            "return r ? r.dataset.sessionRow : null"
        )
        long_press(phone, '#drawer [data-session-row="%s"] [data-session]' % row)
        phone.wait("the row's actions", lambda: count(phone, "#drawer [data-delete-actions] [data-row-menu]") == 1)
        labels = phone.script(
            "return [...document.querySelectorAll('#drawer [data-delete-actions] button')].map((b) => b.getAttribute('aria-label'))"
        )
        assert labels == ["More actions", "Rename", "Delete", "Cancel"], labels
        # A narrow phone shows the four as icons, on one line.
        tops = phone.script(
            "return [...new Set([...document.querySelectorAll('#drawer [data-delete-actions] button')].map((b) => Math.round(b.getBoundingClientRect().top)))]"
        )
        assert len(tops) == 1, "the row's actions wrap: %s" % tops
        fits(phone, "the row's actions")
        phone.shot("phone-row-actions")
        phone.click("#drawer [data-actions-cancel]")
        drag_row(phone, '#drawer [data-session-row="%s"] [data-session]' % row, target + " [data-section-toggle]", touch=True)
        phone.wait("the moved row", lambda: phone_section_of(phone, row) == "project:" + project)
        print("phone: long presses show the header's and the row's buttons; a finger drags a row onto a header")
        # The ⋮ a long press reveals opens a sheet from the bottom, the
        # screen's width, with rows a finger can hit; a tap outside closes
        # it, and its Move to project… moves the row back out.
        long_press(phone, '#drawer [data-session-row="%s"] [data-session]' % row)
        phone.wait("the row's ⋮", lambda: count(phone, "#drawer [data-delete-actions] [data-row-menu]") == 1)
        menu = "#drawer [data-row-menu-list]"
        phone.click("#drawer [data-delete-actions] [data-row-menu]")
        phone.wait("the row's sheet", lambda: count(phone, menu) == 1)
        frame = phone.script(
            "const m = document.querySelector(arguments[0]).getBoundingClientRect();"
            "return [Math.round(m.left), Math.round(m.width), Math.round(innerWidth), Math.round(m.bottom), Math.round(innerHeight)]",
            menu,
        )
        assert frame[0] == 0 and frame[1] == frame[2] and abs(frame[3] - frame[4]) <= 1, "not a bottom sheet: %s" % frame
        assert "Move to project…" in phone.text(menu), phone.text(menu)
        fits(phone, "the row's sheet")
        phone.shot("phone-row-menu")
        phone.click("#drawer [data-row-menu-backdrop]")
        phone.wait("the sheet closed by a tap outside", lambda: count(phone, menu) == 0)
        assert count(phone, "#drawer") == 1, "the tap outside closed the drawer"
        phone.click("#drawer [data-delete-actions] [data-row-menu]")
        phone.wait("the row's sheet", lambda: count(phone, menu) == 1)
        phone.click(menu + " [data-menu-item=move]")
        phone.wait("the picker", lambda: count(phone, "#sheet [data-move-to]") >= 2)
        phone.click('#sheet [data-move-to="none"]')
        phone.wait("the closed picker", lambda: count(phone, "#sheet") == 0)
        phone.click("main [data-sidebar-toggle]")
        phone.wait("the row moved back out", lambda: phone_section_of(phone, row) == "none")
        print("phone: the row's ⋮ opens a bottom sheet a tap outside closes; its Move to project… moved the row")
    finally:
        phone.close()


# The mock OAuth server the daemon's `oauth-*` MCP servers sign in to
# (mock-oauth.py).
OAUTH_SERVER = os.environ.get("OAUTH_SERVER", "")


def mcp_state(browser, server):
    return browser.text('#page [data-server="%s"] .mcp-state' % server)


def window_handles(browser):
    return call("GET", browser.path("/window/handles"))


def signed_in(browser, server):
    """Waits for the page to say it signed in to `server`, and for the
    server to connect."""
    browser.wait(
        "the sign-in to %s" % server,
        lambda: "Signed in to %s" % server in browser.text("#mcp-sign-in-outcome"),
        timeout=40,
    )
    browser.wait("%s connected" % server, lambda: mcp_state(browser, server) == "connected", timeout=40)


def authorization_redirect(authorization_url):
    """Where the mock authorization server sends the browser: the page with
    a code and the state."""

    class Stop(urllib.request.HTTPRedirectHandler):
        def redirect_request(self, *_):
            return None

    try:
        urllib.request.build_opener(Stop).open(authorization_url, timeout=10)
    except urllib.error.HTTPError as error:
        if error.code == 302:
            return error.headers["location"]
        raise
    raise AssertionError("the authorization server did not redirect")


# Makes `window.open` hand the page a stand-in tab, so the authorization URL
# goes nowhere and the scenario reads it, as when the sign-in ends in a
# browser that cannot hand it back.
FAKE_TAB = "window.open = () => (window.fakeTab = {location: {}, close() {}})"


def address_is_clean(browser):
    href = browser.script("return location.href")
    assert "code=" not in href and "state=" not in href and "oauth=" not in href, href


def sign_in_shots(browser):
    """With SCREENSHOT_DIR set, the MCP servers with a sign-in waiting, on a
    desktop and a phone, light and dark."""
    if not SCREENSHOTS:
        return
    for scheme in ("light", "dark"):
        browser.cdp("Emulation.setEmulatedMedia", {"features": [{"name": "prefers-color-scheme", "value": scheme}]})
        browser.resize(1280, 900)
        settle(browser)
        browser.shot("mcp-sign-in-desktop-" + scheme)
        browser.phone(390, 844)
        settle(browser)
        browser.shot("mcp-sign-in-phone-" + scheme)
    browser.cdp("Emulation.setEmulatedMedia", {"features": []})
    browser.resize(1280, 900)


def mcp_sign_in(browser):
    """Signing in to MCP servers that want OAuth, with the page as the
    redirect: through a new tab that hands the callback back over the
    BroadcastChannel and closes, in the same tab when the browser blocks
    the new one, and by pasting the address the browser landed on, with a
    cancel. A callback is cleaned out of the address bar at once, and one
    whose state is stale or unknown is refused."""
    page = LINK.split("#")[0]
    redirect = page + "?oauth=callback"
    browser.resize(1280, 900)
    open_section(browser, "settings", "mcp")
    for server in ("oauth-tab", "oauth-same", "oauth-paste"):
        browser.wait("%s to need a sign-in" % server, lambda: mcp_state(browser, server) == "needs_auth", timeout=40)
    browser.wait("the sign-in buttons", lambda: count(browser, "#page [data-mcp-sign-in]") == 3)
    assert count(browser, "#page [data-https-warning]") == 0, "a loopback page warned about http"
    browser.shot("mcp-sign-in")

    # A new tab: it comes back to the page, which hands the callback to
    # this tab and closes.
    before = window_handles(browser)
    browser.click('#page [data-server="oauth-tab"] [data-mcp-sign-in]')
    signed_in(browser, "oauth-tab")
    browser.wait("the callback tab to close", lambda: window_handles(browser) == before)
    registrations = json.load(urllib.request.urlopen(OAUTH_SERVER + "/registrations", timeout=10))
    assert registrations[-1]["redirect_uris"] == [redirect], registrations[-1]
    assert registrations[-1]["application_type"] == "native", registrations[-1]
    assert count(browser, "#mcp-sign-ins") == 0, "the finished sign-in still waits"
    browser.shot("mcp-signed-in")
    print("signed in through a new tab, which handed the callback back and closed")

    # The same tab, when the browser blocks the new one.
    browser.script("window.open = () => null")
    browser.click('#page [data-server="oauth-same"] [data-mcp-sign-in]')
    browser.wait("the page to come back", lambda: browser.script("return document.readyState") == "complete"
                 and count(browser, "#page [data-section-body=mcp]") == 1, timeout=40)
    address_is_clean(browser)
    signed_in(browser, "oauth-same")
    address_is_clean(browser)
    print("signed in in the same tab; the callback left the address bar at once")

    # The paste fallback: a mistaken address leaves the sign-in waiting, a
    # tampered state ends it, a cancel clears it, and the right address
    # finishes it.
    browser.script(FAKE_TAB)
    paste = '#page [data-sign-in="oauth-paste"]'
    browser.click('#page [data-server="oauth-paste"] [data-mcp-sign-in]')
    browser.wait("the waiting sign-in", lambda: count(browser, paste) == 1)
    browser.shot("mcp-sign-in-paste")
    browser.click(paste + " [data-sign-in-cancel]")
    browser.wait("the cancelled sign-in", lambda: count(browser, paste) == 0)
    browser.wait("the cancel", lambda: "cancelled" in browser.text('#page [data-server="oauth-paste"]'))
    print("cancelled a sign-in")

    browser.click('#page [data-server="oauth-paste"] [data-mcp-sign-in]')
    browser.wait("the waiting sign-in", lambda: count(browser, paste) == 1)
    sign_in_shots(browser)
    callback = authorization_redirect(browser.script("return window.fakeTab.location.href"))
    assert callback.startswith(redirect + "&code="), callback
    set_text(browser, paste + " [data-sign-in-paste]", callback.replace(page, page + "elsewhere/"))
    browser.click(paste + " [data-sign-in-finish]")
    browser.wait("the mistake", lambda: "not the OAuth callback" in browser.text("#mcp-sign-in-outcome"))
    assert count(browser, paste) == 1, "a mistaken address ended the sign-in"
    tampered = re.sub(r"state=[^&]+", "state=tampered", callback)
    set_text(browser, paste + " [data-sign-in-paste]", tampered)
    browser.click(paste + " [data-sign-in-finish]")
    browser.wait("the refused state", lambda: browser.script(
        "const e = document.querySelector('#mcp-sign-in-outcome'); return e && e.dataset.outcome") == "failed"
        and "not the OAuth callback" not in browser.text("#mcp-sign-in-outcome"))
    browser.wait("the ended sign-in", lambda: count(browser, paste) == 0)
    print("a mismatched state is refused and ends the sign-in")

    browser.click('#page [data-server="oauth-paste"] [data-mcp-sign-in]')
    browser.wait("the waiting sign-in", lambda: count(browser, paste) == 1)
    callback = authorization_redirect(browser.script("return window.fakeTab.location.href"))
    set_text(browser, paste + " [data-sign-in-paste]", callback)
    browser.click(paste + " [data-sign-in-finish]")
    signed_in(browser, "oauth-paste")
    browser.wait("the finished sign-in", lambda: count(browser, paste) == 0)
    print("signed in by pasting the address the browser landed on")

    # A callback whose sign-in waited too long, or that this browser did not
    # start, is refused and leaves the address bar too.
    browser.script(
        "localStorage.setItem('cookie.oauth_pending', JSON.stringify("
        "{stale: {server: 'oauth-tab', expires_at: Date.now() - 1000}}))"
    )
    browser.go(redirect + "&code=old&state=stale")
    browser.wait("the expired sign-in", lambda: browser.script(
        "const e = document.querySelector('#oauth-landing'); return e && e.dataset.landing") == "expired")
    address_is_clean(browser)
    assert "stale" not in (browser.script("return localStorage.getItem('cookie.oauth_pending')") or "")
    browser.shot("oauth-landing-expired")
    browser.go(redirect + "&code=unknown-code&state=unknown")
    browser.wait("the unknown sign-in", lambda: browser.script(
        "const e = document.querySelector('#oauth-landing'); return e && e.dataset.landing") == "unknown")
    address_is_clean(browser)
    shown = browser.script("return document.querySelector('#oauth-landing-address').value")
    assert "code=unknown-code" in shown and "state=unknown" in shown, shown
    browser.wait("the connection", lambda: browser.text("#connected") != "")
    browser.shot("oauth-landing-unknown")
    browser.click("#oauth-landing-close")
    browser.wait("the dismissed banner", lambda: count(browser, "#oauth-landing") == 0)
    for server in ("oauth-tab", "oauth-same", "oauth-paste"):
        open_section(browser, "settings", "mcp")
        browser.wait("%s still connected" % server, lambda: mcp_state(browser, server) == "connected")
    print("stale and unknown callbacks are refused and leave the address bar")
    http_warning()


def http_warning():
    """A page on plain http at an address that is not loopback warns that
    most authorization servers refuse its redirect, and still offers
    sign-in. Chrome resolves the name to this machine's loopback, where the
    daemon listens; a server added from the page (runtime only) needs the
    sign-in, as the others are signed in by now."""
    host = "cookie-agent.test"
    browser = Browser(switches=["--host-resolver-rules=MAP %s 127.0.0.1" % host])
    try:
        browser.resize(1280, 900)
        browser.go(LINK.replace("127.0.0.1", host))
        browser.wait("the connection", lambda: browser.text("#connected") != "")
        open_section(browser, "settings", "mcp")
        browser.wait("the MCP panel", lambda: count(browser, "#mcp-new") == 1)
        browser.click("#mcp-new")
        browser.type("#mcp-name", "oauth-http")
        browser.script(
            "const select = document.querySelector('#mcp-transport');"
            "select.value = 'http'; select.dispatchEvent(new Event('change'))"
        )
        browser.type("#mcp-endpoint", OAUTH_SERVER + "/mcp")
        browser.click("#mcp-save")
        row = '#page [data-server="oauth-http"]'
        browser.wait("the new server to need a sign-in", lambda: mcp_state(browser, "oauth-http") == "needs_auth", timeout=40)
        browser.wait("the warning", lambda: count(browser, row + " [data-https-warning]") == 1)
        assert count(browser, row + " [data-mcp-sign-in]") == 1, "the warning took the sign-in away"
        assert "remote-access" in browser.script(
            "return document.querySelector(arguments[0]).getAttribute('href')", row + " [data-https-warning] a")
        browser.script("document.querySelector(arguments[0]).scrollIntoView({block: 'start'})", row)
        browser.shot("mcp-sign-in-http-warning")
        browser.phone(390, 844)
        browser.script("document.querySelector(arguments[0]).scrollIntoView({block: 'start'})", row)
        browser.shot("mcp-sign-in-http-warning-phone")
        browser.click(row + " [data-mcp-remove]")
        browser.wait("the removed server", lambda: count(browser, row) == 0)
        print("a plain http page away from loopback warns, and still offers sign-in")
    finally:
        browser.close()


def connect(browser):
    """Opens the page from the link `cookie web` prints, as a desktop."""
    browser.resize(1280, 900)
    browser.go(LINK)
    browser.wait("the connection", lambda: browser.text("#connected") != "")
    assert browser.script("return location.hash") == "", "the token stayed in the address bar"


def basics(browser):
    """The fresh page, the manifest and the service worker, then a first
    session: streamed markdown, a tool call that asks for approval, and a
    slow run steered, queued behind, and cancelled."""
    print("connected:", browser.text("#connected"))
    composer_clear(browser, "desktop")
    browser.shot("fresh-connect")
    print("the fresh page's composer keeps its text clear of its buttons")
    assert browser.script(
        "return document.querySelector('link[rel=manifest]').getAttribute('href')"
    ) == "manifest.webmanifest"
    home_screen()
    service_worker(browser)

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
    clear_composer(browser)


def narrow(browser):
    """A phone-sized window: no sideways scroll, the sessions in a drawer."""
    browser.resize(375, 800)
    time.sleep(0.5)
    overflow = browser.script(
        "return document.documentElement.scrollWidth - document.documentElement.clientWidth"
    )
    assert overflow <= 0, "the page scrolls sideways by %s px" % overflow
    browser.click("main [data-sidebar-toggle]")
    browser.wait("the drawer", lambda: browser.text("#drawer") != "")
    browser.shot("narrow-sessions")
    browser.script("document.querySelector('[data-drawer-backdrop]').click()")
    browser.wait("the closed drawer", lambda: count(browser, "#drawer") == 0)
    browser.shot("narrow")
    print("the narrow layout fits and opens the sessions in a drawer")


def foreign_origin(browser):
    """The other daemon's page, of a foreign origin, connects to this
    daemon: the token is the guard. A connection that fails before its
    handshake says what to check."""
    browser.resize(1280, 900)
    page, fragment = LINK.split("#")
    token = urllib.parse.parse_qs(fragment)["token"][0]
    browser.go(OTHER_PAGE)
    browser.script(REMEMBER_DAEMON, "ws" + page[len("http"):] + "ws", token)
    browser.go(OTHER_PAGE)
    browser.wait("the connection from a foreign origin", lambda: browser.text("#connected") != "")
    print("a page of a foreign origin connects with the token")
    browser.script(REMEMBER_DAEMON, UNREACHABLE, token)
    browser.go(OTHER_PAGE)
    browser.wait("the refusal hint", lambda: "Check the daemon URL" in browser.text())
    print("a connection refused before its handshake points at the URL and the daemon")
    browser.shot("refused")
    # Back to this daemon's page for whatever runs next.
    connect(browser)


def panic_hook():
    """A panic reaches the console with its message and where it happened,
    which the build's `panic = "abort"` alone loses, and the suite's error
    check sees it. Its own browser: the panic leaves the page's WebAssembly
    in no state to go on."""
    browser = Browser()
    try:
        connect(browser)
        thrown = browser.script(
            "try { window.wasmBindings.cookiePanicProbe(); return 'returned'; }"
            " catch (error) { return String(error); }"
        )
        assert "RuntimeError" in thrown, thrown
        errors = browser.errors()
        assert any(
            "panicked at" in error and "panics.rs" in error and "the panic probe fired" in error
            for error in errors
        ), errors
        print("a panic logs its message and location:", errors[0][:160])
    finally:
        browser.close()


def audit():
    problems = phone_audit()
    for problem in problems:
        print("phone:", problem)
    assert not problems or os.environ.get("PHONE_AUDIT_REPORT_ONLY"), "the phone layout falls short"
    print("every view fits the phone sizes with touch-sized targets")


# Every scenario, in the order a group runs them: (name, function, group,
# needs). The scenarios of a group run in one process against one set of
# daemons (scripts/test-web-page.sh), in this order, and those that take
# `browser` share one desktop browser; groups share nothing, so they run
# side by side. `needs` names the scenarios of its group whose state a
# scenario builds on, which run first when it is picked alone. The phone
# audit stays last in its group, after every scenario that fills the
# daemon it looks through.
def acp_agents(browser):
    """An ACP agent the daemon's `local` runtime hosts (test-web-page.sh
    declares the scripted fixture, and a disabled one, for this group):
    the agent picker lists both with their runtime, the disabled one dimmed
    and not pickable; the agent's turn shows its plan as a checklist and its
    tool call with the agent as its source; its commands are offered in the
    composer; and the model chip opens its options, which change on the
    agent."""
    new_session(browser)
    browser.click("#agent-chip")
    fixture = '#sheet [data-agent="local-fixture"]'
    off = '#sheet [data-agent="local-off"]'
    browser.wait("the runtime agents", lambda: count(browser, fixture) == 1 and count(browser, off) == 1, timeout=40)
    assert "on local" in browser.text(fixture + " [data-agent-runtime]"), browser.text(fixture)
    assert "unavailable" in browser.text(off), browser.text(off)
    assert browser.script("return document.querySelector(arguments[0]).getAttribute('aria-disabled')", off) == "true"
    browser.click(off)
    assert count(browser, "#sheet") == 1, "a dimmed agent was picked"
    settle(browser)
    browser.shot("acp-agent-picker")
    browser.click(fixture)
    browser.wait("the picked agent", lambda: "local-fixture" in browser.text("#agent-chip"))
    send(browser, "work")
    browser.wait("the agent's turn", lambda: "Done." in browser.text("#transcript"), timeout=60)
    statuses = browser.script(
        "return [...document.querySelectorAll('#transcript .agent-plan [data-plan-status]')]"
        ".map(e => e.dataset.planStatus)"
    )
    assert statuses == ["completed", "in_progress"], statuses
    browser.wait(
        "the agent's tool call",
        lambda: "local-fixture on local" in browser.text("#transcript [data-tool-source]"),
    )
    print("the agent's turn shows its plan and names it on its tool call")

    browser.type("#composer", "/re")
    browser.wait("the agent's commands", lambda: count(browser, '#agent-commands [data-agent-command="review"]') == 1)
    assert count(browser, '#agent-commands [data-agent-command="compact"]') == 0
    browser.click('#agent-commands [data-agent-command="review"]')
    browser.wait("the command in the composer", lambda: browser.script("return document.querySelector('#composer').value") == "/review ")
    clear_composer(browser)
    print("the composer offers the agent's commands")

    browser.wait("the agent's model", lambda: "Small" in browser.text("#model-chip"), timeout=40)
    browser.click("#model-chip")
    large = '#sheet [data-agent-option="model"] [data-option-value="large"]'
    browser.wait("the agent's options", lambda: count(browser, large) == 1)
    settle(browser)
    browser.shot("acp-agent-options")
    browser.click(large)
    browser.wait("the changed option", lambda: "Large" in browser.text("#model-chip"), timeout=40)
    close_sheet(browser)
    print("the model chip opens the agent's options and sets one")


def runtime_mcp(browser):
    """The runtime panels list each runtime's MCP servers: the daemon's
    `local` runtime hosts the stdio fixture server, which connects, one
    that cannot start, which fails with its reason, and the fixture once for
    the runtime, shared by its workspaces (test-web-page.sh adds them to
    this group's daemon)."""
    new_session(browser)
    send(browser, "hello there")
    browser.wait("the session", lambda: browser.text("#session-title") != "New session" and answers(browser) >= 1, timeout=60)
    menu(browser, "runtime")
    local = '#sheet .runtime-row[data-attachment^="local"]'
    browser.wait("the local runtime", lambda: count(browser, local) == 1, timeout=40)
    browser.wait(
        "the connected fixture server",
        lambda: count(browser, local + ' [data-mcp-server="fixture"][data-mcp-state="connected"]') == 1,
        timeout=40,
    )
    browser.wait(
        "the failed server",
        lambda: count(browser, local + ' [data-mcp-server="broken"][data-mcp-state="failed"]') == 1,
        timeout=40,
    )
    fixture = browser.text(local + ' [data-mcp-server="fixture"]')
    assert "2 tools" in fixture, fixture
    # The runtime-scoped server is the runtime's: listed under its workspace
    # row too, marked runtime-wide.
    shared = local + ' [data-mcp-server="shared"][data-mcp-scope="runtime"]'
    browser.wait(
        "the runtime-wide server",
        lambda: count(browser, shared + '[data-mcp-state="connected"]') == 1,
        timeout=40,
    )
    shared_text = browser.text(shared)
    assert "runtime-wide" in shared_text and "2 tools" in shared_text, shared_text
    assert count(browser, local + ' [data-mcp-server="fixture"][data-mcp-scope="workspace"]') == 1
    reason = browser.text(local + ' [data-mcp-server="broken"] [data-mcp-message]')
    assert "broken" in reason and not reason.startswith("tool failed"), reason
    # `global` hosts the daemon's top-level servers, which wait for a
    # sign-in and say so; the local runtime's are not among them.
    global_row = '#sheet .runtime-row[data-attachment="global"]'
    browser.wait(
        "the global servers waiting for a sign-in",
        lambda: count(browser, global_row + ' [data-mcp-state="needs_auth"]') == 3,
        timeout=40,
    )
    assert count(browser, global_row + ' [data-mcp-server="fixture"]') == 0
    settle(browser)
    browser.shot("runtime-mcp-desktop")
    browser.phone(390, 844)
    settle(browser)
    browser.shot("runtime-mcp-phone")
    browser.resize(1280, 900)
    print("the runtime sheet lists the local runtime's MCP servers with state, tools, and reason")

    # The Machines view counts them by state on the runtime's row.
    browser.click("#runtime-open-machines")
    machine_mcp = '#page [data-runtime="local"] [data-runtime-mcp-count]'
    browser.wait("the local runtime's MCP servers", lambda: count(browser, machine_mcp) == 1, timeout=40)
    counted = browser.text(machine_mcp)
    assert "2 connected" in counted and "1 failed" in counted, counted
    # And lists them: the runtime-wide one once, the workspace's at its path.
    machine_runtime = '#page [data-runtime="local"]'
    assert count(browser, machine_runtime + ' [data-mcp-server="shared"][data-mcp-scope="runtime"]') == 1
    fixture_row = browser.text(machine_runtime + ' [data-mcp-server="fixture"]')
    assert "at " in fixture_row, fixture_row
    settle(browser)
    browser.shot("runtime-mcp-machines-desktop")
    browser.phone(390, 844)
    settle(browser)
    browser.shot("runtime-mcp-machines-phone")
    browser.resize(1280, 900)
    print("the Machines view counts the local runtime's MCP servers")
    close_sheet(browser)


SCENARIOS = [
    ("basics", basics, "chat", []),
    ("torn_down", torn_down, "chat", ["basics"]),
    ("long_lived", long_lived, "chat", ["basics"]),
    ("composer", composer, "chat", []),
    ("tool_rows", tool_rows, "chat", []),
    ("pinned_rows", pinned_rows, "chat", ["tool_rows"]),
    ("streaming_scroll", streaming_scroll, "chat", []),
    ("message_menu", message_menu, "chat", ["basics"]),
    ("session_depth", session_depth, "depth", []),
    ("permission_rules", permission_rules, "depth", []),
    ("management", management, "chat", []),
    ("views", views_2026, "chat", []),
    ("suspended_tab", suspended_tab, "chat", ["basics"]),
    ("reconnects", reconnects, "chat", []),
    ("foreign_session", foreign_session, "chat", []),
    ("desktop_layout", desktop_layout, "chat", ["basics"]),
    ("runtime_cards", runtime_cards, "chat", []),
    ("draft_first_message", draft_first_message, "chat", []),
    ("session_badges", session_badges, "chat", ["management", "runtime_cards"]),
    ("renaming", renaming, "chat", ["basics"]),
    ("cron", cron_jobs, "chat", []),
    ("deleting", deleting, "chat", []),
    ("projects", projects, "chat", ["basics", "composer"]),
    ("narrow", narrow, "chat", []),
    ("foreign_origin", foreign_origin, "chat", []),
    ("phone_layout", phone_layout, "chat", []),
    ("touch_message_menu", touch_message_menu, "chat", []),
    ("projects_phone", projects_phone, "chat", []),
    ("phone_audit", audit, "chat", []),
    # Its daemon's agent keeps memory (test-web-page.sh).
    ("memory_changes", memory_changes, "memory", []),
    ("mcp_sign_in", mcp_sign_in, "oauth", []),
    # Its daemon's `local` runtime hosts MCP servers (test-web-page.sh).
    ("runtime_mcp", runtime_mcp, "runtime_mcp", []),
    ("acp_agents", acp_agents, "acp", []),
    # It restarts its daemon.
    ("restart_daemon", restart_daemon, "restart", []),
    ("install_prompt", install_prompt, "install", []),
    ("install_on_phones", install_on_phones, "install", []),
    ("panic_hook", panic_hook, "install", []),
    # It stops the other daemon.
    ("daemon_change", daemon_change, "install", []),
]


def plan(names):
    """The groups to run for scenarios `names` (all when empty), each the
    list of its scenarios in suite order with what they need."""
    known = {name: (group, needs) for name, _, group, needs in SCENARIOS}
    unknown = [name for name in names if name not in known]
    if unknown:
        sys.exit("unknown scenarios: %s (known: %s)" % (" ".join(unknown), " ".join(known)))
    chosen = set(names or known)
    pending = list(chosen)
    while pending:
        for need in known[pending.pop()][1]:
            if need not in chosen:
                chosen.add(need)
                pending.append(need)
    groups = {}
    for name, _, group, _ in SCENARIOS:
        if name in chosen:
            groups.setdefault(group, []).append(name)
    return list(groups.values())


def run(names):
    functions = {name: function for name, function, _, _ in SCENARIOS}
    browser = None
    try:
        for name in names:
            started = time.time()
            function = functions[name]
            SCENARIO[0] = name
            if function.__code__.co_argcount:
                if browser is None:
                    browser = Browser()
                    connect(browser)
                function(browser)
                browser.check()
            else:
                function()
            print("scenario %s passed in %.0f s" % (name, time.time() - started), flush=True)
    finally:
        if browser is not None:
            browser.close()


if PLAN:
    for group in plan(sys.argv[2:]):
        print(" ".join(group))
else:
    run(sys.argv[8:] or [name for name, _, _, _ in SCENARIOS])
