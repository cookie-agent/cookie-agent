//! Projects in the chat view (`docs/site/specs/projects.md` section 10.1):
//! the sessions sidebar's project sections and their headers, dragging a
//! session onto a header (a mouse drags at once, a finger after a long
//! press), "Move to project…", the project editor with its emoji picker,
//! and the new session's project picker.

use cookie_agent_client_state::{
    badges::{project_badges, project_badges_tooltip},
    projects::{ProjectSection, delete_question, project_label, project_sections, validate_emoji},
};
use cookie_agent_protocol::{ProjectId, ProjectInfo, SessionId};
use leptos::{ev, html, prelude::*};

use super::{BUTTON, ERROR, ICON_BUTTON, NOTE, PRIMARY, ROW, RowActions, choice_line, session_row};
use crate::{
    browser,
    icons::Glyph,
    layout::{Press, View},
    machines::{Drag, DragEnd, DragStep},
    workspace::{CardSet, Workspace},
};

/// How long a finger holds a session row before it lifts to be dragged,
/// or a header before it shows its buttons.
const LONG_PRESS: std::time::Duration = std::time::Duration::from_millis(500);

/// A drop target: a project section's header, named by its
/// `data-project-drop` (the section's key).
const DROP: &str = "[data-project-drop]";

/// A header's button (pencil, trash, `+`): clear at rest, so it shows the
/// header's own colour (hovered or pressed with it), one step darker under
/// the pointer, darker still pressed. Pressing it never presses the header.
const HEADER_ACTION: &str = "inline-flex min-h-11 min-w-11 shrink-0 items-center justify-center rounded-md text-ink hover:bg-crust hover:text-espresso active:bg-[color-mix(in_oklab,var(--color-crust),var(--color-espresso)_18%)] dark:text-sand dark:hover:bg-smoke dark:hover:text-latte dark:active:bg-[color-mix(in_oklab,var(--color-smoke),var(--color-latte)_18%)]";

/// What the pencil and trash lie on over the header's name: the header's
/// hover colour, fading in from their left, and its pressed colour while
/// the name is pressed.
const HEADER_COVER: &str = "pl-6 bg-[linear-gradient(to_right,transparent,var(--color-toasted)_1.5rem)] dark:bg-[linear-gradient(to_right,transparent,var(--color-ember)_1.5rem)] group-has-[[data-section-toggle]:active]:bg-[linear-gradient(to_right,transparent,var(--color-crust)_1.5rem)] dark:group-has-[[data-section-toggle]:active]:bg-[linear-gradient(to_right,transparent,var(--color-smoke)_1.5rem)]";

/// A text field of the editor.
const FIELD: &str = "min-h-11 w-full min-w-0 rounded-md border border-crust bg-cream px-3 text-sm hover:border-caramel focus:border-caramel focus:outline-none dark:border-smoke dark:bg-night dark:hover:border-maple dark:focus:border-maple";

/// The project a section key names: `None` for "No project".
fn project_of_key(key: &str) -> Option<Option<ProjectId>> {
    if key == "none" {
        return Some(None);
    }
    key.strip_prefix("project:")
        .and_then(|id| ProjectId::new(id).ok())
        .map(Some)
}

/// A session row's drag, shared by the rows, the headers, and the window's
/// listeners.
#[derive(Clone, Copy)]
pub(super) struct SessionDrag {
    gesture: StoredValue<Drag>,
    /// The row pressed, until the press ends.
    pressed: StoredValue<Option<SessionId>>,
    /// The row lifted or dragged, which follows the pointer.
    pub(super) dragged: RwSignal<Option<SessionId>>,
    pub(super) offset: RwSignal<(f64, f64)>,
    /// The section whose header is under the pointer.
    over: RwSignal<Option<String>>,
    timer: StoredValue<Option<TimeoutHandle>>,
    /// A drag or a long press just ended: the click that follows does not
    /// open the row.
    swallow_click: StoredValue<bool>,
    /// The list, which scrolls when a drag nears its edge.
    pub(super) list: NodeRef<html::Nav>,
}

impl SessionDrag {
    pub(super) fn new() -> Self {
        Self {
            gesture: StoredValue::new(Drag::default()),
            pressed: StoredValue::new(None),
            dragged: RwSignal::new(None),
            offset: RwSignal::new((0.0, 0.0)),
            over: RwSignal::new(None),
            timer: StoredValue::new(None),
            swallow_click: StoredValue::new(false),
            list: NodeRef::new(),
        }
    }

