//! The effort control (`docs/site/specs/web-polish.md`, section 6): a
//! sliding pill for a native model's variants and an ACP agent's effort,
//! a switch for a toggle, a list past seven levels, nothing with nothing to
//! choose.

use cookie_agent_client_state::effort::{Form, Levels, short_label};
use leptos::{ev, prelude::*};

use crate::icons::Glyph;

/// The control for `levels`, showing `current`; choosing a level calls
/// `on_choose` with its ID. A drag moves the pill and chooses where it is
/// let go; a click or a key chooses at once. `large` is the phone's
/// finger-sized pill.
#[component]
pub fn EffortControl(
    levels: Levels,
    current: Option<String>,
    on_choose: Callback<String>,
    #[prop(optional)] large: bool,
) -> impl IntoView {
    let form = levels.form();
    if form == Form::Hidden {
        return ().into_any();
    }
    let initial = current.as_deref().and_then(|id| levels.position(id));
    let default = levels.default.as_deref().and_then(|id| levels.position(id));
    let levels = StoredValue::new(levels);
    let count = levels.with_value(|levels| levels.levels.len());
    // The level shown: it follows a drag before the drag chooses it.
    let shown = RwSignal::new(initial);
    let chosen = StoredValue::new(initial);
    let choose = move |index: usize| {
        shown.set(Some(index));
        if chosen.get_value() == Some(index) {
            return;
        }
        chosen.set_value(Some(index));
        if let Some(id) =
            levels.with_value(|levels| levels.levels.get(index).map(|level| level.id.clone()))
        {
            on_choose.run(id);
        }
    };
    let label = move |index: usize| levels.with_value(|levels| levels.levels[index].label.clone());
    let header = move || {
        let name = levels.with_value(|levels| levels.name);
        let value = shown.get().map(label);
        let default_label = default.map(label);
        let reset = default.filter(|default| shown.get() != Some(*default));
        view! {
            <div class="flex items-baseline gap-2 px-1">
                <span class="text-xs font-semibold text-muted">{name}</span>
                {value.map(|value| view! { <span class="effort-value text-[0.8125rem] font-semibold" data-effort-value>{value}</span> })}
                {default_label.map(|label| view! { <span class="text-[0.6875rem] text-faint">{format!("default {label}")}</span> })}
                {reset.map(|default| view! {
                    <button
                        type="button"
                        class="ml-auto rounded px-1 text-xs text-accent hover:bg-hover"
                        data-effort-reset
                        on:click=move |_| choose(default)
                    >
                        "Reset"
                    </button>
                })}
            </div>
        }
    };
    let description = move || {
        shown
            .get()
            .and_then(|index| levels.with_value(|levels| levels.levels[index].description.clone()))
    };
    let scale = levels.with_value(|levels| levels.scale);
    // How warm the pill is: from the accent at the middle level to the hot
    // colour at the top, on a scale only.
    let heat = move || match (scale, shown.get()) {
        (true, Some(index)) if count > 1 => {
            let position = index as f64 / (count - 1) as f64;
            ((position - 0.5) * 2.0).max(0.0)
        }
        _ => 0.0,
    };
    let body = match form {
        Form::Switch => {
            let on = move || shown.get() == Some(1);
            view! {
                <button
                    type="button"
                    role="switch"
                    class="flex min-h-9 items-center gap-2.5 rounded-lg px-1 text-left text-[0.8125rem] font-semibold"
                    aria-checked=move || on().to_string()
                    data-effort-switch
                    on:click=move |_| choose(usize::from(!on()))
                >
                    <span class="effort-switch" class:effort-switch-on=on aria-hidden="true"></span>
                    {move || shown.get().map(label)}
                </button>
            }
            .into_any()
        }
        Form::List => view! {
            <div class="flex flex-col" role="listbox" aria-label=move || levels.with_value(|levels| levels.name)>
                {(0..count).map(|index| view! {
                    <button
                        type="button"
                        role="option"
                        class="flex min-h-9 items-center gap-2 rounded-lg px-2 text-left text-[0.8125rem] hover:bg-hover"
                        aria-selected=move || (shown.get() == Some(index)).to_string()
                        data-effort-level=move || levels.with_value(|levels| levels.levels[index].id.clone())
                        on:click=move |_| choose(index)
                    >
                        <span class="min-w-0 flex-1 truncate">{label(index)}</span>
                        <Show when=move || shown.get() == Some(index)>
                            <Glyph icon=icondata::LuCheck class="text-accent" />
                        </Show>
                    </button>
                }).collect_view()}
            </div>
        }
        .into_any(),
        Form::Pill { short } => pill(levels, count, short, large, shown, choose).into_any(),
        Form::Hidden => ().into_any(),
    };
    view! {
        <div
            class="effort flex flex-col gap-1.5"
            class:effort-large=large
            style=move || format!("--effort-heat: {:.3}", heat())
            data-effort=move || levels.with_value(|levels| levels.name)
        >
            {header}
            {body}
            {move || description().map(|text| view! { <p class="px-1 text-xs text-muted" data-effort-description>{text}</p> })}
            {scale.then(|| view! {
                <div class="flex justify-between px-1 text-[0.6875rem] text-faint" aria-hidden="true">
                    <span>"Faster · cheaper"</span>
                    <span>"Deeper · slower"</span>
                </div>
            })}
        </div>
    }
    .into_any()
}

