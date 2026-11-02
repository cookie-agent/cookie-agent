//! The Agents view: the agent documents in a sidebar like Chat's sessions
//! (the shared agents, then each preset's), and the open document's
//! editor beside it. A desktop pins the sidebar, which the sidebar button
//! hides and shows; a narrower window shows it as the page in place of the
//! editor. A phone shows the agents as the page until one opens. Under the
//! documents, the ACP agents every runtime offers (`[acp.agents]`), whose
//! form opens over the page. Above the documents, the official agents, the
//! ACP agents, and the templates, each with where its document comes from,
//! why it cannot run, and Customize, Turn off, Reset, and Replace
//! (`docs/site/specs/acp-onboarding.md` section 7).

use leptos::prelude::*;

use super::{
    BUTTON, ERROR, ICON_BUTTON, NOTE, PRIMARY, ROW, empty_state, loading,
    manage::{CONFIRM, EDITOR, EDITOR_ACTIONS, EDITOR_PAGE, FIELD, STATE, area_value, field_value},
};
use cookie_agent_client_state::agent_documents::AgentDocumentAction;

use crate::{
    agents::{AgentEntry, agent_groups, agent_id, agent_summary},
    icons::Glyph,
    workspace::Workspace,
};

/// The icon of an agent by the mode its document gives it.
fn mode_icon(mode: Option<&str>) -> crate::icons::Icon {
    match mode {
        Some("primary") => icondata::LuBot,
        Some("subagent") => icondata::LuUserCog,
        Some("all") => icondata::LuUsers,
        _ => icondata::LuFileText,
    }
}

/// Whether `path` is an agent document, which this view edits.
fn is_agent(path: &str) -> bool {
    path.starts_with("agents/")
}

/// The Agents view: the sidebar, then the open agent or what to do. Below
/// desktop widths it shows one at a time: the agents fill the page while
/// the sidebar is open, and on a phone until one opens; the title bar's
/// sidebar button returns to them.
pub fn agents_page(workspace: Workspace) -> impl IntoView {
    // A phone shows the list until an agent opens, then the agent alone;
    // a wider window both, side by side.
    let phone = move || workspace.narrow.get();
    let editing = move || {
        workspace
            .config_edit
            .with(|edit| edit.as_ref().is_some_and(|edit| is_agent(&edit.path)))
    };
    let listing = move || phone() && !editing();
    view! {
        <div class="flex min-h-0 flex-1">
            <aside
                id="agents-sidebar"
                class="flex shrink-0 flex-col border-r border-line"
                class:hidden=move || phone() && editing()
                class:w-full=phone
                class:w-72=move || !phone()
            >
                {move || listing().then(|| {
                    workspace.config_error.get().map(|error| view! { <p class=format!("{ERROR} mx-3 mt-3") id="config-error">{error}</p> })
                })}
                <AgentList workspace=workspace />
            </aside>
            <div
                class="flex min-h-0 min-w-0 flex-1 flex-col overflow-y-auto p-3"
                class:hidden=listing
                data-page-body
            >
                <div class="mx-auto flex w-full max-w-3xl flex-1 flex-col gap-2">
                    {agent_detail(workspace)}
                </div>
            </div>
        </div>
    }
}

