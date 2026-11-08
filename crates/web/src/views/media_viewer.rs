//! The media viewer: an image or video from the transcript over the whole
//! page, to inspect it. The wheel, a pinch, the zoom buttons, and `+`, `-`,
//! `0` zoom; a drag moves the zoomed file; a double click zooms in, or back
//! to fit. A video plays with its own bar: play and pause (Space or `k`),
//! a frame back or forward (`,` and `.`), five seconds back or forward
//! (Left and Right), a seek bar, its speed, and its sound. The bar on top
//! names the file and its size in pixels. Escape, ✕, or a click beside the
//! file closes it. A reply's pictures and videos open as a group the
//! viewer steps through: its arrows, Left and Right on an image, its
//! filmstrip, and a swipe across the file at its fitted size, which the
//! file follows until it slides away and the next comes in from that side
//! (a finger only pans a zoomed file, and a lone one springs back).

use std::{cell::Cell, collections::HashMap};

use leptos::{ev, html, prelude::*};
use wasm_bindgen::{JsCast as _, closure::Closure};

use crate::{
    icons::Glyph,
    media::{Viewing, Zoom, clock},
    workspace::Workspace,
};

const BAR_BUTTON: &str = "inline-flex min-h-11 min-w-11 shrink-0 items-center justify-center gap-1 rounded-md px-2 text-sm hover:bg-white/15 active:bg-white/25 disabled:opacity-40";

/// The step of the frame buttons: a frame at 30 frames a second, since a
/// browser does not tell a video's frame rate.
const FRAME: f64 = 1.0 / 30.0;

/// The speeds a video plays at.
const SPEEDS: [f64; 6] = [0.1, 0.25, 0.5, 1.0, 1.5, 2.0];

/// How long a swiped file takes to slide away or back.
const GLIDE: std::time::Duration = std::time::Duration::from_millis(180);

thread_local! {
    /// The side the next viewer's file comes in from: `1` after a step to
    /// the next, `-1` to the one before, `0` opened anew.
    static ENTER: Cell<i8> = const { Cell::new(0) };
    /// Whether an asset's Details show beside it, kept from file to file.
    static DETAILS: Cell<bool> = const { Cell::new(false) };
}

/// The viewer, open while `workspace.media_viewer` holds a file.
#[component]
pub(crate) fn MediaViewer(workspace: Workspace) -> impl IntoView {
    // Back closes the viewer before anything under it, however many items
    // it stepped through.
    Effect::new(move |was: Option<bool>| {
        let open = workspace.media_viewer.with(Option::is_some);
        if open && was != Some(true) {
            workspace.open_transient(Callback::new(move |()| workspace.media_viewer.set(None)));
        } else if !open && was == Some(true) {
            workspace.transient_closed();
        }
        open
    });
    move || {
        workspace.media_viewer.get().map(|viewing| {
            if viewing.url.is_empty() {
                view! { <Pending workspace viewing /> }.into_any()
            } else {
                view! { <Viewer workspace viewing /> }.into_any()
            }
        })
    }
}

