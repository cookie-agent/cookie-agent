//! The webhook form (`docs/site/specs/webhooks-view.md`, H4 and section
//! 6): presets to start from, then four steps (name and target, what the
//! agent is told, who may send it, which requests count) and Advanced.
//! It fills a [`WebhookDraft`], which checks and sends it.

use cookie_agent_client_state::{
    cron::delivery_label,
    session_actions::permission_mode_label,
    webhook::{
        WebhookDraft, WebhookDraftTarget, parse_verify, target_label, verify_text, webhook_title,
    },
    webhook_view::{
        Condition, Preset, ThreadBy, conditions, conditions_text, thread_by, thread_key_text,
    },
};
use cookie_agent_protocol::{
    PermissionMode, ProducerDeliveryMode, WebhookSignatureEncoding, WebhookVerify,
    WebhookVerifyKind,
};
use leptos::prelude::*;

use super::{
    BUTTON, ERROR, ICON_BUTTON, NOTE, PRIMARY,
    cron::SELECT,
    manage::{EDITOR_ACTIONS, EDITOR_PAGE, FIELD},
};
use crate::{
    cron::{model_pins, session_name},
    icons::Glyph,
    modes::MODES,
    workspace::{WebhookForm, Workspace},
};

/// A text area, as the form's long fields.
const AREA: &str = "min-h-[5rem] w-full resize-y rounded-lg bg-surface p-2 text-sm hover:shadow-[inset_0_0_0_1px_var(--color-faint)] focus:shadow-[inset_0_0_0_1px_var(--color-accent),0_0_0_3px_var(--color-accent-soft)] focus:outline-none shadow-[inset_0_0_0_1px_var(--color-line-2)]";

/// A step's heading.
const STEP: &str = "pt-4 pb-1 text-xs font-semibold tracking-wide text-muted uppercase";

/// A step's rows: the label on the left in a wide window, above on a phone.
const GRID: &str = "grid items-start gap-x-4 gap-y-2 sm:grid-cols-[9.5rem_1fr]";

/// A row's label in [`GRID`].
const LABEL: &str = "pt-2.5 text-[0.8125rem] font-medium text-fg-2 sm:pt-2";

/// One option of a segmented choice.
const SEGMENT: &str = "inline-flex min-h-11 items-center rounded-md px-3 text-[0.8125rem] text-muted transition-colors hover:text-fg aria-checked:bg-raised aria-checked:font-semibold aria-checked:text-fg aria-checked:shadow-[0_0_0_1px_var(--color-line-2),0_1px_2px_rgb(0_0_0/0.06)] lg:min-h-8";

/// The segmented choice around [`SEGMENT`]s.
const SEGMENTS: &str = "inline-flex w-fit shrink-0 gap-0.5 rounded-lg bg-sunken p-0.5";

/// A condition of the filter, as its builder edits it.
#[derive(Clone, Copy)]
struct Row {
    key: usize,
    header: RwSignal<bool>,
    name: RwSignal<String>,
    values: RwSignal<Vec<String>>,
    /// The value being typed, added on Enter, a comma, or leaving.
    typing: RwSignal<String>,
}

impl Row {
    fn new(key: usize, condition: Condition) -> Self {
        Self {
            key,
            header: RwSignal::new(condition.header),
            name: RwSignal::new(condition.name),
            values: RwSignal::new(condition.values),
            typing: RwSignal::new(String::new()),
        }
    }

    /// The condition, with what is being typed added.
    fn condition(self) -> Condition {
        let mut values = self.values.get_untracked();
        let typing = self.typing.get_untracked();
        values.extend(
            typing
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned),
        );
        Condition {
            header: self.header.get_untracked(),
            name: self.name.get_untracked(),
            values,
        }
    }

    /// Adds what is being typed as values.
    fn commit(self) {
        let typed = self.typing.get_untracked();
        let new: Vec<String> = typed
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .collect();
        if !new.is_empty() {
            self.values.update(|values| {
                for value in new {
                    if !values.contains(&value) {
                        values.push(value);
                    }
                }
            });
        }
        self.typing.set(String::new());
    }
}

