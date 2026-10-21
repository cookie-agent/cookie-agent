//! The Memory view (`/memory`): the memories by layer, each project a
//! folder that folds (folded until this browser opens it) and that can be
//! deleted with its memories; the editor, which shows an `agent://` file's
//! daemon-owned description and keywords as fixed; and the choices a write
//! that was not saved offers.

use std::collections::HashMap;

use cookie_agent_client_state::memory::{
    AgentMemoryParts, MemoryRow, agent_memory_parts, layer_title, memory_name,
};
use cookie_agent_protocol::{MemoryInfo, MemoryLayer};
use leptos::prelude::*;

use super::{
    BUTTON, ERROR, ICON_BUTTON, NOTE, PRIMARY, ROW, loading,
    manage::{CONFIRM, EDITOR, EDITOR_ACTIONS, EDITOR_PAGE, FIELD, STATE, area_value, field_value},
};
use crate::{
    browser,
    icons::Glyph,
    memory::{MemoryLine, folder_key, memory_lines, open_from_storage, project_strays},
    workspace::{MemoryChoice, MemoryEdit, MemoryPending, Workspace},
};

/// The icon of a layer's heading.
fn layer_icon(layer: MemoryLayer) -> crate::icons::Icon {
    match layer {
        MemoryLayer::User => icondata::FaUserSolid,
        MemoryLayer::Agent => icondata::FaRobotSolid,
        MemoryLayer::Machine => icondata::FaServerSolid,
        MemoryLayer::Project => icondata::FaFolderSolid,
        MemoryLayer::Unknown => icondata::FaBoxSolid,
    }
}

