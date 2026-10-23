//! The first-run sheet (`docs/site/specs/default-models-and-onboarding.md`
//! section 9): a four-step stepper over the daemon's onboarding API. The
//! provider step is Settings' connect form; the model steps are searchable
//! lists with variant chips; the kit step previews what installing the
//! starter kit does to each file. Full screen on a phone.

use std::collections::HashSet;

use cookie_agent_client_state::onboarding::{
    OnboardingStep, archive_source, default_model_variant, finish_refusal, first_step,
    kit_file_status, model_groups, model_input, model_label, run_summary, small_model_variant,
    variants,
};
use cookie_agent_protocol::{
    AvailableModelDescriptor, ModelKey, OnboardingFileAction, OnboardingKitSource,
    OnboardingPreviewResult, OnboardingRunParams, OnboardingStatus, VariantId,
};
use leptos::{prelude::*, task::spawn_local};

use super::{BUTTON, ERROR, NOTE, PRIMARY, ROW, loading, manage::FIELD};
use crate::{icons::Glyph, workspace::Workspace};

/// A model picker's choice: the model and the variant chosen for it.
type Choice = Option<(ModelKey, Option<VariantId>)>;

/// The DOM ID of the hidden file input of "Use a downloaded kit…".
const KIT_INPUT: &str = "onboarding-kit-input";

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
    let default_choice = RwSignal::new(None::<(ModelKey, Option<VariantId>)>);
    let small_choice = RwSignal::new(None::<(ModelKey, Option<VariantId>)>);
    let kit = RwSignal::new(None::<Result<OnboardingPreviewResult, String>>);
    let kit_source = RwSignal::new(None::<OnboardingKitSource>);
    let install = RwSignal::new(true);
    let load_kit = move |source: Option<OnboardingKitSource>| {
        kit.set(None);
        kit_source.set(source.clone());
        spawn_local(async move {
            let preview = workspace.preview_kit(source).await;
            install.set(preview.as_ref().is_ok_and(|preview| {
                preview
                    .files
                    .iter()
                    .any(|file| file.action != OnboardingFileAction::Skip)
            }));
            kit.set(Some(preview));
        });
    };
    // The kit is read when its step first shows.
    Effect::new(move |_| {
        if step.get() == OnboardingStep::Kit && kit.with_untracked(Option::is_none) {
            load_kit(kit_source.get_untracked());
        }
    });
    let status = move || onboarding.status.get();
    let refusal = move || {
        status().and_then(|status| {
            finish_refusal(
                &status,
                default_choice.with(Option::is_some),
                small_choice.with(Option::is_some),
            )
        })
    };
    let finish = move || {
        let models = workspace.models();
        let input = |choice: Choice| {
            choice.and_then(|(key, variant)| {
                models
                    .iter()
                    .find(|model| model.key == key)
                    .map(|model| model_input(model, variant.as_ref()))
            })
        };
        let kit_ready = kit.with_untracked(|kit| matches!(kit, Some(Ok(_))));
        let install_kit = install.get_untracked() && kit_ready;
        workspace.run_onboarding(OnboardingRunParams {
            install_kit,
            source: if install_kit {
                kit_source.get_untracked()
            } else {
                None
            },
            default_model: input(default_choice.get_untracked()),
            small_model: input(small_choice.get_untracked()),
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
                                            || (each == OnboardingStep::DefaultModel && default_choice.with(Option::is_some))
                                            || (each == OnboardingStep::SmallModel && small_choice.with(Option::is_some))
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
                                OnboardingStep::DefaultModel => model_step(workspace, false, default_choice).into_any(),
                                OnboardingStep::SmallModel => model_step(workspace, true, small_choice).into_any(),
                                OnboardingStep::Kit => kit_step(kit, install, load_kit).into_any(),
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
                                    {move || (step.get() == OnboardingStep::Kit).then(refusal).flatten()}
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
        OnboardingStep::Kit => "kit",
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
/// chips. The small model's list suggests nothing.
fn model_step(workspace: Workspace, small: bool, choice: RwSignal<Choice>) -> impl IntoView {
    let search = RwSignal::new(String::new());
    let field = if small {
        "small_model"
    } else {
        "default_model"
    };
    let current = move || {
        workspace
            .onboarding
            .status
            .get()
            .and_then(|status: OnboardingStatus| {
                if small {
                    status.small_model
                } else {
                    status.default_model
                }
            })
    };
    let pick = move |model: &AvailableModelDescriptor| {
        let variant = if small {
            small_model_variant(model)
        } else {
            default_model_variant(model)
        };
        choice.set(Some((model.key.clone(), variant)));
    };
    view! {
        <p class="text-sm">
            {if small {
                "The small model runs quick work: approvals, titles, and the explorer. It may be the default model."
            } else {
                "The default model runs your agents."
            }}
        </p>
        {move || current().map(|current| view! {
            <p class=NOTE data-current-model>{format!("{field} is {}; choosing another replaces it.", model_label(&current))}</p>
        })}
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
                            let selected = chosen.as_ref().is_some_and(|(key, _)| *key == model.key);
                            let picked = model.clone();
                            let chips = selected.then(|| {
                                let chosen_variant = chosen.as_ref().and_then(|(_, variant)| variant.clone());
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
                                                on:click=move |_| choice.set(Some((key.clone(), Some(value.clone()))))
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
                                        on:click=move |_| pick(&picked)
                                    >
                                        <span class="flex w-full items-center gap-2">
                                            <span class="min-w-0 flex-1 truncate text-sm font-bold">{model.display_name.clone()}</span>
                                            {selected.then(|| view! { <Glyph icon=icondata::FaCheckSolid /> })}
                                        </span>
                                        <span class=format!("font-mono break-all {NOTE}")>{model.key.to_string()}</span>
                                        <span class=NOTE>{format!("{} tokens of context", crate::format::compact_count(model.capabilities.context_tokens))}</span>
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

/// What installing the starter kit does to each file, install or skip, and
/// a downloaded kit instead of the release's.
fn kit_step(
    kit: RwSignal<Option<Result<OnboardingPreviewResult, String>>>,
    install: RwSignal<bool>,
    load_kit: impl Fn(Option<OnboardingKitSource>) + Copy + 'static,
) -> impl IntoView {
    let opened = RwSignal::new(HashSet::<String>::new());
    let picked = move |event: leptos::ev::Event| {
        let input: web_sys::HtmlInputElement = event_target(&event);
        let Some(file) = crate::files::files_of(input.files()).into_iter().next() else {
            return;
        };
        input.set_value("");
        spawn_local(async move {
            match crate::files::read_file(&file).await {
                Ok(bytes) => load_kit(Some(archive_source(&bytes))),
                Err(error) => kit.set(Some(Err(error))),
            }
        });
    };
    view! {
        <p class="text-sm">"The starter kit: a commented config.toml, a primary agent with explore and worker subagents, and a skill for changing Cookie itself. Optional; your own files are never overwritten."</p>
        {move || match kit.get() {
            None => loading("Reading the starter kit…").into_any(),
            Some(Err(message)) => view! {
                <p class=ERROR id="onboarding-kit-error">{format!("The starter kit is not available: {message}")}</p>
            }
            .into_any(),
            Some(Ok(preview)) => view! {
                <p class=NOTE>{format!(
                    "Kit {} from {}",
                    preview.cookie_version.as_deref().map_or_else(|| format!("version {}", preview.kit_version), |version| format!("for cookie {version}")),
                    preview.origin,
                )}</p>
                <ul class="flex flex-col gap-1">
                    {preview
                        .files
                        .into_iter()
                        .map(|file| {
                            let path = file.path.clone();
                            let toggled = path.clone();
                            let shown = Memo::new(move |_| opened.with(|opened| opened.contains(&path)));
                            let action = match file.action {
                                OnboardingFileAction::Write => "write",
                                OnboardingFileAction::Skip => "skip",
                                OnboardingFileAction::WriteNew => "write_new",
                                OnboardingFileAction::Unknown => "unknown",
                            };
                            let diff = file.diff.clone();
                            view! {
                                <li class="flex flex-col gap-1 rounded-md border border-toasted p-2 dark:border-ember" data-kit-file=file.path.clone() data-kit-action=action>
                                    <span class="flex flex-wrap items-center gap-2">
                                        <span class="min-w-0 flex-1 break-all font-mono text-sm">{file.path.clone()}</span>
                                        <span class="shrink-0 rounded-full bg-toasted px-2 py-0.5 text-xs dark:bg-ember">{kit_file_status(&file)}</span>
                                    </span>
                                    {diff.map(|diff| view! {
                                        <button type="button" class=BUTTON data-kit-diff on:click=move |_| opened.update(|opened| {
                                            if !opened.remove(&toggled) {
                                                opened.insert(toggled.clone());
                                            }
                                        })>
                                            {move || if shown.get() { "Hide the diff" } else { "Show the diff" }}
                                        </button>
                                        <Show when=move || shown.get()>
                                            <pre class="max-h-64 overflow-auto rounded-md bg-parchment p-2 font-mono text-xs dark:bg-char">{diff.clone()}</pre>
                                        </Show>
                                    })}
                                </li>
                            }
                        })
                        .collect_view()}
                </ul>
                <div class="flex flex-wrap gap-2" role="group" aria-label="Install the starter kit">
                    <button type="button" id="onboarding-install" class=BUTTON class:bg-toasted=move || install.get() class:dark:bg-ember=move || install.get() aria-pressed=move || install.get().to_string() on:click=move |_| install.set(true)>
                        "Install the kit"
                    </button>
                    <button type="button" id="onboarding-skip" class=BUTTON class:bg-toasted=move || !install.get() class:dark:bg-ember=move || !install.get() aria-pressed=move || (!install.get()).to_string() on:click=move |_| install.set(false)>
                        "Skip it"
                    </button>
                </div>
            }
            .into_any(),
        }}
        <input type="file" id=KIT_INPUT class="hidden" accept=".gz,.tgz,application/gzip" on:change=picked />
        <button type="button" id="onboarding-kit-file" class=BUTTON on:click=move |_| {
            if let Some(input) = web_sys::window()
                .and_then(|window| window.document())
                .and_then(|document| document.get_element_by_id(KIT_INPUT))
                .and_then(|element| wasm_bindgen::JsCast::dyn_into::<web_sys::HtmlElement>(element).ok())
            {
                input.click();
            }
        }>
            <Glyph icon=icondata::FaFolderOpenRegular />
            "Use a downloaded kit…"
        </button>
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
