//! Projects in the chat view (`docs/site/specs/projects.md` section 10.1):
//! the sessions sidebar's project sections and their headers, "Move to
//! project…", the project editor with its emoji picker, and the new
//! session's project picker.

use std::collections::HashMap;

use cookie_agent_client_state::{
    badges::project_badges_tooltip,
    projects::{delete_question, project_label, project_sections_borrowed, validate_emoji},
};
use cookie_agent_protocol::{ProjectId, ProjectInfo, SessionId, SessionMeta};
use leptos::{ev, html, prelude::*};

use super::{
    BUTTON, ERROR, ICON_BUTTON, NOTE, PRIMARY, ROW, choice_line, empty_state, search_query,
    session_rows::{SessionRows, keyed_rows, search_matches},
};
use crate::{
    browser,
    icons::Glyph,
    layout::{Press, View},
    workspace::{CardSet, PickState, Workspace},
};

/// How long a finger holds a header before it shows its buttons.
const LONG_PRESS: std::time::Duration = std::time::Duration::from_millis(500);

/// A header's button (pencil, trash, `+`): clear at rest, so it shows the
/// header's own colour (hovered or pressed with it), one step darker under
/// the pointer, darker still pressed. Pressing it never presses the header.
const HEADER_ACTION: &str = "inline-flex min-h-11 min-w-11 shrink-0 items-center justify-center rounded-md text-muted hover:bg-press hover:text-fg active:bg-press";

/// What the pencil and trash lie on over the header's name: the header's
/// hover colour, fading in from their left, and its pressed colour while
/// the name is pressed.
const HEADER_COVER: &str = "pl-6 bg-[linear-gradient(to_right,transparent,var(--color-hover-solid)_1.5rem)] group-has-[[data-section-toggle]:active]:bg-[linear-gradient(to_right,transparent,var(--color-press-solid)_1.5rem)]";

/// A text field of the editor.
const FIELD: &str = "min-h-11 w-full min-w-0 rounded-lg bg-surface px-3 text-sm hover:shadow-[inset_0_0_0_1px_var(--color-faint)] focus:shadow-[inset_0_0_0_1px_var(--color-accent),0_0_0_3px_var(--color-accent-soft)] focus:outline-none shadow-[inset_0_0_0_1px_var(--color-line-2)]";

/// A header's state shared by every header of the list: which one a
/// mouse is over, which a long press revealed, and which delete waits for
/// confirmation.
#[derive(Clone, Copy)]
pub(super) struct HeaderActions {
    hovered: RwSignal<Option<String>>,
    revealed: RwSignal<Option<String>>,
    confirming: RwSignal<Option<ProjectId>>,
    press: StoredValue<Press>,
    timer: StoredValue<Option<TimeoutHandle>>,
}

impl HeaderActions {
    pub(super) fn new() -> Self {
        Self {
            hovered: RwSignal::new(None),
            revealed: RwSignal::new(None),
            confirming: RwSignal::new(None),
            press: StoredValue::new(Press::default()),
            timer: StoredValue::new(None),
        }
    }

    fn stop_timer(self) {
        if let Some(timer) = self.timer.get_value() {
            timer.clear();
        }
        self.timer.set_value(None);
    }
}

/// One project section as the list shows it.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct ListedSection {
    /// What the browser remembers its folding under.
    key: String,
    project: Option<ProjectInfo>,
    /// Its sessions, while the list's search holds something only those
    /// that match it, in the list's order.
    sessions: Vec<SessionId>,
    /// Those of them a folded section still shows: at work (running, or
    /// waiting on a producer), unread, or the one the page shows.
    folded: Vec<SessionId>,
}

impl ListedSection {
    /// The rows it shows: all of its sessions while open, else
    /// [`Self::folded`].
    fn rows(&self, open: bool) -> &[SessionId] {
        if open { &self.sessions } else { &self.folded }
    }
}

/// The project sections, then "No project": while the list's search holds
/// something only those with sessions that match it, and only those
/// sessions. Worked out once for each change, for both the list and its
/// Select all.
pub(super) fn listed_sections(
    workspace: Workspace,
    searching: Memo<bool>,
    labels: Memo<HashMap<SessionId, String>>,
) -> Memo<Vec<ListedSection>> {
    Memo::new(move |_| {
        if !workspace.has_projects.get() {
            return Vec::new();
        }
        let query = search_query(workspace);
        let searching = searching.get();
        // A filter by agent or machine leaves out the sections it empties,
        // as a search does.
        let narrowed = workspace
            .session_filter
            .with(|filter| !filter.by_date_only());
        // The root of the session the page shows, so a subagent's
        // transcript keeps its tree's row too.
        let selected = workspace
            .selected
            .get()
            .map(|session_id| workspace.root_of(session_id));
        labels.with(|labels| {
            workspace.projects.with(|projects| {
                workspace.sessions.with(|sessions| {
                    project_sections_borrowed(projects, sessions)
                        .into_iter()
                        .filter_map(|section| {
                            let key = section.key();
                            let shown = section
                                .sessions
                                .into_iter()
                                .filter(|session| {
                                    workspace.filter_passes(session)
                                        && search_matches(
                                            labels,
                                            session.session_id,
                                            query.as_deref(),
                                        )
                                })
                                .collect::<Vec<_>>();
                            if (searching || narrowed) && shown.is_empty() {
                                return None;
                            }
                            Some(ListedSection {
                                key,
                                project: section.project,
                                folded: shown
                                    .iter()
                                    .filter(|session| folded_shows(session, selected))
                                    .map(|session| session.session_id)
                                    .collect(),
                                sessions: shown.iter().map(|session| session.session_id).collect(),
                            })
                        })
                        .collect()
                })
            })
        })
    })
}