/// An asset of the deck not read yet (`docs/site/specs/asset-opening.md`
/// section 4): its title and "Loading…" while it is read; Escape closes,
/// and Left and Right step past it.
#[component]
fn Pending(workspace: Workspace, viewing: Viewing) -> impl IntoView {
    let root = NodeRef::<html::Div>::new();
    crate::assets::load_viewing(viewing.clone(), workspace);
    Effect::new(move |_| {
        if let Some(root) = root.get() {
            let _ = root.focus();
        }
    });
    let at = workspace
        .media_group
        .with_untracked(|group| group.iter().position(|shown| *shown == viewing));
    let count = workspace.media_group.with_untracked(Vec::len);
    let turn = move |by: isize| {
        let Some(at) = at.filter(|_| count > 1) else {
            return;
        };
        ENTER.with(|enter| enter.set(by.signum() as i8));
        let next = workspace.media_group.with_untracked(|group| {
            group
                .get((at as isize + by).rem_euclid(count as isize) as usize)
                .cloned()
        });
        workspace.media_viewer.set(next);
    };
    let title = viewing.title().to_owned();
    let label = title.clone();
    view! {
        <div
            node_ref=root
            id="media-viewer"
            role="dialog"
            aria-modal="true"
            aria-label=label
            aria-busy="true"
            tabindex="-1"
            class="fixed inset-0 z-[60] flex flex-col bg-black/95 text-white outline-none"
            on:keydown=move |event: ev::KeyboardEvent| {
                let handled = match event.key().as_str() {
                    "Escape" => {
                        workspace.media_viewer.set(None);
                        true
                    }
                    "ArrowLeft" => {
                        turn(-1);
                        true
                    }
                    "ArrowRight" => {
                        turn(1);
                        true
                    }
                    _ => false,
                };
                if handled {
                    event.prevent_default();
                }
            }
        >
            <div class="flex shrink-0 items-center gap-1 px-2 pt-[max(0.25rem,env(safe-area-inset-top))] pb-1">
                <span class="flex min-w-0 flex-1 items-baseline gap-2 px-2 text-sm">
                    <span id="media-viewer-title" class="min-w-0 truncate">{title}</span>
                    {at.filter(|_| count > 1).map(|at| view! {
                        <span id="media-viewer-position" class="shrink-0 font-mono text-xs text-white/70 tabular-nums">
                            {format!("{} / {count}", at + 1)}
                        </span>
                    })}
                </span>
                <button
                    type="button"
                    id="media-viewer-close"
                    class=BAR_BUTTON
                    aria-label="Close"
                    title="Close (Esc)"
                    on:click=move |_| workspace.media_viewer.set(None)
                >
                    <Glyph icon=icondata::LuX />
                </button>
            </div>
            <div class="flex min-h-0 flex-1 items-center justify-center text-sm text-white/70" data-viewer-loading>
                "Loading…"
            </div>
        </div>
    }
}