    fn stop_timer(self) {
        if let Some(timer) = self.timer.get_value() {
            timer.clear();
        }
        self.timer.set_value(None);
    }

    fn reset(self) {
        browser::set_pressing(false);
        self.stop_timer();
        self.gesture.update_value(Drag::cancel);
        self.pressed.set_value(None);
        self.dragged.set(None);
        self.offset.set((0.0, 0.0));
        self.over.set(None);
    }

    /// Whether the click that just came must be ignored; it is ignored
    /// once.
    pub(super) fn take_click(self) -> bool {
        let swallow = self.swallow_click.get_value();
        self.swallow_click.set_value(false);
        swallow
    }

    /// A press on `session_id`'s row: a mouse drags once it moves (only
    /// where there are headers to drop on), a finger after a long press,
    /// which let go in place reveals the row's buttons.
    pub(super) fn press(
        self,
        workspace: Workspace,
        session_id: SessionId,
        event: &ev::PointerEvent,
    ) {
        let touch = event.pointer_type() == "touch";
        if !touch && (event.button() != 0 || !workspace.has_projects.get_untracked()) {
            return;
        }
        let (x, y) = (f64::from(event.client_x()), f64::from(event.client_y()));
        self.stop_timer();
        self.swallow_click.set_value(false);
        self.gesture
            .update_value(|gesture| gesture.press(x, y, touch));
        self.pressed.set_value(Some(session_id));
        if touch {
            browser::set_pressing(true);
            let timer = set_timeout_with_handle(
                move || {
                    if self.gesture.try_update_value(Drag::elapse).unwrap_or(false) {
                        self.dragged.set(Some(session_id));
                    }
                },
                LONG_PRESS,
            )
            .ok();
            self.timer.set_value(timer);
        }
    }

    /// Follows the pointer and lets go of it, on the window, for as long
    /// as the list shows. A long press let go in place reveals the row's
    /// buttons in `actions`; a drop on a header moves the session.
    pub(super) fn follow(self, workspace: Workspace, actions: RowActions) {
        let moved = window_event_listener(ev::pointermove, move |event| {
            let (x, y) = (f64::from(event.client_x()), f64::from(event.client_y()));
            match self
                .gesture
                .try_update_value(|gesture| gesture.moved(x, y))
                .unwrap_or(DragStep::Nothing)
            {
                DragStep::Nothing => {}
                DragStep::Cancelled => {
                    browser::set_pressing(false);
                    self.stop_timer();
                    self.pressed.set_value(None);
                }
                DragStep::Started => {
                    self.stop_timer();
                    browser::set_pressing(true);
                    actions.revealed.set(None);
                    self.dragged.set(self.pressed.get_value());
                    self.over
                        .set(browser::attribute_at(x, y, DROP, "data-project-drop"));
                }
                DragStep::Moved { dx, dy } => {
                    self.offset.set((dx, dy));
                    self.over
                        .set(browser::attribute_at(x, y, DROP, "data-project-drop"));
                    if let Some(list) = self.list.get_untracked() {
                        browser::scroll_element_near_edge(&list, y);
                    }
                }
            }
        });
        let released = window_event_listener(ev::pointerup, move |_| {
            let end = self
                .gesture
                .try_update_value(Drag::release)
                .unwrap_or(DragEnd::Nothing);
            let pressed = self.pressed.get_value();
            let over = self.over.get_untracked();
            self.reset();
            let Some(session_id) = pressed else {
                return;
            };
            // The click a release may bring comes before any timer: past it,
            // a click (the keyboard's, say) opens the row again.
            if end != DragEnd::Nothing {
                self.swallow_click.set_value(true);
                set_timeout(
                    move || {
                        let _ = self.swallow_click.try_set_value(false);
                    },
                    std::time::Duration::ZERO,
                );
            }
            match end {
                DragEnd::Nothing => {}
                DragEnd::Held => {
                    actions.confirming.set(None);
                    actions.revealed.set(Some(session_id));
                }
                DragEnd::Dropped => {
                    if let Some(project) = over.as_deref().and_then(project_of_key) {
                        workspace.move_session(session_id, project);
                    }
                }
            }
        });
        let cancelled = window_event_listener(ev::pointercancel, move |_| self.reset());
        let unselectable = StoredValue::new_local(browser::hold_selection(move || {
            self.pressed
                .try_with_value(Option::is_some)
                .unwrap_or(false)
        }));
        let hold = StoredValue::new_local(browser::hold_touch_scroll(move || {
            self.gesture
                .try_with_value(Drag::holds_scroll)
                .unwrap_or(false)
        }));
        on_cleanup(move || {
            moved.remove();
            released.remove();
            cancelled.remove();
            if let Some(hold) = hold.try_update_value(Option::take).flatten() {
                hold.remove();
            }
            if let Some(hold) = unselectable.try_update_value(Option::take).flatten() {
                hold.remove();
            }
            browser::set_pressing(false);
        });
    }

