//! The management sheets: `/runtime` with its machines, `/mcp`,
//! `/connect`, `/config`, `/memory`, and `/plugins`.

use cookie_agent_client_state::{
    config::config_rows,
    mcp::{
        McpFields, McpTransport, PersistChoice, definition_display, runtime_label, source_label,
        state_label as mcp_state_label,
    },
    memory::{MemoryRow, layer_title, memory_name, memory_rows},
    plugins::{plugin_detail, state_label as plugin_state_label},
    providers::{
        ProviderOperation, ProviderRowState, action_name, catalog_status_line, credential_values,
        initial_auth_method, initial_setup_text, model_counts_summary, row_state, setup_values,
        unusable_models_note,
    },
    runtimes::{
        MachineAction, MachineEntry, RuntimeChoice, attach_parameters, machine_action_request,
        machine_entries, machine_form_title, machine_name_request, runtime_choices, status_label,
    },
};
use cookie_agent_protocol::{McpServerState, ProviderDescriptor, RuntimeInstanceStatus};
use leptos::{ev, prelude::*};
use wasm_bindgen::JsCast as _;

use super::{BUTTON, ERROR, ICON_BUTTON, NOTE, PRIMARY, ROW, empty_state, loading};
use crate::{
    icons::Glyph,
    workspace::{MemoryChoice, MemoryPending, PANEL_POLL, Workspace},
};

const FIELD: &str = "min-h-11 w-full rounded-md border border-crust bg-cream px-3 font-mono text-sm hover:border-caramel focus:border-caramel focus:outline-none dark:border-smoke dark:bg-night dark:hover:border-maple dark:focus:border-maple";
const EDITOR: &str = "min-h-[40dvh] w-full resize-y rounded-md border border-crust bg-cream p-2 font-mono text-sm hover:border-caramel focus:border-caramel focus:outline-none dark:border-smoke dark:bg-night dark:hover:border-maple dark:focus:border-maple";
const TAB: &str = "min-h-11 flex-1 rounded-md px-3 text-sm hover:bg-toasted active:bg-crust dark:hover:bg-ember dark:active:bg-smoke";
/// A small label for a thing's state.
const STATE: &str = "shrink-0 rounded-full bg-toasted px-2 py-0.5 text-xs dark:bg-ember";
/// A confirmation before something cannot be undone.
const CONFIRM: &str = "flex flex-col gap-2 rounded-lg border border-caramel bg-parchment p-3 dark:border-maple dark:bg-char";

/// Reads `poll` every second while the sheet is open, as the TUI polls its
/// open `/runtime` and `/mcp` panels.
fn poll_while_open(poll: impl Fn() + 'static) {
    let ticker = set_interval_with_handle(poll, PANEL_POLL).ok();
    on_cleanup(move || {
        if let Some(ticker) = ticker {
            ticker.clear();
        }
    });
}

/// The value of the text field `id`, read once.
fn field_value(id: &str) -> String {
    web_sys::window()
        .and_then(|window| window.document())
        .and_then(|document| document.get_element_by_id(id))
        .and_then(|element| element.dyn_into::<web_sys::HtmlInputElement>().ok())
        .map(|input| input.value())
        .unwrap_or_default()
}

// ---- /runtime --------------------------------------------------------------

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuntimeTab {
    Attachments,
    Machines,
}

