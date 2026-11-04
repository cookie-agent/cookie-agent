//! Tool groups and skills in the session panel
//! (`docs/site/specs/session-tool-groups.md` section 12.2): each
//! attachment's groups as chips in the Runtimes tab, a choice of one group
//! at a time, the up-front, deferred, and off figures, and a switch per
//! skill.

use cookie_agent_protocol::{
    ExposureLayer, RuntimeGroupsResult, RuntimeToolGroup, SessionSkill, SessionToolGroup,
    SessionToolsGetResult, SkillSource, SkillState, ToolExposure, ToolGroupKind, ToolGroupSelector,
    ToolGroupState,
};
use leptos::prelude::*;

use super::{BUTTON, NOTE};
use crate::{
    cards::Card,
    icons::Glyph,
    workspace::{CardSet, Workspace},
};

/// A chip's look by state: on plain, deferred in the accent, off dashed
/// and struck through; one hidden by permissions faint, and one known only
/// from a runtime's last run dotted.
fn chip_class(state: ToolGroupState, hidden: bool, cached: bool) -> String {
    if hidden {
        return "group-chip inline-flex min-h-7 items-center gap-1 rounded-md border border-dotted border-line-2 px-2 text-xs text-faint opacity-60".to_owned();
    }
    let look = match state {
        ToolGroupState::On => "border-line-2 bg-surface text-fg-2",
        ToolGroupState::Deferred => "border-transparent bg-accent-soft text-accent",
        ToolGroupState::Off => "border-line-2 text-faint line-through decoration-warn",
    };
    let style = if cached {
        "border-dotted opacity-80"
    } else if state == ToolGroupState::Off {
        "border-dashed"
    } else {
        "border-solid"
    };
    format!(
        "group-chip relative inline-flex min-h-7 items-center gap-1 rounded-md border px-2 text-xs hover:bg-hover {look} {style}"
    )
}

fn kind_glyph(kind: ToolGroupKind) -> impl IntoView {
    match kind {
        ToolGroupKind::Mcp => view! { <Glyph icon=icondata::LuPlug /> }.into_any(),
        ToolGroupKind::Plugin => view! { <Glyph icon=icondata::LuPuzzle /> }.into_any(),
        ToolGroupKind::Builtin => view! { <Glyph icon=icondata::LuBox /> }.into_any(),
    }
}

/// The dot that says who set a group's or a skill's state; none for the
/// defaults.
fn layer_dot(layer: ExposureLayer) -> Option<&'static str> {
    match layer {
        ExposureLayer::Default => None,
        ExposureLayer::Session => {
            Some("absolute -right-1 -top-1 size-2 rounded-full bg-accent ring-2 ring-surface")
        }
        ExposureLayer::Project => {
            Some("absolute -right-1 -top-1 size-2 rounded-full bg-fg-2 ring-2 ring-surface")
        }
        ExposureLayer::Agent => {
            Some("absolute -right-1 -top-1 size-2 rounded-full bg-faint ring-2 ring-surface")
        }
    }
}

fn state_word(state: ToolGroupState) -> &'static str {
    match state {
        ToolGroupState::On => "on",
        ToolGroupState::Deferred => "deferred",
        ToolGroupState::Off => "off",
    }
}

fn layer_word(layer: ExposureLayer) -> &'static str {
    match layer {
        ExposureLayer::Default => "the defaults'",
        ExposureLayer::Agent => "the agent's",
        ExposureLayer::Project => "the project's",
        ExposureLayer::Session => "this session's",
    }
}

/// Whether `group` belongs on the row of `attachment`: the daemon's own
/// groups and top-level MCP servers sit on `global`'s.
fn on_row(group: &SessionToolGroup, attachment: &str) -> bool {
    match group.attachment.as_deref() {
        None => attachment == "global",
        Some(own) => own == attachment,
    }
}

