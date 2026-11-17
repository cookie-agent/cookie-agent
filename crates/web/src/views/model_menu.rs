//! A model menu, for a cookie agent's models and an ACP agent's alike
//! (`docs/site/specs/web-polish.md`, P4, changed 2026-11-16): a search
//! field, the models in a list of their own that scrolls (grouped by
//! provider when there are several), and a foot pinned under it (the effort
//! control, and an ACP agent's other options).

use cookie_agent_client_state::model_menu::{match_range, matches, provider_groups};
use leptos::{ev, html, prelude::*};

use super::{NOTE, ROW};
use crate::{icons::Glyph, workspace::Workspace};

/// One model of the menu.
#[derive(Clone)]
pub(super) struct MenuModel {
    /// Its ID: the `provider/model` key, or the ACP option's value.
    pub id: String,
    pub name: String,
    /// The provider it is grouped under; empty for an ACP agent's.
    pub provider: String,
    pub description: Option<String>,
    pub chosen: bool,
    /// Chooses it.
    pub pick: Callback<()>,
}

/// A model's name with the part the search matched marked.
fn marked_name(name: &str, query: &str) -> AnyView {
    match match_range(name, query) {
        Some((from, to)) => view! {
            {name[..from].to_owned()}
            <mark class="rounded-[0.2rem] bg-accent-soft px-px text-inherit">{name[from..to].to_owned()}</mark>
            {name[to..].to_owned()}
        }
        .into_any(),
        None => name.to_owned().into_any(),
    }
}

/// The menu of `models`, `acp` saying whose they are, with `foot` pinned
/// under the list. Typing filters the list; ↑ and ↓ move through it and
/// Enter chooses.
pub(super) fn model_menu(
    workspace: Workspace,
    models: Vec<MenuModel>,
    acp: bool,
    foot: Option<AnyView>,
) -> impl IntoView {
    let query = RwSignal::new(String::new());
    // The row ↑ and ↓ reached, in the filtered list; none until they move.
    let active = RwSignal::new(None::<usize>);
    let field = NodeRef::<html::Input>::new();
    let count = models.len();
    let models = StoredValue::new(models);
    let shown = move || {
        let query = query.get();
        models.with_value(|models| {
            models
                .iter()
                .filter(|model| {
                    matches(
                        &query,
                        &[
                            &model.name,
                            &model.provider,
                            &model.id,
                            model.description.as_deref().unwrap_or_default(),
                        ],
                    )
                })
                .cloned()
                .collect::<Vec<_>>()
        })
    };
    // On a desktop the field takes the keys at once; a phone's keyboard
    // would cover the list, so there it waits for a tap. The list opens
    // at the current model.
    let narrow = workspace.narrow.get_untracked();
    Effect::new(move |_| {
        if let Some(field) = field.get()
            && !narrow
        {
            let _ = field.focus();
        }
        crate::browser::reveal("[data-model-menu] [aria-current=true]");
    });
    let keydown = move |event: ev::KeyboardEvent| {
        let rows = untrack(shown);
        match event.key().as_str() {
            "ArrowDown" | "ArrowUp" if !rows.is_empty() => {
                event.prevent_default();
                let last = rows.len() - 1;
                let next = match (active.get_untracked(), event.key().as_str()) {
                    (None, "ArrowDown") => 0,
                    (None, _) => last,
                    (Some(at), "ArrowDown") => (at + 1).min(last),
                    (Some(at), _) => at.saturating_sub(1),
                };
                active.set(Some(next));
                crate::browser::reveal("[data-model-menu] [data-active=true]");
            }
            "Enter" => {
                event.prevent_default();
                if let Some(model) = rows.get(active.get_untracked().unwrap_or(0)) {
                    model.pick.run(());
                }
            }
            _ => {}
        }
    };
    let list = move || {
        let query = query.get();
        let rows = shown();
        if rows.is_empty() {
            return view! {
                <p class=format!("px-3 py-4 text-center {NOTE}") data-model-none>
                    {format!("No model matches “{}”.", query.trim())}
                </p>
            }
            .into_any();
        }
        let active = active.get();
        let mut index = 0usize;
        provider_groups(rows, |model| model.provider.clone())
            .into_iter()
            .map(|(provider, members)| {
                let rows = members
                    .into_iter()
                    .map(|model| {
                        let at = index;
                        index += 1;
                        let highlighted = active == Some(at);
                        let MenuModel {
                            id,
                            name,
                            description,
                            chosen,
                            pick,
                            ..
                        } = model;
                        view! {
                            <button
                                type="button"
                                class=ROW
                                class:bg-sel=chosen
                                class:bg-hover=highlighted && !chosen
                                aria-current=chosen.to_string()
                                data-active=highlighted.to_string()
                                title=id.clone()
                                data-model=(!acp).then(|| id.clone())
                                data-acp-model=acp.then(|| id.clone())
                                on:click=move |_| pick.run(())
                            >
                                <span class="flex w-full items-center gap-2">
                                    <span class="min-w-0 flex-1 truncate text-sm font-semibold">{marked_name(&name, &query)}</span>
                                    {chosen.then(|| view! {
                                        <Glyph icon=icondata::LuCheck class="text-accent" />
                                        <span class="sr-only">"(current)"</span>
                                    })}
                                </span>
                                {description.map(|description| view! { <span class=NOTE>{description}</span> })}
                            </button>
                        }
                    })
                    .collect_view();
                view! {
                    {provider.map(|provider| view! {
                        <span class="px-3 pt-2.5 pb-0.5 text-[0.6875rem] font-semibold tracking-wide text-muted uppercase" data-model-group>{provider}</span>
                    })}
                    {rows}
                }
            })
            .collect_view()
            .into_any()
    };
    view! {
        <div class="flex min-h-0 flex-col" data-model-menu>
            <label class="mx-1 mb-1 flex min-h-11 items-center gap-2 rounded-lg bg-surface px-2.5 shadow-[inset_0_0_0_1px_var(--color-line-2)] focus-within:shadow-[inset_0_0_0_1px_var(--color-accent),0_0_0_3px_var(--color-accent-soft)] lg:min-h-9">
                <Glyph icon=icondata::LuSearch class="shrink-0 text-muted" />
                <input
                    type="search"
                    node_ref=field
                    class="min-h-11 min-w-0 flex-1 bg-transparent text-sm placeholder:text-faint focus:outline-none lg:min-h-9"
                    placeholder=format!("Search {count} models")
                    aria-label="Search the models"
                    data-model-search
                    prop:value=move || query.get()
                    on:input=move |event| {
                        query.set(event_target_value(&event));
                        active.set(None);
                    }
                    on:keydown=keydown
                />
            </label>
            <div class="flex max-h-[min(24rem,45vh)] flex-col gap-0.5 overflow-y-auto overscroll-contain px-0.5" data-model-list>
                {list}
            </div>
            {foot.map(|foot| view! {
                <div class="mt-1 border-t border-line px-1 pt-2 pb-1" data-model-foot>{foot}</div>
            })}
        </div>
    }
}
