//! The page: a header with the connection and the theme toggle, the connect
//! screen (spec 7.2), and, once connected, the main area (sessions come in
//! phase W2).

use cookie_agent_client_state::subscriptions::{RECONNECT_FIRST_DELAY, next_reconnect_delay};
use cookie_agent_protocol::{Client, ClientDelivery, ClientError, ClientIdentity, ServerHello};
use leptos::{prelude::*, task::spawn_local};

use crate::{
    browser,
    connect::{
        DAEMON_URL_KEY, THEME_KEY, connects_by_itself, daemon_url_for_page, fragment_without_token,
        refused_before_handshake_hint, token_from_fragment, token_key,
    },
    connection,
    theme::ThemePreference,
};

/// How long a check of the connection after the tab became visible may take
/// before the page redials.
const VISIBLE_CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Where the page's connection stands.
#[derive(Clone, Debug, PartialEq)]
enum Phase {
    /// Waiting for a URL and a token.
    Idle,
    Connecting,
    Connected {
        address: String,
        engine_version: String,
        protocol_version: String,
    },
    /// The connection dropped; the page redials with the shared backoff.
    Reconnecting {
        reason: String,
    },
    /// Connecting failed; `hint` says what to change.
    Failed {
        message: String,
        hint: Option<String>,
    },
}

/// The page's connection and its inputs, shared by the components.
#[derive(Clone, Copy)]
struct Connection {
    url: RwSignal<String>,
    token: RwSignal<String>,
    phase: RwSignal<Phase>,
    identity: StoredValue<ClientIdentity>,
    client: StoredValue<Option<Client>>,
    /// Bumped by every connect and forget, so a superseded attempt or
    /// watcher stops.
    generation: StoredValue<u64>,
}

impl Connection {
    fn new() -> Self {
        let filled = browser::page_href()
            .as_deref()
            .and_then(daemon_url_for_page)
            .unwrap_or_default();
        let url = browser::load(DAEMON_URL_KEY).unwrap_or(filled);
        // A token in the fragment is kept for the daemon and taken out of
        // the address bar, so it never lands in history or a bookmark.
        let fragment = browser::page_fragment().unwrap_or_default();
        if let Some(token) = token_from_fragment(&fragment) {
            browser::store(&token_key(&url), &token);
            browser::replace_fragment(&fragment_without_token(&fragment));
        }
        let token = browser::load(&token_key(&url)).unwrap_or_default();
        Self {
            url: RwSignal::new(url),
            token: RwSignal::new(token),
            phase: RwSignal::new(Phase::Idle),
            identity: StoredValue::new(connection::tab_identity()),
            client: StoredValue::new(None),
            generation: StoredValue::new(0),
        }
    }

    fn next_generation(self) -> u64 {
        self.generation.update_value(|generation| *generation += 1);
        self.generation.get_value()
    }

    fn is_current(self, generation: u64) -> bool {
        self.generation.get_value() == generation
    }

    /// Connects with the URL and token the screen holds.
    fn connect(self) {
        let url = self.url.get_untracked().trim().to_owned();
        let token = self.token.get_untracked().trim().to_owned();
        if !connects_by_itself(Some(&url), Some(&token)) {
            self.phase.set(Phase::Failed {
                message: "Enter the daemon URL and its token.".into(),
                hint: None,
            });
            return;
        }
        self.drop_client();
        let generation = self.next_generation();
        self.phase.set(Phase::Connecting);
        spawn_local(async move {
            let opened = connection::open(&url, &token, self.identity.get_value()).await;
            if !self.is_current(generation) {
                return;
            }
            match opened {
                Ok((client, hello)) => {
                    browser::store(DAEMON_URL_KEY, &url);
                    browser::store(&token_key(&url), &token);
                    self.adopt(generation, &url, client, &hello);
                }
                Err(error) => self.phase.set(failure(&error)),
            }
        });
    }

    /// Makes `client` the page's connection and watches it for a drop.
    fn adopt(self, generation: u64, url: &str, client: Client, hello: &ServerHello) {
        let deliveries = client.subscribe_deliveries();
        self.client.set_value(Some(client));
        self.phase.set(Phase::Connected {
            address: url.to_owned(),
            engine_version: hello.engine_version.clone(),
            protocol_version: hello.protocol_version.to_string(),
        });
        let Some(mut deliveries) = deliveries else {
            return;
        };
        spawn_local(async move {
            let reason = loop {
                match deliveries.recv().await {
                    Some(ClientDelivery::Disconnected { error }) => break error.to_string(),
                    Some(_) => {}
                    None => break "the connection closed".to_owned(),
                }
            };
            if self.is_current(generation) {
                self.reconnect(generation, reason);
            }
        });
    }

