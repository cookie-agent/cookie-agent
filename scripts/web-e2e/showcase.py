"""Photographs the web client for the landing page, driven over WebDriver
(chromedriver, headless Chrome).

Run by scripts/web-screenshots.sh:

    python3 -I showcase.py <driver-port> <link> <output-dir> [all]

It starts a session, sends one request the scripted model
(showcase-model.py) answers with an edit, a `bash` check, and a markdown
reply, opens both tool calls, and saves `web-light.png` and `web-dark.png`
at 1280×800, and `web-phone-light.png` and `web-phone-dark.png` at 390×844.

With `all`, it also opens every view of the rail, every Settings section,
an editor, every sheet and menu, and the phone's drawer, at desktop,
phone, and phone-landscape sizes, and saves each in light and dark under
`<output-dir>/all/`, then the connect screen, for reviewing the page's look
as a whole. First it starts three more sessions on the runtime cards, so
the sessions list shows every kind of badge, and it photographs a row's
title and the title bar's as fields.

In the environment, `CHROME` and `CHROME_ARGS` (a JSON list) are the
browser and its switches.
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
COMPOSER = sys.argv[4:5] == ["composer"]
ELEMENT = "element-6066-11e4-a52e-4f735466cecf"
ENTER = "\ue007"  # WebDriver key: Enter
# What a desktop with a mouse reports, which headless Chrome does not:
# Tailwind's hover: styles apply only under (hover: hover).
DESKTOP_POINTER = (
    "--blink-settings=primaryHoverType=2,availableHoverTypes=2,primaryPointerType=4,availablePointerTypes=4"
)
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
        args = json.loads(os.environ.get("CHROME_ARGS") or '["--headless=new"]')
        args += ["--window-size=1280,800", "--hide-scrollbars", DESKTOP_POINTER]
        options = {"args": args}
        if os.environ.get("CHROME"):
            options["binary"] = os.environ["CHROME"]
        capabilities = {"browserName": "chrome", "goog:chromeOptions": options}
        self.session = call("POST", "/session", {"capabilities": {"alwaysMatch": capabilities}})[
            "sessionId"
        ]
        # Chrome's own install prompt would put its banner in the shots.
        call(
            "POST",
            self.path("/goog/cdp/execute"),
            {
                "cmd": "Page.addScriptToEvaluateOnNewDocument",
                "params": {
                    "source": "addEventListener('beforeinstallprompt',"
                    " (e) => { if (e.isTrusted) e.stopImmediatePropagation(); }, true)"
                },
            },
        )

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
        """Shows the page in a `width`×`height` viewport, which the shots
        then are, by Chrome's device emulation."""
        call(
            "POST",
            self.path("/goog/cdp/execute"),
            {
                "cmd": "Emulation.setDeviceMetricsOverride",
                "params": {"width": width, "height": height, "deviceScaleFactor": 0, "mobile": False},
            },
        )

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

    def shot(self, name, selector=None):
        """Saves the page, or only the element `selector` matches."""
        time.sleep(0.8)
        os.makedirs(os.path.dirname(os.path.join(OUT, name)), exist_ok=True)
        path = "/screenshot"
        if selector:
            path = "/element/%s/screenshot" % self.find(selector)
        with open(os.path.join(OUT, name), "wb") as file:
            file.write(base64.b64decode(call("GET", self.path(path))))
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
# The runtime cards, from the "Runtimes…" chip: `global`, and `local` at the
# greeter.
browser.wait("the runtimes chip", lambda: browser.count("#runtimes-chip") == 1)
browser.click("#runtimes-chip")
browser.wait("the cards", lambda: browser.count('#new-session [data-card="local/coding"]') > 0)
if browser.count('#new-session [data-card="local/coding"][data-selected=true]') == 0:
    browser.click('#new-session [data-card="local/coding"] [data-card-toggle]')
