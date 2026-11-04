//! Settings › Configuration: `config.toml` as a form. The rules are
//! [`cookie_agent_client_state::config_form`]; this draws them.

use cookie_agent_client_state::{
    config_form::{
        ConfigField, FieldKind, FormField, FormManaged, FormSection, ManagedPanel, OtherEntry,
        models_from_value, models_to_value,
    },
    onboarding::{ModelChoice, choice_label},
};
use cookie_agent_protocol::{ModelKey, VariantId};
use leptos::prelude::*;
use serde_json::Value;

use super::{
    BUTTON, ERROR, NOTE, PRIMARY,
    manage::{EDITOR_ACTIONS, FIELD, ROW_ACTION},
    onboarding::{Slot, model_step},
};
use crate::{
    config_toml::{form_of, snippet},
    icons::Glyph,
    layout::{SettingsSection, View},
    workspace::Workspace,
};

const CARD: &str = "group/section rounded-xl bg-raised shadow-[0_0_0_1px_var(--color-line)]";
const SUMMARY: &str = "flex min-h-11 cursor-pointer list-none items-center gap-2 rounded-xl px-3 py-2.5 text-[0.8438rem] font-semibold transition-colors hover:bg-hover [&::-webkit-details-marker]:hidden";
const ROW_GRID: &str = "grid grid-cols-[minmax(0,1fr)_2.75rem] items-center gap-x-3 gap-y-1.5 border-t border-line px-3 py-2.5 lg:grid-cols-[minmax(0,1fr)_14.5rem_2.25rem]";
const CAPTION: &str =
    "px-0.5 pt-2.5 text-[0.6875rem] font-semibold tracking-wide text-muted uppercase";

/// The form, or why `config.toml` cannot be shown as one.
pub(super) fn config_form(workspace: Workspace) -> impl IntoView {
    let text = Memo::new(move |_| {
        workspace.config.with(|config| match config {
            Some(Ok(config)) => config
                .documents
                .iter()
                .find(|document| document.path == "config.toml")
                .map(|document| document.text.clone())
                .unwrap_or_default(),
            _ => String::new(),
        })
    });
    // Discarding draws every row afresh, closing the open pickers.
    let discarded = RwSignal::new(0_u32);
    let form = Memo::new(move |_| form_of(&text.get()));
    let body = move || {
        discarded.track();
        match form.get() {
            Err(error) => view! {
                <p class=ERROR id="config-form-error">{error}</p>
                <p class=NOTE>"Fix it with Edit the file; the settings return once it reads."</p>
            }
            .into_any(),
            Ok(form) => view! {
                <div class="flex flex-col gap-2.5" id="config-form">
                    {form.sections.iter().cloned().map(|section| section_card(workspace, section)).collect_view()}
                </div>
                <h2 class=CAPTION>"In their own panels"</h2>
                <div class="grid gap-2 sm:grid-cols-2" id="config-managed">
                    {form.managed.iter().cloned().map(|managed| managed_row(workspace, managed)).collect_view()}
                </div>
                {(!form.other.is_empty()).then(|| view! {
                    <h2 class=CAPTION>"Other settings"</h2>
                    <div class="overflow-hidden rounded-xl shadow-[0_0_0_1px_var(--color-line)]" id="config-other">
                        {form.other.iter().map(|entry| other_box(workspace, entry)).collect_view()}
                    </div>
                })}
            }
            .into_any(),
        }
    };
    let bar = move || {
        let Ok(form) = form.get() else {
            return None;
        };
        let drafts = workspace.config_form_drafts.get();
        let changes = form.changes(&drafts);
        let lines = match &changes {
            Ok(changes) => changes.iter().map(change_line).collect::<Vec<_>>(),
            Err(_) => Vec::new(),
        };
        let (count, blocked) = match &changes {
            Ok(changes) => (changes.len(), false),
            Err(errors) => (errors.len(), true),
        };
        (count > 0).then(|| {
            let summary = match (blocked, count) {
                (true, 1) => "1 value to fix".to_owned(),
                (true, count) => format!("{count} values to fix"),
                (false, 1) => "1 change".to_owned(),
                (false, count) => format!("{count} changes"),
            };
            view! {
                <div class=format!("{EDITOR_ACTIONS} mt-auto flex-col") id="config-form-bar">
                    <div class="mx-auto flex w-full max-w-3xl flex-col gap-1.5">
                        <div class="flex items-center gap-2">
                            <span class="mr-auto text-[0.8125rem] font-medium" id="config-form-count">{summary}</span>
                            <button
                                type="button"
                                id="config-form-apply"
                                class=PRIMARY
                                disabled=blocked
                                on:click=move |_| workspace.apply_config_form(&form)
                            >
                                "Apply"
                            </button>
                            <button
                                type="button"
                                id="config-form-discard"
                                class=BUTTON
                                on:click=move |_| {
                                    workspace.config_form_drafts.set(Default::default());
                                    discarded.update(|count| *count += 1);
                                }
                            >
                                "Discard"
                            </button>
                        </div>
                        <ul class="max-h-24 overflow-y-auto font-mono text-[0.7188rem] leading-relaxed text-muted wrap-anywhere">
                            {lines.into_iter().map(|line| view! { <li>{line}</li> }).collect_view()}
                        </ul>
                    </div>
                </div>
            }
        })
    };
    view! {
        {body}
        {bar}
    }
}

