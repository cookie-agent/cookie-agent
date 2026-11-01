//! The Skills view: the skills the daemon offers every session (the user's
//! and the official ones), or with a session open that session's, its
//! workspace's included; grouped by where they come from, searched by name
//! and description, and each with what it is for, how to invoke it, and
//! where it lives. Its button refreshes the official skills now.

use cookie_agent_client_state::{
    paths::display_path,
    skills::{skill_groups, source_label},
};
use cookie_agent_protocol::{PermissionEffect, RuntimeId, SkillDescriptor, SkillSource};
use leptos::prelude::*;

use super::{BUTTON, ERROR, NOTE, empty_state, loading, manage::STATE};
use crate::{icons::Glyph, workspace::Workspace};

/// A group's icon.
fn source_icon(source: SkillSource) -> crate::icons::Icon {
    match source {
        SkillSource::User => icondata::FaUserSolid,
        SkillSource::Project => icondata::FaFolderSolid,
        SkillSource::Official => icondata::FaCertificateSolid,
    }
}

pub fn skills_page(workspace: Workspace) -> impl IntoView {
    let session_open = move || workspace.selected.get().is_some();
    // Another session opened: its skills, while the view lists a session's;
    // none open, the daemon's again.
    Effect::new(
        move |previous: Option<Option<cookie_agent_protocol::SessionId>>| {
            let selected = workspace.selected.get();
            if previous.is_some_and(|previous| previous != selected)
                && workspace.skill_list_session.get_untracked()
            {
                workspace.list_session_skills(selected.is_some());
            }
            selected
        },
    );
    let scope_button = move |session: bool, label: &'static str, id: &'static str| {
        let chosen = move || workspace.skill_list_session.get() == session;
        view! {
            <button
                type="button"
                role="radio"
                class=move || format!(
                    "inline-flex min-h-11 items-center rounded-full border px-3 text-sm lg:min-h-9 {}",
                    if chosen() {
                        "border-espresso bg-espresso text-cream dark:border-latte dark:bg-latte dark:text-night"
                    } else {
                        "border-crust hover:bg-toasted active:bg-crust dark:border-smoke dark:hover:bg-ember dark:active:bg-smoke"
                    },
                )
                aria-checked=move || chosen().to_string()
                id=id
                on:click=move |_| {
                    if !chosen() {
                        workspace.list_session_skills(session);
                    }
                }
            >
                {label}
            </button>
        }
    };
    // The daemon's own skills live on its machine: under its home.
    let home = move || {
        workspace.runtime_homes.with(|homes| {
            homes
                .get_str(RuntimeId::GLOBAL)
                .or_else(|| homes.get_str(RuntimeId::LOCAL))
                .map(str::to_owned)
        })
    };
    let list = move || {
        let query = workspace.skill_list_query.get();
        match workspace.skill_list.get() {
            None => loading("Reading the skills…").into_any(),
            Some(Err(message)) => view! { <p class=ERROR>{message}</p> }.into_any(),
            Some(Ok(skills)) if skills.is_empty() => {
                empty_state(icondata::FaBookOpenSolid, "No skills yet.").into_any()
            }
            Some(Ok(skills)) => {
                let groups = skill_groups(&skills, &query);
                if groups.is_empty() {
                    return empty_state(icondata::FaMagnifyingGlassSolid, "No skills match.")
                        .into_any();
                }
                let home = home();
                groups
                    .into_iter()
                    .map(|(source, group)| {
                        let rows = group
                            .into_iter()
                            .map(|skill| skill_row(skill, home.as_deref()))
                            .collect_view();
                        view! {
                            <section class="flex flex-col" data-skill-group=format!("{source:?}").to_lowercase()>
                                <h2 class="flex items-center gap-2 pt-3 pb-1 text-sm font-bold">
                                    <Glyph icon=source_icon(source) class="text-caramel dark:text-maple" />
                                    {source_label(source)}
                                </h2>
                                {rows}
                            </section>
                        }
                    })
                    .collect_view()
                    .into_any()
            }
        }
    };
    view! {
        <div class="flex flex-col gap-2" data-skills-page>
            <p class=NOTE>
                "Skills are instructions an agent loads when a task calls for them; you can run one with /skills in a session."
            </p>
            <div class="flex flex-wrap items-center gap-2">
                <Show when=session_open>
                    <div class="flex flex-wrap gap-1.5" role="radiogroup" aria-label="Which skills">
                        {scope_button(false, "Every session's", "skills-daemon")}
                        {scope_button(true, "This session's", "skills-session")}
                    </div>
                </Show>
                <span class="flex-1"></span>
                <button
                    type="button"
                    class=BUTTON
                    id="skills-update"
                    disabled=move || workspace.skills_updating.get()
                    on:click=move |_| workspace.update_official_skills()
                >
                    <span class="inline-flex" class:animate-spin=move || workspace.skills_updating.get()>
                        <Glyph icon=icondata::FaArrowsRotateSolid />
                    </span>
                    {move || if workspace.skills_updating.get() { "Updating…" } else { "Update official skills" }}
                </button>
            </div>
            {move || workspace.skills_update_note.get().map(|(text, failed)| {
                view! { <p class=if failed { ERROR } else { NOTE } data-skills-update-note>{text}</p> }
            })}
            <label class="relative flex items-center">
                <Glyph icon=icondata::FaMagnifyingGlassSolid class="pointer-events-none absolute left-3 text-xs text-ink dark:text-sand" />
                <input
                    type="text"
                    role="searchbox"
                    id="skills-search"
                    class="min-h-11 w-full min-w-0 rounded-md border border-crust bg-cream pr-3 pl-9 text-sm text-espresso hover:border-caramel focus:border-caramel focus:outline-none dark:border-smoke dark:bg-night dark:text-latte dark:hover:border-maple dark:focus:border-maple"
                    aria-label="Search skills by name or description"
                    placeholder="Search by name or description"
                    prop:value=move || workspace.skill_list_query.get_untracked()
                    on:input=move |event| workspace.skill_list_query.set(event_target_value(&event))
                />
            </label>
            {list}
        </div>
    }
}

