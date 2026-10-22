//! Signing in to MCP servers from the page ([spec]): the OAuth redirect is
//! the page itself with `?oauth=callback`, the sign-ins this browser started
//! are kept by their `state`, and a tab the authorization server sends back
//! hands its address to the tab that started the sign-in over a
//! `BroadcastChannel`, or finishes the sign-in itself.
//!
//! The rules are plain functions, tested natively; the browser calls at the
//! end of the module only work in a browser.
//!
//! [spec]: https://cookie-agent.github.io/doc/latest/specs/web-mcp-oauth/

use std::{cell::RefCell, collections::BTreeMap, collections::HashSet, time::Duration};

use serde_json::{Value, json};
use url::Url;
use wasm_bindgen::{JsCast as _, closure::Closure};

use crate::browser;

/// The storage key of the sign-ins this browser started, in both
/// `sessionStorage` (the tab that started it) and `localStorage` (any tab of
/// the page, such as the one the authorization server opens).
pub const PENDING_KEY: &str = "cookie.oauth_pending";

/// How long a started sign-in waits for its callback.
pub const PENDING_LIFETIME_MS: f64 = 10.0 * 60.0 * 1000.0;

/// The `BroadcastChannel` the tabs of the page hand callbacks over on.
pub const CHANNEL: &str = "cookie-oauth";

/// How long a callback tab waits for the tab that started the sign-in to
/// take it before it finishes the sign-in itself.
pub const HANDOFF_WAIT: Duration = Duration::from_millis(1500);

/// The query parameter that makes the page's address a redirect.
const CALLBACK: (&str, &str) = ("oauth", "callback");

/// What the authorization server adds to the redirect, which the address
/// bar loses at once.
const RESPONSE_PARAMETERS: [&str; 7] = [
    "oauth",
    "code",
    "state",
    "iss",
    "error",
    "error_description",
    "error_uri",
];

/// The redirect a page at `page` (its `location.href`) asks for: its own
/// address without query or fragment, with `?oauth=callback`. `None` for a
/// page not served over HTTP.
pub fn redirect_for_page(page: &str) -> Option<String> {
    let mut url = Url::parse(page).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    url.set_fragment(None);
    url.set_query(Some(&format!("{}={}", CALLBACK.0, CALLBACK.1)));
    Some(url.into())
}

/// A page address the authorization server sent the browser back to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Callback {
    /// The whole address, for `mcp.auth.complete`.
    pub url: String,
    /// Its `state`, which names the sign-in it answers.
    pub state: Option<String>,
}

/// The callback `page` is, when it carries `oauth=callback`.
pub fn callback_of(page: &str) -> Option<Callback> {
    let url = Url::parse(page).ok()?;
    let mut pairs = url.query_pairs();
    if !pairs.any(|(name, value)| name == CALLBACK.0 && value == CALLBACK.1) {
        return None;
    }
    let state = url
        .query_pairs()
        .find(|(name, _)| name == "state")
        .map(|(_, value)| value.into_owned());
    Some(Callback {
        url: page.to_owned(),
        state,
    })
}

/// The address to leave in the address bar of callback `page`: its path,
/// its other query parameters, and its fragment, without the code, the
/// state, or anything else the authorization server added.
pub fn address_without_callback(page: &str) -> Option<String> {
    let url = Url::parse(page).ok()?;
    let rest = url
        .query_pairs()
        .filter(|(name, _)| !RESPONSE_PARAMETERS.contains(&name.as_ref()))
        .collect::<Vec<_>>();
    let mut address = url.path().to_owned();
    if !rest.is_empty() {
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        for (name, value) in &rest {
            query.append_pair(name, value);
        }
        address.push('?');
        address.push_str(&query.finish());
    }
    if let Some(fragment) = url.fragment() {
        address.push('#');
        address.push_str(fragment);
    }
    Some(address)
}