#[component]
fn Viewer(workspace: Workspace, viewing: Viewing) -> impl IntoView {
    let close = move || workspace.media_viewer.set(None);
    // The deck it steps through, and where it is in it.
    let group = workspace.media_group.get_untracked();
    let at = group.iter().position(|shown| *shown == viewing);
    let stepping = group.len() > 1 && at.is_some();
    let count = group.len();
    let go = move |to: usize| {
        let Some(next) = workspace
            .media_group
            .with_untracked(|group| group.get(to).cloned())
        else {
            return;
        };
        workspace.media_viewer.set(Some(next));
        // The viewer drawn anew takes the focus, for its keys.
        request_animation_frame(|| {
            if let Some(viewer) = web_sys::window()
                .and_then(|window| window.document())
                .and_then(|document| document.get_element_by_id("media-viewer"))
                .and_then(|viewer| viewer.dyn_into::<web_sys::HtmlElement>().ok())
            {
                let _ = viewer.focus();
            }
        });
    };
    let turn = move |by: isize| {
        if let Some(at) = at.filter(|_| stepping) {
            ENTER.with(|enter| enter.set(by.signum() as i8));
            go((at as isize + by).rem_euclid(count as isize) as usize);
        }
    };
    let entering = ENTER.with(|enter| enter.replace(0));
    let zoom = RwSignal::new(Zoom::default());
    let asset = viewing.asset.clone();
    let has_asset = asset.is_some();
    let details = RwSignal::new(has_asset && DETAILS.with(Cell::get));
    let toggle_details = move || {
        let open = !details.get_untracked();
        DETAILS.with(|kept| kept.set(open));
        details.set(open);
    };
    let root = NodeRef::<html::Div>::new();
    let stage = NodeRef::<html::Div>::new();
    let video = NodeRef::<html::Video>::new();
    let is_video = viewing.is_video();
    let title = viewing.title().to_owned();
    let label = title.clone();
    let download = if viewing.alt.is_empty() {
        "media".to_owned()
    } else {
        viewing.alt.clone()
    };

    // The focus goes to the viewer, for its keys, and back where it was
    // once the viewer closes.
    let before = StoredValue::new_local(
        web_sys::window()
            .and_then(|window| window.document())
            .and_then(|document| document.active_element()),
    );
    Effect::new(move |_| {
        if let Some(root) = root.get() {
            let _ = root.focus();
        }
    });
    on_cleanup(move || {
        if let Some(Some(before)) = before.try_get_value()
            && before.is_connected()
            && let Ok(before) = before.dyn_into::<web_sys::HtmlElement>()
        {
            let _ = before.focus();
        }
    });

    // Where a point of the page is, from the stage's centre.
    let from_centre = move |client_x: i32, client_y: i32| {
        stage.get_untracked().map_or((0.0, 0.0), |stage| {
            let rect = stage.get_bounding_client_rect();
            (
                f64::from(client_x) - rect.left() - rect.width() / 2.0,
                f64::from(client_y) - rect.top() - rect.height() / 2.0,
            )
        })
    };

    // The wheel zooms. The page gets it through a listener that is not
    // passive, so that the browser's own zoom (Ctrl and the wheel, or a
    // trackpad's pinch) does not zoom the page instead.
    let wheel = StoredValue::new_local(None::<Closure<dyn Fn(web_sys::WheelEvent)>>);
    Effect::new(move |_| {
        let Some(element) = stage.get() else {
            return;
        };
        let listener =
            Closure::<dyn Fn(web_sys::WheelEvent)>::new(move |event: web_sys::WheelEvent| {
                event.prevent_default();
                let delta = match event.delta_mode() {
                    web_sys::WheelEvent::DOM_DELTA_LINE => event.delta_y() * 16.0,
                    web_sys::WheelEvent::DOM_DELTA_PAGE => event.delta_y() * 400.0,
                    _ => event.delta_y(),
                };
                // A pinch arrives as small steps with Ctrl held.
                let rate = if event.ctrl_key() { 0.01 } else { 0.002 };
                let (x, y) = from_centre(event.client_x(), event.client_y());
                zoom.update(|zoom| *zoom = zoom.zoom_at((-delta * rate).exp(), x, y));
            });
        let options = web_sys::AddEventListenerOptions::new();
        options.set_passive(false);
        let _ = element.add_event_listener_with_callback_and_add_event_listener_options(
            "wheel",
            listener.as_ref().unchecked_ref(),
            &options,
        );
        wheel.set_value(Some(listener));
    });

    // The pointers down on the stage, by ID: one drags, two pinch.
    let pointers = StoredValue::new(HashMap::<i32, (f64, f64)>::new());
    // A one-finger drag across the file at its fitted size: how far it went
    // sideways, which the file follows, and where and when it started; and
    // whether the file glides to rest after it.
    let swipe = RwSignal::new(None::<f64>);
    let swipe_from = StoredValue::new(None::<(f64, f64, f64)>);
    let gliding = RwSignal::new(false);
    let stage_width = move || {
        stage
            .get_untracked()
            .map_or(0.0, |stage| stage.get_bounding_client_rect().width())
    };
    // How far the pointers moved since the press, to tell a drag from a
    // click, and whether the press was on the file.
    let travel = StoredValue::new(0.0_f64);
    let on_file = StoredValue::new(false);
    let pressed_on_file = |event: &ev::PointerEvent| {
        event
            .target()
            .and_then(|target| target.dyn_into::<web_sys::Element>().ok())
            .and_then(|target| target.closest("[data-viewer-file]").ok().flatten())
            .is_some()
    };
    let on_pointerdown = move |event: ev::PointerEvent| {
        if event.pointer_type() == "mouse" && event.button() != 0 {
            return;
        }
        if event.pointer_type() == "mouse" {
            // No text or image selection under a drag.
            event.prevent_default();
        }
        let first = pointers.with_value(HashMap::is_empty);
        if first {
            travel.set_value(0.0);
            on_file.set_value(pressed_on_file(&event));
        }
        // A finger swipes the file at its fitted size; a second one pinches.
        if first && zoom.get_untracked() == Zoom::default() {
            swipe_from.set_value(Some((
                f64::from(event.client_x()),
                f64::from(event.client_y()),
                js_sys::Date::now(),
            )));
        } else if swipe_from.get_value().is_some() {
            swipe_from.set_value(None);
            swipe.set(None);
        }
        if let Some(stage) = stage.get_untracked() {
            let _ = stage.set_pointer_capture(event.pointer_id());
        }
        pointers.update_value(|pointers| {
            pointers.insert(
                event.pointer_id(),
                (f64::from(event.client_x()), f64::from(event.client_y())),
            );
        });
    };
    let on_pointermove = move |event: ev::PointerEvent| {
        let id = event.pointer_id();
        let now = (f64::from(event.client_x()), f64::from(event.client_y()));
        let Some(was) = pointers.with_value(|pointers| pointers.get(&id).copied()) else {
            return;
        };
        let other = pointers.with_value(|pointers| {
            pointers
                .iter()
                .find(|(other, _)| **other != id)
                .map(|(_, at)| *at)
        });
        pointers.update_value(|pointers| {
            pointers.insert(id, now);
        });
        travel.update_value(|travel| *travel += (now.0 - was.0).hypot(now.1 - was.1));
        match other {
            // Two fingers: the file follows their midpoint and zooms with
            // their spread.
            Some(other) => {
                let before = was.0 - other.0;
                let before = before.hypot(was.1 - other.1);
                let after = (now.0 - other.0).hypot(now.1 - other.1);
                let mid = |a: (f64, f64)| ((a.0 + other.0) / 2.0, (a.1 + other.1) / 2.0);
                let (from, to) = (mid(was), mid(now));
                let (x, y) = from_centre(to.0 as i32, to.1 as i32);
                zoom.update(|zoom| {
                    let panned = zoom.panned(to.0 - from.0, to.1 - from.1);
                    *zoom = if before > 0.0 {
                        panned.zoom_at(after / before, x, y)
                    } else {
                        panned
                    };
                });
            }
            None => match swipe_from.get_value() {
                // A lone file has nothing to turn to: it gives a little.
                Some((x, _, _)) => {
                    let across = now.0 - x;
                    swipe.set(Some(if stepping { across } else { across * 0.25 }));
                }
                None => zoom.update(|zoom| *zoom = zoom.panned(now.0 - was.0, now.1 - was.1)),
            },
        }
    };
    let on_pointerup = move |event: ev::PointerEvent| {
        pointers.update_value(|pointers| {
            pointers.remove(&event.pointer_id());
        });
        if !pointers.with_value(HashMap::is_empty) {
            return;
        }
        let Some((x, y, at)) = swipe_from.get_value() else {
            return;
        };
        swipe_from.set_value(None);
        let (dx, dy) = (
            f64::from(event.client_x()) - x,
            f64::from(event.client_y()) - y,
        );
        if dx.abs() < 1.0 {
            swipe.set(None);
            return;
        }
        let width = stage_width();
        let by = if stepping {
            crate::media::swipe_turn(dx, dy, width, js_sys::Date::now() - at)
        } else {
            0
        };
        gliding.set(true);
        if by == 0 {
            swipe.set(Some(0.0));
            set_timeout(
                move || {
                    if gliding.try_get_untracked() == Some(true) {
                        swipe.set(None);
                        gliding.set(false);
                    }
                },
                GLIDE,
            );
        } else {
            swipe.set(Some(-f64::from(by) * width));
            set_timeout(move || turn(isize::from(by)), GLIDE);
        }
    };

    // A click on a video plays or pauses it, a click beside the file
    // closes the viewer; a drag does neither.
    let toggle_play = move || {
        if let Some(video) = video.get_untracked() {
            if video.paused() {
                let _ = video.play();
            } else {
                let _ = video.pause();
            }
        }
    };
    let on_click = move |_: ev::MouseEvent| {
        if travel.get_value() > 5.0 {
            return;
        }
        if !on_file.get_value() {
            close();
        } else if is_video {
            toggle_play();
        }
    };
    let on_dblclick = move |event: ev::MouseEvent| {
        if !on_file.get_value() {
            return;
        }
        swipe.set(None);
        let (x, y) = from_centre(event.client_x(), event.client_y());
        zoom.update(|zoom| {
            *zoom = if *zoom == Zoom::default() {
                zoom.zoom_at(2.5, x, y)
            } else {
                Zoom::default()
            };
        });
    };
    let zoom_by = move |factor: f64| zoom.update(|zoom| *zoom = zoom.zoom_at(factor, 0.0, 0.0));

    // The video's state, as its events tell it.
    let playing = RwSignal::new(false);
    let position = RwSignal::new(0.0_f64);
    let duration = RwSignal::new(0.0_f64);
    let speed = RwSignal::new(1.0_f64);
    let muted = RwSignal::new(false);
    let seek = move |to: f64| {
        if let Some(video) = video.get_untracked() {
            let end = video.duration();
            let to = if end.is_finite() {
                to.clamp(0.0, end)
            } else {
                to.max(0.0)
            };
            video.set_current_time(to);
            position.set(to);
        }
    };
    let step = move |by: f64| {
        if let Some(video) = video.get_untracked() {
            let _ = video.pause();
            seek(video.current_time() + by);
        }
    };

    let on_keydown = move |event: ev::KeyboardEvent| {
        if event.ctrl_key() || event.meta_key() || event.alt_key() {
            return;
        }
        let in_field = event
            .target()
            .and_then(|target| target.dyn_into::<web_sys::Element>().ok())
            .is_some_and(|target| target.tag_name() == "SELECT");
        let handled = match event.key().as_str() {
            "Escape" => {
                close();
                true
            }
            "+" | "=" => {
                zoom_by(1.25);
                true
            }
            "-" | "_" => {
                zoom_by(0.8);
                true
            }
            "0" => {
                zoom.set(Zoom::default());
                true
            }
            "i" if has_asset => {
                toggle_details();
                true
            }
            " " | "k" if is_video && !in_field => {
                toggle_play();
                true
            }
            "," if is_video => {
                step(-FRAME);
                true
            }
            "." if is_video => {
                step(FRAME);
                true
            }
            "ArrowLeft" if stepping && !is_video => {
                turn(-1);
                true
            }
            "ArrowRight" if stepping && !is_video => {
                turn(1);
                true
            }
            "ArrowLeft" if is_video && !in_field => {
                seek(position.get_untracked() - 5.0);
                true
            }
            "ArrowRight" if is_video && !in_field => {
                seek(position.get_untracked() + 5.0);
                true
            }
            _ => false,
        };
        if handled {
            event.prevent_default();
            event.stop_propagation();
        }
    };

    // The file's own size in pixels, and how large its pixels show when it
    // fits the window, measured once it loads and whenever the window
    // resizes. Pixels shown three times their size or more show crisp
    // rather than blurred.
    let size = RwSignal::new(None::<(u32, u32)>);
    let fitted = RwSignal::new(1.0_f64);
    let measure = move || {
        let Some(element) = web_sys::window()
            .and_then(|window| window.document())
            .and_then(|document| document.get_element_by_id("media-viewer-file"))
        else {
            return;
        };
        let natural = if let Some(image) = element.dyn_ref::<web_sys::HtmlImageElement>() {
            (image.natural_width(), image.natural_height())
        } else if let Some(video) = element.dyn_ref::<web_sys::HtmlVideoElement>() {
            (video.video_width(), video.video_height())
        } else {
            return;
        };
        if natural.0 == 0 {
            return;
        }
        size.set(Some(natural));
        if let Some(element) = element.dyn_ref::<web_sys::HtmlElement>() {
            fitted.set(f64::from(element.offset_width()) / f64::from(natural.0));
        }
    };
    let resized = window_event_listener(ev::resize, move |_| measure());
    on_cleanup(move || resized.remove());
    let entrance = match entering {
        1 => "viewer-from-next",
        -1 => "viewer-from-previous",
        _ => "",
    };
    let file_class = move || {
        format!(
            "max-h-full max-w-full select-none object-contain will-change-transform {entrance} {} {} {}",
            if gliding.get() {
                "transition-transform duration-[180ms] ease-out"
            } else {
                ""
            },
            if is_video && zoom.get() == Zoom::default() {
                "cursor-pointer"
            } else {
                "cursor-grab active:cursor-grabbing"
            },
            if zoom.get().scale * fitted.get() >= 3.0 {
                "[image-rendering:pixelated]"
            } else {
                ""
            }
        )
    };
    let transform = move || match swipe.get() {
        Some(across) => format!("translate({across:.1}px, 0px)"),
        None => zoom.get().transform(),
    };
    let url = viewing.url.clone();
    let file = if is_video {
        view! {
            <video
                node_ref=video
                id="media-viewer-file"
                data-viewer-file
                src=url
                autoplay
                playsinline
                class=file_class
                style:transform=transform
                on:loadedmetadata=move |_| {
                    if let Some(video) = video.get_untracked() {
                        duration.set(video.duration());
                    }
                    measure();
                }
                on:durationchange=move |_| {
                    if let Some(video) = video.get_untracked() {
                        duration.set(video.duration());
                    }
                }
                on:timeupdate=move |_| {
                    if let Some(video) = video.get_untracked() {
                        position.set(video.current_time());
                    }
                }
                on:seeked=move |_| {
                    if let Some(video) = video.get_untracked() {
                        position.set(video.current_time());
                    }
                }
                on:play=move |_| playing.set(true)
                on:pause=move |_| playing.set(false)
                on:ended=move |_| playing.set(false)
                on:ratechange=move |_| {
                    if let Some(video) = video.get_untracked() {
                        speed.set(video.playback_rate());
                    }
                }
                on:volumechange=move |_| {
                    if let Some(video) = video.get_untracked() {
                        muted.set(video.muted());
                    }
                }
            ></video>
        }
        .into_any()
    } else {
        view! {
            <img
                id="media-viewer-file"
                data-viewer-file
                src=url
                alt=viewing.alt.clone()
                draggable="false"
                class=file_class
                style:transform=transform
                on:load=move |_| measure()
            />
        }
        .into_any()
    };

    let controls = is_video.then(|| {
        let known = move || duration.get().is_finite() && duration.get() > 0.0;
        view! {
            <div
                id="media-viewer-controls"
                class="flex shrink-0 flex-wrap items-center gap-1 px-2 pt-1 pb-[max(0.25rem,env(safe-area-inset-bottom))]"
            >
                <button
                    type="button"
                    id="media-viewer-play"
                    class=BAR_BUTTON
                    aria-label=move || if playing.get() { "Pause" } else { "Play" }
                    title=move || if playing.get() { "Pause (Space)" } else { "Play (Space)" }
                    on:click=move |_| toggle_play()
                >
                    {move || {
                        if playing.get() {
                            view! { <Glyph icon=icondata::LuPause /> }
                        } else {
                            view! { <Glyph icon=icondata::LuPlay /> }
                        }
                    }}
                </button>
                <button
                    type="button"
                    id="media-viewer-back"
                    class=BAR_BUTTON
                    aria-label="Back a frame"
                    title="Back a frame (,)"
                    on:click=move |_| step(-FRAME)
                >
                    <Glyph icon=icondata::LuSkipBack />
                </button>
                <button
                    type="button"
                    id="media-viewer-forward"
                    class=BAR_BUTTON
                    aria-label="Forward a frame"
                    title="Forward a frame (.)"
                    on:click=move |_| step(FRAME)
                >
                    <Glyph icon=icondata::LuSkipForward />
                </button>
                <span class="shrink-0 px-1 font-mono text-xs tabular-nums" id="media-viewer-time">
                    {move || clock(position.get())}
                </span>
                <input
                    type="range"
                    id="media-viewer-seek"
                    aria-label="Position"
                    class="h-11 min-w-32 flex-1 cursor-pointer accent-accent"
                    min="0"
                    step="any"
                    disabled=move || !known()
                    prop:max=move || if known() { duration.get() } else { 0.0 }
                    prop:value=move || position.get()
                    on:input=move |event| {
                        if let Ok(to) = event_target_value(&event).parse::<f64>() {
                            seek(to);
                        }
                    }
                />
                <span class="shrink-0 px-1 font-mono text-xs tabular-nums" id="media-viewer-duration">
                    {move || clock(duration.get())}
                </span>
                <select
                    id="media-viewer-speed"
                    aria-label="Speed"
                    title="Speed"
                    class="min-h-11 shrink-0 rounded-md bg-transparent px-1 text-sm hover:bg-white/15 [&>option]:text-black"
                    prop:value=move || speed.get().to_string()
                    on:change=move |event| {
                        if let (Ok(to), Some(video)) = (
                            event_target_value(&event).parse::<f64>(),
                            video.get_untracked(),
                        ) {
                            video.set_playback_rate(to);
                        }
                    }
                >
                    {SPEEDS
                        .iter()
                        .map(|speed| {
                            view! { <option value=speed.to_string()>{format!("{speed}×")}</option> }
                        })
                        .collect_view()}
                </select>
                <button
                    type="button"
                    id="media-viewer-mute"
                    class=BAR_BUTTON
                    aria-label=move || if muted.get() { "Unmute" } else { "Mute" }
                    title=move || if muted.get() { "Unmute" } else { "Mute" }
                    on:click=move |_| {
                        if let Some(video) = video.get_untracked() {
                            video.set_muted(!video.muted());
                        }
                    }
                >
                    {move || {
                        if muted.get() {
                            view! { <Glyph icon=icondata::LuVolumeX /> }
                        } else {
                            view! { <Glyph icon=icondata::LuVolume2 /> }
                        }
                    }}
                </button>
            </div>
        }
    });

    view! {
        <div
            node_ref=root
            id="media-viewer"
            role="dialog"
            aria-modal="true"
            aria-label=label
            tabindex="-1"
            class="fixed inset-0 z-[60] flex flex-col bg-black/95 text-white outline-none"
            on:keydown=on_keydown
        >
            <div class="flex shrink-0 items-center gap-1 px-2 pt-[max(0.25rem,env(safe-area-inset-top))] pb-1">
                <span class="flex min-w-0 flex-1 items-baseline gap-2 px-2 text-sm">
                    <span id="media-viewer-title" class="min-w-0 truncate">{title}</span>
                    {at.filter(|_| stepping).map(|at| view! {
                        <span id="media-viewer-position" class="shrink-0 font-mono text-xs text-white/70 tabular-nums">
                            {format!("{} / {count}", at + 1)}
                        </span>
                    })}
                    <span id="media-viewer-size" class="shrink-0 font-mono text-xs text-white/70 tabular-nums">
                        {move || size.get().map(|(width, height)| format!("{width}×{height}"))}
                    </span>
                </span>
                <button
                    type="button"
                    id="media-viewer-zoom-out"
                    class=BAR_BUTTON
                    aria-label="Zoom out"
                    title="Zoom out (-)"
                    on:click=move |_| zoom_by(0.8)
                >
                    <Glyph icon=icondata::LuZoomOut />
                </button>
                <button
                    type="button"
                    id="media-viewer-fit"
                    class=format!("{BAR_BUTTON} font-mono tabular-nums")
                    aria-label="Fit to the window"
                    title="Fit to the window (0)"
                    on:click=move |_| zoom.set(Zoom::default())
                >
                    {move || zoom.get().percent()}
                </button>
                <button
                    type="button"
                    id="media-viewer-zoom-in"
                    class=BAR_BUTTON
                    aria-label="Zoom in"
                    title="Zoom in (+)"
                    on:click=move |_| zoom_by(1.25)
                >
                    <Glyph icon=icondata::LuZoomIn />
                </button>
                {viewing.handle.clone().map(|handle| view! {
                    <button
                        type="button"
                        id="media-viewer-copy"
                        class=BAR_BUTTON
                        aria-label="Copy the handle"
                        title=format!("Copy {handle}")
                        on:click=move |_| crate::assets::copy_handle(handle.clone(), workspace)
                    >
                        <Glyph icon=icondata::LuCopy />
                    </button>
                })}
                {asset.clone().map(|(store, id)| {
                    let title = viewing.title().to_owned();
                    view! {
                        <Show when=move || workspace.has_shares.get()>
                            <button
                                type="button"
                                id="media-viewer-share"
                                class=BAR_BUTTON
                                aria-label="Share link"
                                title="Share link"
                                on:click={
                                    let (store, title) = (store.clone(), title.clone());
                                    move |_| workspace.open_share_dialog(store.clone(), id, title.clone())
                                }
                            >
                                <Glyph icon=icondata::LuShare2 />
                            </button>
                        </Show>
                    }
                })}
                <a
                    id="media-viewer-download"
                    class=BAR_BUTTON
                    href=viewing.url.clone()
                    download=download
                    aria-label="Download"
                    title="Download"
                >
                    <Glyph icon=icondata::LuDownload />
                </a>
                {has_asset.then(|| view! {
                    <button
                        type="button"
                        id="media-viewer-details"
                        class=move || format!("{BAR_BUTTON} {}", if details.get() { "bg-white/20" } else { "" })
                        aria-label="Details"
                        title="Details (i)"
                        aria-pressed=move || details.get().to_string()
                        on:click=move |_| toggle_details()
                    >
                        <Glyph icon=icondata::LuInfo />
                    </button>
                })}
                <button
                    type="button"
                    id="media-viewer-close"
                    class=BAR_BUTTON
                    aria-label="Close"
                    title="Close (Esc)"
                    on:click=move |_| close()
                >
                    <Glyph icon=icondata::LuX />
                </button>
            </div>
            <div class="relative flex min-h-0 flex-1">
            // The arrows stand on the file's side, clear of the Details.
            <div class="relative flex min-h-0 min-w-0 flex-1">
            <div
                node_ref=stage
                id="media-viewer-stage"
                class="relative flex min-h-0 min-w-0 flex-1 touch-none items-center justify-center overflow-hidden p-2 select-none"
                on:pointerdown=on_pointerdown
                on:pointermove=on_pointermove
                on:pointerup=on_pointerup
                on:pointercancel=on_pointerup
                on:click=on_click
                on:dblclick=on_dblclick
            >
                {file}
            </div>
            {stepping.then(|| view! {
                <button
                    type="button"
                    id="media-viewer-previous"
                    class=format!("{BAR_BUTTON} absolute top-1/2 left-2 z-10 -translate-y-1/2 rounded-full bg-white/10")
                    aria-label="Previous"
                    title="Previous (←)"
                    on:click=move |_| turn(-1)
                >
                    <Glyph icon=icondata::LuChevronLeft />
                </button>
                <button
                    type="button"
                    id="media-viewer-next"
                    class=format!("{BAR_BUTTON} absolute top-1/2 right-2 z-10 -translate-y-1/2 rounded-full bg-white/10")
                    aria-label="Next"
                    title="Next (→)"
                    on:click=move |_| turn(1)
                >
                    <Glyph icon=icondata::LuChevronRight />
                </button>
            })}
            </div>
            {asset.clone().map(|(store, id)| view! {
                <Show when=move || details.get()>
                    <super::asset_details::AssetDetails workspace store=store.clone() id />
                </Show>
            })}
            </div>
            {stepping.then(|| view! {
                <div id="media-viewer-strip" class="flex shrink-0 justify-center gap-2 overflow-x-auto px-2 pb-2">
                    {group
                        .iter()
                        .enumerate()
                        .map(|(index, shown)| {
                            let current = Some(index) == at;
                            let picture = if shown.is_video() || shown.url.is_empty() {
                                let icon = if shown.is_video() { icondata::LuFilm } else { icondata::LuImage };
                                view! { <Glyph icon=icon /> }.into_any()
                            } else {
                                view! {
                                    <img src=shown.url.clone() alt="" draggable="false" class="h-full w-full object-cover" />
                                }
                                    .into_any()
                            };
                            view! {
                                <button
                                    type="button"
                                    class="flex h-10 w-14 shrink-0 items-center justify-center overflow-hidden rounded-md border-2 bg-white/10"
                                    class:border-white=current
                                    class:border-transparent=!current
                                    class:opacity-50=!current
                                    aria-label=shown.title().to_owned()
                                    aria-current=current.then_some("true")
                                    title=shown.title().to_owned()
                                    on:click=move |_| go(index)
                                >
                                    {picture}
                                </button>
                            }
                        })
                        .collect_view()}
                </div>
            })}
            {controls}
        </div>
    }
}
