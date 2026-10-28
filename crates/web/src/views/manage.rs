//! The management panels: the session's `/runtime` sheet, and the parts
//! of the daemon-wide views: `/mcp`, `/connect`, `/config`, and
//! `/plugins`. The machines are [`super::machines`], the agent documents
//! [`super::agents`], and `/memory` [`super::memory`].

use cookie_agent_client_state::{
    config::config_rows,
    mcp::{
        McpFields, McpTransport, PersistChoice, StateTone, edit_refusal, runtime_label, scope_note,
        server_definition_display, server_message, server_state_label, server_tone,
        server_tool_names, server_tools_label, source_label, state_label as mcp_state_label,
        waits_for_workspace,
    },
    plugins::{plugin_detail, state_label as plugin_state_label},
    providers::{
        ProviderOperation, ProviderRowState, action_name, catalog_status_line, credential_values,
        initial_auth_method, initial_setup_text, model_counts_summary, row_state, setup_values,
        unusable_models_note,
    },
    runtimes::{RuntimeChoice, attach_parameters, runtime_choices, status_label},
};
use cookie_agent_protocol::{
    McpServerInfo, McpServerState, ProviderDescriptor, RuntimeInstanceStatus,
};
use leptos::prelude::*;
use wasm_bindgen::JsCast as _;

use super::{BUTTON, ERROR, ICON_BUTTON, NOTE, PRIMARY, ROW, empty_state, loading};
use crate::{
    browser,
    icons::Glyph,
    workspace::{PANEL_POLL, Workspace},
};

pub(super) const FIELD: &str = "min-h-11 w-full rounded-md border border-crust bg-cream px-3 font-mono text-sm hover:border-caramel focus:border-caramel focus:outline-none dark:border-smoke dark:bg-night dark:hover:border-maple dark:focus:border-maple";
/// A document's text: it fills what its page leaves.
pub(super) const EDITOR: &str = "min-h-[12rem] w-full flex-1 resize-y rounded-md border border-crust bg-cream p-2 font-mono text-sm hover:border-caramel focus:border-caramel focus:outline-none dark:border-smoke dark:bg-night dark:hover:border-maple dark:focus:border-maple";
/// A small label for a thing's state.
pub(super) const STATE: &str = "shrink-0 rounded-full bg-toasted px-2 py-0.5 text-xs dark:bg-ember";
/// An action of one row in a list, such as a runtime's MCP server: quiet
/// text, so a row's several actions stay out of its way, on a full touch
/// target.
pub(super) const ROW_ACTION: &str = "inline-flex min-h-11 min-w-11 items-center justify-center gap-1 rounded-md px-2 text-xs font-bold text-caramel hover:bg-crust active:bg-crust dark:text-maple dark:hover:bg-smoke dark:active:bg-smoke";
/// An editor's page: the text fills it and the actions stay at its foot.
pub(super) const EDITOR_PAGE: &str = "flex min-h-full flex-1 flex-col gap-2";
pub(super) const EDITOR_ACTIONS: &str = "sticky bottom-0 -mx-3 -mb-3 flex gap-2 border-t border-toasted bg-cream px-3 py-2 dark:border-ember dark:bg-night";
/// A confirmation before something cannot be undone.
pub(super) const CONFIRM: &str = "flex flex-col gap-2 rounded-lg border border-caramel bg-parchment p-3 dark:border-maple dark:bg-char";

/// Reads `poll` every second while the sheet is open, as the TUI polls its
/// open `/runtime` and `/mcp` panels.
pub(super) fn poll_while_open(poll: impl Fn() + 'static) {
    let ticker = set_interval_with_handle(poll, PANEL_POLL).ok();
    on_cleanup(move || {
        if let Some(ticker) = ticker {
            ticker.clear();
        }
    });
}

/// The value of the text field `id`, read once.
pub(super) fn field_value(id: &str) -> String {
    web_sys::window()
        .and_then(|window| window.document())
        .and_then(|document| document.get_element_by_id(id))
        .and_then(|element| element.dyn_into::<web_sys::HtmlInputElement>().ok())
        .map(|input| input.value())
        .unwrap_or_default()
}

/// The value of the text area `id`, read once.
pub(super) fn area_value(id: &str) -> String {
    web_sys::window()
        .and_then(|window| window.document())
        .and_then(|document| document.get_element_by_id(id))
        .and_then(|element| element.dyn_into::<web_sys::HtmlTextAreaElement>().ok())
        .map(|area| area.value())
        .unwrap_or_default()
}

// ---- /runtime --------------------------------------------------------------

/// The session's attached runtimes (`/runtime`); the daemon's machines and
/// runtimes are a view of their own.
pub fn runtimes_sheet(workspace: Workspace) -> impl IntoView {
    poll_while_open(move || workspace.poll_runtimes());
    view! {
        {attachments_view(workspace)}
        <button
            type="button"
            id="runtime-open-machines"
            class=format!("{BUTTON} mt-2 self-start")
            on:click=move |_| workspace.open_view(crate::layout::View::Machines, None)
        >
            <Glyph icon=icondata::FaServerSolid />
            "Machines & runtimes"
        </button>
    }
}