/// A `set` as a line for the bar.
fn change_line(change: &cookie_agent_protocol::ConfigChange) -> String {
    let cookie_agent_protocol::ConfigChange::Set { key, value } = change else {
        return String::new();
    };
    let key = key.join(".");
    if value.is_null() {
        format!("{key} removed")
    } else {
        format!("{key} = {value}")
    }
}

fn section_card(workspace: Workspace, section: FormSection) -> impl IntoView {
    let authored = section.authored();
    let set = section
        .fields
        .iter()
        .filter(|field| field.value.is_some())
        .count()
        + section.other.len();
    let other = section
        .other
        .iter()
        .map(|entry| other_box(workspace, entry))
        .collect_view();
    view! {
        <details class=CARD open=authored data-config-section=section.section.id>
            <summary class=SUMMARY>
                <Glyph icon=icondata::LuChevronRight class="text-xs text-faint transition-transform group-open/section:rotate-90" />
                {section.section.title}
                {(set > 0).then(|| view! { <span class="text-[0.6875rem] font-medium text-muted">{format!("{set} set in the file")}</span> })}
            </summary>
            {section.fields.iter().cloned().map(|field| field_row(workspace, field)).collect_view()}
            {other}
        </details>
    }
}

/// One setting: what it is, its control, and the way back to its default.
fn field_row(workspace: Workspace, form_field: FormField) -> impl IntoView {
    let field = form_field.field;
    let path = field.path();
    let draft = {
        let path = path.clone();
        Signal::derive(move || {
            workspace
                .config_form_drafts
                .with(|drafts| drafts.get(&path).cloned())
        })
    };
    let authored = form_field.value.is_some();
    let pending = {
        let form_field = form_field.clone();
        move || {
            draft
                .get()
                .is_some_and(|draft| matches!(form_field.change(&draft), Ok(Some(_))))
        }
    };
    let problem = {
        let form_field = form_field.clone();
        move || {
            draft
                .get()
                .and_then(|draft| form_field.change(&draft).err())
        }
    };
    let picker_open = RwSignal::new(false);
    let picker = RwSignal::new(Vec::<ModelChoice>::new());
    let resettable = field.kind != FieldKind::Secret;
    let reset = {
        let path = path.clone();
        move |_| {
            if field.kind == FieldKind::Models {
                picker_open.set(false);
            }
            workspace.config_form_drafts.update(|drafts| {
                if authored {
                    drafts.insert(path.clone(), String::new());
                } else {
                    drafts.remove(&path);
                }
            });
        }
    };
    let show_reset = move || {
        resettable
            && draft
                .get()
                .map_or(authored, |draft| !draft.trim().is_empty())
    };
    let control = control(workspace, &form_field, draft, picker, picker_open);
    let row_path = path.clone();
    let shown_path = path.clone();
    let highlight = pending.clone();
    let picker_panel = (field.kind == FieldKind::Models).then(|| {
        let slot = match field.key[0] {
            "small_model" => Slot::Small,
            "utility_model" => Slot::Utility,
            _ => Slot::Default,
        };
        let form_field = form_field.clone();
        let path = path.clone();
        // What the picker holds becomes the draft; a list that says what
        // the file says is none.
        Effect::watch(
            move || picker.get(),
            move |chosen, _, _| {
                if !picker_open.get_untracked() {
                    return;
                }
                let default_of = default_variants(workspace);
                let text = if chosen.is_empty() {
                    String::new()
                } else {
                    models_to_value(chosen, default_of).to_string()
                };
                let unchanged = form_field
                    .value
                    .as_ref()
                    .and_then(models_from_value)
                    .map_or_else(String::new, |current| {
                        models_to_value(&current, default_variants(workspace)).to_string()
                    });
                workspace.config_form_drafts.update(|drafts| {
                    if text == unchanged {
                        drafts.remove(&path);
                    } else {
                        drafts.insert(path.clone(), text);
                    }
                });
            },
            false,
        );
        move || {
            picker_open.get().then(|| view! {
                <div class="flex flex-col gap-2 border-t border-line bg-surface-2 px-3 py-3" data-model-picker>
                    {model_step(workspace, slot, picker, false)}
                    <div class="flex gap-2">
                        <button type="button" class=PRIMARY data-picker-done on:click=move |_| picker_open.set(false)>"Done"</button>
                        <button type="button" class=BUTTON data-picker-clear on:click=move |_| picker.set(Vec::new())>"Clear"</button>
                    </div>
                </div>
            })
        }
    });
    view! {
        <div data-config-field=row_path>
            <div class=ROW_GRID class:bg-sel=highlight>
                <div class="min-w-0">
                    <div class="flex flex-wrap items-center gap-2 text-[0.8125rem] font-medium">
                        {field.label}
                        {move || pending().then(|| view! { <span class="rounded-full bg-accent-soft px-2 text-[0.6875rem] font-medium text-accent">"not applied"</span> })}
                    </div>
                    <p class="text-xs text-muted">{field.help}</p>
                    <p class="font-mono text-[0.6875rem] text-faint">{shown_path}</p>
                </div>
                <div class="col-start-1 row-start-2 min-w-0 lg:col-start-2 lg:row-start-1">
                    {control}
                    {move || problem().map(|problem| view! { <p class="mt-1 text-xs text-bad" data-field-error>{problem}</p> })}
                </div>
                <div class="col-start-2 row-span-2 row-start-1 flex justify-center lg:col-start-3 lg:row-span-1">
                    {move || show_reset().then(|| {
                        let reset = reset.clone();
                        view! {
                            <button type="button" class=ROW_ACTION title="Back to the default: removes the key" aria-label="Back to the default" data-field-reset on:click=reset>
                                <Glyph icon=icondata::LuRotateCcw />
                            </button>
                        }
                    })}
                </div>
            </div>
            {picker_panel}
        </div>
    }
}

