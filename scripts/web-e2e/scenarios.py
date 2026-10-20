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
import signal
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

    # A goal, paused while its run streams. The engine tells the agent of a
    # pause only while the session runs (an idle one has no one to tell), so
    # the objective makes the scripted model answer slowly, and the pause
    # waits for that answer to stream: pausing between the goal's runs would
    # send no control message.
    send(browser, "/goal finish the scenario slowly")
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
    send(browser, "/runtime")
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
    browser.click("aside nav[aria-label=Sessions] button")
    browser.wait("an open session", lambda: browser.text("#session-title") != "New session")
    send(browser, "/runtime")
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
    send(browser, "/mcp")
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
    send(browser, "/connect")
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
    send(browser, "/config")
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
    # made meanwhile, and deleted.
    send(browser, "/memory")
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
    with open(path) as file:
        assert "second" in file.read()
    browser.click('#page [data-memory="project://e2e/notes"] [data-memory-delete]')
    browser.click("#memory-delete-confirm")
    browser.wait("the deleted memory", lambda: count(browser, '#page [data-memory="project://e2e/notes"]') == 0)
    print("a memory was refused, created, overwritten over a conflict, and deleted")
    close_sheet(browser)

    # Restart the reference plugin.
    send(browser, "/plugins")
    browser.wait("the plugin", lambda: count(browser, '#page [data-plugin="reference"]') == 1)
    browser.wait("its start", lambda: browser.text('#page [data-plugin="reference"] .plugin-state') == "connected", timeout=40)
    browser.shot("plugins")
    browser.click('#page [data-plugin="reference"] [data-plugin-restart]')
    close_sheet(browser)
    browser.wait("the restart", lambda: "is restarting" in browser.text("#status"))
    print("restarted the reference plugin")


def shown(browser, selector):
    """Whether `selector` matches an element the page lays out."""
    return browser.script(
        "const e = document.querySelector(arguments[0]);"
        "return !!e && e.getClientRects().length > 0",
        selector,
    )


def page_view(browser):
    return browser.script("const p = document.querySelector('#page'); return p ? p.dataset.view : 'chat'")


def desktop_layout(browser):
    """The rail's views, slash commands that open them, the session menu,
    the sidebar toggle, the theme toggle, and the bottom bar's pickers."""
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

    # Daemon-wide commands open views; the session's own open sheets.
    new_session(browser)
    for command, name, section in (
        ("/config", "settings", "configuration"),
        ("/connect", "settings", "providers"),
        ("/mcp", "settings", "mcp"),
        ("/plugins", "settings", "plugins"),
        ("/memory", "memory", None),
    ):
        send(browser, command)
        browser.wait("the %s view" % command, lambda: page_view(browser) == name)
        if section:
            browser.wait(
                "the %s section" % section,
                lambda: count(browser, "#page [data-section-body=%s]" % section) == 1,
            )
        close_sheet(browser)
    send(browser, "/permissions")
    browser.wait("the permissions sheet", lambda: count(browser, "#sheet") == 1)
    assert page_view(browser) == "chat"
    close_sheet(browser)
    browser.click("button[aria-label=Commands]")
    browser.wait("the session menu", lambda: count(browser, "#sheet [data-command]") > 0)
    names = browser.script(
        "return [...document.querySelectorAll('#sheet [data-command]')].map((b) => b.dataset.command)"
    )
    for name in ("runtime", "permissions", "goal", "background", "usage", "compact", "skills", "events"):
        assert name in names, (name, names)
    for name in ("config", "mcp", "connect", "memory", "plugins"):
        assert name not in names, (name, names)
    browser.shot("session-menu")
    close_sheet(browser)
    print("slash commands open views; the ⋮ menu lists only the session's commands")

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
    browser.click("aside nav[aria-label=Sessions] button")
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
    before = browser.script("return document.querySelector('#permission-mode').getAttribute('aria-label')")
    browser.click("#permission-mode")
    browser.wait(
        "the next mode",
        lambda: browser.script("return document.querySelector('#permission-mode').getAttribute('aria-label')") != before,
    )
    for _ in range(4):
        browser.click("#permission-mode")
    browser.wait(
        "the mode again",
        lambda: browser.script("return document.querySelector('#permission-mode').getAttribute('aria-label')") == before,
    )
    print("the bottom bar's agent, model, and mode pickers work")