fn attachments_view(workspace: Workspace) -> impl IntoView {
    let search = RwSignal::new(String::new());
    let chosen = RwSignal::new(None::<RuntimeChoice>);
    let attaching = RwSignal::new(false);
    let rows = move || {
        workspace
            .runtime_rows()
            .into_iter()
            .map(|row| {
                let status = row.instance.as_ref().map(|instance| instance.status);
                let message = row
                    .instance
                    .as_ref()
                    .and_then(|instance| instance.message.clone());
                let failed = status == Some(RuntimeInstanceStatus::Failed);
                let servers = workspace.mcp_servers.with(|listing| match listing {
                    Some(Ok(servers)) => row.mcp_servers(servers),
                    _ => Vec::new(),
                });
                let detach_name = row.attachment.name.clone();
                let retry_row = row.clone();
                view! {
                    <div class="runtime-row flex flex-col gap-1 border-b border-toasted py-2 dark:border-ember" data-attachment=row.attachment.name.to_string()>
                        <div class="flex items-center gap-2">
                            <span class="min-w-0 flex-1 truncate font-mono text-sm font-bold">
                                {format!("{} · {}", row.attachment.name, row.attachment.kind)}
                            </span>
                            <span class=STATE class:text-red-700=failed class:dark:text-red-400=failed>
                                {status_label(status)}
                            </span>
                        </div>
                        <span class=NOTE data-runtime-place>
                            {format!(
                                "{}/{}{}",
                                row.attachment.runtime_id,
                                row.attachment.capability,
                                row.attachment
                                    .workspace
                                    .as_ref()
                                    .map(|path| format!(
                                        " at {}",
                                        workspace.homes().display(&row.attachment.runtime_id, path)
                                    ))
                                    .unwrap_or_default()
                            )}
                        </span>
                        {row.description.clone().map(|description| view! { <span class=NOTE>{description}</span> })}
                        {message.map(|message| view! { <span class=ERROR>{message}</span> })}
                        {(!servers.is_empty()).then(|| runtime_mcp_servers(servers, None, None))}
                        <div class="flex gap-2">
                            <button type="button" class=BUTTON data-retry on:click=move |_| workspace.retry_runtime(retry_row.clone())>
                                <Glyph icon=icondata::FaArrowsRotateSolid />
                                "Retry"
                            </button>
                            <button type="button" class=BUTTON data-detach on:click=move |_| workspace.detach_runtime(detach_name.clone())>
                                <Glyph icon=icondata::FaLinkSlashSolid />
                                "Detach"
                            </button>
                        </div>
                    </div>
                }
            })
            .collect_view()
    };
    let choices = move || {
        let runtimes = workspace
            .runtimes
            .with(|listing| listing.as_ref().map(|listing| listing.runtimes.clone()))
            .unwrap_or_default();
        runtime_choices(&runtimes, &search.get())
    };
    let attach = move |choice: RuntimeChoice, workspace_path: String| match attach_parameters(
        &choice,
        &workspace_path,
    ) {
        Ok((runtime_id, capability, path)) => {
            workspace.attach_runtime(runtime_id, capability, path);
            attaching.set(false);
            chosen.set(None);
        }
        Err(error) => workspace.status.set(error),
    };
    view! {
        {rows}
        <Show
            when=move || attaching.get()
            fallback=move || view! {
                <button type="button" id="runtime-attach" class=PRIMARY on:click=move |_| attaching.set(true)>
                    <Glyph icon=icondata::FaPlusSolid />
                    "Attach a runtime"
                </button>
            }
        >
            {move || match chosen.get() {
                None => view! {
                    <input
                        type="search"
                        id="runtime-search"
                        class=FIELD
                        placeholder="Search runtimes"
                        prop:value=move || search.get()
                        on:input=move |event| search.set(event_target_value(&event))
                    />
                    {move || {
                        choices()
                            .into_iter()
                            .map(|choice| {
                                let picked = choice.clone();
                                view! {
                                    <button
                                        type="button"
                                        class=ROW
                                        data-choice=choice.label()
                                        on:click=move |_| {
                                            if picked.asks_workspace() {
                                                chosen.set(Some(picked.clone()));
                                            } else {
                                                attach(picked.clone(), String::new());
                                            }
                                        }
                                    >
                                        <span class="font-mono text-sm">{choice.label()}</span>
                                        <span class=NOTE>
                                            {choice
                                                .capability
                                                .as_ref()
                                                .map(|capability| capability.description.clone())
                                                .unwrap_or_else(|| if choice.connected { String::new() } else { "not connected".into() })}
                                        </span>
                                        {choice.message.clone().map(|message| view! { <span class=ERROR>{message}</span> })}
                                    </button>
                                }
                            })
                            .collect_view()
                    }}
                    <button type="button" class=BUTTON on:click=move |_| attaching.set(false)>
                        "Cancel"
                    </button>
                }
                    .into_any(),
                Some(choice) => {
                    let label = choice.label();
                    let optional = choice.capability.is_none();
                    view! {
                        <p class="text-sm font-bold">{format!("Attach {label}")}</p>
                        <input
                            type="text"
                            id="runtime-workspace"
                            class=FIELD
                            placeholder=if optional { "workspace path (optional)" } else { "workspace path" }
                        />
                        <div class="flex gap-2">
                            <button
                                type="button"
                                id="runtime-attach-confirm"
                                class=PRIMARY
                                on:click=move |_| attach(choice.clone(), field_value("runtime-workspace"))
                            >
                                "Attach"
                            </button>
                            <button type="button" class=BUTTON on:click=move |_| chosen.set(None)>
                                "Back"
                            </button>
                        </div>
                    }
                        .into_any()
                }
            }}
        </Show>
    }
}

/// A server's tools by its own names, collapsed under its tool count: those
/// it offers, then those its filters left out, dimmed and struck through.
/// With `manage`, each is a button that leaves it out or takes it back.
/// Nothing for a server that has listed none.
fn server_tools(
    server: &McpServerInfo,
    manage: Option<Workspace>,
) -> Option<impl IntoView + use<>> {
    let (offered, left_out) = server_tool_names(server);
    if offered.is_empty() && left_out.is_empty() {
        return None;
    }
    let chip = |name: &str, filtered: bool| {
        let label = name.to_owned();
        let chip = view! {
            <span
                class="rounded bg-toasted px-1.5 font-mono text-[0.7rem] dark:bg-ember"
                class:line-through=filtered
                class:opacity-50=filtered
            >
                {label}
            </span>
        };
        let tool = name.to_owned();
        let toggled = server.clone();
        view! {
            <li
                title=if filtered {
                    "left out by the server's include_tools or exclude_tools"
                } else {
                    "offered to agents"
                }
                data-mcp-tool=name.to_owned()
                data-mcp-tool-filtered=filtered.then_some("true")
            >
                {match manage {
                    Some(workspace) => view! {
                        <button
                            type="button"
                            class="rounded hover:outline hover:outline-1 hover:outline-caramel dark:hover:outline-maple"
                            aria-label=format!("{} {name}", if filtered { "Take back" } else { "Leave out" })
                            data-mcp-tool-toggle
                            on:click=move |_| workspace.toggle_mcp_tool(&toggled, &tool, !filtered)
                        >
                            {chip}
                        </button>
                    }
                    .into_any(),
                    None => chip.into_any(),
                }}
            </li>
        }
    };
    let chips = offered
        .iter()
        .map(|name| chip(name, false))
        .chain(left_out.iter().map(|name| chip(name, true)))
        .collect_view();
    Some(view! {
        <details class="pl-4" data-mcp-tools>
            <summary class=format!("cursor-pointer select-none {NOTE}")>
                {if manage.is_some() { "Tools (select one to leave it out or take it back)" } else { "Tools" }}
            </summary>
            <ul class="mt-1 flex flex-wrap gap-1">{chips}</ul>
        </details>
    })
}