/// The `state` of an authorization URL, which the callback carries back.
pub fn state_of(authorization_url: &str) -> Option<String> {
    Url::parse(authorization_url)
        .ok()?
        .query_pairs()
        .find(|(name, _)| name == "state")
        .map(|(_, value)| value.into_owned())
        .filter(|state| !state.is_empty())
}

/// Whether a page at `page` warns that most authorization servers refuse
/// its redirect: plain `http` at an address that is not loopback.
pub fn warns_about_http(page: &str) -> bool {
    let Ok(url) = Url::parse(page) else {
        return false;
    };
    url.scheme() == "http"
        && !match url.host() {
            Some(url::Host::Ipv4(address)) => address.is_loopback(),
            Some(url::Host::Ipv6(address)) => address.is_loopback(),
            Some(url::Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
            None => false,
        }
}

/// A sign-in this browser started.
#[derive(Clone, Debug, PartialEq)]
pub struct PendingSignIn {
    pub server: String,
    /// When it stops waiting, in milliseconds since the epoch.
    pub expires_at: f64,
}

/// What a callback's `state` names.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Lookup {
    /// A sign-in to this server waits for it.
    Found(String),
    /// The sign-in to this server waited too long.
    Expired(String),
    /// No sign-in this browser started, or one that already ended.
    Unknown,
}

/// The sign-ins this browser started, by `state`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PendingSignIns(BTreeMap<String, PendingSignIn>);

impl PendingSignIns {
    /// The sign-ins `stored` holds; anything malformed is dropped.
    pub fn parse(stored: Option<&str>) -> Self {
        let Some(Value::Object(entries)) = stored.and_then(|text| serde_json::from_str(text).ok())
        else {
            return Self::default();
        };
        Self(
            entries
                .into_iter()
                .filter_map(|(state, entry)| {
                    let server = entry.get("server")?.as_str()?.to_owned();
                    let expires_at = entry.get("expires_at")?.as_f64()?;
                    Some((state, PendingSignIn { server, expires_at }))
                })
                .collect(),
        )
    }

    pub fn to_json(&self) -> String {
        Value::Object(
            self.0
                .iter()
                .map(|(state, entry)| {
                    (
                        state.clone(),
                        json!({ "server": entry.server, "expires_at": entry.expires_at }),
                    )
                })
                .collect(),
        )
        .to_string()
    }

    /// Records a sign-in to `server` started at `now`. The daemon runs one
    /// flow per server, so it replaces any earlier one to that server.
    pub fn add(&mut self, state: &str, server: &str, now: f64) {
        self.0.retain(|_, entry| entry.server != server);
        self.0.insert(
            state.to_owned(),
            PendingSignIn {
                server: server.to_owned(),
                expires_at: now + PENDING_LIFETIME_MS,
            },
        );
    }

    pub fn remove(&mut self, state: &str) {
        self.0.remove(state);
    }

    /// Adds the entries of `other` this one lacks.
    pub fn merge(&mut self, other: Self) {
        for (state, entry) in other.0 {
            self.0.entry(state).or_insert(entry);
        }
    }

    pub fn lookup(&self, state: &str, now: f64) -> Lookup {
        match self.0.get(state) {
            Some(entry) if entry.expires_at > now => Lookup::Found(entry.server.clone()),
            Some(entry) => Lookup::Expired(entry.server.clone()),
            None => Lookup::Unknown,
        }
    }

    /// The sign-ins still waiting at `now`, as `(state, server)` in server
    /// order.
    pub fn waiting(&self, now: f64) -> Vec<(String, String)> {
        let mut waiting = self
            .0
            .iter()
            .filter(|(_, entry)| entry.expires_at > now)
            .map(|(state, entry)| (state.clone(), entry.server.clone()))
            .collect::<Vec<_>>();
        waiting.sort_by(|left, right| left.1.cmp(&right.1));
        waiting
    }
}

/// What the tabs of the page say to each other on [`CHANNEL`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ChannelMessage {
    /// A callback tab's address, for the tab that started its sign-in.
    Callback { state: String, url: String },
    /// The tab that started the sign-in took the callback.
    Taken { state: String },
}

