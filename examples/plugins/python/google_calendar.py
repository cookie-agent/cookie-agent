"""A Google Calendar plugin for cookie-agent, in plain Python with no
dependencies: its own web pages for its settings and the Google sign-in,
an agenda page, and two tools.

- **Pages** (docs/site/development/plugins.md, "Pages"): the daemon serves
  them at `/plugins/<name>/` in the web client. The settings page takes an
  OAuth client ID and secret, shows the redirect URI to register with
  Google, and connects the account. `/oauth/callback` is a public path, so
  Google's redirect back works in a browser without the page's cookie; the
  OAuth `state` and PKCE protect it.
- **Tools:** `calendar_list_events` and `calendar_create_event`, under the
  permission action `calendar`.

Create the client in Google Cloud: APIs & Services, Credentials, an OAuth
client ID of type "Web application", with the redirect URI the settings page
shows as an authorized redirect URI, and the Google Calendar API enabled.
Google takes `https` redirects, or plain `http` at `localhost`, so reach the
web client over HTTPS (such as `tailscale serve`) or at `localhost`.

Configure it as a plugin named `google-calendar`:

    [plugins.google-calendar]
    command = ["python3", "examples/plugins/python/google_calendar.py"]

It keeps its client and tokens in `GOOGLE_CALENDAR_STATE` (by default
`~/.cookie-agent/plugins/google-calendar.json`), readable by you only.
"""

import base64
import datetime
import hashlib
import html
import http.server
import json
import os
import secrets
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import cookie_plugin  # noqa: E402

NAME = "google-calendar"
AUTHORIZE = "https://accounts.google.com/o/oauth2/v2/auth"
TOKEN = "https://oauth2.googleapis.com/token"
EVENTS = "https://www.googleapis.com/calendar/v3/calendars/primary/events"
SCOPE = "https://www.googleapis.com/auth/calendar.events"
STATE_PATH = os.environ.get("GOOGLE_CALENDAR_STATE") or os.path.join(
    os.path.expanduser("~"), ".cookie-agent", "plugins", "google-calendar.json"
)

plugin = cookie_plugin.Plugin(NAME, "1.0.0")


# --- the stored state ---------------------------------------------------------

class Store:
    """The client, the tokens, and the sign-ins in progress. Pages run on
    their own threads and tools on the main one, so every use takes the
    lock."""

    def __init__(self, path):
        self.path = path
        self.lock = threading.Lock()
        self.pending = {}  # state -> {"verifier", "redirect_uri", "at"}
        try:
            with open(path, encoding="utf-8") as stored:
                self.data = json.load(stored)
        except (OSError, ValueError):
            self.data = {}

    def save(self):
        """Writes the state, readable by the user only, atomically."""
        os.makedirs(os.path.dirname(self.path), mode=0o700, exist_ok=True)
        temporary = self.path + ".tmp"
        descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(descriptor, "w", encoding="utf-8") as output:
            json.dump(self.data, output)
        os.replace(temporary, self.path)

    def get(self, key):
        with self.lock:
            return self.data.get(key)

    def update(self, **values):
        with self.lock:
            for key, value in values.items():
                if value is None:
                    self.data.pop(key, None)
                else:
                    self.data[key] = value
            self.save()


store = Store(STATE_PATH)


def post_form(url, fields):
    """POSTs `fields` form-encoded and returns the JSON answer, or raises
    `RuntimeError` with Google's own error."""
    body = urllib.parse.urlencode(fields).encode()
    request = urllib.request.Request(url, data=body, method="POST")
    request.add_header("content-type", "application/x-www-form-urlencoded")
    return call_google(request)


def call_google(request):
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            return json.load(response)
    except urllib.error.HTTPError as error:
        try:
            detail = json.load(error)
            message = detail.get("error_description") or detail.get("error") or detail
            if isinstance(message, dict):
                message = message.get("message", str(message))
        except ValueError:
            message = error.reason
        raise RuntimeError("Google answered %d: %s" % (error.code, message)) from None
    except urllib.error.URLError as error:
        raise RuntimeError("Google could not be reached: %s" % error.reason) from None