/// The MCP servers a runtime row's instance uses: each one's name, tool
/// count, and state, `runtime-wide` for one its runtime shares across
/// workspaces, and under it why it failed or what it waits for. A server
/// that waits for a workspace says it starts with one. With `workspaces`,
/// the home of their runtime, a server one workspace hosts says which.
pub(crate) fn runtime_mcp_servers(
    servers: Vec<McpServerInfo>,
    workspaces: Option<Option<String>>,
    manage: Option<(Workspace, RwSignal<Option<McpEdit>>)>,
) -> impl IntoView {
    let items = servers
        .into_iter()
        .map(|server| {
            let tone = server_tone(&server);
            let (dot, dot_class) = match tone {
                StateTone::Ready => (icondata::FaCircleSolid, "text-green-700 dark:text-green-400"),
                StateTone::Pending => (icondata::FaCircleHalfStrokeSolid, "text-amber-700 dark:text-amber-400"),
                StateTone::Failed => (icondata::FaCircleExclamationSolid, "text-red-700 dark:text-red-400"),
                StateTone::Off => (icondata::FaCircleRegular, "opacity-60"),
            };
            let failed = tone == StateTone::Failed;
            let waiting = waits_for_workspace(&server);
            let at = workspaces.as_ref().and_then(|home| {
                server.workspace.as_deref().map(|workspace| {
                    format!(
                        "at {}",
                        cookie_agent_client_state::paths::display_path(workspace, home.as_deref())
                    )
                })
            });
            view! {
                <li
                    class="flex min-w-0 flex-col gap-0.5"
                    data-mcp-server=server.name.clone()
                    data-mcp-state=mcp_state_label(server.state)
                    data-mcp-scope=server.scope.map(|scope| scope.as_str())
                    data-mcp-waiting=waiting.then_some("true")
                >
                    <div class="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-0.5">
                        <Glyph icon=dot class=format!("shrink-0 text-[0.6rem] {dot_class}") />
                        <span class="min-w-16 flex-1 truncate font-mono text-sm">{server.name.clone()}</span>
                        {(!waiting).then(|| view! { <span class=format!("shrink-0 {NOTE}")>{server_tools_label(&server)}</span> })}
                        <span class=STATE class:text-red-700=failed class:dark:text-red-400=failed>{server_state_label(&server)}</span>
                    </div>
                    {scope_note(&server).map(|note| view! {
                        <span class=format!("pl-4 {NOTE}") data-mcp-scope-note>
                            <span class="rounded bg-toasted px-1.5 text-[0.7rem] dark:bg-ember">{note}</span>
                            " one process every workspace of the runtime shares"
                        </span>
                    })}
                    {at.map(|at| view! { <span class=format!("pl-4 {NOTE} truncate")>{at}</span> })}
                    {server_tools(&server, manage.map(|(workspace, _)| workspace))}
                    {manage.filter(|_| edit_refusal(&server).is_none()).map(|(workspace, form)| {
                        let (edited, toggled, removed) = (server.clone(), server.clone(), server.clone());
                        let enable = !server.definition.enabled;
                        view! {
                            <div class="flex flex-wrap gap-1 pl-3" data-mcp-actions>
                                <button type="button" class=ROW_ACTION data-mcp-edit on:click=move |_| form.set(Some(McpEdit::edit(&edited)))>
                                    "Edit"
                                </button>
                                <button type="button" class=ROW_ACTION data-mcp-toggle on:click=move |_| workspace.toggle_mcp(&toggled, enable)>
                                    {if enable { "Enable" } else { "Disable" }}
                                </button>
                                <button type="button" class=ROW_ACTION data-mcp-remove on:click=move |_| workspace.remove_mcp(&removed)>
                                    "Remove"
                                </button>
                            </div>
                        }
                    })}
                    {server_message(&server).map(|message| view! {
                        <span
                            class=if failed {
                                "pl-4 text-xs whitespace-pre-wrap break-words text-red-800 dark:text-red-300"
                            } else {
                                "pl-4 text-xs whitespace-pre-wrap break-words opacity-80"
                            }
                            data-mcp-message
                        >
                            {message}
                        </span>
                    })}
                </li>
            }
        })
        .collect_view();
    view! {
        <div class="runtime-mcp flex flex-col gap-1 rounded-md bg-parchment px-2 py-1.5 dark:bg-char" data-runtime-mcp>
            <span class="text-xs font-bold opacity-70">"MCP servers"</span>
            <ul class="flex flex-col gap-1">{items}</ul>
        </div>
    }
}

/// An edit form over the page, so it opens in view however far the page
/// was scrolled: a centered dialog over the dimmed page on a wider screen,
/// and on a phone a page of its own, one level deeper, whose arrow goes
/// back. Escape and the backdrop close it as the form's Cancel does.
pub(crate) fn edit_dialog(
    id: &'static str,
    close: impl Fn() + Copy + Send + Sync + 'static,
    body: impl IntoView + 'static,
) -> impl IntoView {
    view! {
        <div class="fixed inset-0 z-40 hidden bg-espresso/30 sm:block dark:bg-black/50" on:click=move |_| close()></div>
        <div
            id=id
            role="dialog"
            aria-modal="true"
            data-edit-dialog
            class="fixed inset-0 z-50 flex flex-col overflow-y-auto bg-cream pb-[env(safe-area-inset-bottom)] sm:inset-auto sm:top-1/2 sm:left-1/2 sm:max-h-[85vh] sm:w-[36rem] sm:max-w-[calc(100vw-2rem)] sm:-translate-x-1/2 sm:-translate-y-1/2 sm:rounded-xl sm:border sm:border-toasted sm:pb-0 sm:shadow-lg dark:bg-night sm:dark:border-ember"
            on:keydown=move |event| {
                if event.key() == "Escape" {
                    close();
                }
            }
        >
            <div class="sticky top-0 z-10 flex items-center border-b border-toasted bg-parchment p-1 sm:justify-end sm:border-0 sm:bg-cream dark:border-ember dark:bg-char sm:dark:bg-night">
                <button type="button" class=ICON_BUTTON aria-label="Close" title="Close" data-edit-dialog-close on:click=move |_| close()>
                    <span class="inline-flex sm:hidden"><Glyph icon=icondata::FaArrowLeftSolid /></span>
                    <span class="hidden sm:inline-flex"><Glyph icon=icondata::FaXmarkSolid /></span>
                </button>
            </div>
            <div class="flex flex-col gap-2 px-3 pt-3 pb-3 sm:pt-0">{body}</div>
        </div>
    }
}

// ---- /mcp ------------------------------------------------------------------

pub fn mcp_sheet(workspace: Workspace) -> impl IntoView {
    poll_while_open(move || workspace.poll_mcp());
    // The form: its fields, the server it edits, and where it is kept.
    let form = RwSignal::new(None::<McpEdit>);
    let persist = RwSignal::new(PersistChoice::Runtime);
    let error = RwSignal::new(None::<String>);
    let list = move || {
        match workspace.mcp_servers.get() {
        None => loading("Reading MCP servers…").into_any(),
        Some(Err(message)) => view! { <p class=ERROR>{message}</p> }.into_any(),
        Some(Ok(servers)) if servers.is_empty() => {
            empty_state(icondata::FaServerSolid, "No MCP servers are configured.").into_any()
        }
        Some(Ok(servers)) => servers
            .into_iter()
            .map(|server| {
                let refusal = Workspace::mcp_refusal(&server);
                let needs_auth = server.state == McpServerState::NeedsAuth;
                let name = server.name.clone();
                let edited = server.clone();
                let removed = server.clone();
                let toggled = (server.clone(), !server.definition.enabled);
                let reconnected = server.clone();
                let editable = refusal.is_none();
                view! {
                    <div class="mcp-server flex flex-col gap-1 border-b border-toasted py-2 dark:border-ember" data-server=name.clone()>
                        <div class="flex items-center gap-2">
                            <span class="min-w-0 flex-1 truncate font-mono text-sm font-bold">{name.clone()}</span>
                            <span class=format!("mcp-state {STATE}")>{mcp_state_label(server.state)}</span>
                        </div>
                        <span class=NOTE>
                            {format!(
                                "{} · {} · {}",
                                runtime_label(&server),
                                source_label(server.source),
                                server_tools_label(&server)
                            )}
                        </span>
                        <span class=format!("{NOTE} font-mono whitespace-pre-wrap break-all")>{server_definition_display(&server)}</span>
                        {server.message.clone().map(|message| if needs_auth {
                            // Why it waits for a sign-in, not a failure.
                            view! { <span class=format!("{NOTE} whitespace-pre-wrap break-words") data-mcp-message>{message}</span> }.into_any()
                        } else {
                            view! { <span class=ERROR data-mcp-message>{message}</span> }.into_any()
                        })}
                        {needs_auth.then(|| sign_in_offer(workspace, &server))}
                        {refusal.clone().map(|refusal| view! { <span class=NOTE>{refusal}</span> })}
                        <div class="flex flex-wrap gap-2">
                            <Show when=move || editable>
                                <button type="button" class=BUTTON data-mcp-edit on:click={
                                    let edited = edited.clone();
                                    move |_| {
                                        error.set(None);
                                        form.set(Some(McpEdit::edit(&edited)));
                                    }
                                }>
                                    "Edit"
                                </button>
                                <button type="button" class=BUTTON data-mcp-toggle on:click={
                                    let toggled = toggled.clone();
                                    move |_| workspace.toggle_mcp(&toggled.0, toggled.1)
                                }>
                                    {if toggled.1 { "Enable" } else { "Disable" }}
                                </button>
                                <button type="button" class=BUTTON data-mcp-remove on:click={
                                    let removed = removed.clone();
                                    move |_| workspace.remove_mcp(&removed)
                                }>
                                    "Remove"
                                </button>
                            </Show>
                            <button type="button" class=BUTTON data-mcp-reconnect on:click=move |_| workspace.reconnect_mcp(&reconnected)>
                                "Reconnect"
                            </button>
                        </div>
                    </div>
                }
            })
            .collect_view()
            .into_any(),
    }
    };
    view! {
        {move || sign_in_outcome(workspace)}
        {move || sign_ins(workspace)}
        {list}
        <button
            type="button"
            id="mcp-new"
            class=PRIMARY
            on:click=move |_| {
                error.set(None);
                form.set(Some(McpEdit::add(None)));
            }
        >
            <Glyph icon=icondata::FaPlusSolid />
            "New server"
        </button>
        {move || form.get().map(|edit| {
            edit_dialog("mcp-form", move || form.set(None), mcp_form(workspace, form, persist, error, edit))
        })}
    }
}