def phone_layout():
    """The phone's drawer, Back, the bottom sheets, the mode toast, and the
    bottom bar that gives way to the keyboard."""
    phone = Browser()
    try:
        phone.go(LINK)
        phone.wait("the connection", lambda: phone.text("#connected") != "")
        phone.phone(390, 844)
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

        # Back to Chat and the latest session, from the drawer.
        phone.click("#page [data-sidebar-toggle]")
        phone.click("#drawer nav[aria-label=Sessions] button")
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
        # The mode toggle is an icon with a toast.
        phone.click("#permission-mode")
        phone.wait("the mode toast", lambda: phone.text("#mode-toast").startswith("now: "))
        phone.shot("phone-mode-toast")
        for _ in range(4):
            phone.click("#permission-mode")
        print("phone: the bottom bar is one row; pickers are bottom sheets; the mode toggle toasts")
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
  // Cut off by the edge of the screen, which `overflow: hidden` hides.
  if (rect.right > innerWidth + 1 || rect.left < -1) {
    small.push('cut off: ' + element.tagName.toLowerCase() + ' "'
      + (element.innerText || element.getAttribute('aria-label') || '').slice(0, 30) + '"');
    continue;
  }
  const tooShort = rect.height < 44;
  const tooNarrow = element.tagName === 'BUTTON' && rect.width < 44;
  if (tooShort || tooNarrow) {
    small.push(element.tagName.toLowerCase() + (element.id ? '#' + element.id : '') + ' "'
      + (element.innerText || element.value || element.getAttribute('aria-label') || '').slice(0, 30)
      + '" ' + Math.round(rect.width) + 'x' + Math.round(rect.height));
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
SHEETS = (
    "/agent", "/model", "/preset", "/approvals", "/usage", "/events", "/goal",
    "/background", "/skills", "/permissions", "/runtime",
)
SCRIPTED = [
    ("view-%s" % name, ["[data-rail=primary] [data-view=%s]" % name])
    for name in ("agents", "settings", "machines", "memory")
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
            phone.wait("the drawer", lambda: count(phone, "#drawer nav button") > 0)
            phone.click("#drawer nav[aria-label=Sessions] button")
            phone.wait("the transcript", lambda: phone.text("#transcript") != "")
            time.sleep(1)
            views = [("transcript", None)] + [(command[1:], command) for command in SHEETS]
            views += [("commands", "menu"), ("drawer", "drawer")] + SCRIPTED
            for view, command in views:
                close_sheet(phone)
                if command == "menu":
                    phone.click("button[aria-label=Commands]")
                elif command == "drawer":
                    phone.click("main [data-sidebar-toggle]")
                elif isinstance(command, list):
                    for selector in command:
                        time.sleep(0.6)
                        phone.script("document.querySelector(arguments[0]).click()", selector)
                elif command:
                    send(phone, command)
                time.sleep(1.2)
                overflow = phone.script(OVERFLOW)
                small = phone.script(SMALL_TARGETS)
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
    """The page names a web app manifest whose icons the daemon serves, for
    adding the page to a phone's home screen."""
    base = LINK.split("#")[0]
    with urllib.request.urlopen(base + "manifest.webmanifest") as response:
        assert response.headers["Content-Type"] == "application/manifest+json"
        manifest = json.load(response)
    assert manifest["display"] == "standalone"
    purposes = set()
    for icon in manifest["icons"]:
        with urllib.request.urlopen(base + icon["src"]) as response:
            assert response.headers["Content-Type"] == icon["type"], icon
            assert response.read(), icon
        purposes.add(icon["purpose"])
    assert purposes == {"any", "maskable"}, purposes
    print("the manifest and its %d icons are served" % len(manifest["icons"]))


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
        assert browser.script(
            "return document.querySelector('link[rel=manifest]').getAttribute('href')"
        ) == "manifest.webmanifest"
        home_screen()

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
        management(browser)
        suspended_tab(browser)
        foreign_session(browser)

        desktop_layout(browser)

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
# geckodriver runs one browser at a time: the phone sizes get theirs last.
phone_layout()
problems = phone_audit()
for problem in problems:
    print("phone:", problem)
assert not problems or os.environ.get("PHONE_AUDIT_REPORT_ONLY"), "the phone layout falls short"
print("every view fits the phone sizes with touch-sized targets")