def access_token():
    """A current access token, refreshed when it is about to expire."""
    with store.lock:
        token = store.data.get("access_token")
        expires = store.data.get("expires_at", 0)
        refresh = store.data.get("refresh_token")
        client_id = store.data.get("client_id")
        client_secret = store.data.get("client_secret")
    if token and expires > time.time() + 60:
        return token
    if not refresh:
        raise cookie_plugin.ToolError(not_connected())
    answer = post_form(TOKEN, {
        "client_id": client_id,
        "client_secret": client_secret,
        "refresh_token": refresh,
        "grant_type": "refresh_token",
    })
    store.update(access_token=answer["access_token"],
                 expires_at=time.time() + int(answer.get("expires_in", 3600)))
    return answer["access_token"]


def not_connected():
    where = (plugin.daemon or {}).get("ui_base_url")
    if where:
        return "Google Calendar is not connected: open %s to connect it." % where
    return ("Google Calendar is not connected: open the google-calendar plugin's page "
            "from Settings, Plugins in the cookie web client to connect it.")


def google_json(method, url, body=None):
    request = urllib.request.Request(
        url, data=None if body is None else json.dumps(body).encode(), method=method)
    request.add_header("authorization", "Bearer " + access_token())
    if body is not None:
        request.add_header("content-type", "application/json")
    return call_google(request)


# --- the tools ------------------------------------------------------------------

LIST_EVENTS = {
    "name": "calendar_list_events",
    "description": "List the events of the user's primary Google Calendar from now on.",
    "permission": "calendar",
    "parameters": {
        "type": "object",
        "properties": {
            "days": {"type": "integer", "minimum": 1, "maximum": 90,
                     "description": "How many days ahead to look; 7 when left out."},
            "query": {"type": "string", "description": "Only events matching this text."},
        },
        "additionalProperties": False,
    },
}

CREATE_EVENT = {
    "name": "calendar_create_event",
    "description": "Create an event in the user's primary Google Calendar.",
    "permission": "calendar",
    "parameters": {
        "type": "object",
        "properties": {
            "summary": {"type": "string", "description": "The event's title."},
            "start": {"type": "string",
                      "description": "When it starts: an RFC 3339 date-time with its offset, "
                                     "or a date (YYYY-MM-DD) for an all-day event."},
            "end": {"type": "string", "description": "When it ends, in the same form as start."},
            "description": {"type": "string"},
            "location": {"type": "string"},
        },
        "required": ["summary", "start", "end"],
        "additionalProperties": False,
    },
}


def when(value):
    """A Calendar API time: `date` for a plain date, else `dateTime`."""
    return {"date": value} if len(value) == 10 else {"dateTime": value}


def upcoming(days, query=None):
    now = datetime.datetime.now(datetime.timezone.utc)
    fields = {
        "timeMin": now.isoformat(),
        "timeMax": (now + datetime.timedelta(days=days)).isoformat(),
        "singleEvents": "true",
        "orderBy": "startTime",
        "maxResults": "100",
    }
    if query:
        fields["q"] = query
    return google_json("GET", EVENTS + "?" + urllib.parse.urlencode(fields)).get("items", [])


def describe(event):
    start = event.get("start", {})
    return "%s  %s%s" % (
        start.get("dateTime") or start.get("date", "?"),
        event.get("summary", "(no title)"),
        "  @ " + event["location"] if event.get("location") else "",
    )


def list_events(call, arguments):
    try:
        events = upcoming(int(arguments.get("days") or 7), arguments.get("query"))
    except RuntimeError as error:
        return cookie_plugin.tool_result(str(error), failed=True)
    if not events:
        return cookie_plugin.tool_result("No events.", title="No events")
    return cookie_plugin.tool_result(
        "\n".join(describe(event) for event in events),
        title="%d events" % len(events),
    )


def create_event(call, arguments):
    body = {
        "summary": arguments["summary"],
        "start": when(arguments["start"]),
        "end": when(arguments["end"]),
    }
    for key in ("description", "location"):
        if arguments.get(key):
            body[key] = arguments[key]
    try:
        event = google_json("POST", EVENTS, body)
    except RuntimeError as error:
        return cookie_plugin.tool_result(str(error), failed=True)
    return cookie_plugin.tool_result(
        "Created %s: %s" % (describe(event), event.get("htmlLink", "")),
        title="Created %s" % event.get("summary", "an event"),
    )