/// What a server that needs authorization offers: signing in from the
/// page when the daemon takes the page as the redirect, else the commands
/// that can.
fn sign_in_offer(workspace: Workspace, server: &cookie_agent_protocol::McpServerInfo) -> AnyView {
    let name = server.name.clone();
    if !cookie_agent_client_state::mcp::is_global(server) {
        return view! {
            <span class="mcp-auth text-sm">
                "Only the daemon's own (global) MCP servers can sign in."
            </span>
        }
        .into_any();
    }
    if !workspace.can_sign_in.get_untracked() {
        return view! {
            <span class="mcp-auth text-sm">
                {format!(
                    "This server needs authorization. The daemon is too old to sign in from the page (that needs client protocol 1.11): run `cookie mcp auth {name}` on a machine with a browser, or authorize it from the TUI."
                )}
            </span>
        }
        .into_any();
    }
    let warns = browser::page_href().is_some_and(|page| crate::oauth::warns_about_http(&page));
    let waiting = workspace
        .sign_ins
        .get_untracked()
        .iter()
        .any(|(_, waiting)| *waiting == name);
    view! {
        <div class="mcp-auth flex flex-col items-start gap-2">
            <span class="text-sm">
                {if waiting {
                    "Waiting for you to finish signing in, above."
                } else {
                    "This server needs you to sign in."
                }}
            </span>
            {warns.then(|| view! {
                <p class=format!("{NOTE} flex items-start gap-1.5") data-https-warning>
                    <Glyph icon=icondata::FaTriangleExclamationSolid class="mt-0.5 shrink-0 text-caramel dark:text-maple" />
                    <span>
                        "This page is on plain http away from this machine, and most sign-in servers refuse to send you back to such an address. Put the daemon behind https, for example with "
                        <code>"tailscale serve"</code>
                        ", as the "
                        <a
                            href=format!("{}daemon/remote-access/", crate::machines::DOCS)
                            target="_blank"
                            rel="noopener noreferrer"
                            class="font-bold text-caramel underline-offset-2 hover:underline dark:text-maple"
                        >
                            "remote access guide"
                        </a>
                        " shows. Some servers allow it, so you can still try."
                    </span>
                </p>
            })}
            <button
                type="button"
                class=if waiting { BUTTON } else { PRIMARY }
                data-mcp-sign-in
                on:click=move |_| workspace.sign_in_mcp(name.clone())
            >
                <Glyph icon=icondata::FaRightToBracketSolid />
                {if waiting { "Open the sign-in again" } else { "Sign in" }}
            </button>
        </div>
    }
    .into_any()
}

/// How the last sign-in ended, above the servers.
fn sign_in_outcome(workspace: Workspace) -> impl IntoView {
    workspace.sign_in_outcome.get().map(|outcome| {
        let ok = outcome.is_ok();
        let text = match outcome {
            Ok(server) => view! { "Signed in to " <code class="font-mono font-bold">{server}</code> }.into_any(),
            Err(error) => error.into_any(),
        };
        view! {
            <div
                id="mcp-sign-in-outcome"
                role="status"
                data-outcome=if ok { "signed-in" } else { "failed" }
                class=if ok {
                    "flex items-start gap-2 rounded-md border border-caramel bg-parchment px-3 py-2 text-sm dark:border-maple dark:bg-char"
                } else {
                    ERROR
                }
            >
                {ok.then(|| view! { <Glyph icon=icondata::FaCircleCheckSolid class="mt-0.5 shrink-0 text-caramel dark:text-maple" /> })}
                <span class="min-w-0 flex-1 whitespace-pre-wrap break-words">{text}</span>
            </div>
        }
    })
}

/// The sign-ins this browser started that still wait: each can be
/// finished with the address the browser landed on, or cancelled.
fn sign_ins(workspace: Workspace) -> impl IntoView {
    let waiting = workspace.sign_ins.get();
    (!waiting.is_empty()).then(|| {
        let entries = waiting
            .into_iter()
            .map(|(state, server)| {
                let pasted = RwSignal::new(String::new());
                let finish = {
                    let (server, state) = (server.clone(), state.clone());
                    move || {
                        let address = pasted.get_untracked().trim().to_owned();
                        if !address.is_empty() {
                            workspace.finish_sign_in(server.clone(), address, Some(state.clone()));
                        }
                    }
                };
                let submit = finish.clone();
                let cancelled = (server.clone(), state.clone());
                view! {
                    <div class=format!("{CONFIRM} mb-2") data-sign-in=server.clone()>
                        <span class="flex items-center gap-2 text-sm font-bold">
                            <Glyph icon=icondata::FaArrowsRotateSolid class="animate-spin text-caramel dark:text-maple" />
                            {format!("Signing in to {server}")}
                        </span>
                        <span class=NOTE>
                            "Finish in the tab that opened. If the sign-in ends on a page that cannot continue here, such as another browser or a phone's browser, paste the address you landed on."
                        </span>
                        <label class="flex flex-col gap-1">
                            <span class="sr-only">"Paste the address you landed on"</span>
                            <input
                                type="url"
                                class=FIELD
                                data-sign-in-paste
                                placeholder="Paste the address you landed on"
                                autocomplete="off"
                                spellcheck="false"
                                prop:value=move || pasted.get()
                                on:input=move |event| pasted.set(event_target_value(&event))
                                on:keydown=move |event| {
                                    if event.key() == "Enter" {
                                        submit();
                                    }
                                }
                            />
                        </label>
                        <div class="flex flex-wrap gap-2">
                            <button
                                type="button"
                                class=PRIMARY
                                data-sign-in-finish
                                disabled=move || pasted.get().trim().is_empty()
                                on:click=move |_| finish()
                            >
                                "Finish sign-in"
                            </button>
                            <button
                                type="button"
                                class=BUTTON
                                data-sign-in-cancel
                                on:click=move |_| workspace.cancel_sign_in(cancelled.0.clone(), cancelled.1.clone())
                            >
                                "Cancel"
                            </button>
                        </div>
                    </div>
                }
            })
            .collect_view();
        view! { <section id="mcp-sign-ins" aria-label="Sign-ins in progress">{entries}</section> }
    })
}

/// An MCP server form open: its fields, the server it edits, and, for a
/// new one, the runtime it goes to (`global` when `None`).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct McpEdit {
    pub(crate) fields: McpFields,
    pub(crate) editing: Option<cookie_agent_protocol::McpServerInfo>,
    pub(crate) runtime: Option<cookie_agent_protocol::RuntimeId>,
}

impl McpEdit {
    pub(crate) fn add(runtime: Option<cookie_agent_protocol::RuntimeId>) -> Self {
        Self {
            fields: McpFields::add(),
            editing: None,
            runtime,
        }
    }