/// One skill: its name, how it is invoked, and its marks; what it does and
/// when; where it lives.
fn skill_row(skill: &SkillDescriptor, home: Option<&str>) -> impl IntoView + use<> {
    let invocation = match &skill.argument_hint {
        Some(hint) => format!("/{} {hint}", skill.name),
        None => format!("/{}", skill.name),
    };
    // What sets it apart, beside its name.
    let mut marks = Vec::new();
    if let Some(version) = &skill.version {
        marks.push(("version", format!("v{version}")));
    }
    if !skill.precedence_winner {
        marks.push(("shadowed", "overridden".to_owned()));
    }
    if !skill.user_invocable {
        marks.push(("agent-only", "agent only".to_owned()));
    }
    if !skill.visible {
        marks.push(("hidden", "hidden from the model".to_owned()));
    }
    match skill.permission_effect {
        PermissionEffect::Deny => marks.push(("denied", "denied".to_owned())),
        PermissionEffect::Ask => marks.push(("asks", "asks first".to_owned())),
        PermissionEffect::Allow => {}
    }
    let marks = marks
        .into_iter()
        .map(|(id, text)| view! { <span class=STATE data-skill-mark=id>{text}</span> })
        .collect_view();
    let when = skill.when_to_use.clone();
    let location = display_path(&skill.location, home);
    view! {
        <div
            class="flex flex-col gap-1 border-b border-toasted py-2 dark:border-ember"
            class:opacity-60=!skill.precedence_winner
            data-skill=skill.name.clone()
        >
            <div class="flex flex-wrap items-center gap-2">
                <span class="min-w-0 truncate font-mono text-sm font-bold" title=invocation.clone()>{invocation.clone()}</span>
                {marks}
            </div>
            <p class="text-sm whitespace-pre-wrap break-words">{skill.description.clone()}</p>
            {when.map(|when| view! { <p class=format!("{NOTE} whitespace-pre-wrap break-words")>{format!("When: {when}")}</p> })}
            <span class=format!("{NOTE} font-mono break-all") title=skill.location.clone()>{location}</span>
        </div>
    }
}
