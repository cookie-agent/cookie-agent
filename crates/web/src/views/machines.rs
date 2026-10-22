//! The Machines & runtimes view: each machine as a card with its runtimes,
//! the runtimes the daemon has placed on none, and the ways to add one.
//! A runtime moves to another machine by dragging it onto that machine's
//! card (a mouse drags at once; a finger presses for half a second
//! first, and a press let go in place opens "Move to…"), or with its
//! "Move to…" button, which the keyboard reaches. A machine lists the
//! offline runtimes it remembers under its connected ones, dimmed, with
//! when they were last seen and a Forget. A runtime that hosts MCP servers
//! counts them by state. [`crate::machines`] holds the rules.

use std::time::Duration;

use cookie_agent_client_state::{
    mcp::{runtime_servers, servers_badge},
    runtimes::{
        MachineAction, MachineEntry, machine_action_request, machine_entries, machine_form_title,
        machine_name_request,
    },
};
use cookie_agent_protocol::{
    MachineName, MachinesListResult, McpServerInfo, RememberedRuntime, RuntimeId, RuntimeListing,
    RuntimesListResult,
};
use leptos::{ev, prelude::*, task::spawn_local};
use wasm_bindgen::JsCast as _;

use super::{
    BUTTON, ERROR, ICON_BUTTON, NOTE, PRIMARY, loading,
    manage::{CONFIRM, FIELD, STATE, field_value, poll_while_open},
};
use crate::{
    browser,
    icons::Glyph,
    machines::{
        Drag, DragEnd, DragStep, GUIDES, SshTemplate, dial_in_command, drop_request, lifetime_icon,
        lifetime_label, masked_token, move_request, platform_icon,
    },
    workspace::Workspace,
};

/// How long a finger holds a runtime before it lifts to be dragged.
const LONG_PRESS: Duration = Duration::from_millis(500);

/// A drop target: a machine's card, named by its `data-machine-drop`.
const DROP: &str = "[data-machine-drop]";

/// A runtime as its row shows it.
#[derive(Clone, Debug, PartialEq)]
struct RuntimeCard {
    id: RuntimeId,
    /// The machine it is on; `None` for one the daemon placed on none.
    machine: Option<MachineName>,
    pinned: bool,
    connected: bool,
    listing: Option<RuntimeListing>,
    /// What the machine remembers of it while it is offline.
    remembered: Option<RememberedRuntime>,
    /// Its MCP servers counted by state, when it has any.
    mcp: Option<String>,
    /// Its MCP servers: its own (runtime-wide, or waiting for a workspace)
    /// and those its instances host.
    servers: Vec<McpServerInfo>,
}

/// The open form: a machine's rename or merge, or a runtime's "Move to…".
#[derive(Clone, Debug, PartialEq)]
enum Form {
    Machine(MachineAction, MachineEntry),
    Move(Box<RuntimeCard>),
}

/// A drag under way, shared by the rows and the window's listeners.
#[derive(Clone, Copy)]
struct Dragging {
    gesture: StoredValue<Drag>,
    /// The row pressed, until the press ends.
    pressed: StoredValue<Option<RuntimeCard>>,
    /// The row lifted or dragged, which follows the pointer.
    dragged: RwSignal<Option<RuntimeId>>,
    offset: RwSignal<(f64, f64)>,
    /// The machine under the pointer.
    over: RwSignal<Option<String>>,
    timer: StoredValue<Option<TimeoutHandle>>,
}