    pub(crate) fn edit(server: &cookie_agent_protocol::McpServerInfo) -> Self {
        Self {
            fields: McpFields::edit(server),
            editing: Some(server.clone()),
            runtime: Some(server.runtime_id.clone()),
        }
    }

    /// The runtime other than `global` the server is on, if it is.
    fn table_runtime(&self) -> Option<&cookie_agent_protocol::RuntimeId> {
        self.runtime
            .as_ref()
            .filter(|runtime| runtime.as_str() != cookie_agent_protocol::RuntimeId::GLOBAL)
    }
}

pub(crate) fn mcp_form(
    workspace: Workspace,
    form: RwSignal<Option<McpEdit>>,
    persist: RwSignal<PersistChoice>,
    error: RwSignal<Option<String>>,
    edit: McpEdit,
) -> impl IntoView {
    let McpEdit {
        fields,
        editing,
        runtime,
    } = edit.clone();
    let on_runtime = edit.table_runtime().cloned();
    let transport = RwSignal::new(fields.transport);
    let enabled = RwSignal::new(fields.enabled);
    let always_load = RwSignal::new(fields.always_load);
    let instructions = RwSignal::new(fields.instructions);
    let extras = RwSignal::new(fields.extras.clone());
    // One copy for the runtime, shared by its workspaces.
    let shared = RwSignal::new(
        editing
            .as_ref()
            .and_then(|server| server.definition.scope)
            .is_some_and(|scope| scope == cookie_agent_protocol::McpServerScope::Runtime),
    );
    let editing_name = editing.as_ref().map(|server| server.name.clone());
    let submit = move || {
        let mut fields = McpFields {
            name: field_value("mcp-name"),
            transport: fields.transport,
            endpoint: field_value("mcp-endpoint"),
            extras: field_value("mcp-extras"),
            environment: field_value("mcp-env"),
            cwd: field_value("mcp-cwd"),
            enabled: enabled.get_untracked(),
            include_tools: field_value("mcp-include-tools"),
            exclude_tools: field_value("mcp-exclude-tools"),
            always_load: always_load.get_untracked(),
            always_load_tools: field_value("mcp-always-load-tools"),
            instructions: instructions.get_untracked(),
        };
        fields.transport = transport.get_untracked();
        if fields.transport == McpTransport::Http {
            fields.environment = "{}".into();
        }
        let scope = Some(if shared.get_untracked() {
            cookie_agent_protocol::McpServerScope::Runtime
        } else {
            cookie_agent_protocol::McpServerScope::Workspace
        });
        match workspace.submit_mcp(
            fields,
            editing.clone(),
            persist.get_untracked(),
            runtime.clone(),
            scope,
        ) {
            Ok(()) => form.set(None),
            Err(message) => error.set(Some(message)),
        }
    };
    view! {
        <p class="text-sm font-bold">
            {match (editing_name.is_some(), &on_runtime) {
                (true, Some(runtime)) => format!("Edit MCP server on {runtime}"),
                (true, None) => "Edit MCP server".to_owned(),
                (false, Some(runtime)) => format!("New MCP server on {runtime}"),
                (false, None) => "New MCP server".to_owned(),
            }}
        </p>
        <label class="flex flex-col gap-1 text-sm">
            <span>"Name"</span>
            <input type="text" id="mcp-name" class=FIELD value=fields.name.clone() />
            {editing_name.is_some().then(|| view! { <span class=NOTE>"A new name renames the server."</span> })}
        </label>
        <label class="flex flex-col gap-1 text-sm">
            <span>"Transport"</span>
            <select
                id="mcp-transport"
                class=FIELD
                on:change=move |event| {
                    let next = if event_target_value(&event) == "http" { McpTransport::Http } else { McpTransport::Stdio };
                    if next != transport.get_untracked() {
                        transport.set(next);
                        extras.set(next.extras().1.to_owned());
                    }
                }
            >
                <option value="stdio" selected=fields.transport == McpTransport::Stdio>"stdio"</option>
                <option value="http" selected=fields.transport == McpTransport::Http>"http"</option>
            </select>
        </label>
        <label class="flex flex-col gap-1 text-sm">
            <span>{move || if transport.get() == McpTransport::Stdio { "Command" } else { "URL" }}</span>
            <input type="text" id="mcp-endpoint" class=FIELD value=fields.endpoint.clone() />
        </label>
        <label class="flex flex-col gap-1 text-sm">
            <span>{move || transport.get().extras().0}</span>
            <input type="text" id="mcp-extras" class=FIELD prop:value=move || extras.get() on:input=move |event| extras.set(event_target_value(&event)) />
        </label>
        <Show when=move || transport.get() == McpTransport::Stdio>
            <label class="flex flex-col gap-1 text-sm">
                <span>"Environment JSON object"</span>
                <input type="text" id="mcp-env" class=FIELD value=fields.environment.clone() />
            </label>
            <label class="flex flex-col gap-1 text-sm">
                <span>"Working directory"</span>
                <input type="text" id="mcp-cwd" class=FIELD value=fields.cwd.clone() />
            </label>
        </Show>
        <label class="flex flex-col gap-1 text-sm">
            <span>"Include tools"</span>
            <input type="text" id="mcp-include-tools" class=FIELD placeholder="every tool" value=fields.include_tools.clone() />
            <span class=NOTE>"Patterns over the server's tool names, comma-separated; * and ? match like permission patterns"</span>
        </label>
        <label class="flex flex-col gap-1 text-sm">
            <span>"Exclude tools"</span>
            <input type="text" id="mcp-exclude-tools" class=FIELD value=fields.exclude_tools.clone() />
        </label>
        <label class="flex flex-col gap-1 text-sm">
            <span>"Always load tools"</span>
            <input type="text" id="mcp-always-load-tools" class=FIELD value=fields.always_load_tools.clone() />
            <span class=NOTE>"Tools sent up front on models that load tools on demand, as patterns"</span>
        </label>
        <label class="flex min-h-11 items-center gap-2 text-sm">
            <input type="checkbox" id="mcp-always-load" class="h-5 w-5" prop:checked=move || always_load.get() on:change=move |_| always_load.update(|on| *on = !*on) />
            <span>"Always load every tool"</span>
        </label>
        <label class="flex min-h-11 items-center gap-2 text-sm">
            <input type="checkbox" id="mcp-instructions" class="h-5 w-5" prop:checked=move || instructions.get() on:change=move |_| instructions.update(|on| *on = !*on) />
            <span>"Show the server's instructions with its tools"</span>
        </label>
        <label class="flex min-h-11 items-center gap-2 text-sm">
            <input type="checkbox" id="mcp-enabled" class="h-5 w-5" prop:checked=move || enabled.get() on:change=move |_| enabled.update(|on| *on = !*on) />
            <span>"Enabled"</span>
        </label>
        {on_runtime.is_some().then(|| view! {
            <label class="flex min-h-11 items-center gap-2 text-sm">
                <input type="checkbox" id="mcp-shared" class="h-5 w-5" prop:checked=move || shared.get() on:change=move |_| shared.update(|on| *on = !*on) />
                <span>"One for the runtime, shared by its workspaces (a browser, say)"</span>
            </label>
        })}
        <label class="flex flex-col gap-1 text-sm" class:hidden=on_runtime.is_some()>
            <span>"Keep it"</span>
            <select
                id="mcp-persist"
                class=FIELD
                on:change=move |event| {
                    persist.set(if event_target_value(&event) == "user" { PersistChoice::User } else { PersistChoice::Runtime });
                }
            >
                <option value="runtime" selected=move || persist.get() == PersistChoice::Runtime>{PersistChoice::Runtime.label()}</option>
                <option value="user" selected=move || persist.get() == PersistChoice::User>{PersistChoice::User.label()}</option>
            </select>
        </label>
        {move || error.get().map(|message| view! { <p class=ERROR>{message}</p> })}
        <div class="flex gap-2">
            <button type="button" id="mcp-save" class=PRIMARY on:click=move |_| submit()>
                "Save"
            </button>
            <button type="button" class=BUTTON on:click=move |_| form.set(None)>
                "Cancel"
            </button>
        </div>
    }
}