/// The up-front, deferred, and off figures of the session's groups.
pub fn tools_summary(workspace: Workspace) -> impl IntoView {
    move || {
        workspace.session_tools.with(|tools| {
            let Some(Ok(tools)) = tools else {
                return None;
            };
            let mut up_front = (0, 0);
            let mut deferred = 0;
            let mut off = (0, 0);
            for group in tools.groups.iter().filter(|group| !group.hidden_by_permissions) {
                match group.state {
                    ToolGroupState::On => {
                        up_front.0 += group.tools.len();
                        up_front.1 += group.token_estimate;
                    }
                    ToolGroupState::Deferred => deferred += group.tools.len(),
                    ToolGroupState::Off => {
                        off.0 += 1;
                        off.1 += group.tools.len();
                    }
                }
            }
            let off_class = if off.0 > 0 {
                "text-sm font-semibold text-warn"
            } else {
                "text-sm font-semibold"
            };
            Some(view! {
                <div class="mb-1 grid grid-cols-3 gap-2" data-tools-summary="">
                    <div class="rounded-lg border border-line px-2 py-1.5">
                        <div class=NOTE>"Up front"</div>
                        <div class="text-sm font-semibold">
                            {format!("{} tools · ~{:.1}k tok", up_front.0, up_front.1 as f64 / 1000.0)}
                        </div>
                    </div>
                    <div class="rounded-lg border border-line px-2 py-1.5">
                        <div class=NOTE>"Deferred"</div>
                        <div class="text-sm font-semibold">{format!("{deferred} tools")}</div>
                    </div>
                    <div class="rounded-lg border border-line px-2 py-1.5">
                        <div class=NOTE>"Off"</div>
                        <div class=off_class>
                            {format!("{} groups · {} tools", off.0, off.1)}
                        </div>
                    </div>
                </div>
                <p class=NOTE>"Click a group to change it for this session; changes apply from the next request."</p>
            })
        })
    }
}

/// The Tools row of the attachment `attachment`: its groups as chips, the
/// untouched ones folded, and the menu of the one opened.
pub fn group_chips(workspace: Workspace, attachment: String) -> impl IntoView {
    let open = RwSignal::new(None::<String>);
    let unfolded = RwSignal::new(false);
    move || {
        let groups = workspace.session_tools.with(|tools| match tools {
            Some(Ok(SessionToolsGetResult { groups, .. })) => groups
                .iter()
                .filter(|group| on_row(group, &attachment))
                .cloned()
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        });
        if groups.is_empty() {
            return None;
        }
        // Groups that are on and untouched fold into one chip.
        let (folded, shown): (Vec<_>, Vec<_>) = groups.iter().cloned().partition(|group| {
            !unfolded.get()
                && group.state == ToolGroupState::On
                && group.layer == ExposureLayer::Default
                && !group.hidden_by_permissions
                && group.kind == ToolGroupKind::Builtin
                && attachment == "global"
        });
        let chips = shown
            .into_iter()
            .map(|group| {
                let selector = group.selector.to_string();
                let toggled = selector.clone();
                let hidden = group.hidden_by_permissions;
                let title = if hidden {
                    "Hidden by permissions: the Permissions tab decides".to_owned()
                } else {
                    format!(
                        "{} · {} tools · {} by {}",
                        selector,
                        group.tools.len(),
                        state_word(group.state),
                        layer_word(group.layer)
                    )
                };
                view! {
                    <button
                        type="button"
                        class=chip_class(group.state, hidden, false)
                        data-group=selector.clone()
                        data-state=state_word(group.state)
                        title=title
                        disabled=hidden
                        on:click=move |_| {
                            open.update(|open| {
                                *open = (open.as_deref() != Some(toggled.as_str())).then(|| toggled.clone());
                            });
                        }
                    >
                        {kind_glyph(group.kind)}
                        {group.name.clone()}
                        {layer_dot(group.layer).map(|class| view! { <span class=class></span> })}
                    </button>
                }
            })
            .collect_view();
        let more = (!folded.is_empty()).then(|| {
            view! {
                <button
                    type="button"
                    class="group-chip inline-flex min-h-7 items-center rounded-md border border-dotted border-line-2 px-2 text-xs text-muted hover:bg-hover"
                    data-group-more=""
                    on:click=move |_| unfolded.set(true)
                >
                    {format!("+{}", folded.len())}
                </button>
            }
        });
        let menu = open.get().and_then(|selector| {
            groups
                .iter()
                .find(|group| group.selector.to_string() == selector)
                .cloned()
        });
        Some(view! {
            <div class="flex flex-wrap items-center gap-1.5" data-tools-row="">
                <span class=NOTE>"Tools"</span>
                {chips}
                {more}
            </div>
            {menu.map(|group| group_menu(workspace, group, open))}
        })
    }
}