/// The sliding pill: one segment per level, a raised pill under the shown
/// one. It is one slider to assistive technology.
fn pill(
    levels: StoredValue<Levels>,
    count: usize,
    short: bool,
    large: bool,
    shown: RwSignal<Option<usize>>,
    choose: impl Fn(usize) + Copy + Send + Sync + 'static,
) -> impl IntoView {
    let rail = NodeRef::<leptos::html::Div>::new();
    let dragging = RwSignal::new(false);
    // The segment under a pointer at `x`, in the rail's own coordinates.
    let at = move |x: f64| -> Option<usize> {
        let rail = rail.get_untracked()?;
        let frame = rail.get_bounding_client_rect();
        if frame.width() <= 0.0 {
            return None;
        }
        let fraction = ((x - frame.left()) / frame.width()).clamp(0.0, 0.999_9);
        Some((fraction * count as f64) as usize)
    };
    let tick = move |index: usize| {
        if shown.get_untracked() != Some(index) {
            shown.set(Some(index));
            // A short buzz at each new segment, where a phone offers one.
            let _ = window().navigator().vibrate_with_duration(4);
        }
    };
    let on_pointerdown = move |event: ev::PointerEvent| {
        if event.button() != 0 {
            return;
        }
        if let Some(rail) = rail.get_untracked() {
            let _ = rail.set_pointer_capture(event.pointer_id());
            let _ = rail.focus();
        }
        dragging.set(true);
        if let Some(index) = at(f64::from(event.client_x())) {
            tick(index);
        }
    };
    let on_pointermove = move |event: ev::PointerEvent| {
        if dragging.get_untracked()
            && let Some(index) = at(f64::from(event.client_x()))
        {
            tick(index);
        }
    };
    let release = move |_: ev::PointerEvent| {
        if dragging.get_untracked() {
            dragging.set(false);
            if let Some(index) = shown.get_untracked() {
                choose(index);
            }
        }
    };
    let on_keydown = move |event: ev::KeyboardEvent| {
        let current = shown.get_untracked();
        let next = match event.key().as_str() {
            "ArrowLeft" | "ArrowDown" => current.map_or(0, |index| index.saturating_sub(1)),
            "ArrowRight" | "ArrowUp" => current.map_or(0, |index| (index + 1).min(count - 1)),
            "Home" => 0,
            "End" => count - 1,
            _ => return,
        };
        event.prevent_default();
        choose(next);
    };
    let name = levels.with_value(|levels| levels.name);
    view! {
        <div
            node_ref=rail
            class="effort-rail"
            class:effort-dragging=move || dragging.get()
            class:effort-rail-large=large
            role="slider"
            tabindex="0"
            aria-label=name
            aria-valuemin="0"
            aria-valuemax=(count - 1).to_string()
            aria-valuenow=move || shown.get().map(|index| index.to_string())
            aria-valuetext=move || shown.get().map(|index| levels.with_value(|levels| levels.levels[index].label.clone()))
            style=move || format!(
                "--effort-count: {count}; --effort-at: {}",
                shown.get().unwrap_or_default(),
            )
            data-effort-pill
            on:pointerdown=on_pointerdown
            on:pointermove=on_pointermove
            on:pointerup=release
            on:pointercancel=release
            on:keydown=on_keydown
        >
            <Show when=move || shown.get().is_some()>
                <span class="effort-knob" aria-hidden="true"></span>
            </Show>
            {(0..count).map(|index| {
                let (whole, id) = levels.with_value(|levels| {
                    let level = &levels.levels[index];
                    (level.label.clone(), level.id.clone())
                });
                let text = if short { short_label(&whole) } else { whole.clone() };
                view! {
                    <span
                        class="effort-segment"
                        class:effort-segment-on=move || shown.get() == Some(index)
                        title=whole
                        data-effort-level=id
                    >
                        {text}
                    </span>
                }
            }).collect_view()}
        </div>
    }
}