/// Whether no section holds a session.
pub(super) fn sections_hold_none(sections: Memo<Vec<ListedSection>>) -> bool {
    sections.with(|sections| sections.iter().all(|section| section.sessions.is_empty()))
}

/// The session list as project sections, then "No project".
pub(super) fn project_list(
    rows: SessionRows,
    sections: Memo<Vec<ListedSection>>,
    searching: Memo<bool>,
    headers: HeaderActions,
) -> impl IntoView {
    let workspace = rows.workspace;
    let narrowed = Memo::new(move |_| {
        workspace
            .session_filter
            .with(|filter| !filter.by_date_only())
    });
    let nothing_matches =
        Memo::new(move |_| (searching.get() || narrowed.get()) && sections.with(Vec::is_empty));
    let keys = move || {
        sections.with(|sections| {
            sections
                .iter()
                .map(|section| section.key.clone())
                .collect::<Vec<_>>()
        })
    };
    move || {
        if nothing_matches.get() {
            if !searching.get() {
                return empty_state(icondata::LuListFilter, "No sessions match the filter.")
                    .into_any();
            }
            return empty_state(icondata::LuSearch, "No sessions match.").into_any();
        }
        view! {
            <For
                each=keys
                key=String::clone
                children=move |key| section_view(rows, sections, searching, key, headers)
            />
        }
        .into_any()
    }
}

/// The root rows the project sections show, in page order: an open
/// section's sessions, and a folded one's at work, unread, or shown; a
/// search's matches, folded or not.
pub(super) fn listed_rows(
    workspace: Workspace,
    sections: Memo<Vec<ListedSection>>,
    searching: Memo<bool>,
) -> Vec<SessionId> {
    let searching = searching.get();
    sections.with(|sections| {
        sections
            .iter()
            .flat_map(|section| {
                section
                    .rows(searching || workspace.section_open(&section.key))
                    .iter()
                    .copied()
            })
            .collect()
    })
}

/// Whether a folded section still shows `session`'s row: at work (running,
/// or waiting on a producer), unread, pinned, or `selected`, the root of the session
/// the page shows, so a row the user just opened stays where they clicked
/// it.
fn folded_shows(session: &SessionMeta, selected: Option<SessionId>) -> bool {
    session.unread
        || session.pinned
        || selected == Some(session.session_id)
        || cookie_agent_client_state::activity::session_at_work(session)
}

/// One section: its header, then its sessions while it is open, and its
/// sessions at work (running, or waiting on a producer), its unread ones,
/// and the one the page shows while it is folded. The header draws again
/// when the project or the section's sessions change; each row only when
/// its own session does.
fn section_view(
    rows: SessionRows,
    sections: Memo<Vec<ListedSection>>,
    searching: Memo<bool>,
    key: String,
    headers: HeaderActions,
) -> impl IntoView {
    let workspace = rows.workspace;
    let section = {
        let key = key.clone();
        Memo::new(move |_| {
            sections.with(|sections| sections.iter().find(|section| section.key == key).cloned())
        })
    };
    let open = {
        let key = key.clone();
        Memo::new(move |_| workspace.section_open(&key))
    };
    // The header's project and members, in no particular order.
    let header = Memo::new(move |_| {
        section.with(|section| {
            section.as_ref().map(|section| {
                let mut members = section.sessions.clone();
                members.sort_unstable();
                (section.project.clone(), members)
            })
        })
    });
    let empty = Memo::new(move |_| {
        section.with(|section| {
            section
                .as_ref()
                .is_none_or(|section| section.sessions.is_empty())
        })
    });
    // A search shows its matches in folded sections too.
    let shown_open = Memo::new(move |_| open.get() || searching.get());
    let shown = Memo::new(move |_| {
        let open = shown_open.get();
        section.with(|section| {
            section
                .as_ref()
                .map(|section| section.rows(open).to_vec())
                .unwrap_or_default()
        })
    });
    let has_rows = Memo::new(move |_| shown.with(|shown| !shown.is_empty()));
    let header_key = key.clone();
    view! {
        <section class="flex flex-col gap-1 pb-2" data-project-section=key.clone() data-open=move || open.get().to_string()>
            {move || {
                header.get().map(|(project, members)| {
                    project_header(workspace, project, header_key.clone(), open, headers, members)
                })
            }}
            {move || {
                if empty.get() {
                    return shown_open
                        .get()
                        .then(|| view! { <p class=format!("px-3 pb-1 {NOTE}") data-section-empty>"No sessions yet."</p> })
                        .into_any();
                }
                // Folded, the section still shows its sessions at work,
                // as a folded subagent tree shows its running agents, its
                // unread ones, and the one the page shows.
                // Its rows sit indented under the header.
                has_rows
                    .get()
                    .then(|| view! {
                        <div class="ml-4 flex flex-col gap-1" data-section-rows>
                            {keyed_rows(rows, move || shown.get())}
                        </div>
                    })
                    .into_any()
            }}
        </section>
    }
}