    /// Whether `session_id`'s row is lifted or dragged.
    pub(super) fn lifts(self, session_id: SessionId) -> bool {
        self.dragged.get() == Some(session_id)
    }
}

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

/// The session list as project sections, then "No project".
pub(super) fn project_list(
    workspace: Workspace,
    now: RwSignal<jiff::Timestamp>,
    actions: RowActions,
    drag: SessionDrag,
    headers: HeaderActions,
) -> impl IntoView {
    let sections = Memo::new(move |_| {
        workspace.projects.with(|projects| {
            workspace
                .sessions
                .with(|sessions| project_sections(projects, sessions))
        })
    });
    move || {
        sections
            .get()
            .into_iter()
            .map(|section| section_view(workspace, section, now, actions, drag, headers))
            .collect_view()
    }
}

/// One section: its header, then its sessions while it is open.
fn section_view(
    workspace: Workspace,
    section: ProjectSection,
    now: RwSignal<jiff::Timestamp>,
    actions: RowActions,
    drag: SessionDrag,
    headers: HeaderActions,
) -> impl IntoView {
    let key = section.key();
    let open = {
        let key = key.clone();
        Memo::new(move |_| workspace.section_open(&key))
    };
    let sessions = section.sessions.clone();
    let empty = sessions.is_empty();
    view! {
        <section class="flex flex-col gap-1" data-project-section=key.clone() data-open=move || open.get().to_string()>
            {project_header(workspace, section.project.clone(), key.clone(), open, drag, headers, sessions.iter().map(|session| session.session_id).collect())}
            <Show when=move || open.get()>
                {
                    let sessions = sessions.clone();
                    move || {
                        if empty {
                            return view! { <p class=format!("px-3 pb-1 {NOTE}") data-section-empty>"No sessions yet."</p> }.into_any();
                        }
                        sessions
                            .clone()
                            .into_iter()
                            .map(|session| session_row(workspace, session, now, actions, drag))
                            .collect_view()
                            .into_any()
                    }
                }
            </Show>
        </section>
    }
}