browser.wait("the field", lambda: browser.count('#new-session [data-card="local/coding"] [data-workspace-field]') > 0)
browser.script(
    "const f = document.querySelector('#new-session [data-card=\"local/coding\"] [data-workspace-field]');"
    "f.value = '~/greeter'; f.dispatchEvent(new Event('input'))"
)
browser.click("#start-session")
browser.wait("the draft", lambda: browser.count("#new-session") == 0)
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
    # The exchange from the user's message on: the message at the top.
    browser.script(
        "const t = document.querySelector('#transcript');"
        "const m = t.querySelector('.user-message');"
        "t.scrollTop += m.getBoundingClientRect().top - t.getBoundingClientRect().top - 8"
    )
    browser.shot("web-phone-%s.png" % theme)

# Every sheet and menu of the chat view: the bottom bar's pickers, then the
# ⋮ menu's entries, each by its clicks.
COMMANDS = [
    ("agent", ["#agent-chip"]),
    ("model", ["#model-chip"]),
    ("modes", ["#permission-mode"]),
] + [
    (name, ["main button[aria-label=Commands]", "#sheet [data-command=%s]" % name])
    for name in ("compact", "goal", "skills", "background", "runtime", "permissions", "usage", "events", "preset")
]
CLICKS = [
    ("commands", "button[aria-label=Commands]"),
    ("message-menu", "#transcript [data-action=user-menu]"),
    ("connection", "[data-rail] button[aria-haspopup=dialog]"),
    ("drawer", "main [data-sidebar-toggle]"),
]
# The rail's views, and the Settings sections.
VIEWS = ["agents", "settings", "machines", "memory"]
SECTIONS = ["configuration", "providers", "mcp", "plugins", "browser"]


def view(name):
    browser.script("document.querySelector('[data-rail] [data-view=%s]').click()" % name)
    time.sleep(1.2)


def close_all():
    # Project folders start folded at every size.
    browser.script("localStorage.removeItem('cookie.memory_project:greeter')")
    browser.script(
        "const s = document.querySelector('#sheet h2 ~ button'); if (s) s.click();"
        "const c = document.querySelector('#connection-close'); if (c) c.click();"
        "const d = document.querySelector('[data-drawer-backdrop]'); if (d) d.click();"
    )
    time.sleep(0.3)
    view("chat")
    # A phone's rail opens Chat on its sessions, in the drawer.
    browser.script("const d = document.querySelector('[data-drawer-backdrop]'); if (d) d.click();")
    time.sleep(0.3)
    browser.script(
        "const c = document.querySelector('#composer');"
        "if (c) { c.value = ''; c.dispatchEvent(new Event('input')); c.blur() }"
    )
    time.sleep(0.3)


def click(selector):
    browser.script("document.querySelector(arguments[0]).click()", selector)
    time.sleep(0.8)


def drag_and_shoot(source, target, name):
    """Drags `source` with the mouse until it is over `target`, photographs
    it there in both themes, then takes it back and lets go where it
    started, so nothing moves."""
    start = {ELEMENT: browser.find(source)}
    over = {ELEMENT: browser.find(target)}
    # Where it started, in the viewport: the lifted row moves with the
    # pointer, so its own centre would be over the target by then.
    x, y = browser.script(
        "const r = document.querySelector(arguments[0]).getBoundingClientRect();"
        "return [Math.round(r.left + r.width / 2) - 120, Math.round(r.top + r.height / 2)]",
        source,
    )
    mouse = {"type": "pointer", "id": "mouse", "parameters": {"pointerType": "mouse"}}
    call("POST", browser.path("/actions"), {"actions": [dict(mouse, actions=[
        {"type": "pointerMove", "duration": 0, "origin": start, "x": -120, "y": 0},
        {"type": "pointerDown", "button": 0},
        {"type": "pointerMove", "duration": 200, "origin": start, "x": -100, "y": 20},
        {"type": "pointerMove", "duration": 300, "origin": over, "x": -100, "y": 10},
    ])]})
    both_themes(name)
    call("POST", browser.path("/actions"), {"actions": [dict(mouse, actions=[
        {"type": "pointerMove", "duration": 200, "origin": "viewport", "x": x, "y": y},
        {"type": "pointerUp", "button": 0},
    ])]})
    call("DELETE", browser.path("/actions"))


def both_themes(name):
    for theme in ("light", "dark"):
        browser.theme(theme)
        browser.shot("all/%s-%s.png" % (name, theme))


SIZES = (("desktop", (1280, 800)), ("phone", (390, 844)), ("landscape", (844, 390)))