impl ChannelMessage {
    pub fn to_json(&self) -> String {
        match self {
            Self::Callback { state, url } => {
                json!({ "kind": "callback", "state": state, "url": url })
            }
            Self::Taken { state } => json!({ "kind": "taken", "state": state }),
        }
        .to_string()
    }

    pub fn parse(text: &str) -> Option<Self> {
        let value: Value = serde_json::from_str(text).ok()?;
        let state = value.get("state")?.as_str()?.to_owned();
        match value.get("kind")?.as_str()? {
            "callback" => Some(Self::Callback {
                state,
                url: value.get("url")?.as_str()?.to_owned(),
            }),
            "taken" => Some(Self::Taken { state }),
            _ => None,
        }
    }
}

// ---- the browser -----------------------------------------------------------

fn now() -> f64 {
    js_sys::Date::now()
}

fn tab_pending() -> PendingSignIns {
    PendingSignIns::parse(browser::load_tab(PENDING_KEY).as_deref())
}

fn browser_pending() -> PendingSignIns {
    PendingSignIns::parse(browser::load(PENDING_KEY).as_deref())
}

/// The sign-ins this tab or any tab of this browser started.
pub fn pending() -> PendingSignIns {
    let mut pending = tab_pending();
    pending.merge(browser_pending());
    pending
}

/// The sign-ins still waiting, as `(state, server)`.
pub fn waiting() -> Vec<(String, String)> {
    pending().waiting(now())
}

/// Records a sign-in this tab started, in both storages.
pub fn record(state: &str, server: &str) {
    let at = now();
    let mut tab = tab_pending();
    tab.add(state, server, at);
    browser::store_tab(PENDING_KEY, &tab.to_json());
    let mut all = browser_pending();
    all.add(state, server, at);
    browser::store(PENDING_KEY, &all.to_json());
}

/// Forgets a sign-in in both storages.
pub fn forget(state: &str) {
    let mut tab = tab_pending();
    tab.remove(state);
    browser::store_tab(PENDING_KEY, &tab.to_json());
    let mut all = browser_pending();
    all.remove(state);
    browser::store(PENDING_KEY, &all.to_json());
}

/// What `state` names among the sign-ins of this browser.
pub fn lookup(state: &str) -> Lookup {
    pending().lookup(state, now())
}

/// The server of the sign-in `state` when this tab started it and it
/// still waits.
pub fn started_here(state: &str) -> Option<String> {
    match tab_pending().lookup(state, now()) {
        Lookup::Found(server) => Some(server),
        _ => None,
    }
}

/// The callback the page was opened at, if it was: its address is taken
/// out of the address bar and history at once, so the code never lands in
/// a bookmark or the history.
pub fn take_callback() -> Option<Callback> {
    let page = browser::page_href()?;
    let callback = callback_of(&page)?;
    if let Some(address) = address_without_callback(&page) {
        browser::replace_address(&address);
    }
    Some(callback)
}

/// A new, blank tab, opened while the click that asked for it still
/// counts, so the browser does not block it; `None` when it did. The
/// browser gives it a copy of this tab's `sessionStorage`, which is
/// emptied so the page it comes back to is a client of its own.
pub fn open_blank_tab() -> Option<web_sys::Window> {
    let tab = web_sys::window()?
        .open_with_url_and_target("about:blank", "_blank")
        .ok()
        .flatten()?;
    if let Ok(Some(storage)) = tab.session_storage() {
        let _ = storage.clear();
    }
    Some(tab)
}

/// Sends `tab` to `url`, or, without a tab, this tab.
pub fn go_to(tab: Option<web_sys::Window>, url: &str) {
    if let Some(tab) = tab {
        // The authorization server's page gets no handle on this one.
        let _ = tab.set_opener(&wasm_bindgen::JsValue::NULL);
        if tab.location().set_href(url).is_ok() {
            return;
        }
        let _ = tab.close();
    }
    if let Some(window) = web_sys::window() {
        let _ = window.location().assign(url);
    }
}