/// A section's header: a click folds or opens it. It shows the project's
/// emoji (a folder without one) and its name; its tooltip names its
/// default attachments. `+` starts a session in it; a project's pencil
/// and delete show on hover, or after a long press on a touch screen.
fn project_header(
    workspace: Workspace,
    project: Option<ProjectInfo>,
    key: String,
    open: Memo<bool>,
    headers: HeaderActions,
    members: Vec<SessionId>,
) -> impl IntoView {
    let project_id = project.as_ref().map(|project| project.id.clone());
    let label = project.as_ref().map_or_else(
        || "No project".to_owned(),
        |project| project.name.to_string(),
    );
    let emoji = project.as_ref().and_then(|project| project.emoji.clone());
    let editable = project.is_some();
    let defaults = project
        .as_ref()
        .and_then(|project| project.defaults.clone());
    let tooltip = {
        let defaults = defaults.clone();
        let label = label.clone();
        move || {
            let places = defaults.as_ref().map(|defaults| {
                workspace.runtime_homes.with(|homes| {
                    workspace
                        .runtimes
                        .with(|listing| project_badges_tooltip(defaults, listing.as_ref(), homes))
                })
            });
            match places {
                Some(places)
                    if defaults
                        .as_ref()
                        .is_some_and(|defaults| !defaults.runtimes.is_empty()) =>
                {
                    format!("{label}\nNew sessions attach:\n{places}")
                }
                _ => label.clone(),
            }
        }
    };
    let HeaderActions {
        hovered,
        revealed,
        confirming,
        press,
        ..
    } = headers;
    let shown = {
        let key = key.clone();
        move || {
            editable
                && (hovered.get().as_deref() == Some(key.as_str())
                    || revealed.get().as_deref() == Some(key.as_str()))
        }
    };
    // In select mode, a checkbox for all of the section's sessions.
    let picks = members.clone();
    let pick_state = {
        let picks = picks.clone();
        Memo::new(move |_| workspace.pick_state(&picks))
    };
    let pick_label = format!("Select the sessions in {label}");
    let picking = {
        let empty = members.is_empty();
        move || workspace.picking.active.get() && !empty
    };
    let on_pointerdown = {
        let key = key.clone();
        move |event: ev::PointerEvent| {
            if event.pointer_type() != "touch" || !editable {
                return;
            }
            let (x, y) = (f64::from(event.client_x()), f64::from(event.client_y()));
            press.update_value(|press| press.start(x, y));
            headers.stop_timer();
            let key = key.clone();
            let timer = set_timeout_with_handle(
                move || {
                    if press.try_update_value(Press::elapse).unwrap_or(false) {
                        revealed.set(Some(key.clone()));
                    }
                },
                LONG_PRESS,
            )
            .ok();
            headers.timer.set_value(timer);
        }
    };
    let on_pointermove = move |event: ev::PointerEvent| {
        let (x, y) = (f64::from(event.client_x()), f64::from(event.client_y()));
        if press
            .try_update_value(|press| press.moved(x, y))
            .unwrap_or(false)
        {
            headers.stop_timer();
        }
    };
    let release = move |_| {
        press.update_value(Press::cancel);
        headers.stop_timer();
    };
    let toggle_key = key.clone();
    let menu_key = key.clone();
    let enter_key = key.clone();
    let leave_key = key.clone();
    let error_key = key.clone();
    let new_in = project_id.clone();
    let edit_id = project_id.clone();
    let delete_id = project_id.clone();
    let confirm_id = project_id.clone();
    let confirm_project = project.clone();
    let shown_buttons = shown.clone();
    view! {
        <div
            class="project-header group relative flex rounded-md"
            data-project-header=project_id.as_ref().map_or_else(|| "none".to_owned(), ToString::to_string)
            on:pointerenter=move |event: ev::PointerEvent| {
                if event.pointer_type() != "touch" {
                    hovered.set(Some(enter_key.clone()));
                }
            }
            on:pointerleave=move |_| {
                if hovered.get_untracked().as_deref() == Some(leave_key.as_str()) {
                    hovered.set(None);
                }
            }
        >
            <Show when=picking>
                <button
                    type="button"
                    class="inline-flex min-h-11 w-9 shrink-0 items-center justify-center self-start rounded-md text-accent hover:bg-press active:bg-press"
                    role="checkbox"
                    aria-checked=move || pick_state.get().aria_checked()
                    aria-label=pick_label.clone()
                    title=pick_label.clone()
                    data-section-pick=move || pick_state.get().aria_checked()
                    on:click={
                        let picks = picks.clone();
                        move |_| workspace.toggle_picks(&picks)
                    }
                >
                    <Glyph icon=Signal::derive(move || match pick_state.get() {
                        PickState::None => icondata::LuSquare,
                        PickState::Some => icondata::LuSquareMinus,
                        PickState::All => icondata::LuSquareCheck,
                    }) />
                </button>
            </Show>
            <button
                type="button"
                class="no-callout flex min-w-0 flex-1 flex-col items-stretch rounded-md text-left select-none hover:bg-hover active:bg-press group-hover:bg-hover"
                aria-expanded=move || open.get().to_string()
                title=tooltip
                data-section-toggle
                on:pointerdown=on_pointerdown
                on:pointermove=on_pointermove
                on:pointerup=release
                on:pointercancel=release
                on:touchend=move |event: ev::TouchEvent| {
                    // Chrome ends a long press with a click, which would
                    // land on the buttons it just showed under the finger.
                    if press.try_update_value(|press| !press.click()).unwrap_or(false) {
                        event.prevent_default();
                    }
                }
                on:contextmenu=move |event: ev::MouseEvent| {
                    // A long press shows the header's buttons, not the
                    // browser's menu; so does a mouse's right-click.
                    if revealed.get_untracked().as_deref() == Some(menu_key.as_str()) {
                        event.prevent_default();
                    } else if !crate::browser::from_touch(&event)
                        && super::row_menu::wants_context_menu(&event)
                    {
                        event.prevent_default();
                        revealed.set(Some(menu_key.clone()));
                    }
                }
                on:click=move |_| {
                    if press.try_update_value(Press::click).unwrap_or(true) {
                        workspace.toggle_section(&toggle_key);
                    }
                }
            >
                // The name's line keeps clear of `+`; the pencil and trash
                // lie over it when they show, without moving it.
                <span class="flex min-h-11 w-full items-center gap-2 pr-12 pl-2">
                    <span class="inline-flex shrink-0 opacity-70 transition-transform" class:rotate-90=move || open.get()>
                        <Glyph icon=icondata::LuChevronRight class="text-[0.625rem]" />
                    </span>
                    {match emoji.clone() {
                        Some(emoji) => view! { <span class="w-5 shrink-0 text-center text-base leading-none" data-project-emoji>{emoji}</span> }.into_any(),
                        None if editable => view! { <Glyph icon=icondata::LuFolder class="w-5 justify-center text-accent" /> }.into_any(),
                        None => view! { <Glyph icon=icondata::LuFolderOpen class="w-5 justify-center opacity-70" /> }.into_any(),
                    }}
                    <span class=move || format!("min-w-0 flex-1 truncate text-sm font-bold {}", if editable { "" } else { "text-muted" }) data-project-name>
                        {label.clone()}
                    </span>
                </span>
            </button>
            // Level with the name's line: the pencil and delete (on hover,
            // or after a long press), then `+`, always there.
            <div
                class=move || format!(
                    "absolute top-0 right-0 flex h-11 items-center gap-0.5 rounded-tr-md pr-0.5 {}",
                    if shown_buttons() { HEADER_COVER } else { "" },
                )
                data-header-actions
            >
                <Show when=shown.clone()>
                    <button
                        type="button"
                        class=HEADER_ACTION
                        aria-label="Edit the project"
                        title="Edit the project"
                        data-header-edit
                        on:click={
                            let edit_id = edit_id.clone();
                            move |_| {
                                revealed.set(None);
                                workspace.open_project_editor(edit_id.clone(), false);
                            }
                        }
                    >
                        <Glyph icon=icondata::LuPencil />
                    </button>
                    <button
                        type="button"
                        class=HEADER_ACTION
                        aria-label="Delete the project"
                        title="Delete the project with its sessions and memories"
                        data-header-delete
                        on:click={
                            let delete_id = delete_id.clone();
                            move |_| {
                                revealed.set(None);
                                confirming.set(delete_id.clone());
                            }
                        }
                    >
                        <Glyph icon=icondata::LuTrash2 />
                    </button>
                </Show>
                <button
                    type="button"
                    class=HEADER_ACTION
                    aria-label=if editable { "New session in this project" } else { "New session in no project" }
                    title=if editable { "New session in this project" } else { "New session in no project" }
                    data-header-new
                    on:click=move |_| workspace.new_session_in(new_in.clone())
                >
                    <Glyph icon=icondata::LuPlus />
                </button>
            </div>
        </div>
        <Show when=move || confirm_id.is_some() && confirming.get() == confirm_id>
            {
                let question = confirm_project.as_ref().map(delete_question).unwrap_or_default();
                let project = confirm_project.as_ref().map(|project| project.id.clone());
                view! {
                    <div class="mx-2 mb-1 flex flex-col gap-2 rounded-xl p-2 text-sm bg-raised shadow-[0_0_0_1px_var(--color-accent),0_0_0_4px_var(--color-accent-soft)]" role="alertdialog" data-project-confirm-box>
                        <p>{question}</p>
                        <div class="flex flex-wrap gap-2">
                            <button
                                type="button"
                                class=PRIMARY
                                data-project-delete-confirm
                                on:click={
                                    let project = project.clone();
                                    move |_| {
                                        confirming.set(None);
                                        if let Some(project) = project.clone() {
                                            workspace.remove_project(project);
                                        }
                                    }
                                }
                            >
                                "Delete"
                            </button>
                            <button type="button" class=BUTTON on:click=move |_| confirming.set(None)>
                                "Keep it"
                            </button>
                        </div>
                    </div>
                }
            }
        </Show>
        {move || {
            workspace
                .project_error
                .get()
                .filter(|(failed, _)| *failed == error_key)
                .map(|(_, message)| view! { <p class=format!("{ERROR} mx-2 mb-1") data-project-error>{message}</p> })
        }}
    }
}