def badge_session(note, picks):
    """Starts a session on the runtime cards with `global` and `picks`,
    (runtime, workspace) pairs, and sends `note`, which titles it."""
    click("aside [data-new-session]")
    browser.wait("the runtimes chip", lambda: browser.count("#runtimes-chip") == 1)
    click("#runtimes-chip")
    browser.wait("the cards", lambda: browser.count("#new-session [data-card]") > 0)
    browser.script(
        "for (const g of document.querySelectorAll('#new-session [data-group][data-open=false] [data-group-toggle]'))"
        " g.click()"
    )
    time.sleep(0.5)
    browser.script(
        "for (const c of document.querySelectorAll('#new-session [data-card][data-selected=true]'))"
        " if (c.dataset.card !== 'global/global') c.querySelector('[data-card-toggle]').click()"
    )
    if browser.count('#new-session [data-card="global/global"][data-selected=true]') == 0:
        click('#new-session [data-card="global/global"] [data-card-toggle]')
    for runtime, workspace in picks:
        key = runtime + "/coding" if browser.count('#new-session [data-card="%s/coding"]' % runtime) else runtime
        card = '#new-session [data-card="%s"]' % key
        if browser.count(card + "[data-selected=true]") == 0:
            click(card + " [data-card-toggle]")
        index = browser.count(card + " [data-workspace-field]") - 1
        if browser.script("return document.querySelector(arguments[0]).value", card + ' [data-workspace-field="%d"]' % index):
            click(card + " [data-add-workspace]")
            index += 1
        browser.script(
            "const f = document.querySelector(arguments[0]); f.value = arguments[1];"
            "f.dispatchEvent(new Event('input'))",
            card + ' [data-workspace-field="%d"]' % index,
            workspace,
        )
    click("#start-session")
    browser.wait("the draft", lambda: browser.count("#new-session") == 0)
    browser.type("#composer", note)
    browser.type("#composer", ENTER)
    title = note[len("Note: "):].rstrip(".")
    browser.wait("the title " + title, lambda: browser.text("#session-title") == title)


def row_of(title, scope="#sidebar"):
    """The session row titled `title` in the sidebar, or `scope`."""
    session = browser.script(
        "const r = [...document.querySelectorAll(arguments[1] + ' [data-session-row]')]"
        "  .find((r) => r.querySelector('[data-session-title]').textContent === arguments[0]);"
        "return r ? r.dataset.sessionRow : null",
        title,
        scope,
    )
    return '%s [data-session-row="%s"]' % (scope, session)


def field_value(selector, value):
    browser.script(
        "const f = document.querySelector(arguments[0]); f.value = arguments[1];"
        "f.dispatchEvent(new Event('input'))",
        selector,
        value,
    )
    time.sleep(0.4)


def long_press(selector):
    origin = {ELEMENT: browser.find(selector)}
    call("POST", browser.path("/actions"), {"actions": [{
        "type": "pointer", "id": "finger", "parameters": {"pointerType": "touch"},
        "actions": [
            {"type": "pointerMove", "duration": 0, "origin": origin, "x": 0, "y": 0},
            {"type": "pointerDown", "button": 0},
            {"type": "pause", "duration": 1200},
            {"type": "pointerUp", "button": 0},
        ],
    }]})
    call("DELETE", browser.path("/actions"))
    time.sleep(0.5)


def hover(selector, dx=0):
    """Moves the mouse over `selector` (`dx` from its middle)."""
    mouse = {"type": "pointer", "id": "mouse", "parameters": {"pointerType": "mouse"}}
    call("POST", browser.path("/actions"), {"actions": [dict(mouse, actions=[
        {"type": "pointerMove", "duration": 0, "origin": {ELEMENT: browser.find(selector)}, "x": dx, "y": 0},
    ])]})
    time.sleep(0.4)


def project_id(name):
    """The ID of the project named `name`, from its sidebar header."""
    return browser.script(
        "const h = [...document.querySelectorAll('#sidebar [data-project-header]')]"
        "  .find((h) => h.querySelector('[data-project-name]').textContent.trim() === arguments[0]);"
        "return h ? h.dataset.projectHeader : null",
        name,
    )


