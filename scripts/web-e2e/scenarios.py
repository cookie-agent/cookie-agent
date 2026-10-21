"""Browser scenarios for the web client, driven over WebDriver (geckodriver).

Run by scripts/test-web-page.sh:

    python3 -I scenarios.py <driver-port> <allowing-link> <other-link> \
        <other-port> <allowing-home> <held-session> [screenshot-dir]

The allowing daemon has a scripted model (mock-model.py) that takes images,
an agent whose `bash` asks for approval, a `helper` subagent, and a `greet`
skill. `held-session` is a session another process made before the daemon
started, which the daemon lists but has not loaded. The other daemon is
empty; its page is of another origin. `COOKIE` in the environment is the
`cookie` binary, for the machine commands the badges need.
"""

import base64
import fcntl
import functools
import http.server
import json
import os
import random
import re
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

DRIVER = "http://127.0.0.1:%s" % sys.argv[1]
LINK, OTHER_LINK, OTHER_PORT, HOME, HELD_SESSION = sys.argv[2:7]
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
SCREENSHOTS = sys.argv[7] if len(sys.argv) > 7 else ""
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


class Browser:
    def __init__(self, user_agent=None, prefs=None):
        for _ in range(100):
            try:
                call("GET", "/status")
                break
            except OSError:
                time.sleep(0.1)
        options = {"args": ["-headless"]}
        options["prefs"] = dict(prefs or {})
        if user_agent:
            options["prefs"]["general.useragent.override"] = user_agent
        capabilities = {
            "browserName": "firefox",
            "moz:firefoxOptions": options,
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

    def on_element(self, selector, action, body):
        # The page can re-render the element between finding it and acting
        # on it; the selector then names its replacement, so find it again.
        for attempt in range(3):
            try:
                return call("POST", self.path("/element/%s/%s" % (self.find(selector), action)), body)
            except RuntimeError as error:
                if "stale element reference" not in str(error) or attempt == 2:
                    raise
                time.sleep(0.1)

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
        call("POST", self.path("/window/rect"), {"width": width, "height": height})

    def phone(self, width, height):
        """Shows the page as a `width`×`height` screen: Firefox windows are
        at least 500 px wide, so a narrower one is the 500 px window zoomed
        in (geckodriver runs with --allow-system-access for the zoom)."""
        zoom = max(1.0, 500 / width)
        chrome = 86  # what the headless window keeps above the page
        self.resize(max(width, 500), round(height * zoom) + chrome)
        call("POST", self.path("/moz/context"), {"context": "chrome"})
        try:
            self.script("gBrowser.selectedBrowser.fullZoom = arguments[0]", zoom)
        finally:
            call("POST", self.path("/moz/context"), {"context": "content"})
        size = self.script("return [innerWidth, innerHeight]")
        assert abs(size[0] - width) <= 3, "the page is %s px wide, not %s" % (size[0], width)

    def close(self):
        call("DELETE", self.path(""))


def send(browser, text, keys=ENTER):
    # A page that opened on a new session's cards starts it first.
    if count(browser, "#composer") == 0 and count(browser, "#new-session") == 1:
        start_cards(browser)
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


def start_cards(browser, workspace=None):
    """Starts the new session on its runtime cards: `local` at `workspace`
    (the daemon's own workspace) when its field is empty."""
    browser.wait("the runtime cards", lambda: count(browser, card("local/coding")) == 1)
    if browser.script("return document.querySelector(arguments[0]).dataset.selected", card("local/coding")) != "true":
        browser.click(card("local/coding") + " [data-card-toggle]")
    browser.wait("the workspace field", lambda: count(browser, field("local/coding")) == 1)
    if browser.script("return document.querySelector(arguments[0]).value", field("local/coding")) == "" or workspace:
        set_text(browser, field("local/coding"), workspace or WORKSPACE)
    browser.click("#start-session")
    browser.wait("the draft", lambda: count(browser, "#composer") == 1, timeout=40)


def new_session(browser):
    browser.click("aside [data-new-session]")
    browser.wait("the new session", lambda: "New session" in browser.text("#session-title"))
    start_cards(browser)


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
    # Under the checkout's target directory, or WEB_E2E_UPLOADS: a sandboxed
    # Firefox (a snap) may not see the system's temporary directory, nor a
    # checkout under it.
    uploads = os.environ.get("WEB_E2E_UPLOADS") or os.path.join(
        os.path.dirname(os.path.abspath(__file__)), "..", "..", "target", "web-e2e"
    )
    os.makedirs(uploads, exist_ok=True)
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
    browser.script(
        "const s = document.querySelector('#permission-action'); s.value = 'bash';"
        "s.dispatchEvent(new Event('change'))"
    )
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
    browser.script(
        "const s = document.querySelector('#permission-action'); s.value = 'read';"
        "s.dispatchEvent(new Event('change'))"
    )
    browser.type("#permission-pattern", "secrets/*")
    browser.click("#sheet [data-new-effect=deny]")
    browser.click("#permission-add")
    browser.wait("the draft rule", lambda: count(browser, '#sheet [data-rule="read secrets/*"]') == 1)
    print("previewed a draft rule for a new session")
    browser.shot("permissions-draft")
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
    the new session's runtime cards, which Start leaves for the composer."""
    browser.wait("the page", lambda: count(browser, "#composer, #new-session") > 0)
    if count(browser, "#new-session"):
        start_cards(browser)
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
    phone = Browser(prefs={"ui.primaryPointerCapabilities": 1, "ui.allPointerCapabilities": 1})
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
    # A new session's draft runtimes: one that never starts fails, a retry
    # fails again, and it can be taken out.
    new_session(browser)
    menu(browser, "runtime")
    browser.wait("the draft runtimes", lambda: count(browser, "#sheet .runtime-row") >= 2)
    browser.click("#runtime-attach")
    browser.type("#runtime-search", "box")
    browser.wait("the box runtime", lambda: count(browser, '#sheet [data-choice="box"]') == 1)
    browser.click('#sheet [data-choice="box"]')
    browser.click("#runtime-attach-confirm")
    browser.wait(
        "the failed draft runtime",
        lambda: "failed" in browser.text('#sheet .runtime-row:last-of-type'),
        timeout=40,
    )
    browser.shot("runtime-draft-failed")
    click_row_button(browser, "#sheet .runtime-row", "box/", "[data-retry]")
    browser.wait("the retry", lambda: "box" in browser.text("#status"))
    browser.wait("the retried runtime", lambda: "cannot start" in browser.text("#status"), timeout=40)
    click_row_button(browser, "#sheet .runtime-row", "box/", "[data-detach]")
    browser.wait("the dropped runtime", lambda: "box/" not in browser.text("#sheet"))
    print("a new session's runtime that cannot start fails, is retried, and is taken out")
    close_sheet(browser)

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
    first = browser.script("return document.querySelector('#page [data-memory=\"project://e2e/notes\"]')?.dataset.revision")
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
        with open(path) as file:
            return "second" in file.read()
    browser.wait("the overwritten file", overwritten)
    # The trash deletes the revision listed: wait for the overwrite's.
    browser.wait("the overwrite listed", lambda: browser.script(
        "return document.querySelector('#page [data-memory=\"project://e2e/notes\"]')?.dataset.revision") not in (None, first))
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

    # Add a runtime: an ssh template into config.toml, the dial-in
    # command, and the guides.
    browser.click("#machines-add")
    browser.wait("the add panel", lambda: count(browser, "#add-runtime") == 1)
    # A click and then keys, as a person types: the field keeps the focus
    # after the mouse button comes up. (A release once cleared the page's
    # selection, which in Chromium also takes a focused field's caret;
    # Firefox keeps it, so this only guards the focus here.)
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
    command = browser.script("return document.querySelector('#dial-in-command').value")
    assert command.startswith("cookie runtime serve --daemon ws://<daemon host>:") and "<the daemon's token>" in command, command
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
    hovered row, its three buttons cover the middle."""
    width = browser.script("return document.querySelector(arguments[0]).getBoundingClientRect().width", selector)
    origin = {ELEMENT: browser.find(selector)}
    perform(browser, [{
        "type": "pointer", "id": "mouse", "parameters": {"pointerType": "mouse"},
        "actions": [
            {"type": "pointerMove", "duration": 0, "origin": origin, "x": int(-width / 2 + 40), "y": 0},
            {"type": "pointerDown", "button": 0},
            {"type": "pointerUp", "button": 0},
        ],
    }])


def long_press(browser, selector, hold=800, drag=0):
    """A finger held on `selector` for `hold` ms, moved `drag` px down
    first (a scroll)."""
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
    browser.wait("the runtime cards", lambda: count(browser, card("global/global")) == 1)


def selected_cards(browser):
    return browser.script(
        "return [...document.querySelectorAll('#new-session [data-card][data-selected=true]')].map((c) => c.dataset.card)"
    )


def runtime_cards(browser):
    """The new session's runtime cards: groups by machine that fold and are
    remembered, two runtimes at their own workspaces, one runtime at two,
    a refusal on its card, an offline card, global deselected, no coding
    card at all, the defaults following the open session, and every path
    under its runtime's home."""
    open_cards(browser)
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
    if count(browser, "#new-session") == 0:
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
    assert browser.script("return document.querySelector('#start-session').disabled"), "Start with an empty field"
    set_text(browser, field("local/coding", 1), "~/second")
    # `alt` has one card, `alt` until it ran and `alt/coding` after.
    alt = "alt" if count(browser, card("alt")) else "alt/coding"
    browser.click(card(alt) + " [data-card-toggle]")
    browser.wait("the alt field", lambda: count(browser, field(alt)) == 1)
    set_text(browser, field(alt), elsewhere)
    browser.wait("the prefix note", lambda: "local_workspace_bash" in browser.text("#prefix-note"))
    assert "3 workspaces" in browser.text("#prefix-note"), browser.text("#prefix-note")
    browser.shot("cards-two-runtimes")
    # A refusal shows on its card, and nothing is created.
    sessions_before = count(browser, "aside [data-session-row]")
    browser.click(card("box") + " [data-card-toggle]")
    set_text(browser, field("box"), WORKSPACE)
    browser.click("#start-session")
    browser.wait("the refusal", lambda: browser.text(card("box") + " [data-card-error]") != "", timeout=40)
    browser.script("document.querySelector(arguments[0]).scrollIntoView({block: 'center'})", card("box"))
    browser.shot("cards-refusal")
    assert count(browser, "#new-session") == 1 and count(browser, "aside [data-session-row]") == sessions_before
    print("a refused workspace shows on its card:", browser.text(card("box") + " [data-card-error]"))
    browser.click(card("box") + " [data-card-toggle]")
    browser.click("#start-session")
    browser.wait("the draft", lambda: count(browser, "#composer") == 1, timeout=60)
    places = browser.text("#draft-places [data-draft-places]")
    assert places == "~/workspace, ~/second, alt:~/elsewhere", places
    assert "~/workspace" in browser.text("#places"), browser.text("#places")
    send(browser, "hello from three workspaces")
    browser.wait("the session", lambda: browser.text("#session-title") != "New session" and answers(browser) >= 1, timeout=60)
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

    # The defaults follow the open session.
    open_cards(browser)
    for name, is_open in browser.script(
        "return [...document.querySelectorAll('#new-session [data-group]')].map((g) => [g.dataset.group, g.dataset.open])"
    ):
        if is_open != "true":
            browser.click('#new-session [data-group-toggle="%s"]' % name)
    alt = "alt/coding" if count(browser, card("alt/coding")) else "alt"
    browser.wait("the preselected cards", lambda: field_values(browser, "local/coding") == ["~/workspace", "~/second"])
    assert field_values(browser, alt) == ["~/elsewhere"], field_values(browser, alt)
    assert "global/global" in selected_cards(browser)
    print("a new session preselects the open session's runtimes and workspaces")

    # One runtime alone: its machine, and its workspace under its home.
    browser.click(card("local/coding") + " [data-card-toggle]")
    browser.click("#start-session")
    browser.wait("the draft", lambda: count(browser, "#composer") == 1, timeout=40)
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
    assert not browser.script("return document.querySelector('#start-session').disabled")
    browser.click("#start-session")
    browser.wait("the draft", lambda: count(browser, "#composer") == 1, timeout=40)
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

    # Back to a session in the daemon's own workspace for what follows.
    open_cards(browser)
    if "global/global" not in selected_cards(browser):
        browser.click(card("global/global") + " [data-card-toggle]")
    start_cards(browser, WORKSPACE)


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
    click_row(browser, rows["one machine"] + " [data-session]")
    browser.wait("the session", lambda: count(browser, "#new-session") == 0)
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
    assert browser.script("return document.activeElement.id") == "session-title-field"
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
    for name in ("agents", "settings", "machines", "memory", "chat"):
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
        "rename", "compact", "goal", "skills", "background", "runtime", "permissions", "usage", "events", "preset",
    ], names
    for name, text, icon in entries:
        assert icon and "/" not in text and "\n" in text, (name, text, icon)
    browser.shot("session-menu")
    close_sheet(browser)
    click_row(browser, "aside nav[aria-label=Sessions] [data-session]")
    browser.wait("an open session", lambda: browser.text("#session-title") != "New session")
    # Rename edits the title in place (renaming); the others open sheets.
    for name in names[1:]:
        menu(browser, name)
        browser.wait("the %s sheet" % name, lambda: count(browser, "#sheet [data-command]") == 0 and count(browser, "#sheet") == 1)
        assert page_view(browser) == "chat"
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
        # Choosing a view in the drawer's rail closes the drawer.
        phone.click("main [data-sidebar-toggle]")
        phone.click("#drawer [data-view=settings]")
        phone.wait("the settings view", lambda: page_view(phone) == "settings" and count(phone, "#drawer") == 0)
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
        # The cards stack in one column, Start pinned at the foot.
        phone.wait("the cards", lambda: count(phone, "#new-session [data-card]") > 1)
        lefts = phone.script(
            "return [...new Set([...document.querySelectorAll('#new-session [data-card]')]"
            ".map((c) => Math.round(c.getBoundingClientRect().left)))]"
        )
        assert len(lefts) == 1, "the cards are not one column: %s" % lefts
        foot = phone.script(
            "return innerHeight - document.querySelector('#new-session [data-start-bar]').getBoundingClientRect().bottom"
        )
        assert 0 <= foot < 40, "Start is %s px above the foot" % foot
        heights = phone.script(
            "return [...document.querySelectorAll('#new-session [data-workspace-field]')].map((f) => f.getBoundingClientRect().height)"
        )
        assert heights and min(heights) >= 44, heights
        phone.shot("phone-cards")
        print("phone: the cards stack in one column with Start at the foot")
        start_cards(phone)
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
        start_cards(phone)
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
    for name in ("agents", "settings", "machines", "memory")
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
            time.sleep(1)
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
                    time.sleep(0.6)
                    phone.script(
                        "const row = document.querySelector('#drawer [data-session][aria-current=true]');"
                        "if (row && row.getAttribute('aria-expanded') !== 'true') row.click()"
                    )
                elif isinstance(command, list):
                    for selector in command:
                        time.sleep(0.6)
                        phone.script("document.querySelector(arguments[0]).click()", selector)
                elif command:
                    send(phone, command)
                time.sleep(1.2)
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
    """Chromium's prompt, which Firefox does not have, handed to the page by
    a script: Settings offers Install app, which opens it once; a dismissed
    prompt leaves the browser's menu, and an install hides the action."""
    browser = Browser()
    try:
        browser.resize(1280, 900)
        browser.go(LINK)
        browser.wait("the connection", lambda: browser.text("#connected") != "")
        browser_section(browser)
        # Firefox hands the page no prompt: its menu.
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
    a script) and iOS's steps (Firefox posing as Safari on an iPhone), in
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
    with Move to project…; folding remembered; the editor's unsaved-edit
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
    browser.wait("the new session", lambda: count(browser, "#new-session-project") == 1)
    assert browser.script("return document.querySelector('#new-session-project').value") == project
    browser.wait("the prefilled card", lambda: field_values(browser, "local/coding") == ["~/workspace"])
    assert "global/global" in selected_cards(browser)
    browser.shot("project-new-session")
    browser.click("#start-session")
    browser.wait("the draft", lambda: count(browser, "#composer") == 1, timeout=40)
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
    # Move to project… sends one back out.
    row = 'aside [data-session-row="%s"]' % second
    hover(browser, "#transcript")
    before = browser.script(ROW_LAYOUT, row)
    hover(browser, row)
    browser.wait("the move button", lambda: shown(browser, row + " [data-move]"))
    # The buttons lie over the row's end: its title, time, and badges keep
    # their layout.
    time.sleep(0.3)
    assert browser.script(ROW_LAYOUT, row) == before, (before, browser.script(ROW_LAYOUT, row))
    order = browser.script(
        "return [...document.querySelectorAll(arguments[0] + ' [data-row-actions] button')].map((b) => b.getAttribute('aria-label'))",
        row,
    )
    assert order[:2] == ["Move to project…", "Rename the session"], order
    browser.shot("project-row-buttons")
    browser.click(row + " [data-move]")
    browser.wait("the picker", lambda: count(browser, "#sheet [data-move-to]") >= 2)
    browser.shot("project-move-picker")
    browser.click('#sheet [data-move-to="none"]')
    browser.wait("the moved session", lambda: section_of(browser, second) == "none")
    print("Move to project… moved a session out of its project")

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