/// The check the form's list shows: none, one of the kinds.
fn kind_id(kind: Option<WebhookVerifyKind>) -> &'static str {
    kind.map_or("", WebhookVerifyKind::as_str)
}

/// The webhook form: a new webhook, or an edit of one.
#[allow(clippy::too_many_lines, reason = "one form, step by step")]
pub(super) fn webhook_form(workspace: Workspace, form: WebhookForm) -> impl IntoView {
    let WebhookForm {
        editing,
        draft,
        session,
        template,
    } = form;
    let creating = editing.is_none();
    let name = RwSignal::new(draft.name.clone());
    let prompt = RwSignal::new(draft.prompt.clone());
    let thread_prompt = RwSignal::new(draft.thread_prompt.clone());
    let (by, by_value) = thread_by(&draft.thread_key);
    let thread_by_choice = RwSignal::new(by);
    let thread_value = RwSignal::new(by_value);
    let title = RwSignal::new(draft.title.clone());
    let title_auto = RwSignal::new(draft.title_auto);
    let delivery = RwSignal::new(draft.delivery);
    let verify = parse_verify(&draft.verify).ok().flatten();
    let kind = RwSignal::new(verify.as_ref().map(|verify| verify.kind));
    let first_kind = kind.get_untracked();
    let hmac_header = RwSignal::new(
        verify
            .as_ref()
            .and_then(|verify| verify.header.clone())
            .unwrap_or_default(),
    );
    let hmac_encoding = RwSignal::new(
        verify
            .as_ref()
            .and_then(|verify| verify.encoding)
            .unwrap_or(WebhookSignatureEncoding::Hex),
    );
    let hmac_prefix = RwSignal::new(
        verify
            .as_ref()
            .and_then(|verify| verify.prefix.clone())
            .unwrap_or_default(),
    );
    let secret = RwSignal::new(String::new());
    let own_secret = RwSignal::new(false);
    // The filter as conditions, or as text when it does not read as them.
    let next_key = StoredValue::new(0usize);
    let row = move |condition: Condition| {
        let key = next_key.get_value();
        next_key.set_value(key + 1);
        Row::new(key, condition)
    };
    let (rows, as_text) = match conditions(&draft.filter) {
        Ok(read) => (read.into_iter().map(row).collect::<Vec<_>>(), false),
        Err(_) => (Vec::new(), true),
    };
    let rows = RwSignal::new(rows);
    let filter_text = RwSignal::new(draft.filter.clone());
    let filter_as_text = RwSignal::new(as_text);
    let filter_error = RwSignal::new(None::<String>);
    let headers = RwSignal::new(draft.headers.clone());
    let max_per_hour = RwSignal::new(draft.max_per_hour.clone());
    let max_concurrent = RwSignal::new(draft.max_concurrent.clone());
    let max_failures = RwSignal::new(draft.max_consecutive_failures.clone());
    let new = RwSignal::new(draft.target.is_new());
    let agent = RwSignal::new(if draft.agent.is_empty() {
        template.agent.to_string()
    } else {
        draft.agent.clone()
    });
    let model = RwSignal::new(draft.model.clone());
    let mode = RwSignal::new(draft.permission_mode);
    let preset = RwSignal::new(None::<Preset>);
    let threaded = move || new.get() && thread_by_choice.get() != ThreadBy::Event;
    let target_note = editing.as_ref().map(|webhook| {
        workspace.projects.with_untracked(|projects| {
            target_label(&webhook.target, projects, |id| {
                workspace
                    .sessions
                    .with_untracked(|sessions| session_name(sessions, id))
            })
        })
    });
    let this_session = session.map(|(id, subagent)| {
        let kind = if subagent {
            "This subagent"
        } else {
            "This session"
        };
        workspace
            .sessions
            .with_untracked(|sessions| session_name(sessions, id))
            .map_or_else(|| kind.to_owned(), |name| format!("{kind}: {name}"))
    });
    let agents = {
        let mut agents: Vec<String> = untrack(|| workspace.selectable_agents())
            .into_iter()
            .map(|agent| agent.id.to_string())
            .collect();
        if !agents.contains(&agent.get_untracked()) {
            agents.insert(0, agent.get_untracked());
        }
        agents
    };
    let pins = {
        let mut pins = model_pins(&untrack(|| workspace.models()));
        let current = model.get_untracked();
        if !current.is_empty() && !pins.contains(&current) {
            pins.insert(0, current);
        }
        pins
    };
    let apply_preset = move |chosen: Preset| {
        let fields = chosen.fields();
        preset.set(Some(chosen));
        let verify = parse_verify(&fields.verify).ok().flatten();
        kind.set(verify.map(|verify| verify.kind));
        filter_as_text.set(false);
        filter_error.set(None);
        rows.set(
            conditions(&fields.filter)
                .unwrap_or_default()
                .into_iter()
                .map(row)
                .collect(),
        );
        filter_text.set(fields.filter);
        headers.set(fields.headers);
        if chosen.starts_sessions() {
            new.set(true);
        }
        let (by, value) = thread_by(&fields.thread_key);
        thread_by_choice.set(by);
        thread_value.set(value);
        // A title typed in stays; one a preset suggested follows the next.
        let suggested = title.with_untracked(|title| {
            title.is_empty()
                || Preset::ALL
                    .iter()
                    .any(|preset| preset.fields().title == *title)
        });
        if suggested {
            title.set(fields.title);
        }
    };
    let filter_lines = move || {
        if filter_as_text.get_untracked() {
            filter_text.get_untracked()
        } else {
            conditions_text(
                &rows
                    .get_untracked()
                    .into_iter()
                    .map(Row::condition)
                    .collect::<Vec<_>>(),
            )
        }
    };
    let build = {
        let draft = draft.clone();
        let template = template.clone();
        move || {
            let verify = kind.get_untracked().map(|kind| {
                let mut verify = WebhookVerify::of_kind(kind);
                if kind == WebhookVerifyKind::HmacSha256 {
                    let header = hmac_header.get_untracked();
                    verify.header = (!header.trim().is_empty()).then(|| header.trim().to_owned());
                    verify.encoding = Some(hmac_encoding.get_untracked());
                    let prefix = hmac_prefix.get_untracked();
                    verify.prefix = (!prefix.is_empty()).then_some(prefix);
                }
                verify
            });
            let is_new = new.get_untracked();
            let mut draft = WebhookDraft {
                name: name.get_untracked(),
                prompt: prompt.get_untracked(),
                thread_key: if is_new {
                    thread_key_text(
                        thread_by_choice.get_untracked(),
                        &thread_value.get_untracked(),
                    )
                } else {
                    String::new()
                },
                thread_prompt: if is_new {
                    thread_prompt.get_untracked()
                } else {
                    String::new()
                },
                title: if is_new {
                    title.get_untracked()
                } else {
                    String::new()
                },
                title_auto: is_new && title_auto.get_untracked(),
                delivery: delivery.get_untracked(),
                verify: verify_text(verify.as_ref()),
                secret: if own_secret.get_untracked() {
                    secret.get_untracked()
                } else {
                    String::new()
                },
                filter: filter_lines(),
                headers: headers.get_untracked(),
                max_per_hour: max_per_hour.get_untracked(),
                max_concurrent: if is_new {
                    max_concurrent.get_untracked()
                } else {
                    String::new()
                },
                max_consecutive_failures: max_failures.get_untracked(),
                permission_mode: mode.get_untracked(),
                model: model.get_untracked(),
                agent: agent.get_untracked(),
                ..draft.clone()
            };
            if creating {
                draft.target = match (is_new, session) {
                    (false, Some((session, subagent))) => {
                        WebhookDraftTarget::Session { session, subagent }
                    }
                    _ => WebhookDraftTarget::New {
                        template: template.clone(),
                    },
                };
            }
            draft
        }
    };
    let heading = match &editing {
        Some(webhook) => format!("Edit {}", webhook_title(webhook)),
        None => "New webhook".to_owned(),
    };
    let text_field =
        move |id: &'static str, value: RwSignal<String>, placeholder: &'static str, mono: bool| {
            view! {
                <input
                    type="text"
                    id=id
                    class=if mono { FIELD.to_owned() } else { format!("{FIELD} font-sans") }
                    placeholder=placeholder
                    prop:value=move || value.get()
                    on:input=move |event| value.set(event_target_value(&event))
                />
            }
        };
    let segment =
        move |id: &'static str, label: &'static str, on: Signal<bool>, pick: Callback<()>| {
            view! {
                <button
                    type="button"
                    role="radio"
                    id=id
                    class=SEGMENT
                    aria-checked=move || on.get().to_string()
                    on:click=move |_| pick.run(())
                >
                    {label}
                </button>
            }
        };
    let presets = creating.then(|| {
        view! {
            <div class="grid grid-cols-2 gap-2 sm:grid-cols-4" role="radiogroup" aria-label="Start from" data-webhook-presets>
                {Preset::ALL.map(|choice| view! {
                    <button
                        type="button"
                        role="radio"
                        id=format!("webhook-preset-{}", choice.id())
                        class="flex min-h-11 flex-col items-start gap-1 rounded-xl bg-raised p-3 text-left shadow-[0_0_0_1px_var(--color-line-2)] transition-shadow hover:bg-hover aria-checked:bg-sel aria-checked:shadow-[0_0_0_1.5px_var(--color-accent)]"
                        aria-checked=move || (preset.get() == Some(choice)).to_string()
                        on:click=move |_| apply_preset(choice)
                    >
                        <Glyph icon=match choice {
                            Preset::Github => icondata::LuGithub,
                            Preset::Standard => icondata::LuShieldCheck,
                            Preset::Bearer => icondata::LuKeyRound,
                            Preset::Anything => icondata::LuBraces,
                        } />
                        <span class="text-sm font-semibold">{choice.label()}</span>
                        <span class="text-xs text-muted">{choice.blurb()}</span>
                    </button>
                })}
            </div>
        }
    });
    let target = match (target_note, &this_session) {
        (Some(target), _) => view! {
            <p class="pt-2 text-sm" id="webhook-target">{target}</p>
        }
        .into_any(),
        // Nothing to choose from: new sessions.
        (None, None) => view! {
            <p class="pt-2 text-sm" id="webhook-target">"New sessions, one per event or per thread key"</p>
        }
        .into_any(),
        (None, Some(_)) => view! {
            <div class=SEGMENTS role="radiogroup" aria-label="Deliver into">
                {this_session.clone().map(|label| segment(
                    "webhook-target-session",
                    // The label names the session; the segment keeps short.
                    if label.starts_with("This subagent") { "This subagent" } else { "This session" },
                    Signal::derive(move || !new.get()),
                    Callback::new(move |()| new.set(false)),
                ))}
                {segment(
                    "webhook-target-new",
                    "New sessions",
                    Signal::derive(move || new.get()),
                    Callback::new(move |()| new.set(true)),
                )}
            </div>
        }
        .into_any(),
    };
    let session_label = this_session.clone();
    let example = move || {
        let named = title.with(|title| title.trim().to_owned());
        let named = if named.is_empty() {
            let name = name.with(|name| name.trim().to_owned());
            if name.is_empty() {
                "the webhook's name".to_owned()
            } else {
                name
            }
        } else {
            named
        };
        let suffix = match thread_by_choice.get() {
            ThreadBy::Event => "the time it arrived".to_owned(),
            ThreadBy::Json | ThreadBy::Header => {
                let value = thread_value.with(|value| value.trim().to_owned());
                if value.is_empty() {
                    "its key".to_owned()
                } else {
                    format!("its {}", value.trim_start_matches('/').replace('/', "."))
                }
            }
        };
        format!("{named} · {suffix}")
    };
    let add_condition = move |_| {
        rows.update(|rows| {
            rows.push(row(Condition {
                header: true,
                name: String::new(),
                values: Vec::new(),
            }));
        });
    };
    let toggle_text = move |_| {
        if filter_as_text.get_untracked() {
            match conditions(&filter_text.get_untracked()) {
                Ok(read) => {
                    rows.set(read.into_iter().map(row).collect());
                    filter_error.set(None);
                    filter_as_text.set(false);
                }
                Err(error) => filter_error.set(Some(error)),
            }
        } else {
            filter_text.set(filter_lines());
            filter_as_text.set(true);
        }
    };
    view! {
        <div class=EDITOR_PAGE id="webhook-form">
            <div class="flex flex-col gap-1 pb-2">
                <h2 class="text-xl font-semibold tracking-tight">{heading}</h2>
                {creating.then(|| view! {
                    <p class="text-[0.8125rem] text-muted">"Start from what will send it; every field stays editable."</p>
                })}
            </div>
            {presets}

            <h3 class=STEP>"1 · Name and target"</h3>
            <div class=GRID>
                <label class=LABEL for="webhook-name">"Name"</label>
                {text_field("webhook-name", name, "GitHub issues", false)}
                <span class=LABEL>"Deliver into"</span>
                <div class="flex flex-col gap-1">
                    {target}
                    {session_label.map(|label| view! {
                        <Show when=move || !new.get()>
                            <span class=NOTE>{label.clone()}</span>
                        </Show>
                    })}
                </div>
                <Show when=move || new.get()>
                    <span class=LABEL>"Agent, model, mode"</span>
                    <div class="grid gap-2 sm:grid-cols-3">
                        <select id="webhook-agent" class=SELECT aria-label="Agent" on:change=move |event| agent.set(event_target_value(&event))>
                            {agents.clone().into_iter().map(|id| {
                                let (chosen, value) = (id.clone(), id.clone());
                                view! { <option value=value selected=move || agent.get() == chosen>{id}</option> }
                            }).collect_view()}
                        </select>
                        <select id="webhook-model" class=SELECT aria-label="Model" on:change=move |event| model.set(event_target_value(&event))>
                            <option value="" selected=move || model.with(String::is_empty)>"the agent's own models"</option>
                            {pins.clone().into_iter().map(|pin| {
                                let (chosen, value) = (pin.clone(), pin.clone());
                                view! { <option value=value selected=move || model.get() == chosen>{pin}</option> }
                            }).collect_view()}
                        </select>
                        <select
                            id="webhook-mode"
                            class=SELECT
                            aria-label="Permission mode"
                            on:change=move |event| {
                                let value = event_target_value(&event);
                                mode.set(MODES.iter().map(|choice| choice.mode).find(|choice| permission_mode_label(*choice) == value));
                            }
                        >
                            <option value="" selected=move || mode.get().is_none()>
                                {format!("{} (the default)", permission_mode_label(PermissionMode::AutoApproveN))}
                            </option>
                            {MODES.iter().map(|choice| {
                                let value = choice.mode;
                                view! {
                                    <option value=permission_mode_label(value) selected=move || mode.get() == Some(value)>
                                        {format!("{} — {}", permission_mode_label(value), choice.description)}
                                    </option>
                                }
                            }).collect_view()}
                        </select>
                    </div>
                    <span class=LABEL>"One session per"</span>
                    <div class="flex flex-col gap-2 sm:flex-row sm:items-center">
                        <div class=SEGMENTS role="radiogroup" aria-label="One session per">
                            {segment("webhook-thread-by-json", "JSON field", Signal::derive(move || thread_by_choice.get() == ThreadBy::Json), Callback::new(move |()| thread_by_choice.set(ThreadBy::Json)))}
                            {segment("webhook-thread-by-header", "Header", Signal::derive(move || thread_by_choice.get() == ThreadBy::Header), Callback::new(move |()| thread_by_choice.set(ThreadBy::Header)))}
                            {segment("webhook-thread-by-event", "Event", Signal::derive(move || thread_by_choice.get() == ThreadBy::Event), Callback::new(move |()| thread_by_choice.set(ThreadBy::Event)))}
                        </div>
                        <Show when=move || thread_by_choice.get() != ThreadBy::Event>
                            <input
                                type="text"
                                id="webhook-thread-key"
                                class=FIELD
                                aria-label="Thread key"
                                placeholder=move || if thread_by_choice.get() == ThreadBy::Json { "/issue/number" } else { "X-Thread" }
                                prop:value=move || thread_value.get()
                                on:input=move |event| thread_value.set(event_target_value(&event))
                            />
                        </Show>
                    </div>
                    <span class=LABEL>"Session title"</span>
                    <div class="flex flex-col gap-1.5">
                        <input
                            type="text"
                            id="webhook-title"
                            class=format!("{FIELD} font-sans")
                            maxlength="96"
                            placeholder="the webhook's name when empty"
                            class:opacity-60=move || title_auto.get()
                            prop:value=move || title.get()
                            on:input=move |event| title.set(event_target_value(&event))
                        />
                        <span class=NOTE data-webhook-title-example>
                            {move || if title_auto.get() {
                                "The model names each session after its first turn.".to_owned()
                            } else {
                                format!("Sessions read “{}”.", example())
                            }}
                        </span>
                        <button
                            type="button"
                            role="switch"
                            id="webhook-title-auto"
                            class="inline-flex min-h-11 w-fit items-center gap-2 rounded-lg pr-2 text-[0.8125rem] text-fg-2 hover:text-fg lg:min-h-8"
                            aria-checked=move || title_auto.get().to_string()
                            on:click=move |_| title_auto.update(|auto| *auto = !*auto)
                        >
                            <span class="effort-switch" class:effort-switch-on=move || title_auto.get() aria-hidden="true"></span>
                            "Let the model name it after its first turn"
                        </button>
                    </div>
                </Show>
            </div>

            <h3 class=STEP>"2 · What the agent is told"</h3>
            <div class=GRID>
                <Show when=move || new.get()>
                    <label class=LABEL for="webhook-thread-prompt">"When a session starts"</label>
                    <textarea
                        id="webhook-thread-prompt"
                        class=AREA
                        placeholder="optional; sent as your message when a session starts, and again after each compaction"
                        prop:value=move || thread_prompt.get()
                        on:input=move |event| thread_prompt.set(event_target_value(&event))
                    ></textarea>
                </Show>
                <label class=LABEL for="webhook-prompt">"With every event"</label>
                <textarea
                    id="webhook-prompt"
                    class=AREA
                    placeholder="optional; sent with every event, inside its webhook_event block"
                    prop:value=move || prompt.get()
                    on:input=move |event| prompt.set(event_target_value(&event))
                ></textarea>
            </div>

            <h3 class=STEP>"3 · Who may send it"</h3>
            <div class=GRID>
                <label class=LABEL for="webhook-verify">"Check"</label>
                <select
                    id="webhook-verify"
                    class=SELECT
                    on:change=move |event| {
                        let value = event_target_value(&event);
                        kind.set(match value.as_str() {
                            "github" => Some(WebhookVerifyKind::Github),
                            "standard" => Some(WebhookVerifyKind::Standard),
                            "bearer" => Some(WebhookVerifyKind::Bearer),
                            "hmac_sha256" => Some(WebhookVerifyKind::HmacSha256),
                            _ => None,
                        });
                    }
                >
                    {[
                        (None, "None: the URL is the key"),
                        (Some(WebhookVerifyKind::Github), "GitHub signature (X-Hub-Signature-256)"),
                        (Some(WebhookVerifyKind::Standard), "Standard Webhooks signature"),
                        (Some(WebhookVerifyKind::Bearer), "Bearer token (Authorization header)"),
                        (Some(WebhookVerifyKind::HmacSha256), "Custom HMAC-SHA256"),
                    ].map(|(choice, label)| view! {
                        <option value=kind_id(choice) selected=move || kind.get() == choice>{label}</option>
                    })}
                </select>
                <Show when=move || kind.get() == Some(WebhookVerifyKind::HmacSha256)>
                    <span class=LABEL>"Signature"</span>
                    <div class="grid gap-2 sm:grid-cols-3">
                        <input type="text" id="webhook-hmac-header" class=FIELD aria-label="Signature header" placeholder="X-Signature"
                            prop:value=move || hmac_header.get() on:input=move |event| hmac_header.set(event_target_value(&event)) />
                        <select id="webhook-hmac-encoding" class=SELECT aria-label="Encoding" on:change=move |event| hmac_encoding.set(
                            if event_target_value(&event) == "base64" { WebhookSignatureEncoding::Base64 } else { WebhookSignatureEncoding::Hex })>
                            <option value="hex" selected=move || hmac_encoding.get() == WebhookSignatureEncoding::Hex>"hex"</option>
                            <option value="base64" selected=move || hmac_encoding.get() == WebhookSignatureEncoding::Base64>"base64"</option>
                        </select>
                        <input type="text" id="webhook-hmac-prefix" class=FIELD aria-label="Prefix" placeholder="prefix, such as sha256="
                            prop:value=move || hmac_prefix.get() on:input=move |event| hmac_prefix.set(event_target_value(&event)) />
                    </div>
                </Show>
                <Show when=move || kind.get().is_some()>
                    <span class=LABEL>"Secret"</span>
                    <div class="flex flex-col gap-2 sm:flex-row sm:items-center">
                        <Show
                            when=move || own_secret.get()
                            fallback=move || view! {
                                <span class="min-h-11 flex-1 content-center rounded-lg bg-sunken px-3 text-[0.8125rem] text-muted lg:min-h-9">
                                    {move || if creating || kind.get() != first_kind {
                                        "cookie makes one when you save"
                                    } else {
                                        "the current one stays"
                                    }}
                                </span>
                                <button type="button" class=BUTTON id="webhook-own-secret" on:click=move |_| own_secret.set(true)>
                                    "Use my own…"
                                </button>
                            }
                        >
                            {text_field("webhook-secret", secret, "the sender's secret", true)}
                        </Show>
                    </div>
                </Show>
            </div>

            <h3 class=STEP>"4 · Which requests count"</h3>
            <div class="flex flex-col gap-2" data-webhook-conditions>
                <Show
                    when=move || filter_as_text.get()
                    fallback=move || view! {
                        <For each=move || rows.get() key=|row| row.key let:row>
                            {condition_row(row, rows)}
                        </For>
                        <button
                            type="button"
                            id="webhook-filter-add"
                            class="inline-flex min-h-11 w-fit items-center gap-2 rounded-lg px-3 text-[0.8125rem] text-fg-2 shadow-[inset_0_0_0_1px_var(--color-line-2)] [box-shadow:inset_0_0_0_1px_var(--color-line-2)] hover:bg-hover lg:min-h-9"
                            on:click=add_condition
                        >
                            <Glyph icon=icondata::LuPlus />
                            "Add a condition"
                        </button>
                    }
                >
                    <textarea
                        id="webhook-filter"
                        class=format!("{AREA} font-mono")
                        placeholder="one condition per line, such as header:X-GitHub-Event=issues,pull_request or json:/action=opened"
                        prop:value=move || filter_text.get()
                        on:input=move |event| filter_text.set(event_target_value(&event))
                    ></textarea>
                </Show>
                {move || filter_error.get().map(|error| view! { <p class=ERROR>{error}</p> })}
                <p class=NOTE>
                    "Every condition must match; requests that do not are answered 204 and counted, not kept. "
                    <button type="button" id="webhook-filter-text" class="hit-area font-semibold text-accent hover:underline" on:click=toggle_text>
                        {move || if filter_as_text.get() { "Edit as conditions" } else { "Edit as text" }}
                    </button>
                </p>
            </div>

            <details class="pt-3" data-webhook-advanced>
                <summary class="min-h-11 cursor-pointer content-center text-xs font-semibold tracking-wide text-muted uppercase lg:min-h-8">
                    "5 · Advanced: delivery, headers, limits"
                </summary>
                <div class=format!("{GRID} pt-2")>
                    <Show when=move || !new.get() || threaded()>
                        <label class=LABEL for="webhook-delivery">"Delivery"</label>
                        <select
                            id="webhook-delivery"
                            class=SELECT
                            on:change=move |event| delivery.set(if event_target_value(&event) == "steer" {
                                ProducerDeliveryMode::Steer
                            } else {
                                ProducerDeliveryMode::Queue
                            })
                        >
                            {[ProducerDeliveryMode::Queue, ProducerDeliveryMode::Steer].map(|choice| view! {
                                <option value=delivery_label(choice) selected=move || delivery.get() == choice>
                                    {match choice {
                                        ProducerDeliveryMode::Steer => "steer: into the running turn",
                                        _ => "queue: after the current turn",
                                    }}
                                </option>
                            })}
                        </select>
                    </Show>
                    <label class=LABEL for="webhook-headers">"Headers the agent sees"</label>
                    {text_field("webhook-headers", headers, "X-GitHub-Event, …", true)}
                    <label class=LABEL for="webhook-max-per-hour">"Events per hour"</label>
                    {text_field("webhook-max-per-hour", max_per_hour, "the daemon's default; 0 or less for no limit", false)}
                    <Show when=move || new.get()>
                        <label class=LABEL for="webhook-max-concurrent">"Sessions at once"</label>
                        {text_field("webhook-max-concurrent", max_concurrent, "1; 0 or less for no limit", false)}
                    </Show>
                    <label class=LABEL for="webhook-max-failures">"Failures before off"</label>
                    {text_field("webhook-max-failures", max_failures, "the daemon's default", false)}
                </div>
            </details>

            {move || workspace.webhook_error.get().map(|error| view! { <p class=ERROR id="webhook-error">{error}</p> })}
            <div class=EDITOR_ACTIONS>
                <button type="button" id="webhook-save" class=PRIMARY on:click=move |_| workspace.save_webhook(build())>
                    {if creating { "Create webhook" } else { "Save" }}
                </button>
                <button type="button" id="webhook-cancel" class=BUTTON on:click=move |_| workspace.close_webhook_form()>
                    "Cancel"
                </button>
            </div>
        </div>
    }
}