/// The agent documents by preset, with New agent and Reload above them,
/// then the ACP agents every runtime offers.
#[component]
pub fn AgentList(workspace: Workspace) -> impl IntoView {
    let creating = RwSignal::new(false);
    // `global`'s ACP agent declarations, read once the daemon shares them.
    let acp = super::machines::Manage::new();
    Effect::new(move |_| {
        if workspace.shares_acp_agents.get() {
            acp.read_agents(
                workspace,
                [
                    cookie_agent_protocol::RuntimeId::new(cookie_agent_protocol::RuntimeId::GLOBAL)
                        .expect("`global` is a runtime ID"),
                ],
            );
        }
    });
    let create = move || workspace.new_agent(&field_value("config-new-path"));
    // The panel's rows follow the configuration.
    Effect::new(move |_| {
        workspace.config.track();
        workspace.poll_agent_documents();
    });
    view! {
        <div class="flex flex-col gap-2 p-3">
            <div class="flex gap-2">
                <button
                    type="button"
                    class=format!("{PRIMARY} flex-1")
                    data-new-agent
                    aria-expanded=move || creating.get().to_string()
                    on:click=move |_| creating.update(|creating| *creating = !*creating)
                >
                    <Glyph icon=icondata::LuPlus />
                    "New agent"
                </button>
                <button
                    type="button"
                    id="config-reload"
                    class=ICON_BUTTON
                    aria-label="Reload from disk"
                    title="Reload the documents from disk, after editing them by hand"
                    on:click=move |_| workspace.reload_config()
                >
                    <Glyph icon=icondata::LuRefreshCw />
                </button>
            </div>
            <Show when=move || creating.get()>
                <label class="flex flex-col gap-1 text-xs font-bold text-muted">
                    <span>"Document"</span>
                    <input
                        type="text"
                        id="config-new-path"
                        class=FIELD
                        value="agents/"
                        on:keydown=move |event: leptos::ev::KeyboardEvent| {
                            if event.key() == "Enter" {
                                event.prevent_default();
                                create();
                            }
                        }
                    />
                </label>
                <p class=NOTE>"agents/<id>.md, or agents/<preset>/<id>.md for a preset's agent"</p>
                <div class="flex gap-2">
                    <button type="button" id="config-new" class=PRIMARY on:click=move |_| create()>
                        "Create"
                    </button>
                    <button type="button" class=BUTTON on:click=move |_| creating.set(false)>
                        "Cancel"
                    </button>
                </div>
            </Show>
        </div>
        <nav class="flex min-h-0 flex-1 flex-col gap-1 overflow-y-auto px-2 pb-3" aria-label="Agents">
            {agent_panel(workspace)}
            {move || {
                let config = match workspace.config.get() {
                    None => return loading("Reading the agents…").into_any(),
                    Some(Err(error)) => return view! { <p class=ERROR>{error}</p> }.into_any(),
                    Some(Ok(config)) => config,
                };
                let groups = agent_groups(&config);
                if groups.is_empty() {
                    return empty_state(icondata::LuBot, "No agent documents yet. Create one with New agent.")
                        .into_any();
                }
                groups
                    .into_iter()
                    .map(|group| {
                        let (icon, heading) = match &group.preset {
                            None => (icondata::LuUsers, "Shared agents".to_owned()),
                            Some(preset) => (icondata::LuLayers, format!("Preset {preset}")),
                        };
                        let empty = group.agents.is_empty();
                        let rows = group
                            .agents
                            .into_iter()
                            .map(|agent| agent_row(workspace, agent))
                            .collect_view();
                        view! {
                            <h3
                                class="mt-2 flex items-center gap-2 px-3 text-xs font-bold tracking-wide text-muted uppercase"
                                data-preset=group.preset.clone().unwrap_or_default()
                            >
                                <Glyph icon=icon class="opacity-70" />
                                <span class="min-w-0 truncate">{heading}</span>
                            </h3>
                            {empty.then(|| view! { <p class=format!("{NOTE} px-3")>"No agents in this preset."</p> })}
                            {rows}
                        }
                    })
                    .collect_view()
                    .into_any()
            }}
            <Show when=move || workspace.shares_acp_agents.get()>
                <div class="mt-3">{super::machines::shared_agents(workspace, acp)}</div>
            </Show>
        </nav>
        {super::machines::acp_dialog(workspace, acp, "agents-acp-form")}
    }
}