// ---- /connect --------------------------------------------------------------

pub fn connect_sheet(workspace: Workspace) -> impl IntoView {
    let search = RwSignal::new(String::new());
    let open = RwSignal::new(None::<ProviderDescriptor>);
    let confirm_disconnect = RwSignal::new(false);
    view! {
        {move || match open.get() {
            Some(provider) => provider_view(workspace, provider, open, confirm_disconnect).into_any(),
            None => view! {
                <input
                    type="search"
                    id="provider-search"
                    class=FIELD
                    placeholder="Search providers"
                    prop:value=move || search.get()
                    on:input=move |event| search.set(event_target_value(&event))
                />
                {move || workspace
                    .with_runtime(|runtime| runtime.snapshot().map(catalog_status_line))
                    .map(|line| view! { <p class=NOTE>{line}</p> })}
                {move || {
                    let query = search.get().trim().to_lowercase();
                    let operations = workspace.provider_operations.get();
                    workspace
                        .providers()
                        .into_iter()
                        .filter(|provider| {
                            query.is_empty()
                                || provider.id.as_str().to_lowercase().contains(&query)
                                || provider.display_name.to_string().to_lowercase().contains(&query)
                        })
                        .map(|provider| {
                            let operation = operations.get(&provider.id).cloned();
                            let state = row_state(&provider, &[], operation.as_ref());
                            let detail = match (&operation, state) {
                                (Some(ProviderOperation::InProgress(action)), _) => {
                                    format!("{} in progress…", action_name(*action))
                                }
                                (Some(ProviderOperation::Error { message, .. }), _) => format!("error · {message}"),
                                (_, ProviderRowState::Unsupported) => "unsupported".into(),
                                (_, ProviderRowState::ConnectedReconnect) => "connected".into(),
                                (_, ProviderRowState::Configured) => "configured in config.toml".into(),
                                (_, ProviderRowState::Removed) => "removed from the current catalog".into(),
                                _ => "disconnected".into(),
                            };
                            let note = unusable_models_note(&provider);
                            let picked = provider.clone();
                            view! {
                                <button
                                    type="button"
                                    class=ROW
                                    data-provider=provider.id.to_string()
                                    on:click=move |_| {
                                        confirm_disconnect.set(false);
                                        open.set(Some(picked.clone()));
                                    }
                                >
                                    <span class="flex w-full items-center gap-2">
                                        <span class="min-w-0 flex-1 truncate text-sm font-bold">{format!("{} ({})", provider.display_name, provider.id)}</span>
                                        {matches!(state, ProviderRowState::ConnectedReconnect | ProviderRowState::Configured).then(|| view! {
                                            <Glyph icon=icondata::FaPlugSolid class="text-green-700 dark:text-green-400" />
                                        })}
                                    </span>
                                    <span class=NOTE>{detail}</span>
                                    {note.map(|note| view! { <span class=NOTE>{note}</span> })}
                                </button>
                            }
                        })
                        .collect_view()
                }}
            }
                .into_any(),
        }}
    }
}

/// A provider whose connection is a browser sign-in: the sign-in button,
/// then either "finish in the new tab" or the paste-back field.
fn sign_in_view(
    workspace: Workspace,
    provider: ProviderDescriptor,
    method: cookie_agent_protocol::SignInMethodDescriptor,
    open: RwSignal<Option<ProviderDescriptor>>,
    confirm_disconnect: RwSignal<bool>,
) -> impl IntoView {
    use crate::workspace::ProviderSignIn;

    let summary = model_counts_summary(&provider);
    let status = cookie_agent_client_state::providers::sign_in_status(&provider)
        .unwrap_or_else(|| "not signed in".to_owned());
    let connected = provider.durable_connection.is_some();
    let state = RwSignal::new(ProviderSignIn::Idle);
    let begin_provider = provider.clone();
    let finish_id = provider.id.clone();
    let cancel_id = provider.id.clone();
    let disconnect_provider = provider.clone();
    // A sign-in the daemon caught itself ends when the account shows up.
    let watched = provider.id.clone();
    let was_connected = provider
        .sign_in_account
        .as_ref()
        .filter(|account| !account.sign_in_required)
        .and_then(|account| account.email.clone());
    Effect::new(move |_| {
        let account = workspace.providers().into_iter().find_map(|listed| {
            (listed.id == watched)
                .then(|| listed.sign_in_account.clone())
                .flatten()
        });
        if matches!(
            state.get_untracked(),
            ProviderSignIn::Waiting {
                caught_by_daemon: true,
                ..
            }
        ) && account
            .is_some_and(|account| !account.sign_in_required && account.email != was_connected)
        {
            state.set(ProviderSignIn::Idle);
            open.set(None);
        }
    });
    view! {
        <p class="text-sm font-bold">{format!("{} ({})", provider.display_name, provider.id)}</p>
        <p class=NOTE>{summary}</p>
        <p class="text-sm" id="provider-sign-in-status">{status}</p>
        {move || match state.get() {
            ProviderSignIn::Idle => None,
            ProviderSignIn::Waiting { caught_by_daemon: true, authorization_url } => Some(view! {
                <p class=NOTE>"Finish signing in in the new tab. "
                    <a class="underline" href=authorization_url target="_blank" rel="noopener">"Open it again"</a>
                </p>
            }.into_any()),
            ProviderSignIn::Waiting { caught_by_daemon: false, authorization_url } => Some(view! {
                <p class=NOTE>"Sign in in the new tab. "
                    <a class="underline" href=authorization_url target="_blank" rel="noopener">"Open it again"</a>
                </p>
                <p class=NOTE>{cookie_agent_client_state::providers::SIGN_IN_PASTE_HINT}</p>
                <label class="flex flex-col gap-1 text-sm">
                    <span>"Address the tab could not load"</span>
                    <input type="text" id="provider-sign-in-callback" class=FIELD autocomplete="off" spellcheck="false" />
                </label>
                <button type="button" id="provider-sign-in-finish" class=PRIMARY on:click={
                    let provider_id = finish_id.clone();
                    move |_| {
                        let pasted = field_value("provider-sign-in-callback");
                        if !pasted.trim().is_empty() {
                            workspace.finish_provider_sign_in(provider_id.clone(), pasted.trim().to_owned(), state);
                        }
                    }
                }>"Finish sign-in"</button>
            }.into_any()),
            ProviderSignIn::Failed(message) => Some(view! {
                <p class=ERROR id="provider-error">{message}</p>
            }.into_any()),
        }}
        <div class="flex flex-wrap gap-2">
            <button type="button" id="provider-sign-in" class=PRIMARY on:click={
                let provider = begin_provider.clone();
                move |_| workspace.sign_in_provider(provider.clone(), state)
            }>
                <Glyph icon=icondata::FaPlugSolid />
                {if connected { format!("{} again", method.display_name) } else { method.display_name.to_string() }}
            </button>
            <button type="button" class=BUTTON on:click=move |_| {
                if matches!(state.get_untracked(), ProviderSignIn::Waiting { .. }) {
                    workspace.cancel_provider_sign_in(cancel_id.clone(), state);
                }
                open.set(None);
            }>"Cancel"</button>
            <Show when=move || connected>
                <button type="button" id="provider-disconnect" class=BUTTON on:click=move |_| confirm_disconnect.set(true)>
                    "Sign out"
                </button>
            </Show>
        </div>
        <Show when=move || confirm_disconnect.get()>
            <div class=CONFIRM>
                <p class="text-sm">"Sign out and revoke this sign-in?"</p>
                <div class="flex gap-2">
                    <button type="button" id="provider-disconnect-confirm" class=PRIMARY on:click={
                        let provider = disconnect_provider.clone();
                        move |_| {
                            confirm_disconnect.set(false);
                            open.set(None);
                            workspace.disconnect_provider(provider.clone());
                        }
                    }>
                        "Sign out"
                    </button>
                    <button type="button" class=BUTTON on:click=move |_| confirm_disconnect.set(false)>"Keep it"</button>
                </div>
            </div>
        </Show>
    }
}

