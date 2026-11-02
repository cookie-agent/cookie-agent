//! The first-run sheet (`docs/site/specs/default-models-and-onboarding.md`
//! section 9): a four-step stepper over the daemon's onboarding API. The
//! provider step is Settings' connect form; the three model steps are
//! searchable lists with variant chips that build each field's fallback list
//! (`docs/site/specs/model-fallback-lists.md` section 6), the small and
//! utility models optional. There is no starter kit: the daemon keeps the official agents
//! itself. Full screen on a phone.

use cookie_agent_client_state::onboarding::{
    ModelChoice, OnboardingStep, choice_entries, choice_label, configured_label,
    default_model_variant, field_name, finish_refusal, first_step, model_groups, run_summary,
    small_model_variant, utility_model_variant, variants,
};
use cookie_agent_protocol::{AvailableModelDescriptor, OnboardingModelField, OnboardingRunParams};
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
    let status = move || onboarding.status.get();
    let refusal = move || {
        status().and_then(|status| {
            finish_refusal(
                &status,
                default_choice.with(|choice| !choice.is_empty()),
                false,
            )
        })
    };
    let finish = move || {
        let models = workspace.models();
        let list = |choice: Choice| {
            choice_entries(&choice, |key| models.iter().find(|model| model.key == *key))
        };
        // The daemon fetches the official agents first when it has none.
        workspace.run_onboarding(OnboardingRunParams {
            install_kit: false,
            source: None,
            default_model: None,
            small_model: None,
            utility_model: None,
            default_model_list: list(default_choice.get_untracked()),
            small_model_list: list(small_choice.get_untracked()),
            utility_model_list: list(utility_choice.get_untracked()),
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
            <div class="flex h-full w-full min-w-0 flex-col bg-cream text-espresso sm:h-auto sm:max-h-[90vh] sm:max-w-2xl sm:rounded-xl sm:border sm:border-toasted dark:bg-night dark:text-latte sm:dark:border-ember">
                <header class="flex items-center gap-2 border-b border-toasted px-4 py-2 dark:border-ember">
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
                        <nav aria-label="Onboarding steps" class="grid grid-cols-2 gap-1 border-b border-toasted px-4 py-2 sm:flex sm:flex-wrap dark:border-ember">
                            {OnboardingStep::ALL
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
                                            class="inline-flex min-h-11 min-w-0 items-center gap-1.5 rounded-md px-2 text-left text-sm hover:bg-toasted active:bg-crust dark:hover:bg-ember dark:active:bg-smoke"
                                            class:font-bold=current
                                            class:bg-toasted=current
                                            class:dark:bg-ember=current
                                            aria-current=move || current().then_some("step")
                                            data-step=step_id(each)
                                            data-done=move || done().to_string()
                                            on:click=move |_| step.set(each)
                                        >
                                            {move || if done() {
                                                view! { <Glyph icon=icondata::FaCircleCheckSolid class="text-green-700 dark:text-green-400" /> }.into_any()
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
                                OnboardingStep::Provider => provider_step(workspace).into_any(),
                                OnboardingStep::DefaultModel => model_step(workspace, Slot::Default, default_choice).into_any(),
                                OnboardingStep::SmallModel => model_step(workspace, Slot::Small, small_choice).into_any(),
                                OnboardingStep::UtilityModel => model_step(workspace, Slot::Utility, utility_choice).into_any(),
                            }}
                        </div>
                        <footer class="flex flex-col gap-2 border-t border-toasted px-4 py-2 dark:border-ember">
                            {move || onboarding.error.get().map(|message| view! { <p class=ERROR id="onboarding-error">{message}</p> })}
                            <div class="flex flex-wrap items-center gap-2">
                                <button
                                    type="button"
                                    id="onboarding-back"
                                    class=BUTTON
                                    disabled=move || step.get().previous().is_none()
                                    on:click=move |_| {
                                        if let Some(previous) = step.get_untracked().previous() {
                                            step.set(previous);
                                        }
                                    }
                                >
                                    <Glyph icon=icondata::FaArrowLeftSolid />
                                    "Back"
                                </button>
                                <span class=format!("min-w-0 flex-1 {NOTE}") id="onboarding-refusal">
                                    {move || step.get().next().is_none().then(refusal).flatten()}
                                </span>
                                {move || match step.get().next() {
                                    Some(next) => view! {
                                        <button type="button" id="onboarding-next" class=PRIMARY on:click=move |_| step.set(next)>
                                            "Next"
                                            <Glyph icon=icondata::FaChevronRightSolid />
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
        OnboardingStep::Provider => "provider",
        OnboardingStep::DefaultModel => "default-model",
        OnboardingStep::SmallModel => "small-model",
        OnboardingStep::UtilityModel => "utility-model",
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
                                    <Glyph icon=icondata::FaPlugSolid class="text-green-700 dark:text-green-400" />
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
                    class:bg-toasted=on.clone()
                    class:dark:bg-ember=on.clone()
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
                                                class:bg-toasted=on
                                                class:dark:bg-ember=on
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
                                        class:bg-toasted=selected
                                        class:dark:bg-ember=selected
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
                            <h3 class="pt-2 text-xs font-bold uppercase tracking-wide text-ink dark:text-sand">{group.provider.to_string()}</h3>
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
        <footer class="flex flex-wrap gap-2 border-t border-toasted px-4 py-2 dark:border-ember">
            <button type="button" id="onboarding-first-session" class=PRIMARY on:click=move |_| workspace.start_first_session()>
                "Start your first session"
            </button>
            <button type="button" id="onboarding-done" class=BUTTON on:click=move |_| workspace.close_onboarding()>
                "Done"
            </button>
        </footer>
    }
}
