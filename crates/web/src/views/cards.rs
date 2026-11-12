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
    workspace::{CardSet, DraftPanel, Workspace},
};

const FIELD: &str = "min-h-11 w-full min-w-0 rounded-lg bg-surface px-3 font-mono text-sm hover:shadow-[inset_0_0_0_1px_var(--color-faint)] focus:shadow-[inset_0_0_0_1px_var(--color-accent),0_0_0_3px_var(--color-accent-soft)] focus:outline-none shadow-[inset_0_0_0_1px_var(--color-line-2)]";

/// A card's frame, with its selected look.
fn card_class(selected: bool, offline: bool) -> String {
    let state = if selected {
        "border-accent bg-sunken"
    } else if offline {
        "border-line opacity-60"
    } else {
        "border-line-2 bg-surface hover:border-accent hover:bg-hover"
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
        CardState::Ready => "text-ok",
        CardState::StartsOnUse => "text-accent",
        CardState::Offline => "text-muted",
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
    // The chips' words (docs/site/specs/web-polish.md, P2).
    let runs_on = move || {
        cards.with(|cards| {
            workspace.selection.with(|selection| {
                let selected: Vec<&Card> = cards
                    .iter()
                    .filter(|card| selection.selected(card))
                    .collect();
                crate::cards::runs_on(&selected)
            })
        })
    };
    // The folder chip, only when a coding runtime has a folder: "global
    // only" is said by Runs on.
    let folder = move || {
        workspace
            .places_label()
            .filter(|places| places.contains('/'))
            .map(|places| crate::places::folder_names(&places))
            .unwrap_or_default()
    };
    let skills_off = move || super::tool_groups::cards_skills_off(workspace, workspace.new_cards());
    let panel = move || workspace.draft_panel.get();
    let toggle = move |which: DraftPanel| {
        workspace.draft_panel.update(|open| {
            *open = if *open == Some(which) {
                None
            } else {
                Some(which)
            }
        });
    };
    view! {
        <section
            id="new-session"
            aria-label="New session"
            aria-busy=move || workspace.creating.get().to_string()
            class="mx-auto flex w-full max-w-3xl flex-col gap-3 py-3"
            class:opacity-60=move || workspace.creating.get()
            class:pointer-events-none=move || workspace.creating.get()
        >
            // The composer comes first: a heading over it, and one row of
            // chips for what the session gets, each opening its panel.
            <div class="flex flex-col items-center pt-4 pb-1">
                <h2 class="text-center text-[1.375rem] font-semibold tracking-tight text-fg">"What should cookie work on?"</h2>
            </div>
            <div class="flex flex-wrap items-center justify-center gap-1.5" data-draft-chips>
                <super::projects::NewSessionProject workspace=workspace />
                <button
                    type="button"
                    class=move || chip_class(panel() == Some(DraftPanel::Where))
                    aria-expanded=move || (panel() == Some(DraftPanel::Where)).to_string()
                    aria-controls="draft-where"
                    title="Where the session runs: click for the runtimes"
                    data-new-session-chip="where"
                    on:click=move |_| toggle(DraftPanel::Where)
                >
                    <Glyph icon=icondata::LuHouse class="shrink-0 text-muted" />
                    <span class="min-w-0 truncate" data-runs-on>{runs_on}</span>
                    <Glyph icon=icondata::LuChevronDown class="shrink-0 text-[0.625rem] opacity-50" />
                </button>
                {move || {
                    let folder = folder();
                    (!folder.is_empty()).then(|| view! {
                        <button
                            type="button"
                            class=chip_class(false)
                            title=move || workspace.places_label().unwrap_or_default()
                            data-new-session-chip="folder"
                            on:click=move |_| workspace.draft_panel.set(Some(DraftPanel::Where))
                        >
                            <Glyph icon=icondata::LuFolder class="shrink-0 text-muted" />
                            <span class="min-w-0 truncate font-mono text-xs">{folder}</span>
                        </button>
                    })
                }}
                <button
                    type="button"
                    class=move || chip_class(panel() == Some(DraftPanel::Tools))
                    aria-expanded=move || (panel() == Some(DraftPanel::Tools)).to_string()
                    aria-controls="draft-tools"
                    title="The tools the runtimes bring, and the skills"
                    data-new-session-chip="tools"
                    on:click=move |_| toggle(DraftPanel::Tools)
                >
                    <Glyph icon=icondata::LuWandSparkles class="shrink-0 text-muted" />
                    <span>"Tools & skills"</span>
                    {move || {
                        let off = skills_off();
                        (off > 0).then(|| view! { <span class="text-xs font-semibold text-warn">{format!("{off} off")}</span> })
                    }}
                    <Glyph icon=icondata::LuChevronDown class="shrink-0 text-[0.625rem] opacity-50" />
                </button>
            </div>
            {move || blocker().map(|blocker| view! {
                <p id="start-blocker" class=format!("text-center {NOTE}")>{blocker}</p>
            })}
            <Show when=move || panel() == Some(DraftPanel::Where)>
                <div id="draft-where" class=PANEL>
                    <p class=NOTE>
                        "Pick the runtimes this session uses; a coding runtime needs a folder. With only global, the session hands work on files to subagents."
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
                    <p class="flex items-start gap-2 text-sm" id="draft-places">
                        <Glyph icon=icondata::LuFolderOpen class="mt-0.5 text-accent" />
                        <span class="min-w-0 break-words font-mono" data-draft-places>
                            {move || workspace.places_label().unwrap_or_default()}
                        </span>
                    </p>
                </div>
            </Show>
            <Show when=move || panel() == Some(DraftPanel::Tools)>
                <div id="draft-tools" class=PANEL>
                    {super::tool_groups::draft_figures(workspace, workspace.new_cards())}
                    {super::tool_groups::cards_skills(workspace, workspace.new_cards(), "Skills", "new-session-skills")}
                </div>
            </Show>
        </section>
    }
}

/// A panel the New session's chips open, under them.
const PANEL: &str = "flex flex-col gap-3 rounded-xl bg-raised p-3 shadow-[0_0_0_1px_var(--color-line),0_8px_24px_-12px_rgb(0_0_0/0.18)]";

/// A chip of the New session's row, tinted while its panel is open.
fn chip_class(open: bool) -> &'static str {
    if open {
        "inline-flex h-9 max-w-full min-w-0 items-center gap-1.5 rounded-lg bg-sel px-2.5 text-[0.8125rem] text-fg shadow-[inset_0_0_0_1px_var(--color-accent)] transition-colors"
    } else {
        "inline-flex h-9 max-w-full min-w-0 items-center gap-1.5 rounded-lg bg-surface px-2.5 text-[0.8125rem] text-fg-2 shadow-[inset_0_0_0_1px_var(--color-line-2)] transition-colors hover:bg-hover hover:text-fg"
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
                        <p id="draft-creating" role="status" class="mx-3 mt-2 flex items-center gap-2 text-sm text-accent">
                            <Glyph icon=icondata::LuLoaderCircle class="animate-spin" />
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
                class="flex min-h-11 w-full items-center gap-2 rounded-md px-2 text-left hover:bg-hover active:bg-press"
                aria-expanded=move || shown.get().to_string()
                data-group-toggle=toggle_name
                on:click=move |_| workspace.toggle_group(&header, set)
            >
                <span
                    class="inline-flex transition-transform"
                    class:rotate-90=move || shown.get()
                >
                    <Glyph icon=icondata::LuChevronRight class="text-xs" />
                </span>
                <Glyph icon=if group.daemon { icondata::LuHouse } else { icondata::LuMonitor } class="text-accent" />
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
                        .closest("input, button, a, [data-card-fields], [data-card-tools]")
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
                        icon=Signal::derive(move || if selected.get() { icondata::LuCircleCheck } else { icondata::LuCircle })
                        class="mt-0.5 text-lg text-accent"
                    />
                    <span class="flex min-w-0 flex-1 flex-col">
                        <span class="flex min-w-0 items-center gap-1.5">
                            // Its machine's colour, as on the session rows.
                            {crate::badges::runtime_dot(
                                &card.machine.as_ref().map_or_else(|| card.runtime_id.to_string(), ToString::to_string),
                                card.runtime_id.as_str(),
                            )}
                            <span class="truncate text-sm font-semibold">{card.label()}</span>
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
            {super::tool_groups::card_tools(workspace, card.clone(), set, selected)}
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
                                        <Glyph icon=icondata::LuX />
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