/// A provider's connect form, or its details when it cannot be connected.
/// Credentials stay in their password fields: they are read once when the
/// form is sent and never kept in the page's state.
fn provider_view(
    workspace: Workspace,
    provider: ProviderDescriptor,
    open: RwSignal<Option<ProviderDescriptor>>,
    confirm_disconnect: RwSignal<bool>,
) -> impl IntoView {
    let state = row_state(&provider, &[], None);
    let summary = model_counts_summary(&provider);
    let back = move |_| open.set(None);
    if state == ProviderRowState::Unsupported {
        let reason = provider
            .support
            .reason
            .as_ref()
            .map_or_else(|| "unsupported".to_owned(), ToString::to_string);
        return view! {
            <p class="text-sm font-bold">{format!("{} ({})", provider.display_name, provider.id)}</p>
            <p class="text-sm">{reason}</p>
            <p class=NOTE>{summary}</p>
            <button type="button" class=BUTTON on:click=back>"Back"</button>
        }
        .into_any();
    }
    if let Some(method) = cookie_agent_client_state::providers::sign_in_method(&provider).cloned() {
        return sign_in_view(workspace, provider, method, open, confirm_disconnect).into_any();
    }
    let Some(initial) = initial_auth_method(&provider).cloned() else {
        return view! {
            <p class="text-sm">"This provider has no store-backed authentication form."</p>
            <button type="button" class=BUTTON on:click=back>"Back"</button>
        }
        .into_any();
    };
    let method = RwSignal::new(initial.id.clone());
    let error = RwSignal::new(None::<String>);
    let submitted = RwSignal::new(false);
    let reconnect = provider.durable_connection.is_some();
    // Credentials from config.toml win over any stored connection: the form
    // only stores one for when they are removed.
    let configured = (state == ProviderRowState::Configured)
        .then(|| cookie_agent_client_state::providers::configured_in_config_note(&provider));
    let provider_id = provider.id.clone();
    // A connect that finished without an error closes the form.
    Effect::new(move |_| {
        let operation = workspace
            .provider_operations
            .with(|operations| operations.get(&provider_id).cloned());
        if !submitted.get_untracked() {
            return;
        }
        match operation {
            None => {
                submitted.set(false);
                open.set(None);
            }
            Some(ProviderOperation::Error { message, .. }) => {
                submitted.set(false);
                error.set(Some(message));
            }
            Some(ProviderOperation::InProgress(_)) => {}
        }
    });
    let methods = provider.auth_methods.clone();
    let credential_methods = provider.auth_methods.clone();
    let setup_fields = provider.setup_fields.clone();
    let form_provider = provider.clone();
    let submit = move || {
        let provider = form_provider.clone();
        let Some(auth) = provider
            .auth_methods
            .iter()
            .find(|candidate| candidate.id == method.get_untracked())
            .cloned()
        else {
            return;
        };
        let typed_setup = provider
            .setup_fields
            .iter()
            .enumerate()
            .map(|(index, field)| (field, field_value(&format!("setup-{index}"))))
            .collect::<Vec<_>>();
        let setup = match setup_values(
            &typed_setup
                .iter()
                .map(|(field, value)| (*field, value.as_str()))
                .collect::<Vec<_>>(),
        ) {
            Ok(values) => values,
            Err(message) => {
                error.set(Some(format!("Invalid public setup: {message}")));
                return;
            }
        };
        let typed = auth
            .credentials
            .iter()
            .enumerate()
            .map(|(index, field)| (field, field_value(&format!("credential-{index}"))))
            .collect::<Vec<_>>();
        let credentials = credential_values(
            &typed
                .iter()
                .map(|(field, value)| (*field, value.as_str()))
                .collect::<Vec<_>>(),
        );
        drop(typed);
        let credentials = match credentials {
            Ok(values) => values,
            Err(message) => {
                error.set(Some(format!("Invalid credentials: {message}")));
                return;
            }
        };
        error.set(None);
        match workspace.connect_provider(provider, auth.id.clone(), setup, credentials, reconnect) {
            Ok(()) => submitted.set(true),
            Err(message) => error.set(Some(message)),
        }
    };
    let disconnect_provider = provider.clone();
    let stores_only = configured.is_some();
    view! {
        <p class="text-sm font-bold">{format!("{} ({})", provider.display_name, provider.id)}</p>
        <p class=NOTE>{summary}</p>
        {configured.map(|note| view! { <p class="text-sm" id="provider-configured">{note}</p> })}
        <p class=NOTE>{cookie_agent_client_state::providers::DURABLE_PROVIDER_COPY}</p>
        <Show when={
            let count = methods.len();
            move || count > 1
        }>
            <label class="flex flex-col gap-1 text-sm">
                <span>"Authentication"</span>
                <select
                    id="provider-auth-method"
                    class=FIELD
                    on:change=move |event| {
                        if let Ok(id) = cookie_agent_protocol::AuthMethodId::new(event_target_value(&event)) {
                            method.set(id);
                        }
                    }
                >
                    {methods
                        .iter()
                        .map(|candidate| {
                            let id = candidate.id.clone();
                            view! {
                                <option value=candidate.id.to_string() selected=move || method.get() == id>
                                    {candidate.display_name.to_string()}
                                </option>
                            }
                        })
                        .collect_view()}
                </select>
            </label>
        </Show>
        {move || {
            let current = method.get();
            credential_methods
                .iter()
                .find(|candidate| candidate.id == current)
                .map(|auth| {
                    auth.credentials
                        .iter()
                        .enumerate()
                        .map(|(index, field)| view! {
                            <label class="flex flex-col gap-1 text-sm">
                                <span>{format!("{}{}", field.display_name, if field.required { "" } else { " (optional)" })}</span>
                                <input
                                    type="password"
                                    id=format!("credential-{index}")
                                    class=FIELD
                                    autocomplete="off"
                                    spellcheck="false"
                                />
                                <span class=NOTE>{field.help.to_string()}</span>
                            </label>
                        })
                        .collect_view()
                })
        }}
        {setup_fields
            .iter()
            .enumerate()
            .map(|(index, field)| view! {
                <label class="flex flex-col gap-1 text-sm">
                    <span>{format!("{}{}", field.display_name, if field.required { "" } else { " (optional)" })}</span>
                    <input
                        type="text"
                        id=format!("setup-{index}")
                        class=FIELD
                        value=initial_setup_text(&disconnect_provider, field)
                    />
                    <span class=NOTE>{field.help.to_string()}</span>
                </label>
            })
            .collect_view()}
        {move || error.get().map(|message| view! { <p class=ERROR id="provider-error">{message}</p> })}
        <div class="flex flex-wrap gap-2">
            <button type="button" id="provider-connect" class=PRIMARY on:click=move |_| submit()>
                <Glyph icon=icondata::FaPlugSolid />
                {if stores_only {
                    "Store a connection"
                } else if reconnect {
                    "Reconnect"
                } else {
                    "Connect"
                }}
            </button>
            <button type="button" class=BUTTON on:click=back>"Cancel"</button>
            <Show when=move || reconnect>
                <button type="button" id="provider-disconnect" class=BUTTON on:click=move |_| confirm_disconnect.set(true)>
                    "Disconnect"
                </button>
            </Show>
        </div>
        <Show when=move || confirm_disconnect.get()>
            <div class=CONFIRM>
                <p class="text-sm">"Remove the stored connection and its credentials?"</p>
                <div class="flex gap-2">
                    <button type="button" id="provider-disconnect-confirm" class=PRIMARY on:click={
                        let provider = provider.clone();
                        move |_| {
                            confirm_disconnect.set(false);
                            open.set(None);
                            workspace.disconnect_provider(provider.clone());
                        }
                    }>
                        "Disconnect"
                    </button>
                    <button type="button" class=BUTTON on:click=move |_| confirm_disconnect.set(false)>"Keep it"</button>
                </div>
            </div>
        </Show>
    }
    .into_any()
}