/// One condition: its source, its name, its values as chips, and a field
/// that adds more.
fn condition_row(row: Row, rows: RwSignal<Vec<Row>>) -> impl IntoView {
    let remove_value = move |value: String| {
        row.values
            .update(|values| values.retain(|kept| *kept != value));
    };
    view! {
        <div class="flex flex-wrap items-center gap-2 rounded-lg bg-sunken p-2" data-webhook-condition>
            <select
                class=format!("{SELECT} w-auto sm:w-32")
                aria-label="Source"
                data-condition-source
                on:change=move |event| row.header.set(event_target_value(&event) == "header")
            >
                <option value="header" selected=move || row.header.get()>"Header"</option>
                <option value="json" selected=move || !row.header.get()>"JSON field"</option>
            </select>
            <input
                type="text"
                class=format!("{FIELD} w-auto min-w-0 flex-1 sm:max-w-[13rem]")
                aria-label="Name"
                data-condition-name
                placeholder=move || if row.header.get() { "X-GitHub-Event" } else { "/action" }
                prop:value=move || row.name.get()
                on:input=move |event| row.name.set(event_target_value(&event))
            />
            <span class="text-[0.8125rem] text-muted">"is"</span>
            <For each=move || row.values.get() key=|value| value.clone() let:value>
                {
                    let removed = value.clone();
                    view! {
                        <span class="inline-flex h-7 items-center gap-1 rounded-md bg-accent-soft pl-2 font-mono text-xs text-accent" data-condition-value=value.clone()>
                            {value.clone()}
                            <button
                                type="button"
                                class="hit-area inline-flex h-7 w-6 items-center justify-center rounded-md hover:bg-press"
                                aria-label=format!("Remove {value}")
                                on:click=move |_| remove_value(removed.clone())
                            >
                                <Glyph icon=icondata::LuX class="text-xs" />
                            </button>
                        </span>
                    }
                }
            </For>
            <input
                type="text"
                class=format!("{FIELD} w-auto min-w-[7rem] flex-1")
                aria-label="Add a value"
                data-condition-typing
                placeholder="a value, Enter"
                prop:value=move || row.typing.get()
                on:input=move |event| {
                    let text = event_target_value(&event);
                    row.typing.set(text.clone());
                    if text.contains(',') {
                        row.commit();
                    }
                }
                on:keydown=move |event: leptos::ev::KeyboardEvent| {
                    if event.key() == "Enter" {
                        event.prevent_default();
                        row.commit();
                    }
                }
                on:change=move |_| row.commit()
            />
            <button
                type="button"
                class=ICON_BUTTON
                title="Remove this condition"
                aria-label="Remove this condition"
                on:click=move |_| rows.update(|rows| rows.retain(|kept| kept.key != row.key))
            >
                <Glyph icon=icondata::LuX />
            </button>
        </div>
    }
}