/// "Move to project…": the projects, then "No project"; the session's own
/// is checked.
pub(super) fn move_sheet(workspace: Workspace) -> impl IntoView {
    let Some(session_id) = workspace.moving.get_untracked() else {
        return ().into_any();
    };
    let current = workspace.sessions.with_untracked(|sessions| {
        sessions
            .iter()
            .find(|session| session.session_id == session_id)
            .and_then(|session| session.project.clone())
    });
    let choice = move |project: Option<ProjectInfo>| {
        let id = project.as_ref().map(|project| project.id.clone());
        let chosen = id == current;
        let label = project.as_ref().map_or_else(
            || "No project".to_owned(),
            |project| project.name.to_string(),
        );
        let emoji = project.as_ref().and_then(|project| project.emoji.clone());
        view! {
            <button
                type="button"
                class=ROW
                class:bg-sel=chosen
                data-move-to=id.as_ref().map_or_else(|| "none".to_owned(), ToString::to_string)
                on:click=move |_| {
                    workspace.move_session(session_id, id.clone());
                    workspace.discard_panel();
                }
            >
                <span class="flex w-full items-center gap-2">
                    {project_mark(emoji, project.is_some())}
                    {choice_line(label, chosen, "text-sm font-bold")}
                </span>
            </button>
        }
    };
    let projects = workspace.projects.get_untracked();
    view! {
        <p class=NOTE>"The session keeps its runtimes and its agent; it takes the project's memory and AGENTS.md from its next message."</p>
        {projects.into_iter().map(|project| choice(Some(project))).collect_view()}
        {choice(None)}
    }
    .into_any()
}