def edit_project(name, emoji, workspace=None, agent=None, agents_md=None):
    """Gives the project `name` an emoji, and optionally default
    attachments (`global` and `local` at `workspace`), a default agent, and
    an AGENTS.md, in its editor."""
    header = '#sidebar [data-project-header="%s"]' % project_id(name)
    hover(header + " [data-section-toggle]", dx=-60)
    click(header + " [data-header-edit]")
    browser.wait("the editor", lambda: browser.count("#project-editor") == 1)
    browser.wait("the AGENTS.md field", lambda: browser.script(
        "const e = document.querySelector('#project-agents-md'); return !!e && !e.disabled"))
    click("#project-emoji")
    field_value("#emoji-typed", emoji)
    click("#emoji-use")
    if workspace:
        browser.wait("the cards", lambda: browser.count('#sheet [data-card="local/coding"]') == 1)
        if browser.count('#sheet [data-card="local/coding"][data-selected=true]') == 0:
            click('#sheet [data-card="local/coding"] [data-card-toggle]')
        field_value('#sheet [data-card="local/coding"] [data-workspace-field="0"]', workspace)
    if agent:
        click("#project-agent")
        click('[data-agent-choice="%s"]' % agent)
    if agents_md:
        field_value("#project-agents-md", agents_md)
    click("#project-save")
    browser.wait("the saved project", lambda: browser.count("#sheet") == 0)


def move_to(title, name):
    """Moves the session titled `title` into the project `name` with Move to
    project…."""
    row = row_of(title)
    hover(row + " [data-session]", dx=-80)
    click(row + " [data-move]")
    click('#sheet [data-move-to="%s"]' % project_id(name))
    time.sleep(0.6)


def projects_showcase(size, width):
    """The project sections (one folded), a header's buttons, a session
    being dragged onto a header, Move to project…, the editor and its emoji
    picker, New session's project picker, and a project's delete."""
    close_all()
    drawer = width < 1024
    scope = "#drawer" if drawer else "#sidebar"
    greeter = project_id("greeter")
    website = project_id("website")
    if drawer:
        click("main [data-sidebar-toggle]")
    both_themes("%s-project-sections" % size)
    click('%s [data-project-header="%s"] [data-section-toggle]' % (scope, website))
    both_themes("%s-project-folded" % size)
    click('%s [data-project-header="%s"] [data-section-toggle]' % (scope, website))
    header = '%s [data-project-header="%s"]' % (scope, greeter)
    if drawer:
        long_press(header + " [data-section-toggle]")
    else:
        hover(header + " [data-section-toggle]", dx=-60)
    both_themes("%s-project-header-actions" % size)
    if not drawer:
        drag_and_shoot(
            row_of("Brainstorm release names") + " [data-session]",
            header + " [data-section-toggle]",
            "%s-project-drag" % size,
        )
    close_all()
    if drawer:
        click("main [data-sidebar-toggle]")
        long_press(row_of("Brainstorm release names", scope) + " [data-session]")
        both_themes("%s-project-row-actions" % size)
        click(row_of("Brainstorm release names", scope) + " ~ [data-delete-actions-frame] [data-move-reveal]")
    else:
        row = row_of("Brainstorm release names")
        hover(row + " [data-session]", dx=-80)
        click(row + " [data-move]")
    both_themes("%s-project-move" % size)
    close_all()
    if drawer:
        click("main [data-sidebar-toggle]")
        long_press(header + " [data-section-toggle]")
    else:
        hover(header + " [data-section-toggle]", dx=-60)
    click(header + " [data-header-edit]")
    browser.wait("the editor", lambda: browser.count("#project-editor") == 1)
    time.sleep(0.8)
    both_themes("%s-project-editor" % size)
    click("#project-emoji")
    field_value("#emoji-search", "")
    both_themes("%s-project-emoji" % size)
    field_value("#emoji-search", "heart")
    both_themes("%s-project-emoji-search" % size)
    close_all()
    if drawer:
        click("main [data-sidebar-toggle]")
    click(header + " [data-header-new]")
    browser.wait("the cards", lambda: browser.count("#new-session-project") == 1)
    time.sleep(0.6)
    both_themes("%s-project-new-session" % size)
    click(row_of("Fix the greeting in greet.py") + " [data-session]")
    close_all()
    if drawer:
        click("main [data-sidebar-toggle]")
        long_press(header + " [data-section-toggle]")
    else:
        hover(header + " [data-section-toggle]", dx=-60)
    click(header + " [data-header-delete]")
    both_themes("%s-project-delete" % size)
    click("%s [data-project-confirm-box] button:not([data-project-delete-confirm])" % scope)
    close_all()