pub fn runtimes_sheet(workspace: Workspace) -> impl IntoView {
    poll_while_open(move || workspace.poll_runtimes());
    let tab = RwSignal::new(RuntimeTab::Attachments);
    let tab_button = move |which: RuntimeTab, label: &'static str, id: &'static str| {
        view! {
            <button
                type="button"
                id=id
                class=TAB
                class:font-bold=move || tab.get() == which
                class:bg-cream=move || tab.get() == which
                class:shadow-sm=move || tab.get() == which
                class:dark:bg-night=move || tab.get() == which
                aria-pressed=move || (tab.get() == which).to_string()
                on:click=move |_| tab.set(which)
            >
                {label}
            </button>
        }
    };
    view! {
        <div class="flex gap-1 rounded-lg bg-parchment p-1 dark:bg-char" role="group" aria-label="Runtimes or machines">
            {tab_button(RuntimeTab::Attachments, "Attached runtimes", "runtime-tab-attachments")}
            {tab_button(RuntimeTab::Machines, "Machines", "runtime-tab-machines")}
        </div>
        {move || match tab.get() {
            RuntimeTab::Attachments => attachments_view(workspace).into_any(),
            RuntimeTab::Machines => machines_view(workspace).into_any(),
        }}
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
            .enumerate()
            .map(|(index, row)| {
                let status = row.instance.as_ref().map(|instance| instance.status);
                let message = row
                    .instance
                    .as_ref()
                    .and_then(|instance| instance.message.clone());
                let failed = status == Some(RuntimeInstanceStatus::Failed);
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
                        <span class=NOTE>
                            {format!(
                                "{}/{}{}",
                                row.attachment.runtime_id,
                                row.attachment.capability,
                                row.attachment
                                    .workspace
                                    .as_ref()
                                    .map(|workspace| format!(" at {workspace}"))
                                    .unwrap_or_default()
                            )}
                        </span>
                        {row.description.clone().map(|description| view! { <span class=NOTE>{description}</span> })}
                        {message.map(|message| view! { <span class=ERROR>{message}</span> })}
                        <div class="flex gap-2">
                            <button type="button" class=BUTTON data-retry on:click=move |_| workspace.retry_runtime(retry_row.clone(), index)>
                                <Glyph icon=icondata::FaArrowsRotateSolid />
                                "Retry"
                            </button>
                            <button type="button" class=BUTTON data-detach on:click=move |_| workspace.detach_runtime(detach_name.clone(), index)>
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
        <Show when=move || workspace.new_session.get()>
            <p class=NOTE>"The runtimes the new session will attach; each starts now so its status shows."</p>
        </Show>
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

fn machines_view(workspace: Workspace) -> impl IntoView {
    let form = RwSignal::new(None::<(MachineAction, MachineEntry)>);
    let act = move |entry: MachineEntry, action: MachineAction| match machine_action_request(
        &entry, action,
    ) {
        Ok(Some(request)) => workspace.change_machines(request),
        Ok(None) => form.set(Some((action, entry))),
        Err(error) => workspace.status.set(error),
    };
    move || {
        let listing = workspace.machines.get();
        let machines = match &listing {
            None => return loading("Reading machines…").into_any(),
            Some(Err(error)) => return view! { <p class=ERROR>{error.clone()}</p> }.into_any(),
            Some(Ok(listing)) => listing.clone(),
        };
        let rows = machine_entries(&machines)
            .into_iter()
            .map(|entry| {
                let (label, detail, indent) = match &entry {
                    MachineEntry::Machine(name) => {
                        let machine = machines.machines.iter().find(|machine| machine.name == *name);
                        let platform = machine
                            .and_then(|machine| machine.platform.clone())
                            .map(|platform| format!(" ({platform})"))
                            .unwrap_or_default();
                        let memories = machine.map_or(0, |machine| machine.memories);
                        (
                            format!("{name}{platform}"),
                            if memories == 1 { "1 memory".to_owned() } else { format!("{memories} memories") },
                            false,
                        )
                    }
                    MachineEntry::Runtime { runtime, assigned_by, pinned, .. } => (
                        runtime.to_string(),
                        match (assigned_by, pinned) {
                            (Some(_), true) => "connected · pinned".to_owned(),
                            (Some(_), false) => "connected".to_owned(),
                            (None, _) => "pinned · not connected".to_owned(),
                        },
                        true,
                    ),
                };
                let actions = entry
                    .actions()
                    .into_iter()
                    .map(|action| {
                        let entry = entry.clone();
                        view! {
                            <button
                                type="button"
                                class=BUTTON
                                data-machine-action=action.label()
                                on:click=move |_| act(entry.clone(), action)
                            >
                                {action.label()}
                            </button>
                        }
                    })
                    .collect_view();
                view! {
                    <div class="machine-row flex flex-col gap-1 border-b border-toasted py-2 dark:border-ember" class:pl-6=indent data-machine-entry=label.clone()>
                        <span class="font-mono text-sm" class:font-bold=!indent>{label.clone()}</span>
                        <span class=NOTE>{detail}</span>
                        <div class="flex flex-wrap gap-2">{actions}</div>
                    </div>
                }
            })
            .collect_view();
        view! {
            {rows}
            {move || {
                form.get()
                    .map(|(action, entry)| {
                        let title = machine_form_title(action, &entry);
                        let submit = StoredValue::new_local(move || {
                            match machine_name_request(action, &entry, &field_value("machine-name")) {
                                Ok(request) => {
                                    form.set(None);
                                    workspace.change_machines(request);
                                }
                                Err(error) => workspace.status.set(error),
                            }
                        });
                        let submit = move || submit.with_value(|submit| submit());
                        view! {
                            <div class="mt-2 flex flex-col gap-2 border-t border-toasted pt-2 dark:border-ember">
                                <label class="flex flex-col gap-1 text-sm">
                                    <span>{title}</span>
                                    <input
                                        type="text"
                                        id="machine-name"
                                        class=FIELD
                                        on:keydown=move |event: ev::KeyboardEvent| {
                                            if event.key() == "Enter" {
                                                event.prevent_default();
                                                submit();
                                            }
                                        }
                                    />
                                </label>
                                <div class="flex gap-2">
                                    <button type="button" id="machine-apply" class=PRIMARY on:click=move |_| submit()>
                                        "Apply"
                                    </button>
                                    <button type="button" class=BUTTON on:click=move |_| form.set(None)>
                                        "Cancel"
                                    </button>
                                </div>
                            </div>
                        }
                    })
            }}
        }
        .into_any()
    }
}

// ---- /mcp ------------------------------------------------------------------

pub fn mcp_sheet(workspace: Workspace) -> impl IntoView {
    poll_while_open(move || workspace.poll_mcp());
    // The form: its fields, the server it edits, and where it is kept.
    let form = RwSignal::new(None::<(McpFields, Option<cookie_agent_protocol::McpServerInfo>)>);
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
                let removed = server.name.clone();
                let toggled = (server.name.clone(), !server.definition.enabled);
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
                                "{} · {} · {} tools",
                                runtime_label(&server),
                                source_label(server.source),
                                server.tool_count
                            )}
                        </span>
                        <span class=format!("{NOTE} font-mono whitespace-pre-wrap break-all")>{definition_display(&server.definition)}</span>
                        {server.message.clone().map(|message| view! { <span class=ERROR>{message}</span> })}
                        {needs_auth.then(|| view! {
                            <span class="mcp-auth text-sm">
                                {format!(
                                    "This server needs authorization, which the page cannot do: run `cookie mcp auth {name}` on a machine with a browser, or authorize it from the TUI."
                                )}
                            </span>
                        })}
                        {refusal.clone().map(|refusal| view! { <span class=NOTE>{refusal}</span> })}
                        <div class="flex flex-wrap gap-2">
                            <Show when=move || editable>
                                <button type="button" class=BUTTON data-mcp-edit on:click={
                                    let edited = edited.clone();
                                    move |_| {
                                        error.set(None);
                                        form.set(Some((McpFields::edit(&edited), Some(edited.clone()))));
                                    }
                                }>
                                    "Edit"
                                </button>
                                <button type="button" class=BUTTON data-mcp-toggle on:click={
                                    let toggled = toggled.clone();
                                    move |_| workspace.toggle_mcp(toggled.0.clone(), toggled.1)
                                }>
                                    {if toggled.1 { "Enable" } else { "Disable" }}
                                </button>
                                <button type="button" class=BUTTON data-mcp-remove on:click={
                                    let removed = removed.clone();
                                    move |_| workspace.remove_mcp(removed.clone())
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
        {move || match form.get() {
            None => view! {
                {list}
                <button
                    type="button"
                    id="mcp-new"
                    class=PRIMARY
                    on:click=move |_| {
                        error.set(None);
                        form.set(Some((McpFields::add(), None)));
                    }
                >
                    <Glyph icon=icondata::FaPlusSolid />
                    "New server"
                </button>
            }
                .into_any(),
            Some((fields, editing)) => mcp_form(workspace, form, persist, error, fields, editing).into_any(),
        }}
    }
}

fn mcp_form(
    workspace: Workspace,
    form: RwSignal<Option<(McpFields, Option<cookie_agent_protocol::McpServerInfo>)>>,
    persist: RwSignal<PersistChoice>,
    error: RwSignal<Option<String>>,
    fields: McpFields,
    editing: Option<cookie_agent_protocol::McpServerInfo>,
) -> impl IntoView {
    let transport = RwSignal::new(fields.transport);
    let enabled = RwSignal::new(fields.enabled);
    let extras = RwSignal::new(fields.extras.clone());
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
        };
        fields.transport = transport.get_untracked();
        if fields.transport == McpTransport::Http {
            fields.environment = "{}".into();
        }
        match workspace.submit_mcp(fields, editing.clone(), persist.get_untracked()) {
            Ok(()) => form.set(None),
            Err(message) => error.set(Some(message)),
        }
    };
    view! {
        <p class="text-sm font-bold">{if editing_name.is_some() { "Edit MCP server" } else { "New MCP server" }}</p>
        <label class="flex flex-col gap-1 text-sm">
            <span>"Name"</span>
            <input type="text" id="mcp-name" class=FIELD value=fields.name.clone() readonly=editing_name.is_some() />
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
        <label class="flex min-h-11 items-center gap-2 text-sm">
            <input type="checkbox" id="mcp-enabled" class="h-5 w-5" prop:checked=move || enabled.get() on:change=move |_| enabled.update(|on| *on = !*on) />
            <span>"Enabled"</span>
        </label>
        <label class="flex flex-col gap-1 text-sm">
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
                                        {(state == ProviderRowState::ConnectedReconnect).then(|| view! {
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
    view! {
        <p class="text-sm font-bold">{format!("{} ({})", provider.display_name, provider.id)}</p>
        <p class=NOTE>{summary}</p>
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
                {if reconnect { "Reconnect" } else { "Connect" }}
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

pub fn config_sheet(workspace: Workspace) -> impl IntoView {
    let confirm_delete = RwSignal::new(None::<String>);
    move || {
        if let Some(edit) = workspace.config_edit.get() {
            let path = edit.path.clone();
            return view! {
                <p class="font-mono text-sm font-bold" id="config-path">{path}</p>
                {move || workspace.config_error.get().map(|error| view! { <p class=ERROR id="config-error">{error}</p> })}
                <textarea id="config-text" class=EDITOR spellcheck="false">{edit.text.clone()}</textarea>
                <div class="flex gap-2">
                    <button
                        type="button"
                        id="config-apply"
                        class=PRIMARY
                        on:click=move |_| {
                            let text = web_sys::window()
                                .and_then(|window| window.document())
                                .and_then(|document| document.get_element_by_id("config-text"))
                                .and_then(|element| element.dyn_into::<web_sys::HtmlTextAreaElement>().ok())
                                .map(|area| area.value())
                                .unwrap_or_default();
                            workspace.apply_config(text);
                        }
                    >
                        "Apply"
                    </button>
                    <button type="button" class=BUTTON on:click=move |_| workspace.config_edit.set(None)>
                        "Close"
                    </button>
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
        let drafts = workspace.config_drafts.get();
        let rows = config_rows(&config)
            .into_iter()
            .map(|row| match row.path.clone() {
                None => view! { <h3 class="mt-3 px-3 text-xs font-bold tracking-wide text-ink uppercase dark:text-sand">{row.label}</h3> }.into_any(),
                Some(path) => {
                    let draft = drafts.contains_key(&path);
                    let opened = path.clone();
                    let deleted = path.clone();
                    let deletable = path != "config.toml";
                    view! {
                        <div class="flex items-center gap-2" data-config-path=path.clone()>
                            <button type="button" class=format!("{ROW} min-w-0 flex-1") on:click=move |_| workspace.edit_config(&opened)>
                                <span class="flex items-center gap-2 font-mono text-sm whitespace-pre">
                                    <Glyph icon=icondata::FaFileLinesRegular class="opacity-70" />
                                    {row.label.clone()}
                                </span>
                                {draft.then(|| view! { <span class="text-xs text-caramel dark:text-maple">"not applied"</span> })}
                            </button>
                            <Show when=move || deletable>
                                <button
                                    type="button"
                                    class=ICON_BUTTON
                                    aria-label="Delete"
                                    title="Delete"
                                    data-config-delete
                                    on:click={
                                        let deleted = deleted.clone();
                                        move |_| confirm_delete.set(Some(deleted.clone()))
                                    }
                                >
                                    <Glyph icon=icondata::FaTrashCanRegular />
                                </button>
                            </Show>
                        </div>
                    }
                    .into_any()
                }
            })
            .collect_view();
        view! {
            {move || workspace.config_error.get().map(|error| view! { <p class=ERROR id="config-error">{error}</p> })}
            {config.status.error.clone().map(|error| view! { <p class=ERROR>{format!("The running configuration has an error: {error}")}</p> })}
            {rows}
            {move || confirm_delete.get().map(|path| {
                let deleted = path.clone();
                view! {
                    <div class=CONFIRM>
                        <p class="text-sm">{format!("Delete {path}?")}</p>
                        <div class="flex gap-2">
                            <button type="button" id="config-delete-confirm" class=PRIMARY on:click=move |_| {
                                confirm_delete.set(None);
                                workspace.delete_config(deleted.clone());
                            }>
                                "Delete"
                            </button>
                            <button type="button" class=BUTTON on:click=move |_| {
                                confirm_delete.set(None);
                                workspace.status.set("Delete cancelled.".into());
                            }>
                                "Keep it"
                            </button>
                        </div>
                    </div>
                }
            })}
            <div class="mt-3 flex flex-col gap-2 border-t border-toasted pt-3 dark:border-ember">
                <label class="flex flex-col gap-1 text-sm">
                    <span class="font-bold">"New agent"</span>
                    <input type="text" id="config-new-path" class=FIELD value="agents/" />
                </label>
                <div class="flex flex-wrap gap-2">
                    <button type="button" id="config-new" class=PRIMARY on:click=move |_| workspace.new_agent(&field_value("config-new-path"))>
                        <Glyph icon=icondata::FaPlusSolid />
                        "Create"
                    </button>
                    <button type="button" id="config-reload" class=BUTTON on:click=move |_| workspace.reload_config()>
                        <Glyph icon=icondata::FaArrowsRotateSolid />
                        "Reload from disk"
                    </button>
                </div>
            </div>
        }
        .into_any()
    }
}

// ---- /memory ---------------------------------------------------------------

pub fn memory_sheet(workspace: Workspace) -> impl IntoView {
    let confirm_delete = RwSignal::new(None::<(String, String)>);
    let new_url = RwSignal::new("project://".to_owned());
    move || {
        if let Some(edit) = workspace.memory_edit.get() {
            return view! {
                <p class="font-mono text-sm font-bold" id="memory-url">{edit.url.clone()}</p>
                <textarea id="memory-text" class=EDITOR spellcheck="false">{edit.text.clone()}</textarea>
                <div class="flex gap-2">
                    <button
                        type="button"
                        id="memory-save"
                        class=PRIMARY
                        on:click=move |_| {
                            let text = web_sys::window()
                                .and_then(|window| window.document())
                                .and_then(|document| document.get_element_by_id("memory-text"))
                                .and_then(|element| element.dyn_into::<web_sys::HtmlTextAreaElement>().ok())
                                .map(|area| area.value())
                                .unwrap_or_default();
                            workspace.save_memory(text);
                        }
                    >
                        "Save"
                    </button>
                    <button type="button" class=BUTTON on:click=move |_| workspace.memory_edit.set(None)>
                        "Close"
                    </button>
                </div>
            }
            .into_any();
        }
        if let Some(pending) = workspace.memory_pending.get() {
            let choice = |id: &'static str, label: &'static str, choice: MemoryChoice| {
                view! {
                    <button type="button" id=id class=BUTTON on:click=move |_| workspace.resolve_memory(choice)>
                        {label}
                    </button>
                }
            };
            return match pending {
                MemoryPending::Invalid { edit, message } => view! {
                    <p class=ERROR id="memory-pending">{format!("{} is not a valid memory: {message}", edit.url)}</p>
                    <div class="flex gap-2">
                        {choice("memory-edit-again", "Edit again", MemoryChoice::EditAgain)}
                        {choice("memory-drop", "Drop the edit", MemoryChoice::Drop)}
                    </div>
                }
                .into_any(),
                MemoryPending::Changed { edit } => view! {
                    <p class="text-sm" id="memory-pending">{format!("{} changed since it was opened. Reload it and lose this edit, or overwrite it with this edit?", edit.url)}</p>
                    <div class="flex flex-wrap gap-2">
                        {choice("memory-reload", "Reload current", MemoryChoice::Reload)}
                        {choice("memory-overwrite", "Overwrite", MemoryChoice::Overwrite)}
                        {choice("memory-drop", "Drop the edit", MemoryChoice::Drop)}
                    </div>
                }
                .into_any(),
            };
        }
        let listing = match workspace.memory.get() {
            None => return loading("Reading memories…").into_any(),
            Some(Err(error)) => return view! { <p class=ERROR>{error}</p> }.into_any(),
            Some(Ok(listing)) => listing,
        };
        let rows = memory_rows(&listing)
            .into_iter()
            .map(|row| {
                let prefix = row.new_prefix();
                match row {
                    MemoryRow::Layer(layer) => view! {
                        <button
                            type="button"
                            class=format!("{ROW} mt-2")
                            title="Create a memory in this layer"
                            on:click=move |_| new_url.set(prefix.clone())
                        >
                            <span class="text-xs font-bold tracking-wide text-ink uppercase dark:text-sand">{layer_title(layer)}</span>
                        </button>
                    }
                    .into_any(),
                    MemoryRow::Group { name, .. } => view! {
                        <button
                            type="button"
                            class=format!("{ROW} pl-6")
                            title="Create a memory in this group"
                            on:click=move |_| new_url.set(prefix.clone())
                        >
                            <span class="flex items-center gap-2 font-mono text-sm">
                                <Glyph icon=icondata::FaFolderSolid class="opacity-70" />
                                {name}
                            </span>
                        </button>
                    }
                    .into_any(),
                    MemoryRow::Memory(memory) => {
                        let url = memory.url.clone();
                        let deleted = (memory.url.clone(), memory.revision.clone());
                        let description = memory.description.clone().unwrap_or_default();
                        view! {
                            <div class="memory-row flex items-center gap-2 pl-6" data-memory=memory.url.clone()>
                                <button type="button" class=format!("{ROW} min-w-0 flex-1") on:click=move |_| workspace.edit_memory(url.clone())>
                                    <span class="font-mono text-sm">{memory_name(&memory).to_owned()}</span>
                                    <span class=NOTE>{description}</span>
                                    {memory.invalid.clone().map(|reason| view! { <span class=ERROR>{reason}</span> })}
                                </button>
                                <button
                                    type="button"
                                    class=ICON_BUTTON
                                    aria-label="Delete"
                                    title="Delete"
                                    data-memory-delete
                                    on:click=move |_| confirm_delete.set(Some(deleted.clone()))
                                >
                                    <Glyph icon=icondata::FaTrashCanRegular />
                                </button>
                            </div>
                        }
                        .into_any()
                    }
                    MemoryRow::IgnoredHeading => view! { <p class="mt-2 text-sm font-bold">"Not memories"</p> }.into_any(),
                    MemoryRow::Ignored(entry) => view! {
                        <p class=format!("{NOTE} pl-6")>{format!("{} — {}", entry.path, entry.reason)}</p>
                    }
                    .into_any(),
                }
            })
            .collect_view();
        view! {
            {rows}
            {move || confirm_delete.get().map(|(url, revision)| {
                let deleted = (url.clone(), revision);
                view! {
                    <div class=CONFIRM>
                        <p class="text-sm">{format!("Delete {url}?")}</p>
                        <div class="flex gap-2">
                            <button type="button" id="memory-delete-confirm" class=PRIMARY on:click=move |_| {
                                confirm_delete.set(None);
                                workspace.delete_memory(deleted.0.clone(), deleted.1.clone());
                            }>
                                "Delete"
                            </button>
                            <button type="button" class=BUTTON on:click=move |_| {
                                confirm_delete.set(None);
                                workspace.status.set("Delete cancelled.".into());
                            }>
                                "Keep it"
                            </button>
                        </div>
                    </div>
                }
            })}
            <div class="mt-3 flex flex-col gap-2 border-t border-toasted pt-3 dark:border-ember">
                <label class="flex flex-col gap-1 text-sm">
                    <span class="font-bold">"New memory"</span>
                    <input
                        type="text"
                        id="memory-new-url"
                        class=FIELD
                        prop:value=move || new_url.get()
                        on:input=move |event| new_url.set(event_target_value(&event))
                    />
                </label>
                <button type="button" id="memory-new" class=PRIMARY on:click=move |_| workspace.new_memory(&new_url.get_untracked())>
                    <Glyph icon=icondata::FaPlusSolid />
                    "Create"
                </button>
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