/// A project's mark before its name: its emoji, or a folder.
fn project_mark(emoji: Option<String>, project: bool) -> impl IntoView {
    match emoji {
        Some(emoji) => {
            view! { <span class="w-5 shrink-0 text-center text-base leading-none">{emoji}</span> }
                .into_any()
        }
        None if project => {
            view! { <Glyph icon=icondata::LuFolder class="w-5 justify-center text-accent" /> }
                .into_any()
        }
        None => {
            view! { <Glyph icon=icondata::LuFolderOpen class="w-5 justify-center opacity-70" /> }
                .into_any()
        }
    }
}

/// The new session's project, above its runtime cards: the projects, "No
/// project", and "New project…", which opens the editor.
#[component]
pub(super) fn NewSessionProject(workspace: Workspace) -> impl IntoView {
    const NEW: &str = "__new";
    let select = NodeRef::<html::Select>::new();
    let current = move || {
        workspace
            .new_project
            .get()
            .map(|id| id.to_string())
            .unwrap_or_default()
    };
    view! {
        <Show when=move || workspace.has_projects.get()>
            <label class="flex flex-col gap-1 text-xs font-bold text-muted">
                <span>"Project"</span>
                <select
                    id="new-session-project"
                    node_ref=select
                    class="min-h-11 rounded-lg bg-surface px-2 text-sm font-normal text-fg hover:shadow-[inset_0_0_0_1px_var(--color-faint)] shadow-[inset_0_0_0_1px_var(--color-line-2)]"
                    prop:value=current
                    on:change=move |event| {
                        let value = event_target_value(&event);
                        if value == NEW {
                            // The picker keeps its choice until the new
                            // project is saved.
                            if let Some(select) = select.get_untracked() {
                                select.set_value(&current());
                            }
                            workspace.open_project_editor(None, true);
                        } else {
                            workspace.choose_new_project(ProjectId::new(value).ok());
                        }
                    }
                >
                    {move || {
                        let chosen = workspace.new_project.get();
                        workspace
                            .projects
                            .get()
                            .into_iter()
                            .map(|project| {
                                let selected = chosen.as_ref() == Some(&project.id);
                                view! {
                                    <option value=project.id.to_string() selected=selected>
                                        {project_label(Some(&project))}
                                    </option>
                                }
                            })
                            .collect_view()
                    }}
                    <option value="" selected=move || workspace.new_project.get().is_none()>"No project"</option>
                    <option value=NEW>"New project…"</option>
                </select>
            </label>
        </Show>
    }
}