def badges_and_titles(size, width):
    """The sessions list with every kind of badge, a row's title as a field
    (offered by the hover's pencil, or a phone's long press), emptied, and
    the title bar's title as a field, emptied."""
    close_all()
    drawer = width < 1024
    if drawer:
        click("main [data-sidebar-toggle]")
    both_themes("%s-session-badges" % size)
    row = row_of("Deploy the site from the studio", "#drawer" if drawer else "#sidebar")
    if drawer:
        long_press(row + " [data-session]")
        both_themes("%s-row-actions" % size)
        click(row + " ~ [data-delete-actions-frame] [data-rename-reveal]")
    else:
        mouse = {"type": "pointer", "id": "mouse", "parameters": {"pointerType": "mouse"}}
        call("POST", browser.path("/actions"), {"actions": [dict(mouse, actions=[
            {"type": "pointerMove", "duration": 0, "origin": {ELEMENT: browser.find(row)}, "x": 0, "y": 0},
        ])]})
        time.sleep(0.4)
        both_themes("%s-row-hover" % size)
        click(row + " [data-rename]")
    both_themes("%s-rename-row" % size)
    field_value(row + " [data-rename-field]", "")
    both_themes("%s-rename-row-empty" % size)
    browser.script("document.activeElement.blur()")
    time.sleep(0.3)
    close_all()
    click("main [data-title-edit]")
    both_themes("%s-rename-title" % size)
    field_value("#session-title-field", "")
    both_themes("%s-rename-title-empty" % size)
    browser.script("document.activeElement.blur()")
    time.sleep(0.3)


