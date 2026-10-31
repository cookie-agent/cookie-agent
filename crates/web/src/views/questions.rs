//! The question an agent asks the user (ACP elicitation), as a card above
//! the composer: the oldest of the viewed session's waiting questions, who
//! asks, and its message, then a form to fill in, a page to open, or, for
//! a kind this client does not know, nothing to fill in. Submit (or Done,
//! for a page) accepts, Decline says no, and Cancel dismisses it without
//! choosing. The card leaves once the daemon records the answer.

use cookie_agent_client_state::elicitation::{
    DraftValue, FieldProblem, FormDraft, QuestionKind, QuestionState, field_hint, is_other_field,
    other_field, url_host, url_warnings,
};
use cookie_agent_protocol::{
    ElicitationAction, ElicitationContent, ElicitationField, ElicitationForm, ElicitationInput,
};
use leptos::{html, prelude::*};

use super::{BUTTON, ERROR, NOTE, PRIMARY};
use crate::{icons::Glyph, workspace::Workspace};

const FIELD: &str = "min-h-11 w-full min-w-0 rounded-md border border-crust bg-cream px-3 text-sm hover:border-caramel focus:border-caramel focus:outline-none dark:border-smoke dark:bg-night dark:hover:border-maple dark:focus:border-maple";
/// A radio or checkbox row: the whole row is the hit area.
const OPTION: &str = "flex min-h-11 cursor-pointer items-start gap-2 rounded-md px-2 py-2 hover:bg-toasted dark:hover:bg-ember";

/// `url` split around its host, for showing the host highlighted: the part
/// before it, the host, and the rest. Without a host the whole URL comes
/// first.
pub(crate) fn host_parts(url: &str) -> (String, String, String) {
    let Some(host) = url_host(url) else {
        return (url.to_owned(), String::new(), String::new());
    };
    let start = url.find("://").map_or(0, |index| index + 3);
    match url[start..].find(host) {
        Some(offset) => {
            let at = start + offset;
            (
                url[..at].to_owned(),
                host.to_owned(),
                url[at + host.len()..].to_owned(),
            )
        }
        None => (url.to_owned(), String::new(), String::new()),
    }
}

/// "1 of 3" for the first of several waiting questions; nothing for one.
pub(crate) fn queue_label(count: usize) -> Option<String> {
    (count > 1).then(|| format!("1 of {count}"))
}

/// The `type` of the input a text field's format asks for.
pub(crate) fn text_input_type(format: Option<&str>) -> &'static str {
    match format {
        Some("email") => "email",
        Some("uri") => "url",
        Some("date") => "date",
        _ => "text",
    }
}

/// The viewed session's oldest waiting question, if any.
#[component]
pub(super) fn QuestionCard(workspace: Workspace) -> impl IntoView {
    let first = Memo::new(move |_| workspace.questions().into_iter().next());
    let count = Memo::new(move |_| workspace.questions().len());
    move || {
        first
            .get()
            .map(|question| question_card(workspace, question, count))
    }
}