/// A group's menu: its states, Reset to the layer below, and its tools.
fn group_menu(
    workspace: Workspace,
    group: SessionToolGroup,
    open: RwSignal<Option<String>>,
) -> impl IntoView {
    let chosen = group.selector.clone();
    let below = group.below;
    let choice = move |state: Option<ToolGroupState>| {
        let selector = chosen.clone();
        move |_| {
            workspace.choose_tool_group(selector.clone(), state, below);
            open.set(None);
        }
    };
    let option = |label: &'static str, note: &'static str, state: ToolGroupState| {
        let current = group.state == state;
        view! {
            <button
                type="button"
                class="flex min-h-9 items-center justify-between gap-3 rounded-md px-2 text-sm hover:bg-hover"
                data-choose=state_word(state)
                on:click=choice(Some(state))
            >
                <span>{label}</span>
                <span class=NOTE>{if current { "✓" } else { note }}</span>
            </button>
        }
    };
    let reset = (group.layer == ExposureLayer::Session || group.layer == ExposureLayer::Project)
        .then(|| {
            view! {
                <button
                    type="button"
                    class="flex min-h-9 items-center justify-between gap-3 rounded-md border-t border-line px-2 text-sm hover:bg-hover"
                    data-choose="reset"
                    on:click=choice(None)
                >
                    <span>"Reset"</span>
                    <span class=NOTE>
                        {format!("to {} {}", layer_word(group.below_layer), state_word(group.below))}
                    </span>
                </button>
            }
        });
    view! {
        <div class="mt-1 flex flex-col rounded-lg border border-line-2 bg-raised p-1 shadow" data-group-menu=group.selector.to_string()>
            <p class="px-2 py-1 text-xs text-muted">
                <b class="font-mono text-fg">{group.selector.to_string()}</b>
                {format!(" · {} tools · ~{:.1}k tok", group.tools.len(), group.token_estimate as f64 / 1000.0)}
            </p>
            {option("On", "sent up front", ToolGroupState::On)}
            {option("Deferred", "loaded on demand", ToolGroupState::Deferred)}
            {option("Off", "not offered", ToolGroupState::Off)}
            {reset}
            <p class="border-t border-line px-2 py-1 font-mono text-xs text-muted">{group.tools.join(" · ")}</p>
        </div>
    }
}

/// The switch of the skill `name` for the agent, from the session's
/// choices: nothing until they are read.
pub fn skill_switch(workspace: Workspace, name: String) -> impl IntoView {
    move || {
        let skill = workspace.session_tools.with(|tools| match tools {
            Some(Ok(tools)) => tools
                .skills
                .iter()
                .find(|skill| skill.name == name && !skill.shadowed)
                .cloned(),
            _ => None,
        })?;
        Some(switch(workspace, skill))
    }
}

