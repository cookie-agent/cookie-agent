//! The page: a header with the connection and the theme toggle, the connect
//! screen (spec 7.2), and, once connected, the workspace: sessions, the
//! transcript, and the composer.

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
    icons::{self, Glyph},
    theme::ThemePreference,
    views::WorkspaceView,
    workspace::Workspace,
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
    /// What the page shows of the daemon; it outlives a redial.
    workspace: Workspace,
    /// The workspace has had a connection, so a redial keeps it on screen.
    attached: RwSignal<bool>,
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
            workspace: Workspace::new(),
            attached: RwSignal::new(false),
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
        self.client.set_value(Some(client.clone()));
        self.workspace.adopt(client);
        self.attached.set(true);
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
                    Some(delivery) if self.is_current(generation) => {
                        self.workspace.handle(delivery);
                    }
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
        self.workspace.disconnected();
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
        self.attached.set(false);
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
    browser::follow_visual_viewport();
    if connects_by_itself(
        Some(&connection.url.get_untracked()),
        Some(&connection.token.get_untracked()),
    ) {
        connection.connect();
    }

    view! {
        <div class="app-frame flex flex-col overflow-hidden bg-cream text-espresso dark:bg-night dark:text-latte">
            <Header connection=connection theme=theme />
            {move || {
                let phase = connection.phase.get();
                let attached = connection.attached.get();
                match phase {
                    Phase::Connected { .. } => {
                        view! { <WorkspaceView workspace=connection.workspace /> }.into_any()
                    }
                    Phase::Reconnecting { reason } if attached => {
                        view! {
                            <div id="reconnecting" role="status" class="flex items-center gap-2 border-b border-caramel bg-toasted px-3 py-2 text-sm dark:border-maple dark:bg-ember">
                                <Glyph icon=icondata::FaArrowsRotateSolid class="animate-spin text-caramel dark:text-maple" />
                                <span class="min-w-0 flex-1">{format!("Connection lost ({reason}); reconnecting…")}</span>
                            </div>
                            <WorkspaceView workspace=connection.workspace />
                        }
                            .into_any()
                    }
                    phase => {
                        view! {
                            <main class="flex flex-1 items-start justify-center overflow-y-auto px-4 py-10">
                                <ConnectScreen connection=connection phase=phase />
                            </main>
                        }
                            .into_any()
                    }
                }
            }}
        </div>
    }
}

/// An icon-only button in the header.
const HEADER_BUTTON: &str = "inline-flex min-h-11 min-w-11 items-center justify-center rounded-md text-ink hover:bg-toasted hover:text-espresso active:bg-crust dark:text-sand dark:hover:bg-ember dark:hover:text-latte dark:active:bg-smoke";

#[component]
fn Header(connection: Connection, theme: RwSignal<ThemePreference>) -> impl IntoView {
    view! {
        <header class="flex items-center gap-2 border-b border-toasted bg-parchment px-3 dark:border-ember dark:bg-char">
            <span class="flex min-w-0 flex-1 items-center gap-2 font-mono font-bold whitespace-nowrap">
                <img src="icons/icon.svg" alt="" class="h-6 w-6 shrink-0" />
                "cookie agent"
            </span>
            <ConnectionStatus connection=connection />
            <button
                type="button"
                class=HEADER_BUTTON
                aria-label="Theme"
                data-theme=move || theme.get().name()
                title=move || {
                    let preference = theme.get();
                    format!("Theme: {} (click for {})", preference.name(), preference.next().name())
                }
                on:click=move |_| theme.update(|preference| *preference = preference.next())
            >
                <Glyph icon=Signal::derive(move || icons::theme_icon(theme.get())) class="text-lg" />
            </button>
        </header>
    }
}