def projects_phone():
    """Projects on a phone: a long press on a header shows its buttons, a
    long press on a row offers Move, Rename, and Delete, and a finger drags
    a row onto a header."""
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
        phone.wait("the row's actions", lambda: count(phone, "#drawer [data-delete-actions] [data-move-reveal]") == 1)
        labels = phone.script(
            "return [...document.querySelectorAll('#drawer [data-delete-actions] button')].map((b) => b.getAttribute('aria-label'))"
        )
        assert labels == ["Move to project…", "Rename", "Delete", "Cancel"], labels
        # A narrow phone shows the four as icons, on one line.
        tops = phone.script(
            "return [...new Set([...document.querySelectorAll('#drawer [data-delete-actions] button')].map((b) => Math.round(b.getBoundingClientRect().top)))]"
        )
        assert len(tops) == 1, "the row's actions wrap: %s" % tops
        fits(phone, "the row's actions")
        phone.shot("phone-row-actions")
        phone.click("#drawer [data-actions-cancel]")
        drag_row(phone, '#drawer [data-session-row="%s"] [data-session]' % row, target + " [data-section-toggle]", touch=True)
        phone.wait("the moved row", lambda: phone.script(
            "const r = document.querySelector('#drawer [data-session-row=\"' + arguments[0] + '\"]');"
            "return r && r.closest('[data-project-section]').dataset.projectSection", row) == "project:" + project)
        print("phone: long presses show the header's and the row's buttons; a finger drags a row onto a header")
    finally:
        phone.close()