fn switch(workspace: Workspace, skill: SessionSkill) -> impl IntoView {
    let on = skill.state == SkillState::On;
    let name = skill.name.clone();
    let below = skill.below;
    let next = if on { SkillState::Off } else { SkillState::On };
    let knob = if on {
        "absolute left-4.5 size-4 rounded-full bg-white shadow transition-[left]"
    } else {
        "absolute left-0.5 size-4 rounded-full bg-white shadow transition-[left]"
    };
    let reset = (skill.layer == ExposureLayer::Session || skill.layer == ExposureLayer::Project)
        .then(|| {
            let name = name.clone();
            view! {
                <button
                    type="button"
                    class=BUTTON
                    title=format!("Reset to {} {}", layer_word(skill.below_layer), if below == SkillState::On { "on" } else { "off" })
                    data-skill-reset=name.clone()
                    on:click=move |event| {
                        event.stop_propagation();
                        workspace.choose_skill(name.clone(), None, below);
                    }
                >
                    <Glyph icon=icondata::LuUndo2 />
                </button>
            }
        });
    view! {
        <span class="inline-flex items-center gap-1">
            {reset}
            <button
                type="button"
                role="switch"
                aria-checked=on.to_string()
                title=if on { "On for the agent" } else { "Off for the agent: you can still run it" }
                class="relative inline-flex h-5 w-9 shrink-0 items-center rounded-full transition-colors"
                class:bg-accent=on
                class:bg-line-2=!on
                data-skill-switch=name.clone()
                on:click=move |event| {
                    event.stop_propagation();
                    workspace.choose_skill(name.clone(), Some(next), below);
                }
            >
                <span class=knob></span>
            </button>
        </span>
    }
}

/// When a runtime that is not running last reported what it offers.
fn as_of(offer: &RuntimeGroupsResult) -> Option<String> {
    if offer.live {
        return None;
    }
    let when = offer
        .as_of
        .map(|at| cookie_agent_client_state::format::relative_time(jiff::Timestamp::now(), at))?;
    Some(match &offer.seen_at {
        Some(seen_at) => format!("as of its last start, {when}, at {seen_at}"),
        None => format!("as of its last start, {when}"),
    })
}

/// The choice `exposure` makes for `selector`: its state and whether it
/// came from the project.
fn card_choice(
    exposure: &ToolExposure,
    selector: &ToolGroupSelector,
) -> Option<(ToolGroupState, bool)> {
    exposure
        .tool_groups
        .iter()
        .find(|rule| rule.selector == *selector)
        .map(|rule| (rule.state, rule.from_project))
}

/// The dot of a choice on the cards: the accent for the layer the cards
/// edit (the new session's, or in the project editor the project's), grey
/// for one the new session took from its project.
fn choice_dot(set: CardSet, from_project: bool) -> &'static str {
    if from_project && !set.project {
        "absolute -right-1 -top-1 size-2 rounded-full bg-fg-2 ring-2 ring-surface"
    } else {
        "absolute -right-1 -top-1 size-2 rounded-full bg-accent ring-2 ring-surface"
    }
}

fn choice_word(set: CardSet, from_project: bool) -> &'static str {
    match (set.project, from_project) {
        (true, _) => "set by the project",
        (false, true) => "from the project",
        (false, false) => "chosen for this session",
    }
}

/// The workspace typed on `card` in `set`, when it has one.
fn card_place(set: CardSet, card: &Card) -> Option<String> {
    set.selection
        .with(|selection| selection.workspaces(card).into_iter().next())
        .filter(|workspace| !workspace.trim().is_empty())
}

