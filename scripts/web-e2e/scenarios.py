"""Browser scenarios for the web client, driven over WebDriver (geckodriver).

Run by scripts/test-web-page.sh:

    python3 -I scenarios.py <driver-port> <allowing-link> <refusing-link> \
        <refusing-port> <allowing-home> <held-session> [screenshot-dir]

The allowing daemon has a scripted model (mock-model.py) that takes images,
an agent whose `bash` asks for approval, a `helper` subagent, and a `greet`
skill. `held-session` is a session another process made before the daemon
started, which the daemon lists but has not loaded.
"""

import base64
import fcntl
import json
import os
import random
import re
import struct
import sys
import time
import urllib.error
import urllib.request
import zlib

DRIVER = "http://127.0.0.1:%s" % sys.argv[1]
LINK, REFUSING_LINK, REFUSING_PORT, HOME, HELD_SESSION = sys.argv[2:7]
SCREENSHOTS = sys.argv[7] if len(sys.argv) > 7 else ""
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


def new_session(browser):
    browser.click("aside [data-new-session]")
    browser.wait("the new session", lambda: "New session" in browser.text("#session-title"))


def answers(browser):
    return len(re.findall(r"\bok\b", browser.text("#transcript")))


def clear_composer(browser):
    browser.script(
        "const c = document.querySelector('#composer'); c.value = '';"
        "c.dispatchEvent(new Event('input'))"
    )


def session_depth(browser):
    """Subagents, history edits, goals, background calls, media, skills, and
    permission overrides."""
    # A delegation: the tree panel shows the child; its session_ref opens it.
    new_session(browser)
    send(browser, "delegate the work")
    browser.wait("the delegation", lambda: "The tool ran." in browser.text("#transcript"), timeout=60)
    # The finished child is no longer live, so `auto` hides the panel.
    send(browser, "/agent-panel show")
    browser.wait("the agent tree", lambda: "helper" in browser.text("#agent-tree"))
    browser.shot("subagent-tree")
    root_title = browser.text("#session-title")
    browser.click("#transcript [data-action=toggle-tool]")
    browser.wait(
        "the session reference",
        lambda: count(browser, "#transcript [data-action=open-session]") > 0,
    )
    browser.click("#transcript [data-action=open-session]")
    browser.wait("the child session", lambda: "child task" in browser.text("#transcript"))
    assert "helper" in browser.text("#session-title"), browser.text("#session-title")
    print("the delegation shows in the tree; its session reference opens the child")
    browser.shot("subagent")
    browser.click("#agent-tree [data-tree-session]")
    browser.wait("back at the root", lambda: browser.text("#session-title") == root_title)
    browser.click("#agent-tree button[aria-label=Fold]")
    browser.wait("the folded tree", lambda: "helper" not in browser.text("#agent-tree"))
    print("the tree folds")

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

    # Compact the context.
    send(browser, "/compact keep the greetings")
    browser.wait(
        "the compaction",
        lambda: any(
            word in browser.text("#status")
            for word in ("context compacted", "checkpoint", "compaction failed")
        ),
        timeout=60,
    )
    print("compact:", browser.text("#status"))
    browser.shot("compact")

    # A goal, then pause it.
    send(browser, "/goal finish the scenario")
    browser.wait("the goal bar", lambda: "finish the scenario" in browser.text("#goal-bar"))
    browser.shot("goal")
    browser.wait("the pause control", lambda: count(browser, "#goal-bar [data-goal-action=pause]") == 1)
    browser.click("#goal-bar [data-goal-action=pause]")
    browser.wait("the paused goal", lambda: count(browser, "#goal-bar [data-goal-action=resume]") == 1)
    print("set a goal and paused it")
    browser.shot("goal-paused")
    browser.wait("the goal run's end", lambda: "working" not in browser.text("#status-bar"), timeout=60)

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
    send(browser, "/background")
    browser.wait("the background sheet", lambda: count(browser, "#sheet [data-stop]") > 0)
    browser.shot("background-sheet")
    browser.click("#sheet [data-stop]")
    browser.wait("the stopped call", lambda: count(browser, "#background-bar") == 0, timeout=40)
    print("moved a call to the background, listed it, and stopped it")
    browser.wait("the run's end", lambda: "working" not in browser.text("#status-bar"), timeout=60)

    # Attach an image; one too large is offered downscaled.
    # Under the checkout's target directory: a sandboxed Firefox may not see
    # the system's temporary directory.
    uploads = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "target", "web-e2e")
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
    send(browser, "/skills gre")
    browser.wait("the skills", lambda: count(browser, "#sheet [data-skill=greet]") == 1)
    browser.click("#sheet [data-skill=greet]")
    browser.type("#skill-args", "Ada")
    browser.shot("skills")
    browser.click("#skill-run")
    browser.wait("the skill", lambda: "greet" in browser.text("#transcript").lower() and "skill" in browser.text("#transcript").lower())
    print("ran the greet skill")
    browser.wait("the skill run's end", lambda: "working" not in browser.text("#status-bar"), timeout=60)

    # A permission override, then its removal.
    send(browser, "/permissions")
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
    send(browser, "/permissions")
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
        browser.click('aside [data-session="%s"]' % HELD_SESSION)
        browser.wait("the read-only snapshot", lambda: "read-only snapshot" in browser.text("main"))
        browser.wait(
            "the snapshot's transcript",
            lambda: "hello from another process" in browser.text("#transcript"),
        )
        assert browser.script("return document.querySelector('#composer').disabled")
        print("a session another process holds opens as a read-only snapshot")
        browser.shot("read-only")


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
        clear_composer(browser)

        session_depth(browser)
        foreign_session(browser)

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
