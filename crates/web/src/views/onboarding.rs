//! The first-run sheet (`docs/site/specs/default-models-and-onboarding.md`
//! section 9): a four-step stepper over the daemon's onboarding API. The
//! provider step is Settings' connect form; the three model steps are
//! searchable lists with variant chips that build each field's fallback list
//! (`docs/site/specs/model-fallback-lists.md` section 6), the small and
//! utility models optional. There is no starter kit: the daemon keeps the official agents
//! itself. Full screen on a phone. Its first step chooses how agents run, on
//! model providers, an ACP agent, or both, and its last optional one adds
//! MCP servers (`docs/site/specs/acp-onboarding.md` section 9).

use std::collections::BTreeSet;

use cookie_agent_client_state::onboarding::{
    AcpPlace, ModelChoice, OnboardingStep, PathChoice, acp_check_line, choice_entries,
    choice_label, configured_label, default_model_variant, field_name, finish_refusal_for,
    first_step, join_command_line, mcp_preset_configured, mcp_preset_input, model_groups,
    offered_mcp_presets, other_acp_input, run_summary, small_model_variant, utility_model_variant,
    variants,
};
use cookie_agent_protocol::{
    AvailableModelDescriptor, McpPresetPlace, OnboardingModelField, OnboardingRunParams, RuntimeId,
};
use leptos::prelude::*;

use super::{BUTTON, ERROR, NOTE, PRIMARY, ROW, manage::FIELD};
use crate::{icons::Glyph, workspace::Workspace};

/// A model step's fallback list, in order; empty keeps what is configured.
type Choice = Vec<ModelChoice>;

/// Which model a model step chooses.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Slot {
    Default,
    Small,
    Utility,
}

impl Slot {
    const fn field(self) -> OnboardingModelField {
        match self {
            Self::Default => OnboardingModelField::DefaultModel,
            Self::Small => OnboardingModelField::SmallModel,
            Self::Utility => OnboardingModelField::UtilityModel,
        }
    }

    /// The fields an entry of this one may name: the steps before it.
    const fn earlier(self) -> &'static [OnboardingModelField] {
        match self {
            Self::Default => &[],
            Self::Small => &[OnboardingModelField::DefaultModel],
            Self::Utility => &[
                OnboardingModelField::DefaultModel,
                OnboardingModelField::SmallModel,
            ],
        }
    }
}

/// The sheet, while it is open.
#[component]
pub fn OnboardingSheet(workspace: Workspace) -> impl IntoView {
    move || {
        workspace
            .onboarding
            .open
            .get()
            .then(|| view! { <Stepper workspace=workspace /> })
    }
}