fn question_card(
    workspace: Workspace,
    question: QuestionState,
    count: Memo<usize>,
) -> impl IntoView {
    let error = RwSignal::new(None::<String>);
    let sending = RwSignal::new(false);
    let problem = RwSignal::new(None::<FieldProblem>);
    let id = question.request.elicitation_id.to_string();
    let heading = question.heading();
    let region_label = heading.clone();
    let message = question.request.message.clone();
    let stored = StoredValue::new(question.clone());
    let send = move |action: ElicitationAction, content: Option<ElicitationContent>| {
        sending.set(true);
        error.set(None);
        stored.with_value(|question| {
            workspace.answer_question(question, action, content, move |failure| {
                sending.set(false);
                error.set(failure);
            });
        });
    };
    let draft = RwSignal::new(match question.kind() {
        QuestionKind::Form(form) => Some(FormDraft::new(form)),
        _ => None,
    });
    let (body, accept_label) = match question.kind() {
        QuestionKind::Form(form) => (form_view(&id, form, draft, problem).into_any(), Some("Submit")),
        QuestionKind::Url(url) => (url_view(url).into_any(), Some("Done")),
        QuestionKind::Unsupported => (
            view! {
                <p class=NOTE>"This client cannot show this kind of question: decline or cancel it."</p>
            }
            .into_any(),
            None,
        ),
    };
    let accept = move |_| {
        let form = stored.with_value(|question| question.request.form.clone());
        match form {
            Some(form) => match draft.with_untracked(|draft| {
                draft
                    .as_ref()
                    .map(|draft| draft.content(&form))
                    .unwrap_or_else(|| Ok(ElicitationContent::new()))
            }) {
                Ok(content) => {
                    problem.set(None);
                    send(ElicitationAction::Accept, Some(content));
                }
                Err(found) => problem.set(Some(found)),
            },
            None => send(ElicitationAction::Accept, None),
        }
    };
    view! {
        <section
            id="question"
            data-question=id
            role="region"
            aria-label=region_label
            class="flex flex-col gap-2 rounded-lg border-2 border-caramel bg-parchment p-3 dark:border-maple dark:bg-char"
        >
            <p class="flex items-center gap-2 text-sm font-bold">
                <Glyph icon=icondata::FaCircleQuestionSolid class="text-caramel dark:text-maple" />
                <span class="min-w-0 flex-1 break-words" data-question-heading>{heading}</span>
                {move || queue_label(count.get()).map(|label| view! { <span class=NOTE>{label}</span> })}
            </p>
            <p class="text-sm whitespace-pre-wrap break-words" data-question-message>{message}</p>
            {body}
            {move || error.get().map(|error| view! { <p class=ERROR role="alert" data-question-error>{error}</p> })}
            <div class="flex flex-wrap gap-2">
                {accept_label.map(|label| view! {
                    <button type="button" class=PRIMARY data-answer="accept" disabled=move || sending.get() on:click=accept>
                        <Glyph icon=icondata::FaCheckSolid />
                        {label}
                    </button>
                })}
                <button
                    type="button"
                    class=BUTTON
                    data-answer="decline"
                    disabled=move || sending.get()
                    on:click=move |_| send(ElicitationAction::Decline, None)
                >
                    <Glyph icon=icondata::FaXmarkSolid />
                    "Decline"
                </button>
                <button
                    type="button"
                    class=BUTTON
                    data-answer="cancel"
                    title="Dismiss the question without choosing"
                    disabled=move || sending.get()
                    on:click=move |_| send(ElicitationAction::Cancel, None)
                >
                    "Cancel"
                </button>
            </div>
        </section>
    }
}

/// A URL question: the whole URL with its host picked out, what looks off
/// about it, and the link, which opens only on the user's click.
fn url_view(url: &str) -> impl IntoView {
    let (before, host, after) = host_parts(url);
    let warnings = url_warnings(url);
    let host_label = url_host(url).unwrap_or("the page").to_owned();
    view! {
        <p class="rounded-md bg-cream px-3 py-2 font-mono text-sm break-all dark:bg-night" data-question-url>
            <span class="opacity-70">{before}</span>
            <strong class="text-espresso dark:text-latte" data-question-host>{host}</strong>
            <span class="opacity-70">{after}</span>
        </p>
        {(!warnings.is_empty()).then(|| view! {
            <ul class="flex flex-col gap-1 text-sm text-red-800 dark:text-red-300" data-question-warnings>
                {warnings.into_iter().map(|warning| view! {
                    <li class="flex items-start gap-2">
                        <Glyph icon=icondata::FaTriangleExclamationSolid />
                        <span>{format!("Careful: {warning}.")}</span>
                    </li>
                }).collect_view()}
            </ul>
        })}
        <p class=NOTE>"Open the page, finish there, then choose Done."</p>
        <a
            class=format!("{BUTTON} self-start")
            href=url.to_owned()
            target="_blank"
            rel="noopener noreferrer"
            data-question-open
        >
            <Glyph icon=icondata::FaArrowUpRightFromSquareSolid />
            {format!("Open {host_label}")}
        </a>
    }
}