def create_presentation(name, arguments, resource):
    return {
        "title": "Create a calendar event",
        "primary_argument": arguments.get("summary", ""),
        "preview": [{"type": "text", "text": "%s → %s" % (
            arguments.get("start", "?"), arguments.get("end", "?"))}],
    }


plugin.tool(LIST_EVENTS, list_events, resource=lambda arguments: "primary")
plugin.tool(CREATE_EVENT, create_event, resource=lambda arguments: "primary",
            presentation=create_presentation)


# --- the pages --------------------------------------------------------------------

STYLE = """
body { font: 15px/1.5 system-ui, sans-serif; max-width: 42rem; margin: 2rem auto;
       padding: 0 1rem; color: #1f2328; }
code { background: #f2f2f2; padding: .1rem .3rem; border-radius: 4px; word-break: break-all; }
input { width: 100%; padding: .4rem; margin: .2rem 0 .8rem; box-sizing: border-box; }
button, .button { padding: .4rem .8rem; border-radius: 6px; border: 1px solid #888;
                  background: #fff; cursor: pointer; text-decoration: none; color: inherit; }
.note { color: #666; font-size: 13px; }
.error { color: #b00020; }
li { margin: .3rem 0; }
@media (prefers-color-scheme: dark) {
  body { background: #16181c; color: #e6e6e6; }
  code { background: #2a2d33; }
  button, .button { background: #22252b; color: #e6e6e6; }
}
"""


def page(title, body):
    return ("<!doctype html><meta charset=utf-8>"
            "<meta name=viewport content='width=device-width, initial-scale=1'>"
            "<title>%s</title><style>%s</style><h1>%s</h1>%s"
            % (html.escape(title), STYLE, html.escape(title), body))