/// A runtime card's Tools row: on a selected card its groups as chips
/// with a menu each, as the session's Runtimes tab has them; on another,
/// what it would bring.
pub fn card_tools(
    workspace: Workspace,
    card: Card,
    set: CardSet,
    selected: Memo<bool>,
) -> impl IntoView {
    let open = RwSignal::new(None::<String>);
    // While a new workspace is read the chips stay as they were.
    let last = StoredValue::new(None::<RuntimeGroupsResult>);
    let place = {
        let card = card.clone();
        Memo::new(move |_| card_place(set, &card))
    };
    {
        let card = card.clone();
        Effect::new(move |_| {
            let place = place.get();
            workspace.read_card_offer(&card, place.as_deref());
        });
    }
    move || {
        let offer = match workspace.card_offer(&card, place.get().as_deref()) {
            Some(offer) => {
                last.set_value(Some(offer.clone()));
                offer
            }
            None => last.get_value()?,
        };
        if !selected.get() {
            if !offer.known || offer.groups.is_empty() {
                return None;
            }
            let names = offer
                .groups
                .iter()
                .map(|group| group.name.as_str())
                .collect::<Vec<_>>();
            return Some(
                view! { <p class=format!("truncate {NOTE}") data-card-brings="">{format!("Brings {}", names.join(", "))}</p> }
                    .into_any(),
            );
        }
        let exposure = set.exposure.get();
        let runtime = card.runtime_id.clone();
        if !offer.known {
            // The choices that name it, then what is not known yet.
            let named = exposure
                .tool_groups
                .iter()
                .filter(|rule| rule.selector.runtime() == Some(&runtime))
                .map(|rule| {
                    let class = chip_class(rule.state, false, true);
                    let dot = choice_dot(set, rule.from_project);
                    view! {
                        <span class=class data-group=rule.selector.to_string() data-state=state_word(rule.state)>
                            {kind_glyph(rule.selector.kind())}
                            {rule.selector.name().to_owned()}
                            <span class=dot></span>
                        </span>
                    }
                })
                .collect_view();
            return Some(
                view! {
                    <div class="flex flex-wrap items-center gap-1.5" data-card-tools="" data-tools-row="">
                        <span class=NOTE>"Tools"</span>
                        {named}
                        <span class="inline-flex min-h-7 items-center rounded-md border border-dotted border-line-2 px-2 text-xs text-muted" data-known-later="">
                            "known once it starts"
                        </span>
                    </div>
                }
                .into_any(),
            );
        }
        let cached = !offer.live;
        let chips = offer
            .groups
            .iter()
            .map(|group| {
                let selector = group.selector.to_string();
                let toggled = selector.clone();
                let choice = card_choice(&exposure, &group.selector);
                let state = choice.map_or(group.default, |(state, _)| state);
                let title = format!(
                    "{} · {} tools · {}{}",
                    selector,
                    group.tools.len(),
                    state_word(state),
                    choice.map_or(String::new(), |(_, from_project)| format!(", {}", choice_word(set, from_project)))
                );
                view! {
                    <button
                        type="button"
                        class=chip_class(state, false, cached)
                        data-group=selector.clone()
                        data-state=state_word(state)
                        title=title
                        on:click=move |_| {
                            open.update(|open| {
                                *open = (open.as_deref() != Some(toggled.as_str())).then(|| toggled.clone());
                            });
                        }
                    >
                        {kind_glyph(group.kind)}
                        {group.name.clone()}
                        {choice.map(|(_, from_project)| view! { <span class=choice_dot(set, from_project)></span> })}
                    </button>
                }
            })
            .collect_view();
        let menu = open.get().and_then(|selector| {
            offer
                .groups
                .iter()
                .find(|group| group.selector.to_string() == selector)
                .cloned()
        });
        Some(
            view! {
                <div class="flex flex-col gap-1" data-card-tools="">
                    <div class="flex flex-wrap items-center gap-1.5" data-tools-row="">
                        <span class=NOTE>"Tools"</span>
                        {chips}
                        {as_of(&offer).map(|when| view! { <span class=NOTE data-as-of="">{when}</span> })}
                    </div>
                    {menu.map(|group| card_group_menu(workspace, set, group, &exposure, open))}
                </div>
            }
            .into_any(),
        )
    }
}