pub fn memory_sheet(workspace: Workspace) -> impl IntoView {
    let confirm_delete = RwSignal::new(None::<(String, String)>);
    let confirm_project = RwSignal::new(None::<String>);
    let new_url = RwSignal::new("project://".to_owned());
    // The folders opened or folded on this page; the rest are as this
    // browser keeps them, folded when it keeps nothing.
    let toggled = RwSignal::new(HashMap::<String, bool>::new());
    let is_open = move |project: &str| {
        toggled
            .with(|toggled| toggled.get(project).copied())
            .unwrap_or_else(|| open_from_storage(browser::load(&folder_key(project)).as_deref()))
    };
    move || {
        if let Some(edit) = workspace.memory_edit.get() {
            return memory_editor(workspace, edit).into_any();
        }
        if let Some(pending) = workspace.memory_pending.get() {
            return pending_choices(workspace, pending).into_any();
        }
        let listing = match workspace.memory.get() {
            None => return loading("Reading memories…").into_any(),
            Some(Err(error)) => return view! { <p class=ERROR>{error}</p> }.into_any(),
            Some(Ok(listing)) => listing,
        };
        let lines = memory_lines(&listing, is_open)
            .into_iter()
            .map(|line| match line {
                MemoryLine::Project {
                    name,
                    memories,
                    open,
                } => {
                    let strays = project_strays(&listing, &name);
                    project_folder(
                        workspace,
                        name,
                        memories,
                        open,
                        strays,
                        toggled,
                        confirm_project,
                        new_url,
                    )
                    .into_any()
                }
                MemoryLine::Row(row) => {
                    memory_row(workspace, row, confirm_delete, new_url).into_any()
                }
            })
            .collect_view();
        view! {
            {lines}
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

/// A row of the shared list: a layer's heading or a machine's group (a
/// click fills in its prefix for a new memory), a memory, or what is not
/// a memory.
fn memory_row(
    workspace: Workspace,
    row: MemoryRow,
    confirm_delete: RwSignal<Option<(String, String)>>,
    new_url: RwSignal<String>,
) -> impl IntoView {
    let prefix = row.new_prefix();
    match row {
        MemoryRow::Layer(layer) => view! {
            <button
                type="button"
                class=format!("{ROW} mt-2")
                title="Create a memory in this layer"
                data-memory-layer=layer.as_str()
                on:click=move |_| new_url.set(prefix.clone())
            >
                <span class="flex items-center gap-2 text-xs font-bold tracking-wide text-ink uppercase dark:text-sand">
                    <Glyph icon=layer_icon(layer) class="opacity-70" />
                    {layer_title(layer)}
                </span>
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
                    <Glyph icon=icondata::FaServerSolid class="opacity-70" />
                    {name}
                </span>
            </button>
        }
        .into_any(),
        MemoryRow::Memory(memory) => {
            let url = memory.url.clone();
            let deleted = (memory.url.clone(), memory.revision.clone());
            let description = memory.description.clone().unwrap_or_default();
            let indent = if memory.layer == MemoryLayer::Project {
                "pl-10"
            } else {
                "pl-6"
            };
            view! {
                <div class=format!("memory-row flex items-center gap-2 {indent}") data-memory=memory.url.clone() data-revision=memory.revision.clone()>
                    <button type="button" class=format!("{ROW} min-w-0 flex-1") on:click=move |_| workspace.edit_memory(url.clone())>
                        <span class="flex w-full items-center gap-2 font-mono text-sm">
                            <Glyph icon=icondata::FaFileLinesRegular class="opacity-60" />
                            <span class="min-w-0 truncate">{memory_name(&memory).to_owned()}</span>
                        </span>
                        <span class=format!("pl-6 {NOTE}")>{description}</span>
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
}

/// A project's folder: a click opens or folds it, and the browser keeps
/// the choice; its + starts a memory in it, and its trash deletes its
/// memories behind a confirmation.
#[expect(clippy::too_many_arguments, reason = "the folder's shared state")]
fn project_folder(
    workspace: Workspace,
    name: String,
    memories: usize,
    open: bool,
    strays: usize,
    toggled: RwSignal<HashMap<String, bool>>,
    confirm_project: RwSignal<Option<String>>,
    new_url: RwSignal<String>,
) -> impl IntoView {
    let toggle = {
        let name = name.clone();
        move |_| {
            let opened = !open;
            if opened {
                browser::store(&folder_key(&name), "open");
            } else {
                browser::forget(&folder_key(&name));
            }
            toggled.update(|toggled| {
                toggled.insert(name.clone(), opened);
            });
        }
    };
    let prefix = format!("project://{name}/");
    let confirming = {
        let name = name.clone();
        move || confirm_project.get().as_deref() == Some(name.as_str())
    };
    let deleted = name.clone();
    let asked = name.clone();
    let count = if memories == 1 {
        "1 memory".to_owned()
    } else {
        format!("{memories} memories")
    };
    let kept = match strays {
        0 => "The folder goes with them.".to_owned(),
        1 => "Its 1 file that is not a memory stays, and so does the folder.".to_owned(),
        strays => format!("Its {strays} files that are not memories stay, and so does the folder."),
    };
    let memory_question =
        format!("Delete project {name} and its {count}? This cannot be undone. {kept}");
    // A daemon that keeps projects deletes the project itself, with its
    // sessions: the question counts both.
    let project = {
        let name = name.clone();
        move || {
            workspace
                .projects
                .with(|projects| {
                    projects
                        .iter()
                        .find(|project| project.name.as_str() == name)
                        .cloned()
                })
                .filter(|_| workspace.has_projects.get())
        }
    };
    let question = {
        let project = project.clone();
        move || {
            project().map_or_else(
                || memory_question.clone(),
                |project| cookie_agent_client_state::projects::delete_question(&project),
            )
        }
    };
    view! {
        <div class="flex items-center gap-1 pl-3" data-project=name.clone()>
            <button
                type="button"
                class=format!("{ROW} min-w-0 flex-1")
                aria-expanded=open.to_string()
                title=if open { "Fold the project" } else { "Open the project" }
                data-project-toggle
                on:click=toggle
            >
                <span class="flex w-full items-center gap-2 font-mono text-sm">
                    <Glyph
                        icon=if open { icondata::FaChevronDownSolid } else { icondata::FaChevronRightSolid }
                        class="w-3 text-xs opacity-60"
                    />
                    <Glyph
                        icon=if open { icondata::FaFolderOpenSolid } else { icondata::FaFolderSolid }
                        class="text-caramel dark:text-maple"
                    />
                    <span class="min-w-0 flex-1 truncate font-bold">{name.clone()}</span>
                    <span class=STATE>{memories.to_string()}</span>
                </span>
            </button>
            <button
                type="button"
                class=ICON_BUTTON
                aria-label="New memory in this project"
                title="New memory in this project"
                data-project-new
                on:click=move |_| new_url.set(prefix.clone())
            >
                <Glyph icon=icondata::FaPlusSolid />
            </button>
            <button
                type="button"
                class=ICON_BUTTON
                aria-label="Delete the project"
                title=move || if workspace.has_projects.get() { "Delete the project with its sessions and memories" } else { "Delete the project's memories" }
                data-project-delete
                on:click=move |_| confirm_project.set(Some(asked.clone()))
            >
                <Glyph icon=icondata::FaTrashCanRegular />
            </button>
        </div>
        <Show when=confirming>
            <div class=format!("{CONFIRM} ml-6") role="alertdialog" data-project-confirm>
                <p class="text-sm">{question.clone()}</p>
                {move || {
                    workspace
                        .project_error
                        .get()
                        .map(|(_, message)| view! { <p class=ERROR data-project-error>{message}</p> })
                }}
                <div class="flex gap-2">
                    <button
                        type="button"
                        id="project-delete-confirm"
                        class=PRIMARY
                        on:click={
                            let deleted = deleted.clone();
                            let project = project.clone();
                            move |_| {
                                match project() {
                                    Some(project) => workspace.remove_project(project.id),
                                    None => {
                                        confirm_project.set(None);
                                        workspace.delete_project(deleted.clone());
                                    }
                                }
                            }
                        }
                    >
                        "Delete"
                    </button>
                    <button type="button" class=BUTTON on:click=move |_| confirm_project.set(None)>
                        "Keep it"
                    </button>
                </div>
            </div>
        </Show>
    }
}

/// The memory open in the editor: an `agent://` file as its parts, the
/// daemon's fixed, else the whole file as text.
fn memory_editor(workspace: Workspace, edit: MemoryEdit) -> impl IntoView {
    let info = workspace.memory.with_untracked(|listing| {
        listing
            .as_ref()
            .and_then(|listing| listing.as_ref().ok())
            .and_then(|listing| {
                listing
                    .memories
                    .iter()
                    .find(|memory| memory.url == edit.url)
                    .cloned()
            })
    });
    let parts = info
        .as_ref()
        .and_then(|info| agent_memory_parts(info, &edit.text));
    let original = info
        .as_ref()
        .and_then(|info: &MemoryInfo| agent_memory_parts(info, &edit.original));
    let url = edit.url.clone();
    let body = match &parts {
        Some(parts) => view! {
            <div class="flex flex-col gap-1" data-agent-memory>
                <span class="text-xs font-bold text-ink dark:text-sand">"Description"</span>
                <p
                    id="memory-description"
                    class="flex items-start gap-2 rounded-md border border-dashed border-crust px-3 py-2 text-sm dark:border-smoke"
                    title="The daemon keeps it: the agent's own description"
                >
                    <Glyph icon=icondata::FaLockSolid class="mt-1 text-xs opacity-60" />
                    <span>{parts.description.clone()}</span>
                </p>
                <span class="mt-1 text-xs font-bold text-ink dark:text-sand">"Keywords"</span>
                <div class="flex flex-wrap items-center gap-1.5">
                    {parts.fixed.iter().map(|keyword| view! {
                        <span class=format!("{STATE} inline-flex items-center gap-1 font-mono") title="The daemon keeps it" data-fixed-keyword>
                            <Glyph icon=icondata::FaLockSolid class="text-[0.6rem] opacity-60" />
                            {keyword.clone()}
                        </span>
                    }).collect_view()}
                </div>
                <input
                    type="text"
                    id="memory-keywords"
                    class=FIELD
                    placeholder="more keywords, separated by commas"
                    value=parts.extra.join(", ")
                    aria-label="More keywords"
                />
                <p class=NOTE>"The daemon owns the description and the first keywords (the agent and its mode); the rest are yours."</p>
                <span class="mt-1 text-xs font-bold text-ink dark:text-sand">"Memory"</span>
            </div>
            <textarea id="memory-text" class=EDITOR spellcheck="false">{parts.body.clone()}</textarea>
        }
        .into_any(),
        None => view! {
            <textarea id="memory-text" class=EDITOR spellcheck="false">{edit.text.clone()}</textarea>
        }
        .into_any(),
    };
    let save = StoredValue::new(parts.clone());
    view! {
        <div class=EDITOR_PAGE>
            <p class="font-mono text-sm font-bold" id="memory-url">{url}</p>
            {body}
            <div class=EDITOR_ACTIONS>
                <button
                    type="button"
                    id="memory-save"
                    class=PRIMARY
                    on:click=move |_| {
                        let text = match save.get_value() {
                            Some(parts) => {
                                let typed = AgentMemoryParts {
                                    extra: AgentMemoryParts::typed_keywords(&field_value("memory-keywords")),
                                    body: area_value("memory-text"),
                                    ..parts
                                };
                                // Unchanged parts are the text as read, so
                                // nothing is written.
                                if original.as_ref() == Some(&typed) {
                                    workspace.memory_edit.with_untracked(|edit| {
                                        edit.as_ref().map(|edit| edit.original.clone()).unwrap_or_default()
                                    })
                                } else {
                                    typed.text()
                                }
                            }
                            None => area_value("memory-text"),
                        };
                        workspace.save_memory(text);
                    }
                >
                    "Save"
                </button>
                <button type="button" class=BUTTON on:click=move |_| workspace.memory_edit.set(None)>
                    "Cancel"
                </button>
            </div>
        </div>
    }
}

/// A write that was not saved: what the view offers to do with it.
fn pending_choices(workspace: Workspace, pending: MemoryPending) -> impl IntoView {
    let choice = |id: &'static str, label: &'static str, choice: MemoryChoice| {
        view! {
            <button type="button" id=id class=BUTTON on:click=move |_| workspace.resolve_memory(choice)>
                {label}
            </button>
        }
    };
    match pending {
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
    }
}