/// The project editor: name, emoji, default attachments, default agent and
/// preset, `AGENTS.md`, and the way to the project's memory folder. Save
/// sends one change at the revision the editor read.
pub(super) fn editor_sheet(workspace: Workspace) -> impl IntoView {
    let picking = RwSignal::new(false);
    let agents_open = RwSignal::new(false);
    let errors = RwSignal::new(std::collections::HashMap::new());
    let set = CardSet {
        selection: workspace.editor_selection,
        exposure: workspace.editor_exposure,
        project: true,
        errors,
    };
    let field = move |read: fn(&crate::workspace::ProjectEditor) -> String| {
        move || {
            workspace
                .project_editor
                .with(|editor| editor.as_ref().map(read).unwrap_or_default())
        }
    };
    let existing = move || {
        workspace
            .project_editor
            .with(|editor| editor.as_ref().and_then(|editor| editor.project.clone()))
    };
    let blocker = move || {
        let cards = workspace.cards();
        workspace
            .editor_selection
            .with(|selection| selection.blocker(&cards))
    };
    let saving = move || {
        workspace
            .project_editor
            .with(|editor| editor.as_ref().is_some_and(|editor| editor.saving))
    };
    let session_attachments = move || {
        workspace
            .tree_root
            .get()
            .or(workspace.selected.get())
            .filter(|_| !workspace.new_session.get())
            .and_then(|session| {
                workspace.sessions.with(|sessions| {
                    sessions
                        .iter()
                        .find(|meta| meta.session_id == session)
                        .is_some_and(|meta| !meta.attachments.is_empty())
                        .then_some(())
                })
            })
            .is_some()
    };
    let agent_label = move || {
        workspace.project_editor.with(|editor| {
            let Some(editor) = editor.as_ref() else {
                return String::new();
            };
            match (&editor.agent, &editor.preset) {
                (Some(agent), Some(preset)) => format!("{agent} · {preset}"),
                (Some(agent), None) => agent.to_string(),
                (None, Some(preset)) => format!("preset {preset}"),
                (None, None) => "None: the usual agent".to_owned(),
            }
        })
    };
    view! {
        <div class="flex flex-col gap-4" id="project-editor">
            {move || {
                workspace.project_editor.with(|editor| {
                    let editor = editor.as_ref()?;
                    editor.confirm_close.then(|| view! {
                        <div class="flex flex-col gap-2 rounded-xl p-2 text-sm bg-raised shadow-[0_0_0_1px_var(--color-accent),0_0_0_4px_var(--color-accent-soft)]" role="alertdialog" data-discard-box>
                            <p>"Discard your changes to this project?"</p>
                            <div class="flex flex-wrap gap-2">
                                <button type="button" class=PRIMARY data-discard on:click=move |_| workspace.discard_panel()>
                                    "Discard"
                                </button>
                                <button type="button" class=BUTTON on:click=move |_| workspace.edit_editor(|editor| editor.confirm_close = false)>
                                    "Keep editing"
                                </button>
                            </div>
                        </div>
                    })
                })
            }}
            {move || {
                workspace.project_editor.with(|editor| {
                    let editor = editor.as_ref()?;
                    let error = editor.error.clone()?;
                    let reload = editor
                        .project
                        .as_ref()
                        .filter(|_| editor.conflict)
                        .map(|project| project.id.clone());
                    Some(view! {
                        <div class=format!("{ERROR} flex flex-col gap-2") data-editor-error>
                            <p>{error}</p>
                            {reload.map(|project| view! {
                                <button type="button" class=format!("{BUTTON} self-start") data-editor-reload on:click=move |_| workspace.load_editor(project.clone())>
                                    <Glyph icon=icondata::LuRedo2 />
                                    "Reload"
                                </button>
                            })}
                        </div>
                    })
                })
            }}
            <label class="flex flex-col gap-1 text-sm font-bold">
                "Name"
                <input
                    type="text"
                    id="project-name"
                    class=format!("{FIELD} font-mono font-normal")
                    autocomplete="off"
                    spellcheck="false"
                    placeholder="my-project"
                    prop:value=field(|editor| editor.name.clone())
                    on:input=move |event| {
                        let text = event_target_value(&event);
                        workspace.edit_editor(|editor| editor.name = text);
                    }
                />
                <span class=format!("font-normal {NOTE}")>
                    "Lowercase letters, digits, and -; it names the project's memory folder too."
                </span>
            </label>
            <div class="relative flex flex-col gap-1">
                <span class="text-sm font-bold">"Emoji"</span>
                <div class="flex items-center gap-2">
                    <button
                        type="button"
                        id="project-emoji"
                        class=format!("{BUTTON} min-w-16 text-xl")
                        aria-label="Choose the project's emoji"
                        title="Choose the project's emoji"
                        aria-expanded=move || picking.get().to_string()
                        on:click=move |_| picking.update(|open| *open = !*open)
                    >
                        {move || {
                            workspace.project_editor.with(|editor| {
                                match editor.as_ref().and_then(|editor| editor.emoji.clone()) {
                                    Some(emoji) => view! { <span data-chosen-emoji>{emoji}</span> }.into_any(),
                                    None => view! { <Glyph icon=icondata::LuFolder class="text-base text-accent" /> }.into_any(),
                                }
                            })
                        }}
                        <Glyph icon=icondata::LuChevronDown class="text-xs opacity-70" />
                    </button>
                    <span class=NOTE>"Shown before the name in every client."</span>
                </div>
                <Show when=move || picking.get()>
                    <EmojiPicker workspace=workspace open=picking />
                </Show>
            </div>
            <div class="flex flex-col gap-2">
                <div class="flex flex-wrap items-center gap-2">
                    <span class="min-w-0 flex-1 text-sm font-bold">"New sessions attach"</span>
                    <Show when=session_attachments>
                        <button type="button" class=BUTTON id="editor-use-session" on:click=move |_| workspace.editor_use_session()>
                            <Glyph icon=icondata::LuCopy />
                            "Use this session's attachments"
                        </button>
                    </Show>
                </div>
                <p class=NOTE>"A new session in the project starts with these selected; they stay editable there."</p>
                {move || {
                    if workspace.runtimes.with(Option::is_none) {
                        return super::loading("Reading the runtimes…").into_any();
                    }
                    workspace
                        .card_groups()
                        .into_iter()
                        .map(|group| super::cards::group_view(workspace, group, set))
                        .collect_view()
                        .into_any()
                }}
                {super::tool_groups::cards_skills(workspace, set, "New sessions' skills", "project-skills")}
            </div>
            <div class="flex flex-col gap-1">
                <span class="text-sm font-bold">"Default agent"</span>
                <button
                    type="button"
                    id="project-agent"
                    class=format!("{BUTTON} justify-between")
                    aria-expanded=move || agents_open.get().to_string()
                    on:click=move |_| agents_open.update(|open| *open = !*open)
                >
                    <span class="flex min-w-0 items-center gap-2">
                        <Glyph icon=icondata::LuBot class="text-accent" />
                        <span class="truncate font-mono" data-project-agent>{agent_label}</span>
                    </span>
                    <Glyph icon=icondata::LuChevronDown class="text-xs opacity-70" />
                </button>
                <Show when=move || agents_open.get()>
                    {agent_choices(workspace, agents_open)}
                </Show>
                <span class=NOTE>"A new session in the project starts with this agent and its own model."</span>
            </div>
            <label class="flex flex-col gap-1 text-sm font-bold">
                "AGENTS.md"
                <textarea
                    id="project-agents-md"
                    class=format!("{FIELD} min-h-40 py-2 font-mono font-normal")
                    spellcheck="false"
                    placeholder="Instructions every session of the project reads first."
                    disabled=move || {
                        workspace
                            .project_editor
                            .with(|editor| editor.as_ref().is_some_and(|editor| !editor.agents_md_loaded))
                    }
                    prop:value=field(|editor| editor.agents_md.clone())
                    on:input=move |event| {
                        let text = event_target_value(&event);
                        workspace.edit_editor(|editor| editor.agents_md = text);
                    }
                ></textarea>
                <span class=format!("font-normal {NOTE}")>"Read at every message, before the workspaces' AGENTS.md. Empty removes it."</span>
            </label>
            {move || existing().map(|project| {
                let name = project.name.to_string();
                view! {
                    <button
                        type="button"
                        class=format!("{BUTTON} self-start")
                        id="project-memory"
                        on:click=move |_| {
                            if untrack(|| workspace.editor_dirty()) {
                                workspace.edit_editor(|editor| editor.confirm_close = true);
                                return;
                            }
                            browser::store(&crate::memory::folder_key(&name), "open");
                            workspace.discard_panel();
                            workspace.open_view(View::Memory, None);
                        }
                    >
                        <Glyph icon=icondata::LuBrain />
                        "Open its memory folder"
                    </button>
                }
            })}
            <div class="sticky bottom-0 -mx-3 -mb-3 flex flex-wrap items-center gap-2 border-t border-line bg-surface-2 px-3 py-2">
                <button
                    type="button"
                    id="project-save"
                    class=PRIMARY
                    disabled=move || saving() || blocker().is_some()
                    on:click=move |_| workspace.save_editor()
                >
                    {move || if saving() {
                        view! { <Glyph icon=icondata::LuLoaderCircle class="animate-spin" /> "Saving…" }.into_any()
                    } else if existing().is_some() {
                        view! { <Glyph icon=icondata::LuCheck /> "Save" }.into_any()
                    } else {
                        view! { <Glyph icon=icondata::LuPlus /> "Create" }.into_any()
                    }}
                </button>
                <button type="button" class=BUTTON on:click=move |_| workspace.close_panel()>"Cancel"</button>
                {move || blocker().map(|blocker| view! { <span class=NOTE data-editor-blocker>{blocker}</span> })}
            </div>
        </div>
    }
}