/// A card group's menu: its states, Reset to what the agent and the
/// defaults give, and its tools.
fn card_group_menu(
    workspace: Workspace,
    set: CardSet,
    group: RuntimeToolGroup,
    exposure: &ToolExposure,
    open: RwSignal<Option<String>>,
) -> impl IntoView + use<> {
    let choice = card_choice(exposure, &group.selector);
    let state = choice.map_or(group.default, |(state, _)| state);
    let below = group.default;
    let chosen = group.selector.clone();
    let pick = move |picked: Option<ToolGroupState>| {
        let selector = chosen.clone();
        move |_| {
            workspace.choose_card_group(set, selector.clone(), picked, below);
            open.set(None);
        }
    };
    let option = |label: &'static str, note: &'static str, option: ToolGroupState| {
        view! {
            <button
                type="button"
                class="flex min-h-9 items-center justify-between gap-3 rounded-md px-2 text-sm hover:bg-hover"
                data-choose=state_word(option)
                on:click=pick(Some(option))
            >
                <span>{label}</span>
                <span class=NOTE>{if state == option { "✓" } else { note }}</span>
            </button>
        }
    };
    let reset = choice.is_some().then(|| {
        view! {
            <button
                type="button"
                class="flex min-h-9 items-center justify-between gap-3 rounded-md border-t border-line px-2 text-sm hover:bg-hover"
                data-choose="reset"
                on:click=pick(None)
            >
                <span>"Reset"</span>
                <span class=NOTE>{format!("to the agent's or the defaults' ({} by default)", state_word(below))}</span>
            </button>
        }
    });
    view! {
        <div class="flex flex-col rounded-lg border border-line-2 bg-raised p-1 shadow" data-group-menu=group.selector.to_string()>
            <p class="px-2 py-1 text-xs text-muted">
                <b class="font-mono text-fg">{group.selector.to_string()}</b>
                {format!(" · {} tools · ~{:.1}k tok", group.tools.len(), group.token_estimate as f64 / 1000.0)}
            </p>
            {option("On", "sent up front", ToolGroupState::On)}
            {option("Deferred", "loaded on demand", ToolGroupState::Deferred)}
            {option("Off", "not offered", ToolGroupState::Off)}
            {reset}
            <p class="border-t border-line px-2 py-1 font-mono text-xs text-muted">{group.tools.join(" · ")}</p>
        </div>
    }
}

/// One skill on the cards' Skills list.
#[derive(Clone)]
struct CardSkill {
    name: String,
    description: String,
    /// From a runtime's last run rather than a running one.
    cached: bool,
}

/// One section of the cards' Skills list: a selected card's project
/// skills, the user's, or the official ones.
struct SkillSection {
    title: String,
    place: Option<String>,
    skills: Vec<CardSkill>,
    /// The runtime never ran: its skills are known once it starts.
    later: Option<String>,
}

/// The skills the selected cards in `set` bring, by section.
fn skill_sections(workspace: Workspace, set: CardSet) -> Vec<SkillSection> {
    let cards = workspace
        .cards()
        .into_iter()
        .filter(|card| set.selection.with(|selection| selection.selected(card)))
        .collect::<Vec<_>>();
    let mut sections = Vec::new();
    let mut user = Vec::<CardSkill>::new();
    let mut official = Vec::<CardSkill>::new();
    for card in &cards {
        let place = card_place(set, card);
        let Some(offer) = workspace.card_offer(card, place.as_deref()) else {
            continue;
        };
        let cached = !offer.live;
        if !offer.known {
            if !card.is_global() {
                sections.push(SkillSection {
                    title: format!("Project · {}", card.label()),
                    place,
                    skills: Vec::new(),
                    later: Some(format!("Known once {} starts", card.runtime_id)),
                });
            }
            continue;
        }
        let mut project = Vec::new();
        for skill in &offer.skills {
            let entry = CardSkill {
                name: skill.name.clone(),
                description: skill.description.clone(),
                cached,
            };
            let list = match skill.source {
                SkillSource::Project => &mut project,
                SkillSource::Official => &mut official,
                SkillSource::User => &mut user,
            };
            if !list.iter().any(|known| known.name == entry.name) {
                list.push(entry);
            }
        }
        if !project.is_empty() {
            sections.push(SkillSection {
                title: format!("Project · {}", card.label()),
                place: place.clone(),
                skills: project,
                later: None,
            });
        }
    }
    // A project skill shadows a user or official one of its name, and a
    // user one an official one.
    let projects = sections
        .iter()
        .flat_map(|section| section.skills.iter().map(|skill| skill.name.clone()))
        .collect::<Vec<_>>();
    user.retain(|skill| !projects.contains(&skill.name));
    official.retain(|skill| {
        !projects.contains(&skill.name) && !user.iter().any(|known| known.name == skill.name)
    });
    for (title, skills) in [("User", user), ("Official", official)] {
        if !skills.is_empty() {
            sections.push(SkillSection {
                title: title.to_owned(),
                place: None,
                skills,
                later: None,
            });
        }
    }
    sections
}