/// Looks up the default variant of a model the daemon offers.
fn default_variants(workspace: Workspace) -> impl Fn(&ModelKey) -> Option<VariantId> {
    let models = workspace.models();
    move |key| {
        models
            .iter()
            .find(|model| &model.key == key)
            .and_then(|model| model.default_variant.clone())
    }
}

/// Stores `text` as the draft of `field`, or none when it is what the file
/// holds.
fn set_draft(workspace: Workspace, field: &FormField, text: String) {
    let current = field.current_text();
    let path = field.field.path();
    workspace.config_form_drafts.update(|drafts| {
        if text == current {
            drafts.remove(&path);
        } else {
            drafts.insert(path, text);
        }
    });
}

fn control(
    workspace: Workspace,
    form_field: &FormField,
    draft: Signal<Option<String>>,
    picker: RwSignal<Vec<ModelChoice>>,
    picker_open: RwSignal<bool>,
) -> AnyView {
    let field: &'static ConfigField = form_field.field;
    let current = form_field.current_text();
    let for_change = form_field.clone();
    match field.kind {
        FieldKind::Secret => {
            let label = if form_field.value.is_some() {
                "•••••••• (set)"
            } else {
                "not set"
            };
            view! { <p class="font-mono text-[0.8125rem] text-muted lg:text-right">{label}</p> }
                .into_any()
        }
        FieldKind::Bool => {
            let shown = form_field.clone();
            view! {
                <label class="-ml-3 flex min-h-11 w-fit min-w-11 cursor-pointer items-center justify-center lg:ml-auto lg:-mr-3">
                    <input
                        type="checkbox"
                        class="hit-area h-5 w-5 accent-accent"
                        aria-label=field.label
                        prop:checked=move || shown.shown_text(draft.get().as_deref()) == "true"
                        on:change=move |event| set_draft(workspace, &for_change, event_target_checked(&event).to_string())
                    />
                </label>
            }
            .into_any()
        }
        FieldKind::Choice(options) => {
            let value = move || draft.get().unwrap_or_else(|| current.clone());
            let value = Signal::derive(value);
            let default = field.default_text().unwrap_or_default();
            view! {
                <select
                    class=FIELD
                    aria-label=field.label
                    on:change=move |event| set_draft(workspace, &for_change, event_target_value(&event))
                >
                    <option value="" prop:selected=move || value.get().is_empty()>{format!("default ({default})")}</option>
                    {options.iter().map(|&option| view! {
                        <option value=option prop:selected=move || value.get() == option>{option}</option>
                    }).collect_view()}
                </select>
            }
            .into_any()
        }
        FieldKind::Choices(options) => {
            let value = Signal::derive(move || draft.get().unwrap_or_else(|| current.clone()));
            let chosen = move || {
                value.with(|text| {
                    text.split(',')
                        .map(str::trim)
                        .filter(|name| !name.is_empty())
                        .map(str::to_owned)
                        .collect::<Vec<_>>()
                })
            };
            view! {
                <div class="flex flex-wrap gap-x-2.5 gap-y-1 font-mono text-xs">
                    {options.iter().map(|&option| {
                        let for_change = for_change.clone();
                        view! {
                            <label class="flex min-h-11 cursor-pointer items-center gap-1.5 lg:min-h-7">
                                <input
                                    type="checkbox"
                                    class="hit-area h-4 w-4 accent-accent"
                                    prop:checked=move || chosen().iter().any(|name| name == option)
                                    on:change=move |event| {
                                        let on = event_target_checked(&event);
                                        let mut names = chosen();
                                        names.retain(|name| name != option);
                                        if on {
                                            names.push(option.to_owned());
                                        }
                                        // In the list's order, so ticking and
                                        // unticking comes back to the file's.
                                        let ordered = options.iter().copied().filter(|name| names.iter().any(|chosen| chosen == name)).collect::<Vec<_>>();
                                        set_draft(workspace, &for_change, ordered.join(","));
                                    }
                                />
                                {option}
                            </label>
                        }
                    }).collect_view()}
                </div>
            }
            .into_any()
        }
        FieldKind::Models => {
            let shown = form_field.clone();
            let note = match field.default {
                cookie_agent_client_state::config_form::FieldDefault::Note(note) => note,
                cookie_agent_client_state::config_form::FieldDefault::Value(value) => value,
            };
            let chosen_now = move || {
                let text = draft
                    .get_untracked()
                    .unwrap_or_else(|| shown.current_text());
                serde_json::from_str::<Value>(&text)
                    .ok()
                    .as_ref()
                    .and_then(models_from_value)
                    .unwrap_or_default()
            };
            let label_of = {
                let form_field = form_field.clone();
                move || {
                    let text = draft.get().unwrap_or_else(|| form_field.current_text());
                    serde_json::from_str::<Value>(&text)
                        .ok()
                        .as_ref()
                        .and_then(models_from_value)
                        .filter(|choices| !choices.is_empty())
                        .map(|choices| {
                            choices
                                .iter()
                                .map(choice_label)
                                .collect::<Vec<_>>()
                                .join(", then ")
                        })
                }
            };
            let has_value = {
                let label_of = label_of.clone();
                move || label_of().is_some()
            };
            view! {
                <div class="flex flex-col items-start gap-1 lg:items-end">
                    <p class="font-mono text-xs wrap-anywhere lg:text-right" class:text-faint=move || !has_value() data-model-label>
                        {move || label_of().unwrap_or_else(|| note.to_owned())}
                    </p>
                    <button
                        type="button"
                        class=BUTTON
                        data-model-change
                        on:click=move |_| {
                            if picker_open.get_untracked() {
                                picker_open.set(false);
                                return;
                            }
                            // A model's variant shows on its chips, the
                            // default's included.
                            let models = workspace.models();
                            let chosen = chosen_now().into_iter().map(|choice| match choice {
                                ModelChoice::Model { model, variant: None } => {
                                    let variant = models.iter().find(|each| each.key == model).and_then(|each| each.default_variant.clone());
                                    ModelChoice::Model { model, variant }
                                }
                                other => other,
                            }).collect();
                            picker.set(chosen);
                            picker_open.set(true);
                        }
                    >
                        "Change"
                    </button>
                </div>
            }
            .into_any()
        }
        FieldKind::Integer | FieldKind::Number | FieldKind::Text => {
            let placeholder = match field.default {
                cookie_agent_client_state::config_form::FieldDefault::Value(value)
                | cookie_agent_client_state::config_form::FieldDefault::Note(value) => value,
            };
            let numeric = matches!(field.kind, FieldKind::Integer | FieldKind::Number);
            view! {
                <input
                    type="text"
                    class=FIELD
                    aria-label=field.label
                    autocomplete="off"
                    spellcheck="false"
                    inputmode=if numeric { "numeric" } else { "text" }
                    placeholder=placeholder
                    prop:value=move || draft.get().unwrap_or_else(|| current.clone())
                    on:input=move |event| set_draft(workspace, &for_change, event_target_value(&event))
                />
            }
            .into_any()
        }
    }
}

