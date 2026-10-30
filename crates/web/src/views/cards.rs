//! The new session's runtime cards, above its composer: grouped by
//! machine, each group folding to a one-line summary; a click anywhere on a
//! card selects it; a selected card that takes a workspace has its fields;
//! below them, where the session will work. The first message creates the
//! session with them, and a runtime that refuses says why on its card.

use leptos::{ev, prelude::*};
use wasm_bindgen::JsCast as _;

use super::{ICON_BUTTON, NOTE};
use crate::{
    cards::{Card, CardState, Group},
    icons::Glyph,
    workspace::{CardSet, Workspace},
};

const FIELD: &str = "min-h-11 w-full min-w-0 rounded-md border border-crust bg-cream px-3 font-mono text-sm hover:border-caramel focus:border-caramel focus:outline-none dark:border-smoke dark:bg-night dark:hover:border-maple dark:focus:border-maple";

/// A card's frame, with its selected look.
fn card_class(selected: bool, offline: bool) -> String {
    let state = if selected {
        "border-caramel bg-toasted dark:border-maple dark:bg-ember"
    } else if offline {
        "border-toasted opacity-60 dark:border-ember"
    } else {
        "border-crust bg-cream hover:border-caramel hover:bg-parchment dark:border-smoke dark:bg-night dark:hover:border-maple dark:hover:bg-char"
    };
    let cursor = if offline && !selected {
        "cursor-not-allowed"
    } else {
        "cursor-pointer"
    };
    format!("runtime-card flex min-w-0 flex-col gap-2 rounded-lg border-2 p-3 {state} {cursor}")
}

fn state_class(state: CardState) -> &'static str {
    match state {
        CardState::Ready => "text-green-700 dark:text-green-400",
        CardState::StartsOnUse => "text-caramel dark:text-maple",
        CardState::Offline => "text-ink dark:text-sand",
    }
}

/// The new session's runtime cards, above the composer whose first message
/// creates the session with them.
#[component]
pub fn NewSessionCards(workspace: Workspace) -> impl IntoView {
    let groups = Memo::new(move |_| workspace.card_groups());
    let cards = Memo::new(move |_| workspace.cards());
    let blocker = move || {
        workspace
            .selection
            .with(|selection| cards.with(|cards| selection.blocker(cards)))
    };
    // Changing the cards answers why the last message did not create the
    // session.
    Effect::new(move |previous: Option<()>| {
        workspace.selection.track();
        if previous.is_some() {
            workspace.draft_error.set(None);
        }
    });
    view! {
        <section
            id="new-session"
            aria-label="New session"
            aria-busy=move || workspace.creating.get().to_string()
            class="mx-auto flex w-full max-w-3xl flex-col gap-3 py-3"
            class:opacity-60=move || workspace.creating.get()
            class:pointer-events-none=move || workspace.creating.get()
        >
            // The title bar names the view: no heading here.
            <super::projects::NewSessionProject workspace=workspace />
            <p class=NOTE>
                "Pick where the new session works: click a card to select it, and give a coding runtime its workspace. With only global, the session has no workspace: it hands work that needs files or commands to subagents on the runtimes it names. The first message creates the session, starting the runtimes that are not running."
            </p>
            {move || {
                if workspace.runtimes.with(Option::is_none) {
                    return super::loading("Reading the runtimes…").into_any();
                }
                groups
                    .get()
                    .into_iter()
                    .map(|group| group_view(workspace, group, workspace.new_cards()))
                    .collect_view()
                    .into_any()
            }}
            <div class="flex flex-col gap-1">
                {move || blocker().map(|blocker| view! {
                    <p id="start-blocker" class=NOTE>{blocker}</p>
                })}
                <p class="flex items-start gap-2 text-sm" id="draft-places">
                    <Glyph icon=icondata::FaFolderOpenSolid class="mt-0.5 text-caramel dark:text-maple" />
                    <span class="min-w-0 break-words font-mono" data-draft-places>
                        {move || workspace.places_label().unwrap_or_default()}
                    </span>
                </p>
            </div>
        </section>
    }
}