/// Whether the skill `name` is on in `exposure`, and whether a choice
/// says so (with whether it came from the project).
fn skill_choice(exposure: &ToolExposure, name: &str) -> (bool, Option<bool>) {
    exposure
        .skills
        .iter()
        .find(|rule| rule.skill.as_str() == name)
        .map_or((true, None), |rule| {
            (rule.state == SkillState::On, Some(rule.from_project))
        })
}

/// How many of the skills the cards bring are off.
pub fn cards_skills_off(workspace: Workspace, set: CardSet) -> usize {
    let exposure = set.exposure.get();
    skill_sections(workspace, set)
        .iter()
        .flat_map(|section| &section.skills)
        .filter(|skill| !skill_choice(&exposure, &skill.name).0)
        .count()
}

/// The Skills the selected cards in `set` bring, each with its switch:
/// below the New session page's cards and the project editor's.
pub fn cards_skills(
    workspace: Workspace,
    set: CardSet,
    heading: &'static str,
    id: &'static str,
) -> impl IntoView {
    // The selected cards' offers, read even when their machine is folded.
    Effect::new(move |_| {
        for card in workspace.cards() {
            if set.selection.with(|selection| selection.selected(&card)) {
                workspace.read_card_offer(&card, card_place(set, &card).as_deref());
            }
        }
    });
    move || {
        let sections = skill_sections(workspace, set);
        let exposure = set.exposure.get();
        let all = sections
            .iter()
            .flat_map(|section| &section.skills)
            .collect::<Vec<_>>();
        let on = all
            .iter()
            .filter(|skill| skill_choice(&exposure, &skill.name).0)
            .count();
        let count = all.len();
        let sections = sections
            .into_iter()
            .map(|section| {
                let rows = section
                    .skills
                    .into_iter()
                    .map(|skill| card_skill_row(workspace, set, skill, &exposure))
                    .collect_view();
                view! {
                    <div class="flex flex-col gap-1" data-skill-section=section.title.clone()>
                        <p class="flex items-center gap-1.5 text-xs font-bold text-muted">
                            {section.title.clone()}
                            {section.place.map(|place| view! { <span class="font-mono font-normal">{place}</span> })}
                        </p>
                        {rows}
                        {section.later.map(|later| view! { <p class=NOTE data-known-later="">{later}</p> })}
                    </div>
                }
            })
            .collect_view();
        view! {
            <section id=id class="flex flex-col gap-3" data-cards-skills="">
                <h3 class="text-sm font-semibold">
                    {heading}
                    <span class=format!("ml-1 font-normal {NOTE}")>{format!("· {on} of {count} on for the agent")}</span>
                </h3>
                {sections}
            </section>
        }
    }
}

fn card_skill_row(
    workspace: Workspace,
    set: CardSet,
    skill: CardSkill,
    exposure: &ToolExposure,
) -> impl IntoView + use<> {
    let (on, chosen) = skill_choice(exposure, &skill.name);
    let name = skill.name.clone();
    let row = name.clone();
    let next = if on { SkillState::Off } else { SkillState::On };
    let knob = if on {
        "absolute left-4.5 size-4 rounded-full bg-white shadow transition-[left]"
    } else {
        "absolute left-0.5 size-4 rounded-full bg-white shadow transition-[left]"
    };
    let reset = chosen.map(|from_project| {
        let name = name.clone();
        view! {
            <button
                type="button"
                class=BUTTON
                title=format!("Reset: {}", choice_word(set, from_project))
                data-skill-reset=name.clone()
                on:click=move |_| workspace.choose_card_skill(set, name.clone(), None, SkillState::On)
            >
                <Glyph icon=icondata::LuUndo2 />
            </button>
        }
    });
    let description = if skill.cached {
        format!("{} · as of its runtime's last start", skill.description)
    } else {
        skill.description.clone()
    };
    view! {
        <div class="flex min-h-11 items-center gap-2 rounded-lg border border-line px-3 py-1.5" class:border-dotted=skill.cached data-card-skill=row>
            <div class="flex min-w-0 flex-1 flex-col">
                <span class="relative inline-flex items-center gap-1.5 truncate text-sm font-semibold">
                    {skill.name.clone()}
                    {chosen.map(|from_project| view! { <span class=choice_dot(set, from_project).replace("absolute -right-1 -top-1 ", "")></span> })}
                </span>
                <span class=format!("line-clamp-1 {NOTE}")>{description}</span>
            </div>
            {reset}
            <button
                type="button"
                role="switch"
                aria-checked=on.to_string()
                title=if on { "On for the agent" } else { "Off for the agent: you can still run it" }
                class="relative inline-flex h-5 w-9 shrink-0 items-center rounded-full transition-colors"
                class:bg-accent=on
                class:bg-line-2=!on
                data-skill-switch=name.clone()
                on:click=move |_| workspace.choose_card_skill(set, name.clone(), Some(next), SkillState::On)
            >
                <span class=knob></span>
            </button>
        </div>
    }
}