#[component]
fn Stepper(workspace: Workspace) -> impl IntoView {
    let onboarding = workspace.onboarding;
    let step = RwSignal::new(
        onboarding
            .status
            .with_untracked(|status| status.as_ref().map(first_step))
            .unwrap_or(OnboardingStep::Provider),
    );
    let default_choice = RwSignal::new(Choice::new());
    let small_choice = RwSignal::new(Choice::new());
    let utility_choice = RwSignal::new(Choice::new());
    let paths = RwSignal::new(
        onboarding
            .status
            .with_untracked(|status| status.as_ref().map(PathChoice::of))
            .unwrap_or(PathChoice {
                models: true,
                acp: false,
            }),
    );
    let tools = RwSignal::new(BTreeSet::<String>::new());
    let tools_runtime = RwSignal::new("local".to_owned());
    let default_agent = RwSignal::new(String::new());
    let status = move || onboarding.status.get();
    let refusal = move || {
        status().and_then(|status| {
            finish_refusal_for(
                &status,
                paths.get(),
                default_choice.with(|choice| !choice.is_empty()),
            )
        })
    };
    let finish = move || {
        let models = workspace.models();
        let list = |choice: Choice| {
            choice_entries(&choice, |key| models.iter().find(|model| model.key == *key))
        };
        let with_models = paths.get_untracked().models;
        let runtime = RuntimeId::new(tools_runtime.get_untracked()).ok();
        let mcp_servers = onboarding
            .status
            .get_untracked()
            .map(|status| {
                let chosen = tools.get_untracked();
                offered_mcp_presets(&status)
                    .into_iter()
                    .filter(|preset| chosen.contains(&preset.id))
                    .filter(|preset| !mcp_preset_configured(&status, preset, runtime.as_ref()))
                    .map(|preset| mcp_preset_input(preset, runtime.clone()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        // The daemon fetches the official agents first when it has none.
        workspace.run_onboarding(OnboardingRunParams {
            default_model_list: with_models
                .then(|| list(default_choice.get_untracked()))
                .flatten(),
            small_model_list: with_models
                .then(|| list(small_choice.get_untracked()))
                .flatten(),
            utility_model_list: with_models
                .then(|| list(utility_choice.get_untracked()))
                .flatten(),
            mcp_servers: (!mcp_servers.is_empty()).then_some(mcp_servers),
            default_agent: default_agent.get_untracked().parse().ok(),
            ..Default::default()
        });
    };
    view! {
        <div
            id="onboarding"
            role="dialog"
            aria-modal="true"
            aria-labelledby="onboarding-title"
            class="fixed inset-0 z-50 flex items-stretch justify-center bg-black/40 sm:items-center sm:p-4"
        >
            <div class="flex h-full w-full min-w-0 flex-col bg-surface text-fg sm:h-auto sm:max-h-[90vh] sm:max-w-2xl sm:rounded-xl sm:border sm:border-line">
                <header class="flex items-center gap-2 border-b border-line px-4 py-2">
                    <h2 id="onboarding-title" class="min-w-0 flex-1 truncate text-base font-bold">"Set up Cookie"</h2>
                    <Show when=move || onboarding.result.with(Option::is_none)>
                        <button type="button" id="onboarding-not-now" class=BUTTON on:click=move |_| workspace.dismiss_onboarding()>
                            "Not now"
                        </button>
                    </Show>
                </header>
                {move || match onboarding.result.get() {
                    Some(result) => summary(workspace, &result).into_any(),
                    None => view! {
                        <nav aria-label="Onboarding steps" class="grid grid-cols-2 gap-1 border-b border-line px-4 py-2 sm:flex sm:flex-wrap">
                            {move || paths.get().steps()
                                .into_iter()
                                .enumerate()
                                .map(|(index, each)| {
                                    let current = move || step.get() == each;
                                    let done = move || {
                                        status().is_some_and(|status| each.done(&status))
                                            || (each == OnboardingStep::DefaultModel && default_choice.with(|choice| !choice.is_empty()))
                                            || (each == OnboardingStep::SmallModel && small_choice.with(|choice| !choice.is_empty()))
                                            || (each == OnboardingStep::UtilityModel && utility_choice.with(|choice| !choice.is_empty()))
                                    };
                                    view! {
                                        <button
                                            type="button"
                                            class="inline-flex min-h-11 min-w-0 items-center gap-1.5 rounded-md px-2 text-left text-sm hover:bg-hover active:bg-press"
                                            class:font-bold=current
                                            class:bg-sel=current
                                            aria-current=move || current().then_some("step")
                                            data-step=step_id(each)
                                            data-done=move || done().to_string()
                                            on:click=move |_| step.set(each)
                                        >
                                            {move || if done() {
                                                view! { <Glyph icon=icondata::LuCircleCheck class="text-ok" /> }.into_any()
                                            } else {
                                                view! { <span class="inline-flex size-5 items-center justify-center rounded-full border border-current text-xs">{index + 1}</span> }.into_any()
                                            }}
                                            {each.label()}
                                        </button>
                                    }
                                })
                                .collect_view()}
                        </nav>
                        <div class="flex min-h-0 flex-1 flex-col gap-2 overflow-y-auto px-4 py-3" data-onboarding-body>
                            {move || match step.get() {
                                OnboardingStep::Start => start_step(paths).into_any(),
                                OnboardingStep::AcpAgent => acp_step(workspace).into_any(),
                                OnboardingStep::Tools => tools_step(workspace, tools, tools_runtime, default_agent).into_any(),
                                OnboardingStep::Provider => provider_step(workspace).into_any(),
                                OnboardingStep::DefaultModel => model_step(workspace, Slot::Default, default_choice).into_any(),
                                OnboardingStep::SmallModel => model_step(workspace, Slot::Small, small_choice).into_any(),
                                OnboardingStep::UtilityModel => model_step(workspace, Slot::Utility, utility_choice).into_any(),
                            }}
                        </div>
                        <footer class="flex flex-col gap-2 border-t border-line px-4 py-2">
                            {move || onboarding.error.get().map(|message| view! { <p class=ERROR id="onboarding-error">{message}</p> })}
                            <div class="flex flex-wrap items-center gap-2">
                                <button
                                    type="button"
                                    id="onboarding-back"
                                    class=BUTTON
                                    disabled=move || step.get().previous(paths.get()).is_none()
                                    on:click=move |_| {
                                        if let Some(previous) = step.get_untracked().previous(paths.get_untracked()) {
                                            step.set(previous);
                                        }
                                    }
                                >
                                    <Glyph icon=icondata::LuArrowLeft />
                                    "Back"
                                </button>
                                <span class=format!("min-w-0 flex-1 {NOTE}") id="onboarding-refusal">
                                    {move || step.get().next(paths.get()).is_none().then(refusal).flatten()}
                                </span>
                                {move || match step.get().next(paths.get()) {
                                    Some(next) => view! {
                                        <button type="button" id="onboarding-next" class=PRIMARY on:click=move |_| step.set(next)>
                                            "Next"
                                            <Glyph icon=icondata::LuChevronRight />
                                        </button>
                                    }
                                    .into_any(),
                                    None => view! {
                                        <button
                                            type="button"
                                            id="onboarding-finish"
                                            class=PRIMARY
                                            disabled=move || refusal().is_some() || onboarding.running.get()
                                            title=move || refusal().unwrap_or_default()
                                            on:click=move |_| finish()
                                        >
                                            {move || if onboarding.running.get() { "Finishing…" } else { "Finish" }}
                                        </button>
                                    }
                                    .into_any(),
                                }}
                            </div>
                        </footer>
                    }
                    .into_any(),
                }}
            </div>
        </div>
    }
}

const fn step_id(step: OnboardingStep) -> &'static str {
    match step {
        OnboardingStep::Start => "start",
        OnboardingStep::AcpAgent => "acp-agent",
        OnboardingStep::Tools => "tools",
        OnboardingStep::Provider => "provider",
        OnboardingStep::DefaultModel => "default-model",
        OnboardingStep::SmallModel => "small-model",
        OnboardingStep::UtilityModel => "utility-model",
    }
}

/// How agents run: on model providers, an ACP agent, or both.
fn start_step(paths: RwSignal<PathChoice>) -> impl IntoView {
    let card = move |models: bool| {
        let on = move || {
            let chosen = paths.get();
            if models { chosen.models } else { chosen.acp }
        };
        let (title, text, id) = if models {
            (
                "Model providers",
                "Cookie's own agents run on models you connect: an API key or a sign-in with a provider.",
                "models",
            )
        } else {
            (
                "An ACP agent",
                "Claude Code, Codex, or another ACP agent runs with its own login and models; cookie drives it.",
                "acp",
            )
        };
        view! {
            <button
                type="button"
                class=ROW
                class:bg-sel=on
                aria-pressed=move || on().to_string()
                data-path=id
                on:click=move |_| paths.update(|chosen| *chosen = chosen.toggled(models))
            >
                <span class="flex w-full items-center gap-2">
                    {move || if on() {
                        view! { <Glyph icon=icondata::LuSquareCheck /> }.into_any()
                    } else {
                        view! { <Glyph icon=icondata::LuSquare /> }.into_any()
                    }}
                    <span class="text-sm font-bold">{title}</span>
                </span>
                <span class=NOTE>{text}</span>
            </button>
        }
    };
    view! {
        <p class="text-sm">"How should agents run? Choose either, or both."</p>
        {card(true)}
        {card(false)}
    }
}

/// The ACP agent step: a preset or another agent, where it runs, its
/// command; Add declares it at once, so each runtime that offers it can be
/// checked.
fn acp_step(workspace: Workspace) -> impl IntoView {
    let onboarding = workspace.onboarding;
    let presets = move || {
        onboarding
            .status
            .get()
            .map(|status| status.acp_presets)
            .unwrap_or_default()
    };
    // `None`: another ACP agent.
    let picked = RwSignal::new(None::<usize>);
    let place = RwSignal::new(AcpPlace::DaemonMachine);
    let id = RwSignal::new(String::new());
    let command = RwSignal::new(String::new());
    let env = RwSignal::new(String::new());
    let description = RwSignal::new(String::new());
    let pick = move |index: Option<usize>| {
        picked.set(index);
        let preset = index.and_then(|index| presets().get(index).cloned());
        id.set(
            preset
                .as_ref()
                .map(|preset| preset.id.to_string())
                .unwrap_or_default(),
        );
        command.set(
            preset
                .as_ref()
                .map(|preset| join_command_line(&preset.command))
                .unwrap_or_default(),
        );
        env.set(
            preset
                .as_ref()
                .map(|preset| {
                    preset
                        .env
                        .iter()
                        .map(|(key, value)| format!("{key}={value}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default(),
        );
        description.set(preset.map(|preset| preset.description).unwrap_or_default());
    };
    // Read once: the step's view must not follow the status, or each
    // change would rebuild it and lose what was typed.
    untrack(|| {
        if !presets().is_empty() {
            pick(Some(0));
        }
    });
    let add = move |_| {
        let lines = env
            .get_untracked()
            .lines()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        match other_acp_input(
            &id.get_untracked(),
            &command.get_untracked(),
            &lines,
            &description.get_untracked(),
            place.get_untracked(),
        ) {
            Ok(agent) => workspace.add_onboarding_acp(agent),
            Err(reason) => onboarding.acp_note.set(Some(reason)),
        }
    };
    let field = |label: &'static str, value: RwSignal<String>, name: &'static str| {
        view! {
            <label class="flex flex-col gap-1 text-sm">
                <span>{label}</span>
                <input
                    type="text"
                    class=FIELD
                    data-acp-field=name
                    prop:value=move || value.get()
                    on:input=move |event| value.set(event_target_value(&event))
                />
            </label>
        }
    };
    view! {
        <p class="text-sm">"Cookie launches the agent on a machine, where it uses that machine's login. Install it and log in there first."</p>
        <div class="flex flex-wrap gap-2" data-acp-presets>
            {move || presets()
                .into_iter()
                .enumerate()
                .map(|(index, preset)| {
                    let on = move || picked.get() == Some(index);
                    view! {
                        <button
                            type="button"
                            class=BUTTON
                            class:bg-sel=on
                            aria-pressed=move || on().to_string()
                            data-acp-preset=preset.id.to_string()
                            on:click=move |_| pick(Some(index))
                        >
                            {preset.name.clone()}
                        </button>
                    }
                })
                .collect_view()}
            <button
                type="button"
                class=BUTTON
                class:bg-sel=move || picked.get().is_none()
                aria-pressed=move || picked.get().is_none().to_string()
                data-acp-preset="other"
                on:click=move |_| pick(None)
            >
                "Another ACP agent"
            </button>
        </div>
        {move || picked
            .get()
            .and_then(|index| presets().get(index).cloned())
            .map(|preset| view! { <p class=NOTE data-acp-needs>{format!("Needs: {}", preset.needs)}</p> })}
        <div class="flex flex-wrap gap-2" data-acp-place>
            {AcpPlace::ALL
                .into_iter()
                .map(|each| {
                    let on = move || place.get() == each;
                    view! {
                        <button
                            type="button"
                            class=BUTTON
                            class:bg-sel=on
                            aria-pressed=move || on().to_string()
                            data-acp-where=match each {
                                AcpPlace::EveryMachine => "every",
                                AcpPlace::DaemonMachine => "local",
                            }
                            on:click=move |_| place.set(each)
                        >
                            {each.label()}
                        </button>
                    }
                })
                .collect_view()}
        </div>
        {field("ID", id, "id")}
        {field("Command", command, "command")}
        <label class="flex flex-col gap-1 text-sm">
            <span>"Environment (KEY=VALUE, one per line)"</span>
            <textarea
                class=FIELD
                rows="2"
                data-acp-field="env"
                prop:value=move || env.get()
                on:input=move |event| env.set(event_target_value(&event))
            ></textarea>
        </label>
        {field("Description", description, "description")}
        <div class="flex flex-wrap items-center gap-2">
            <button type="button" id="onboarding-acp-add" class=PRIMARY disabled=move || onboarding.running.get() on:click=add>
                "Add ACP agent"
            </button>
            {move || onboarding.acp_note.get().map(|note| view! { <span class=NOTE id="onboarding-acp-note">{note}</span> })}
        </div>
        {move || acp_declarations(workspace)}
    }
}

/// The declared ACP agents, each runtime that offers it with a Check.
fn acp_declarations(workspace: Workspace) -> impl IntoView {
    let onboarding = workspace.onboarding;
    let agents = onboarding
        .status
        .get()
        .and_then(|status| status.paths)
        .map(|paths| paths.acp.agents)
        .unwrap_or_default();
    (!agents.is_empty()).then(|| {
        let rows = agents
            .into_iter()
            .map(|agent| {
                let place = agent
                    .runtime
                    .as_ref()
                    .map_or_else(|| "every machine".to_owned(), ToString::to_string);
                let runtimes = agent
                    .runtimes
                    .into_iter()
                    .map(|runtime| {
                        let key = (runtime.runtime_id.clone(), agent.id.clone());
                        let check = {
                            let key = key.clone();
                            move |_| workspace.check_onboarding_acp(key.0.clone(), key.1.clone())
                        };
                        let outcome = {
                            let key = key.clone();
                            move || onboarding.checks.with(|checks| checks.get(&key).cloned())
                        };
                        let name = runtime.runtime_id.to_string();
                        view! {
                            <li class="flex flex-wrap items-center gap-2 text-sm" data-acp-check=format!("{}@{name}", key.1)>
                                <span class="font-mono">{format!("on {name}")}</span>
                                {(!runtime.available).then(|| view! { <span class=NOTE>"away"</span> })}
                                <button type="button" class=BUTTON disabled=!runtime.available on:click=check>
                                    "Check"
                                </button>
                                {move || match outcome() {
                                    None => None,
                                    Some(None) => Some(view! { <span class=NOTE>"Checking…"</span> }.into_any()),
                                    Some(Some(Ok(result))) => Some(view! {
                                        <span class=if result.ok { NOTE } else { ERROR } data-check-ok=result.ok.to_string()>{acp_check_line(&result)}</span>
                                    }.into_any()),
                                    Some(Some(Err(error))) => Some(view! { <span class=ERROR>{error}</span> }.into_any()),
                                }}
                            </li>
                        }
                    })
                    .collect_view();
                view! {
                    <li class="flex flex-col gap-1" data-acp-agent=agent.id.to_string()>
                        <span class="text-sm">
                            <span class="font-mono font-bold">{agent.id.to_string()}</span>
                            {format!(" · {place}{}", if agent.enabled { "" } else { " · turned off" })}
                        </span>
                        <ul class="flex flex-col gap-1 pl-3">{runtimes}</ul>
                    </li>
                }
            })
            .collect_view();
        view! {
            <h3 class="pt-2 text-xs font-bold uppercase tracking-wide text-muted">"Linked ACP agents"</h3>
            <ul class="flex flex-col gap-2" id="onboarding-acp-agents">{rows}</ul>
        }
    })
}

/// The optional Tools: MCP servers that need no login, and the agent new
/// sessions start with.
fn tools_step(
    workspace: Workspace,
    tools: RwSignal<BTreeSet<String>>,
    runtime: RwSignal<String>,
    default_agent: RwSignal<String>,
) -> impl IntoView {
    let onboarding = workspace.onboarding;
    let runtimes = move || {
        let mut ids = workspace
            .runtimes
            .get()
            .map(|list| {
                list.runtimes
                    .into_iter()
                    .map(|listing| listing.runtime_id.to_string())
                    .filter(|id| id != RuntimeId::GLOBAL)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if !ids.iter().any(|id| id == "local") {
            ids.insert(0, "local".to_owned());
        }
        ids
    };
    let agents = move || {
        let mut ids = workspace
            .selectable_agents()
            .into_iter()
            .filter(|agent| agent.preset.is_none())
            .map(|agent| agent.id.to_string())
            .collect::<Vec<_>>();
        if let Some(status) = onboarding.status.get() {
            for agent in status
                .paths
                .map(|paths| paths.acp.agents)
                .unwrap_or_default()
            {
                if !ids.contains(&agent.id.to_string()) {
                    ids.push(agent.id.to_string());
                }
            }
        }
        ids
    };
    view! {
        <p class="text-sm">"Optional: tools that need no login. Agents ask you before each call until you allow it."</p>
        {move || {
            let Some(status) = onboarding.status.get() else {
                return view! { <p class=NOTE>"Asking the daemon…"</p> }.into_any();
            };
            let place = RuntimeId::new(runtime.get()).ok();
            let presets = offered_mcp_presets(&status)
                .into_iter()
                .cloned()
                .collect::<Vec<_>>();
            if presets.is_empty() {
                return view! { <p class=NOTE>"The daemon has no tool presets yet: they arrive with its official agents."</p> }.into_any();
            }
            let machine = presets.iter().any(|preset| preset.place == McpPresetPlace::Machine);
            let rows = presets
                .into_iter()
                .map(|preset| {
                    let configured = mcp_preset_configured(&status, &preset, place.as_ref());
                    let name = preset.id.clone();
                    let on = {
                        let name = name.clone();
                        Signal::derive(move || {
                            configured || tools.with(|chosen| chosen.contains(&name))
                        })
                    };
                    let toggle = move |_| {
                        if !configured {
                            tools.update(|chosen| {
                                if !chosen.remove(&name) {
                                    chosen.insert(name.clone());
                                }
                            });
                        }
                    };
                    view! {
                        <button
                            type="button"
                            class=ROW
                            class:opacity-50=configured
                            aria-pressed=move || on.get().to_string()
                            data-mcp-preset=preset.id.clone()
                            on:click=toggle
                        >
                            <span class="flex w-full items-center gap-2">
                                {move || if on.get() {
                                    view! { <Glyph icon=icondata::LuSquareCheck /> }.into_any()
                                } else {
                                    view! { <Glyph icon=icondata::LuSquare /> }.into_any()
                                }}
                                <span class="text-sm font-bold">{preset.name.clone()}</span>
                                <span class=NOTE>{preset.category.clone()}</span>
                                {configured.then(|| view! { <span class=NOTE>"· set up"</span> })}
                            </span>
                            <span class=NOTE>{preset.description.clone()}</span>
                            <span class=NOTE>{format!("Needs: {}", preset.needs)}</span>
                        </button>
                    }
                })
                .collect_view();
            view! {
                <div class="flex flex-col gap-1" id="onboarding-tools">{rows}</div>
                {machine.then(|| view! {
                    <label class="flex flex-col gap-1 text-sm">
                        <span>"Tools that run a program run on"</span>
                        <select
                            class=FIELD
                            data-mcp-runtime
                            on:change=move |event| runtime.set(event_target_value(&event))
                        >
                            {runtimes()
                                .into_iter()
                                .map(|id| {
                                    let selected = id == runtime.get_untracked();
                                    view! { <option value=id.clone() selected=selected>{id.clone()}</option> }
                                })
                                .collect_view()}
                        </select>
                    </label>
                })}
            }
            .into_any()
        }}
        <label class="flex flex-col gap-1 pt-2 text-sm">
            <span>"New sessions start with"</span>
            <select
                class=FIELD
                id="onboarding-default-agent"
                on:change=move |event| default_agent.set(event_target_value(&event))
            >
                <option value="" selected=true>
                    {move || {
                        let current = onboarding
                            .status
                            .get()
                            .and_then(|status| status.default_agent)
                            .map_or_else(|| workspace.default_agent().to_string(), |agent| agent.to_string());
                        format!("Keep: {current}")
                    }}
                </option>
                {move || agents()
                    .into_iter()
                    .map(|id| view! { <option value=id.clone()>{id.clone()}</option> })
                    .collect_view()}
            </select>
        </label>
    }
}

/// The connected providers with their models, then Settings' connect form.
fn provider_step(workspace: Workspace) -> impl IntoView {
    let connected = move || {
        let models = workspace.models();
        workspace
            .onboarding
            .status
            .get()
            .map(|status| status.providers_connected)
            .unwrap_or_default()
            .into_iter()
            .map(|provider| {
                let count = models
                    .iter()
                    .filter(|model| model.key.provider_id() == provider)
                    .count();
                format!(
                    "{provider} · {count} available model{}",
                    if count == 1 { "" } else { "s" }
                )
            })
            .collect::<Vec<_>>()
    };
    view! {
        <p class="text-sm">"Connect at least one model provider. You can connect several."</p>
        {move || {
            let connected = connected();
            if connected.is_empty() {
                view! { <p class=NOTE id="onboarding-connected">"No provider is connected yet."</p> }.into_any()
            } else {
                view! {
                    <ul class="flex flex-col gap-1" id="onboarding-connected">
                        {connected
                            .into_iter()
                            .map(|line| view! {
                                <li class="flex items-center gap-2 text-sm">
                                    <Glyph icon=icondata::LuPlug class="text-ok" />
                                    {line}
                                </li>
                            })
                            .collect_view()}
                    </ul>
                }
                .into_any()
            }
        }}
        {super::manage::connect_sheet(workspace)}
    }
}

/// A searchable list of the connected providers' models with variant
/// chips: choosing a model adds it to the end of the field's fallback list,
/// choosing it again takes it out; the small and utility models may also
/// take the models of a field before them.
fn model_step(workspace: Workspace, slot: Slot, choice: RwSignal<Choice>) -> impl IntoView {
    let search = RwSignal::new(String::new());
    let field = field_name(slot.field());
    let current = move || {
        workspace
            .onboarding
            .status
            .get()
            .and_then(|status| configured_label(&status, slot.field()))
    };
    let position = move |chosen: &Choice, key: &cookie_agent_protocol::ModelKey| {
        chosen
            .iter()
            .position(|entry| matches!(entry, ModelChoice::Model { model, .. } if model == key))
    };
    let pick = move |model: &AvailableModelDescriptor| {
        choice.update(|chosen| {
            if let Some(index) = position(chosen, &model.key) {
                chosen.remove(index);
            } else if chosen.len() < cookie_agent_protocol::MAX_ONBOARDING_MODEL_ENTRIES {
                let variant = match slot {
                    Slot::Default => default_model_variant(model),
                    Slot::Small => small_model_variant(model),
                    Slot::Utility => utility_model_variant(model),
                };
                chosen.push(ModelChoice::Model {
                    model: model.key.clone(),
                    variant,
                });
            }
        });
    };
    let same_as = slot
        .earlier()
        .iter()
        .map(|earlier| {
            let earlier = *earlier;
            let entry = ModelChoice::Field(earlier);
            let on = {
                let entry = entry.clone();
                move || choice.with(|chosen| chosen.contains(&entry))
            };
            let toggle = move |_| {
                choice.update(|chosen| {
                    if let Some(index) = chosen.iter().position(|each| *each == entry) {
                        chosen.remove(index);
                    } else {
                        chosen.push(entry.clone());
                    }
                });
            };
            view! {
                <button
                    type="button"
                    class=BUTTON
                    class:bg-sel=on.clone()
                    aria-pressed=move || on().to_string()
                    data-same-as=field_name(earlier)
                    on:click=toggle
                >
                    {match earlier {
                        OnboardingModelField::DefaultModel => "Same as the default model",
                        _ => "Same as the small model",
                    }}
                </button>
            }
        })
        .collect_view();
    view! {
        <p class="text-sm">
            {match slot {
                Slot::Default => "The default model runs your agents. Choose more for fallbacks: the first available one runs, and the next take over when it fails.",
                Slot::Small => "Optional: the models of subagents that carry out work, such as the worker and the explorer. Without them they use the default model.",
                Slot::Utility => "Optional: fast, cheap models for approvals and session titles. Without them they use the session's own model.",
            }}
        </p>
        {move || current().map(|current| view! {
            <p class=NOTE data-current-model>{format!("{field} is {current}; choosing models replaces it.")}</p>
        })}
        {(!slot.earlier().is_empty()).then(|| view! { <div class="flex flex-wrap gap-2">{same_as}</div> })}
        {move || {
            let chosen = choice.get();
            (!chosen.is_empty()).then(|| view! {
                <p class="text-sm" data-chosen-list>
                    {format!(
                        "In fallback order: {}",
                        chosen.iter().map(choice_label).collect::<Vec<_>>().join(", then ")
                    )}
                </p>
            })
        }}
        <input
            type="search"
            class=FIELD
            data-model-search
            placeholder="Search models"
            prop:value=move || search.get()
            on:input=move |event| search.set(event_target_value(&event))
        />
        {move || {
            let models = workspace.models();
            let groups = model_groups(&models, &search.get());
            if groups.is_empty() {
                return view! {
                    <p class=NOTE>
                        {if models.is_empty() { "No model is available yet: connect a provider first." } else { "No model matches." }}
                    </p>
                }
                .into_any();
            }
            let chosen = choice.get();
            groups
                .into_iter()
                .map(|group| {
                    let rows = group
                        .models
                        .into_iter()
                        .map(|model| {
                            let index = position(&chosen, &model.key);
                            let selected = index.is_some();
                            let picked = model.clone();
                            let chips = index.map(|index| {
                                let chosen_variant = match &chosen[index] {
                                    ModelChoice::Model { variant, .. } => variant.clone(),
                                    ModelChoice::Field(_) => None,
                                };
                                variants(model)
                                    .into_iter()
                                    .map(|variant| {
                                        let on = chosen_variant.as_ref() == Some(&variant);
                                        let key = model.key.clone();
                                        let value = variant.clone();
                                        view! {
                                            <button
                                                type="button"
                                                class=BUTTON
                                                class:bg-sel=on
                                                aria-pressed=on.to_string()
                                                data-variant=variant.to_string()
                                                on:click=move |_| choice.update(|chosen| {
                                                    if let Some(index) = position(chosen, &key) {
                                                        chosen[index] = ModelChoice::Model {
                                                            model: key.clone(),
                                                            variant: Some(value.clone()),
                                                        };
                                                    }
                                                })
                                            >
                                                {variant.to_string()}
                                            </button>
                                        }
                                    })
                                    .collect_view()
                            });
                            view! {
                                <div class="flex flex-col gap-1">
                                    <button
                                        type="button"
                                        class=ROW
                                        class:bg-sel=selected
                                        aria-pressed=selected.to_string()
                                        data-model=model.key.to_string()
                                        data-position=index.map(|index| (index + 1).to_string())
                                        on:click=move |_| pick(&picked)
                                    >
                                        <span class="flex w-full items-center gap-2">
                                            <span class="min-w-0 flex-1 truncate text-sm font-bold">{model.display_name.clone()}</span>
                                            {index.map(|index| view! {
                                                <span class="inline-flex size-5 items-center justify-center rounded-full border border-current text-xs" title="Place in the fallback list">{index + 1}</span>
                                            })}
                                        </span>
                                        {(model.display_name.as_str() != model.key.to_string()).then(|| view! {
                                            <span class=format!("font-mono break-all {NOTE}") data-doc-id>{model.key.to_string()}</span>
                                        })}
                                        <span class=NOTE>{format!("{} tokens of context", cookie_agent_client_state::format::compact_count(model.capabilities.context_tokens))}</span>
                                    </button>
                                    {chips.map(|chips| view! { <div class="flex flex-wrap gap-2 px-3 pb-1">{chips}</div> })}
                                </div>
                            }
                        })
                        .collect_view();
                    view! {
                        <section class="flex flex-col gap-1" data-provider-group=group.provider.to_string()>
                            <h3 class="pt-2 text-xs font-bold uppercase tracking-wide text-muted">{group.provider.to_string()}</h3>
                            {rows}
                        </section>
                    }
                })
                .collect_view()
                .into_any()
        }}
    }
}

/// The run's summary and the first session.
fn summary(
    workspace: Workspace,
    result: &cookie_agent_protocol::OnboardingRunResult,
) -> impl IntoView {
    let lines = run_summary(result);
    let ready = result.status.ready;
    view! {
        <div class="flex min-h-0 flex-1 flex-col gap-2 overflow-y-auto px-4 py-3" id="onboarding-summary">
            <p class="text-sm font-bold">{if ready { "Cookie is set up." } else { "Saved." }}</p>
            <ul class="flex flex-col gap-1">
                {lines.into_iter().map(|line| view! { <li class="text-sm">{line}</li> }).collect_view()}
            </ul>
        </div>
        <footer class="flex flex-wrap gap-2 border-t border-line px-4 py-2">
            <button type="button" id="onboarding-first-session" class=PRIMARY on:click=move |_| workspace.start_first_session()>
                "Start your first session"
            </button>
            <button type="button" id="onboarding-done" class=BUTTON on:click=move |_| workspace.close_onboarding()>
                "Done"
            </button>
        </footer>
    }
}