/// Above the composer of a new session: that the first message is creating
/// it, or why it did not.
#[component]
pub fn DraftNotice(workspace: Workspace) -> impl IntoView {
    view! {
        <Show when=move || workspace.new_session.get()>
            {move || {
                if workspace.creating.get() {
                    Some(view! {
                        <p id="draft-creating" role="status" class="mx-3 mt-2 flex items-center gap-2 text-sm text-caramel dark:text-maple">
                            <Glyph icon=icondata::FaCircleNotchSolid class="animate-spin" />
                            "Creating the session…"
                        </p>
                    }.into_any())
                } else {
                    workspace.draft_error.get().map(|error| view! {
                        <p id="draft-error" role="alert" class=format!("{} mx-3 mt-2", super::ERROR)>{error}</p>
                    }.into_any())
                }
            }}
        </Show>
    }
}

/// A machine's cards under a header that folds them, selected in `set`.
pub(super) fn group_view(workspace: Workspace, group: Group, set: CardSet) -> impl IntoView {
    let name = group.name();
    let toggle_name = name.clone();
    let header = group.clone();
    let shown = {
        let group = group.clone();
        Memo::new(move |_| workspace.group_shown(&group, set))
    };
    let summary = {
        let group = group.clone();
        move || set.selection.with(|selection| group.summary(selection))
    };
    let cards = group.cards.clone();
    view! {
        // Two cards a row only where the cards' own container is wide (the
        // new session's page), one in a narrow one (the project editor).
        <div class="@container flex flex-col gap-2" data-group=name data-open=move || shown.get().to_string()>
            <button
                type="button"
                class="flex min-h-11 w-full items-center gap-2 rounded-md px-2 text-left hover:bg-toasted active:bg-crust dark:hover:bg-ember dark:active:bg-smoke"
                aria-expanded=move || shown.get().to_string()
                data-group-toggle=toggle_name
                on:click=move |_| workspace.toggle_group(&header, set)
            >
                <span
                    class="inline-flex transition-transform"
                    class:rotate-90=move || shown.get()
                >
                    <Glyph icon=icondata::FaChevronRightSolid class="text-xs" />
                </span>
                <Glyph icon=if group.daemon { icondata::FaHouseSolid } else { icondata::FaComputerSolid } class="text-caramel dark:text-maple" />
                <span class="min-w-0 flex-1 truncate text-sm" data-group-summary>{summary}</span>
            </button>
            <Show when=move || shown.get()>
                <div class="grid grid-cols-1 items-start gap-2 @lg:grid-cols-2">
                    {cards.clone().into_iter().map(|card| card_view(workspace, card, set)).collect_view()}
                </div>
            </Show>
        </div>
    }
}

/// One card: a click anywhere selects it, except in its fields and
/// buttons.
fn card_view(workspace: Workspace, card: Card, set: CardSet) -> impl IntoView {
    let key = card.key();
    let selected = {
        let card = card.clone();
        Memo::new(move |_| set.selection.with(|selection| selection.selected(&card)))
    };
    let offline = card.state == CardState::Offline;
    let on_click = {
        let card = card.clone();
        move |event: ev::MouseEvent| {
            let inside_control = event
                .target()
                .and_then(|target| target.dyn_into::<web_sys::Element>().ok())
                .and_then(|target| {
                    target
                        .closest("input, button, a, [data-card-fields]")
                        .ok()
                        .flatten()
                })
                .is_some();
            if !inside_control {
                workspace.toggle_card(&card, set);
            }
        }
    };
    let toggle = card.clone();
    let fields = card.clone();
    let label = card.label();
    view! {
        <div
            class=move || card_class(selected.get(), offline)
            data-card=key.clone()
            data-state=card.state.label()
            data-selected=move || selected.get().to_string()
            on:click=on_click
        >
            <div class="flex items-start gap-2">
                <button
                    type="button"
                    class="flex min-h-11 min-w-0 flex-1 items-start gap-2 rounded-md text-left disabled:cursor-not-allowed"
                    aria-pressed=move || selected.get().to_string()
                    aria-label=format!("{label}: select")
                    disabled=move || offline && !selected.get()
                    data-card-toggle
                    on:click=move |_| workspace.toggle_card(&toggle, set)
                >
                    <Glyph
                        icon=Signal::derive(move || if selected.get() { icondata::FaCircleCheckSolid } else { icondata::FaCircleRegular })
                        class="mt-0.5 text-lg text-caramel dark:text-maple"
                    />
                    <span class="flex min-w-0 flex-1 flex-col">
                        <span class="flex min-w-0 items-center gap-1.5">
                            // Its machine's colour, as on the session rows.
                            {crate::badges::runtime_dot(
                                &card.machine.as_ref().map_or_else(|| card.runtime_id.to_string(), ToString::to_string),
                                card.runtime_id.as_str(),
                            )}
                            <span class="truncate font-mono text-sm font-bold">{card.label()}</span>
                        </span>
                        {(!card.description.is_empty()).then(|| view! {
                            <span class=format!("line-clamp-2 {NOTE}")>{card.description.clone()}</span>
                        })}
                    </span>
                </button>
                <span class=format!("mt-1 shrink-0 text-xs font-bold {}", state_class(card.state)) data-card-state>
                    {card.state.label()}
                </span>
            </div>
            {card.message.clone().filter(|_| offline).map(|message| view! {
                <p class=format!("break-words {NOTE}")>{message}</p>
            })}
            {move || {
                (selected.get() && fields.takes_workspace)
                    .then(|| workspace_fields(workspace, fields.clone(), set))
            }}
            {
                let key = key.clone();
                move || {
                    // A refusal of a card without a workspace field.
                    (!card.takes_workspace)
                        .then(|| set.errors.with(|errors| errors.get(&(key.clone(), 0)).cloned()))
                        .flatten()
                        .map(|error| view! { <p class=super::ERROR data-card-error>{error}</p> })
                }
            }
        </div>
    }
}