/// The connection's state as a small indicator; a click shows the daemon's
/// details, the way to forget it, and the page's credits.
#[component]
fn ConnectionStatus(connection: Connection) -> impl IntoView {
    let open = RwSignal::new(false);
    let confirming = RwSignal::new(false);
    let close = move || {
        open.set(false);
        confirming.set(false);
    };
    let indicator = move || {
        let (icon, label, class) = match connection.phase.get() {
            Phase::Connected { .. } => (icondata::FaCircleSolid, "connected", "text-[0.5rem]"),
            Phase::Connecting => (
                icondata::FaCircleNotchSolid,
                "connecting",
                "animate-spin text-sm",
            ),
            Phase::Reconnecting { .. } => (
                icondata::FaArrowsRotateSolid,
                "reconnecting",
                "animate-spin text-sm",
            ),
            Phase::Idle | Phase::Failed { .. } => {
                (icondata::FaPlugCircleXmarkSolid, "offline", "text-sm")
            }
        };
        view! {
            <Glyph icon=icon class=class />
            <span class="text-xs" id=(label == "connected").then_some("connected")>{label}</span>
        }
    };
    let tone = move || match connection.phase.get() {
        Phase::Connected { .. } => "text-green-700 dark:text-green-400",
        Phase::Connecting | Phase::Reconnecting { .. } => "text-caramel dark:text-maple",
        Phase::Idle | Phase::Failed { .. } => "text-ink dark:text-sand",
    };
    let details = move || match connection.phase.get() {
        Phase::Connected {
            address,
            engine_version,
            protocol_version,
        } => vec![
            ("Daemon", address),
            ("Version", format!("cookie {engine_version}")),
            ("Client protocol", protocol_version),
        ],
        Phase::Connecting => vec![("State", "connecting…".to_owned())],
        Phase::Reconnecting { reason } => vec![("Connection lost", reason)],
        Phase::Idle => vec![("State", "not connected".to_owned())],
        Phase::Failed { message, .. } => vec![("Connecting failed", message)],
    };
    let summary = move || {
        details()
            .into_iter()
            .map(|(name, value)| format!("{name}: {value}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    view! {
        <div class="relative">
            <button
                type="button"
                id="connection"
                class=move || format!("inline-flex min-h-11 min-w-11 items-center justify-center gap-1.5 rounded-full px-3 hover:bg-toasted active:bg-crust dark:hover:bg-ember dark:active:bg-smoke {}", tone())
                aria-haspopup="dialog"
                aria-expanded=move || open.get().to_string()
                title=summary
                on:click=move |_| {
                    if open.get_untracked() { close() } else { open.set(true) }
                }
            >
                {indicator}
            </button>
            <Show when=move || open.get()>
                <div class="fixed inset-0 z-30" on:click=move |_| close()></div>
                <div
                    id="connection-details"
                    role="dialog"
                    aria-label="Connection"
                    class="absolute top-full right-0 z-40 mt-1 flex w-80 max-w-[calc(100vw-1.5rem)] flex-col gap-3 rounded-lg border border-toasted bg-cream p-3 text-sm shadow-lg dark:border-ember dark:bg-night"
                >
                    <div class="flex items-center gap-2">
                        <p class="flex-1 font-bold">"Connection"</p>
                        <button
                            type="button"
                            id="connection-close"
                            class=HEADER_BUTTON
                            aria-label="Close"
                            title="Close"
                            on:click=move |_| close()
                        >
                            <Glyph icon=icondata::FaXmarkSolid />
                        </button>
                    </div>
                    <dl class="grid grid-cols-[auto_1fr] gap-x-3 gap-y-1">
                        {move || {
                            details()
                                .into_iter()
                                .map(|(name, value)| {
                                    view! {
                                        <dt class="text-ink dark:text-sand">{name}</dt>
                                        <dd class="min-w-0 font-mono text-xs break-all">{value}</dd>
                                    }
                                })
                                .collect_view()
                        }}
                    </dl>
                    <Show when=move || connection.attached.get()>
                        <Show
                            when=move || confirming.get()
                            fallback=move || {
                                view! {
                                    <button
                                        type="button"
                                        id="forget"
                                        class="inline-flex min-h-11 items-center justify-center gap-2 rounded-md border border-crust px-3 text-sm hover:bg-toasted active:bg-crust dark:border-smoke dark:hover:bg-ember dark:active:bg-smoke"
                                        on:click=move |_| confirming.set(true)
                                    >
                                        <Glyph icon=icondata::FaRightFromBracketSolid />
                                        "Forget this daemon"
                                    </button>
                                }
                            }
                        >
                            <div class="flex flex-col gap-2 rounded-md border border-caramel p-2 dark:border-maple">
                                <p>"Delete this daemon's token from the browser and disconnect? Connecting again takes a new link from `cookie web`."</p>
                                <div class="flex flex-wrap gap-2">
                                    <button
                                        type="button"
                                        id="forget-confirm"
                                        class="inline-flex min-h-11 items-center justify-center gap-2 rounded-md bg-espresso px-3 font-bold text-cream hover:bg-caramel active:bg-ink dark:bg-latte dark:text-night dark:hover:bg-maple dark:active:bg-sand"
                                        on:click=move |_| {
                                            close();
                                            connection.forget();
                                        }
                                    >
                                        "Forget"
                                    </button>
                                    <button
                                        type="button"
                                        class="inline-flex min-h-11 items-center justify-center rounded-md border border-crust px-3 hover:bg-toasted active:bg-crust dark:border-smoke dark:hover:bg-ember dark:active:bg-smoke"
                                        on:click=move |_| confirming.set(false)
                                    >
                                        "Keep it"
                                    </button>
                                </div>
                            </div>
                        </Show>
                    </Show>
                    <Credits />
                </div>
            </Show>
        </div>
    }
}

/// The licence credit for the page's icons.
#[component]
fn Credits() -> impl IntoView {
    view! {
        <p class="text-xs text-ink dark:text-sand">
            <a
                href=icons::CREDIT_URL
                target="_blank"
                rel="noopener noreferrer"
                class="inline-flex min-h-11 items-center underline decoration-crust underline-offset-2 hover:text-caramel dark:decoration-smoke dark:hover:text-maple"
            >
                {icons::CREDIT}
            </a>
        </p>
    }
}

const FIELD: &str = "min-h-11 rounded-md border border-crust bg-cream px-3 py-2 font-mono text-espresso hover:border-caramel focus:border-caramel focus:outline-none dark:border-smoke dark:bg-night dark:text-latte dark:hover:border-maple dark:focus:border-maple";

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
        <div class="flex w-full max-w-xl flex-col gap-3">
            <form
                class="flex flex-col gap-4 rounded-lg border border-toasted bg-parchment p-6 dark:border-ember dark:bg-char"
                on:submit=move |event| {
                    event.prevent_default();
                    connection.connect();
                }
            >
                <div class="flex items-center gap-3">
                    <img src="icons/icon.svg" alt="" class="h-10 w-10 shrink-0" />
                    <div>
                        <h1 class="text-xl font-bold">"Connect to a cookie daemon"</h1>
                        <p class="text-sm text-ink dark:text-sand">
                            "Open the link `cookie web` prints, or enter the daemon's URL and token."
                        </p>
                    </div>
                </div>
                <label class="flex flex-col gap-1 text-sm">
                    <span class="font-bold">"Daemon URL"</span>
                    <input
                        type="url"
                        class=FIELD
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
                    <span class="font-bold">"Token"</span>
                    <input
                        type="password"
                        autocomplete="off"
                        class=FIELD
                        placeholder="paste the token, or open the link `cookie web` prints"
                        prop:value=move || connection.token.get()
                        on:input=move |event| connection.token.set(event_target_value(&event))
                    />
                </label>
                {notice
                    .map(|(message, hint)| {
                        view! {
                            <div role="alert" class="flex gap-2 rounded-md border border-caramel bg-cream p-3 text-sm dark:border-maple dark:bg-night">
                                <Glyph icon=icondata::FaTriangleExclamationSolid class="mt-0.5 text-caramel dark:text-maple" />
                                <div class="min-w-0">
                                    <p>{message}</p>
                                    {hint.map(|hint| view! { <p class="mt-2 font-mono break-words">{hint}</p> })}
                                </div>
                            </div>
                        }
                    })}
                <div class="flex flex-wrap gap-3">
                    <button
                        type="submit"
                        disabled=busy
                        class="inline-flex min-h-11 items-center justify-center gap-2 rounded-md bg-espresso px-4 font-bold text-cream hover:bg-caramel active:bg-ink disabled:cursor-not-allowed disabled:opacity-50 dark:bg-latte dark:text-night dark:hover:bg-maple dark:active:bg-sand"
                    >
                        <Glyph
                            icon=if busy { icondata::FaCircleNotchSolid } else { icondata::FaPlugSolid }
                            class=if busy { "animate-spin" } else { "" }
                        />
                        {if busy { "Connecting…" } else { "Connect" }}
                    </button>
                    <button
                        type="button"
                        class="inline-flex min-h-11 items-center justify-center gap-2 rounded-md border border-crust px-4 hover:bg-toasted active:bg-crust dark:border-smoke dark:hover:bg-ember dark:active:bg-smoke"
                        title="Delete the stored token for this URL"
                        on:click=move |_| connection.forget()
                    >
                        <Glyph icon=icondata::FaRightFromBracketSolid />
                        <span class="sm:hidden">"Forget"</span>
                        <span class="hidden sm:inline">"Forget this daemon"</span>
                    </button>
                </div>
            </form>
            <footer class="px-2 text-center">
                <Credits />
            </footer>
        </div>
    }
}