/// Closes this tab; a browser closes only a tab a script opened.
pub fn close_this_tab() {
    if let Some(window) = web_sys::window() {
        let _ = window.close();
    }
}

thread_local! {
    /// The page's one channel: its own messages never come back to it.
    static OPEN: RefCell<Option<web_sys::BroadcastChannel>> = const { RefCell::new(None) };
    /// The callbacks another tab took.
    static TAKEN: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
}

/// Opens the page's channel and calls `heard` with every callback another
/// tab sends, for the life of the page. Without `BroadcastChannel` nothing
/// is heard, and a callback tab finishes its sign-in itself.
pub fn listen(heard: impl Fn(String, String) + 'static) {
    let Ok(channel) = web_sys::BroadcastChannel::new(CHANNEL) else {
        return;
    };
    let callback =
        Closure::<dyn Fn(web_sys::MessageEvent)>::new(move |event: web_sys::MessageEvent| {
            let Some(message) = event
                .data()
                .as_string()
                .and_then(|text| ChannelMessage::parse(&text))
            else {
                return;
            };
            match message {
                ChannelMessage::Callback { state, url } => heard(state, url),
                ChannelMessage::Taken { state } => {
                    TAKEN.with_borrow_mut(|taken| taken.insert(state));
                }
            }
        });
    channel.set_onmessage(Some(callback.as_ref().unchecked_ref()));
    callback.forget();
    OPEN.with_borrow_mut(|open| *open = Some(channel));
}

/// Says `message` to the other tabs of the page.
pub fn post(message: &ChannelMessage) {
    OPEN.with_borrow(|open| {
        if let Some(channel) = open {
            let _ = channel.post_message(&wasm_bindgen::JsValue::from_str(&message.to_json()));
        }
    });
}