/// The default agent's choices: none, or each agent that can start a
/// session, with its preset.
fn agent_choices(workspace: Workspace, open: RwSignal<bool>) -> impl IntoView {
    let current = workspace.project_editor.with_untracked(|editor| {
        editor
            .as_ref()
            .map(|editor| (editor.agent.clone(), editor.preset.clone()))
            .unwrap_or_default()
    });
    let mut agents = workspace.root_agents();
    agents.sort_by(|left, right| (&left.preset, &left.id).cmp(&(&right.preset, &right.id)));
    let none_chosen = current.0.is_none() && current.1.is_none();
    view! {
        <div class="flex flex-col gap-1 rounded-xl p-1 bg-raised shadow-[0_0_0_1px_var(--color-line),0_1px_2px_rgb(0_0_0/0.03)]" data-agent-choices>
            <button
                type="button"
                class=ROW
                class:bg-sel=none_chosen
                data-agent-choice=""
                on:click=move |_| {
                    workspace.edit_editor(|editor| {
                        editor.agent = None;
                        editor.preset = None;
                    });
                    open.set(false);
                }
            >
                {choice_line("None".into(), none_chosen, "text-sm font-bold")}
                <span class=NOTE>"New sessions start with the usual agent."</span>
            </button>
            {agents.into_iter().map(|agent| {
                let chosen = current.0.as_ref() == Some(&agent.id) && current.1 == agent.preset;
                let label = match &agent.preset {
                    Some(preset) => format!("{} · {preset}", agent.id),
                    None => agent.id.to_string(),
                };
                let (id, preset) = (agent.id.clone(), agent.preset.clone());
                view! {
                    <button
                        type="button"
                        class=ROW
                        class:bg-sel=chosen
                        data-agent-choice=label.clone()
                        on:click=move |_| {
                            let (id, preset) = (id.clone(), preset.clone());
                            workspace.edit_editor(|editor| {
                                editor.agent = Some(id);
                                editor.preset = preset;
                            });
                            open.set(false);
                        }
                    >
                        {choice_line(label.clone(), chosen, "font-mono text-sm font-bold")}
                        <span class=format!("line-clamp-2 {NOTE}")>{agent.description.clone()}</span>
                    </button>
                }
            }).collect_view()}
        </div>
    }
}