def main():
    browser = Browser()
    try:
        browser.resize(1280, 900)
        browser.go(LINK)
        browser.wait("the connection", lambda: browser.text("#connected") != "")
        assert browser.script("return location.hash") == "", "the token stayed in the address bar"
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

        composer(browser)
        tool_rows(browser)
        pinned_rows(browser)
        message_menu(browser)
        session_depth(browser)
        management(browser)
        views_2026(browser)
        suspended_tab(browser)
        foreign_session(browser)

        desktop_layout(browser)
        runtime_cards(browser)
        session_badges(browser)
        renaming(browser)
        deleting(browser)
        projects(browser)

        # A phone-sized window: no sideways scroll, the sessions in a drawer.
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

        # The other daemon's page, of a foreign origin, connects to this
        # daemon: the token is the guard.
        browser.resize(1280, 900)
        page, fragment = LINK.split("#")
        token = urllib.parse.parse_qs(fragment)["token"][0]
        browser.go(OTHER_PAGE)
        browser.script(REMEMBER_DAEMON, "ws" + page[len("http"):] + "ws", token)
        browser.go(OTHER_PAGE)
        browser.wait("the connection from a foreign origin", lambda: browser.text("#connected") != "")
        print("a page of a foreign origin connects with the token")
        # A connection that fails before its handshake says what to check.
        browser.script(REMEMBER_DAEMON, UNREACHABLE, token)
        browser.go(OTHER_PAGE)
        browser.wait("the refusal hint", lambda: "Check the daemon URL" in browser.text())
        print("a connection refused before its handshake points at the URL and the daemon")
        browser.shot("refused")
    finally:
        browser.close()


main()
# geckodriver runs one browser at a time: the phone sizes get theirs last.
phone_layout()
touch_message_menu()
projects_phone()
problems = phone_audit()
for problem in problems:
    print("phone:", problem)
assert not problems or os.environ.get("PHONE_AUDIT_REPORT_ONLY"), "the phone layout falls short"
print("every view fits the phone sizes with touch-sized targets")
install_prompt()
install_on_phones()
# Last: it stops the other daemon.
daemon_change()