/// A table another panel manages.
fn managed_row(workspace: Workspace, managed: FormManaged) -> impl IntoView {
    let (place, open): (&str, fn(Workspace)) = match managed.table.panel {
        ManagedPanel::Providers => ("Settings › Providers", |workspace| {
            workspace.open_view(View::Settings, Some(SettingsSection::Providers));
        }),
        ManagedPanel::Mcp => ("MCP", |workspace| workspace.open_view(View::Mcp, None)),
        ManagedPanel::Plugins => ("Settings › Plugins", |workspace| {
            workspace.open_view(View::Settings, Some(SettingsSection::Plugins));
        }),
        ManagedPanel::Machines => ("Machines", |workspace| {
            workspace.open_view(View::Machines, None);
        }),
    };
    let count = match managed.count {
        0 => "none in the file".to_owned(),
        count => format!("{count} in the file"),
    };
    view! {
        <button
            type="button"
            class="flex items-center justify-between gap-2 rounded-[0.625rem] bg-raised px-3 py-2 text-left shadow-[0_0_0_1px_var(--color-line)] transition-colors hover:bg-hover active:bg-press"
            data-config-managed=managed.table.label
            on:click=move |_| open(workspace)
        >
            <span class="min-w-0">
                <span class="block text-[0.8125rem] font-medium">{managed.table.label}</span>
                <span class="block text-[0.6875rem] text-muted">{format!("{count} · {place}")}</span>
            </span>
            <Glyph icon=icondata::LuChevronRight class="text-faint" />
        </button>
    }
}

/// A key the form has no control for, as the file holds it.
fn other_box(workspace: Workspace, entry: &OtherEntry) -> impl IntoView + use<> {
    let key = entry.key.join(".");
    view! {
        <div class="border-t border-line bg-surface-2 px-3 py-2.5 first:border-t-0" data-config-other=key>
            <div class="mb-1 flex items-center justify-between gap-2">
                <span class="text-[0.6875rem] text-muted">"Shown as written; change it in the file"</span>
                <button
                    type="button"
                    class="min-h-11 min-w-11 text-[0.6875rem] font-medium text-accent hover:underline lg:min-h-0 lg:min-w-0"
                    on:click=move |_| workspace.edit_config("config.toml")
                >
                    "Edit in file"
                </button>
            </div>
            <pre class="font-mono text-xs leading-relaxed whitespace-pre-wrap text-fg-2 wrap-anywhere">{snippet(&entry.key, &entry.value)}</pre>
        </div>
    }
}