/// The emoji picker (`docs/site/specs/projects.md` section 10.3): a search
/// by name over a bundled list by category, a field that takes any one
/// emoji (the system's emoji keyboard included), and "No emoji". Under
/// its button in the editor; a bottom sheet on a phone.
#[component]
fn EmojiPicker(workspace: Workspace, open: RwSignal<bool>) -> impl IntoView {
    let query = RwSignal::new(String::new());
    let typed = RwSignal::new(String::new());
    // The emoji the pointer or the keyboard is on, named below the grid:
    // a native tooltip on each would pop up afresh as the pointer crosses
    // the grid, which lags.
    let pointed = RwSignal::new(None::<(&'static str, &'static str)>);
    let search = NodeRef::<html::Input>::new();
    Effect::new(move |_| {
        if let Some(input) = search.get() {
            // A phone's keyboard would cover the sheet: only a wider
            // window starts in the search box.
            if !workspace.narrow.get_untracked() {
                request_animation_frame(move || {
                    let _ = input.focus();
                });
            }
        }
    });
    let choose = move |emoji: Option<String>| {
        workspace.edit_editor(|editor| editor.emoji = emoji);
        open.set(false);
    };
    let typed_error = move || {
        let text = typed.get();
        let text = text.trim();
        (!text.is_empty())
            .then(|| validate_emoji(text).err())
            .flatten()
    };
    let use_typed = move || {
        let text = typed.get_untracked().trim().to_owned();
        if validate_emoji(&text).is_ok() {
            choose(Some(text));
        }
    };
    view! {
        // A phone's sheet covers the page below a dimmed backdrop.
        <div class="fixed inset-0 z-30 bg-scrim sm:hidden" on:click=move |_| open.set(false)></div>
        <div
            id="emoji-picker"
            role="dialog"
            aria-label="Choose an emoji"
            class="fixed inset-x-0 bottom-0 z-40 flex max-h-[75dvh] flex-col gap-2 rounded-t-2xl border-t border-line bg-surface p-3 shadow-xl sm:static sm:z-auto sm:max-h-96 sm:rounded-lg sm:border sm:shadow-md"
        >
            <span class="mx-auto h-1 w-10 shrink-0 rounded-full bg-line-2 sm:hidden" aria-hidden="true"></span>
            <div class="flex items-center gap-2">
                <input
                    type="search"
                    node_ref=search
                    id="emoji-search"
                    class=FIELD
                    placeholder="Search by name"
                    autocomplete="off"
                    prop:value=move || query.get()
                    on:input=move |event| query.set(event_target_value(&event))
                />
                <button type="button" class=ICON_BUTTON aria-label="Close" title="Close" on:click=move |_| open.set(false)>
                    <Glyph icon=icondata::LuX />
                </button>
            </div>
            <div class="min-h-0 flex-1 overflow-y-auto" data-emoji-grid on:pointerleave=move |_| pointed.set(None)>
                {move || {
                    let found = crate::emoji::search(&query.get());
                    if found.is_empty() {
                        return view! { <p class=format!("py-4 text-center {NOTE}")>"No emoji has that name. Type one below instead."</p> }.into_any();
                    }
                    found
                        .into_iter()
                        .map(|(group, emoji)| view! {
                            <div class="flex flex-col gap-1 pb-2">
                                <span class=format!("sticky top-0 bg-surface py-0.5 font-bold {NOTE}")>{group}</span>
                                <div class="grid grid-cols-[repeat(auto-fill,minmax(2.75rem,1fr))]">
                                    {emoji.into_iter().map(|(emoji, name)| view! {
                                        <button
                                            type="button"
                                            class="inline-flex h-11 w-11 items-center justify-center rounded-md text-2xl hover:bg-hover active:bg-press"
                                            aria-label=name
                                            data-emoji=emoji
                                            on:pointerenter=move |_| pointed.set(Some((emoji, name)))
                                            on:focus=move |_| pointed.set(Some((emoji, name)))
                                            on:click=move |_| choose(Some(emoji.to_owned()))
                                        >
                                            {emoji}
                                        </button>
                                    }).collect_view()}
                                </div>
                            </div>
                        })
                        .collect_view()
                        .into_any()
                }}
            </div>
            <p class=format!("hidden h-5 shrink-0 items-center gap-1.5 truncate sm:flex {NOTE}") data-emoji-pointed>
                {move || pointed.get().map(|(emoji, name)| view! {
                    <span class="text-base">{emoji}</span>
                    <span class="truncate">{name}</span>
                })}
            </p>
            <div class="flex flex-wrap items-start gap-2 border-t border-line pt-2">
                <div class="flex min-w-0 flex-1 flex-col gap-1">
                    <input
                        type="text"
                        id="emoji-typed"
                        class=format!("{FIELD} text-lg")
                        placeholder="Or type or paste one"
                        aria-label="Any one emoji"
                        autocomplete="off"
                        prop:value=move || typed.get()
                        on:input=move |event| typed.set(event_target_value(&event))
                        on:keydown=move |event: ev::KeyboardEvent| {
                            if event.key() == "Enter" {
                                event.prevent_default();
                                use_typed();
                            }
                        }
                    />
                    {move || typed_error().map(|reason| view! {
                        <span class="text-xs text-bad" data-emoji-error>{format!("Not one emoji: {reason}.")}</span>
                    })}
                </div>
                <button
                    type="button"
                    class=BUTTON
                    id="emoji-use"
                    disabled=move || typed.with(|text| validate_emoji(text.trim()).is_err())
                    on:click=move |_| use_typed()
                >
                    "Use"
                </button>
                <button type="button" class=BUTTON id="emoji-none" on:click=move |_| choose(None)>
                    "No emoji"
                </button>
            </div>
        </div>
    }
}