/// A section's header: a click folds or opens it. It shows the project's
/// emoji (a folder without one) and its name, then its default
/// attachments as badges. `+` starts a session in it; a project's pencil
/// and delete show on hover, or after a long press on a touch screen.
/// Sessions dropped on it move into it.
fn project_header(
    workspace: Workspace,
    project: Option<ProjectInfo>,
    key: String,
    open: Memo<bool>,
    drag: SessionDrag,
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
    let badge_line = NodeRef::<html::Span>::new();
    crate::badges::keep_fitted(badge_line);
    let badges = {
        let defaults = defaults.clone();
        move || {
            let homes = workspace.homes();
            let shown = workspace
                .runtimes
                .with(|listing| project_badges(defaults.as_ref(), listing.as_ref(), &homes));
            crate::badges::fit_soon(badge_line);
            crate::badges::badge_line(shown)
        }
    };
    let has_badges = {
        let defaults = defaults.clone();
        move || {
            let homes = workspace.homes();
            workspace.runtimes.with(|listing| {
                !project_badges(defaults.as_ref(), listing.as_ref(), &homes).is_empty()
            })
        }
    };
    let tooltip = {
        let defaults = defaults.clone();
        let label = label.clone();
        move || {
            let homes = workspace.homes();
            let places = defaults.as_ref().map(|defaults| {
                workspace
                    .runtimes
                    .with(|listing| project_badges_tooltip(defaults, listing.as_ref(), &homes))
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
    // A dragged session from another section can land here.
    let target = {
        let key = key.clone();
        move || {
            drag.dragged
                .get()
                .is_some_and(|dragged| !members.contains(&dragged))
                && drag.over.get().as_deref() == Some(key.as_str())
        }
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
    let ring = target.clone();
    view! {
        <div
            class=move || format!(
                "project-header group relative rounded-md {}",
                if ring() { "bg-toasted ring-2 ring-caramel dark:bg-ember dark:ring-maple" } else { "" },
            )
            data-project-drop=key.clone()
            data-project-header=project_id.as_ref().map_or_else(|| "none".to_owned(), ToString::to_string)
            data-drop-target=move || target().then_some("true")
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
            <button
                type="button"
                class="no-callout flex w-full flex-col items-stretch rounded-md text-left select-none hover:bg-toasted active:bg-crust group-hover:bg-toasted dark:hover:bg-ember dark:active:bg-smoke dark:group-hover:bg-ember"
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
                    // browser's menu.
                    if revealed.get_untracked().as_deref() == Some(menu_key.as_str()) {
                        event.prevent_default();
                    }
                }
                on:click=move |_| {
                    if press.try_update_value(Press::click).unwrap_or(true) {
                        workspace.toggle_section(&toggle_key);
                    }
                }
            >
                // The name's line keeps clear of `+`; the pencil and trash
                // lie over it when they show, without moving it. The
                // badges' line below has the width to itself.
                <span class="flex min-h-11 w-full items-center gap-2 pr-12 pl-2">
                    <span class="inline-flex shrink-0 opacity-70 transition-transform" class:rotate-90=move || open.get()>
                        <Glyph icon=icondata::FaChevronRightSolid class="text-[0.625rem]" />
                    </span>
                    {match emoji.clone() {
                        Some(emoji) => view! { <span class="w-5 shrink-0 text-center text-base leading-none" data-project-emoji>{emoji}</span> }.into_any(),
                        None if editable => view! { <Glyph icon=icondata::FaFolderSolid class="w-5 justify-center text-caramel dark:text-maple" /> }.into_any(),
                        None => view! { <Glyph icon=icondata::FaFolderOpenRegular class="w-5 justify-center opacity-70" /> }.into_any(),
                    }}
                    <span class=move || format!("min-w-0 flex-1 truncate text-sm font-bold {}", if editable { "" } else { "text-ink dark:text-sand" }) data-project-name>
                        {label.clone()}
                    </span>
                </span>
                <Show when=has_badges.clone()>
                    <span class="flex min-w-0 items-center gap-1 overflow-hidden pb-1.5 pr-2 pl-9 text-xs" node_ref=badge_line data-project-badges>
                        {badges.clone()}
                    </span>
                </Show>
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
                        <Glyph icon=icondata::FaPenSolid />
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
                        <Glyph icon=icondata::FaTrashCanRegular />
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
                    <Glyph icon=icondata::FaPlusSolid />
                </button>
            </div>
        </div>
        <Show when=move || confirm_id.is_some() && confirming.get() == confirm_id>
            {
                let question = confirm_project.as_ref().map(delete_question).unwrap_or_default();
                let project = confirm_project.as_ref().map(|project| project.id.clone());
                view! {
                    <div class="mx-2 mb-1 flex flex-col gap-2 rounded-lg border border-caramel bg-cream p-2 text-sm dark:border-maple dark:bg-night" role="alertdialog" data-project-confirm-box>
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
                class:bg-toasted=chosen
                class:dark:bg-ember=chosen
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
        Some(emoji) => view! { <span class="w-5 shrink-0 text-center text-base leading-none">{emoji}</span> }.into_any(),
        None if project => view! { <Glyph icon=icondata::FaFolderSolid class="w-5 justify-center text-caramel dark:text-maple" /> }.into_any(),
        None => view! { <Glyph icon=icondata::FaFolderOpenRegular class="w-5 justify-center opacity-70" /> }.into_any(),
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
            <label class="flex flex-col gap-1 text-xs font-bold text-ink dark:text-sand">
                <span>"Project"</span>
                <select
                    id="new-session-project"
                    node_ref=select
                    class="min-h-11 rounded-md border border-crust bg-cream px-2 text-sm font-normal text-espresso hover:border-caramel dark:border-smoke dark:bg-night dark:text-latte dark:hover:border-maple"
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
                        <div class="flex flex-col gap-2 rounded-lg border border-caramel bg-cream p-2 text-sm dark:border-maple dark:bg-night" role="alertdialog" data-discard-box>
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
                                    <Glyph icon=icondata::FaRotateRightSolid />
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
                                    None => view! { <Glyph icon=icondata::FaFolderSolid class="text-base text-caramel dark:text-maple" /> }.into_any(),
                                }
                            })
                        }}
                        <Glyph icon=icondata::FaChevronDownSolid class="text-xs opacity-70" />
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
                            <Glyph icon=icondata::FaCopyRegular />
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
                        <Glyph icon=icondata::FaRobotSolid class="text-caramel dark:text-maple" />
                        <span class="truncate font-mono" data-project-agent>{agent_label}</span>
                    </span>
                    <Glyph icon=icondata::FaChevronDownSolid class="text-xs opacity-70" />
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
                        <Glyph icon=icondata::FaBrainSolid />
                        "Open its memory folder"
                    </button>
                }
            })}
            <div class="sticky bottom-0 -mx-3 -mb-3 flex flex-wrap items-center gap-2 border-t border-toasted bg-parchment px-3 py-2 dark:border-ember dark:bg-char">
                <button
                    type="button"
                    id="project-save"
                    class=PRIMARY
                    disabled=move || saving() || blocker().is_some()
                    on:click=move |_| workspace.save_editor()
                >
                    {move || if saving() {
                        view! { <Glyph icon=icondata::FaCircleNotchSolid class="animate-spin" /> "Saving…" }.into_any()
                    } else if existing().is_some() {
                        view! { <Glyph icon=icondata::FaCheckSolid /> "Save" }.into_any()
                    } else {
                        view! { <Glyph icon=icondata::FaPlusSolid /> "Create" }.into_any()
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
        <div class="flex flex-col gap-1 rounded-lg border border-toasted p-1 dark:border-ember" data-agent-choices>
            <button
                type="button"
                class=ROW
                class:bg-toasted=none_chosen
                class:dark:bg-ember=none_chosen
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
                        class:bg-toasted=chosen
                        class:dark:bg-ember=chosen
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
        <div class="fixed inset-0 z-30 bg-espresso/30 sm:hidden dark:bg-night/60" on:click=move |_| open.set(false)></div>
        <div
            id="emoji-picker"
            role="dialog"
            aria-label="Choose an emoji"
            class="fixed inset-x-0 bottom-0 z-40 flex max-h-[75dvh] flex-col gap-2 rounded-t-2xl border-t border-toasted bg-cream p-3 shadow-xl sm:static sm:z-auto sm:max-h-96 sm:rounded-lg sm:border sm:shadow-md dark:border-ember dark:bg-night"
        >
            <span class="mx-auto h-1 w-10 shrink-0 rounded-full bg-crust sm:hidden dark:bg-smoke" aria-hidden="true"></span>
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
                    <Glyph icon=icondata::FaXmarkSolid />
                </button>
            </div>
            <div class="min-h-0 flex-1 overflow-y-auto" data-emoji-grid>
                {move || {
                    let found = crate::emoji::search(&query.get());
                    if found.is_empty() {
                        return view! { <p class=format!("py-4 text-center {NOTE}")>"No emoji has that name. Type one below instead."</p> }.into_any();
                    }
                    found
                        .into_iter()
                        .map(|(group, emoji)| view! {
                            <div class="flex flex-col gap-1 pb-2">
                                <span class=format!("sticky top-0 bg-cream py-0.5 font-bold dark:bg-night {NOTE}")>{group}</span>
                                <div class="grid grid-cols-[repeat(auto-fill,minmax(2.75rem,1fr))]">
                                    {emoji.into_iter().map(|(emoji, name)| view! {
                                        <button
                                            type="button"
                                            class="inline-flex h-11 w-11 items-center justify-center rounded-md text-2xl hover:bg-toasted active:bg-crust dark:hover:bg-ember dark:active:bg-smoke"
                                            title=name
                                            aria-label=name
                                            data-emoji=emoji
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
            <div class="flex flex-wrap items-start gap-2 border-t border-toasted pt-2 dark:border-ember">
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
                        <span class="text-xs text-red-800 dark:text-red-300" data-emoji-error>{format!("Not one emoji: {reason}.")}</span>
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