if ALL:
    # Sessions for every kind of badge: two runtimes on this machine, a
    # runtime on another machine beside one on this, and no workspace; the
    # first session has one runtime at one workspace.
    browser.size(1280, 800)
    time.sleep(1)
    badge_session("Note: Review the website copy.", [("local", "~/greeter"), ("alt", "~/website")])
    badge_session("Note: Deploy the site from the studio.", [("local", "~/greeter"), ("studio", "~/website")])
    badge_session("Note: Brainstorm release names.", [])
    # The projects the memory folders became: greeter with an emoji, its
    # defaults, its agent, and an AGENTS.md, and website with an emoji;
    # two sessions moved into them.
    edit_project(
        "greeter", "👋", workspace="~/greeter", agent="primary · fast",
        agents_md="Run python3 test_greet.py before you finish.\n",
    )
    edit_project("website", "🌐")
    move_to("Fix the greeting in greet.py", "greeter")
    move_to("Review the website copy", "website")
    click(row_of("Fix the greeting in greet.py") + " [data-session]")
    browser.wait("the first session", lambda: "Fix the greeting" in browser.text("#session-title"))
    for size, (width, height) in SIZES:
        browser.size(width, height)
        time.sleep(1)
        close_all()
        both_themes("%s-transcript" % size)
        # The system prompt and the tools, pinned at the transcript's top,
        # opened.
        browser.script(
            "for (const b of document.querySelectorAll('#transcript [data-pinned] > button')) b.click();"
            "document.querySelector('#transcript').scrollTop = 0"
        )
        time.sleep(0.6)
        both_themes("%s-pinned-rows" % size)
        browser.script("for (const b of document.querySelectorAll('#transcript [data-pinned] > button')) b.click()")
        time.sleep(0.4)
        if size != "landscape":
            badges_and_titles(size, width)
            projects_showcase(size, width)
        for name, clicks in COMMANDS:
            close_all()
            for selector in clicks:
                browser.script("document.querySelector(arguments[0]).click()", selector)
                time.sleep(0.6)
            time.sleep(0.6)
            both_themes("%s-%s" % (size, name))
        for name in VIEWS:
            close_all()
            view(name)
            both_themes("%s-view-%s" % (size, name))
        if width < 640:
            # A phone's rail shows Chat's sessions first, from another view.
            close_all()
            view("memory")
            view("chat")
            both_themes("%s-view-chat" % size)
        for section in SECTIONS:
            close_all()
            view("settings")
            browser.script("document.querySelector('[data-section=%s]').click()" % section)
            time.sleep(1.2)
            both_themes("%s-settings-%s" % (size, section))
        # The views of 2026-10: an agent open beside the agents (and the
        # agents in a narrow window's drawer), the machines' Add a runtime,
        # Move to…, and a drag, and a project folder open, its delete's
        # confirmation, and an agent's memory.
        close_all()
        view("agents")
        click("[data-config-path=\"agents/reviewer.md\"] button")
        both_themes("%s-agents-open" % size)
        if width < 1024:
            click("#page [data-sidebar-toggle]")
            both_themes("%s-agents-drawer" % size)
        close_all()
        view("machines")
        click("#machines-add")
        both_themes("%s-machines-add" % size)
        click("#add-runtime button[aria-label=Close]")
        click("[data-runtime=labpc] [data-machine-action=\"move to machine\"]")
        both_themes("%s-machines-move" % size)
        if width >= 1024:
            close_all()
            view("machines")
            drag_and_shoot("[data-runtime=local]", "[data-machine-drop=labpc]", "%s-machines-drag" % size)
        close_all()
        view("memory")
        click("[data-project=greeter] [data-project-toggle]")
        both_themes("%s-memory-project-open" % size)
        click("[data-project=greeter] [data-project-delete]")
        both_themes("%s-memory-project-delete" % size)
        close_all()
        view("memory")
        click("[data-memory=\"agent://reviewer\"] button")
        both_themes("%s-memory-agent" % size)
        # An editor: config.toml, then a memory.
        close_all()
        view("settings")
        browser.script("document.querySelector('[data-section=configuration]').click()")
        time.sleep(1)
        browser.script("document.querySelector('[data-config-path=\"config.toml\"] button').click()")
        time.sleep(1)
        both_themes("%s-editor-config" % size)
        browser.script("history.back()")
        time.sleep(0.5)
        for name, selector in CLICKS:
            close_all()
            if name == "drawer" and width >= 1024:
                # A desktop pins the sidebar instead (photographed below).
                continue
            elif browser.count(selector):
                browser.script("document.querySelector(arguments[0]).click()", selector)
            else:
                continue
            time.sleep(0.8)
            both_themes("%s-%s" % (size, name))
        close_all()
        # A new session's card.
        if width >= 1024:
            browser.script("document.querySelector('aside [data-new-session]').click()")
            time.sleep(1)
            both_themes("%s-new-session" % size)
            browser.script("document.querySelector('aside nav[aria-label=Sessions] [data-session]').click()")
            time.sleep(1)
            # The sidebar hidden.
            browser.script("document.querySelector('main [data-sidebar-toggle]').click()")
            time.sleep(0.5)
            both_themes("%s-sidebar-hidden" % size)
            browser.script("document.querySelector('main [data-sidebar-toggle]').click()")
            time.sleep(0.5)
    # The connect screen, once the page holds no token.
    browser.size(1280, 800)
    browser.script("localStorage.clear()")
    call("POST", browser.path("/url"), {"url": LINK.split("#")[0]})
    time.sleep(2)
    both_themes("desktop-connect")
    browser.size(390, 844)
    time.sleep(1)
    both_themes("phone-connect")


def set_composer(text):
    browser.script(
        "const c = document.querySelector('#composer'); c.value = arguments[0];"
        "c.dispatchEvent(new Event('input')); c.blur()",
        text,
    )
    time.sleep(0.4)


def chips_line_up():
    """The agent, model, and mode chips share their height and their top."""
    boxes = browser.script(
        "return ['#agent-chip', '#model-chip', '#permission-mode'].map((s) => {"
        "  const r = document.querySelector(s).getBoundingClientRect();"
        "  return [Math.round(r.top), Math.round(r.height)]; })"
    )
    assert len(set(map(tuple, boxes))) == 1, "the chips do not line up: %s" % boxes
    return boxes[0]


