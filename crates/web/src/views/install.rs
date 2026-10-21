//! Installing the page as an app (spec 9): what the browser says about it,
//! kept as the page's context, and the three places that offer it: Settings
//! › This browser, the connect screen, and a phone's one-time banner.
//! [`crate::install`] decides which offer each makes.

use leptos::{prelude::*, task::spawn_local};

use super::{BUTTON, ICON_BUTTON, NOTE, PRIMARY};
use crate::{
    browser,
    icons::Glyph,
    install::{BANNER_KEY, InstallContext, InstallOffer, banner_dismissed, is_ios, shows_banner},
};

/// What the browser says about installing the page, as it changes.
#[derive(Clone, Copy)]
pub(crate) struct Install {
    context: RwSignal<InstallContext>,
    /// The window is a phone's.
    phone: RwSignal<bool>,
    /// The phone's banner was dismissed, now or before.
    dismissed: RwSignal<bool>,
}

impl Install {
    /// Reads the browser, follows its install events, and makes the result
    /// the page's context.
    pub(crate) fn provide() {
        let (agent, touches) = browser::user_agent();
        let context = RwSignal::new(InstallContext {
            secure: browser::is_secure_context(),
            standalone: browser::is_standalone(),
            installed: browser::app_installed(),
            prompt: browser::install_prompt_ready(),
            ios: is_ios(&agent, touches),
        });
        browser::on_install_change(move || {
            context.update(|context| {
                context.prompt = browser::install_prompt_ready();
                context.installed |= browser::app_installed();
            });
        });
        browser::on_media_change(browser::STANDALONE_QUERY, move |standalone| {
            context.update(|context| context.standalone = standalone);
        });
        let phone = RwSignal::new(browser::matches(browser::PHONE_QUERY));
        browser::on_media_change(browser::PHONE_QUERY, move |narrow| phone.set(narrow));
        let dismissed = RwSignal::new(banner_dismissed(browser::load(BANNER_KEY).as_deref()));
        browser::register_service_worker();
        provide_context(Self {
            context,
            phone,
            dismissed,
        });
    }

    fn get() -> Self {
        expect_context::<Self>()
    }

    fn offer(self) -> InstallOffer {
        self.context.get().offer()
    }

    /// Opens the browser's prompt; the page forgets it, which the browser
    /// offers once.
    fn prompt(self) {
        let Some(choice) = browser::prompt_install() else {
            return;
        };
        self.context.update(|context| context.prompt = false);
        spawn_local(async move {
            if choice.await {
                self.context.update(|context| context.installed = true);
            }
        });
    }

    fn dismiss(self) {
        browser::store(BANNER_KEY, "dismissed");
        self.dismissed.set(true);
    }
}

/// iOS's steps: Share, then Add to Home Screen. The app it adds keeps its
/// own storage, apart from the browser's, so it needs the token once.
fn ios_steps() -> impl IntoView {
    view! {
        <div data-install-steps class="flex flex-col gap-2 text-sm">
            <ol class="flex flex-col gap-1">
                <li class="flex items-center gap-2">
                    <Glyph icon=icondata::FaArrowUpFromBracketSolid class="w-4 justify-center text-caramel dark:text-maple" />
                    <span>"Tap " <b>"Share"</b> " in the browser's toolbar."</span>
                </li>
                <li class="flex items-center gap-2">
                    <Glyph icon=icondata::FaSquarePlusRegular class="w-4 justify-center text-caramel dark:text-maple" />
                    <span>"Choose " <b>"Add to Home Screen"</b> ", then " <b>"Add"</b> "."</span>
                </li>
            </ol>
            <p class=NOTE>
                "The app keeps its own storage: the first time it opens, paste the link `cookie web` prints into its Token field."
            </p>
        </div>
    }
}

/// **Install app**, which opens the browser's prompt.
fn prompt_button(install: Install, class: &'static str) -> impl IntoView {
    view! {
        <button type="button" data-install-action class=class on:click=move |_| install.prompt()>
            <Glyph icon=icondata::FaDownloadSolid />
            "Install app"
        </button>
    }
}