/// The official agents, the ACP agents, and the templates, by group, each
/// with its actions.
fn agent_panel(workspace: Workspace) -> impl IntoView {
    use cookie_agent_client_state::agent_documents::{
        AgentDocumentGroup, actions, group, origin_label, unavailable_line,
    };
    // An action that asks first, waiting for its second click.
    let pending = RwSignal::new(None::<(String, AgentDocumentAction)>);
    move || {
        let documents = match workspace.agent_documents.get() {
            None => return None,
            Some(Err(error)) => return Some(view! { <p class=ERROR>{error}</p> }.into_any()),
            Some(Ok(documents)) => documents,
        };
        let agents = workspace.with_runtime(|runtime| {
            runtime
                .snapshot()
                .map(|snapshot| snapshot.agents.clone())
                .unwrap_or_default()
        });
        let sections = [
            AgentDocumentGroup::Official,
            AgentDocumentGroup::Acp,
            AgentDocumentGroup::Templates,
        ]
        .into_iter()
        .filter_map(|each| {
            let rows = documents
                .iter()
                .filter(|info| info.preset.is_none() && group(info) == each)
                .cloned()
                .collect::<Vec<_>>();
            (!rows.is_empty()).then(|| {
                let rows = rows
                    .into_iter()
                    .map(|info| {
                        let reason = unavailable_line(&info, &agents);
                        let buttons = actions(&info)
                            .into_iter()
                            .map(|action| {
                                let key = (info.id.clone(), action);
                                let asking = {
                                    let key = key.clone();
                                    move || pending.get().as_ref() == Some(&key)
                                };
                                let (id, preset, path) =
                                    (info.id.clone(), info.preset.clone(), info.path.clone());
                                let click = {
                                    let key = key.clone();
                                    let asking = asking.clone();
                                    move |_| {
                                        if action.confirms() && !asking() {
                                            pending.set(Some(key.clone()));
                                            return;
                                        }
                                        pending.set(None);
                                        workspace.agent_document_action(
                                            action,
                                            id.clone(),
                                            preset.clone(),
                                            path.clone(),
                                        );
                                    }
                                };
                                view! {
                                    <button
                                        type="button"
                                        class=PANEL_BUTTON
                                        data-agent-action=format!("{:?}", action).to_lowercase()
                                        on:click=click
                                    >
                                        {move || if asking() {
                                            format!("Sure? {}", action.label())
                                        } else {
                                            action.label().to_owned()
                                        }}
                                    </button>
                                }
                            })
                            .collect_view();
                        // A right-click offers the same actions; one that
                        // asks first asks on the row, as its button does.
                        let context = RwSignal::new(None);
                        let context_items = {
                            let info = info.clone();
                            move || {
                                actions(&info)
                                    .into_iter()
                                    .map(|action| {
                                        let (id, preset, path) =
                                            (info.id.clone(), info.preset.clone(), info.path.clone());
                                        super::row_menu::MenuItem {
                                            id: "agent-action",
                                            label: action.label(),
                                            icon: icondata::LuBot,
                                            act: Callback::new(move |()| {
                                                if action.confirms() {
                                                    pending.set(Some((id.clone(), action)));
                                                } else {
                                                    workspace.agent_document_action(
                                                        action,
                                                        id.clone(),
                                                        preset.clone(),
                                                        path.clone(),
                                                    );
                                                }
                                            }),
                                            danger: action.confirms(),
                                        }
                                    })
                                    .collect::<Vec<_>>()
                            }
                        };
                        view! {
                            {super::row_menu::context_menu(context, (), info.id.clone(), context_items)}
                            <div
                                class="flex flex-col gap-1 rounded-lg px-3 py-1.5 transition-colors hover:bg-hover"
                                data-agent-document=info.id.clone()
                                on:contextmenu=super::row_menu::on_context(context)
                            >
                                <span class="flex w-full items-center gap-2">
                                    <span class="min-w-0 flex-1 truncate font-mono text-sm font-bold">{info.id.clone()}</span>
                                    <span class=STATE>{origin_label(&info)}</span>
                                </span>
                                {reason.map(|reason| view! { <span class=NOTE data-agent-unavailable>{reason}</span> })}
                                <span class="flex flex-wrap gap-1">{buttons}</span>
                            </div>
                        }
                    })
                    .collect_view();
                view! {
                    <h3 class="mt-2 flex items-center gap-2 px-3 text-xs font-bold tracking-wide text-muted uppercase">
                        {each.label()}
                    </h3>
                    {rows}
                }
            })
        })
        .collect_view();
        Some(
            view! { <section id="agent-panel" class="flex flex-col gap-1">{sections}</section> }
                .into_any(),
        )
    }
}

/// A panel row's action: small, beside the others.
const PANEL_BUTTON: &str = "inline-flex min-h-11 items-center rounded-lg px-2 text-xs hover:bg-sunken active:bg-press bg-raised shadow-[0_0_0_1px_var(--color-line-2)]";