/// A form: one control per field, in the form's order.
fn form_view(
    id: &str,
    form: &ElicitationForm,
    draft: RwSignal<Option<FormDraft>>,
    problem: RwSignal<Option<FieldProblem>>,
) -> impl IntoView {
    let set = move |index: usize, value: DraftValue| {
        draft.update(|draft| {
            if let Some(slot) = draft.as_mut().and_then(|draft| draft.values.get_mut(index)) {
                *slot = value;
            }
        });
        problem.update(|problem| {
            if problem
                .as_ref()
                .is_some_and(|problem| problem.index == index)
            {
                *problem = None;
            }
        });
    };
    let current = move |index: usize| {
        draft.with_untracked(|draft| {
            draft
                .as_ref()
                .and_then(|draft| draft.values.get(index).cloned())
        })
    };
    view! {
        {form.title.clone().map(|title| view! { <p class="text-sm font-bold">{title}</p> })}
        {form.description.clone().map(|description| view! { <p class=NOTE>{description}</p> })}
        <div class="flex flex-col gap-3">
            {form.fields.iter().enumerate().filter(|(index, _)| !is_other_field(form, *index)).map(|(index, field)| {
                // A choice's "Other" text shows as its last option.
                let other = other_field(form, index).map(|other_index| {
                    let text = match current(other_index) {
                        Some(DraftValue::Text(text)) => text,
                        _ => String::new(),
                    };
                    Other {
                        index: other_index,
                        chosen: RwSignal::new(!text.is_empty()),
                        text: RwSignal::new(text),
                    }
                });
                let control = field_control(id, index, field, current(index), other, set);
                field_view(index, field, control, other.map(|other| other.index), problem)
            }).collect_view()}
        </div>
    }
}

/// A choice's "Other" text field: its index, whether "Other" is chosen,
/// and what its box holds.
#[derive(Clone, Copy)]
struct Other {
    index: usize,
    chosen: RwSignal<bool>,
    text: RwSignal<String>,
}

impl Other {
    /// Chooses "Other" or lets go of it: the draft holds the box's text
    /// only while it is chosen.
    fn choose(self, on: bool, set: impl Fn(usize, DraftValue)) {
        self.chosen.set(on);
        let text = if on {
            self.text.get_untracked()
        } else {
            String::new()
        };
        set(self.index, DraftValue::Text(text));
    }
}

/// "Other" as a choice's last option (a radio, or a checkbox for a
/// multi-choice), then a text box while it is chosen.
fn other_option(
    group: String,
    multiple: bool,
    other: Other,
    choose: impl Fn(bool) + Copy + Send + Sync + 'static,
    set: impl Fn(usize, DraftValue) + Copy + Send + Sync + 'static,
) -> impl IntoView {
    let field = NodeRef::<html::Input>::new();
    // The box takes the focus as it shows.
    Effect::new(move |_| {
        if let Some(input) = field.get() {
            request_animation_frame(move || {
                let _ = input.focus();
            });
        }
    });
    view! {
        <label class=OPTION>
            <input
                type=if multiple { "checkbox" } else { "radio" }
                class="mt-0.5 h-5 w-5 shrink-0"
                name=(!multiple).then_some(group)
                data-choice-other
                prop:checked=other.chosen.get_untracked()
                on:change=move |event| choose(event_target_checked(&event))
            />
            <span class="text-sm">"Other"</span>
        </label>
        <Show when=move || other.chosen.get()>
            <div class="pr-2 pb-1 pl-9">
                <input
                    type="text"
                    class=FIELD
                    aria-label="Your answer"
                    placeholder="Your answer"
                    data-other-text
                    node_ref=field
                    prop:value=move || other.text.get_untracked()
                    on:input=move |event| {
                        let text = event_target_value(&event);
                        other.text.set(text.clone());
                        set(other.index, DraftValue::Text(text));
                    }
                />
            </div>
        </Show>
    }
}