if COMPOSER:
    MODES = ["auto-approve", "auto-approve-n", "auto-approve-y", "ask", "yolo"]
    for size, (width, height) in (("desktop", (1280, 800)), ("phone", (390, 844))):
        browser.size(width, height)
        time.sleep(1)
        # Tool rows: collapsed as they start, then open.
        browser.script(
            "for (const t of document.querySelectorAll('#transcript [data-action=toggle-tool][aria-expanded=true]'))"
            " t.click()"
        )
        time.sleep(0.5)
        show_tools = (
            "const t = document.querySelector('#transcript');"
            "const r = t.querySelector('[data-tool]');"
            "t.scrollTop += r.getBoundingClientRect().top - t.getBoundingClientRect().top - 80"
        )
        browser.script(show_tools)
        for theme in ("light", "dark"):
            browser.theme(theme)
            browser.shot("composer/%s-tools-collapsed-%s.png" % (size, theme))
        browser.script(
            "for (const t of document.querySelectorAll('#transcript [data-action=toggle-tool]')) t.click()"
        )
        time.sleep(0.5)
        browser.script(show_tools)
        for theme in ("light", "dark"):
            browser.theme(theme)
            browser.shot("composer/%s-tools-expanded-%s.png" % (size, theme))
        # The box: one line and Send disabled, Send with text, five lines.
        for name, text in (
            ("idle-empty", ""),
            ("idle-text", "Add a test for an empty name"),
            ("five-lines", "\n".join("line %d of the message" % n for n in range(1, 9))),
        ):
            set_composer(text)
            for theme in ("light", "dark"):
                browser.theme(theme)
                browser.shot("composer/%s-%s-%s.png" % (size, name, theme))
        set_composer("")
        # The bottom bar in each permission mode; its chips line up.
        for mode in MODES:
            for theme in ("light", "dark"):
                browser.theme(theme)
                top, height = chips_line_up()
                browser.shot("composer/%s-bar-%s-%s.png" % (size, mode, theme), "#status-bar")
            print("%s %s: chips at %s px, %s px tall" % (size, mode, top, height))
            following = MODES[(MODES.index(mode) + 1) % len(MODES)]
            browser.script("document.querySelector('#permission-mode').click()")
            time.sleep(0.6)
            browser.script("document.querySelector('#sheet [data-mode=\"%s\"]').click()" % following)
            time.sleep(0.6)
        # A run going: Stop with an empty box, Steer and Queue with text.
        browser.type("#composer", "Explain the change; take your time")
        browser.type("#composer", ENTER)
        browser.wait("the run", lambda: browser.count("#stop") == 1)
        browser.script("document.querySelector('#composer').blur()")
        for theme in ("light", "dark"):
            browser.theme(theme)
            browser.shot("composer/%s-running-empty-%s.png" % (size, theme))
        set_composer("Also check the docstring")
        browser.wait("steer and queue", lambda: browser.count("#steer") == 1)
        for theme in ("light", "dark"):
            browser.theme(theme)
            browser.shot("composer/%s-running-text-%s.png" % (size, theme))
        set_composer("")
        browser.script("document.querySelector('#stop').click()")
        browser.wait("the stopped run", lambda: browser.count("#send") == 1)
        # A long message of several lines: its bubble ends at the right edge
        # of the transcript's column, as the short one does.
        set_composer(
            "\n".join(["Please take your time and walk me through the change to greet.py."] * 3)
        )
        browser.script("document.querySelector('#send').click()")
        browser.wait("the run", lambda: browser.count("#stop") == 1)
        browser.script("document.querySelector('#stop').click()")
        browser.wait("the stopped run", lambda: browser.count("#send") == 1)
        browser.script(
            "const t = document.querySelector('#transcript');"
            "const m = [...t.querySelectorAll('.user-message')].pop();"
            "t.scrollTop += m.getBoundingClientRect().top - t.getBoundingClientRect().top - 8"
        )
        for theme in ("light", "dark"):
            browser.theme(theme)
            browser.shot("composer/%s-long-message-%s.png" % (size, theme))
call("DELETE", browser.path(""))