/// An agent's row: its mode's icon, its ID, whether it is off or has an
/// edit the daemon refused, and its description; a click opens it.
fn agent_row(workspace: Workspace, agent: AgentEntry) -> impl IntoView {
    let path = agent.path.clone();
    let current = {
        let path = path.clone();
        move || {
            workspace
                .config_edit
                .with(|edit| edit.as_ref().is_some_and(|edit| edit.path == path))
        }
    };
    let draft = {
        let path = path.clone();
        move || {
            workspace
                .config_drafts
                .with(|drafts| drafts.contains_key(&path))
        }
    };
    let opened = path.clone();
    let title = path.clone();
    let summary = agent.summary;
    let note = summary
        .description
        .clone()
        .or_else(|| summary.mode.clone())
        .unwrap_or_default();
    let current_class = current.clone();
    view! {
        <div data-config-path=path.clone()>
            <button
                type="button"
                class=ROW
                class:bg-sel=current_class
                aria-current=move || current().then_some("true")
                title=title
                on:click=move |_| {
                    workspace.edit_config(&opened);
                    workspace.sidebar_open.set(false);
                }
            >
                <span class="flex w-full items-center gap-2">
                    <Glyph icon=mode_icon(summary.mode.as_deref()) class="text-sm opacity-80" />
                    <span class="min-w-0 flex-1 truncate font-mono text-sm font-bold">{agent.id}</span>
                    {(!summary.enabled).then(|| view! { <span class=STATE>"off"</span> })}
                    <Show when=draft.clone()>
                        <span class="text-xs text-accent">"not applied"</span>
                    </Show>
                </span>
                <span class=format!("w-full truncate pl-6 {NOTE}")>{note}</span>
            </button>
        </div>
    }
}

/// The open agent's editor, or, with none open, what the view offers.
fn agent_detail(workspace: Workspace) -> impl IntoView {
    let confirm_delete = RwSignal::new(false);
    move || {
        let Some(edit) = workspace
            .config_edit
            .get()
            .filter(|edit| is_agent(&edit.path))
        else {
            confirm_delete.set(false);
            return view! {
                {move || workspace.config_error.get().map(|error| view! { <p class=ERROR id="config-error">{error}</p> })}
                <div class="flex flex-col items-center gap-3 px-4 py-10 text-center text-sm text-muted" data-agents-empty>
                    <Glyph icon=icondata::LuBot class="text-3xl opacity-70" />
                    <p>"Pick an agent to edit its document, or create one with New agent."</p>
                    <button
                        type="button"
                        class=BUTTON
                        class:lg:hidden=move || workspace.sidebar_pinned.get()
                        data-show-agents
                        on:click=move |_| workspace.toggle_sidebar()
                    >
                        <Glyph icon=icondata::LuPanelLeft />
                        "Show the agents"
                    </button>
                </div>
            }
            .into_any();
        };
        let path = edit.path.clone();
        let preset = path
            .strip_prefix("agents/")
            .and_then(|rest| rest.split_once('/'))
            .map(|(preset, _)| preset.to_owned());
        let deletable = edit.original.is_some();
        let deleted = path.clone();
        view! {
            <div class=EDITOR_PAGE>
                <div class="flex items-center gap-2">
                    <Glyph icon=mode_icon(agent_summary(&edit.text).mode.as_deref()) class="text-lg opacity-80" />
                    <h2 class="min-w-0 truncate text-xl font-semibold tracking-tight" id="agent-name">{agent_id(&path).to_owned()}</h2>
                    {preset.map(|preset| view! {
                        <span class=STATE>
                            {format!("preset {preset}")}
                        </span>
                    })}
                    {(!deletable).then(|| view! { <span class=STATE>"new"</span> })}
                </div>
                <p class=format!("font-mono {NOTE}") id="config-path">{path.clone()}</p>
                {move || workspace.config_error.get().map(|error| view! { <p class=ERROR id="config-error">{error}</p> })}
                <textarea id="config-text" class=EDITOR spellcheck="false">{edit.text.clone()}</textarea>
                <Show when=move || confirm_delete.get()>
                    <div class=CONFIRM role="alertdialog">
                        <p class="text-sm">{format!("Delete {}? This cannot be undone.", deleted.clone())}</p>
                        <div class="flex gap-2">
                            <button
                                type="button"
                                id="config-delete-confirm"
                                class=PRIMARY
                                on:click={
                                    let deleted = deleted.clone();
                                    move |_| {
                                        confirm_delete.set(false);
                                        workspace.delete_config(deleted.clone());
                                    }
                                }
                            >
                                "Delete"
                            </button>
                            <button type="button" class=BUTTON on:click=move |_| confirm_delete.set(false)>
                                "Keep it"
                            </button>
                        </div>
                    </div>
                </Show>
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
                        "Close"
                    </button>
                    <span class="flex-1"></span>
                    <Show when=move || deletable>
                        <button
                            type="button"
                            class=BUTTON
                            data-config-delete
                            on:click=move |_| confirm_delete.set(true)
                        >
                            <Glyph icon=icondata::LuTrash2 />
                            "Delete"
                        </button>
                    </Show>
                </div>
            </div>
        }
        .into_any()
    }
}