/// Settings › This browser: how to install the page here, or why it
/// cannot be.
pub(crate) fn install_section() -> impl IntoView {
    let install = Install::get();
    let body = move || {
        match install.offer() {
        InstallOffer::Installed => {
            let text = if install.context.get().standalone {
                "Running as an installed app."
            } else {
                "Installed: open cookie from the home screen or the app list."
            };
            view! {
                <p class="flex items-center gap-2 text-sm">
                    <Glyph icon=icondata::FaCircleCheckSolid class="text-green-700 dark:text-green-400" />
                    {text}
                </p>
            }
            .into_any()
        }
        InstallOffer::Prompt => view! {
            <p class=NOTE>
                "It opens in its own window, without the address bar, and works while it reaches the daemon."
            </p>
            <div>{prompt_button(install, PRIMARY)}</div>
        }
        .into_any(),
        InstallOffer::AddToHomeScreen => ios_steps().into_any(),
        InstallOffer::NeedsHttps => view! {
            <p class="flex items-start gap-2 text-sm">
                <Glyph icon=icondata::FaLockSolid class="mt-0.5 text-caramel dark:text-maple" />
                <span>
                    "Installing needs HTTPS, e.g. "
                    <code class="font-mono text-xs">"tailscale serve --bg 7419"</code>
                    " on the daemon's machine."
                </span>
            </p>
        }
        .into_any(),
        InstallOffer::BrowserMenu => view! {
            <p class=NOTE>
                "This browser has not offered to install the page. Chrome and Edge offer it once the page has been used for a moment; elsewhere, look for Install or Add to Home screen in the browser's menu."
            </p>
        }
        .into_any(),
    }
    };
    view! {
        <h2 class="text-sm font-bold">"Install as an app"</h2>
        <div id="install" class="flex flex-col gap-2" data-install-offer=move || offer_name(install.offer())>
            {body}
        </div>
    }
}

/// The connect screen's **Install app**, where the browser can install the
/// page: its prompt, or iOS's steps under the button.
#[component]
pub(crate) fn InstallAction() -> impl IntoView {
    let install = Install::get();
    let steps = RwSignal::new(false);
    view! {
        {move || match install.offer() {
            InstallOffer::Prompt => prompt_button(install, BUTTON).into_any(),
            InstallOffer::AddToHomeScreen => view! {
                <button
                    type="button"
                    data-install-action
                    class=BUTTON
                    aria-expanded=move || steps.get().to_string()
                    on:click=move |_| steps.update(|shown| *shown = !*shown)
                >
                    <Glyph icon=icondata::FaMobileScreenButtonSolid />
                    "Install app"
                </button>
            }
            .into_any(),
            _ => ().into_any(),
        }}
        <Show when=move || steps.get() && install.offer() == InstallOffer::AddToHomeScreen>
            <div class="w-full rounded-md border border-toasted bg-cream p-3 dark:border-ember dark:bg-night">
                {ios_steps()}
            </div>
        </Show>
    }
}

/// A phone's banner over the workspace, until it is dismissed: **Install
/// app**, or iOS's steps.
#[component]
pub(crate) fn InstallBanner() -> impl IntoView {
    let install = Install::get();
    let shown = move || {
        shows_banner(
            install.offer(),
            install.phone.get(),
            install.dismissed.get(),
        )
    };
    view! {
        <Show when=shown>
            <div
                id="install-banner"
                role="region"
                aria-label="Install"
                // iOS's steps make it taller: its × stays at the top.
                class=move || {
                    let align = if install.offer() == InstallOffer::AddToHomeScreen {
                        "items-start"
                    } else {
                        "items-center"
                    };
                    format!("flex shrink-0 {align} gap-2 border-b border-toasted bg-parchment py-1 pr-1 pl-3 text-sm dark:border-ember dark:bg-char")
                }
            >
                <div class="flex min-w-0 flex-1 flex-col gap-1 py-2">
                    <p class="flex items-center gap-2 font-bold">
                        <Glyph icon=icondata::FaMobileScreenButtonSolid class="text-caramel dark:text-maple" />
                        "Install Cookie Agent"
                    </p>
                    {move || (install.offer() == InstallOffer::AddToHomeScreen).then(ios_steps)}
                </div>
                {move || (install.offer() == InstallOffer::Prompt).then(|| prompt_button(install, PRIMARY))}
                <button
                    type="button"
                    data-install-dismiss
                    class=ICON_BUTTON
                    aria-label="Dismiss"
                    title="Dismiss"
                    on:click=move |_| install.dismiss()
                >
                    <Glyph icon=icondata::FaXmarkSolid />
                </button>
            </div>
        </Show>
    }
}

/// The offer's `data-install-offer`, for the page's scripts.
const fn offer_name(offer: InstallOffer) -> &'static str {
    match offer {
        InstallOffer::Installed => "installed",
        InstallOffer::Prompt => "prompt",
        InstallOffer::AddToHomeScreen => "ios",
        InstallOffer::NeedsHttps => "needs-https",
        InstallOffer::BrowserMenu => "browser-menu",
    }
}