impl Dragging {
    fn new() -> Self {
        Self {
            gesture: StoredValue::new(Drag::default()),
            pressed: StoredValue::new(None),
            dragged: RwSignal::new(None),
            offset: RwSignal::new((0.0, 0.0)),
            over: RwSignal::new(None),
            timer: StoredValue::new(None),
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

    /// Follows the pointer and lets go of it, on the window, for as long
    /// as the view shows.
    fn follow(self, workspace: Workspace, form: RwSignal<Option<Form>>) {
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
                    form.set(None);
                    self.dragged.set(
                        self.pressed
                            .with_value(|pressed| pressed.as_ref().map(|card| card.id.clone())),
                    );
                    self.over
                        .set(browser::attribute_at(x, y, DROP, "data-machine-drop"));
                }
                DragStep::Moved { dx, dy } => {
                    self.offset.set((dx, dy));
                    self.over
                        .set(browser::attribute_at(x, y, DROP, "data-machine-drop"));
                    browser::scroll_near_edge("#page [data-page-body]", y);
                }
            }
        });
        let released = window_event_listener(ev::pointerup, move |_| {
            let end = self
                .gesture
                .try_update_value(Drag::release)
                .unwrap_or(DragEnd::Nothing);
            let card = self.pressed.get_value();
            let over = self.over.get_untracked();
            self.reset();
            let Some(card) = card else {
                return;
            };
            match end {
                DragEnd::Nothing => {}
                DragEnd::Held => form.set(Some(Form::Move(Box::new(card)))),
                DragEnd::Dropped => {
                    if let Some(request) =
                        drop_request(&card.id, card.machine.as_ref(), over.as_deref())
                    {
                        workspace.change_machines(request);
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
}

/// The daemon's machines and its runtimes: the Machines & runtimes view.
pub fn machines_page(workspace: Workspace) -> impl IntoView {
    poll_while_open(move || workspace.poll_runtimes());
    let form = RwSignal::new(None::<Form>);
    let adding = RwSignal::new(false);
    let dragging = Dragging::new();
    dragging.follow(workspace, form);
    let machines = Memo::new(move |_| workspace.machines.get());
    let registry = Memo::new(move |_| workspace.runtimes.get());
    let mcp = Memo::new(move |_| workspace.mcp_servers.get().and_then(Result::ok));
    view! {
        <div class="flex flex-wrap items-center gap-2">
            <h2 class="min-w-0 flex-1 text-sm font-bold">"Machines"</h2>
            <button
                type="button"
                id="machines-add"
                class=PRIMARY
                aria-expanded=move || adding.get().to_string()
                on:click=move |_| {
                    if !adding.get_untracked() {
                        workspace.poll_config();
                    }
                    adding.update(|adding| *adding = !*adding);
                }
            >
                <Glyph icon=Signal::derive(move || if adding.get() { icondata::FaMinusSolid } else { icondata::FaPlusSolid }) />
                "Add a runtime"
            </button>
        </div>
        <p class=NOTE>
            "Drag a runtime onto another machine to move it there (on a touch screen, hold it first), or use its Move to… button."
        </p>
        <Show when=move || adding.get()>
            {add_runtime(workspace, adding)}
        </Show>
        {move || {
            // The view reads both lists every second: only a change
            // renders the cards again, so an open form keeps its text.
            let machines = match machines.get() {
                None => return loading("Reading machines…").into_any(),
                Some(Err(error)) => return view! { <p class=ERROR>{error}</p> }.into_any(),
                Some(Ok(listing)) => listing,
            };
            let registry = registry.get();
            let mcp = mcp.get().unwrap_or_default();
            machine_cards(workspace, &machines, registry.as_ref(), &mcp, form, dragging).into_any()
        }}
    }
}

/// Each machine's card, then the runtimes on none.
fn machine_cards(
    workspace: Workspace,
    machines: &MachinesListResult,
    registry: Option<&RuntimesListResult>,
    mcp: &[McpServerInfo],
    form: RwSignal<Option<Form>>,
    dragging: Dragging,
) -> impl IntoView {
    let mcp_badge = |id: &RuntimeId| servers_badge(&runtime_servers(mcp, id));
    let mcp_servers = |id: &RuntimeId| runtime_servers(mcp, id);
    let listing = |id: &RuntimeId| {
        registry.and_then(|registry| {
            registry
                .runtimes
                .iter()
                .find(|runtime| runtime.runtime_id == *id)
                .cloned()
        })
    };
    let mut groups: Vec<(MachineName, Vec<RuntimeCard>)> = Vec::new();
    for entry in machine_entries(machines) {
        match entry {
            MachineEntry::Machine(name) => groups.push((name, Vec::new())),
            MachineEntry::Runtime {
                machine,
                runtime,
                pinned,
                ..
            } => {
                let listed = listing(&runtime);
                let remembered = machines
                    .machines
                    .iter()
                    .find(|info| info.name == machine)
                    .and_then(|info| {
                        info.remembered
                            .iter()
                            .find(|remembered| remembered.runtime_id == runtime)
                            .cloned()
                    });
                // Remembered offline runtimes carry an assignment too, so
                // only the machine's connected list stands in for a missing
                // registry row: a dial-in runtime has none until it
                // connects to this daemon.
                let on_machine = machines.machines.iter().any(|info| {
                    info.name == machine
                        && info
                            .connected
                            .iter()
                            .any(|connected| connected.runtime_id == runtime)
                });
                let card = RuntimeCard {
                    mcp: mcp_badge(&runtime),
                    servers: mcp_servers(&runtime),
                    remembered,
                    connected: listed
                        .as_ref()
                        .map_or(on_machine, |listed| listed.connected),
                    listing: listed,
                    id: runtime,
                    machine: Some(machine),
                    pinned,
                };
                if let Some((_, cards)) = groups.last_mut() {
                    cards.push(card);
                }
            }
        }
    }
    let placed = |id: &RuntimeId| {
        groups
            .iter()
            .any(|(_, cards)| cards.iter().any(|card| card.id == *id))
    };
    let unplaced = registry
        .map(|registry| {
            registry
                .runtimes
                .iter()
                .filter(|runtime| !placed(&runtime.runtime_id))
                .map(|runtime| RuntimeCard {
                    mcp: mcp_badge(&runtime.runtime_id),
                    servers: mcp_servers(&runtime.runtime_id),
                    id: runtime.runtime_id.clone(),
                    machine: None,
                    pinned: false,
                    connected: runtime.connected,
                    listing: Some(runtime.clone()),
                    remembered: None,
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let names = groups
        .iter()
        .map(|(name, _)| name.clone())
        .collect::<Vec<_>>();
    let cards = groups
        .into_iter()
        .map(|(name, runtimes)| {
            let info = machines
                .machines
                .iter()
                .find(|machine| machine.name == name)
                .cloned();
            machine_card(
                workspace,
                name,
                info,
                runtimes,
                names.clone(),
                form,
                dragging,
            )
        })
        .collect_view();
    view! {
        {cards}
        {(!unplaced.is_empty()).then(|| {
            let rows = unplaced
                .into_iter()
                .map(|card| runtime_row(workspace, card, names.clone(), form, dragging))
                .collect_view();
            view! {
                <section class="rounded-xl border border-toasted dark:border-ember" data-unplaced>
                    <div class="flex items-center gap-3 border-b border-toasted px-3 py-2 dark:border-ember">
                        <span class="inline-flex h-9 w-9 shrink-0 items-center justify-center rounded-lg bg-toasted dark:bg-ember">
                            <Glyph icon=icondata::FaCircleQuestionRegular class="text-lg" />
                        </span>
                        <div class="min-w-0 flex-1">
                            <p class="text-sm font-bold">"On no machine yet"</p>
                            <p class=NOTE>"Runtimes the daemon cannot place: drag one onto a machine to pin it there."</p>
                        </div>
                    </div>
                    <ul class="flex flex-col">{rows}</ul>
                </section>
            }
        })}
    }
}

/// A machine's card: its operating system's icon, its name, what it
/// holds, its rename, merge, and forget, and its runtimes. While a runtime
/// is dragged, the card takes it.
fn machine_card(
    workspace: Workspace,
    name: MachineName,
    info: Option<cookie_agent_protocol::MachineInfo>,
    runtimes: Vec<RuntimeCard>,
    names: Vec<MachineName>,
    form: RwSignal<Option<Form>>,
    dragging: Dragging,
) -> impl IntoView {
    let platform = info.as_ref().and_then(|info| info.platform.clone());
    let memories = info.as_ref().map_or(0, |info| info.memories);
    let detail = [
        platform.clone(),
        Some(match runtimes.len() {
            1 => "1 runtime".to_owned(),
            count => format!("{count} runtimes"),
        }),
        Some(if memories == 1 {
            "1 memory".to_owned()
        } else {
            format!("{memories} memories")
        }),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" · ");
    let entry = MachineEntry::Machine(name.clone());
    let act = move |entry: MachineEntry, action: MachineAction| match machine_action_request(
        &entry, action,
    ) {
        Ok(Some(request)) => workspace.change_machines(request),
        Ok(None) => form.set(Some(Form::Machine(action, entry))),
        Err(error) => workspace.status.set(error),
    };
    let actions = entry
        .actions()
        .into_iter()
        .map(|action| {
            let entry = entry.clone();
            let (icon, label) = match action {
                MachineAction::Rename => (icondata::FaPenSolid, "Rename the machine"),
                MachineAction::Merge => (icondata::FaCodeMergeSolid, "Merge into another machine"),
                _ => (icondata::FaTrashCanRegular, "Forget the machine"),
            };
            view! {
                <button
                    type="button"
                    class=ICON_BUTTON
                    aria-label=label
                    title=label
                    data-machine-action=action.label()
                    on:click=move |_| act(entry.clone(), action)
                >
                    <Glyph icon=icon />
                </button>
            }
        })
        .collect_view();
    let key = name.to_string();
    let over = {
        let key = key.clone();
        move || dragging.over.get().as_deref() == Some(key.as_str())
    };
    let over_dark = over.clone();
    let over_ring = over.clone();
    // Another machine's runtime is dragged: this card can take it.
    let target = {
        let key = key.clone();
        let runtimes = runtimes
            .iter()
            .map(|card| card.id.clone())
            .collect::<Vec<_>>();
        move || {
            dragging
                .dragged
                .with(|dragged| dragged.as_ref().is_some_and(|id| !runtimes.contains(id)))
                && dragging.over.get().as_deref() != Some(key.as_str())
        }
    };
    let target_dark = target.clone();
    let empty = runtimes.is_empty();
    let rows = runtimes
        .into_iter()
        .map(|card| runtime_row(workspace, card, names.clone(), form, dragging))
        .collect_view();
    let form_entry = entry.clone();
    view! {
        <section
            class="machine-card rounded-xl border border-toasted bg-parchment transition-colors dark:border-ember dark:bg-char"
            class:ring-2=over_ring
            class:ring-caramel=over.clone()
            class:dark:ring-maple=over_dark
            class:border-dashed=target.clone()
            class:border-caramel=target
            class:dark:border-maple=target_dark
            data-machine-drop=key.clone()
        >
            <div class="machine-row flex items-center gap-3 border-b border-toasted py-1 pr-1 pl-3 dark:border-ember" data-machine-entry=key.clone()>
                <span class="inline-flex h-9 w-9 shrink-0 items-center justify-center rounded-lg bg-toasted dark:bg-ember" title=platform.clone().unwrap_or_else(|| "operating system not reported".into())>
                    <Glyph icon=platform_icon(platform.as_deref()) class="text-lg" />
                </span>
                <div class="min-w-0 flex-1 py-1">
                    <p class="truncate font-mono text-sm font-bold">{key.clone()}</p>
                    <p class=NOTE>{detail}</p>
                </div>
                {actions}
            </div>
            {move || match form.get() {
                Some(Form::Machine(action, entry)) if entry == form_entry => machine_form(workspace, action, entry, form).into_any(),
                _ => ().into_any(),
            }}
            <ul class="flex flex-col">{rows}</ul>
            {empty.then(|| view! { <p class=format!("{NOTE} px-3 py-3")>"No runtimes. Drop one here to pin it to this machine."</p> })}
        </section>
    }
}

/// A machine's rename or merge: the name it asks for.
fn machine_form(
    workspace: Workspace,
    action: MachineAction,
    entry: MachineEntry,
    form: RwSignal<Option<Form>>,
) -> impl IntoView {
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
        <div class="flex flex-col gap-2 border-b border-toasted p-3 dark:border-ember">
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
}

/// A runtime's row: a grip, how it came to the daemon, its ID, whether it
/// is connected and pinned, what it offers, and why it last failed; then
/// "Move to…" and, when pinned, Unpin. Pressing the row drags it.
fn runtime_row(
    workspace: Workspace,
    card: RuntimeCard,
    names: Vec<MachineName>,
    form: RwSignal<Option<Form>>,
    dragging: Dragging,
) -> impl IntoView {
    let id = card.id.clone();
    let lifetime = card.listing.as_ref().map(|listing| listing.lifetime);
    let message = card
        .listing
        .as_ref()
        .and_then(|listing| listing.message.clone())
        .filter(|_| !card.connected);
    let capabilities = card
        .listing
        .as_ref()
        .map(|listing| {
            listing
                .capabilities
                .iter()
                .map(|capability| capability.name.to_string())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let (status_icon, status_class, status) = if card.connected {
        (
            icondata::FaCircleSolid,
            "text-green-700 dark:text-green-400",
            "connected",
        )
    } else if message.is_some() {
        (
            icondata::FaCircleExclamationSolid,
            "text-red-700 dark:text-red-400",
            "not connected",
        )
    } else {
        (icondata::FaCircleRegular, "opacity-60", "not connected")
    };
    // An offline runtime the machine remembers: when it was last seen.
    let remembered = card.remembered.clone().filter(|_| !card.connected);
    let detail = [
        Some(status.to_owned()),
        remembered.as_ref().map(|remembered| {
            format!(
                "last seen {}",
                crate::format::relative_time(jiff::Timestamp::now(), remembered.last_seen)
            )
        }),
        lifetime.map(|lifetime| lifetime_label(lifetime).to_owned()),
        card.listing
            .as_ref()
            .and_then(|listing| listing.platform.clone())
            .or_else(|| {
                remembered
                    .as_ref()
                    .and_then(|remembered| remembered.platform.clone())
            }),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" · ");
    let pressed = card.clone();
    let on_pointerdown = move |event: ev::PointerEvent| {
        // The row's buttons are clicks, not drags.
        let on_button = event
            .target()
            .and_then(|target| target.dyn_into::<web_sys::Element>().ok())
            .and_then(|target| target.closest("button").ok().flatten())
            .is_some();
        let touch = event.pointer_type() == "touch";
        if on_button || (!touch && event.button() != 0) {
            return;
        }
        let (x, y) = (f64::from(event.client_x()), f64::from(event.client_y()));
        dragging.stop_timer();
        dragging
            .gesture
            .update_value(|gesture| gesture.press(x, y, touch));
        dragging.pressed.set_value(Some(pressed.clone()));
        if touch {
            browser::set_pressing(true);
            let id = pressed.id.clone();
            let timer = set_timeout_with_handle(
                move || {
                    if dragging
                        .gesture
                        .try_update_value(Drag::elapse)
                        .unwrap_or(false)
                    {
                        dragging.dragged.set(Some(id.clone()));
                    }
                },
                LONG_PRESS,
            )
            .ok();
            dragging.timer.set_value(timer);
        } else {
            // No text selection while the mouse drags.
            event.prevent_default();
        }
    };
    let lifted = {
        let id = id.clone();
        move || {
            dragging
                .dragged
                .with(|dragged| dragged.as_ref() == Some(&id))
        }
    };
    let moving = card.clone();
    let unpinned = card.id.clone();
    let forgotten = card.id.clone();
    let offline = remembered.is_some();
    let picker_card = card.clone();
    view! {
        <li
            class="runtime-entry no-callout relative flex cursor-grab touch-manipulation items-center gap-2 border-b border-toasted py-1 pr-1 pl-2 select-none last:border-b-0 dark:border-ember"
            class:z-30=lifted.clone()
            class:rounded-lg=lifted.clone()
            class:shadow-xl=lifted.clone()
            class:pointer-events-none=lifted.clone()
            class:bg-cream=lifted.clone()
            class:dark:bg-night=lifted.clone()
            class:ring-2=lifted.clone()
            class:ring-caramel=lifted.clone()
            class:dark:ring-maple=lifted.clone()
            class:opacity-60={
                let lifted = lifted.clone();
                move || offline && !lifted()
            }
            data-offline=offline.then_some("true")
            style:transform={
                let lifted = lifted.clone();
                move || {
                    let (dx, dy) = dragging.offset.get();
                    if lifted() {
                        format!("translate({dx}px, {dy}px) scale(1.02)")
                    } else {
                        String::new()
                    }
                }
            }
            data-runtime=id.to_string()
            data-machine-entry=id.to_string()
            data-dragging=move || lifted().then_some("true")
            on:pointerdown=on_pointerdown
            on:contextmenu=move |event: ev::MouseEvent| {
                // A long press lifts the row; it is not the browser's menu.
                if dragging.pressed.with_value(Option::is_some) {
                    event.prevent_default();
                }
            }
        >
            <Glyph icon=icondata::FaGripVerticalSolid class="opacity-40" />
            <span class="inline-flex h-8 w-8 shrink-0 items-center justify-center rounded-md bg-toasted dark:bg-ember" title=lifetime.map_or("not listed", lifetime_label)>
                <Glyph icon=lifetime_icon(lifetime) />
            </span>
            <div class="flex min-w-0 flex-1 flex-col gap-0.5 py-1">
                <span class="flex min-w-0 items-center gap-2">
                    <span class="min-w-0 truncate font-mono text-sm font-bold">{id.to_string()}</span>
                    {card.pinned.then(|| view! {
                        <span class=format!("{STATE} inline-flex items-center gap-1") title="Pinned to this machine, whatever it reports">
                            <Glyph icon=icondata::FaThumbtackSolid />
                            "pinned"
                        </span>
                    })}
                </span>
                <span class=format!("flex min-w-0 items-center gap-1.5 {NOTE}")>
                    <Glyph icon=status_icon class=format!("text-[0.6rem] {status_class}") />
                    <span class="min-w-0 truncate">{detail}</span>
                </span>
                {(!capabilities.is_empty()).then(|| view! {
                    <span class="flex flex-wrap gap-1">
                        {capabilities.into_iter().map(|capability| view! {
                            <span class="rounded bg-toasted px-1.5 font-mono text-[0.7rem] dark:bg-ember">{capability}</span>
                        }).collect_view()}
                    </span>
                })}
                {card.mcp.clone().map(|mcp| view! {
                    <span class=format!("flex min-w-0 items-center gap-1.5 {NOTE}") data-runtime-mcp-count>
                        <Glyph icon=icondata::FaServerSolid class="text-[0.6rem] opacity-70" />
                        <span class="min-w-0 truncate">{mcp}</span>
                    </span>
                })}
                {(!card.servers.is_empty()).then(|| {
                    let home = card.listing.as_ref().and_then(|listing| listing.home.clone());
                    crate::views::manage::runtime_mcp_servers(card.servers.clone(), Some(home))
                })}
                {message.map(|message| view! { <span class="text-xs break-words text-red-800 dark:text-red-300">{message}</span> })}
            </div>
            <button
                type="button"
                class=format!("{BUTTON} px-2")
                aria-label="Move to…"
                title="Move to another machine"
                data-machine-action=MachineAction::Move.label()
                on:click=move |_| form.set(Some(Form::Move(Box::new(moving.clone()))))
            >
                <Glyph icon=icondata::FaArrowRightArrowLeftSolid />
                <span class="hidden sm:inline">"Move to…"</span>
            </button>
            {card.pinned.then(|| view! {
                <button
                    type="button"
                    class=ICON_BUTTON
                    aria-label="Unpin"
                    title="Unpin: follow the machine it reports again"
                    data-machine-action=MachineAction::Unpin.label()
                    on:click=move |_| workspace.change_machines(
                        cookie_agent_client_state::runtimes::MachineRequest::Unassign { runtime: unpinned.clone() },
                    )
                >
                    <Glyph icon=icondata::FaThumbtackSlashSolid />
                </button>
            })}
            {offline.then(|| view! {
                <button
                    type="button"
                    class=format!("{BUTTON} px-2")
                    aria-label="Forget"
                    title="Forget where this offline runtime ran: it leaves this machine until it connects again"
                    data-machine-action="forget-runtime"
                    on:click=move |_| workspace.change_machines(
                        cookie_agent_client_state::runtimes::MachineRequest::ForgetRuntime { runtime: forgotten.clone() },
                    )
                >
                    <Glyph icon=icondata::FaTrashCanRegular />
                    <span class="hidden sm:inline">"Forget"</span>
                </button>
            })}
        </li>
        {move || match form.get() {
            Some(Form::Move(open)) if open.id == picker_card.id => move_picker(workspace, *open, names.clone(), form).into_any(),
            _ => ().into_any(),
        }}
    }
}

/// "Move to…": the other machines, or a new machine's name.
fn move_picker(
    workspace: Workspace,
    card: RuntimeCard,
    names: Vec<MachineName>,
    form: RwSignal<Option<Form>>,
) -> impl IntoView {
    let targets = names
        .iter()
        .filter(|name| Some(*name) != card.machine.as_ref())
        .map(|name| {
            let request = cookie_agent_client_state::runtimes::MachineRequest::Assign {
                runtime: card.id.clone(),
                machine: name.clone(),
            };
            view! {
                <button
                    type="button"
                    class=BUTTON
                    data-move-target=name.to_string()
                    on:click=move |_| {
                        form.set(None);
                        workspace.change_machines(request.clone());
                    }
                >
                    <Glyph icon=icondata::FaServerSolid />
                    <span class="font-mono">{name.to_string()}</span>
                </button>
            }
        })
        .collect_view();
    let runtime = card.id.clone();
    let submit = StoredValue::new_local(move || {
        match move_request(&runtime, &field_value("machine-name")) {
            Ok(request) => {
                form.set(None);
                workspace.change_machines(request);
            }
            Err(error) => workspace.status.set(error),
        }
    });
    let submit = move || submit.with_value(|submit| submit());
    view! {
        <li class=format!("{CONFIRM} m-2") data-move-picker=card.id.to_string()>
            <p class="text-sm font-bold">{format!("Move {} to", card.id)}</p>
            <div class="flex flex-wrap gap-2">{targets}</div>
            <label class="flex flex-col gap-1 text-sm">
                <span>"Or a new machine"</span>
                <input
                    type="text"
                    id="machine-name"
                    class=FIELD
                    placeholder="machine name"
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
                    "Move"
                </button>
                <button type="button" class=BUTTON on:click=move |_| form.set(None)>
                    "Cancel"
                </button>
            </div>
        </li>
    }
}

/// "Add a runtime": a template the daemon starts over ssh, written into
/// `config.toml`; the command a runtime dials in with; and the guides.
fn add_runtime(workspace: Workspace, adding: RwSignal<bool>) -> impl IntoView {
    let id = RwSignal::new(String::new());
    let host = RwSignal::new(String::new());
    let root = RwSignal::new(String::new());
    let outcome = RwSignal::new(None::<Result<String, String>>);
    let dial_id = RwSignal::new(String::new());
    let template = move || SshTemplate::new(&id.get(), &host.get(), &root.get());
    let daemon_url = StoredValue::new(browser::load(crate::connect::DAEMON_URL_KEY));
    // The token this page connected with, so the command runs as it is.
    let token = StoredValue::new(daemon_url.with_value(|url| {
        url.as_deref()
            .and_then(|url| browser::load(&crate::connect::token_key(url)))
    }));
    let show_token = RwSignal::new(false);
    let command = move || {
        daemon_url.with_value(|daemon_url| {
            token.with_value(|token| {
                dial_in_command(daemon_url.as_deref(), &dial_id.get(), token.as_deref())
            })
        })
    };
    let shown_command = move || {
        let command = command();
        if show_token.get() {
            command
        } else {
            token.with_value(|token| masked_token(&command, token.as_deref()))
        }
    };
    let field = |label: &'static str,
                 field_id: &'static str,
                 placeholder: &'static str,
                 value: RwSignal<String>| {
        view! {
            <label class="flex min-w-0 flex-1 flex-col gap-1 text-xs font-bold text-ink dark:text-sand">
                <span>{label}</span>
                <input
                    type="text"
                    id=field_id
                    class=FIELD
                    placeholder=placeholder
                    autocomplete="off"
                    autocapitalize="off"
                    spellcheck="false"
                    prop:value=move || value.get()
                    on:input=move |event| value.set(event_target_value(&event))
                />
            </label>
        }
    };
    let guides = GUIDES
        .into_iter()
        .map(|guide| {
            view! {
                <li>
                    <a
                        href=guide.url()
                        target="_blank"
                        rel="noopener noreferrer"
                        class="flex min-h-11 items-start gap-2 rounded-md px-2 py-1.5 hover:bg-toasted active:bg-crust dark:hover:bg-ember dark:active:bg-smoke"
                        data-guide
                    >
                        <Glyph icon=icondata::FaArrowUpRightFromSquareSolid class="mt-1 text-xs opacity-70" />
                        <span class="flex min-w-0 flex-col">
                            <span class="text-sm font-bold text-caramel underline-offset-2 hover:underline dark:text-maple">{guide.title}</span>
                            <span class=NOTE>{guide.note}</span>
                        </span>
                    </a>
                </li>
            }
        })
        .collect_view();
    let heading = "flex items-center gap-2 text-sm font-bold";
    view! {
        <section id="add-runtime" class="flex flex-col gap-3 rounded-xl border border-caramel bg-parchment p-3 dark:border-maple dark:bg-char">
            <div class="flex items-center gap-2">
                <h3 class="min-w-0 flex-1 font-bold">"Add a runtime"</h3>
                <button type="button" class=ICON_BUTTON aria-label="Close" title="Close" on:click=move |_| adding.set(false)>
                    <Glyph icon=icondata::FaXmarkSolid class="text-lg" />
                </button>
            </div>
            <div class="flex flex-col gap-2" data-add="ssh">
                <h4 class=heading>
                    <Glyph icon=icondata::FaTerminalSolid />
                    "Start one over ssh"
                </h4>
                <p class=NOTE>
                    "The daemon runs cookie runtime serve on the other machine when a session first needs it. Install the same cookie version there, and check that ssh <host> cookie --version works without a password."
                </p>
                <div class="flex flex-col gap-2 sm:flex-row">
                    {field("Runtime ID", "ssh-id", "labpc", id)}
                    {field("ssh host", "ssh-host", "labpc or me@labpc.lan", host)}
                </div>
                {field("Only workspaces under (optional)", "ssh-root", "~/src", root)}
                {move || match template() {
                    Ok(template) => view! {
                        <pre id="ssh-preview" class="overflow-x-auto rounded-md border border-crust bg-cream p-2 font-mono text-xs dark:border-smoke dark:bg-night">{template.toml()}</pre>
                    }
                    .into_any(),
                    Err(error) => view! {
                        <p id="ssh-preview" class=NOTE>
                            {if id.get().trim().is_empty() || host.get().trim().is_empty() {
                                "Name the runtime and the host to see the template.".to_owned()
                            } else {
                                error
                            }}
                        </p>
                    }
                    .into_any(),
                }}
                <div class="flex flex-wrap items-center gap-2">
                    <button
                        type="button"
                        id="ssh-add"
                        class=PRIMARY
                        disabled=move || template().is_err()
                        on:click=move |_| {
                            if let Ok(template) = template() {
                                workspace.add_runtime_template(template, outcome);
                            }
                        }
                    >
                        <Glyph icon=icondata::FaFileCodeSolid />
                        "Add to config.toml"
                    </button>
                </div>
                {move || outcome.get().map(|outcome| match outcome {
                    Ok(done) => view! {
                        <p id="ssh-outcome" class="flex items-start gap-2 text-sm text-green-800 dark:text-green-300">
                            <Glyph icon=icondata::FaCheckSolid class="mt-1" />
                            {done}
                        </p>
                    }
                    .into_any(),
                    Err(error) => view! { <p id="ssh-outcome" class=ERROR>{error}</p> }.into_any(),
                })}
            </div>
            <div class="flex flex-col gap-2 border-t border-toasted pt-3 dark:border-ember" data-add="dial-in">
                <h4 class=heading>
                    <Glyph icon=icondata::FaPlugSolid />
                    "Let one dial in"
                </h4>
                <p class=NOTE>
                    {move || if token.with_value(Option::is_some) {
                        "Run this on the other machine: it connects to this daemon and stays connected. It carries the token this page connected with; keep it private. If the address is a placeholder, use the daemon's address as that machine reaches it (the daemon listens there with cookie daemon --listen 0.0.0.0:7419)."
                    } else {
                        "Run this on the other machine: it connects to this daemon and stays connected. Replace the placeholders with the daemon's address as that machine reaches it (the daemon listens there with cookie daemon --listen 0.0.0.0:7419) and its token ([server] token in the daemon's config.toml)."
                    }}
                </p>
                {field("Runtime ID (optional; the host's name otherwise)", "dial-id", "labpc", dial_id)}
                <textarea
                    id="dial-in-command"
                    readonly
                    rows="2"
                    class="w-full resize-none rounded-md border border-crust bg-cream p-2 font-mono text-xs dark:border-smoke dark:bg-night"
                    prop:value=shown_command
                ></textarea>
                <div class="flex flex-wrap gap-2">
                <button
                    type="button"
                    id="dial-in-copy"
                    class=BUTTON
                    on:click=move |_| {
                        let text = command();
                        spawn_local(async move {
                            let copied = browser::copy_text(&text).await;
                            workspace.status.set(if copied {
                                "copied the dial-in command".into()
                            } else {
                                // The selection must hold the token, not its mask.
                                show_token.set(true);
                                browser::select_text("dial-in-command");
                                "copying needs a secure page; the command is selected, copy it with the keyboard".into()
                            });
                        });
                    }
                >
                    <Glyph icon=icondata::FaCopyRegular />
                    "Copy"
                </button>
                <Show when=move || token.with_value(Option::is_some)>
                    <button
                        type="button"
                        id="dial-in-reveal"
                        class=BUTTON
                        aria-pressed=move || show_token.get().to_string()
                        on:click=move |_| show_token.update(|shown| *shown = !*shown)
                    >
                        <Glyph icon=Signal::derive(move || if show_token.get() { icondata::FaEyeSlashRegular } else { icondata::FaEyeRegular }) />
                        {move || if show_token.get() { "Hide token" } else { "Show token" }}
                    </button>
                </Show>
                </div>
            </div>
            <div class="flex flex-col gap-1 border-t border-toasted pt-3 dark:border-ember" data-add="guides">
                <h4 class=heading>
                    <Glyph icon=icondata::FaBookOpenSolid />
                    "Guides"
                </h4>
                <ul class="flex flex-col">{guides}</ul>
            </div>
        </section>
    }
}