class Pages(http.server.BaseHTTPRequestHandler):
    """The plugin's pages. Paths are below its root: the daemon strips
    `/plugins/<name>`. Links are relative, so they work wherever the page
    is served."""

    def log_message(self, format, *args):
        pass  # Standard error is the plugin's status in the daemon.

    def send_page(self, title, body, status=200):
        data = page(title, body).encode()
        self.send_response(status)
        self.send_header("content-type", "text/html; charset=utf-8")
        self.send_header("cache-control", "no-store")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def redirect(self, location):
        self.send_response(303)
        self.send_header("location", location)
        self.send_header("content-length", "0")
        self.end_headers()

    def form(self):
        length = int(self.headers.get("content-length") or 0)
        fields = urllib.parse.parse_qs(self.rfile.read(length).decode(), keep_blank_values=True)
        return {key: values[-1] for key, values in fields.items()}

    def do_GET(self):
        url = urllib.parse.urlsplit(self.path)
        query = dict(urllib.parse.parse_qsl(url.query))
        if url.path == "/":
            self.settings(query.get("note"))
        elif url.path == "/connect":
            self.connect()
        elif url.path == "/oauth/callback":
            self.callback(query)
        elif url.path == "/agenda":
            self.agenda()
        else:
            self.send_page("Not found", "<p><a href='./'>Back</a></p>", status=404)

    def do_POST(self):
        url = urllib.parse.urlsplit(self.path)
        if url.path == "/settings":
            fields = self.form()
            changes = {"client_id": fields.get("client_id", "").strip() or None}
            if fields.get("client_secret", "").strip():
                changes["client_secret"] = fields["client_secret"].strip()
            store.update(**changes)
            self.redirect("./?note=saved")
        elif url.path == "/disconnect":
            store.update(refresh_token=None, access_token=None, expires_at=None)
            self.redirect("./?note=disconnected")
        else:
            self.send_page("Not found", "<p><a href='./'>Back</a></p>", status=404)

    def settings(self, note):
        redirect_uri = cookie_plugin.external_url(self, "oauth/callback")
        client_id = store.get("client_id") or ""
        has_secret = bool(store.get("client_secret"))
        connected = bool(store.get("refresh_token"))
        notes = {
            "saved": "Saved.",
            "disconnected": "Disconnected.",
            "connected": "Connected to Google Calendar.",
        }
        parts = []
        if note in notes:
            parts.append("<p><b>%s</b></p>" % notes[note])
        parts.append(
            "<h2>OAuth client</h2>"
            "<p class=note>Create an OAuth client ID of type <i>Web application</i> in Google "
            "Cloud, with this authorized redirect URI:</p><p><code>%s</code></p>"
            "<form method=post action=settings>"
            "<label>Client ID<input name=client_id value='%s' autocomplete=off></label>"
            "<label>Client secret<input name=client_secret type=password autocomplete=off "
            "placeholder='%s'></label><button>Save</button></form>"
            % (html.escape(redirect_uri), html.escape(client_id, quote=True),
               "set; type a new one to replace it" if has_secret else "")
        )
        parts.append("<h2>Account</h2>")
        if connected:
            parts.append("<p>Connected. <a class=button href=agenda>Agenda</a></p>"
                         "<form method=post action=disconnect><button>Disconnect</button></form>")
        elif client_id and has_secret:
            # Google's sign-in refuses to show in a frame, so it gets a tab
            # of its own and comes back to this page there.
            parts.append("<p><a class=button href=connect target=_blank>"
                         "Connect Google Calendar</a></p><p class=note>Google's sign-in opens "
                         "in a new tab. Reload this page once it says you are connected.</p>")
        else:
            parts.append("<p class=note>Save a client ID and secret first.</p>")
        self.send_page("Google Calendar", "".join(parts))

    def connect(self):
        client_id = store.get("client_id")
        if not client_id or not store.get("client_secret"):
            self.redirect("./")
            return
        state = secrets.token_urlsafe(24)
        verifier = secrets.token_urlsafe(48)
        challenge = base64.urlsafe_b64encode(
            hashlib.sha256(verifier.encode()).digest()).rstrip(b"=").decode()
        redirect_uri = cookie_plugin.external_url(self, "oauth/callback")
        with store.lock:
            now = time.time()
            store.pending = {key: value for key, value in store.pending.items()
                             if now - value["at"] < 600}
            store.pending[state] = {"verifier": verifier, "redirect_uri": redirect_uri, "at": now}
        self.redirect(AUTHORIZE + "?" + urllib.parse.urlencode({
            "client_id": client_id,
            "redirect_uri": redirect_uri,
            "response_type": "code",
            "scope": SCOPE,
            "access_type": "offline",
            "prompt": "consent",
            "state": state,
            "code_challenge": challenge,
            "code_challenge_method": "S256",
        }))

    def callback(self, query):
        with store.lock:
            pending = store.pending.pop(query.get("state", ""), None)
        if pending is None or time.time() - pending["at"] > 600:
            self.send_page("Sign-in not recognized",
                           "<p class=error>This sign-in was not started here, or it expired. "
                           "Open the plugin's page and connect again.</p>", status=400)
            return
        if "error" in query:
            self.send_page("Not connected", "<p class=error>Google said: %s</p>"
                           % html.escape(query["error"]), status=400)
            return
        try:
            answer = post_form(TOKEN, {
                "client_id": store.get("client_id"),
                "client_secret": store.get("client_secret"),
                "code": query.get("code", ""),
                "code_verifier": pending["verifier"],
                "redirect_uri": pending["redirect_uri"],
                "grant_type": "authorization_code",
            })
        except RuntimeError as error:
            self.send_page("Not connected", "<p class=error>%s</p>" % html.escape(str(error)),
                           status=502)
            return
        store.update(refresh_token=answer.get("refresh_token") or store.get("refresh_token"),
                     access_token=answer["access_token"],
                     expires_at=time.time() + int(answer.get("expires_in", 3600)))
        # Back to the settings page, the plugin's root: `../` from
        # `/oauth/callback`.
        self.redirect("../?note=connected")

    def agenda(self):
        try:
            events = upcoming(7)
        except (RuntimeError, cookie_plugin.ToolError) as error:
            self.send_page("Agenda", "<p class=error>%s</p><p><a href='./'>Settings</a></p>"
                           % html.escape(str(error)))
            return
        # Event text comes from elsewhere: everything is escaped.
        items = "".join("<li>%s</li>" % html.escape(describe(event)) for event in events)
        self.send_page("The next 7 days", "<ul>%s</ul><p><a href='./'>Settings</a></p>"
                       % (items or "<li>No events.</li>"))


plugin.pages(Pages, public_paths=["/oauth/callback"])

if __name__ == "__main__":
    plugin.run()