/// Under the New session page's cards: the up-front, deferred, and off
/// figures of the selected cards' groups, and in a project the choices it
/// gave, with a way back to them.
pub fn draft_figures(workspace: Workspace, set: CardSet) -> impl IntoView {
    move || {
        let exposure = set.exposure.get();
        let mut up_front = (0, 0);
        let mut deferred = 0;
        let mut off = 0;
        let mut later = Vec::new();
        for card in workspace.cards() {
            if !set.selection.with(|selection| selection.selected(&card)) {
                continue;
            }
            let Some(offer) = workspace.card_offer(&card, card_place(set, &card).as_deref()) else {
                continue;
            };
            if !offer.known {
                later.push(card.runtime_id.to_string());
                continue;
            }
            for group in &offer.groups {
                match card_choice(&exposure, &group.selector)
                    .map_or(group.default, |(state, _)| state)
                {
                    ToolGroupState::On => {
                        up_front.0 += group.tools.len();
                        up_front.1 += group.token_estimate;
                    }
                    ToolGroupState::Deferred => deferred += group.tools.len(),
                    ToolGroupState::Off => off += 1,
                }
            }
        }
        let mut line = format!(
            "{} tools up front · ~{:.1}k tok · {} deferred · {} {} off",
            up_front.0,
            up_front.1 as f64 / 1000.0,
            deferred,
            off,
            if off == 1 { "group" } else { "groups" }
        );
        if !later.is_empty() {
            line.push_str(&format!(" · {} not counted yet", later.join(", ")));
        }
        let project = workspace
            .new_project
            .get()
            .filter(|_| workspace.has_projects.get());
        let copy = workspace.project_exposure(project.as_ref());
        let from_project = (!copy.is_empty()).then(|| {
            let name = untrack(|| workspace.project(project.as_ref()))
                .map(|info| info.name.to_string())
                .unwrap_or_default();
            let mut parts = copy
                .tool_groups
                .iter()
                .map(|rule| format!("{} {}", rule.selector, state_word(rule.state)))
                .collect::<Vec<_>>();
            let skills_off = copy
                .skills
                .iter()
                .filter(|rule| rule.state == SkillState::Off)
                .count();
            if skills_off > 0 {
                parts.push(format!(
                    "{skills_off} {} off",
                    if skills_off == 1 { "skill" } else { "skills" }
                ));
            }
            let changed = exposure != copy;
            view! {
                <p class=format!("ml-6 {NOTE}") data-from-project="">
                    {format!("From project {name}: {}", parts.join(", "))}
                    {changed.then(|| view! {
                        " · "
                        <button
                            type="button"
                            class="text-accent underline"
                            data-reset-to-project=""
                            on:click=move |_| set.exposure.set(copy.clone())
                        >
                            "reset to the project's"
                        </button>
                    })}
                </p>
            }
        });
        view! {
            <p class="flex items-start gap-2 text-sm" data-draft-figures="">
                <Glyph icon=icondata::LuWrench class="mt-0.5 text-muted" />
                <span class="min-w-0 break-words text-muted">{line}</span>
            </p>
            {from_project}
        }
    }
}
