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
use wasm_bindgen::JsCast as _;

use super::{
    BUTTON, ERROR, ICON_BUTTON, NOTE, PRIMARY, ROW, empty_state, loading,
    manage::{CONFIRM, EDITOR, EDITOR_ACTIONS, EDITOR_PAGE, FIELD, STATE, area_value, field_value},
};
use cookie_agent_client_state::agent_documents::AgentDocumentAction;
use cookie_agent_protocol::{AgentsResolvedParams, AgentsResolvedResult};

use crate::{
    agents::AgentSummary,
    agents::{AgentEntry, agent_groups, agent_id, agent_summary},
    icons::Glyph,
    workspace::{AgentOpen, AgentTab, Workspace},
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
            || workspace.agent_open.with(Option::is_some)
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
    let create = move || {
        workspace.agent_tab.set(crate::workspace::AgentTab::File);
        workspace.new_agent(&field_value("config-new-path"));
    };
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
    // The one open ⋯ menu among the rows.
    let menu = RwSignal::new(None::<String>);
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
                        let description = agents
                            .iter()
                            .find(|agent| agent.id.as_str() == info.id.as_str())
                            .map(|agent| agent.description.clone())
                            .filter(|description| !description.is_empty());
                        // An action that asks first asks on the row.
                        let asking = {
                            let id = info.id.clone();
                            move || pending.get().filter(|(pending, _)| *pending == id).map(|(_, action)| action)
                        };
                        let confirm = {
                            let (id, preset, path) = (info.id.clone(), info.preset.clone(), info.path.clone());
                            move || {
                                asking().map(|action| {
                                    let (id, preset, path) = (id.clone(), preset.clone(), path.clone());
                                    view! {
                                        <span class="flex flex-wrap items-center gap-2 text-xs" data-agent-confirm>
                                            <span class="text-muted">{format!("{}: this overwrites your document.", action.label())}</span>
                                            <button
                                                type="button"
                                                class=format!("{PANEL_BUTTON} text-bad")
                                                data-agent-action=format!("{:?}", action).to_lowercase()
                                                on:click=move |_| {
                                                    pending.set(None);
                                                    workspace.agent_document_action(action, id.clone(), preset.clone(), path.clone());
                                                }
                                            >
                                                {action.label()}
                                            </button>
                                            <button type="button" class=PANEL_BUTTON on:click=move |_| pending.set(None)>"Cancel"</button>
                                        </span>
                                    }
                                })
                            }
                        };
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
                        let menu_items = context_items.clone();
                        // A click opens it on Resolved: the user's document
                        // in the editor, else the agent read-only
                        // (docs/site/specs/agent-shortcuts.md section 3.1).
                        let open = {
                            let info = info.clone();
                            move |_| match &info.path {
                                Some(path) => workspace.show_agent_document(path),
                                None => workspace.open_agent_read_only(crate::workspace::AgentOpen {
                                    id: info.id.clone(),
                                    preset: info.preset.clone(),
                                    path: None,
                                    origin: origin_label(&info),
                                    info: Some(info.clone()),
                                }),
                            }
                        };
                        let current = {
                            let (id, path) = (info.id.clone(), info.path.clone());
                            move || match &path {
                                Some(path) => workspace
                                    .config_edit
                                    .with(|edit| edit.as_ref().is_some_and(|edit| edit.path == *path)),
                                None => workspace.agent_open.with(|open| {
                                    open.as_ref().is_some_and(|open| open.path.is_none() && open.id == id)
                                }),
                            }
                        };
                        let current_name = current.clone();
                        view! {
                            {super::row_menu::context_menu(context, (), info.id.clone(), context_items)}
                            // Its name, what it does, and its actions in ⋯
                            // (docs/site/specs/web-polish.md, P5).
                            <div
                                class="group relative flex flex-col gap-0.5 rounded-lg px-3 py-1.5 transition-colors hover:bg-hover"
                                class:bg-sel=current
                                data-agent-document=info.id.clone()
                                on:contextmenu=super::row_menu::on_context(context)
                            >
                                // The whole row opens it; its name reads as the
                                // link, and ⋯ stays above.
                                <button
                                    type="button"
                                    class="absolute inset-0 rounded-lg"
                                    aria-label=format!("Show what {} runs", info.id)
                                    aria-current=move || current_name().then_some("true")
                                    data-agent-open=info.id.clone()
                                    on:click=open
                                ></button>
                                <span class="pointer-events-none relative flex w-full items-center gap-2">
                                    <span class="min-w-0 truncate text-sm font-semibold decoration-line-2 underline-offset-[3px] group-hover:underline">
                                        {info.id.clone()}
                                    </span>
                                    <span class=STATE>{origin_label(&info)}</span>
                                    <span class="flex-1"></span>
                                    <span class="pointer-events-auto">
                                        {super::row_menu::row_menu(menu, info.id.clone(), info.id.clone(), info.id.clone(), ROW_MENU_BUTTON, menu_items)}
                                    </span>
                                </span>
                                {description.map(|description| view! { <span class=format!("{NOTE} pointer-events-none relative line-clamp-2") data-agent-description>{description}</span> })}
                                {reason.map(|reason| view! { <span class=format!("{NOTE} pointer-events-none relative") data-agent-unavailable>{reason}</span> })}
                                <span class="relative">{confirm}</span>
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

/// A row's ⋯: quiet until the row is hovered or the menu is open.
const ROW_MENU_BUTTON: &str = "inline-flex h-11 w-11 lg:h-8 lg:w-8 shrink-0 items-center justify-center rounded-md text-muted opacity-60 transition hover:bg-sunken hover:text-fg hover:opacity-100 group-hover:opacity-100 aria-expanded:opacity-100";

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
                class=format!("group {ROW}")
                class:bg-sel=current_class
                aria-current=move || current().then_some("true")
                title=title
                on:click=move |_| workspace.show_agent_document(&opened)
            >
                <span class="flex w-full items-center gap-2">
                    <Glyph icon=mode_icon(summary.mode.as_deref()) class="text-sm opacity-80" />
                    <span class="min-w-0 flex-1 truncate text-sm font-semibold decoration-line-2 underline-offset-[3px] group-hover:underline">{agent.id}</span>
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

/// The open agent's editor, the agent shown read-only, or, with neither
/// open, what the view offers.
fn agent_detail(workspace: Workspace) -> impl IntoView {
    let confirm_delete = RwSignal::new(false);
    move || {
        let Some(edit) = workspace
            .config_edit
            .get()
            .filter(|edit| is_agent(&edit.path))
        else {
            confirm_delete.set(false);
            if let Some(open) = workspace.agent_open.get() {
                return read_only_agent(workspace, open).into_any();
            }
            return view! {
                {move || workspace.config_error.get().map(|error| view! { <p class=ERROR id="config-error">{error}</p> })}
                <div class="flex flex-col items-center gap-3 px-4 py-10 text-center text-sm text-muted" data-agents-empty>
                    <Glyph icon=icondata::LuBot class="text-3xl opacity-70" />
                    <p>"Pick an agent to see what it runs, or create one with New agent."</p>
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
        // The form reads the file's text as it opens, and writes each
        // change back into it, so Save saves either.
        let tab = workspace.agent_tab;
        let fields = RwSignal::new(AgentSummary::default());
        let open_form = move || {
            fields.set(crate::agents::agent_summary(&area_value("config-text")));
            tab.set(AgentTab::Form);
        };
        if tab.get_untracked() == AgentTab::Form {
            fields.set(crate::agents::agent_summary(&edit.text));
        }
        // A new document has nothing applied to resolve.
        if !deletable && tab.get_untracked() == AgentTab::Resolved {
            tab.set(AgentTab::File);
        }
        let resolved_params = AgentsResolvedParams {
            path: Some(path.clone()),
            ..AgentsResolvedParams::default()
        };
        // Resolved shows the applied document: say so when the editor
        // holds changes.
        let unapplied = {
            let original = edit.original.clone();
            let text = edit.text.clone();
            move || original.as_deref() != Some(text.as_str())
        };
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
                <div class="flex flex-wrap items-center gap-2">
                    <p class=format!("min-w-0 flex-1 font-mono {NOTE}") id="config-path">{path.clone()}</p>
                    // The same text three ways (docs/site/specs/web-polish.md,
                    // P5; docs/site/specs/agent-shortcuts.md section 3.1).
                    <div class="inline-flex rounded-lg bg-sunken p-0.5 text-xs" role="tablist" aria-label="Show as">
                        <button type="button" role="tab" class=move || seg_class(tab.get() == AgentTab::Form) aria-selected=move || (tab.get() == AgentTab::Form).to_string() data-agent-form-tab on:click=move |_| open_form()>"Form"</button>
                        <button type="button" role="tab" class=move || seg_class(tab.get() == AgentTab::File) aria-selected=move || (tab.get() == AgentTab::File).to_string() data-agent-file-tab on:click=move |_| tab.set(AgentTab::File)>"File"</button>
                        <Show when=move || deletable>
                            <button type="button" role="tab" class=move || seg_class(tab.get() == AgentTab::Resolved) aria-selected=move || (tab.get() == AgentTab::Resolved).to_string() data-agent-resolved-tab title="The document after every inherit, read-only" on:click=move |_| tab.set(AgentTab::Resolved)>
                                <Glyph icon=icondata::LuLayers class="mr-1" />
                                "Resolved"
                            </button>
                        </Show>
                    </div>
                </div>
                {move || workspace.config_error.get().map(|error| view! { <p class=ERROR id="config-error">{error}</p> })}
                <Show when=move || tab.get() == AgentTab::Form>
                    {agent_form(fields, tab)}
                </Show>
                <Show when=move || tab.get() == AgentTab::Resolved>
                    {
                        let unapplied = unapplied.clone();
                        let params = resolved_params.clone();
                        view! {
                            <Show when=unapplied.clone()>
                                <p class=NOTE data-resolved-unapplied>"Resolved shows the applied document. Apply to see your changes."</p>
                            </Show>
                            {resolved_view(workspace, params)}
                        }
                    }
                </Show>
                <textarea id="config-text" class=EDITOR class:hidden=move || tab.get() != AgentTab::File spellcheck="false">{edit.text.clone()}</textarea>
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
                        disabled=move || tab.get() == AgentTab::Resolved
                        title=move || (tab.get() == AgentTab::Resolved).then_some("Resolved is read-only; Apply saves the file")
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

/// An agent with no document of the user's, or an official document,
/// read-only: its resolved document, with Customize and Turn off when the
/// panel offers them (`docs/site/specs/agent-shortcuts.md` section 3.1).
fn read_only_agent(workspace: Workspace, open: AgentOpen) -> impl IntoView {
    use cookie_agent_client_state::agent_documents::actions;
    let name = open.name();
    let offered = open.info.as_ref().map(actions).unwrap_or_default();
    let customize = offered.contains(&AgentDocumentAction::Customize);
    let turn_off = offered.contains(&AgentDocumentAction::Disable);
    let template = name.starts_with('_');
    let acp = open
        .info
        .as_ref()
        .is_some_and(|info| info.kind == cookie_agent_protocol::AgentDocumentKind::Acp);
    let icon = if template {
        icondata::LuFileText
    } else if acp {
        icondata::LuUsers
    } else {
        icondata::LuBot
    };
    let act = move |action: AgentDocumentAction, open: &AgentOpen| {
        workspace.agent_document_action(action, open.id.clone(), open.preset.clone(), None);
    };
    let callout = open.path.is_none().then(|| {
        let what = if acp {
            format!("{name} has no document of its own, so it runs the _acp template.")
        } else {
            "This is the official document.".to_owned()
        };
        let target = match &open.preset {
            Some(preset) => format!("agents/{preset}/{name}.md"),
            None => format!("agents/{name}.md"),
        };
        view! {
            <p class="flex items-start gap-2 rounded-lg bg-accent-soft px-3 py-2 text-[0.8125rem] text-fg-2" data-agent-callout>
                <Glyph icon=icondata::LuInfo class="mt-0.5 text-accent" />
                <span>
                    {what}
                    {customize.then(|| view! {
                        " Customize writes "
                        <span class="font-mono text-xs">{target}</span>
                        ", which inherits it, with the inherited keys as comments."
                    })}
                </span>
            </p>
        }
    });
    let customize_open = open.clone();
    let off_open = open.clone();
    view! {
        <div class=EDITOR_PAGE data-agent-read-only>
            <div class="flex items-center gap-2">
                <Glyph icon=icon class="text-lg opacity-80" />
                <h2 class="min-w-0 truncate text-xl font-semibold tracking-tight" id="agent-name">{name.clone()}</h2>
                <span class=STATE>{open.origin.clone()}</span>
                {open.preset.clone().map(|preset| view! { <span class=STATE>{format!("preset {preset}")}</span> })}
            </div>
            <div class="flex flex-wrap items-center gap-2">
                <p class=format!("min-w-0 flex-1 font-mono {NOTE}")>
                    {open.path.clone().unwrap_or_else(|| if acp { "no document of yours".to_owned() } else { format!("official_agents/{name}.md") })}
                </p>
                <span class=format!("inline-flex items-center gap-1.5 {NOTE}")>
                    <Glyph icon=icondata::LuEye />
                    "Resolved, read-only"
                </span>
            </div>
            {move || workspace.config_error.get().map(|error| view! { <p class=ERROR id="config-error">{error}</p> })}
            {callout}
            {resolved_view(workspace, open.params())}
            <div class=EDITOR_ACTIONS>
                {customize.then(|| view! {
                    <button type="button" class=PRIMARY data-agent-customize on:click=move |_| act(AgentDocumentAction::Customize, &customize_open)>
                        <Glyph icon=icondata::LuPencil />
                        "Customize"
                    </button>
                })}
                <button type="button" class=BUTTON on:click=move |_| workspace.agent_open.set(None)>
                    "Close"
                </button>
                <span class="flex-1"></span>
                {turn_off.then(|| view! {
                    <button type="button" class=BUTTON data-agent-turn-off on:click=move |_| act(AgentDocumentAction::Disable, &off_open)>
                        "Turn off"
                    </button>
                })}
            </div>
        </div>
    }
}

/// Where "Mark where each key comes from" is kept.
const MARKS_KEY: &str = "cookie.agents.resolved_marks";

/// How many of the inherited prompt's lines show until it is unfolded.
const PROMPT_FOLD: u32 = 6;

/// The colour of the `index`th document of a chain of `length`: the accent
/// for the first, faint for the root, a hue for one between.
fn source_class(index: usize, length: usize) -> &'static str {
    if length <= 1 {
        ""
    } else if index + 1 == length {
        "bg-line-2"
    } else if index == 0 {
        "bg-accent"
    } else {
        "bg-hue-3 dark:bg-hue-3-dark"
    }
}

/// What a key's tag says it replaced: the inherited value when it was one
/// line, else how many lines (rules, for a permission action).
fn replaced_note(key: &str, replaced: &str) -> String {
    let lines = replaced.lines().collect::<Vec<_>>();
    let value = |line: &str| {
        let value = line
            .split_once(':')
            .map_or(line, |(_, value)| value)
            .trim()
            .trim_start_matches("- ")
            .to_owned();
        if value.chars().count() > 36 {
            format!("{}…", value.chars().take(34).collect::<String>().trim_end())
        } else {
            value
        }
    };
    match lines.as_slice() {
        [line] => format!("was {}", value(line)),
        [_, line] if !line.trim_start().starts_with('#') => format!("was {}", value(line.trim())),
        _ => format!(
            "replaces {} inherited {}",
            lines.len().saturating_sub(1),
            if key.starts_with("permissions.") {
                "rules"
            } else {
                "lines"
            },
        ),
    }
}

/// An agent document after every `inherit` (`agents.resolved`): its chain,
/// then the text with each key's source marked
/// (`docs/site/specs/agent-shortcuts.md` section 3.2).
fn resolved_view(workspace: Workspace, params: AgentsResolvedParams) -> impl IntoView {
    let resolved =
        RwSignal::new(None::<(AgentsResolvedParams, Result<AgentsResolvedResult, String>)>);
    let marks = RwSignal::new(crate::browser::load(MARKS_KEY).as_deref() != Some("off"));
    let unfolded = RwSignal::new(false);
    // Read again as the configuration changes, after an Apply among others.
    {
        let params = params.clone();
        Effect::new(move |_| {
            workspace.config.track();
            workspace.read_resolved_agent(params.clone(), resolved);
        });
    }
    move || {
        let Some((asked, result)) = resolved.get() else {
            return loading("Resolving the document…").into_any();
        };
        if asked != params {
            return loading("Resolving the document…").into_any();
        }
        let document = match result {
            Ok(document) => document,
            Err(error) => {
                return view! { <p class=ERROR data-resolved-error>{error}</p> }.into_any();
            }
        };
        resolved_document(workspace, document, marks, unfolded).into_any()
    }
}

fn resolved_document(
    workspace: Workspace,
    document: AgentsResolvedResult,
    marks: RwSignal<bool>,
    unfolded: RwSignal<bool>,
) -> impl IntoView {
    let chain = document.chain.clone();
    let length = chain.len();
    let index_of = {
        let chain = chain.clone();
        move |path: &str| {
            chain
                .iter()
                .position(|each| each == path)
                .unwrap_or(length.saturating_sub(1))
        }
    };
    let pills = chain
        .iter()
        .enumerate()
        .map(|(index, path)| {
            let opened = path.clone();
            view! {
                {(index > 0).then(|| view! { <Glyph icon=icondata::LuArrowRight class="text-faint" /> })}
                <button
                    type="button"
                    class="hit-area inline-flex items-center gap-1.5 rounded-full bg-raised px-2 py-0.5 font-mono text-xs shadow-[0_0_0_1px_var(--color-line-2)] hover:bg-sunken"
                    title=format!("Open {path}")
                    data-chain-document=path.clone()
                    on:click=move |_| workspace.open_agent_path(&opened)
                >
                    <span class=format!("h-1.5 w-1.5 rounded-full {}", source_class(index, length))></span>
                    {path.clone()}
                </button>
            }
        })
        .collect_view();
    let ghost = (!document.own_document && !chain.first().is_some_and(|path| path.starts_with("official_agents/") && length == 1)).then(|| {
        view! {
            <span class="rounded-full px-2 py-0.5 text-xs text-muted italic shadow-[inset_0_0_0_1px_var(--color-line)]">"no document of yours"</span>
            <Glyph icon=icondata::LuArrowRight class="text-faint" />
        }
    });
    let chain_row = view! {
        <div class="flex flex-wrap items-center gap-1.5 text-xs" data-resolved-chain>
            <span class="mr-0.5 text-muted">{if length > 1 { "Inherits" } else { "From" }}</span>
            {ghost}
            {pills}
            {(length == 1).then(|| view! { <span class=NOTE>"It inherits nothing."</span> })}
        </div>
    };
    let several = length > 1;
    let narrow = workspace.narrow.get_untracked();
    // Each line's key, by line number.
    let lines = document.text.lines().map(str::to_owned).collect::<Vec<_>>();
    let key_at = |line: u32| {
        document
            .keys
            .iter()
            .find(|key| key.first_line <= line && line <= key.last_line)
    };
    let prompt = document.prompt.clone();
    let values = document.values.clone();
    let mut rows = Vec::new();
    for (index, text) in lines.iter().enumerate() {
        let number = u32::try_from(index + 1).unwrap_or(u32::MAX);
        let in_prompt = prompt
            .as_ref()
            .is_some_and(|prompt| prompt.first_line <= number && number <= prompt.last_line);
        if let Some(prompt) = prompt.as_ref().filter(|prompt| prompt.first_line == number) {
            let bar = source_class(index_of(&prompt.source), length);
            rows.push(
                view! {
                    <div class="flex items-center gap-2 py-0.5 pr-3 font-sans text-[0.6875rem] text-muted" data-resolved-prompt>
                        <span class=move || format!("w-[3px] self-stretch {}", if marks.get() { bar } else { "" })></span>
                        <span class="ml-2">{format!("The prompt of {}", prompt.source)}</span>
                    </div>
                }
                .into_any(),
            );
        }
        let folded = prompt.as_ref().filter(|_| in_prompt).map(|prompt| {
            (
                number - prompt.first_line >= PROMPT_FOLD,
                prompt.last_line - prompt.first_line + 1,
            )
        });
        let (hidden, total) = folded.unwrap_or((false, 0));
        let bar = if in_prompt {
            prompt
                .as_ref()
                .map(|prompt| source_class(index_of(&prompt.source), length))
        } else {
            key_at(number).map(|key| source_class(index_of(&key.source), length))
        };
        let tag = key_at(number)
            .filter(|key| key.first_line == number && several)
            .map(|key| {
                let dot = source_class(index_of(&key.source), length);
                let note = key.replaced.as_deref().map(|replaced| replaced_note(&key.key, replaced));
                view! {
                    <span class=move || format!(
                        "{} items-center gap-1.5 font-sans text-[0.6875rem] whitespace-nowrap text-muted {}",
                        if marks.get() { "inline-flex" } else { "hidden" },
                        if narrow { "col-start-2 pl-3" } else { "ml-4 justify-self-end" },
                    )>
                        <span class=format!("h-1.5 w-1.5 rounded-full {dot}")></span>
                        {key.source.clone()}
                        {note.map(|note| view! { <span class="text-faint">{format!("· {note}")}</span> })}
                    </span>
                }
            });
        let value = values
            .iter()
            .filter(|value| text.contains(&value.name))
            .map(|value| {
                view! { <span class="ml-2 rounded bg-sunken px-1 font-sans text-[0.6875rem] text-muted">{format!("→ {}", value.value)}</span> }
            })
            .collect_view();
        let comment = text.trim_start().starts_with('#');
        let text = text.clone();
        rows.push(
            view! {
                <div
                    class=if narrow { "grid grid-cols-[3px_1fr] items-baseline pr-3" } else { "grid grid-cols-[3px_1fr_auto] items-baseline pr-3" }
                    class:hidden=move || hidden && !unfolded.get()
                >
                    <span class=move || format!("w-[3px] self-stretch rounded-r-sm {}", if marks.get() { bar.unwrap_or("") } else { "" })></span>
                    <span class="min-w-0 pl-3 break-words whitespace-pre-wrap" class:text-muted=comment>{text}{value}</span>
                    {tag}
                </div>
            }
            .into_any(),
        );
        if let Some(prompt) = prompt.as_ref().filter(|prompt| {
            in_prompt && number == prompt.first_line + PROMPT_FOLD - 1 && total > PROMPT_FOLD
        }) {
            let hidden_lines = prompt.last_line - prompt.first_line + 1 - PROMPT_FOLD;
            rows.push(
                view! {
                    <div class="py-0.5 pl-[15px]" class:hidden=move || unfolded.get()>
                        <button type="button" class="font-sans text-xs text-accent hover:underline" data-resolved-unfold on:click=move |_| unfolded.set(true)>
                            {format!("Show {hidden_lines} more lines")}
                        </button>
                    </div>
                }
                .into_any(),
            );
        }
    }
    view! {
        <div class="flex flex-col gap-2" data-resolved>
            {chain_row}
            <div class="flex flex-wrap items-center justify-between gap-2">
                {(!document.comments).then(|| view! { <span class=NOTE data-resolved-no-comments>"Shown without comments."</span> })}
                <span class="flex-1"></span>
                {several.then(|| view! {
                    <label class="inline-flex cursor-pointer items-center gap-1.5 text-xs text-muted select-none">
                        <input
                            type="checkbox"
                            class="accent-accent"
                            prop:checked=move || marks.get()
                            data-resolved-marks
                            on:change=move |event| {
                                let on = event_target_checked(&event);
                                marks.set(on);
                                crate::browser::store(MARKS_KEY, if on { "on" } else { "off" });
                            }
                        />
                        "Mark where each key comes from"
                    </label>
                })}
            </div>
            <div class="overflow-x-auto rounded-lg bg-surface-2 py-2 font-mono text-[0.8125rem] leading-relaxed shadow-[inset_0_0_0_1px_var(--color-line)]" data-resolved-text>
                {rows}
            </div>
        </div>
    }
}

/// A segment of the editor's Form | File switch.
fn seg_class(on: bool) -> &'static str {
    if on {
        "inline-flex min-h-11 min-w-11 items-center justify-center rounded-md bg-raised px-3 font-medium text-fg shadow-[0_1px_2px_rgb(0_0_0/0.08)] lg:min-h-0 lg:min-w-0 lg:px-2.5 lg:py-1"
    } else {
        "inline-flex min-h-11 min-w-11 items-center justify-center rounded-md px-3 text-muted hover:text-fg lg:min-h-0 lg:min-w-0 lg:px-2.5 lg:py-1"
    }
}

/// The agent editor's form: its description and mode, written into the
/// file's front matter as they change (`agents::set_front_matter`); the
/// rest of the document is the file's.
fn agent_form(fields: RwSignal<AgentSummary>, tab: RwSignal<AgentTab>) -> impl IntoView {
    let write = move |key: &'static str, value: String| {
        let text = area_value("config-text");
        match crate::agents::set_front_matter(&text, key, &value) {
            Some(text) => {
                if let Some(area) = document()
                    .get_element_by_id("config-text")
                    .and_then(|area| area.dyn_into::<web_sys::HtmlTextAreaElement>().ok())
                {
                    area.set_value(&text);
                }
            }
            // A key the file writes as a block: the file edits it.
            None => tab.set(AgentTab::File),
        }
    };
    let mode =
        move || fields.with(|fields| fields.mode.clone().unwrap_or_else(|| "primary".to_owned()));
    let mode_button = move |value: &'static str, label: &'static str| {
        view! {
            <button
                type="button"
                class=move || seg_class(mode() == value)
                aria-pressed=move || (mode() == value).to_string()
                data-agent-mode=value
                on:click=move |_| {
                    fields.update(|fields| fields.mode = Some(value.to_owned()));
                    write("mode", value.to_owned());
                }
            >
                {label}
            </button>
        }
    };
    view! {
        <div class="grid grid-cols-1 items-center gap-x-5 gap-y-3 rounded-xl bg-raised p-4 shadow-[0_0_0_1px_var(--color-line)] sm:grid-cols-[9rem_1fr]" data-agent-form>
            <label class="text-[0.8125rem] text-muted" for="agent-form-description">"Description"</label>
            <input
                type="text"
                id="agent-form-description"
                class=FIELD
                prop:value=move || fields.with(|fields| fields.description.clone().unwrap_or_default())
                on:input=move |event| {
                    let value = event_target_value(&event);
                    fields.update(|fields| fields.description = Some(value.clone()));
                    write("description", value);
                }
            />
            <span class="text-[0.8125rem] text-muted">"Mode"</span>
            <div class="inline-flex w-fit rounded-lg bg-sunken p-0.5 text-xs">
                {mode_button("primary", "Primary")}
                {mode_button("subagent", "Subagent")}
                {mode_button("all", "Both")}
            </div>
            <span class="text-[0.8125rem] text-muted sm:self-start">"The rest"</span>
            <p class=NOTE>
                "Models, permissions, tool groups, and the instructions are in the file; "
                <button type="button" class="text-accent underline-offset-2 hover:underline" on:click=move |_| tab.set(AgentTab::File)>"edit them there"</button>
                ", or see everything it inherits in "
                <button type="button" class="text-accent underline-offset-2 hover:underline" on:click=move |_| tab.set(AgentTab::Resolved)>"Resolved"</button>
                "."
            </p>
        </div>
    }
}