    /// Redials after a drop with the shared backoff and the tab's identity,
    /// until it connects or the user connects or forgets anew.
    fn reconnect(self, generation: u64, reason: String) {
        self.drop_client();
        self.phase.set(Phase::Reconnecting { reason });
        let url = self.url.get_untracked();
        let token = self.token.get_untracked();
        spawn_local(async move {
            let mut delay = RECONNECT_FIRST_DELAY;
            loop {
                gloo_timers::future::sleep(delay).await;
                if !self.is_current(generation) {
                    return;
                }
                match connection::open(&url, &token, self.identity.get_value()).await {
                    Ok((client, hello)) if self.is_current(generation) => {
                        self.adopt(generation, &url, client, &hello);
                        return;
                    }
                    Ok((client, _)) => {
                        client.shutdown();
                        return;
                    }
                    Err(error) => {
                        if !self.is_current(generation) {
                            return;
                        }
                        self.phase.set(Phase::Reconnecting {
                            reason: error.to_string(),
                        });
                        delay = next_reconnect_delay(delay);
                    }
                }
            }
        });
    }

    /// Checks the connection once the tab is visible again, as a phone
    /// suspends background tabs, and redials when it does not answer.
    fn check(self) {
        let Some(client) = self.client.get_value() else {
            return;
        };
        let generation = self.generation.get_value();
        spawn_local(async move {
            let answer = std::pin::pin!(client.runtime_snapshot());
            let timeout = gloo_timers::future::sleep(VISIBLE_CHECK_TIMEOUT);
            let answered = matches!(
                futures_util::future::select(answer, timeout).await,
                futures_util::future::Either::Left((Ok(_), _))
            );
            if !answered && self.is_current(generation) {
                let generation = self.next_generation();
                self.reconnect(generation, "the daemon stopped answering".into());
            }
        });
    }

    /// Deletes the stored token and disconnects.
    fn forget(self) {
        self.next_generation();
        self.drop_client();
        browser::forget(&token_key(self.url.get_untracked().trim()));
        self.token.set(String::new());
        self.phase.set(Phase::Idle);
    }

    fn drop_client(self) {
        if let Some(client) = self.client.get_value() {
            client.shutdown();
        }
        self.client.set_value(None);
    }
}

/// What the connect screen shows for a failed connection.
fn failure(error: &ClientError) -> Phase {
    match error {
        ClientError::ConnectionRefusedBeforeHandshake { .. } => Phase::Failed {
            message: error.to_string(),
            hint: Some(refused_before_handshake_hint(
                &browser::page_origin().unwrap_or_default(),
            )),
        },
        _ if error.error_code() == Some("unauthenticated") => Phase::Failed {
            message: "The daemon refused the token.".into(),
            hint: Some(
                "Run `cookie web` on the daemon's machine for a link with its token, \
                 or paste the token from its ready line."
                    .into(),
            ),
        },
        _ => Phase::Failed {
            message: error.to_string(),
            hint: None,
        },
    }
}

/// The page.
#[component]
pub fn App() -> impl IntoView {
    let theme = RwSignal::new(ThemePreference::parse(browser::load(THEME_KEY).as_deref()));
    let system_dark = RwSignal::new(browser::system_prefers_dark());
    browser::on_system_scheme_change(move || system_dark.set(browser::system_prefers_dark()));
    Effect::new(move || {
        let preference = theme.get();
        browser::set_dark(preference.is_dark(system_dark.get()));
        browser::store(THEME_KEY, preference.as_str());
    });

    let connection = Connection::new();
    browser::on_visible(move || connection.check());
    if connects_by_itself(
        Some(&connection.url.get_untracked()),
        Some(&connection.token.get_untracked()),
    ) {
        connection.connect();
    }

    view! {
        <div class="flex min-h-screen flex-col bg-cream text-espresso dark:bg-night dark:text-latte">
            <Header connection=connection theme=theme />
            <main class="flex flex-1 items-start justify-center px-4 py-10">
                {move || match connection.phase.get() {
                    Phase::Connected { address, engine_version, protocol_version } => {
                        view! {
                            <Connected
                                connection=connection
                                address=address
                                engine_version=engine_version
                                protocol_version=protocol_version
                            />
                        }
                            .into_any()
                    }
                    phase => view! { <ConnectScreen connection=connection phase=phase /> }.into_any(),
                }}
            </main>
        </div>
    }
}