/// A selected card's workspace fields, their suggestions, and "+ another
/// workspace".
fn workspace_fields(workspace: Workspace, card: Card, set: CardSet) -> impl IntoView {
    let list = format!("workspaces-{}", card.runtime_id);
    let suggestions = workspace.workspace_suggestions(&card.runtime_id);
    let rows = {
        let card = card.clone();
        let list = list.clone();
        move || {
            let workspaces = set.selection.with(|selection| selection.workspaces(&card));
            let removable = workspaces.len() > 1;
            workspaces
                .into_iter()
                .enumerate()
                .map(|(index, text)| {
                    let edited = card.clone();
                    let removed = card.clone();
                    // A runtime that started while the session was being
                    // created has a card per capability now (`alt` became
                    // `alt/coding`): the error its first card got shows here.
                    let error = set.errors.with(|errors| {
                        errors
                            .get(&(card.key(), index))
                            .or_else(|| errors.get(&(card.runtime_id.to_string(), index)))
                            .cloned()
                    });
                    view! {
                        <div class="flex flex-col gap-1">
                            <div class="flex items-center gap-1">
                                <input
                                    type="text"
                                    class=FIELD
                                    list=list.clone()
                                    placeholder="~/project"
                                    autocomplete="off"
                                    spellcheck="false"
                                    aria-label=format!("Workspace {} of {}", index + 1, card.label())
                                    data-workspace-field=index.to_string()
                                    prop:value=text
                                    on:input=move |event| {
                                        let text = event_target_value(&event);
                                        set.selection.update(|selection| {
                                            selection.set_workspace(&edited, index, text);
                                        });
                                    }
                                />
                                {removable.then(|| view! {
                                    <button
                                        type="button"
                                        class=ICON_BUTTON
                                        aria-label="Remove this workspace"
                                        title="Remove this workspace"
                                        data-remove-workspace=index.to_string()
                                        on:click=move |_| set.selection.update(|selection| {
                                            selection.remove_workspace(&removed, index);
                                        })
                                    >
                                        <Glyph icon=icondata::FaXmarkSolid />
                                    </button>
                                })}
                            </div>
                            {error.map(|error| view! { <p class=super::ERROR data-card-error>{error}</p> })}
                        </div>
                    }
                })
                .collect_view()
        }
    };
    // One workspace a card: one machine offers each tool once, so a second
    // directory there is reached by absolute path.
    view! {
        <div class="flex flex-col gap-2" data-card-fields>
            {rows}
            <datalist id=list>
                {suggestions.into_iter().map(|suggestion| view! { <option value=suggestion></option> }).collect_view()}
            </datalist>
        </div>
    }
}