// ---- /config ---------------------------------------------------------------

/// `config.toml`: the Settings view's Configuration section. The agent
/// documents are the Agents view ([`super::agents`]).
pub fn config_sheet(workspace: Workspace) -> impl IntoView {
    move || {
        if let Some(edit) = workspace
            .config_edit
            .get()
            .filter(|edit| edit.path == "config.toml")
        {
            let path = edit.path.clone();
            return view! {
                <div class=EDITOR_PAGE>
                <p class="font-mono text-sm font-bold" id="config-path">{path}</p>
                {move || workspace.config_error.get().map(|error| view! { <p class=ERROR id="config-error">{error}</p> })}
                <textarea id="config-text" class=EDITOR spellcheck="false">{edit.text.clone()}</textarea>
                <div class=EDITOR_ACTIONS>
                    <button
                        type="button"
                        id="config-apply"
                        class=PRIMARY
                        on:click=move |_| workspace.apply_config(area_value("config-text"))
                    >
                        "Apply"
                    </button>
                    <button type="button" class=BUTTON on:click=move |_| workspace.config_edit.set(None)>
                        "Cancel"
                    </button>
                </div>
                </div>
            }
            .into_any();
        }
        let config = match workspace.config.get() {
            None => {
                return loading("Reading the configuration…").into_any();
            }
            Some(Err(error)) => return view! { <p class=ERROR>{error}</p> }.into_any(),
            Some(Ok(config)) => config,
        };
        let draft = workspace
            .config_drafts
            .with(|drafts| drafts.contains_key("config.toml"));
        let label = config_rows(&config)
            .into_iter()
            .find(|row| row.path.as_deref() == Some("config.toml"))
            .map(|row| row.label)
            .unwrap_or_default();
        view! {
            {move || workspace.config_error.get().map(|error| view! { <p class=ERROR id="config-error">{error}</p> })}
            {config.status.error.clone().map(|error| view! { <p class=ERROR>{format!("The running configuration has an error: {error}")}</p> })}
            {(!config.status.diagnostics.is_empty() || !config.status.native_compaction.is_empty() || !config.status.deferred_tools.is_empty()).then(|| view! {
                <ul class="flex flex-col gap-1 text-sm text-caramel dark:text-maple" id="config-diagnostics">
                    {config.status.diagnostics.iter().cloned().chain(cookie_agent_client_state::config::record_lines(&config.status)).map(|line| view! { <li>{line}</li> }).collect_view()}
                </ul>
            })}
            <div class="flex items-center gap-2" data-config-path="config.toml">
                <button type="button" class=format!("{ROW} min-w-0 flex-1") on:click=move |_| workspace.edit_config("config.toml")>
                    <span class="flex items-center gap-2 font-mono text-sm whitespace-pre">
                        <Glyph icon=icondata::FaFileLinesRegular class="opacity-70" />
                        {label}
                    </span>
                    {draft.then(|| view! { <span class="text-xs text-caramel dark:text-maple">"not applied"</span> })}
                </button>
            </div>
            <div class="mt-3 flex flex-col gap-2 border-t border-toasted pt-3 dark:border-ember">
                <div class="flex flex-wrap gap-2">
                    <button type="button" id="config-reload" class=BUTTON on:click=move |_| workspace.reload_config()>
                        <Glyph icon=icondata::FaArrowsRotateSolid />
                        "Reload from disk"
                    </button>
                    // Beside the reload, so the parts it cannot apply are
                    // applied where the configuration was changed; it
                    // stands out while some wait for a restart.
                    {move || workspace.can_restart.get().then(|| {
                        let needed = move || workspace.config_status.with(|status| {
                            status.as_ref().is_some_and(|status| !status.restart_required.is_empty())
                        });
                        view! {
                            <button
                                type="button"
                                id="restart-daemon"
                                class=move || if needed() { PRIMARY } else { BUTTON }
                                data-restart-needed=move || needed().then_some("")
                                on:click=move |_| workspace.ask_restart()
                            >
                                <Glyph icon=icondata::FaPowerOffSolid />
                                "Restart daemon"
                            </button>
                        }
                    })}
                </div>
                {move || workspace.can_restart.get().then(|| view! {
                    <p class=NOTE>
                        {move || {
                            let parts = workspace.config_status.with(|status| {
                                status.as_ref().map(cookie_agent_client_state::restart::restart_parts).unwrap_or_default()
                            });
                            if parts.is_empty() {
                                "Restarting relaunches the daemon with the same arguments; clients reconnect on their own.".to_owned()
                            } else {
                                format!("Waiting for a restart: {parts}.")
                            }
                        }}
                    </p>
                })}
            </div>
        }
        .into_any()
    }
}

// ---- /plugins --------------------------------------------------------------

pub fn plugins_sheet(workspace: Workspace) -> impl IntoView {
    move || {
        match workspace.plugins.get() {
        None => loading("Reading plugins…").into_any(),
        Some(Err(error)) => view! { <p class=ERROR>{error}</p> }.into_any(),
        Some(Ok(plugins)) if plugins.is_empty() => empty_state(
            icondata::FaPuzzlePieceSolid,
            "No plugins are configured. Add one under [plugins.<name>] in the config.",
        )
        .into_any(),
        Some(Ok(plugins)) => plugins
            .into_iter()
            .map(|plugin| {
                let name = plugin.name.clone();
                view! {
                    <div class="plugin flex flex-col gap-1 border-b border-toasted py-2 dark:border-ember" data-plugin=plugin.name.clone()>
                        <div class="flex items-center gap-2">
                            <span class="min-w-0 flex-1 truncate font-mono text-sm font-bold">{plugin.name.clone()}</span>
                            <span class=format!("plugin-state {STATE}")>{plugin_state_label(plugin.state)}</span>
                        </div>
                        <pre class=format!("{NOTE} whitespace-pre-wrap font-mono")>{plugin_detail(&plugin)}</pre>
                        <button type="button" class=format!("{BUTTON} self-start") data-plugin-restart on:click=move |_| workspace.restart_plugin(name.clone())>
                            <Glyph icon=icondata::FaArrowsRotateSolid />
                            "Restart"
                        </button>
                    </div>
                }
            })
            .collect_view()
            .into_any(),
    }
    }
}