/// Offers callback `url` of sign-in `state` to the tab that started it,
/// and whether that tab took it within [`HANDOFF_WAIT`].
pub async fn hand_off(state: &str, url: &str) -> bool {
    if OPEN.with_borrow(Option::is_none) {
        return false;
    }
    post(&ChannelMessage::Callback {
        state: state.to_owned(),
        url: url.to_owned(),
    });
    gloo_timers::future::sleep(HANDOFF_WAIT).await;
    TAKEN.with_borrow_mut(|taken| taken.remove(state))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_redirect_is_the_page_with_the_callback_parameter() {
        for (page, redirect) in [
            ("https://h.ts.net/", "https://h.ts.net/?oauth=callback"),
            (
                "https://h.ts.net/cookie/index.html?x=1#token=t",
                "https://h.ts.net/cookie/index.html?oauth=callback",
            ),
            (
                "http://127.0.0.1:7419/",
                "http://127.0.0.1:7419/?oauth=callback",
            ),
        ] {
            assert_eq!(redirect_for_page(page).as_deref(), Some(redirect), "{page}");
        }
        assert_eq!(redirect_for_page("file:///index.html"), None);
        assert_eq!(redirect_for_page("not a url"), None);
    }

    #[test]
    fn a_callback_is_recognised_and_leaves_the_address_bar_clean() {
        let page =
            "https://h.ts.net/app/?oauth=callback&code=secret&state=s1&iss=https%3A%2F%2Fas#view=x";
        assert_eq!(
            callback_of(page),
            Some(Callback {
                url: page.to_owned(),
                state: Some("s1".into())
            })
        );
        assert_eq!(
            address_without_callback(page).as_deref(),
            Some("/app/#view=x")
        );
        let denied =
            "http://127.0.0.1:7419/?keep=1&oauth=callback&error=access_denied&error_description=no";
        assert_eq!(
            callback_of(denied).and_then(|callback| callback.state),
            None
        );
        assert_eq!(
            address_without_callback(denied).as_deref(),
            Some("/?keep=1")
        );
        assert_eq!(callback_of("https://h.ts.net/?code=c&state=s"), None);
        assert_eq!(callback_of("https://h.ts.net/?oauth=other&code=c"), None);
    }

    #[test]
    fn the_state_comes_from_the_authorization_url() {
        assert_eq!(
            state_of("https://as.example/authorize?client_id=c&state=abc&redirect_uri=x")
                .as_deref(),
            Some("abc")
        );
        assert_eq!(state_of("https://as.example/authorize?state="), None);
        assert_eq!(state_of("https://as.example/authorize"), None);
        assert_eq!(state_of("not a url"), None);
    }

    #[test]
    fn only_plain_http_away_from_loopback_warns() {
        assert!(warns_about_http("http://192.168.1.5:7419/"));
        assert!(warns_about_http("http://labpc:7419/"));
        assert!(!warns_about_http("http://127.0.0.1:7419/"));
        assert!(!warns_about_http("http://[::1]:7419/"));
        assert!(!warns_about_http("http://localhost:7419/"));
        assert!(!warns_about_http("https://h.ts.net/"));
        assert!(!warns_about_http("not a url"));
    }

    #[test]
    fn pending_sign_ins_expire_and_one_server_has_one() {
        let mut pending = PendingSignIns::default();
        pending.add("s1", "github", 1_000.0);
        pending.add("s2", "linear", 2_000.0);
        assert_eq!(
            pending.lookup("s1", 1_000.0 + PENDING_LIFETIME_MS - 1.0),
            Lookup::Found("github".into())
        );
        assert_eq!(
            pending.lookup("s1", 1_000.0 + PENDING_LIFETIME_MS),
            Lookup::Expired("github".into())
        );
        assert_eq!(pending.lookup("other", 1_000.0), Lookup::Unknown);
        // A new sign-in to a server replaces its earlier one.
        pending.add("s3", "github", 3_000.0);
        assert_eq!(pending.lookup("s1", 3_000.0), Lookup::Unknown);
        assert_eq!(
            pending.waiting(3_000.0),
            vec![
                ("s3".to_owned(), "github".to_owned()),
                ("s2".to_owned(), "linear".to_owned())
            ]
        );
        assert_eq!(
            pending.waiting(2_000.0 + PENDING_LIFETIME_MS),
            vec![("s3".to_owned(), "github".to_owned())]
        );
        pending.remove("s3");
        assert_eq!(pending.lookup("s3", 3_000.0), Lookup::Unknown);
    }

    #[test]
    fn pending_sign_ins_round_trip_and_merge() {
        let mut pending = PendingSignIns::default();
        pending.add("s1", "github", 5.0);
        let stored = pending.to_json();
        assert_eq!(PendingSignIns::parse(Some(&stored)), pending);
        assert_eq!(PendingSignIns::parse(None), PendingSignIns::default());
        assert_eq!(
            PendingSignIns::parse(Some("[1]")),
            PendingSignIns::default()
        );
        let partly = PendingSignIns::parse(Some(
            r#"{"good":{"server":"a","expires_at":9},"bad":{"server":1},"worse":3}"#,
        ));
        assert_eq!(partly.lookup("good", 1.0), Lookup::Found("a".into()));
        assert_eq!(partly.lookup("bad", 1.0), Lookup::Unknown);
        let mut merged = partly.clone();
        merged.merge(pending);
        assert_eq!(merged.lookup("s1", 1.0), Lookup::Found("github".into()));
        assert_eq!(merged.lookup("good", 1.0), Lookup::Found("a".into()));
    }

    #[test]
    fn channel_messages_round_trip() {
        for message in [
            ChannelMessage::Callback {
                state: "s".into(),
                url: "https://h/?oauth=callback&code=c&state=s".into(),
            },
            ChannelMessage::Taken { state: "s".into() },
        ] {
            assert_eq!(ChannelMessage::parse(&message.to_json()), Some(message));
        }
        assert_eq!(
            ChannelMessage::parse(r#"{"kind":"other","state":"s"}"#),
            None
        );
        assert_eq!(
            ChannelMessage::parse(r#"{"kind":"callback","state":"s"}"#),
            None
        );
        assert_eq!(ChannelMessage::parse("not json"), None);
    }
}
