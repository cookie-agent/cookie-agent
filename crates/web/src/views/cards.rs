//! The new session's runtime cards: grouped by machine, each group
//! folding to a one-line summary; a click anywhere on a card selects it;
//! a selected card that takes a workspace has its fields; Start, pinned at
//! the foot, opens them all. Then the draft shows where it will work.

use leptos::{ev, prelude::*};
use wasm_bindgen::JsCast as _;

use super::{BUTTON, ICON_BUTTON, NOTE, PRIMARY};
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

/// The new session's runtime cards, and Start.
#[component]
pub fn NewSessionCards(workspace: Workspace) -> impl IntoView {
    let groups = Memo::new(move |_| workspace.card_groups());
    let cards = Memo::new(move |_| workspace.cards());
    let blocker = move || {
        workspace
            .selection
            .with(|selection| cards.with(|cards| selection.blocker(cards)))
    };
    let note = move || {
        workspace
            .selection
            .with(|selection| cards.with(|cards| selection.prefix_note(cards)))
    };
    // The first empty workspace field takes the focus, once the cards show.
    let focused = StoredValue::new(false);
    Effect::new(move |_| {
        if groups.with(Vec::is_empty) || focused.get_value() {
            return;
        }
        focused.set_value(true);
        request_animation_frame(|| {
            if let Some(field) = web_sys::window()
                .and_then(|window| window.document())
                .and_then(|document| {
                    document
                        .query_selector("#new-session [data-workspace-field]:placeholder-shown")
                        .ok()
                        .flatten()
                })
                .and_then(|field| field.dyn_into::<web_sys::HtmlInputElement>().ok())
            {
                let _ = field.focus();
            }
        });
    });
    view! {
        <section id="new-session" aria-label="New session" class="flex min-h-0 flex-1 flex-col">
            <div class="min-h-0 flex-1 overflow-y-auto px-3 py-3">
                <div class="mx-auto flex w-full max-w-3xl flex-col gap-3">
                    // The title bar names the view: no heading here.
                    <super::projects::NewSessionProject workspace=workspace />
                    <p class=NOTE>
                        "Pick what the new session works with: click a card to select it, and give a coding runtime its workspace. With no coding runtime, the session has no workspace."
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
                </div>
            </div>
            <div class="shrink-0 border-t border-toasted bg-parchment px-3 py-2 dark:border-ember dark:bg-char" data-start-bar>
                <div class="mx-auto flex w-full max-w-3xl flex-wrap items-center gap-x-3 gap-y-1">
                    <div class="flex min-w-0 flex-1 flex-col gap-0.5">
                        {move || note().map(|note| view! {
                            <p id="prefix-note" class="flex items-start gap-1.5 text-xs">
                                <Glyph icon=icondata::FaCircleInfoSolid class="mt-0.5 text-caramel dark:text-maple" />
                                <span>{note}</span>
                            </p>
                        })}
                        {move || blocker().map(|blocker| view! {
                            <p id="start-blocker" class=NOTE>{blocker}</p>
                        })}
                    </div>
                    <button
                        type="button"
                        id="start-session"
                        class=PRIMARY
                        disabled=move || blocker().is_some() || workspace.starting.get()
                        on:click=move |_| workspace.start_cards()
                    >
                        {move || if workspace.starting.get() {
                            view! { <Glyph icon=icondata::FaCircleNotchSolid class="animate-spin" /> "Opening…" }.into_any()
                        } else {
                            view! { <Glyph icon=icondata::FaPlaySolid /> "Start" }.into_any()
                        }}
                    </button>
                </div>
            </div>
        </section>
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
                    let error = set
                        .errors
                        .with(|errors| errors.get(&(card.key(), index)).cloned());
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
    let added = card.clone();
    view! {
        <div class="flex flex-col gap-2" data-card-fields>
            {rows}
            <datalist id=list>
                {suggestions.into_iter().map(|suggestion| view! { <option value=suggestion></option> }).collect_view()}
            </datalist>
            <button
                type="button"
                class=format!("{BUTTON} self-start")
                data-add-workspace
                on:click=move |_| set.selection.update(|selection| selection.add_workspace(&added))
            >
                <Glyph icon=icondata::FaPlusSolid />
                "another workspace"
            </button>
        </div>
    }
}

/// The draft's line above the transcript: where the new session works,
/// and the way back to its cards.
#[component]
pub fn DraftPlaces(workspace: Workspace) -> impl IntoView {
    view! {
        <div class="mx-auto my-4 flex max-w-xl flex-col gap-2 rounded-lg border border-toasted bg-parchment p-4 dark:border-ember dark:bg-char" id="draft-places">
            <h2 class="flex items-center gap-2 text-lg font-bold">
                <Glyph icon=icondata::FaPlusSolid class="text-caramel dark:text-maple" />
                "New session"
            </h2>
            <p class="flex items-start gap-2 text-sm">
                <Glyph icon=icondata::FaFolderOpenSolid class="mt-0.5 text-caramel dark:text-maple" />
                <span class="min-w-0 break-words font-mono" data-draft-places>
                    {move || workspace.places_label().unwrap_or_default()}
                </span>
            </p>
            <p class=NOTE>
                "The first message creates the session with these runtimes and the agent and model below."
            </p>
            <button type="button" class=format!("{BUTTON} self-start") id="edit-cards" on:click=move |_| workspace.edit_cards()>
                <Glyph icon=icondata::FaPenToSquareSolid />
                "Change runtimes"
            </button>
        </div>
    }
}