fn field_view(
    index: usize,
    field: &ElicitationField,
    control: AnyView,
    other: Option<usize>,
    problem: RwSignal<Option<FieldProblem>>,
) -> impl IntoView {
    let hint = field_hint(field);
    view! {
        <div class="flex flex-col gap-1" data-field=field.name.clone()>
            <span class="text-sm font-bold">
                {field.label().to_owned()}
                {field.required.then(|| view! { <span class="text-caramel dark:text-maple" aria-hidden="true">" *"</span> })}
            </span>
            {field.description.clone().map(|description| view! { <span class=NOTE>{description}</span> })}
            {(!hint.is_empty()).then(|| view! { <span class=NOTE data-field-hint>{hint}</span> })}
            {control}
            {move || problem.get().filter(|problem| problem.index == index || Some(problem.index) == other).map(|problem| view! {
                <p class=ERROR role="alert" data-field-problem>{problem.message}</p>
            })}
        </div>
    }
}

/// The input of field `index`, starting at `value`, with a choice's
/// "Other" as its last option; `set` records each change in the draft.
fn field_control(
    id: &str,
    index: usize,
    field: &ElicitationField,
    value: Option<DraftValue>,
    other: Option<Other>,
    set: impl Fn(usize, DraftValue) + Copy + Send + Sync + 'static,
) -> AnyView {
    let let_go_of_other = move || {
        if let Some(other) = other {
            other.choose(false, set);
        }
    };
    let label = field.label().to_owned();
    let group = format!("question-{id}-{index}");
    match (field.input.known(), value) {
        (Some(ElicitationInput::Text { max_length, format, .. }), Some(DraftValue::Text(text))) => {
            let kind = text_input_type(format.as_deref());
            let placeholder = (format.as_deref() == Some("date-time")).then_some("2026-10-24T10:00:00Z");
            view! {
                <input
                    type=kind
                    class=FIELD
                    aria-label=label
                    maxlength=max_length.map(|max| max.to_string())
                    placeholder=placeholder
                    prop:value=text
                    on:input=move |event| set(index, DraftValue::Text(event_target_value(&event)))
                />
            }
            .into_any()
        }
        (
            Some(ElicitationInput::Number { .. } | ElicitationInput::Integer { .. }),
            Some(DraftValue::Text(text)),
        ) => {
            let decimal = matches!(field.input.known(), Some(ElicitationInput::Number { .. }));
            view! {
                <input
                    type="text"
                    inputmode=if decimal { "decimal" } else { "numeric" }
                    class=format!("{FIELD} font-mono")
                    aria-label=label
                    prop:value=text
                    on:input=move |event| set(index, DraftValue::Text(event_target_value(&event)))
                />
            }
            .into_any()
        }
        (Some(ElicitationInput::Choice { choices, .. }), Some(DraftValue::Choice(chosen))) => {
            let none = (!field.required).then(|| {
                let group = group.clone();
                view! {
                    <label class=OPTION>
                        <input
                            type="radio"
                            class="mt-0.5 h-5 w-5 shrink-0"
                            name=group
                            prop:checked=chosen.is_none() && !other.is_some_and(|other| other.chosen.get_untracked())
                            on:change=move |_| {
                                let_go_of_other();
                                set(index, DraftValue::Choice(None));
                            }
                        />
                        <span class="text-sm opacity-70">"No choice"</span>
                    </label>
                }
            });
            view! {
                <fieldset class="flex flex-col" aria-label=label>
                    {choices.iter().enumerate().map(|(choice_index, choice)| {
                        let group = group.clone();
                        view! {
                            <label class=OPTION>
                                <input
                                    type="radio"
                                    class="mt-0.5 h-5 w-5 shrink-0"
                                    name=group
                                    data-choice=choice.value.clone()
                                    prop:checked=chosen == Some(choice_index)
                                    on:change=move |_| {
                                        let_go_of_other();
                                        set(index, DraftValue::Choice(Some(choice_index)));
                                    }
                                />
                                <span class="flex min-w-0 flex-col">
                                    <span class="text-sm break-words">{choice.title.clone()}</span>
                                    {choice.description.clone().map(|description| view! { <span class=NOTE>{description}</span> })}
                                </span>
                            </label>
                        }
                    }).collect_view()}
                    {other.map(|other| other_option(group.clone(), false, other, move |_| {
                        set(index, DraftValue::Choice(None));
                        other.choose(true, set);
                    }, set))}
                    {none}
                </fieldset>
            }
            .into_any()
        }
        (
            Some(ElicitationInput::MultiChoice { choices, .. }),
            Some(DraftValue::MultiChoice(chosen)),
        ) => {
            let picked = RwSignal::new(chosen);
            view! {
                <fieldset class="flex flex-col" aria-label=label>
                    {choices.iter().enumerate().map(|(choice_index, choice)| view! {
                        <label class=OPTION>
                            <input
                                type="checkbox"
                                class="mt-0.5 h-5 w-5 shrink-0"
                                data-choice=choice.value.clone()
                                prop:checked=picked.with_untracked(|picked| picked.get(choice_index).copied().unwrap_or(false))
                                on:change=move |event| {
                                    let on = event_target_checked(&event);
                                    picked.update(|picked| {
                                        if let Some(slot) = picked.get_mut(choice_index) {
                                            *slot = on;
                                        }
                                    });
                                    set(index, DraftValue::MultiChoice(picked.get_untracked()));
                                }
                            />
                            <span class="flex min-w-0 flex-col">
                                <span class="text-sm break-words">{choice.title.clone()}</span>
                                {choice.description.clone().map(|description| view! { <span class=NOTE>{description}</span> })}
                            </span>
                        </label>
                    }).collect_view()}
                    {other.map(|other| other_option(group.clone(), true, other, move |on| other.choose(on, set), set))}
                </fieldset>
            }
            .into_any()
        }
        (Some(ElicitationInput::Boolean { .. }), Some(DraftValue::Boolean(on))) => view! {
            <label class=OPTION>
                <input
                    type="checkbox"
                    class="mt-0.5 h-5 w-5 shrink-0"
                    aria-label=label
                    prop:checked=on
                    on:change=move |event| set(index, DraftValue::Boolean(event_target_checked(&event)))
                />
                <span class="text-sm">"Yes"</span>
            </label>
        }
        .into_any(),
        _ => view! { <p class=NOTE data-field-unsupported>"This client cannot fill it in."</p> }.into_any(),
    }
}

#[cfg(test)]
mod tests {
    use super::{host_parts, queue_label, text_input_type};

    #[test]
    fn the_host_is_picked_out_of_the_url() {
        assert_eq!(
            host_parts("https://user@auth.example.com:8443/connect?id=1"),
            (
                "https://user@".to_owned(),
                "auth.example.com".to_owned(),
                ":8443/connect?id=1".to_owned()
            )
        );
        assert_eq!(
            host_parts("mailto:someone"),
            ("mailto:someone".to_owned(), String::new(), String::new())
        );
    }

    #[test]
    fn several_questions_say_which_is_shown() {
        assert_eq!(queue_label(1), None);
        assert_eq!(queue_label(3).as_deref(), Some("1 of 3"));
    }

    #[test]
    fn a_text_fields_format_picks_its_input() {
        assert_eq!(text_input_type(Some("email")), "email");
        assert_eq!(text_input_type(Some("uri")), "url");
        assert_eq!(text_input_type(Some("date")), "date");
        assert_eq!(text_input_type(Some("date-time")), "text");
        assert_eq!(text_input_type(None), "text");
    }
}