#[component]
fn Header(connection: Connection, theme: RwSignal<ThemePreference>) -> impl IntoView {
    let status = move || match connection.phase.get() {
        Phase::Idle | Phase::Failed { .. } => "not connected".to_owned(),
        Phase::Connecting => "connecting…".to_owned(),
        Phase::Connected { address, .. } => format!("connected to {address}"),
        Phase::Reconnecting { .. } => "reconnecting…".to_owned(),
    };
    view! {
        <header class="flex items-center gap-3 border-b border-toasted bg-parchment px-4 py-2 dark:border-ember dark:bg-char">
            <span class="font-mono font-bold">"cookie agent"</span>
            <span class="flex-1 truncate font-mono text-sm text-ink dark:text-sand">{status}</span>
            <button
                type="button"
                class="rounded border border-crust px-3 py-1 text-sm hover:bg-toasted active:bg-crust dark:border-smoke dark:hover:bg-ember dark:active:bg-smoke"
                on:click=move |_| theme.update(|preference| *preference = preference.next())
            >
                {move || theme.get().label()}
            </button>
        </header>
    }
}

#[component]
fn ConnectScreen(connection: Connection, phase: Phase) -> impl IntoView {
    let busy = matches!(phase, Phase::Connecting | Phase::Reconnecting { .. });
    let notice = match phase {
        Phase::Failed { message, hint } => Some((message, hint)),
        Phase::Reconnecting { reason } => {
            Some((format!("Connection lost ({reason}); reconnecting…"), None))
        }
        _ => None,
    };
    view! {
        <form
            class="flex w-full max-w-xl flex-col gap-4 rounded-lg border border-toasted bg-parchment p-6 dark:border-ember dark:bg-char"
            on:submit=move |event| {
                event.prevent_default();
                connection.connect();
            }
        >
            <h1 class="text-xl font-bold">"Connect to a cookie daemon"</h1>
            <label class="flex flex-col gap-1 text-sm">
                <span>"Daemon URL"</span>
                <input
                    type="url"
                    class="rounded border border-crust bg-cream px-3 py-2 font-mono text-espresso hover:border-caramel focus:border-caramel focus:outline-none dark:border-smoke dark:bg-night dark:text-latte dark:hover:border-maple dark:focus:border-maple"
                    placeholder="wss://host:7419/ws"
                    prop:value=move || connection.url.get()
                    on:input=move |event| {
                        let url = event_target_value(&event);
                        connection
                            .token
                            .set(browser::load(&token_key(url.trim())).unwrap_or_default());
                        connection.url.set(url);
                    }
                />
            </label>
            <label class="flex flex-col gap-1 text-sm">
                <span>"Token"</span>
                <input
                    type="password"
                    autocomplete="off"
                    class="rounded border border-crust bg-cream px-3 py-2 font-mono text-espresso hover:border-caramel focus:border-caramel focus:outline-none dark:border-smoke dark:bg-night dark:text-latte dark:hover:border-maple dark:focus:border-maple"
                    placeholder="paste the token, or open the link `cookie web` prints"
                    prop:value=move || connection.token.get()
                    on:input=move |event| connection.token.set(event_target_value(&event))
                />
            </label>
            {notice
                .map(|(message, hint)| {
                    view! {
                        <div class="rounded border border-caramel p-3 text-sm dark:border-maple">
                            <p>{message}</p>
                            {hint.map(|hint| view! { <p class="mt-2 font-mono">{hint}</p> })}
                        </div>
                    }
                })}
            <div class="flex flex-wrap gap-3">
                <button
                    type="submit"
                    disabled=busy
                    class="rounded bg-espresso px-4 py-2 font-bold text-cream hover:bg-caramel active:bg-ink disabled:opacity-60 dark:bg-latte dark:text-night dark:hover:bg-maple dark:active:bg-sand"
                >
                    {if busy { "Connecting…" } else { "Connect" }}
                </button>
                <button
                    type="button"
                    class="rounded border border-crust px-4 py-2 hover:bg-toasted active:bg-crust dark:border-smoke dark:hover:bg-ember dark:active:bg-smoke"
                    on:click=move |_| connection.forget()
                >
                    "Forget this daemon"
                </button>
            </div>
        </form>
    }
}

#[component]
fn Connected(
    connection: Connection,
    address: String,
    engine_version: String,
    protocol_version: String,
) -> impl IntoView {
    view! {
        <section
            id="connected"
            class="flex w-full max-w-xl flex-col gap-2 rounded-lg border border-toasted bg-parchment p-6 dark:border-ember dark:bg-char"
        >
            <h1 class="text-xl font-bold">"Connected"</h1>
            <p class="font-mono text-sm">{address}</p>
            <p class="text-sm text-ink dark:text-sand">
                "cookie " {engine_version} " · client protocol " {protocol_version}
            </p>
            <p class="text-sm text-ink dark:text-sand">
                "Sessions arrive in a later version of this page."
            </p>
            <div>
                <button
                    type="button"
                    class="mt-2 rounded border border-crust px-4 py-2 hover:bg-toasted active:bg-crust dark:border-smoke dark:hover:bg-ember dark:active:bg-smoke"
                    on:click=move |_| connection.forget()
                >
                    "Forget this daemon"
                </button>
            </div>
        </section>
    }
}
